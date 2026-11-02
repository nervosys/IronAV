//! The CELT layer of Opus: band energies, bit allocation, pyramid vector quantisation, the inverse
//! MDCT with its low-overlap window, the pitch post-filter and de-emphasis (RFC 6716 section 4.3).
//!
//! Translated from the structure of FFmpeg's native Opus decoder (`libavcodec/opus/{celt,dec_celt,pvq}.c`).

use crate::mdct::Dct4;
use super::rc::{ilog, Coder, RangeDecoder};

pub(super) mod enc;
pub(super) mod prefilter;
use super::tables::*;
use ironav_core::{Error, Result};

pub const MAX_BANDS: usize = 21;
const SHORT_BLOCK: usize = 120;
const OVERLAP: usize = SHORT_BLOCK;
const MAX_FRAME: usize = SHORT_BLOCK * 8;
const VECTORS: i32 = 11;
const ALLOC_STEPS: i32 = 6;
const FINE_OFFSET: i32 = 21;
const MAX_FINE_BITS: i32 = 8;
const QTHETA_OFFSET: i32 = 4;
const QTHETA_OFFSET_TWOPHASE: i32 = 16;
const POSTFILTER_MINPERIOD: i32 = 15;
pub const ENERGY_SILENCE: f32 = -28.0;
const EMPH_COEFF: f32 = 0.850_006_1;

/// Scale of the inverse MDCT: the coefficients are in units of 1/32768 of full scale.
const IMDCT_SCALE: f32 = -1.0 / 32768.0;

const SPREAD_NONE: i32 = 0;
const SPREAD_AGGRESSIVE: i32 = 3;
const SPREAD_NORMAL: i32 = 2;

const PVQ_ROW: [usize; 15] = [0, 176, 351, 525, 698, 870, 1041, 1131, 1178, 1207, 1226, 1240, 1248, 1254, 1257];

fn pvq_u(n: u32, k: u32) -> u32 {
    CELT_PVQ_U[PVQ_ROW[n.min(k) as usize] + n.max(k) as usize]
}

fn pvq_v(n: u32, k: u32) -> u32 {
    pvq_u(n, k).wrapping_add(pvq_u(n, k + 1))
}

fn window(i: usize) -> f32 {
    CELT_WINDOW_PADDED[8 + i]
}

fn bad(msg: &str) -> Error {
    Error::invalid_input(format!("Opus: {msg}"))
}

struct Block {
    energy: [f32; MAX_BANDS],
    prev_energy: [[f32; MAX_BANDS]; 2],
    collapse_masks: [u8; MAX_BANDS],
    /// MDCT output and post-filter history.
    buf: Vec<f32>,
    coeffs: Vec<f32>,
    pf_period_new: i32,
    pf_gains_new: [f32; 3],
    pf_period: i32,
    pf_gains: [f32; 3],
    pf_period_old: i32,
    pf_gains_old: [f32; 3],
    emph_coeff: f32,
}

impl Block {
    fn new() -> Self {
        Self {
            energy: [0.0; MAX_BANDS],
            prev_energy: [[ENERGY_SILENCE; MAX_BANDS]; 2],
            collapse_masks: [0; MAX_BANDS],
            buf: vec![0.0; 2048],
            coeffs: vec![0.0; MAX_FRAME],
            pf_period_new: 0,
            pf_gains_new: [0.0; 3],
            pf_period: 0,
            pf_gains: [0.0; 3],
            pf_period_old: 0,
            pf_gains_old: [0.0; 3],
            emph_coeff: 0.0,
        }
    }
}

/// Everything about the frame being decoded that the band quantiser needs to see.
struct Params {
    /// log2 of the frame size over 120.
    size: i32,
    start_band: usize,
    end_band: usize,
    coded_bands: i32,
    coded_channels: i32,
    transient: bool,
    blocks: u32,
    blocksize: usize,
    anticollapse_needed: i32,
    intensity_stereo: i32,
    dual_stereo: bool,
    seed: u32,
    spread: i32,
    framebits: i32,
    remaining: i32,
    remaining2: i32,
    caps: [i32; MAX_BANDS],
    fine_bits: [i32; MAX_BANDS],
    fine_priority: [i32; MAX_BANDS],
    pulses: [i32; MAX_BANDS],
    tf_change: [i32; MAX_BANDS],
    alloc_trim: i32,
    apply_phase_inv: bool,
}

impl Params {
    fn rng(&mut self) -> u32 {
        self.seed = self.seed.wrapping_mul(1664525).wrapping_add(1013904223);
        self.seed
    }
}

pub struct Celt {
    output_channels: usize,
    apply_phase_inv: bool,
    block: [Block; 2],
    p: Params,
    dct: Vec<Dct4>,
    flushed: bool,
    /// Frames concealed in a row since the last one decoded.
    lost: u32,
    tmp: Vec<f32>,
    norm: Vec<f32>,
    enc_amp: [[f32; MAX_BANDS]; 2],
}

fn renormalize(x: &mut [f32], gain: f32) {
    let mut g = 1e-15f32;
    for v in x.iter() {
        g += v * v;
    }
    let g = gain / g.sqrt();
    for v in x.iter_mut() {
        *v *= g;
    }
}

fn haar1(x: &mut [f32], n0: usize, stride: usize) {
    let n0 = n0 >> 1;
    let s = std::f32::consts::FRAC_1_SQRT_2;
    for i in 0..stride {
        for j in 0..n0 {
            let x0 = x[stride * (2 * j) + i];
            let x1 = x[stride * (2 * j + 1) + i];
            x[stride * (2 * j) + i] = (x0 + x1) * s;
            x[stride * (2 * j + 1) + i] = (x0 - x1) * s;
        }
    }
}

fn interleave_hadamard(tmp: &mut [f32], x: &mut [f32], n0: usize, stride: usize, hadamard: bool) {
    let n = n0 * stride;
    let off = if hadamard { stride - 2 } else { 30 };
    for i in 0..stride {
        let o = CELT_HADAMARD_ORDER[off + i] as usize;
        for j in 0..n0 {
            tmp[j * stride + i] = x[o * n0 + j];
        }
    }
    x[..n].copy_from_slice(&tmp[..n]);
}

fn deinterleave_hadamard(tmp: &mut [f32], x: &mut [f32], n0: usize, stride: usize, hadamard: bool) {
    let n = n0 * stride;
    let off = if hadamard { stride - 2 } else { 30 };
    for i in 0..stride {
        let o = CELT_HADAMARD_ORDER[off + i] as usize;
        for j in 0..n0 {
            tmp[o * n0 + j] = x[j * stride + i];
        }
    }
    x[..n].copy_from_slice(&tmp[..n]);
}

fn exp_rotation_impl(x: &mut [f32], len: usize, stride: usize, c: f32, s: f32) {
    for i in 0..len.saturating_sub(stride) {
        let x1 = x[i];
        let x2 = x[i + stride];
        x[i + stride] = c * x2 + s * x1;
        x[i] = c * x1 - s * x2;
    }
    if len < 2 * stride + 1 {
        return;
    }
    for i in (0..=len - 2 * stride - 1).rev() {
        let x1 = x[i];
        let x2 = x[i + stride];
        x[i + stride] = c * x2 + s * x1;
        x[i] = c * x1 - s * x2;
    }
}

fn exp_rotation(x: &mut [f32], len: usize, stride: usize, k: u32, spread: i32) {
    if 2 * k as usize >= len || spread == SPREAD_NONE {
        return;
    }
    let gain = len as f32 / (len as f32 + (20 - 5 * spread) as f32 * k as f32);
    let theta = std::f32::consts::PI * gain * gain / 4.0;
    let (c, s) = (theta.cos(), theta.sin());
    let mut stride2 = 0;
    if len >= stride << 3 {
        stride2 = 1;
        // sqrt(len / stride), rounded
        while (stride2 * stride2 + stride2) * stride + (stride >> 2) < len {
            stride2 += 1;
        }
    }
    let len = len / stride;
    for i in 0..stride {
        let xs = &mut x[i * len..];
        if stride2 != 0 {
            exp_rotation_impl(xs, len, stride2, s, c);
        }
        exp_rotation_impl(xs, len, 1, c, s);
    }
}

fn collapse_mask(iy: &[i32], n: usize, b: usize) -> u32 {
    if b <= 1 {
        return 1;
    }
    let n0 = n / b;
    let mut mask = 0u32;
    for i in 0..b {
        for j in 0..n0 {
            mask |= ((iy[i * n0 + j] != 0) as u32) << i;
        }
    }
    mask
}

