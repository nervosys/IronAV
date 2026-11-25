//! Parametric stereo decorrelation from 3GPP SP-040428, §8.6.1.5.

use super::{Cx, MAX_PAR, SLOTS};
use crate::aac::ps_tables::{K_TO_I_20, K_TO_I_34};
use std::f64::consts::PI;
use std::sync::OnceLock;

const SUBBANDS: usize = K_TO_I_34.len();
const TAPS: [usize; 3] = [3, 4, 5];
const REFLECTION: [f32; 3] = [0.651_439_07, 0.564_718_1, 0.489_541_66];
const LONG_DELAY: usize = 14;

struct Layout {
    parameter_bands: &'static [u8],
    all_pass_end: usize,
    short_delay_start: usize,
    decay_start: usize,
}

// Full-rate constants from §8.6.1.5.1. Use the existing decoder's preset at all rates.
static LAYOUTS: [Layout; 2] = [
    Layout { parameter_bands: &K_TO_I_20, all_pass_end: 30, short_delay_start: 42, decay_start: 10 },
    Layout { parameter_bands: &K_TO_I_34, all_pass_end: 50, short_delay_start: 62, decay_start: 32 },
];

/// One sample's power estimate and the two low-pass outputs in §8.6.1.5.3.
#[derive(Clone, Copy, Default)]
struct Attenuator {
    peak: f32,
    power: f32,
    peak_excess: f32,
}

impl Attenuator {
    fn gain(&mut self, power: f32) -> f32 {
        const PEAK_DECAY: f32 = 0.765_928_3;
        const SMOOTHING: f32 = 0.25;
        const TRANSIENT_IMPACT: f32 = 1.5;
        self.peak = power.max(PEAK_DECAY * self.peak);
        self.power += SMOOTHING * (power - self.power);
        self.peak_excess += SMOOTHING * (self.peak - power - self.peak_excess);
        let transient = TRANSIENT_IMPACT * self.peak_excess;
        if transient > self.power { self.power / transient } else { 1.0 }
    }
}

#[derive(Clone, Copy)]
struct History<const CAPACITY: usize> {
    samples: [Cx; CAPACITY],
    next: usize,
}

impl<const CAPACITY: usize> Default for History<CAPACITY> {
    fn default() -> Self {
        Self { samples: [[0.0; 2]; CAPACITY], next: 0 }
    }
}

impl<const CAPACITY: usize> History<CAPACITY> {
    fn read(&self, delay: usize) -> Cx {
        self.samples[(self.next + CAPACITY - delay) % CAPACITY]
    }

    fn write(&mut self, sample: Cx) {
        self.samples[self.next] = sample;
        self.next = (self.next + 1) % CAPACITY;
    }
}

/// State of the serial delay and the three all-pass links in one hybrid subband.
#[derive(Clone, Copy, Default)]
struct DelayState {
    input: History<LONG_DELAY>,
    links: [History<5>; 3],
}

#[derive(Clone, Copy)]
struct AllPass {
    input_rotation: Cx,
    link_rotations: [Cx; 3],
    feedback: [f32; 3],
}

fn product(a: Cx, b: Cx) -> Cx {
    [a[0] * b[0] - a[1] * b[1], a[0] * b[1] + a[1] * b[0]]
}

impl DelayState {
    fn delay(&mut self, sample: Cx, lag: usize) -> Cx {
        let delayed = self.input.read(lag);
        self.input.write(sample);
        delayed
    }

    fn all_pass(&mut self, sample: Cx, filter: &AllPass) -> Cx {
        let delayed = self.delay(sample, 2);
        let mut value = product(delayed, filter.input_rotation);
        for (i, history) in self.links.iter_mut().enumerate() {
            // For each section, y = Q z^-d (x + a y) - a x. Store x + a y in its ring.
            let rotated = product(history.read(TAPS[i]), filter.link_rotations[i]);
            let a = filter.feedback[i];
            let filtered = [rotated[0] - a * value[0], rotated[1] - a * value[1]];
            history.write([value[0] + a * filtered[0], value[1] + a * filtered[1]]);
            value = filtered;
        }
        value
    }
}

fn center_frequency(extended: bool, band: usize) -> f64 {
    // Tables 8.14 and 8.15, expressed with a common denominator for each grid.
    const CENTER_20: [i32; 10] = [-3, -1, 1, 3, 5, 7, 10, 14, 18, 22];
    const CENTER_34: [i32; 32] = [
        2, 6, 10, 14, 18, 22, 26, 30, 34, -10, -6, -2, 51, 57, 15, 21,
        27, 33, 39, 45, 54, 66, 78, 42, 102, 66, 78, 90, 102, 114, 126, 90,
    ];
    if extended {
        CENTER_34.get(band).map_or(band as f64 - 26.5, |&v| f64::from(v) / 24.0)
    } else {
        CENTER_20.get(band).map_or(band as f64 - 6.5, |&v| f64::from(v) / 8.0)
    }
}

