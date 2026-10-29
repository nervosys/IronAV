//! Audio analyzers: levels, loudness (EBU R128) and silence.

use crate::{db_power, round, Analyzer, Input, Observation, Spans};
use ironav_core::{StreamInfo, StreamParams};
use serde_json::{json, Value};

/// A sample at or above this magnitude counts as clipped.
const CLIP: f64 = 0.999;

#[derive(Default)]
pub struct Levels {
    peak: f64,
    sum_sq: f64,
    n: u64,
    clipped: u64,
    all_peak: f64,
    all_sum_sq: f64,
    all_n: u64,
    all_clipped: u64,
    last_clip_report: Option<f64>,
}

impl Analyzer for Levels {
    fn observe(&mut self, input: &Input, out: &mut Vec<Observation>) {
        let Input::Audio { frame, t } = input else { return };
        let mut clipped = 0;
        for &s in &frame.samples {
            let a = s.abs();
            self.peak = self.peak.max(a);
            self.sum_sq += s * s;
            if a >= CLIP {
                clipped += 1;
            }
        }
        self.n += frame.samples.len() as u64;
        self.clipped += clipped;
        if clipped > 0 && self.last_clip_report.is_none_or(|l| t - l >= 1.0) {
            self.last_clip_report = Some(*t);
            out.push(Observation::new("clipping", "levels", *t, json!({ "samples": clipped })));
        }
    }

    fn digest(&mut self, _seconds: f64) -> Option<Value> {
        let v = json!({
            "peak_dbfs": db_power(self.peak * self.peak),
            "rms_dbfs": db_power(self.sum_sq / self.n.max(1) as f64),
            "clipped_samples": self.clipped,
        });
        self.all_peak = self.all_peak.max(self.peak);
        self.all_sum_sq += self.sum_sq;
        self.all_n += self.n;
        self.all_clipped += self.clipped;
        (self.peak, self.sum_sq, self.n, self.clipped) = (0.0, 0.0, 0, 0);
        Some(v)
    }

    fn finish(&mut self, _t: f64, _out: &mut Vec<Observation>) -> Option<Value> {
        self.digest(0.0);
        Some(json!({
            "peak_dbfs": db_power(self.all_peak * self.all_peak),
            "rms_dbfs": db_power(self.all_sum_sq / self.all_n.max(1) as f64),
            "clipped_samples": self.all_clipped,
        }))
    }
}

/// A second-order IIR section (direct form I).
#[derive(Clone, Copy)]
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    x: [f64; 2],
    y: [f64; 2],
}

impl Biquad {
    fn run(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.b[1] * self.x[0] + self.b[2] * self.x[1] - self.a[0] * self.y[0] - self.a[1] * self.y[1];
        self.x = [x, self.x[0]];
        self.y = [y, self.y[0]];
        y
    }
}