fn stereo_merge(x: &mut [f32], y: &mut [f32], mid: f32, n: usize) {
    let (mut xp, mut side) = (0f32, 0f32);
    for i in 0..n {
        xp += x[i] * y[i];
        side += y[i] * y[i];
    }
    xp *= mid;
    let mid2 = mid;
    let e0 = mid2 * mid2 + side - 2.0 * xp;
    let e1 = mid2 * mid2 + side + 2.0 * xp;
    if e0 < 6e-4 || e1 < 6e-4 {
        y[..n].copy_from_slice(&x[..n]);
        return;
    }
    let g0 = 1.0 / e0.sqrt();
    let g1 = 1.0 / e1.sqrt();
    for i in 0..n {
        let v0 = mid * x[i];
        let v1 = y[i];
        x[i] = g0 * (v0 - v1);
        y[i] = g1 * (v0 + v1);
    }
}

fn mul16_round(a: i32, b: i32) -> i32 {
    (a * b + 16384) >> 15
}

fn celt_cos(x: i16) -> i16 {
    let x = ((x as i32 * x as i32 + 4096) >> 13) as i16 as i32;
    let r = (32767 - x) + mul16_round(x, -7651 + mul16_round(x, 8277 + mul16_round(-626, x)));
    (r as i16).wrapping_add(1)
}

fn log2tan(isin: i32, icos: i32) -> i32 {
    let lc = ilog(icos as u32) as i32;
    let ls = ilog(isin as u32) as i32;
    let icos = icos << (15 - lc);
    let isin = isin << (15 - ls);
    (ls << 11) - (lc << 11) + mul16_round(isin, mul16_round(isin, -2597) + 7932) - mul16_round(icos, mul16_round(icos, -2597) + 7932)
}

fn bits2pulses(cache: &[u8], bits: i32) -> i32 {
    let mut low = 0i32;
    let mut high = cache[0] as i32;
    let bits = bits - 1;
    for _ in 0..6 {
        let center = (low + high + 1) >> 1;
        if cache[center as usize] as i32 >= bits {
            high = center;
        } else {
            low = center;
        }
    }
    let lowv = if low == 0 { -1 } else { cache[low as usize] as i32 };
    if bits - lowv <= cache[high as usize] as i32 - bits { low } else { high }
}

fn pulses2bits(cache: &[u8], pulses: i32) -> i32 {
    if pulses == 0 { 0 } else { cache[pulses as usize] as i32 + 1 }
}

fn compute_qn(n: i32, b: i32, offset: i32, pulse_cap: i32, stereo: bool) -> i32 {
    let mut n2 = 2 * n - 1;
    if stereo && n == 2 {
        n2 -= 1;
    }
    let qb = (b - pulse_cap - (4 << 3)).min((b + n2 * offset) / n2).min(8 << 3);
    if qb < (1 << 3 >> 1) {
        1
    } else {
        ((((CELT_QN_EXP2[(qb & 7) as usize] as i32) >> (14 - (qb >> 3))) + 1) >> 1) << 1
    }
}

/// Turn the index of a pulse vector into the vector, returning its squared norm.
fn cwrsi(mut n: u32, mut k: u32, mut i: u32, y: &mut [i32]) -> u64 {
    let mut norm = 0u64;
    let mut at = 0;
    let row = |r: u32, c: u32| CELT_PVQ_U[PVQ_ROW[r as usize] + c as usize];
    while n > 2 {
        if k >= n {
            // lots of pulses
            let p = row(n, k + 1);
            let s = -((i >= p) as i32);
            i -= p & s as u32;
            let k0 = k;
            let q = row(n, n);
            let mut p;
            if q > i {
                k = n;
                loop {
                    k -= 1;
                    p = row(k, n);
                    if p <= i {
                        break;
                    }
                }
            } else {
                p = row(n, k);
                while p > i {
                    k -= 1;
                    p = row(n, k);
                }
            }
            i -= p;
            let val = (k0 as i32 - k as i32 + s) ^ s;
            norm += (val as i64 * val as i64) as u64;
            y[at] = val;
            at += 1;
        } else {
            // lots of dimensions
            let p = row(k, n);
            let q = row(k + 1, n);
            if p <= i && i < q {
                i -= p;
                y[at] = 0;
                at += 1;
            } else {
                let s = -((i >= q) as i32);
                i -= q & s as u32;
                let k0 = k;
                let mut p;
                loop {
                    k -= 1;
                    p = row(k, n);
                    if p <= i {
                        break;
                    }
                }
                i -= p;
                let val = (k0 as i32 - k as i32 + s) ^ s;
                norm += (val as i64 * val as i64) as u64;
                y[at] = val;
                at += 1;
            }
        }
        n -= 1;
    }
    // n == 2
    let p = 2 * k + 1;
    let s = -((i >= p) as i32);
    i -= p & s as u32;
    let k0 = k;
    k = i.div_ceil(2);
    if k != 0 {
        i -= 2 * k - 1;
    }
    let val = (k0 as i32 - k as i32 + s) ^ s;
    norm += (val as i64 * val as i64) as u64;
    y[at] = val;
    at += 1;
    // n == 1
    let s = -(i as i32);
    let val = (k as i32 + s) ^ s;
    norm += (val as i64 * val as i64) as u64;
    y[at] = val;
    norm
}

/// State shared by the recursive band decoder.
struct Bands<'a, R: Coder> {
    rc: &'a mut R,
    p: &'a mut Params,
    hadamard_tmp: [f32; 256],
    iy: [i32; 256],
    /// Encoder only: the linear amplitude of the band in each channel before it was normalised.
    amp: [f32; 2],
}

