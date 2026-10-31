//! Voice activity: a cheap speech detector. A frame is judged in the speech band (100 Hz to 4 kHz)
//! against an adaptive noise floor, by its pitch (or, without one, its few zero crossings) and by
//! syllabic modulation. It is a gate for a transcription model, not a classifier: music with a
//! voice-like rhythm can pass it, while a steady tone, hum or hiss does not.

use crate::{round, Analyzer, Input, Observation};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::f64::consts::FRAC_1_SQRT_2;

/// Analysis frame, in seconds.
const FRAME: f64 = 0.02;
/// The speech band: a high-pass at the first and a low-pass at the second, in Hz.
const BAND_LOW: f64 = 100.0;
const BAND_HIGH: f64 = 4000.0;
/// The pitch search runs on the band decimated to about this rate.
const PITCH_RATE: f64 = 8000.0;
/// Voice pitch lies between these, in Hz.
const PITCH_MIN: f64 = 70.0;
const PITCH_MAX: f64 = 400.0;
/// A frame this many dB above the noise floor (in the band) may be speech...
const ABOVE_FLOOR: f64 = 6.0;
/// ...and must also be at least this loud (dBFS).
const MIN_DB: f64 = -55.0;
/// A frame whose normalized autocorrelation reaches this at a pitch lag is voiced.
const MIN_PERIODICITY: f64 = 0.5;
/// Zero crossings per sample (in the band) above this are hiss or noise, when the frame is not voiced.
const MAX_ZCR: f64 = 0.25;
/// Where the noise floor starts, how fast it rises (dB per frame: 1 dB/s) while nothing quieter
/// comes, and how low it goes: digital silence must not leave it where it takes a minute to recover.
const START_FLOOR: f64 = -50.0;
const FLOOR_RISE: f64 = 0.02;
const LOWEST_FLOOR: f64 = -75.0;
/// Speech comes in syllables: loud frames over the last second vary by at least this many dB
/// (standard deviation), and at least `MIN_LOUD` of them are needed to tell.
const MIN_MODULATION: f64 = 3.0;
const MIN_LOUD: usize = 10;
/// The frames in a second.
const SECOND: usize = 50;
/// Speech starts when `START_ACTIVE` of the last `START_WINDOW` frames are active, and ends after
/// `HANGOVER` inactive frames in a row.
const START_WINDOW: usize = 15;
const START_ACTIVE: usize = 10;
const HANGOVER: usize = 25;

/// A second-order filter (RBJ cookbook, Q of 1/sqrt 2), in direct form I.
#[derive(Clone, Copy)]
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    x: [f64; 2],
    y: [f64; 2],
}

impl Biquad {
    fn new(rate: f64, hz: f64, high: bool) -> Self {
        let (sin, cos) = (2.0 * std::f64::consts::PI * hz / rate).sin_cos();
        let alpha = sin * FRAC_1_SQRT_2;
        let a0 = 1.0 + alpha;
        let b = if high { [(1.0 + cos) / 2.0, -(1.0 + cos), (1.0 + cos) / 2.0] } else { [(1.0 - cos) / 2.0, 1.0 - cos, (1.0 - cos) / 2.0] };
        Biquad { b: b.map(|v| v / a0), a: [-2.0 * cos / a0, (1.0 - alpha) / a0], x: [0.0; 2], y: [0.0; 2] }
    }

    fn run(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.b[1] * self.x[0] + self.b[2] * self.x[1] - self.a[0] * self.y[0] - self.a[1] * self.y[1];
        self.x = [x, self.x[0]];
        self.y = [y, self.y[0]];
        y
    }
}

/// The per-frame speech classifier: a frame is speech when it is loud in the speech band, has a
/// pitch (or few zero crossings) and is among loud frames that vary as syllables do.
pub struct SpeechFrames {
    high: Biquad,
    /// None when the rate is too low for a low-pass to matter.
    low: Option<Biquad>,
    /// The decimation factor for the pitch search, and the decimated rate.
    block: usize,
    decimated: f64,
    lag_min: usize,
    lag_max: usize,
    /// The decimated band, the last 40 ms or more (as the pitch search needs), oldest first.
    history: Vec<f64>,
    floor: f64,
    /// The band levels of the loud frames among the last `SECOND` frames, oldest first; `None` for the others.
    last: VecDeque<Option<f64>>,
}

