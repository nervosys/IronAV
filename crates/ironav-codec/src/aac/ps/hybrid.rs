//! Hybrid QMF filtering from 3GPP SP-040428, §8.6.1.3 and §8.6.1.7.

use super::{Cx, SLOTS};
use std::f64::consts::TAU;
use std::sync::OnceLock;

const HISTORY: usize = 6;
const WINDOW: usize = 2 * HISTORY + 1;
const QMF_SLOTS: usize = SLOTS + HISTORY;
const QMF_BANDS: usize = 64;

type Kernel = [Cx; WINDOW];

// Symmetric prototypes. The entries run from the oldest tap to the centre tap.
// Table 8.11 covers the narrow grid and Table 8.12 the wide grid.
const NARROW_FIRST: [f64; 7] = [0.00746082949812, 0.02270420949825, 0.04546865930473, 0.07266113929591, 0.09885108575264, 0.11793710567217, 0.125];
const NARROW_TWO: [f64; 7] = [0.0, 0.01899487526049, 0.0, -0.07293139167538, 0.0, 0.30596630545168, 0.5];
const WIDE_FIRST: [f64; 7] = [0.04081179924692, 0.03812810994926, 0.05144908135699, 0.06399831151592, 0.07428313801106, 0.08100347892914, 1.0 / 12.0];
const WIDE_SECOND: [f64; 7] = [0.01565675600122, 0.03752716391991, 0.05417891378782, 0.08417044116767, 0.10307344158036, 0.12222452249753, 0.125];
const WIDE_FOUR: [f64; 7] = [-0.05908211155639, -0.04871498374946, 0.0, 0.07778723915851, 0.16486303567403, 0.23279856662996, 0.25];

// Output order of the split banks from Figures 8.3 and 8.5. A pair adds two filter outputs.
const NARROW_ROUTES: [&[(usize, Option<usize>)]; 3] = [
    &[(6, None), (7, None), (0, None), (1, None), (2, Some(5)), (3, Some(4))],
    &[(1, None), (0, None)],
    &[(0, None), (1, None)],
];
const WIDE_COUNTS: [usize; 5] = [12, 8, 4, 4, 4];

struct Kernels {
    narrow_first: Vec<Kernel>,
    narrow_two: Vec<Kernel>,
    wide_first: Vec<Kernel>,
    wide_second: Vec<Kernel>,
    wide_four: Vec<Kernel>,
}

fn kernels(prototype: &[f64; 7], channels: usize, complex: bool) -> Vec<Kernel> {
    (0..channels).map(|channel| {
        let shift = if complex { 0.5 } else { 0.0 };
        std::array::from_fn(|tap| {
            let weight = prototype[tap.min(2 * HISTORY - tap)];
            let angle = TAU * (channel as f64 + shift) * (HISTORY as f64 - tap as f64) / channels as f64;
            [(weight * angle.cos()) as f32, if complex { (weight * angle.sin()) as f32 } else { 0.0 }]
        })
    }).collect()
}

fn filterbank() -> &'static Kernels {
    static FILTERBANK: OnceLock<Kernels> = OnceLock::new();
    FILTERBANK.get_or_init(|| Kernels {
        narrow_first: kernels(&NARROW_FIRST, 8, true),
        narrow_two: kernels(&NARROW_TWO, 2, false),
        wide_first: kernels(&WIDE_FIRST, 12, true),
        wide_second: kernels(&WIDE_SECOND, 8, true),
        wide_four: kernels(&WIDE_FOUR, 4, true),
    })
}

fn convolve(input: &[Cx], kernel: &Kernel) -> Cx {
    let mut result = [0.0; 2];
    for (&[re, im], &[cos, sin]) in input.iter().zip(kernel) {
        result[0] += re * cos - im * sin;
        result[1] += im * cos + re * sin;
    }
    result
}

#[derive(Default)]
pub(super) struct HybridBank {
    past: [[Cx; HISTORY]; 5],
}

impl HybridBank {
    pub(super) fn analyze(&mut self, input: &[[f32; QMF_BANDS]], output: &mut [Cx], wide: bool) {
        let bank = filterbank();
        let kernels: [&[Kernel]; 5] = if wide {
            [&bank.wide_first, &bank.wide_second, &bank.wide_four, &bank.wide_four, &bank.wide_four]
        } else {
            [&bank.narrow_first, &bank.narrow_two, &bank.narrow_two, &[], &[]]
        };
        let mut hybrid_start = 0;
        for (qmf, filters) in kernels.into_iter().enumerate() {
            let mut samples = [[0.0; 2]; SLOTS + 2 * HISTORY];
            samples[..HISTORY].copy_from_slice(&self.past[qmf]);
            for (slot, sample) in samples[HISTORY..].iter_mut().enumerate() {
                *sample = [input[slot][qmf], input[QMF_SLOTS + slot][qmf]];
            }
            self.past[qmf].copy_from_slice(&samples[SLOTS..SLOTS + HISTORY]);

            if filters.is_empty() {
                output[hybrid_start * SLOTS..(hybrid_start + 1) * SLOTS].copy_from_slice(&samples[HISTORY..HISTORY + SLOTS]);
                hybrid_start += 1;
                continue;
            }
            let routing = if wide { None } else { Some(NARROW_ROUTES[qmf]) };
            let count = routing.map_or(filters.len(), |routes| routes.len());
            for slot in 0..SLOTS {
                let mut channel = [[0.0; 2]; 12];
                for (value, kernel) in channel.iter_mut().zip(filters) {
                    *value = convolve(&samples[slot..slot + WINDOW], kernel);
                }
                for band in 0..count {
                    let (a, b) = routing.map_or((band, None), |routes| routes[band]);
                    let mut value = channel[a];
                    if let Some(b) = b {
                        value[0] += channel[b][0];
                        value[1] += channel[b][1];
                    }
                    output[(hybrid_start + band) * SLOTS + slot] = value;
                }
            }
            hybrid_start += count;
        }
        for qmf in self.past.len()..QMF_BANDS {
            for slot in 0..SLOTS {
                output[hybrid_start * SLOTS + slot] = [input[slot][qmf], input[QMF_SLOTS + slot][qmf]];
            }
            hybrid_start += 1;
        }
    }

    pub(super) fn synthesize(source: &[Cx], output: &mut [[f32; QMF_BANDS]], wide: bool) {
        let mut start = 0;
        for qmf in 0..QMF_BANDS {
            let width = if wide {
                WIDE_COUNTS.get(qmf).copied().unwrap_or(1)
            } else {
                NARROW_ROUTES.get(qmf).map_or(1, |routes| routes.len())
            };
            for slot in 0..SLOTS {
                let mut value = source[start * SLOTS + slot];
                for band in start + 1..start + width {
                    let sample = source[band * SLOTS + slot];
                    value[0] += sample[0];
                    value[1] += sample[1];
                }
                output[slot][qmf] = value[0];
                output[QMF_SLOTS + slot][qmf] = value[1];
            }
            start += width;
        }
    }
}