impl<R: Coder> Bands<'_, R> {
    /// Decode the pulses of one band and combine them into the normalised shape; returns the collapse mask.
    fn unquant(&mut self, x: &mut [f32], n: usize, k: u32, blocks: u32, gain: f32) -> u32 {
        if R::ENC {
            // x holds the band's shape: rotate it the way the decoder will undo, find the closest
            // pulse vector and code its index
            enc::exp_rotation_fwd(x, n, blocks as usize, k, self.p.spread);
            enc::pvq_search(&x[..n], k as usize, &mut self.iy[..n]);
            let idx = enc::icwrs(n, &self.iy[..n]);
            self.rc.uint(pvq_v(n as u32, k), idx);
            return collapse_mask(&self.iy, n, blocks as usize);
        }
        let idx = self.rc.uint(pvq_v(n as u32, k), 0);
        let norm = cwrsi(n as u32, k, idx, &mut self.iy);
        let g = gain / (norm as f32).sqrt();
        for i in 0..n {
            x[i] = g * self.iy[i] as f32;
        }
        exp_rotation(x, n, blocks as usize, k, self.p.spread);
        collapse_mask(&self.iy, n, blocks as usize)
    }

    #[allow(clippy::too_many_arguments)]
    // Several locals are assigned in different branches further down; hoisting each declaration
    // to its first assignment would restructure the whole band loop.
    #[allow(clippy::needless_late_init)]
    fn quant_band(
        &mut self,
        band: usize,
        x: &mut [f32],
        y: Option<&mut [f32]>,
        n_in: usize,
        b_in: i32,
        blocks_in: u32,
        lowband: Option<&[f32]>,
        duration_in: i32,
        lowband_out: Option<&mut [f32]>,
        level: u32,
        gain: f32,
        fill_in: u32,
    ) -> u32 {
        let mut lowband_out = lowband_out;
        let stereo = y.is_some();
        let mut y = y;
        let mut n = n_in;
        let mut b = b_in;
        let mut blocks = blocks_in;
        let mut duration = duration_in;
        let mut fill = fill_in;
        let n0 = n_in;
        let mut n_b = n / blocks as usize;
        let mut n_b0 = n_b;
        let mut b0 = blocks;
        let mut time_divide = 0u32;
        let mut recombine = 0u32;
        let mut inv = false;
        let longblocks = b0 == 1;
        let mut cm = 0u32;

        if n == 1 {
            let do_one = |x0: &mut f32, this: &mut Self| {
                let mut sign = 0;
                if this.p.remaining2 >= 1 << 3 {
                    sign = this.rc.raw(1, (*x0 < 0.0) as u32);
                    this.p.remaining2 -= 1 << 3;
                }
                *x0 = 1.0 - 2.0 * sign as f32;
            };
            do_one(&mut x[0], self);
            if let Some(yy) = y.as_mut() {
                do_one(&mut yy[0], self);
            }
            if let Some(lo) = lowband_out {
                lo[0] = x[0];
            }
            return 1;
        }

        // The low band this band folds from, copied so it can be reshaped in place.
        let mut lb = [0f32; 256];
        let mut have_lb = false;
        if let Some(l) = lowband {
            let m = l.len().min(256);
            lb[..m].copy_from_slice(&l[..m]);
            have_lb = true;
        }

        if !stereo && level == 0 {
            if R::ENC && b0 > 1 {
                // the encoder's band arrives with the blocks interleaved; the quantiser wants them one after another
                deinterleave_hadamard(&mut self.hadamard_tmp, x, n_b, b0 as usize, longblocks);
            }
            let tf_change = self.p.tf_change[band];
            if tf_change > 0 {
                recombine = tf_change as u32;
            }
            for k in 0..recombine {
                if have_lb {
                    haar1(&mut lb, n >> k, 1 << k);
                }
                fill = (CELT_BIT_INTERLEAVE[(fill & 0xF) as usize] as u32) | (CELT_BIT_INTERLEAVE[(fill >> 4) as usize] as u32) << 2;
            }
            blocks >>= recombine;
            n_b <<= recombine;

            // Increasing the time resolution
            let mut tf = tf_change;
            while (n_b & 1) == 0 && tf < 0 {
                if have_lb {
                    haar1(&mut lb, n_b, blocks as usize);
                }
                fill |= fill << blocks;
                blocks <<= 1;
                n_b >>= 1;
                time_divide += 1;
                tf += 1;
            }
            b0 = blocks;
            n_b0 = n_b;

            // Reorganize the samples in time order instead of frequency order
            if b0 > 1 && have_lb {
                deinterleave_hadamard(&mut self.hadamard_tmp, &mut lb, n_b >> recombine, (b0 << recombine) as usize, longblocks);
            }
        }

        // If we need 1.5 more bit than we can produce, split the band in two.
        let cache_idx = CELT_CACHE_INDEX[((duration + 1) as usize) * MAX_BANDS + band] as isize;
        let cache: &[u8] = if cache_idx >= 0 { &CELT_CACHE_BITS[cache_idx as usize..] } else { &[0u8; 1] };
        let mut split = stereo;
        let mut mono_split = false;
        if !stereo && duration >= 0 && b > cache[cache[0] as usize] as i32 + 12 && n > 2 {
            n >>= 1;
            mono_split = true;
            split = true;
            duration -= 1;
            if blocks == 1 {
                fill = (fill & 1) | (fill << 1);
            }
            blocks = (blocks + 1) >> 1;
        }

        if split {
            let mut imid;
            let mut iside;
            let delta;
            let qalloc;
            let orig_fill;
            let itheta;
            let mid;
            let side;

            // Decide on the resolution to give to the split parameter theta
            let pulse_cap = CELT_LOG_FREQ_RANGE[band] as i32 + duration * 8;
            let offset = (pulse_cap >> 1) - if stereo && n == 2 { QTHETA_OFFSET_TWOPHASE } else { QTHETA_OFFSET };
            let qn = if stereo && band as i32 >= self.p.intensity_stereo { 1 } else { compute_qn(n as i32, b, offset, pulse_cap, stereo) };
            // encoder: the angle between the two halves (or channels), before it is quantised
            let mut theta_raw = 0i32;
            let mut want = 0u32;
            if R::ENC {
                let (a, c): (&[f32], &[f32]) = if mono_split { (&x[..n], &x[n..2 * n]) } else { (&x[..n], &y.as_deref().expect("stereo has a second channel")[..n]) };
                theta_raw = enc::stereo_itheta(a, c, stereo);
                want = ((theta_raw as i64 * qn as i64 + 8192) >> 14) as u32;
            }
            let tell = self.rc.tell_frac();
            let mut it;
            if qn != 1 {
                // Entropy coding of the angle: a step for stereo, uniform for a time split, triangular otherwise.
                it = if stereo && n > 2 {
                    self.rc.uint_step(qn as u32 / 2, want) as i32
                } else if stereo || b0 > 1 {
                    self.rc.uint(qn as u32 + 1, want) as i32
                } else {
                    self.rc.uint_tri(qn as u32, want) as i32
                };
                it = it * 16384 / qn;
            } else if stereo {
                let coded = b > 2 << 3 && self.p.remaining2 > 2 << 3;
                // the right channel's phase is flipped when the channels are mostly out of phase
                let flip = R::ENC && self.p.apply_phase_inv && coded && theta_raw > 8192;
                inv = if coded { self.rc.log(2, flip as u32) != 0 } else { false };
                inv = if self.p.apply_phase_inv { inv } else { false };
                it = 0;
            } else {
                it = 0;
            }
            itheta = it;
            qalloc = self.rc.tell_frac() - tell;
            b -= qalloc;

            // encoder: turn left/right into mid/side, or into one channel when the angle is zero
            if R::ENC && stereo {
                let yy = y.as_deref_mut().expect("stereo has a second channel");
                if qn != 1 && itheta != 0 {
                    enc::stereo_split(&mut x[..n], &mut yy[..n]);
                } else {
                    if inv {
                        for v in yy[..n].iter_mut() {
                            *v = -*v;
                        }
                    }
                    enc::intensity_stereo(&mut x[..n], &yy[..n], self.amp);
                }
            }

            orig_fill = fill;
            if itheta == 0 {
                imid = 32767;
                iside = 0;
                fill &= (1u32 << blocks) - 1;
                delta = -16384;
            } else if itheta == 16384 {
                imid = 0;
                iside = 32767;
                fill &= ((1u32 << blocks) - 1) << blocks;
                delta = 16384;
            } else {
                imid = celt_cos(itheta as i16) as i32;
                iside = celt_cos((16384 - itheta) as i16) as i32;
                // This is the mid vs side allocation that minimizes squared error in that band.
                delta = mul16_round(((n as i32) - 1) << 7, log2tan(iside, imid));
            }
            let _ = (&mut imid, &mut iside);
            mid = imid as f32 / 32768.0;
            side = iside as f32 / 32768.0;

            // the two halves: separate channels, or the two halves of one band
            let (xa, xb): (&mut [f32], &mut [f32]) = if mono_split {
                x.split_at_mut(n)
            } else {
                (&mut x[..], y.as_mut().map(|v| &mut v[..]).expect("stereo has a second channel"))
            };

            if n == 2 && stereo {
                // A special case for N=2 that only works for stereo: mid and side are orthogonal, so
                // the side takes one bit.
                let mut mbits = b;
                let sbits = if itheta != 0 && itheta != 16384 { 1 << 3 } else { 0 };
                mbits -= sbits;
                let c = itheta > 8192;
                self.p.remaining2 -= qalloc + sbits;
                let (x2, y2) = if c { (&mut *xb, &mut *xa) } else { (&mut *xa, &mut *xb) };
                let mut sign = 0;
                if sbits != 0 {
                    sign = self.rc.raw(1, (R::ENC && x2[0] * y2[1] - x2[1] * y2[0] < 0.0) as u32);
                }
                let sign = 1.0 - 2.0 * sign as f32;
                // orig_fill is used so that the side can be folded even when itheta == 16384
                cm = self.quant_band(band, x2, None, n, mbits, blocks, have_lb.then_some(&lb[..]), duration, lowband_out.take(), level, gain, orig_fill);
                // N=2 bands are not split, so cm is 0 or 1 (a fold collapse) and needs no mixing with the other channel
                y2[0] = -sign * x2[1];
                y2[1] = sign * x2[0];
                xa[0] *= mid;
                xa[1] *= mid;
                xb[0] *= side;
                xb[1] *= side;
                let t = xa[0];
                xa[0] = t - xb[0];
                xb[0] += t;
                let t = xa[1];
                xa[1] = t - xb[1];
                xb[1] += t;
            } else {
                // "Normal" split code
                let mut delta = delta;
                let mut next_level = 0;
                // Give more bits to low-energy MDCTs than they would otherwise deserve
                if b0 > 1 && !stereo && (itheta & 0x3fff) != 0 {
                    if itheta > 8192 {
                        // Rough approximation for pre-echo masking
                        delta -= delta >> (4 - duration);
                    } else {
                        // Corresponds to a forward-masking slope of 1.5 dB per 10 ms
                        delta = (delta + ((n as i32) << 3 >> (5 - duration))).min(0);
                    }
                }
                let mut mbits = ((b - delta) / 2).clamp(0, b);
                let mut sbits = b - mbits;
                self.p.remaining2 -= qalloc;

                let next_lowband2: Option<&[f32]> = if have_lb && !stereo { Some(&lb[n..]) } else { None };
                // Only stereo needs to pass on lowband_out; otherwise it is handled at the end
                let out1 = if stereo { lowband_out.take() } else { None };
                if !stereo {
                    next_level = level + 1;
                }
                let lowband1: Option<&[f32]> = if have_lb { Some(&lb[..]) } else { None };

                let mut rebalance = self.p.remaining2;
                if mbits >= sbits {
                    // In stereo mode no scaling is applied to the mid: the normalised mid is needed for folding
                    cm = self.quant_band(band, xa, None, n, mbits, blocks, lowband1, duration, out1, next_level, if stereo { 1.0 } else { gain * mid }, fill);
                    rebalance = mbits - (rebalance - self.p.remaining2);
                    if rebalance > 3 << 3 && itheta != 0 {
                        sbits += rebalance - (3 << 3);
                    }
                    // For a stereo split the high bits of fill are always zero, so the side is never folded
                    let cmt = self.quant_band(band, xb, None, n, sbits, blocks, next_lowband2, duration, None, next_level, gain * side, fill >> blocks);
                    cm |= cmt << ((b0 >> 1) & (stereo as u32).wrapping_sub(1));
                } else {
                    cm = self.quant_band(band, xb, None, n, sbits, blocks, next_lowband2, duration, None, next_level, gain * side, fill >> blocks);
                    cm <<= (b0 >> 1) & (stereo as u32).wrapping_sub(1);
                    rebalance = sbits - (rebalance - self.p.remaining2);
                    if rebalance > 3 << 3 && itheta != 16384 {
                        mbits += rebalance - (3 << 3);
                    }
                    cm |= self.quant_band(band, xa, None, n, mbits, blocks, lowband1, duration, out1, next_level, if stereo { 1.0 } else { gain * mid }, fill);
                }
            }
            if stereo {
                // stereo merge below uses mid
                if n > 2 {
                    // fallthrough to the shared stereo merge after this block, which needs mid
                }
            }
            // Stereo merge and inversion
            if stereo && !R::ENC {
                let yv = y.as_mut().unwrap();
                if n > 2 {
                    stereo_merge(x, yv, mid, n);
                }
                if inv {
                    for v in yv[..n].iter_mut() {
                        *v = -*v;
                    }
                }
            }
        } else {
            // The basic no-split case
            let mut q = bits2pulses(cache, b);
            let mut curr_bits = pulses2bits(cache, q);
            self.p.remaining2 -= curr_bits;
            // Ensures we can never bust the budget
            while self.p.remaining2 < 0 && q > 0 {
                self.p.remaining2 += curr_bits;
                q -= 1;
                curr_bits = pulses2bits(cache, q);
                self.p.remaining2 -= curr_bits;
            }
            if q != 0 {
                let k = if q < 8 { q as u32 } else { (8 + (q as u32 & 7)) << ((q >> 3) - 1) };
                cm = self.unquant(x, n, k, blocks, gain);
            } else {
                // If there's no pulse, fill the band anyway
                let cm_mask = (1u32 << blocks) - 1;
                fill &= cm_mask;
                if fill != 0 {
                    if !have_lb {
                        // Noise
                        for i in 0..n {
                            x[i] = ((self.p.rng() as i32) >> 20) as f32;
                        }
                        cm = cm_mask;
                    } else {
                        // Folded spectrum
                        for i in 0..n {
                            // About 48 dB below the "normal" folding level
                            x[i] = lb[i] + if self.p.rng() & 0x8000 != 0 { 1.0 / 256.0 } else { -1.0 / 256.0 };
                        }
                        cm = fill;
                    }
                    renormalize(&mut x[..n], gain);
                } else {
                    x[..n].fill(0.0);
                }
            }
        }

        // What is left to do after any split: only mono bands at the top level.
        if !stereo && level == 0 {
            // Undo the sample reorganization going from time order to frequency order
            if b0 > 1 {
                interleave_hadamard(&mut self.hadamard_tmp, x, n_b >> recombine, (b0 << recombine) as usize, longblocks);
            }
            // Undo time-freq changes that we did earlier
            n_b = n_b0;
            blocks = b0;
            for _ in 0..time_divide {
                blocks >>= 1;
                n_b <<= 1;
                cm |= cm >> blocks;
                haar1(x, n_b, blocks as usize);
            }
            for k in 0..recombine {
                cm = CELT_BIT_DEINTERLEAVE[cm as usize & 0xFF] as u32;
                haar1(x, n0 >> k, 1 << k);
            }
            blocks <<= recombine;
            // Scale output for later folding
            if let Some(lo) = lowband_out {
                let nn = (n0 as f32).sqrt();
                for i in 0..n0 {
                    lo[i] = nn * x[i];
                }
            }
            cm &= (1u32 << blocks) - 1;
        }
        let _ = (SPREAD_AGGRESSIVE, SPREAD_NORMAL);
        cm
    }
}