impl SpeechFrames {
    /// A classifier for frames of mono sound at `rate` samples a second.
    pub fn new(rate: u32) -> Self {
        let rate = rate.max(1) as f64;
        let block = ((rate / PITCH_RATE).round() as usize).max(1);
        let decimated = rate / block as f64;
        SpeechFrames {
            high: Biquad::new(rate, BAND_LOW, true),
            low: (rate / 2.0 > BAND_HIGH).then(|| Biquad::new(rate, BAND_HIGH, false)),
            block,
            decimated,
            lag_min: ((decimated / PITCH_MAX).floor() as usize).max(1),
            lag_max: (decimated / PITCH_MIN).ceil() as usize,
            history: Vec::new(),
            floor: START_FLOOR,
            last: VecDeque::with_capacity(SECOND + 1),
        }
    }

    /// Whether one frame of mono samples (about 20 ms) sounds like speech.
    pub fn process(&mut self, x: &[f64]) -> bool {
        let n = x.len().max(1) as f64;
        let band: Vec<f64> = x
            .iter()
            .map(|&v| {
                let h = self.high.run(v);
                self.low.as_mut().map_or(h, |l| l.run(h))
            })
            .collect();
        // the band is never louder than the frame itself: after an abrupt stop the filter rings on,
        // and that ring must not count as a loud frame
        let raw = 10.0 * (x.iter().map(|s| s * s).sum::<f64>() / n + 1e-12).log10();
        let e = (10.0 * (band.iter().map(|s| s * s).sum::<f64>() / n + 1e-12).log10()).min(raw);
        let zcr = band.windows(2).filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0)).count() as f64 / n;
        self.floor = if e < self.floor { e.max(LOWEST_FLOOR) } else { self.floor + FLOOR_RISE };
        let loud = e > self.floor + ABOVE_FLOOR && e > MIN_DB;
        self.last.push_back(loud.then_some(e));
        if self.last.len() > SECOND {
            self.last.pop_front();
        }
        // the history must be continuous, so it is kept whether or not the frame is judged
        let w = self.push_history(&band);
        let levels: Vec<f64> = self.last.iter().flatten().copied().collect();
        let modulated = levels.len() >= MIN_LOUD && {
            let m = levels.len() as f64;
            let mean = levels.iter().sum::<f64>() / m;
            (levels.iter().map(|l| (l - mean).powi(2)).sum::<f64>() / m).sqrt() >= MIN_MODULATION
        };
        if !(loud && modulated) {
            return false;
        }
        let voiced = self.periodicity(w) >= MIN_PERIODICITY;
        voiced || zcr < MAX_ZCR
    }

    /// Decimates the band to the pitch rate into the history, and returns the decimated length
    /// of this frame.
    fn push_history(&mut self, band: &[f64]) -> usize {
        let k = self.block as f64;
        let dec: Vec<f64> = band.chunks_exact(self.block).map(|c| c.iter().sum::<f64>() / k).collect();
        self.history.extend_from_slice(&dec);
        let keep = ((0.04 * self.decimated) as usize).max(dec.len() + self.lag_max);
        if self.history.len() > keep {
            self.history.drain(..self.history.len() - keep);
        }
        dec.len()
    }

    /// The largest normalized autocorrelation of the last `w` decimated samples with their past at
    /// a pitch lag.
    fn periodicity(&self, w: usize) -> f64 {
        let n = self.history.len();
        if w == 0 || w > n {
            return 0.0;
        }
        let a = &self.history[n - w..];
        let ea: f64 = a.iter().map(|v| v * v).sum();
        let mut best = 0.0f64;
        for lag in self.lag_min..=self.lag_max {
            if lag + w > n {
                break;
            }
            let b = &self.history[n - w - lag..n - lag];
            let eb: f64 = b.iter().map(|v| v * v).sum();
            if ea > 0.0 && eb > 0.0 {
                let num: f64 = a.iter().zip(b).map(|(p, q)| p * q).sum();
                best = best.max(num / (ea * eb).sqrt());
            }
        }
        best
    }
}

pub struct VoiceActivity {
    /// Mono samples not yet in a frame, and the time of the first.
    acc: Vec<f64>,
    acc_t: f64,
    rate: u32,
    speech: SpeechFrames,
    /// Start times and verdicts of the last `START_WINDOW` frames.
    recent: VecDeque<(f64, bool)>,
    /// The start of the speech under way, the end of its last active frame, and inactive frames since.
    start: Option<f64>,
    last_active: f64,
    quiet: usize,
    window: f64,
    total: f64,
    count: u64,
}

