//! Speech processing for a voice link, a frame at a time: a voice activity detector, a gate that
//! trims the silence at the ends of a push-to-talk press, automatic gain control and a light
//! noise suppressor. Mono `f32` samples in -1 to 1, at any rate.

use crate::mdct::Fft;
use ironav_core::{Error, Result};

/// The level of `x` in dB relative to full scale (a full-scale square wave is 0).
fn level_db(x: &[f32]) -> f32 {
    let e = x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32;
    10.0 * (e + 1e-12).log10()
}

/// A cheap speech detector: a frame is active when it stands well above an adaptive noise floor
/// and is not hiss (too many zero crossings). Speech starts after two active frames in a row,
/// and lasts until `hangover_ms` without one, so the ends of words are kept.
pub struct Vad {
    floor: f32,
    /// Floor rise per frame (1 dB a second).
    rise: f32,
    run: u32,
    quiet: u32,
    hangover: u32,
    speaking: bool,
    level: f32,
}

impl Vad {
    /// A detector for frames of `frame_ms`.
    pub fn new(frame_ms: f64, hangover_ms: f64) -> Self {
        Self { floor: -50.0, rise: (frame_ms / 1000.0) as f32, run: 0, quiet: 0, hangover: (hangover_ms / frame_ms).ceil() as u32, speaking: false, level: -120.0 }
    }

    /// Whether speech is under way after this frame.
    pub fn process(&mut self, frame: &[f32]) -> bool {
        let e = level_db(frame);
        self.level = e;
        let zcr = frame.windows(2).filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0)).count() as f32 / frame.len().max(1) as f32;
        // the floor follows the quietest frames down at once and creeps up
        self.floor = if e < self.floor { e.max(-75.0) } else { self.floor + self.rise };
        let active = e > self.floor + 10.0 && e > -55.0 && zcr < 0.35;
        if active {
            self.run += 1;
            self.quiet = 0;
            if self.run >= 2 {
                self.speaking = true;
            }
        } else {
            self.run = 0;
            if self.speaking {
                self.quiet += 1;
                if self.quiet > self.hangover {
                    self.speaking = false;
                }
            }
        }
        self.speaking
    }

    pub fn speaking(&self) -> bool {
        self.speaking
    }

    /// The last frame's level and the noise floor, in dBFS.
    pub fn levels(&self) -> (f32, f32) {
        (self.level, self.floor)
    }
}

/// Trims a push-to-talk press: frames before the speech starts and after it has ended (past the
/// detector's hangover) are held back. With `preroll` frames, the ones just before the start are
/// sent with it, so a soft first sound is not cut; they go out at once, which the receiver's
/// playout buffer absorbs as jitter.
pub struct Gate {
    vad: Vad,
    frame: usize,
    /// The last `preroll` frames, oldest first from `start`.
    ring: Vec<f32>,
    slots: usize,
    held: usize,
    start: usize,
    /// Frames ready to send, in `ring`'s order, then the current one.
    out: Vec<f32>,
    ready: usize,
    taken: usize,
    started: bool,
}

impl Gate {
    /// A gate for frames of `frame` samples lasting `frame_ms`.
    pub fn new(frame: usize, frame_ms: f64, preroll: usize, hangover_ms: f64) -> Self {
        Self {
            vad: Vad::new(frame_ms, hangover_ms),
            frame,
            ring: vec![0.0; preroll.max(1) * frame],
            slots: preroll,
            held: 0,
            start: 0,
            out: vec![0.0; (preroll + 1) * frame],
            ready: 0,
            taken: 0,
            started: false,
        }
    }

    /// Starts a new press: the next speech is its start.
    pub fn reset(&mut self) {
        (self.held, self.start, self.ready, self.taken, self.started) = (0, 0, 0, 0, false);
    }