impl Celt {
    pub fn new(output_channels: usize, apply_phase_inv: bool) -> Self {
        let mut c = Self {
            output_channels,
            apply_phase_inv,
            block: [Block::new(), Block::new()],
            p: Params {
                size: 0,
                start_band: 0,
                end_band: 0,
                coded_bands: 0,
                coded_channels: 1,
                transient: false,
                blocks: 1,
                blocksize: 0,
                anticollapse_needed: 0,
                intensity_stereo: 0,
                dual_stereo: false,
                seed: 0,
                spread: 0,
                framebits: 0,
                remaining: 0,
                remaining2: 0,
                caps: [0; MAX_BANDS],
                fine_bits: [0; MAX_BANDS],
                fine_priority: [0; MAX_BANDS],
                pulses: [0; MAX_BANDS],
                tf_change: [0; MAX_BANDS],
                alloc_trim: 5,
                apply_phase_inv,
            },
            dct: [120, 240, 480, 960].iter().map(|&n| Dct4::new(n)).collect(),
            flushed: false,
            lost: 0,
            tmp: vec![0.0; MAX_FRAME],
            norm: vec![0.0; 2 * 8 * 100],
            enc_amp: [[0.0; MAX_BANDS]; 2],
        };
        c.flush();
        c
    }

    pub fn flush(&mut self) {
        if self.flushed {
            return;
        }
        for b in self.block.iter_mut() {
            b.prev_energy = [[ENERGY_SILENCE; MAX_BANDS]; 2];
            b.energy = [0.0; MAX_BANDS];
            b.buf.fill(0.0);
            b.pf_gains = [0.0; 3];
            b.pf_gains_old = [0.0; 3];
            b.pf_gains_new = [0.0; 3];
            b.emph_coeff = 0.0;
        }
        self.p.seed = 0;
        self.flushed = true;
    }

    fn decode_coarse_energy(&mut self, rc: &mut RangeDecoder) {
        let p = &self.p;
        let mut prev = [0f32; 2];
        let mut alpha = CELT_ALPHA_COEF[p.size as usize];
        let mut beta = CELT_BETA_COEF[p.size as usize];
        let mut model = &CELT_COARSE_ENERGY_DIST[p.size as usize][0];
        // intra frame
        if rc.tell() + 3 <= p.framebits && rc.log(3) != 0 {
            alpha = 0.0;
            beta = 1.0 - 4915.0 / 32768.0;
            model = &CELT_COARSE_ENERGY_DIST[p.size as usize][1];
        }
        for i in 0..MAX_BANDS {
            for j in 0..p.coded_channels as usize {
                let block = &mut self.block[j];
                if i < p.start_band || i >= p.end_band {
                    block.energy[i] = 0.0;
                    continue;
                }
                let available = p.framebits - rc.tell();
                let value: f32 = if available >= 15 {
                    // Laplace distribution
                    let k = i.min(20) << 1;
                    rc.laplace((model[k] as u32) << 7, (model[k + 1] as u32) << 6) as f32
                } else if available >= 2 {
                    let x = rc.cdf(&CELT_MODEL_TAPSET) as i32;
                    ((x >> 1) ^ -(x & 1)) as f32
                } else if available >= 1 {
                    -(rc.log(1) as f32)
                } else {
                    -1.0
                };
                block.energy[i] = block.energy[i].max(-9.0) * alpha + prev[j] + value;
                prev[j] += beta * value;
            }
        }
    }

    fn decode_fine_energy(&mut self, rc: &mut RangeDecoder) {
        let p = &self.p;
        for i in p.start_band..p.end_band {
            if p.fine_bits[i] == 0 {
                continue;
            }
            for j in 0..p.coded_channels as usize {
                let q2 = rc.raw(p.fine_bits[i] as u32);
                let offset = (q2 as f32 + 0.5) * (1 << (14 - p.fine_bits[i])) as f32 / 16384.0 - 0.5;
                self.block[j].energy[i] += offset;
            }
        }
    }

    fn decode_final_energy(&mut self, rc: &mut RangeDecoder) {
        let p = &self.p;
        let mut bits_left = p.framebits - rc.tell();
        for priority in 0..2 {
            let mut i = p.start_band;
            while i < p.end_band && bits_left >= p.coded_channels {
                if p.fine_priority[i] != priority || p.fine_bits[i] >= MAX_FINE_BITS {
                    i += 1;
                    continue;
                }
                for j in 0..p.coded_channels as usize {
                    let q2 = rc.raw(1);
                    let offset = (q2 as f32 - 0.5) * (1 << (14 - p.fine_bits[i] - 1)) as f32 / 16384.0;
                    self.block[j].energy[i] += offset;
                    bits_left -= 1;
                }
                i += 1;
            }
        }
    }

