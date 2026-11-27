//! Effects run on the decoder worker, before samples reach the device queue.

/// Center frequencies of the five peaking EQ bands.
pub const EQ_FREQUENCIES: [u32; 5] = [80, 250, 1000, 4000, 12000];

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct AudioEffects {
    /// Boost or cut each band by up to 12 dB.
    pub eq_db: [f32; EQ_FREQUENCIES.len()],
    /// Amount of linked-channel dynamic compression, 0 to 1.
    pub compression: f32,
    /// Mix of a 180 ms echo, 0 to 1.
    pub echo: f32,
}

impl AudioEffects {
    pub(crate) fn clamped(self) -> Self {
        fn limit(value: f32, min: f32, max: f32) -> f32 {
            if value.is_finite() { value.clamp(min, max) } else { 0.0 }
        }
        Self {
            eq_db: self.eq_db.map(|gain| limit(gain, -12.0, 12.0)),
            compression: limit(self.compression, 0.0, 1.0),
            echo: limit(self.echo, 0.0, 1.0),
        }
    }
}

#[derive(Clone, Copy)]
struct Peak {
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
}

impl Peak {
    fn new(gain: f32, center: u32, rate: u32) -> Option<Self> {
        if gain == 0.0 || center as f32 >= rate as f32 * 0.48 {
            return None;
        }
        let a = 10f32.powf(gain / 40.0);
        let (sin, cos) = (std::f32::consts::TAU * center as f32 / rate as f32).sin_cos();
        let alpha = sin / 2.0; // Q = 1
        let norm = 1.0 + alpha / a;
        let b1 = -2.0 * cos / norm;
        Some(Self { b0: (1.0 + alpha * a) / norm, b1, b2: (1.0 - alpha * a) / norm, a1: b1, a2: (1.0 - alpha / a) / norm })
    }

    fn process(self, sample: f32, state: &mut [f32; 2]) -> f32 {
        let output = self.b0 * sample + state[0];
        state[0] = self.b1 * sample - self.a1 * output + state[1];
        state[1] = self.b2 * sample - self.a2 * output;
        output
    }

    fn response_db(self, sin: f64, cos: f64, sin2: f64, cos2: f64) -> f64 {
        let power = |c0: f32, c1: f32, c2: f32| {
            let (c0, c1, c2) = (f64::from(c0), f64::from(c1), f64::from(c2));
            (c0 + c1 * cos + c2 * cos2).powi(2) + (c1 * sin + c2 * sin2).powi(2)
        };
        let ratio = power(self.b0, self.b1, self.b2) / power(1.0, self.a1, self.a2).max(f64::MIN_POSITIVE);
        10.0 * ratio.max(f64::MIN_POSITIVE).log10()
    }
}

fn equalizer_filters(rate: u32, settings: AudioEffects) -> [Option<Peak>; EQ_FREQUENCIES.len()] {
    let settings = settings.clamped();
    std::array::from_fn(|band| Peak::new(settings.eq_db[band], EQ_FREQUENCIES[band], rate))
}

pub(crate) fn equalizer_coefficients(rate: u32, settings: AudioEffects) -> [[f32; 8]; EQ_FREQUENCIES.len()] {
    equalizer_filters(rate, settings).map(|filter| filter.map_or(
        [1.0, 0.0, 0.0, 0.0, 1.0, 0.0, 0.0, 0.0],
        |f| [f.b0, f.b1, f.b2, 0.0, 1.0, f.a1, f.a2, 0.0],
    ))
}

pub(crate) fn equalizer_response(rate: u32, settings: AudioEffects, frequencies: &[f32], db: &mut [f32]) {
    let filters = equalizer_filters(rate, settings);
    for (output, &frequency) in db.iter_mut().zip(frequencies) {
        if !(0.0..=rate as f32 * 0.5).contains(&frequency) {
            *output = f32::NAN;
            continue;
        }
        let angle = std::f64::consts::TAU * f64::from(frequency) / f64::from(rate);
        let (sin, cos) = angle.sin_cos();
        let (sin2, cos2) = (2.0 * sin * cos, cos * cos - sin * sin);
        *output = filters.iter().flatten().map(|filter| filter.response_db(sin, cos, sin2, cos2)).sum::<f64>() as f32;
    }
}

pub(crate) struct EffectsProcessor {
    rate: u32,
    channels: usize,
    settings: AudioEffects,
    filters: [Option<Peak>; EQ_FREQUENCIES.len()],
    states: Vec<[[f32; 2]; EQ_FREQUENCIES.len()]>,
    envelope: f32,
    attack: f32,
    release: f32,
    threshold: f32,
    ratio_exponent: f32,
    delay: Vec<f32>,
    cursor: usize,
}