    /// Takes one frame; returns how many frames to send now (take them with [`next`](Self::next)).
    pub fn push(&mut self, frame: &[f32]) -> Result<usize> {
        if frame.len() != self.frame {
            return Err(Error::invalid_arg(format!("the gate takes frames of {} samples, not {}", self.frame, frame.len())));
        }
        let speech = self.vad.process(frame);
        let f = self.frame;
        (self.ready, self.taken) = (0, 0);
        if speech {
            if !self.started {
                self.started = true;
                for k in 0..self.held {
                    let s = (self.start + k) % self.slots;
                    self.out[k * f..(k + 1) * f].copy_from_slice(&self.ring[s * f..(s + 1) * f]);
                }
                self.ready = self.held;
                self.held = 0;
            }
            self.out[self.ready * f..(self.ready + 1) * f].copy_from_slice(frame);
            self.ready += 1;
        } else if !self.started && self.slots > 0 {
            // before the speech: keep the last few for the preroll
            let s = (self.start + self.held) % self.slots;
            self.ring[s * f..(s + 1) * f].copy_from_slice(frame);
            if self.held < self.slots {
                self.held += 1;
            } else {
                self.start = (self.start + 1) % self.slots;
            }
        } else if self.started {
            // the speech has ended: a new start needs speech again
            self.started = false;
            (self.held, self.start) = (0, 0);
        }
        Ok(self.ready)
    }

    /// The next frame to send after a [`push`](Self::push), oldest first.
    // A lending iterator: the item borrows from `self`, which `Iterator::next` cannot express.
    #[allow(clippy::should_implement_trait)]
    pub fn next(&mut self) -> Option<&[f32]> {
        if self.taken >= self.ready {
            return None;
        }
        let k = self.taken;
        self.taken += 1;
        Some(&self.out[k * self.frame..(k + 1) * self.frame])
    }

    pub fn speaking(&self) -> bool {
        self.vad.speaking()
    }
}

/// Automatic gain control: brings speech to `target_dbfs`, measuring only frames the caller says
/// are speech, so pauses and noise are not pumped up. The gain falls quickly when the voice is too
/// loud and rises slowly (about 6 dB a second at 20 ms frames), changes smoothly within a frame,
/// and is held down so no sample clips.
pub struct Agc {
    target: f32,
    max_gain: f32,
    /// The gain now, in dB.
    gain: f32,
}

impl Agc {
    pub fn new(target_dbfs: f32, max_gain_db: f32) -> Self {
        Self { target: target_dbfs, max_gain: max_gain_db, gain: 0.0 }
    }

    /// The gain now, in dB.
    pub fn gain_db(&self) -> f32 {
        self.gain
    }

    /// Applies the gain to `frame` in place, after updating it from the frame if `speech`.
    pub fn process(&mut self, frame: &mut [f32], speech: bool) {
        let before = self.gain;
        if speech {
            let want = (self.target - level_db(frame)).clamp(-20.0, self.max_gain);
            self.gain += (want - self.gain) * if want < self.gain { 0.5 } else { 0.02 };
        }
        // no sample may go past full scale
        let peak = frame.iter().fold(0f32, |m, v| m.max(v.abs()));
        if peak > 0.0 {
            let most = 20.0 * (0.98 / peak).log10();
            self.gain = self.gain.min(most);
        }
        let (g0, g1) = (10f32.powf(before / 20.0), 10f32.powf(self.gain / 20.0));
        let n = frame.len().max(1) as f32;
        for (i, v) in frame.iter_mut().enumerate() {
            let g = g0 + (g1 - g0) * (i + 1) as f32 / n;
            *v = (*v * g).clamp(-0.99, 0.99);
        }
    }
}

/// A light noise suppressor: each `hop` samples, a spectrum of the last `2 * hop` (a square-root
/// Hann window) is scaled bin by bin by a gain from the noise it has learned (each bin's
/// quietest level, creeping up), never below `floor_db` so the voice keeps its body and nothing
/// "underwater" is heard, and added back. It delays the signal by `hop` samples.
pub struct Denoiser {
    hop: usize,
    fft: Fft,
    window: Vec<f32>,
    /// The last `2 * hop` input samples.
    input: Vec<f32>,
    /// The second half of the last hop's output, still to be overlap-added.
    overlap: Vec<f32>,
    /// The finished hop.
    done: Vec<f32>,
    spec: Vec<[f32; 2]>,
    work: Vec<[f32; 2]>,
    /// Each bin's power, smoothed over a few hops.
    smooth: Vec<f32>,
    /// Each bin's quietest smoothed power: the noise, before its bias is taken out.
    noise: Vec<f32>,
    prev_gain: Vec<f32>,
    prev_power: Vec<f32>,
    frames: u32,
    floor: f32,
    rate: u32,
}