    fn decode_tf_changes<R: Coder>(&mut self, rc: &mut R) {
        let p = &mut self.p;
        let (mut diff, mut tf_select, mut tf_changed) = (0usize, 0usize, 0usize);
        let mut bits = if p.transient { 2 } else { 4 };
        let mut consumed = rc.tell();
        let tf_select_bit = p.size != 0 && consumed + bits < p.framebits;
        let t = p.transient as usize;
        let size = p.size as usize;
        for i in p.start_band..p.end_band {
            if consumed + bits + tf_select_bit as i32 <= p.framebits {
                // an encoder of short blocks asks for tf_change 1 (which the table maps to no change) in every band
                diff ^= rc.log(bits as u32, (R::ENC && p.transient && i == p.start_band) as u32) as usize;
                consumed = rc.tell();
                tf_changed |= diff;
            }
            p.tf_change[i] = diff as i32;
            bits = if p.transient { 4 } else { 5 };
        }
        if tf_select_bit && CELT_TF_SELECT[size][t][0][tf_changed] != CELT_TF_SELECT[size][t][1][tf_changed] {
            tf_select = rc.log(1, 0) as usize;
        }
        for i in p.start_band..p.end_band {
            p.tf_change[i] = CELT_TF_SELECT[size][t][tf_select][p.tf_change[i] as usize] as i32;
        }
    }

    fn bitalloc<R: Coder>(&mut self, rc: &mut R) {
        let f = &mut self.p;
        let ch = f.coded_channels;
        let size = f.size;
        let norm_c = |bits: i32| (bits << (ch - 1) << size) >> 2;
        let mut skip_startband = f.start_band as i32;
        let mut skip_bit = 0;
        let mut intensitystereo_bit = 0;
        let mut dualstereo_bit = 0;
        let mut dynalloc = 6;
        let mut extrabits = 0;
        let start = f.start_band as i32;
        let end = f.end_band as i32;

        let mut boost = [0i32; MAX_BANDS];
        let mut trim_offset = [0i32; MAX_BANDS];
        let mut threshold = [0i32; MAX_BANDS];
        let mut bits1 = [0i32; MAX_BANDS];
        let mut bits2 = [0i32; MAX_BANDS];

        // Spread
        if rc.tell() + 4 <= f.framebits {
            f.spread = rc.cdf(&CELT_MODEL_SPREAD, SPREAD_NORMAL as usize) as i32;
        } else {
            f.spread = SPREAD_NORMAL;
        }

        // Initialize static allocation caps
        for i in 0..MAX_BANDS {
            f.caps[i] = norm_c((CELT_STATIC_CAPS[size as usize][(ch - 1) as usize][i] as i32 + 64) * CELT_FREQ_RANGE[i] as i32);
        }

        // Band boosts
        let mut tbits_8ths = f.framebits << 3;
        for i in f.start_band..f.end_band {
            let mut quanta = (CELT_FREQ_RANGE[i] as i32) << (ch - 1) << size;
            let mut b_dynalloc = dynalloc;
            quanta = (quanta << 3).min((6 << 3).max(quanta));
            while rc.tell_frac() + (b_dynalloc << 3) < tbits_8ths && boost[i] < f.caps[i] {
                let is_boost = rc.log(b_dynalloc as u32, 0);
                if is_boost == 0 {
                    break;
                }
                boost[i] += quanta;
                tbits_8ths -= quanta;
                b_dynalloc = 1;
            }
            if boost[i] != 0 {
                dynalloc = (dynalloc - 1).max(2);
            }
        }

        // Allocation trim (an encoder sets the one it wants beforehand)
        let want = f.alloc_trim.clamp(0, 10) as usize;
        f.alloc_trim = 5;
        if rc.tell_frac() + (6 << 3) <= tbits_8ths {
            f.alloc_trim = rc.cdf(&CELT_MODEL_ALLOC_TRIM, want) as i32;
        }

        // Anti-collapse bit reservation
        tbits_8ths = (f.framebits << 3) - rc.tell_frac() - 1;
        f.anticollapse_needed = 0;
        if f.transient && size >= 2 && tbits_8ths >= ((size + 2) << 3) {
            f.anticollapse_needed = 1 << 3;
        }
        tbits_8ths -= f.anticollapse_needed;

        // Band skip bit reservation
        if tbits_8ths >= 1 << 3 {
            skip_bit = 1 << 3;
        }
        tbits_8ths -= skip_bit;

        // Intensity/dual stereo bit reservation
        if ch == 2 {
            intensitystereo_bit = CELT_LOG2_FRAC[(end - start) as usize] as i32;
            if intensitystereo_bit <= tbits_8ths {
                tbits_8ths -= intensitystereo_bit;
                if tbits_8ths >= 1 << 3 {
                    dualstereo_bit = 1 << 3;
                    tbits_8ths -= 1 << 3;
                }
            } else {
                intensitystereo_bit = 0;
            }
        }

        // Trim offsets
        for i in f.start_band..f.end_band {
            let trim = f.alloc_trim - 5 - size;
            let band = CELT_FREQ_RANGE[i] as i32 * (end - i as i32 - 1);
            let duration = size + 3;
            let scale = duration + ch - 1;
            // PVQ minimum allocation threshold; below this value the band is skipped
            threshold[i] = ((3 * CELT_FREQ_RANGE[i] as i32) << duration >> 4).max(ch << 3);
            trim_offset[i] = (trim * (band << scale)) >> 6;
            if (CELT_FREQ_RANGE[i] as i32) << size == 1 {
                trim_offset[i] -= ch << 3;
            }
        }

        // Bisection over the static allocation tables
        let mut low = 1;
        let mut high = VECTORS - 1;
        while low <= high {
            let center = (low + high) >> 1;
            let mut done = false;
            let mut total = 0;
            let mut i = end - 1;
            while i >= start {
                let iu = i as usize;
                let mut bandbits = norm_c(CELT_FREQ_RANGE[iu] as i32 * CELT_STATIC_ALLOC[center as usize][iu] as i32);
                if bandbits != 0 {
                    bandbits = (bandbits + trim_offset[iu]).max(0);
                }
                bandbits += boost[iu];
                if bandbits >= threshold[iu] || done {
                    done = true;
                    total += bandbits.min(f.caps[iu]);
                } else if bandbits >= ch << 3 {
                    total += ch << 3;
                }
                i -= 1;
            }
            if total > tbits_8ths {
                high = center - 1;
            } else {
                low = center + 1;
            }
        }
        high = low;
        low -= 1;

        for i in f.start_band..f.end_band {
            bits1[i] = norm_c(CELT_FREQ_RANGE[i] as i32 * CELT_STATIC_ALLOC[low as usize][i] as i32);
            bits2[i] = if high >= VECTORS { f.caps[i] } else { norm_c(CELT_FREQ_RANGE[i] as i32 * CELT_STATIC_ALLOC[high as usize][i] as i32) };
            if bits1[i] != 0 {
                bits1[i] = (bits1[i] + trim_offset[i]).max(0);
            }
            if bits2[i] != 0 {
                bits2[i] = (bits2[i] + trim_offset[i]).max(0);
            }
            if low != 0 {
                bits1[i] += boost[i];
            }
            bits2[i] += boost[i];
            if boost[i] != 0 {
                skip_startband = i as i32;
            }
            bits2[i] = (bits2[i] - bits1[i]).max(0);
        }

        // Interpolation between the two
        let mut low = 0;
        let mut high = 1 << ALLOC_STEPS;
        for _ in 0..ALLOC_STEPS {
            let center = (low + high) >> 1;
            let mut done = false;
            let mut total = 0;
            let mut j = end - 1;
            while j >= start {
                let ju = j as usize;
                let bandbits = bits1[ju] + ((center * bits2[ju]) >> ALLOC_STEPS);
                if bandbits >= threshold[ju] || done {
                    done = true;
                    total += bandbits.min(f.caps[ju]);
                } else if bandbits >= ch << 3 {
                    total += ch << 3;
                }
                j -= 1;
            }
            if total > tbits_8ths {
                high = center;
            } else {
                low = center;
            }
        }

        let mut done = false;
        let mut total = 0;
        let mut i = end - 1;
        while i >= start {
            let iu = i as usize;
            let mut bandbits = bits1[iu] + ((low * bits2[iu]) >> ALLOC_STEPS);
            if bandbits >= threshold[iu] || done {
                done = true;
            } else {
                bandbits = if bandbits >= ch << 3 { ch << 3 } else { 0 };
            }
            bandbits = bandbits.min(f.caps[iu]);
            f.pulses[iu] = bandbits;
            total += bandbits;
            i -= 1;
        }

        // Band skipping
        f.coded_bands = end;
        loop {
            let j = f.coded_bands - 1;
            if j == skip_startband {
                // all remaining bands are not skipped
                tbits_8ths += skip_bit;
                break;
            }
            // determine the number of bits available for coding "do not skip" markers
            let ju = j as usize;
            let mut remaining = tbits_8ths - total;
            let span = CELT_FREQ_BANDS[ju + 1] as i32 - CELT_FREQ_BANDS[start as usize] as i32;
            let bandbits = remaining / span;
            remaining -= bandbits * span;
            let mut allocation = f.pulses[ju] + bandbits * CELT_FREQ_RANGE[ju] as i32;
            allocation += (remaining - (CELT_FREQ_BANDS[ju] as i32 - CELT_FREQ_BANDS[start as usize] as i32)).max(0);

            // a "do not skip" marker is only coded if the allocation is above the chosen threshold
            if allocation >= threshold[ju].max((ch + 1) << 3) {
                if rc.log(1, 1) != 0 {
                    break;
                }
                total += 1 << 3;
                allocation -= 1 << 3;
            }

            // the band is skipped, so reclaim its bits
            total -= f.pulses[ju];
            if intensitystereo_bit != 0 {
                total -= intensitystereo_bit;
                intensitystereo_bit = CELT_LOG2_FRAC[(j - start) as usize] as i32;
                total += intensitystereo_bit;
            }
            f.pulses[ju] = if allocation >= ch << 3 { ch << 3 } else { 0 };
            total += f.pulses[ju];
            f.coded_bands -= 1;
        }

        // IS start band
        f.intensity_stereo = 0;
        f.dual_stereo = false;
        if intensitystereo_bit != 0 {
            f.intensity_stereo = start + rc.uint((f.coded_bands + 1 - start) as u32, (f.coded_bands - start) as u32) as i32;
        }

        // DS flag
        if f.intensity_stereo <= start {
            tbits_8ths += dualstereo_bit; // no intensity stereo means no dual stereo
        } else if dualstereo_bit != 0 {
            f.dual_stereo = rc.log(1, 0) != 0;
        }

        // Supply the remaining bits in this frame to lower bands
        let mut remaining = tbits_8ths - total;
        let span = CELT_FREQ_BANDS[f.coded_bands as usize] as i32 - CELT_FREQ_BANDS[start as usize] as i32;
        let bandbits = remaining / span;
        remaining -= bandbits * span;
        for i in f.start_band..f.coded_bands as usize {
            let bits = remaining.min(CELT_FREQ_RANGE[i] as i32);
            f.pulses[i] += bits + bandbits * CELT_FREQ_RANGE[i] as i32;
            remaining -= bits;
        }

        // Finally determine the allocation
        let mut i = f.start_band;
        while (i as i32) < f.coded_bands {
            let n = (CELT_FREQ_RANGE[i] as i32) << size;
            let prev_extra = extrabits;
            f.pulses[i] += extrabits;
            if n > 1 {
                extrabits = (f.pulses[i] - f.caps[i]).max(0);
                f.pulses[i] -= extrabits;

                // intensity stereo makes use of an extra degree of freedom
                let dof = n * ch + (ch == 2 && n > 2 && !f.dual_stereo && (i as i32) < f.intensity_stereo) as i32;
                let temp = dof * (CELT_LOG_FREQ_RANGE[i] as i32 + (size << 3));
                let mut offset = (temp >> 1) - dof * FINE_OFFSET;
                if n == 2 {
                    // dof=2 is the only case that doesn't fit the model
                    offset += dof << 1;
                }
                // grant an additional bias for the first and second pulses
                if f.pulses[i] + offset < 2 * (dof << 3) {
                    offset += temp >> 2;
                } else if f.pulses[i] + offset < 3 * (dof << 3) {
                    offset += temp >> 3;
                }
                let fine_bits = (f.pulses[i] + offset + (dof << 2)) / (dof << 3);
                let max_bits = ((f.pulses[i] >> 3) >> (ch - 1)).clamp(0, MAX_FINE_BITS);
                f.fine_bits[i] = fine_bits.clamp(0, max_bits);

                // If fine_bits was rounded down or capped, give priority for the final fine energy pass
                f.fine_priority[i] = (f.fine_bits[i] * (dof << 3) >= f.pulses[i] + offset) as i32;

                // the remaining bits are assigned to PVQ
                f.pulses[i] -= f.fine_bits[i] << (ch - 1) << 3;
            } else {
                // all bits go to fine energy except for the sign bit
                extrabits = (f.pulses[i] - (ch << 3)).max(0);
                f.pulses[i] -= extrabits;
                f.fine_bits[i] = 0;
                f.fine_priority[i] = 1;
            }

            // hand back a limited number of extra fine energy bits to this band
            if extrabits > 0 {
                let mut fineextra = (extrabits >> (ch + 2)).min(MAX_FINE_BITS - f.fine_bits[i]);
                f.fine_bits[i] += fineextra;
                fineextra <<= ch + 2;
                f.fine_priority[i] = (fineextra >= extrabits - prev_extra) as i32;
                extrabits -= fineextra;
            }
            i += 1;
        }
        f.remaining = extrabits;

        // skipped bands dedicate all of their bits for fine energy
        while i < f.end_band {
            f.fine_bits[i] = f.pulses[i] >> (ch - 1) >> 3;
            f.pulses[i] = 0;
            f.fine_priority[i] = (f.fine_bits[i] < 1) as i32;
            i += 1;
        }
    }

