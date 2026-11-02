//! Sample-rate conversion: a polyphase windowed-sinc filter for any ratio of two rates.
//!
//! Each output sample is the input convolved with a Kaiser-windowed sinc centred on the output's
//! instant, so there is no delay. The cut-off is the lower of the two Nyquist frequencies, which
//! keeps anything above it from aliasing when the rate goes down. One set of filter taps is
//! precomputed per fractional position (`L` of them for a ratio `L / M`).

mod stream;

pub use stream::StreamResampler;

#[cfg(feature = "filters")]
use ironav_core::{AudioFrame, Error, Filter, Frame, Result, StreamParams};
use std::f64::consts::PI;

/// Input samples on each side of the centre in the passband, before widening for a lower cut-off.
const HALF_WIDTH: f64 = 16.0;
const KAISER_BETA: f64 = 9.0;

fn bessel_i0(x: f64) -> f64 {
    let (mut sum, mut term) = (1.0, 1.0);
    let q = x * x / 4.0;
    for k in 1..60 {
        term *= q / (k * k) as f64;
        sum += term;
        if term < sum * 1e-18 {
            break;
        }
    }
    sum
}

fn gcd(a: u64, b: u64) -> u64 {
    if b == 0 { a } else { gcd(b, a % b) }
}

// The `Resample` filter: the sample-rate conversion behind the registry's `resample` entry.
#[cfg(feature = "filters")]
struct State {
    channels: usize,
    /// Output samples per `m` input samples.
    l: usize,
    m: usize,
    taps: usize,
    /// `l` rows of `taps` coefficients.
    table: Vec<f64>,
    /// Input samples not yet out of reach of a future output, per channel, from index `base`.
    hist: Vec<Vec<f64>>,
    base: i64,
    /// Index of the next output sample.
    next: i64,
    /// Input samples received per channel.
    received: i64,
    in_rate: u32,
    started: bool,
}

/// The filter for a conversion: `(l, m, half, table)`, where `l / m` is the output samples per
/// input sample in lowest terms, `half` the input samples on each side of an output's instant,
/// and `table` the `l` rows of `2 * half` coefficients, one row per fractional position.
pub(crate) fn design(in_rate: u32, out_rate: u32) -> (usize, usize, usize, Vec<f64>) {
    {
        let g = gcd(in_rate as u64, out_rate as u64);
        let (l, m) = ((out_rate as u64 / g) as usize, (in_rate as u64 / g) as usize);
        // the cut-off, as a fraction of the input's Nyquist frequency
        let fc = (out_rate as f64 / in_rate as f64).min(1.0);
        let half = (HALF_WIDTH / fc).ceil() as usize;
        let taps = 2 * half;
        let norm = bessel_i0(KAISER_BETA);
        let mut table = vec![0.0; l * taps];
        for p in 0..l {
            let frac = p as f64 / l as f64;
            let row = &mut table[p * taps..(p + 1) * taps];
            for (k, c) in row.iter_mut().enumerate() {
                let tau = frac + half as f64 - 1.0 - k as f64;
                let x = fc * tau;
                let sinc = if x.abs() < 1e-12 { 1.0 } else { (PI * x).sin() / (PI * x) };
                let r = tau / half as f64;
                let win = if r.abs() >= 1.0 { 0.0 } else { bessel_i0(KAISER_BETA * (1.0 - r * r).sqrt()) / norm };
                *c = fc * sinc * win;
            }
            let sum: f64 = row.iter().sum();
            for c in row.iter_mut() {
                *c /= sum;
            }
        }
        (l, m, half, table)
    }
}

#[cfg(feature = "filters")]
impl State {
    fn new(in_rate: u32, out_rate: u32, channels: usize) -> Self {
        let (l, m, half, table) = design(in_rate, out_rate);
        Self { channels, l, m, taps: 2 * half, table, hist: vec![Vec::new(); channels], base: -(half as i64 - 1), next: 0, received: 0, in_rate, started: false }
    }

    fn half(&self) -> i64 {
        (self.taps / 2) as i64
    }

    /// Append interleaved input and return the output samples that are now complete, interleaved.
    /// `limit` stops at that output index (for the end of the stream).
    fn run(&mut self, input: &[f64], limit: Option<i64>) -> Vec<f64> {
        let ch = self.channels;
        if !self.started {
            self.started = true;
            // before the first sample the signal is zero
            let lead = (self.half() - 1) as usize;
            for h in self.hist.iter_mut() {
                h.resize(lead, 0.0);
            }
        }
        for (c, h) in self.hist.iter_mut().enumerate() {
            h.extend(input.iter().skip(c).step_by(ch));
        }
        self.received += (input.len() / ch) as i64;
        let mut out = Vec::new();
        loop {
            if limit.is_some_and(|l| self.next >= l) {
                break;
            }
            let pos = self.next * self.m as i64;
            let (i0, p) = (pos / self.l as i64, (pos % self.l as i64) as usize);
            // the window covers input i0 - half + 1 ..= i0 + half; it must all have arrived
            let last = i0 + self.half();
            if last >= self.base + self.hist[0].len() as i64 {
                break;
            }
            let first = (i0 - self.half() + 1 - self.base) as usize;
            let row = &self.table[p * self.taps..(p + 1) * self.taps];
            for h in &self.hist {
                out.push(h[first..first + self.taps].iter().zip(row).map(|(a, b)| a * b).sum());
            }
            self.next += 1;
        }
        // forget what no later output reaches
        let pos = self.next * self.m as i64;
        let keep_from = pos / self.l as i64 - self.half() + 1;
        let drop = (keep_from - self.base).max(0) as usize;
        if drop > 0 {
            for h in self.hist.iter_mut() {
                h.drain(..drop.min(h.len()));
            }
            self.base += drop as i64;
        }
        out
    }
}