impl Default for VoiceActivity {
    fn default() -> Self {
        VoiceActivity {
            acc: Vec::new(),
            acc_t: 0.0,
            rate: 0,
            speech: SpeechFrames::new(16_000),
            recent: VecDeque::new(),
            start: None,
            last_active: 0.0,
            quiet: 0,
            window: 0.0,
            total: 0.0,
            count: 0,
        }
    }
}

impl VoiceActivity {
    /// Whether speech is under way.
    pub fn speaking(&self) -> bool {
        self.start.is_some()
    }

    fn frame(&mut self, t: f64, x: &[f64], out: &mut Vec<Observation>) {
        let dur = x.len() as f64 / self.rate as f64;
        let active = self.speech.process(x);
        self.recent.push_back((t, active));
        if self.recent.len() > START_WINDOW {
            self.recent.pop_front();
        }
        match self.start {
            None => {
                if self.recent.iter().filter(|r| r.1).count() >= START_ACTIVE {
                    let first = self.recent.iter().find(|r| r.1).map_or(t, |r| r.0);
                    self.start = Some(first);
                    self.last_active = t + dur;
                    self.quiet = 0;
                    self.count += 1;
                    self.window += t + dur - first;
                    self.total += t + dur - first;
                    out.push(Observation::new("speech_start", "voice_activity", first, Value::Object(Default::default())));
                }
            }
            Some(_) => {
                self.window += dur;
                self.total += dur;
                if active {
                    self.last_active = t + dur;
                    self.quiet = 0;
                } else {
                    self.quiet += 1;
                    if self.quiet >= HANGOVER {
                        // the hangover was not speech
                        let tail = self.quiet as f64 * dur;
                        self.window = (self.window - tail).max(0.0);
                        self.total = (self.total - tail).max(0.0);
                        self.end(out);
                    }
                }
            }
        }
    }

    fn end(&mut self, out: &mut Vec<Observation>) {
        if let Some(start) = self.start.take() {
            out.push(Observation::new("speech_end", "voice_activity", start, json!({ "duration": round(self.last_active - start, 3) })).until(self.last_active));
        }
        self.quiet = 0;
    }
}

impl Analyzer for VoiceActivity {
    fn observe(&mut self, input: &Input, out: &mut Vec<Observation>) {
        let Input::Audio { frame, t } = input else { return };
        let ch = frame.channels.max(1) as usize;
        if frame.sample_rate == 0 {
            return;
        }
        if frame.sample_rate != self.rate {
            self.rate = frame.sample_rate;
            self.speech = SpeechFrames::new(self.rate);
            self.acc.clear();
        }
        if self.acc.is_empty() {
            self.acc_t = *t;
        }
        self.acc.extend(frame.samples.chunks(ch).map(|c| c.iter().sum::<f64>() / ch as f64));
        let len = ((self.rate as f64 * FRAME) as usize).max(1);
        let mut used = 0;
        while self.acc.len() - used >= len {
            let x = self.acc[used..used + len].to_vec();
            let ft = self.acc_t;
            self.frame(ft, &x, out);
            used += len;
            self.acc_t += len as f64 / self.rate as f64;
        }
        self.acc.drain(..used);
    }

    fn digest(&mut self, seconds: f64) -> Option<Value> {
        let w = std::mem::take(&mut self.window);
        Some(json!({ "speech_seconds": round(w, 3), "speech_fraction": if seconds > 0.0 { round((w / seconds).min(1.0), 3) } else { 0.0 } }))
    }