impl Denoiser {
    /// A suppressor at `rate` that works in hops of `hop` samples (its delay); `2 * hop` must have
    /// no prime factor above 5 (80 at 16 kHz and 240 at 48 kHz, 5 ms, do).
    pub fn new(rate: u32, hop: usize, floor_db: f32) -> Result<Self> {
        let n = 2 * hop;
        let mut rest = n;
        for p in [2, 3, 5] {
            while rest > 1 && rest.is_multiple_of(p) {
                rest /= p;
            }
        }
        if hop == 0 || rest != 1 {
            return Err(Error::invalid_arg(format!("a hop of {hop} samples gives a transform of {n}, which has a prime factor above 5")));
        }
        let window = (0..n).map(|i| (std::f64::consts::PI * (i as f64 + 0.5) / n as f64).sin() as f32).collect();
        Ok(Self {
            hop,
            fft: Fft::new(n),
            window,
            input: vec![0.0; n],
            overlap: vec![0.0; hop],
            done: vec![0.0; hop],
            spec: vec![[0.0; 2]; n],
            work: vec![[0.0; 2]; n],
            smooth: vec![0.0; n / 2 + 1],
            noise: vec![0.0; n / 2 + 1],
            prev_gain: vec![1.0; n / 2 + 1],
            prev_power: vec![0.0; n / 2 + 1],
            frames: 0,
            floor: 10f32.powf(floor_db / 20.0),
            rate,
        })
    }

    /// The delay it adds, in milliseconds.
    pub fn delay_ms(&self) -> f64 {
        self.hop as f64 * 1000.0 / self.rate as f64
    }

    /// Suppresses noise in `frame` (a whole number of hops) in place; what comes out is `hop`
    /// samples late.
    pub fn process(&mut self, frame: &mut [f32]) -> Result<()> {
        if !frame.len().is_multiple_of(self.hop) {
            return Err(Error::invalid_arg(format!("the suppressor takes whole hops of {} samples, not {}", self.hop, frame.len())));
        }
        let h = self.hop;
        for k in 0..frame.len() / h {
            self.input.copy_within(h.., 0);
            self.input[h..].copy_from_slice(&frame[k * h..(k + 1) * h]);
            self.step();
            frame[k * h..(k + 1) * h].copy_from_slice(&self.done);
        }
        Ok(())
    }

    /// One hop: analyse the last two hops, scale, and overlap-add; `tail` then holds the finished hop.
    fn step(&mut self) {
        let n = 2 * self.hop;
        for i in 0..n {
            self.work[i] = [self.input[i] * self.window[i], 0.0];
        }
        self.fft.run(&self.work, &mut self.spec);
        self.frames = self.frames.saturating_add(1);
        // the noise: each bin's quietest smoothed level, followed down at once and up by about
        // 3 dB a second; the minimum of a noisy level sits below its mean, hence the bias
        let creep = 1.0 + 0.7 * self.hop as f32 / self.rate as f32;
        for b in 0..=n / 2 {
            let [re, im] = self.spec[b];
            let p = re * re + im * im;
            self.smooth[b] = if self.frames == 1 { p } else { 0.7 * self.smooth[b] + 0.3 * p };
            let s = self.smooth[b].max(1e-12);
            self.noise[b] = if self.frames == 1 || s < self.noise[b] { s } else { self.noise[b] * creep };
            let nb = 2.0 * self.noise[b];
            // the decision-directed estimate of the clean signal's share
            let post = (p / nb - 1.0).max(0.0);
            let prior = 0.98 * self.prev_gain[b] * self.prev_gain[b] * self.prev_power[b] / nb + 0.02 * post;
            // the square root of the Wiener gain: gentler on speech, which matters more than the last dB of noise
            let g = (prior / (1.0 + prior)).sqrt().max(self.floor);
            self.prev_gain[b] = g;
            self.prev_power[b] = p;
            for k in [b, (n - b) % n] {
                self.spec[k] = [self.spec[k][0] * g, self.spec[k][1] * g];
            }
        }
        // the inverse transform, as the forward one on the conjugate
        for i in 0..n {
            self.spec[i][1] = -self.spec[i][1];
        }
        self.fft.run(&self.spec, &mut self.work);
        // the first half finishes the last hop's second half; the second half waits for the next
        let scale = 1.0 / n as f32;
        let h = self.hop;
        for i in 0..h {
            self.done[i] = self.overlap[i] + self.work[i][0] * scale * self.window[i];
            self.overlap[i] = self.work[h + i][0] * scale * self.window[h + i];
        }
    }
}