/// The two K-weighting stages of BS.1770 at sample rate `fs`: a high shelf, then a high pass. The
/// analogue prototypes are the ones libebur128 derives, so any rate gets the 48 kHz response.
fn k_weighting(fs: f64) -> [Biquad; 2] {
    let pi = std::f64::consts::PI;
    let (f0, g, q) = (1681.974450955533, 3.999843853973347, 0.7071752369554196);
    let k = (pi * f0 / fs).tan();
    let vh = 10f64.powf(g / 20.0);
    let vb = vh.powf(0.4996667741545416);
    let a0 = 1.0 + k / q + k * k;
    let shelf = Biquad {
        b: [(vh + vb * k / q + k * k) / a0, 2.0 * (k * k - vh) / a0, (vh - vb * k / q + k * k) / a0],
        a: [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
        x: [0.0; 2],
        y: [0.0; 2],
    };
    let (f0, q) = (38.13547087602444, 0.5003270373238773);
    let k = (pi * f0 / fs).tan();
    let a0 = 1.0 + k / q + k * k;
    let high_pass = Biquad { b: [1.0, -2.0, 1.0], a: [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0], x: [0.0; 2], y: [0.0; 2] };
    [shelf, high_pass]
}

fn lufs(energy: f64) -> f64 {
    -0.691 + 10.0 * energy.log10()
}

fn lufs_value(energy: f64) -> Value {
    if energy > 0.0 { json!(round(lufs(energy), 2)) } else { Value::Null }
}

/// Mean of `v`.
fn mean(v: &[f64]) -> f64 {
    v.iter().sum::<f64>() / v.len().max(1) as f64
}

/// Gated loudness (BS.1770-4) of 400 ms block energies: an absolute gate at -70 LUFS, then a
/// relative one `rel` LU below the loudness of what passed it. Returns the energies that pass both.
fn gated(blocks: &[f64], rel: f64) -> Vec<f64> {
    let abs: Vec<f64> = blocks.iter().copied().filter(|&e| e > 0.0 && lufs(e) > -70.0).collect();
    if abs.is_empty() {
        return abs;
    }
    let gate = lufs(mean(&abs)) + rel;
    abs.into_iter().filter(|&e| lufs(e) > gate).collect()
}

/// Loudness per EBU R128: 100 ms blocks of K-weighted, channel-weighted energy; momentary loudness
/// over 4 of them, short-term over 30; integrated loudness gated over every momentary block, and
/// the loudness range (EBU Tech 3342) from the short-term values.
pub struct Loudness {
    filters: Vec<[Biquad; 2]>,
    weights: Vec<f64>,
    block_len: usize,
    acc: f64,
    filled: usize,
    /// Energies of the 100 ms blocks so far.
    blocks: Vec<f64>,
    momentary: Vec<f64>,
    short_term: Vec<f64>,
    window_max_momentary: f64,
}

impl Loudness {
    pub fn new(s: &StreamInfo) -> Self {
        let (rate, channels) = match s.params {
            StreamParams::Audio { sample_rate, channels } => (sample_rate.max(1), channels.max(1) as usize),
            _ => (48000, 2),
        };
        // BS.1770 weights: the surround channels count 1.41 and the LFE not at all. With six
        // channels the order is taken to be L R C LFE Ls Rs.
        let weights = (0..channels).map(|c| if channels == 6 { [1.0, 1.0, 1.0, 0.0, 1.41, 1.41][c] } else { 1.0 }).collect();
        Loudness {
            filters: vec![k_weighting(rate as f64); channels],
            weights,
            block_len: (rate as usize / 10).max(1),
            acc: 0.0,
            filled: 0,
            blocks: Vec::new(),
            momentary: Vec::new(),
            short_term: Vec::new(),
            window_max_momentary: 0.0,
        }
    }

    fn block_done(&mut self, t: f64, out: &mut Vec<Observation>) {
        self.blocks.push(self.acc / self.block_len as f64);
        self.acc = 0.0;
        self.filled = 0;
        let n = self.blocks.len();
        if n >= 4 {
            let m = mean(&self.blocks[n - 4..]);
            self.momentary.push(m);
            self.window_max_momentary = self.window_max_momentary.max(m);
        }
        if n >= 30 {
            self.short_term.push(mean(&self.blocks[n - 30..]));
        }
        if n % 10 == 0 {
            out.push(Observation::new("loudness", "loudness", t, json!({
                "momentary_lufs": lufs_value(*self.momentary.last().unwrap_or(&0.0)),
                "short_term_lufs": lufs_value(*self.short_term.last().unwrap_or(&0.0)),
            })));
        }
    }

    fn integrated(&self) -> Value {
        let g = gated(&self.momentary, -10.0);
        if g.is_empty() { Value::Null } else { json!(round(lufs(mean(&g)), 2)) }
    }

    fn range(&self) -> Value {
        let mut g: Vec<f64> = gated(&self.short_term, -20.0).into_iter().map(lufs).collect();
        if g.len() < 2 {
            return Value::Null;
        }
        g.sort_by(f64::total_cmp);
        let at = |p: f64| g[((g.len() - 1) as f64 * p).round() as usize];
        json!(round(at(0.95) - at(0.10), 2))
    }
}

impl Analyzer for Loudness {
    fn observe(&mut self, input: &Input, out: &mut Vec<Observation>) {
        let Input::Audio { frame, t } = input else { return };
        let ch = self.filters.len();
        if frame.channels as usize != ch {
            return;
        }
        let rate = frame.sample_rate.max(1) as f64;
        for (i, samples) in frame.samples.chunks_exact(ch).enumerate() {
            for (c, &x) in samples.iter().enumerate() {
                let [a, b] = &mut self.filters[c];
                let y = b.run(a.run(x));
                self.acc += self.weights[c] * y * y;
            }
            self.filled += 1;
            if self.filled == self.block_len {
                self.block_done(t + (i + 1) as f64 / rate, out);
            }
        }
    }

    fn digest(&mut self, _seconds: f64) -> Option<Value> {
        let v = json!({
            "momentary_max_lufs": lufs_value(self.window_max_momentary),
            "short_term_lufs": lufs_value(*self.short_term.last().unwrap_or(&0.0)),
            "integrated_lufs": self.integrated(),
        });
        self.window_max_momentary = 0.0;
        Some(v)
    }

    fn finish(&mut self, _t: f64, _out: &mut Vec<Observation>) -> Option<Value> {
        Some(json!({
            "integrated_lufs": self.integrated(),
            "range_lu": self.range(),
            "momentary_max_lufs": lufs_value(self.momentary.iter().copied().fold(0.0, f64::max)),
        }))
    }
}

/// RMS below this, in dBFS, is silence.
const SILENCE_DB: f64 = -50.0;
/// Silence is measured over chunks this long.
const SILENCE_CHUNK: f64 = 0.01;

pub struct Silence {
    spans: Spans,
    sum_sq: f64,
    n: usize,
    chunk_start: Option<f64>,
    last_end: f64,
}

impl Silence {
    pub fn new() -> Self {
        Silence { spans: Spans::new("silence", "silence_start", "silence_end", 0.5), sum_sq: 0.0, n: 0, chunk_start: None, last_end: 0.0 }
    }
}

impl Analyzer for Silence {
    fn observe(&mut self, input: &Input, out: &mut Vec<Observation>) {
        let Input::Audio { frame, t } = input else { return };
        let ch = frame.channels.max(1) as usize;
        let rate = frame.sample_rate.max(1) as f64;
        let chunk = ((rate * SILENCE_CHUNK) as usize).max(1);
        for (i, samples) in frame.samples.chunks_exact(ch).enumerate() {
            let at = t + i as f64 / rate;
            let start = *self.chunk_start.get_or_insert(at);
            self.sum_sq += samples.iter().map(|s| s * s).sum::<f64>() / ch as f64;
            self.n += 1;
            if self.n == chunk {
                let ms = self.sum_sq / chunk as f64;
                let silent = ms <= 0.0 || 10.0 * ms.log10() < SILENCE_DB;
                let dt = chunk as f64 / rate;
                self.spans.update(start, dt, silent, out);
                self.last_end = start + dt;
                (self.sum_sq, self.n, self.chunk_start) = (0.0, 0, None);
            }
        }
    }

    fn digest(&mut self, _seconds: f64) -> Option<Value> {
        Some(json!({ "silent_seconds": round(self.spans.take_window(), 3) }))
    }

    fn finish(&mut self, t: f64, out: &mut Vec<Observation>) -> Option<Value> {
        self.spans.close(t.max(self.last_end), out);
        Some(self.spans.summary())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironav_core::{AudioFrame, CodecId, Rational};

    fn stream(rate: u32, channels: u16) -> StreamInfo {
        StreamInfo {
            index: 0,
            codec: CodecId::PcmF32Le,
            time_base: Rational { num: 1, den: rate as i64 },
            duration: None,
            bits: None,
            extradata: Vec::new(),
            lead_in: 0,
            params: StreamParams::Audio { sample_rate: rate, channels },
        }
    }

    /// `seconds` of a stereo sine of amplitude `amp` at `freq` Hz, in 0.1 s frames.
    fn sine(rate: u32, freq: f64, amp: f64, seconds: f64) -> Vec<AudioFrame> {
        let n = (rate as f64 * seconds) as usize;
        let per = rate as usize / 10;
        (0..n)
            .step_by(per)
            .map(|s0| AudioFrame {
                sample_rate: rate,
                channels: 2,
                pts: s0 as i64,
                samples: (s0..(s0 + per).min(n)).flat_map(|i| {
                    let v = amp * (2.0 * std::f64::consts::PI * freq * i as f64 / rate as f64).sin();
                    [v, v]
                }).collect(),
            })
            .collect()
    }

    fn feed(a: &mut dyn Analyzer, frames: &[AudioFrame], rate: u32) -> Vec<Observation> {
        let mut out = Vec::new();
        for f in frames {
            a.observe(&Input::Audio { frame: f, t: f.pts as f64 / rate as f64 }, &mut out);
        }
        out
    }

    #[test]
    fn a_stereo_sine_at_minus_23_dbfs_measures_minus_23_lufs() {
        // EBU Tech 3341 test 1: a 1 kHz stereo sine at -23 dBFS reads -23.0 LUFS (±0.1).
        for rate in [44100, 48000] {
            let mut l = Loudness::new(&stream(rate, 2));
            feed(&mut l, &sine(rate, 1000.0, 10f64.powf(-23.0 / 20.0), 20.0), rate);
            let s = l.finish(20.0, &mut Vec::new()).unwrap();
            let i = s["integrated_lufs"].as_f64().unwrap();
            assert!((i + 23.0).abs() <= 0.1, "{rate} Hz: integrated {i}");
            // a steady tone has no loudness range
            assert!(s["range_lu"].as_f64().unwrap().abs() <= 0.1, "{s}");
        }
    }

    #[test]
    fn loudness_range_of_two_levels_is_their_difference() {
        // EBU Tech 3342 test 1: 20 s at -20 dBFS then 20 s at -30 dBFS gives LRA 10 LU (±1).
        let rate = 48000;
        let mut frames = sine(rate, 1000.0, 10f64.powf(-20.0 / 20.0), 20.0);
        for mut f in sine(rate, 1000.0, 10f64.powf(-30.0 / 20.0), 20.0) {
            f.pts += 20 * rate as i64;
            frames.push(f);
        }
        let mut l = Loudness::new(&stream(rate, 2));
        feed(&mut l, &frames, rate);
        let lra = l.finish(40.0, &mut Vec::new()).unwrap()["range_lu"].as_f64().unwrap();
        assert!((lra - 10.0).abs() <= 1.0, "LRA {lra}");
    }

    #[test]
    fn silence_between_tones_is_one_span() {
        let rate = 48000;
        // a tone, two seconds of nothing, a tone; renumbered so the three parts follow each other
        let per = rate as i64 / 10;
        let frames: Vec<_> = [(0.5, 1.0), (0.0, 2.0), (0.5, 1.0)]
            .iter()
            .flat_map(|&(amp, secs)| sine(rate, 440.0, amp, secs))
            .enumerate()
            .map(|(k, mut f)| {
                f.pts = k as i64 * per;
                f
            })
            .collect();
        let mut s = Silence::new();
        let mut out = feed(&mut s, &frames, rate);
        s.finish(4.0, &mut out);
        let kinds: Vec<_> = out.iter().map(|o| o.kind).collect();
        assert_eq!(kinds, ["silence_start", "silence_end"], "{out:?}");
        assert!((out[1].t - 1.0).abs() < 0.02 && (out[1].t_end.unwrap() - 3.0).abs() < 0.02, "{out:?}");
    }

    #[test]
    fn full_scale_samples_are_reported_as_clipping() {
        let rate = 48000;
        let mut lv = Levels::default();
        let out = feed(&mut lv, &sine(rate, 100.0, 1.2f64.min(1.0), 0.5), rate);
        assert!(out.iter().any(|o| o.kind == "clipping"), "{out:?}");
        let s = lv.finish(0.5, &mut Vec::new()).unwrap();
        assert_eq!(s["peak_dbfs"].as_f64(), Some(0.0));
    }
}