impl EffectsProcessor {
    pub fn new(rate: u32, channels: usize) -> Self {
        Self {
            rate, channels, settings: AudioEffects::default(),
            filters: [None; EQ_FREQUENCIES.len()],
            states: vec![[[0.0; 2]; EQ_FREQUENCIES.len()]; channels],
            envelope: 0.0, attack: (-1.0 / (0.008 * rate as f32)).exp(),
            release: (-1.0 / (0.12 * rate as f32)).exp(),
            threshold: 1.0, ratio_exponent: 0.0,
            delay: Vec::new(), cursor: 0,
        }
    }

    pub fn reset(&mut self) {
        self.states.fill([[0.0; 2]; EQ_FREQUENCIES.len()]);
        self.envelope = 0.0;
        self.delay.fill(0.0);
        self.cursor = 0;
    }

    fn configure(&mut self, settings: AudioEffects) {
        if settings == self.settings {
            return;
        }
        self.filters = equalizer_filters(self.rate, settings);
        for band in 0..self.filters.len() {
            if self.filters[band].is_none() {
                for state in &mut self.states {
                    state[band] = [0.0; 2];
                }
            }
        }
        self.threshold = 10f32.powf(-1.2 * settings.compression);
        self.ratio_exponent = 1.0 - (1.0 + 3.0 * settings.compression).recip();
        if settings.compression == 0.0 {
            self.envelope = 0.0;
        }
        if settings.echo > 0.0 && self.delay.is_empty() {
            let frames = (f64::from(self.rate) * 0.18).round().max(1.0) as usize;
            self.delay.resize(frames * self.channels, 0.0);
        } else if settings.echo == 0.0 && !self.delay.is_empty() {
            self.delay.clear();
            self.cursor = 0;
        }
        self.settings = settings;
    }

    pub fn process(&mut self, samples: &mut [f32], settings: AudioEffects) {
        self.configure(settings);
        if settings == AudioEffects::default() {
            return;
        }
        for frame in samples.chunks_exact_mut(self.channels) {
            for (sample, states) in frame.iter_mut().zip(&mut self.states) {
                for (filter, state) in self.filters.iter().zip(states) {
                    if let Some(filter) = filter {
                        *sample = filter.process(*sample, state);
                    }
                }
            }
            if settings.compression > 0.0 {
                let peak = frame.iter().fold(0.0f32, |peak, sample| peak.max(sample.abs()));
                let smoothing = if peak > self.envelope { self.attack } else { self.release };
                self.envelope = peak + smoothing * (self.envelope - peak);
                if self.envelope > self.threshold {
                    let gain = (self.threshold / self.envelope).powf(self.ratio_exponent);
                    for sample in frame.iter_mut() {
                        *sample *= gain;
                    }
                }
            }
            if !self.delay.is_empty() {
                let delayed = &mut self.delay[self.cursor..self.cursor + self.channels];
                for (sample, delay) in frame.iter_mut().zip(delayed) {
                    let echo = *delay;
                    *delay = *sample + echo * 0.28;
                    *sample = *sample * (1.0 - settings.echo * 0.35) + echo * settings.echo * 0.65;
                }
                self.cursor = (self.cursor + self.channels) % self.delay.len();
            }
            for sample in frame.iter_mut() {
                *sample = sample.clamp(-1.0, 1.0);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flat_is_exact_and_the_mid_band_boosts_a_tone() {
        let tone: Vec<f32> = (0..4800).map(|n| (n as f32 * std::f32::consts::TAU / 48.0).sin() * 0.1).collect();
        let mut samples = tone.clone();
        let mut processor = EffectsProcessor::new(48_000, 1);
        processor.process(&mut samples, AudioEffects::default());
        assert_eq!(samples, tone);
        let mut eq_db = [0.0; EQ_FREQUENCIES.len()];
        eq_db[2] = 12.0;
        processor.process(&mut samples, AudioEffects { eq_db, ..Default::default() });
        let peak = samples[2400..].iter().copied().fold(0.0, f32::max);
        assert!((0.38..0.42).contains(&peak), "{peak}");
        let mut response = [0.0];
        equalizer_response(48_000, AudioEffects { eq_db, ..Default::default() }, &[1000.0], &mut response);
        assert!((response[0] - 20.0 * (peak / 0.1).log10()).abs() < 0.1);
    }

    #[test]
    fn echo_is_delayed_and_compression_reduces_a_sustained_peak() {
        let mut processor = EffectsProcessor::new(1000, 2);
        let mut impulse = vec![0.0; 400];
        impulse[0] = 1.0;
        processor.process(&mut impulse, AudioEffects { echo: 1.0, ..Default::default() });
        assert_eq!(impulse[360], 0.65);
        assert_eq!(impulse[361], 0.0, "echo stays on the originating channel");

        let mut loud = vec![0.8; 2000];
        processor.process(&mut loud, AudioEffects { compression: 1.0, ..Default::default() });
        assert!(loud[1999] < 0.25);
        assert_eq!(loud[1998], loud[1999], "compression links both channels");
    }
}