    fn quant_bands<R: Coder>(&mut self, rc: &mut R) {
        let ch = self.p.coded_channels as usize;
        let size = self.p.size;
        let mut lowband_scratch_free = ();
        let _ = &mut lowband_scratch_free;
        let totalbits = (self.p.framebits << 3) - self.p.anticollapse_needed;
        let mut update_lowband = true;
        let mut lowband_offset = 0usize;
        let start = self.p.start_band;
        let end = self.p.end_band;
        let blocks = self.p.blocks;
        self.norm.fill(0.0);
        let [b0, b1] = &mut self.block;
        let mut bands = Bands { rc, p: &mut self.p, hadamard_tmp: [0.0; 256], iy: [0; 256], amp: [0.0; 2] };

        for i in start..end {
            let mut cm = [(1u32 << blocks) - 1; 2];
            let band_offset = (CELT_FREQ_BANDS[i] as usize) << size;
            let band_size = (CELT_FREQ_RANGE[i] as usize) << size;
            let consumed = bands.rc.tell_frac();
            let mut effective_lowband: i32 = -1;
            let mut b = 0;

            // Compute how many bits we want to allocate to this band
            if i != start {
                bands.p.remaining -= consumed;
            }
            bands.p.remaining2 = totalbits - consumed - 1;
            if (i as i32) < bands.p.coded_bands {
                let curr_balance = bands.p.remaining / 3.min(bands.p.coded_bands - i as i32);
                b = (bands.p.remaining2 + 1).min(bands.p.pulses[i] + curr_balance).clamp(0, (1 << 14) - 1);
            }

            if (CELT_FREQ_BANDS[i] as i32 - CELT_FREQ_RANGE[i] as i32 >= CELT_FREQ_BANDS[start] as i32 || i == start + 1) && (update_lowband || lowband_offset == 0) {
                lowband_offset = i;
            }

            if i == start + 1 {
                // Special Hybrid Folding (RFC 8251 section 9). Copy the first band into the second to
                // ensure the second band never has to use the LCG.
                let count = ((CELT_FREQ_RANGE[i] - CELT_FREQ_RANGE[i - 1]) as usize) << size;
                self.norm.copy_within(band_offset - count..band_offset, band_offset);
                if ch == 2 {
                    self.norm.copy_within(800 + band_offset - count..800 + band_offset, 800 + band_offset);
                }
            }

            // Get a conservative estimate of the collapse_mask's for the bands we're going to be folding from.
            if lowband_offset != 0 && (bands.p.spread != SPREAD_AGGRESSIVE || blocks > 1 || bands.p.tf_change[i] < 0) {
                // This ensures we never repeat spectral content within one band
                effective_lowband = (CELT_FREQ_BANDS[start] as i32).max(CELT_FREQ_BANDS[lowband_offset] as i32 - CELT_FREQ_RANGE[i] as i32);
                let mut foldstart = lowband_offset;
                loop {
                    foldstart -= 1;
                    if CELT_FREQ_BANDS[foldstart] as i32 <= effective_lowband {
                        break;
                    }
                }
                let mut foldend = lowband_offset - 1;
                loop {
                    foldend += 1;
                    if !(foldend < i && (CELT_FREQ_BANDS[foldend] as i32) < effective_lowband + CELT_FREQ_RANGE[i] as i32) {
                        break;
                    }
                }
                cm = [0, 0];
                for j in foldstart..foldend {
                    cm[0] |= b0.collapse_masks[j] as u32;
                    cm[1] |= if ch == 2 { b1.collapse_masks[j] } else { b0.collapse_masks[j] } as u32;
                }
            }

            if bands.p.dual_stereo && i as i32 == bands.p.intensity_stereo {
                // Switch off dual stereo to do intensity
                bands.p.dual_stereo = false;
                for j in ((CELT_FREQ_BANDS[start] as usize) << size)..band_offset {
                    self.norm[j] = (self.norm[j] + self.norm[800 + j]) / 2.0;
                }
            }

            let low_at = if effective_lowband != -1 { Some((effective_lowband as usize) << size) } else { None };
            let mut low1 = [0f32; 256];
            let mut low2 = [0f32; 256];
            if let Some(l) = low_at {
                let n = band_size.min(256);
                low1[..n].copy_from_slice(&self.norm[l..l + n]);
                low2[..n].copy_from_slice(&self.norm[800 + l..800 + l + n]);
            }
            // an encoder never resynthesises, so it has nothing to fold from
            let l1 = if R::ENC { None } else { low_at.map(|_| &low1[..band_size]) };
            let l2 = if R::ENC { None } else { low_at.map(|_| &low2[..band_size]) };
            bands.amp = [self.enc_amp[0][i], self.enc_amp[1][i]];

            let (n1, n2) = self.norm.split_at_mut(800);
            let out1 = &mut n1[band_offset..band_offset + band_size];
            let out2 = &mut n2[band_offset..band_offset + band_size];
            let x = &mut b0.coeffs[band_offset..band_offset + band_size];

            if bands.p.dual_stereo {
                let y = &mut b1.coeffs[band_offset..band_offset + band_size];
                cm[0] = bands.quant_band(i, x, None, band_size, b >> 1, blocks, l1, size, Some(out1), 0, 1.0, cm[0]);
                cm[1] = bands.quant_band(i, y, None, band_size, b >> 1, blocks, l2, size, Some(out2), 0, 1.0, cm[1]);
            } else {
                let y = if ch == 2 { Some(&mut b1.coeffs[band_offset..band_offset + band_size]) } else { None };
                cm[0] = bands.quant_band(i, x, y, band_size, b, blocks, l1, size, Some(out1), 0, 1.0, cm[0] | cm[1]);
                cm[1] = cm[0];
            }

            b0.collapse_masks[i] = cm[0] as u8;
            if ch == 2 {
                b1.collapse_masks[i] = cm[1] as u8;
            } else {
                b0.collapse_masks[i] = cm[1] as u8;
            }
            bands.p.remaining += bands.p.pulses[i] + consumed;

            // Update the folding position only as long as we have 1 bit/sample depth
            update_lowband = b > (band_size as i32) << 3;
        }
    }

