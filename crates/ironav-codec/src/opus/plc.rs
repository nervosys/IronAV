//! Concealment of a lost packet in voiced speech: the last pitch period is found in what was
//! decoded and repeated, fading, and the next frame that arrives is cross-faded in from it. This
//! is the idea of libopus's pitch concealment, done on the decoder's output, so it serves after
//! SILK and hybrid frames as well as CELT ones.

/// Output kept for the pitch search, per channel: 40 ms at 48 kHz.
const HIST: usize = 1920;
/// The pitch periods searched: 60 to 500 Hz.
const MIN_PERIOD: usize = 96;
const MAX_PERIOD: usize = 800;
/// The stretch of the latest output the periods are matched on (15 ms).
const WINDOW: usize = 720;
/// How far a repeated period matches for the speech to count as voiced.
const VOICED: f32 = 0.6;
/// Full level for the first 10 ms, then 3 dB less each 10 ms, and over after 100 ms.
const HOLD: usize = 480;
const LONGEST: usize = 4800;
/// The cross-fade into the next frame that arrives (2.5 ms).
const FADE: usize = 120;

pub(super) struct Plc {
    channels: usize,
    /// The latest output, oldest first.
    hist: [Vec<f32>; 2],
    /// The channels summed, for the search.
    mono: Vec<f32>,
    /// The period being repeated, per channel, and its length (0: none).
    period: [Vec<f32>; 2],
    len: usize,
    /// Samples given out since the loss began.
    pos: usize,
    /// Whether the last output was this concealment, so the next frame fades in from it.
    active: bool,
}

impl Plc {
    pub(super) fn new(channels: usize) -> Self {
        Self {
            channels,
            hist: [vec![0.0; HIST], vec![0.0; HIST]],
            mono: vec![0.0; HIST],
            period: [vec![0.0; MAX_PERIOD], vec![0.0; MAX_PERIOD]],
            len: 0,
            pos: 0,
            active: false,
        }
    }

    /// Takes `n` decoded samples of each plane, fading them in from the concealment if it was the
    /// last output.
    pub(super) fn decoded(&mut self, planes: &mut [Vec<f32>; 2], n: usize) {
        if self.active {
            let f = FADE.min(n);
            for c in 0..self.channels {
                for i in 0..f {
                    let w = (i as f32 + 0.5) / f as f32;
                    let e = self.sample(c, i);
                    planes[c][i] = w * planes[c][i] + (1.0 - w) * e;
                }
            }
        }
        (self.active, self.len, self.pos) = (false, 0, 0);
        self.remember(planes, n);
    }

    /// Fills `n` samples of each plane for a lost frame if the speech before it was voiced, and
    /// says whether it did; if not, the caller's own concealment stands.
    pub(super) fn conceal(&mut self, planes: &mut [Vec<f32>; 2], n: usize) -> bool {
        if self.pos == 0 && !self.active {
            self.len = self.search();
            for c in 0..self.channels {
                let l = self.len;
                self.period[c][..l].copy_from_slice(&self.hist[c][HIST - l..]);
            }
        }
        if self.len == 0 || self.pos >= LONGEST {
            self.active = false;
            self.remember(planes, n);
            return false;
        }
        for c in 0..self.channels {
            for i in 0..n {
                planes[c][i] = self.sample(c, i);
            }
        }
        self.pos += n;
        self.active = true;
        self.remember(planes, n);
        true
    }

    /// The `i`th sample from here of the repeated period, at its level then.
    fn sample(&self, c: usize, i: usize) -> f32 {
        let at = self.pos + i;
        if self.len == 0 || at >= LONGEST {
            return 0.0;
        }
        let gain = if at < HOLD { 1.0 } else { 10f32.powf(-0.15 * (at - HOLD) as f32 / 480.0) };
        // the last 10 ms fade out entirely
        let tail = ((LONGEST - at) as f32 / 480.0).min(1.0);
        self.period[c][at % self.len] * gain * tail
    }

    fn remember(&mut self, planes: &[Vec<f32>; 2], n: usize) {
        for c in 0..self.channels {
            let h = &mut self.hist[c];
            if n >= HIST {
                h.copy_from_slice(&planes[c][n - HIST..n]);
            } else {
                h.copy_within(n.., 0);
                h[HIST - n..].copy_from_slice(&planes[c][..n]);
            }
        }
    }

    /// The pitch period of the latest output, or 0 if it is not voiced.
    fn search(&mut self) -> usize {
        for i in 0..HIST {
            self.mono[i] = self.hist[0][i] + if self.channels > 1 { self.hist[1][i] } else { 0.0 };
        }
        let x = &self.mono;
        let at = HIST - WINDOW;
        let gain = |t: usize| {
            let (mut xy, mut xx, mut yy) = (0f32, 0f32, 0f32);
            for i in at..HIST {
                xy += x[i] * x[i - t];
                xx += x[i] * x[i];
                yy += x[i - t] * x[i - t];
            }
            if xy <= 0.0 || xx < 1e-6 {
                0.0
            } else {
                xy / (xx * yy).sqrt().max(1e-12)
            }
        };
        // a coarse search every other lag, then the neighbours of the best
        let mut best = (0usize, 0f32);
        for t in (MIN_PERIOD..=MAX_PERIOD).step_by(2) {
            let g = gain(t);
            if g > best.1 {
                best = (t, g);
            }
        }
        for t in [best.0.saturating_sub(1).max(MIN_PERIOD), (best.0 + 1).min(MAX_PERIOD)] {
            let g = gain(t);
            if g > best.1 {
                best = (t, g);
            }
        }
        // a multiple of the true period matches nearly as well; take the shortest that does
        for k in (2..=4).rev() {
            let s = (best.0 + k / 2) / k;
            if s < MIN_PERIOD {
                continue;
            }
            let mut sub = (s, 0f32);
            for t in s - 1..=s + 1 {
                let g = gain(t);
                if g > sub.1 {
                    sub = (t, g);
                }
            }
            if sub.1 > 0.9 * best.1 {
                best = sub;
                break;
            }
        }
        if best.1 >= VOICED {
            best.0
        } else {
            0
        }
    }
}
