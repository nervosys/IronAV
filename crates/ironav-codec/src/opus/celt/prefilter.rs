//! The encoder's pitch pre-filter: a comb filter that takes out the signal's pitch before the
//! transform, so the bands code what is left; the decoder's post-filter puts it back. Voiced
//! speech gains most, since its harmonics would otherwise spread bits over every band.

use super::*;

/// The longest pitch period the filter takes, in samples, and the history it keeps.
pub const MAX_PERIOD: usize = 1024;
const MIN_PERIOD: usize = POSTFILTER_MINPERIOD as usize;

/// What the decoder needs to undo a frame's pre-filter: the period, the gain (0 to 7, for
/// 3/32 to 24/32) and the tap set.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct PitchParams {
    pub period: usize,
    pub qg: u32,
    pub tapset: usize,
}

/// The pre-filter's state: the unfiltered history and the filter the last frame used.
pub struct Prefilter {
    hist: [Vec<f32>; 2],
    /// History and frame together, unfiltered, per channel.
    pre: [Vec<f32>; 2],
    /// The decimated, channel-summed signal the pitch is searched on.
    low: Vec<f32>,
    period: usize,
    gain: f32,
    tapset: usize,
}

impl Default for Prefilter {
    fn default() -> Self {
        Self {
            hist: [vec![0.0; MAX_PERIOD], vec![0.0; MAX_PERIOD]],
            pre: [vec![0.0; MAX_PERIOD + MAX_FRAME], vec![0.0; MAX_PERIOD + MAX_FRAME]],
            low: vec![0.0; (MAX_PERIOD + MAX_FRAME) / 2],
            period: MIN_PERIOD,
            gain: 0.0,
            tapset: 0,
        }
    }
}

impl Prefilter {
    /// Starts again from no filtering, as a decoder's post-filter does after a reset, keeping
    /// the input's history.
    pub fn restart(&mut self) {
        (self.period, self.gain, self.tapset) = (MIN_PERIOD, 0.0, 0);
    }

    /// Keeps the input's history up to date over a frame of `n` samples (each slice as
    /// [`run`](Self::run) takes it) that is coded without the pre-filter.
    pub fn skip(&mut self, sig: &[&[f32]], n: usize) {
        for (h, s) in self.hist.iter_mut().zip(sig) {
            let x = &s[OVERLAP..OVERLAP + n];
            if n >= MAX_PERIOD {
                h.copy_from_slice(&x[n - MAX_PERIOD..]);
            } else {
                h.copy_within(n.., 0);
                h[MAX_PERIOD - n..].copy_from_slice(x);
            }
        }
    }
}

/// `x . y` and the energies of both.
fn corr(x: &[f32], y: &[f32]) -> (f32, f32, f32) {
    let (mut xy, mut xx, mut yy) = (0f32, 0f32, 0f32);
    for (a, b) in x.iter().zip(y) {
        xy += a * b;
        xx += a * a;
        yy += b * b;
    }
    (xy, xx, yy)
}

/// How well the frame (from `at` in `x`, `n` long) matches itself `t` earlier, as a gain from 0 to 1.
fn pitch_gain(x: &[f32], at: usize, n: usize, t: usize) -> f32 {
    let (xy, xx, yy) = corr(&x[at..at + n], &x[at - t..at - t + n]);
    if xy <= 0.0 {
        0.0
    } else {
        (xy / (1.0 + xx * yy).sqrt()).min(1.0)
    }
}

impl Prefilter {
    /// The pitch period of the `n` new samples of `pre` (after the `MAX_PERIOD` of history),
    /// and how strong it is.
    fn search(&mut self, channels: usize, n: usize) -> (usize, f32) {
        // half rate, both channels summed, lightly low-passed
        let len = (MAX_PERIOD + n) / 2;
        for i in 0..len {
            let mut v = 0f32;
            for c in 0..channels {
                let x = &self.pre[c];
                let k = 2 * i;
                v += 0.5 * x[k] + 0.25 * (if k > 0 { x[k - 1] } else { 0.0 } + x[k + 1]);
            }
            self.low[i] = v;
        }
        let (at, m) = (MAX_PERIOD / 2, n / 2);
        let target = &self.low[at..at + m];
        let mut best = (0usize, 0f32);
        for t in MIN_PERIOD / 2..MAX_PERIOD / 2 - 1 {
            let (xy, _, yy) = corr(target, &self.low[at - t..at - t + m]);
            if xy > 0.0 {
                let score = xy * xy / (yy + 1e-9);
                if score > best.1 {
                    best = (t, score);
                }
            }
        }
        if best.0 == 0 {
            return (MIN_PERIOD, 0.0);
        }
        // refine at the full rate, on the summed channels
        let mut mix = [0f32; MAX_PERIOD + MAX_FRAME];
        for c in 0..channels {
            for (m, v) in mix.iter_mut().zip(&self.pre[c][..MAX_PERIOD + n]) {
                *m += v;
            }
        }
        let mut t = 2 * best.0;
        let mut g = pitch_gain(&mix, MAX_PERIOD, n, t);
        for cand in (2 * best.0).saturating_sub(2)..=(2 * best.0 + 2).min(MAX_PERIOD - 2) {
            if cand >= MIN_PERIOD {
                let gc = pitch_gain(&mix, MAX_PERIOD, n, cand);
                if gc > g {
                    (t, g) = (cand, gc);
                }
            }
        }
        // a multiple of the true period matches nearly as well; take the shortest that does
        for k in (2..=6).rev() {
            let s = (t + k / 2) / k;
            if s < MIN_PERIOD {
                continue;
            }
            let mut gs = 0f32;
            let mut ts = s;
            for cand in s.saturating_sub(1).max(MIN_PERIOD)..=s + 1 {
                let gc = pitch_gain(&mix, MAX_PERIOD, n, cand);
                if gc > gs {
                    (ts, gs) = (cand, gc);
                }
            }
            if gs > 0.85 * g {
                (t, g) = (ts, gs);
                break;
            }
        }
        (t, g)
    }