    fn denormalize(&mut self, ch: usize) {
        let p = &self.p;
        let block = &mut self.block[ch];
        for i in p.start_band..p.end_band {
            let off = (CELT_FREQ_BANDS[i] as usize) << p.size;
            let log_norm = block.energy[i] + CELT_MEAN_ENERGY[i];
            let norm = log_norm.min(32.0).exp2();
            for v in block.coeffs[off..off + ((CELT_FREQ_RANGE[i] as usize) << p.size)].iter_mut() {
                *v *= norm;
            }
        }
    }

    /// Anti-collapse over the coded channels. As libopus does, the bands are walked in order and,
    /// within each band, the channels, so the noise generator is shared in that order.
    fn process_anticollapse(&mut self) {
        let channels = self.p.coded_channels as usize;
        let (start, end, size) = (self.p.start_band, self.p.end_band, self.p.size);
        for i in start..end {
            let range = (CELT_FREQ_RANGE[i] as usize) << size;
            // depth in 1/8 bits
            let depth = (1 + self.p.pulses[i]) / range as i32;
            let thresh = (-1.0 - 0.125 * depth as f32).exp2();
            let sqrt_1 = 1.0 / (range as f32).sqrt();
            let xoff = (CELT_FREQ_BANDS[i] as usize) << size;
            for ch in 0..channels {
                let mut renormalize_needed = false;
                let mut prev = [self.block[ch].prev_energy[0][i], self.block[ch].prev_energy[1][i]];
                if channels == 1 {
                    prev[0] = prev[0].max(self.block[1].prev_energy[0][i]);
                    prev[1] = prev[1].max(self.block[1].prev_energy[1][i]);
                }
                let mut ediff = self.block[ch].energy[i] - prev[0].min(prev[1]);
                ediff = ediff.max(0.0);

                // r needs to be multiplied by 2 or 2*sqrt(2) depending on LM because short blocks don't
                // have the same energy as long
                let mut r = (1.0 - ediff).exp2();
                if size == 3 {
                    r *= std::f32::consts::SQRT_2;
                }
                r = thresh.min(r) * sqrt_1;
                for k in 0..(1usize << size) {
                    // Detect collapse
                    if self.block[ch].collapse_masks[i] & (1 << k) == 0 {
                        // Fill with noise
                        for j in 0..CELT_FREQ_RANGE[i] as usize {
                            let rnd = self.p.rng();
                            let v = if rnd & 0x8000 != 0 { r } else { -r };
                            self.block[ch].coeffs[xoff + (j << size) + k] = v;
                        }
                        renormalize_needed = true;
                    }
                }
                // We just added some energy, so we need to renormalize
                if renormalize_needed {
                    renormalize(&mut self.block[ch].coeffs[xoff..xoff + range], 1.0);
                }
            }
        }
    }

    /// Decode one CELT frame of `frame_size` samples (120, 240, 480 or 960) into `out[0..output_channels]`.
    pub fn decode_frame(
        &mut self,
        rc: &mut RangeDecoder,
        out: &mut [Vec<f32>; 2],
        channels: usize,
        frame_size: usize,
        start_band: usize,
        end_band: usize,
    ) -> Result<()> {
        if channels != 1 && channels != 2 {
            return Err(bad("invalid number of coded channels"));
        }
        if start_band > end_band || end_band > MAX_BANDS {
            return Err(bad("invalid band range"));
        }
        let size = (frame_size / SHORT_BLOCK).checked_ilog2().unwrap_or(0) as i32;
        if size > 3 || frame_size != SHORT_BLOCK << size {
            return Err(bad("invalid CELT frame size"));
        }
        self.flushed = false;
        {
            let p = &mut self.p;
            p.transient = false;
            p.coded_channels = channels as i32;
            p.start_band = start_band;
            p.end_band = end_band;
            p.framebits = rc.frame_bits();
            p.size = size;
            p.apply_phase_inv = self.apply_phase_inv;
        }
        for i in 0..channels {
            self.block[i].coeffs.fill(0.0);
            self.block[i].collapse_masks = [0; MAX_BANDS];
        }

        let mut consumed = rc.tell();
        // silence flag
        let mut silence = false;
        if consumed >= self.p.framebits {
            silence = true;
        } else if consumed == 1 {
            silence = rc.log(15) != 0;
        }
        if silence {
            consumed = self.p.framebits;
            rc.skip_bits((self.p.framebits - rc.tell()) as u32);
        }

        // post-filter options
        for b in self.block.iter_mut() {
            b.pf_gains_new = [0.0; 3];
        }
        if start_band == 0 && consumed + 16 <= self.p.framebits {
            if rc.log(1) != 0 {
                let octave = rc.uint(6);
                let period = (16 << octave) + rc.raw(4 + octave) as i32 - 1;
                let gain = 0.09375 * (rc.raw(3) + 1) as f32;
                let tapset = if rc.tell() + 2 <= self.p.framebits { rc.cdf(&CELT_MODEL_TAPSET) } else { 0 };
                for b in self.block.iter_mut() {
                    b.pf_period_new = period.max(POSTFILTER_MINPERIOD);
                    b.pf_gains_new = [gain * CELT_POSTFILTER_TAPS[tapset][0], gain * CELT_POSTFILTER_TAPS[tapset][1], gain * CELT_POSTFILTER_TAPS[tapset][2]];
                }
            }
            consumed = rc.tell();
        }

        // transient flag
        if size != 0 && consumed + 3 <= self.p.framebits {
            self.p.transient = rc.log(3) != 0;
        }
        self.p.blocks = if self.p.transient { 1 << size } else { 1 };
        self.p.blocksize = frame_size / self.p.blocks as usize;
        let imdct_idx = if self.p.transient { 0 } else { size as usize };

        if channels == 1 {
            for i in 0..MAX_BANDS {
                self.block[0].energy[i] = self.block[0].energy[i].max(self.block[1].energy[i]);
            }
        }

        self.decode_coarse_energy(rc);
        self.decode_tf_changes(rc);
        self.bitalloc(rc);
        self.decode_fine_energy(rc);
        self.quant_bands(rc);

        self.p.alloc_trim = self.p.alloc_trim.max(0);
        let anticollapse = if self.p.anticollapse_needed != 0 { rc.raw(1) != 0 } else { false };
        self.decode_final_energy(rc);

        // anti-collapse processing and denormalization of each coded channel
        if anticollapse {
            self.process_anticollapse();
        }
        for i in 0..channels {
            self.denormalize(i);
        }

        // stereo -> mono downmix
        let mut downmix = false;
        if self.output_channels < channels {
            let (a, b) = self.block.split_at_mut(1);
            for (x, y) in a[0].coeffs.iter_mut().zip(b[0].coeffs.iter()) {
                *x += *y;
            }
            downmix = true;
        } else if self.output_channels > channels {
            let (a, b) = self.block.split_at_mut(1);
            b[0].coeffs[..frame_size].copy_from_slice(&a[0].coeffs[..frame_size]);
        }

        if silence {
            for b in self.block.iter_mut() {
                b.energy = [ENERGY_SILENCE; MAX_BANDS];
                b.coeffs.fill(0.0);
            }
        }

        self.synthesize(out, frame_size, imdct_idx, downmix);

        if channels == 1 {
            let e = self.block[0].energy;
            self.block[1].energy = e;
        }

        for b in self.block.iter_mut() {
            if !self.p.transient {
                b.prev_energy[1] = b.prev_energy[0];
                b.prev_energy[0] = b.energy;
            } else {
                for j in 0..MAX_BANDS {
                    b.prev_energy[0][j] = b.prev_energy[0][j].min(b.energy[j]);
                }
            }
            for j in 0..start_band {
                b.prev_energy[0][j] = ENERGY_SILENCE;
                b.energy[j] = 0.0;
            }
            for j in end_band..MAX_BANDS {
                b.prev_energy[0][j] = ENERGY_SILENCE;
                b.energy[j] = 0.0;
            }
        }

        self.p.seed = rc.range();
        self.lost = 0;
        Ok(())
    }