/// The `resample` filter.
#[cfg(feature = "filters")]
pub struct Resample {
    pub rate: u32,
    state: Option<State>,
}

#[cfg(feature = "filters")]
impl Resample {
    pub fn new(rate: u32) -> Self {
        Self { rate, state: None }
    }
}

#[cfg(feature = "filters")]
impl Filter for Resample {
    fn output_params(&self, input: &StreamParams) -> StreamParams {
        match input {
            StreamParams::Audio { channels, .. } => StreamParams::Audio { sample_rate: self.rate, channels: *channels },
            other => other.clone(),
        }
    }

    fn process(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        let Frame::Audio(a) = frame else {
            return Err(Error::invalid_input("the resample filter received a video frame"));
        };
        if a.sample_rate == self.rate {
            return Ok(vec![Frame::Audio(a)]);
        }
        let channels = a.channels as usize;
        let st = self.state.get_or_insert_with(|| State::new(a.sample_rate, self.rate, channels));
        if st.channels != channels || st.in_rate != a.sample_rate {
            return Err(Error::invalid_input("the audio changed sample rate or channel count mid-stream"));
        }
        let first = st.next;
        let samples = st.run(&a.samples, None);
        if samples.is_empty() {
            return Ok(Vec::new());
        }
        Ok(vec![Frame::Audio(AudioFrame { sample_rate: self.rate, channels: a.channels, pts: first, samples })])
    }

    fn flush(&mut self) -> Result<Vec<Frame>> {
        let Some(st) = self.state.as_mut() else { return Ok(Vec::new()) };
        // as many output samples as the input's length asks for; the window needs zeros past its end
        let total = (st.received * st.l as i64 + st.m as i64 - 1) / st.m as i64;
        let first = st.next;
        let zeros = vec![0.0; st.taps * st.channels];
        let samples = st.run(&zeros, Some(total));
        if samples.is_empty() {
            return Ok(Vec::new());
        }
        Ok(vec![Frame::Audio(AudioFrame { sample_rate: self.rate, channels: st.channels as u16, pts: first, samples })])
    }
}

#[cfg(all(test, feature = "filters"))]
mod tests {
    use super::*;

    fn run(rate_in: u32, rate_out: u32, hz: f64, secs: f64) -> Vec<f64> {
        let n = (rate_in as f64 * secs) as usize;
        let x: Vec<f64> = (0..n).map(|i| 0.5 * (2.0 * PI * hz * i as f64 / rate_in as f64).sin()).collect();
        let mut f = Resample::new(rate_out);
        let mut out = Vec::new();
        for chunk in x.chunks(1000) {
            for fr in f.process(Frame::Audio(AudioFrame { sample_rate: rate_in, channels: 1, pts: 0, samples: chunk.to_vec() })).unwrap() {
                let Frame::Audio(a) = fr else { panic!() };
                out.extend(a.samples);
            }
        }
        for fr in f.flush().unwrap() {
            let Frame::Audio(a) = fr else { panic!() };
            out.extend(a.samples);
        }
        out
    }

    #[test]
    fn a_tone_keeps_its_frequency_phase_and_length() {
        for (a, b) in [(44_100u32, 48_000u32), (48_000, 44_100), (22_050, 48_000), (96_000, 48_000), (8_000, 48_000), (48_000, 16_000)] {
            let hz = 1000.0;
            let out = run(a, b, hz, 0.5);
            let want = (a as f64 * 0.5 * b as f64 / a as f64).ceil() as usize;
            assert_eq!(out.len(), want, "{a}->{b}");
            // away from the ends the output is the same sinusoid, sampled at the new rate
            for (j, &v) in out.iter().enumerate().skip(200).take(out.len() - 400) {
                let want = 0.5 * (2.0 * PI * hz * j as f64 / b as f64).sin();
                assert!((v - want).abs() < 2e-3, "{a}->{b} sample {j}: {v} vs {want}");
            }
        }
    }

    #[test]
    fn what_the_new_rate_cannot_hold_is_removed_not_aliased() {
        // 30 kHz at 96 kHz has no place at 48 kHz (Nyquist 24 kHz) and must not fold down to 18 kHz
        let out = run(96_000, 48_000, 30_000.0, 0.2);
        let peak = out.iter().skip(200).take(out.len() - 400).fold(0f64, |m, v| m.max(v.abs()));
        assert!(peak < 0.01, "{peak}");
    }
}