    /// Filters the `n` new samples of each channel in `sig` (after its `OVERLAP` samples of the
    /// last frame) in place, and says how, or `None` when it is off. `enabled` is false when the
    /// frame has too few bits for the parameters or is silent: the filter then fades out.
    pub fn run(&mut self, sig: &mut [&mut [f32]], n: usize, enabled: bool, bytes: usize) -> Option<PitchParams> {
        let channels = sig.len();
        for c in 0..channels {
            self.pre[c][..MAX_PERIOD].copy_from_slice(&self.hist[c]);
            self.pre[c][MAX_PERIOD..MAX_PERIOD + n].copy_from_slice(&sig[c][OVERLAP..OVERLAP + n]);
        }
        let (mut period, mut gain) = if enabled { self.search(channels, n) } else { (MIN_PERIOD, 0.0) };
        period = period.clamp(MIN_PERIOD, MAX_PERIOD - 2);
        gain *= 0.7;
        // libopus's thresholds: harder to turn on for a new period or a small frame
        let mut threshold = 0.2f32;
        if period.abs_diff(self.period) * 10 > period {
            threshold += 0.2;
        }
        if bytes < 25 {
            threshold += 0.1;
        }
        if bytes < 35 {
            threshold += 0.1;
        }
        if self.gain > 0.4 {
            threshold -= 0.1;
        }
        if self.gain > 0.55 {
            threshold -= 0.1;
        }
        threshold = threshold.max(0.2);
        let params = if gain < threshold {
            gain = 0.0;
            None
        } else {
            if (gain - self.gain).abs() < 0.1 {
                gain = self.gain;
            }
            let qg = ((0.5 + gain * 32.0 / 3.0).floor() as i32 - 1).clamp(0, 7) as u32;
            gain = 0.09375 * (qg + 1) as f32;
            Some(PitchParams { period, qg, tapset: 0 })
        };
        let tapset = 0;
        for c in 0..channels {
            comb(&mut sig[c][OVERLAP..OVERLAP + n], &self.pre[c], MAX_PERIOD, (self.period, period), (-self.gain, -gain), (self.tapset, tapset));
            self.hist[c].copy_from_slice(&self.pre[c][n..n + MAX_PERIOD]);
        }
        (self.period, self.gain, self.tapset) = (period, gain, tapset);
        params
    }
}

/// `y = x + g * (taps around x[i - T])`, changing from the old period, gain and tap set to the
/// new over the window's overlap. `x` is read from `base` on; `y` holds as many samples.
fn comb(y: &mut [f32], x: &[f32], base: usize, t: (usize, usize), g: (f32, f32), tapset: (usize, usize)) {
    let n = y.len();
    let (t0, t1) = (t.0.max(MIN_PERIOD), t.1.max(MIN_PERIOD));
    let tap = |s: usize, gain: f32| [gain * CELT_POSTFILTER_TAPS[s][0], gain * CELT_POSTFILTER_TAPS[s][1], gain * CELT_POSTFILTER_TAPS[s][2]];
    let (a, b) = (tap(tapset.0, g.0), tap(tapset.1, g.1));
    let at = |i: usize, back: usize, k: isize| x[((base + i - back) as isize + k) as usize];
    let overlap = if g.0 == g.1 && t0 == t1 && tapset.0 == tapset.1 { 0 } else { OVERLAP.min(n) };
    for i in 0..n {
        let new = b[0] * at(i, t1, 0) + b[1] * (at(i, t1, 1) + at(i, t1, -1)) + b[2] * (at(i, t1, 2) + at(i, t1, -2));
        y[i] = x[base + i]
            + if i < overlap {
                let f = window(i) * window(i);
                let old = a[0] * at(i, t0, 0) + a[1] * (at(i, t0, 1) + at(i, t0, -1)) + a[2] * (at(i, t0, 2) + at(i, t0, -2));
                (1.0 - f) * old + f * new
            } else {
                new
            };
    }
}
