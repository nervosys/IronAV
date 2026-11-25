//! Intensity, coherence, phase and interpolation from 3GPP SP-040428, §8.6.1.6.

use super::bands::project_coefficients;
use super::{Cx, Par, MAX_ENV, MAX_PAR, SLOTS};
use crate::aac::ps_tables::{K_TO_I_20, K_TO_I_34};
use std::f64::consts::{FRAC_PI_2, FRAC_PI_4, SQRT_2};
use std::sync::OnceLock;

const ICC: [f64; 8] = [1.0, 0.937, 0.84118, 0.60092, 0.36764, 0.0, -0.589, -1.0];
const COARSE_DB: [i8; 15] = [-25, -18, -14, -10, -7, -4, -2, 0, 2, 4, 7, 10, 14, 18, 25];
const FINE_DB: [i8; 31] = [
    -50, -45, -40, -35, -30, -25, -22, -19, -16, -13, -10, -8, -6, -4, -2, 0,
    2, 4, 6, 8, 10, 13, 16, 19, 22, 25, 30, 35, 40, 45, 50,
];

// The rows are the output channels. Their two entries weight the mono and decorrelated input.
type Gains = [[f32; 2]; 2];
type Rotations = [[[Gains; 8]; 46]; 2];

fn gains(db: i8, coherence: f64, rotation_b: bool) -> Gains {
    let ratio = 10.0f64.powf(f64::from(db) / 20.0);
    let rows = if rotation_b {
        let rho = coherence.max(0.05);
        let theta = (0.5 * (2.0 * ratio * rho).atan2(ratio * ratio - 1.0)).rem_euclid(FRAC_PI_2);
        let q = ratio + ratio.recip();
        let root_mu = (1.0 + (4.0 * rho * rho - 4.0) / (q * q)).sqrt();
        let gamma = ((1.0 - root_mu) / (1.0 + root_mu)).max(0.0).sqrt().atan();
        let (s_theta, c_theta) = theta.sin_cos();
        let (s_gamma, c_gamma) = gamma.sin_cos();
        [[SQRT_2 * c_theta * c_gamma, -SQRT_2 * s_theta * s_gamma], [SQRT_2 * s_theta * c_gamma, SQRT_2 * c_theta * s_gamma]]
    } else {
        let right = SQRT_2 / (1.0 + ratio * ratio).sqrt();
        let left = ratio * right;
        let angle = coherence.acos() / 2.0;
        let offset = angle * (right - left) / SQRT_2;
        let (left_sin, left_cos) = (offset + angle).sin_cos();
        let (right_sin, right_cos) = (offset - angle).sin_cos();
        [[left * left_cos, left * left_sin], [right * right_cos, right * right_sin]]
    };
    rows.map(|row| row.map(|gain| gain as f32))
}

fn gain_tables() -> &'static Rotations {
    static TABLES: OnceLock<Rotations> = OnceLock::new();
    TABLES.get_or_init(|| {
        std::array::from_fn(|rotation| {
            std::array::from_fn(|iid| {
                let db = COARSE_DB.get(iid).copied().unwrap_or_else(|| FINE_DB[iid - COARSE_DB.len()]);
                ICC.map(|rho| gains(db, rho, rotation == 1))
            })
        })
    })
}

#[derive(Clone, Copy, Default)]
struct PhaseHistory([usize; 2]);

impl PhaseHistory {
    fn add(&mut self, phase: i8) -> Cx {
        static PHASES: OnceLock<[[f64; 2]; 8]> = OnceLock::new();
        let unit = PHASES.get_or_init(|| {
            std::array::from_fn(|i| {
                let turn = FRAC_PI_4 * i as f64;
                [turn.cos(), turn.sin()]
            })
        });
        let [older, last] = self.0;
        let current = phase as usize & 7;
        self.0 = [last, current];
        let average = std::array::from_fn::<_, 2, _>(|i| 0.25 * unit[older][i] + 0.5 * unit[last][i] + unit[current][i]);
        let size = average[0].hypot(average[1]);
        average.map(|part| (part / size) as f32)
    }
}

#[derive(Clone, Copy, Default)]
struct Matrix {
    paths: [[Cx; 2]; 2],
}

impl Matrix {
    fn update(&mut self, from: Gains, phase: Option<[Cx; 2]>) {
        for (channel, (paths, weights)) in self.paths.iter_mut().zip(from).enumerate() {
            for (coefficient, weight) in paths.iter_mut().zip(weights) {
                if let Some(rotation) = phase {
                    *coefficient = rotation[channel].map(|part| part * weight);
                } else {
                    coefficient[0] = weight;
                }
            }
        }
    }

    fn interpolate(self, next: Self, part: f32, phase: bool, conjugate_start: bool) -> Self {
        let paths = std::array::from_fn(|channel| {
            std::array::from_fn(|input| {
                let [re, im] = self.paths[channel][input];
                let [end_re, end_im] = next.paths[channel][input];
                let im = if conjugate_start { -im } else { im };
                [re + part * (end_re - re), if phase { im + part * (end_im - im) } else { 0.0 }]
            })
        });
        Self { paths }
    }