fn filters(extended: bool) -> &'static [AllPass] {
    static FILTERS: OnceLock<[Vec<AllPass>; 2]> = OnceLock::new();
    &FILTERS.get_or_init(|| {
        std::array::from_fn(|grid| {
            let layout = &LAYOUTS[grid];
            (0..layout.all_pass_end).map(|band| {
                let frequency = center_frequency(grid == 1, band);
                let rotation = |fraction: f64| {
                    let angle = -PI * fraction * frequency;
                    [angle.cos() as f32, angle.sin() as f32]
                };
                let damping = (1.0 - 0.05 * band.saturating_sub(layout.decay_start) as f32).max(0.0);
                AllPass {
                    input_rotation: rotation(0.39),
                    link_rotations: [0.43, 0.75, 0.347].map(rotation),
                    feedback: REFLECTION.map(|a| a * damping),
                }
            }).collect()
        })
    })[usize::from(extended)]
}

pub(super) struct Decorrelator {
    energy: [Attenuator; MAX_PAR],
    delay: Box<[DelayState]>,
}

impl Decorrelator {
    pub(super) fn new() -> Self {
        Self { energy: [Attenuator::default(); MAX_PAR], delay: vec![DelayState::default(); SUBBANDS].into_boxed_slice() }
    }

    pub(super) fn process(&mut self, mono: &[Cx], side: &mut [Cx], extended: bool, layout_changed: bool, top: usize) {
        let layout = &LAYOUTS[usize::from(extended)];
        let map = layout.parameter_bands;
        let all_pass = filters(extended);
        if layout_changed {
            // Preserve the decoder's delay-history reset at a change of hybrid layout.
            self.energy.fill(Attenuator::default());
            self.delay.fill(DelayState::default());
        } else {
            self.delay[top.min(map.len())..map.len()].fill(DelayState::default());
        }

        let (mono_bands, _) = mono.as_chunks::<SLOTS>();
        for slot in 0..SLOTS {
            let mut band_gain = [0.0; MAX_PAR];
            for (&band, samples) in map.iter().zip(mono_bands) {
                let [re, im] = samples[slot];
                band_gain[usize::from(band)] += re * re + im * im;
            }
            let count = if extended { 34 } else { 20 };
            for (gain, envelope) in band_gain[..count].iter_mut().zip(&mut self.energy) {
                *gain = envelope.gain(*gain);
            }
            for (subband, &band) in map.iter().enumerate() {
                let at = subband * SLOTS + slot;
                let history = &mut self.delay[subband];
                let value = if let Some(filter) = all_pass.get(subband) {
                    history.all_pass(mono[at], filter)
                } else {
                    let lag = if subband < layout.short_delay_start { LONG_DELAY } else { 1 };
                    history.delay(mono[at], lag)
                };
                let gain = band_gain[usize::from(band)];
                side[at] = [gain * value[0], gain * value[1]];
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn zero_feedback_cascades_the_four_delays() {
        let mut state = DelayState::default();
        let filter = AllPass { input_rotation: [1.0, 0.0], link_rotations: [[1.0, 0.0]; 3], feedback: [0.0; 3] };
        for slot in 0..40 {
            let input = if slot == 0 { [1.0, -2.0] } else { [0.0; 2] };
            let expected = if slot == 2 + 3 + 4 + 5 { [1.0, -2.0] } else { [0.0; 2] };
            assert_eq!(state.all_pass(input, &filter), expected, "slot {slot}");
        }
    }

    #[test]
    fn long_delay_retains_an_impulse_across_frames() {
        let mut state = DelayState::default();
        for slot in 0..2 * SLOTS {
            let input = if slot == SLOTS - 1 { [1.0, 0.0] } else { [0.0; 2] };
            let expected = if slot == SLOTS - 1 + LONG_DELAY { [1.0, 0.0] } else { [0.0; 2] };
            assert_eq!(state.delay(input, LONG_DELAY), expected, "slot {slot}");
        }
    }

    #[test]
    fn steady_power_gain_is_one_and_decays_after_an_impulse() {
        let mut steady = Attenuator::default();
        assert!((0..128).all(|_| steady.gain(1.0) == 1.0));
        let mut impulse = Attenuator::default();
        assert_eq!(impulse.gain(1.0), 1.0);
        assert!(impulse.gain(0.0) < 1.0);
    }
}