    /// A frame of `frame_size` samples standing in for one that was lost: noise in each band at
    /// the last frame's energy, made 1.5 log2 units (9 dB) quieter for the first frame lost and 0.5
    /// (3 dB) for each one after, through the same synthesis as a decoded frame, so the next one
    /// that arrives joins it without a click. Before anything is decoded it is silence.
    pub fn conceal(&mut self, out: &mut [Vec<f32>; 2], frame_size: usize) -> Result<()> {
        let size = (frame_size / SHORT_BLOCK).checked_ilog2().unwrap_or(0) as i32;
        if size > 3 || frame_size != SHORT_BLOCK << size {
            return Err(bad("invalid CELT frame size"));
        }
        if self.flushed {
            for o in out.iter_mut() {
                o[..frame_size].fill(0.0);
            }
            return Ok(());
        }
        let decay = if self.lost == 0 { 1.5 } else { 0.5 };
        self.lost = self.lost.saturating_add(1);
        let end = if self.p.end_band == 0 { MAX_BANDS } else { self.p.end_band };
        {
            let p = &mut self.p;
            (p.size, p.transient, p.blocks, p.blocksize) = (size, false, 1, frame_size);
            (p.start_band, p.end_band, p.coded_channels) = (0, end, self.output_channels as i32);
        }
        for c in 0..self.output_channels {
            self.block[c].coeffs.fill(0.0);
            for i in 0..end {
                let e = &mut self.block[c].energy[i];
                *e = (*e - decay).max(ENERGY_SILENCE);
                let (off, w) = ((CELT_FREQ_BANDS[i] as usize) << size, (CELT_FREQ_RANGE[i] as usize) << size);
                for j in 0..w {
                    let r = self.p.rng();
                    self.block[c].coeffs[off + j] = if r & 0x8000 != 0 { 1.0 } else { -1.0 };
                }
                renormalize(&mut self.block[c].coeffs[off..off + w], 1.0);
            }
            self.denormalize(c);
            // the pitch post-filter fades out
            self.block[c].pf_gains_new = [0.0; 3];
        }
        self.synthesize(out, frame_size, size as usize, false);
        for b in self.block.iter_mut() {
            b.prev_energy[1] = b.prev_energy[0];
            b.prev_energy[0] = b.energy;
        }
        Ok(())
    }

    /// The inverse transform, overlap-add, post-filter and de-emphasis of the coefficients in
    /// each output channel's block, into `out`.
    fn synthesize(&mut self, out: &mut [Vec<f32>; 2], frame_size: usize, imdct_idx: usize, downmix: bool) {
        let blocks = self.p.blocks as usize;
        let blocksize = self.p.blocksize;
        for i in 0..self.output_channels {
            // iMDCT and overlap-add
            for j in 0..blocks {
                let dst = 1024 + j * blocksize;
                {
                    let block = &mut self.block[i];
                    let n = blocksize;
                    let tmp = &mut self.tmp[..n];
                    self.dct[imdct_idx].run(&block.coeffs[j..], blocks, tmp);
                    for t in 0..n {
                        block.buf[dst + OVERLAP / 2 + t] = IMDCT_SCALE * tmp[n - 1 - t];
                    }
                    // window the overlap
                    let len = OVERLAP / 2;
                    for t in 0..len {
                        let (ii, jj) = (t, len - 1 - t);
                        let s0 = block.buf[dst + ii];
                        let s1 = block.buf[dst + len + jj];
                        let wi = window(ii + len - len); // win[i] with i in -len..0 -> window[len + i]
                        let wj = window(len + jj);
                        let wi = {
                            let _ = wi;
                            window(ii)
                        };
                        block.buf[dst + ii] = s0 * wj - s1 * wi;
                        block.buf[dst + len + jj] = s0 * wi + s1 * wj;
                    }
                }
            }
            if downmix {
                for v in self.block[i].buf[1024..1024 + frame_size].iter_mut() {
                    *v *= 0.5;
                }
            }
            self.postfilter(i, frame_size);

            // de-emphasis
            let block = &mut self.block[i];
            let mut coeff = block.emph_coeff;
            let o = &mut out[i];
            for t in 0..frame_size {
                coeff = block.buf[1024 - frame_size + t] + coeff * EMPH_COEFF;
                o[t] = coeff;
            }
            block.emph_coeff = if coeff.is_normal() { coeff } else { 0.0 };
        }
    }

    fn postfilter(&mut self, ch: usize, frame_size: usize) {
        let len = self.p.blocksize * self.p.blocks as usize;
        let filter_len = len as i32 - 2 * OVERLAP as i32;
        let block = &mut self.block[ch];

        postfilter_transition(block, 1024);

        block.pf_period_old = block.pf_period;
        block.pf_gains_old = block.pf_gains;
        block.pf_period = block.pf_period_new;
        block.pf_gains = block.pf_gains_new;

        if len > OVERLAP {
            postfilter_transition(block, 1024 + OVERLAP);
            if block.pf_gains[0] > f32::EPSILON && filter_len > 0 {
                postfilter_comb(&mut block.buf, 1024 + 2 * OVERLAP, block.pf_period as usize, block.pf_gains, filter_len as usize);
            }
            block.pf_period_old = block.pf_period;
            block.pf_gains_old = block.pf_gains;
        }
        let _ = frame_size;
        block.buf.copy_within(len..len + 1024 + OVERLAP / 2, 0);
    }
}

fn postfilter_comb(buf: &mut [f32], base: usize, period: usize, g: [f32; 3], len: usize) {
    let mut x4 = buf[base - period - 2];
    let mut x3 = buf[base - period - 1];
    let mut x2 = buf[base - period];
    let mut x1 = buf[base - period + 1];
    for i in 0..len {
        let x0 = buf[base + i - period + 2];
        buf[base + i] += g[0] * x2 + g[1] * (x1 + x3) + g[2] * (x0 + x4);
        x4 = x3;
        x3 = x2;
        x2 = x1;
        x1 = x0;
    }
}

fn postfilter_transition(block: &mut Block, base: usize) {
    let t0 = block.pf_period_old as usize;
    let t1 = block.pf_period as usize;
    if block.pf_gains[0] == 0.0 && block.pf_gains_old[0] == 0.0 {
        return;
    }
    let [g00, g01, g02] = block.pf_gains_old;
    let [g10, g11, g12] = block.pf_gains;
    let buf = &mut block.buf;
    let mut x1 = buf[base - t1 + 1];
    let mut x2 = buf[base - t1];
    let mut x3 = buf[base - t1 - 1];
    let mut x4 = buf[base - t1 - 2];
    for i in 0..OVERLAP {
        let w = CELT_WINDOW2[i];
        let x0 = buf[base + i - t1 + 2];
        let d = |k: isize| buf[(base as isize + i as isize - t0 as isize + k) as usize];
        let v = (1.0 - w) * g00 * d(0) + (1.0 - w) * g01 * (d(-1) + d(1)) + (1.0 - w) * g02 * (d(-2) + d(2)) + w * g10 * x2 + w * g11 * (x1 + x3) + w * g12 * (x0 + x4);
        buf[base + i] += v;
        x4 = x3;
        x3 = x2;
        x2 = x1;
        x1 = x0;
    }
}