    fn apply(self, mid: Cx, side: Cx) -> [Cx; 2] {
        self.paths.map(|[a, b]| {
            [
                a[0] * mid[0] + b[0] * side[0] - a[1] * mid[1] - b[1] * side[1],
                a[0] * mid[1] + b[0] * side[1] + a[1] * mid[0] + b[1] * side[0],
            ]
        })
    }
}

pub(super) struct Cues<'a> {
    pub(super) iid: &'a Par,
    pub(super) icc: &'a Par,
    pub(super) phase: Option<(&'a Par, &'a Par)>,
    pub(super) fine_iid: bool,
    pub(super) rotation_b: bool,
    pub(super) borders: &'a [i32],
    pub(super) previous_envelopes: usize,
    pub(super) wide: bool,
    pub(super) layout_changed: bool,
}

pub(super) struct Mixer {
    borders: [[Matrix; MAX_ENV + 1]; MAX_PAR],
    ipd: [PhaseHistory; MAX_PAR],
    opd: [PhaseHistory; MAX_PAR],
}

impl Mixer {
    pub(super) fn new() -> Self {
        Self {
            borders: [[Matrix::default(); MAX_ENV + 1]; MAX_PAR],
            ipd: [PhaseHistory::default(); MAX_PAR],
            opd: [PhaseHistory::default(); MAX_PAR],
        }
    }

    fn restore_boundary(&mut self, cues: &Cues<'_>) {
        if cues.previous_envelopes > 0 {
            for band in &mut self.borders {
                band[0] = band[cues.previous_envelopes.min(MAX_ENV)];
            }
        }
        if cues.layout_changed {
            for output in 0..2 {
                for input in 0..2 {
                    for component in 0..2 {
                        let mut row = std::array::from_fn(|band| self.borders[band][0].paths[output][input][component]);
                        project_coefficients(&mut row, cues.wide);
                        for (band, coefficient) in self.borders.iter_mut().zip(row) {
                            band[0].paths[output][input][component] = coefficient;
                        }
                    }
                }
            }
            self.ipd.fill(PhaseHistory::default());
            self.opd.fill(PhaseHistory::default());
        }
    }

    pub(super) fn apply(&mut self, mid: &mut [Cx], side: &mut [Cx], cues: Cues<'_>) {
        self.restore_boundary(&cues);
        let (band_map, band_count, phase_count): (&[u8], _, _) = if cues.wide { (&K_TO_I_34, 34, 17) } else { (&K_TO_I_20, 20, 11) };
        let templates = &gain_tables()[usize::from(cues.rotation_b)];
        let offset = if cues.fine_iid { 30 } else { 7 };
        for (band, edges) in self.borders[..band_count].iter_mut().enumerate() {
            for envelope in 0..cues.borders.len() - 1 {
                let iid = (i32::from(cues.iid[envelope][band]) + offset).clamp(0, 45) as usize;
                let values = templates[iid][cues.icc[envelope][band] as usize & 7];
                let phase = cues.phase.filter(|_| band < phase_count).map(|(ipd, opd)| {
                    let [pr, pi] = self.ipd[band].add(ipd[envelope][band]);
                    let [or, oi] = self.opd[band].add(opd[envelope][band]);
                    // The right channel uses the difference between the overall and inter-channel phases.
                    [[or, oi], [or * pr + oi * pi, oi * pr - or * pi]]
                });
                edges[envelope + 1].update(values, phase);
            }
        }

        for (subband, &parameter) in band_map.iter().enumerate() {
            let edges = &self.borders[usize::from(parameter)];
            let conjugate = if cues.wide { (9..=13).contains(&subband) } else { subband < 2 };
            for (envelope, bounds) in cues.borders.windows(2).enumerate() {
                let (start, end) = (bounds[0], bounds[1]);
                if end <= start || start < -1 || end >= SLOTS as i32 {
                    continue;
                }
                let first = subband * SLOTS + (start + 1) as usize;
                let length = (end - start) as usize;
                for slot in 0..length {
                    let proportion = (slot + 1) as f32 / length as f32;
                    let matrix = edges[envelope].interpolate(edges[envelope + 1], proportion, cues.phase.is_some(), conjugate);
                    let i = first + slot;
                    let [left, right] = matrix.apply(mid[i], side[i]);
                    mid[i] = left;
                    side[i] = right;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn centered_coherent_signal_sends_the_same_mono_to_both_outputs() {
        for rotation_b in [false, true] {
            let mut matrix = Matrix::default();
            matrix.update(gains(0, 1.0, rotation_b), None);
            let result = matrix.apply([1.0, 0.0], [0.0, 1.0]);
            for [re, im] in result {
                assert!((re - 1.0).abs() < 1e-6);
                assert!(im.abs() < 1e-6);
            }
        }
    }

    #[test]
    fn phase_filter_converges_to_a_constant_quarter_turn() {
        let mut phase = PhaseHistory::default();
        let _ = phase.add(2);
        let _ = phase.add(2);
        let [re, im] = phase.add(2);
        assert!(re.abs() < 1e-6);
        assert!((im - 1.0).abs() < 1e-6);
    }
}