    fn finish(&mut self, _t: f64, out: &mut Vec<Observation>) -> Option<Value> {
        self.end(out);
        Some(json!({ "spans": self.count, "seconds": round(self.total, 3) }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironav_core::AudioFrame;
    use std::f64::consts::PI;

    fn run(samples: &[f64], rate: u32) -> Vec<(String, f64, Option<f64>)> {
        let mut v = VoiceActivity::default();
        let mut out = Vec::new();
        for (k, c) in samples.chunks(1024).enumerate() {
            let f = AudioFrame { sample_rate: rate, channels: 1, pts: 0, samples: c.to_vec() };
            v.observe(&Input::Audio { frame: &f, t: (k * 1024) as f64 / rate as f64 }, &mut out);
        }
        v.finish(0.0, &mut out);
        out.iter().map(|o| (o.kind.to_string(), o.t, o.t_end)).collect()
    }

    /// A voiced sound: harmonics of 120 Hz under a 4 Hz syllable envelope, from `a` to `b` seconds.
    pub(crate) fn voice(rate: u32, secs: f64, a: f64, b: f64) -> Vec<f64> {
        (0..(rate as f64 * secs) as usize)
            .map(|i| {
                let t = i as f64 / rate as f64;
                let noise = 0.001 * ((i * 7919 % 1000) as f64 / 500.0 - 1.0);
                if t < a || t >= b {
                    return noise;
                }
                let env = 0.5 - 0.5 * (2.0 * PI * 4.0 * (t - a)).cos();
                let h: f64 = (1..8).map(|k| (2.0 * PI * 120.0 * k as f64 * t).sin() / k as f64).sum();
                noise + 0.2 * env * h
            })
            .collect()
    }

    #[test]
    fn speech_is_found_and_a_steady_tone_is_not() {
        let got = run(&voice(16000, 6.0, 1.0, 4.0), 16000);
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0].0, "speech_start");
        assert!((got[0].1 - 1.0).abs() < 0.3, "{got:?}");
        let end = got[1].2.unwrap();
        assert!((end - 4.0).abs() < 0.3, "{got:?}");
        // a 1 kHz tone between silences
        let rate = 48000;
        let tone: Vec<f64> = (0..rate * 6).map(|i| if (rate..rate * 4).contains(&i) { 0.1 * (2.0 * PI * 1000.0 * i as f64 / rate as f64).sin() } else { 0.0 }).collect();
        assert_eq!(run(&tone, rate as u32), []);
        // white noise
        let mut x = 1u32;
        let hiss: Vec<f64> = (0..48000 * 3)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 17;
                x ^= x << 5;
                0.2 * (x as f64 / u32::MAX as f64 - 0.5)
            })
            .collect();
        assert_eq!(run(&hiss, 48000), []);
    }

    /// Deterministic white noise, uniform from -0.5 to 0.5 (a variance of 1/12).
    fn white(n: usize, seed: u32) -> Vec<f64> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                (s >> 8) as f64 / (1u32 << 24) as f64 - 0.5
            })
            .collect()
    }

    /// `x` with white noise added, at `snr` dB below the mean power of `x` from `a` to `b` seconds.
    fn with_noise(x: &[f64], rate: u32, a: f64, b: f64, snr: f64, seed: u32) -> Vec<f64> {
        let span = &x[(a * rate as f64) as usize..(b * rate as f64) as usize];
        let power = span.iter().map(|v| v * v).sum::<f64>() / span.len() as f64;
        let scale = (power / 10f64.powf(snr / 10.0) * 12.0).sqrt();
        x.iter().zip(white(x.len(), seed)).map(|(v, n)| v + scale * n).collect()
    }

    #[test]
    fn speech_in_noise_is_found() {
        let rate = 16000;
        let clean = voice(rate, 6.0, 1.0, 4.0);
        for snr in [10.0, 5.0] {
            let got = run(&with_noise(&clean, rate, 1.0, 4.0, snr, 7), rate);
            let found: f64 = got.iter().filter(|o| o.0 == "speech_end").map(|o| o.2.unwrap() - o.1).sum();
            assert!(found >= 0.7 * 3.0, "{snr} dB: {found} s of 3 s, {got:?}");
        }
    }

    #[test]
    fn noise_tones_and_hum_are_not_speech() {
        let rate = 16000;
        let n = rate as usize * 6;
        let mut y = 0.0;
        let pinkish: Vec<f64> = white(n, 3)
            .iter()
            .map(|w| {
                y = 0.95 * y + w;
                0.2 * y
            })
            .collect();
        let at = |f: &dyn Fn(f64) -> f64| -> Vec<f64> { (0..n).map(|i| f(i as f64 / rate as f64)).collect() };
        let hum = at(&|t| (1..=20).map(|k| 0.1 / k as f64 * (2.0 * PI * 50.0 * k as f64 * t).sin()).sum());
        let cases = [
            ("white noise", white(n, 5).iter().map(|w| 0.4 * w).collect()),
            ("pink-ish noise", pinkish),
            ("a steady 440 Hz tone", at(&|t| 0.1 * (2.0 * PI * 440.0 * t).sin())),
            ("50 Hz hum with harmonics", hum),
            ("silence", vec![0.0; n]),
        ];
        for (name, x) in cases {
            let got = run(&x, rate);
            assert!(got.iter().all(|o| o.0 != "speech_start"), "{name}: {got:?}");
        }
    }
}