/// What a stretch of sound is like, for a detector deciding whether to look closer.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct Spectrum {
    /// Root-mean-square and peak level, in dBFS (a full-scale square wave is 0).
    pub rms_dbfs: f32,
    pub peak_dbfs: f32,
    /// The power-weighted mean frequency, in Hz: low for a hum, high for hiss.
    pub centroid_hz: f32,
    /// Below this frequency lies 85% of the power.
    pub rolloff_hz: f32,
    /// Geometric over arithmetic mean of the power spectrum: near 1 for noise, near 0 for a tone.
    pub flatness: f32,
}

/// Measures [`Spectrum`]s: the power spectrum is averaged over Hann-windowed blocks of 512
/// samples, half overlapping.
pub struct SpectrumMeter {
    fft: Fft,
    window: Vec<f32>,
    work: Vec<[f32; 2]>,
    spec: Vec<[f32; 2]>,
    power: Vec<f32>,
}

const SPECTRUM_BLOCK: usize = 512;

impl Default for SpectrumMeter {
    fn default() -> Self {
        let n = SPECTRUM_BLOCK;
        Self {
            fft: Fft::new(n),
            window: (0..n).map(|i| (0.5 - 0.5 * (2.0 * std::f64::consts::PI * i as f64 / n as f64).cos()) as f32).collect(),
            work: vec![[0.0; 2]; n],
            spec: vec![[0.0; 2]; n],
            power: vec![0.0; n / 2 + 1],
        }
    }
}

impl SpectrumMeter {
    /// The spectrum of `x`, mono at `rate`. Shorter than a block, it is padded with silence.
    pub fn measure(&mut self, x: &[f32], rate: u32) -> Spectrum {
        let n = SPECTRUM_BLOCK;
        let level = |e: f32| 10.0 * (e + 1e-12).log10();
        let peak = x.iter().fold(0f32, |m, v| m.max(v.abs()));
        let mut out = Spectrum { rms_dbfs: level(x.iter().map(|v| v * v).sum::<f32>() / x.len().max(1) as f32), peak_dbfs: level(peak * peak), ..Spectrum::default() };
        self.power.fill(0.0);
        let mut start = 0;
        loop {
            for i in 0..n {
                self.work[i] = [x.get(start + i).copied().unwrap_or(0.0) * self.window[i], 0.0];
            }
            self.fft.run(&self.work, &mut self.spec);
            for (p, [re, im]) in self.power.iter_mut().zip(&self.spec) {
                *p += re * re + im * im;
            }
            start += n / 2;
            if start + n > x.len() {
                break;
            }
        }
        let total: f32 = self.power.iter().sum();
        if total <= 1e-12 {
            return out;
        }
        let hz = rate as f32 / n as f32;
        out.centroid_hz = self.power.iter().enumerate().map(|(b, p)| b as f32 * hz * p).sum::<f32>() / total;
        let mut run = 0.0;
        for (b, p) in self.power.iter().enumerate() {
            run += p;
            if run >= 0.85 * total {
                out.rolloff_hz = b as f32 * hz;
                break;
            }
        }
        let bins = self.power.len() as f32;
        let log_mean = self.power.iter().map(|p| (p + 1e-20).ln()).sum::<f32>() / bins;
        out.flatness = (log_mean.exp() / (total / bins)).min(1.0);
        out
    }
}
