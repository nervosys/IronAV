//! The CELT encoder: one 20 ms frame of 48 kHz audio, long blocks only, mono or stereo.
//!
//! It shares the decoder's bit allocation and band quantiser (`Celt::bitalloc`, `Celt::quant_bands`)
//! through the `Coder` trait, so the two cannot drift apart; this file adds the analysis (the
//! forward MDCT, band energies) and the parts that only an encoder has to decide (energy
//! quantisation, the pulse search, the stereo angle).
//!
//! Not done: transient detection and short blocks, the pitch pre-filter, band boosts, a tuned
//! allocation trim and variable bit rate.

use super::*;
use crate::opus::rc::RangeEncoder;

/// Samples in the frame this encoder codes.
pub const FRAME: usize = 960;
/// Samples of look-ahead a frame needs beyond its own: the window's overlap.
pub const LOOKAHEAD: usize = OVERLAP;
const SIZE: i32 = 3;
const END: usize = 21;
/// Undoes the decoder's scale: its transform is `IMDCT_SCALE` times a DCT-IV whose square is `FRAME / 2`.
const KAPPA: f32 = 1.0 / (IMDCT_SCALE * (FRAME / 2) as f32);

/// Rotate a band the way the decoder will undo (the inverse of `exp_rotation`).
pub(super) fn exp_rotation_fwd(x: &mut [f32], len: usize, stride: usize, k: u32, spread: i32) {
    if 2 * k as usize >= len || spread == SPREAD_NONE {
        return;
    }
    let gain = len as f32 / (len as f32 + (20 - 5 * spread) as f32 * k as f32);
    let theta = std::f32::consts::PI * gain * gain / 4.0;
    let (c, s) = (theta.cos(), theta.sin());
    let mut stride2 = 0;
    if len >= stride << 3 {
        stride2 = 1;
        while (stride2 * stride2 + stride2) * stride + (stride >> 2) < len {
            stride2 += 1;
        }
    }
    let len = len / stride;
    for i in 0..stride {
        let xs = &mut x[i * len..];
        exp_rotation_impl(xs, len, 1, c, -s);
        if stride2 != 0 {
            exp_rotation_impl(xs, len, stride2, s, -c);
        }
    }
}

/// The pulse vector of `k` pulses closest in direction to `x`.
pub(super) fn pvq_search(x: &[f32], k: usize, iy: &mut [i32]) {
    let n = x.len();
    let mut ax = [0f32; 256];
    let mut y = [0f32; 256];
    let mut negative = [false; 256];
    let mut sum = 0f32;
    for j in 0..n {
        negative[j] = x[j] < 0.0;
        ax[j] = x[j].abs();
        iy[j] = 0;
        sum += ax[j];
    }
    let (mut xy, mut yy) = (0f32, 0f32);
    let mut left = k as i32;
    // project onto the pyramid first, so the greedy search below has few pulses to place
    if k > n >> 1 {
        if !(sum > 1e-15 && sum < 64.0) {
            ax[0] = 1.0;
            ax[1..n].fill(0.0);
            sum = 1.0;
        }
        let rcp = (k as f32 + 0.8) / sum;
        for j in 0..n {
            let p = (rcp * ax[j]) as i32;
            iy[j] = p;
            let pf = p as f32;
            yy += pf * pf;
            xy += ax[j] * pf;
            y[j] = 2.0 * pf;
            left -= p;
        }
    }
    // silence or a pathological band: put the remaining pulses in the first bin
    if left > n as i32 + 3 {
        let t = left as f32;
        yy += t * t + t * y[0];
        iy[0] += left;
        left = 0;
    }
    for _ in 0..left {
        yy += 1.0;
        let rxy = xy + ax[0];
        let (mut best_num, mut best_den, mut best) = (rxy * rxy, yy + y[0], 0);
        for j in 1..n {
            let rxy = xy + ax[j];
            let (num, den) = (rxy * rxy, yy + y[j]);
            if best_den * num > den * best_num {
                best_num = num;
                best_den = den;
                best = j;
            }
        }
        xy += ax[best];
        yy += y[best];
        y[best] += 2.0;
        iy[best] += 1;
    }
    for j in 0..n {
        if negative[j] {
            iy[j] = -iy[j];
        }
    }
}

/// The index of a pulse vector among all those with its dimension and pulse count (the inverse of `cwrsi`).
pub(super) fn icwrs(n: usize, y: &[i32]) -> u32 {
    let mut j = n - 1;
    let mut i = (y[j] < 0) as u32;
    let mut k = y[j].unsigned_abs();
    loop {
        j -= 1;
        i = i.wrapping_add(pvq_u((n - j) as u32, k));
        k += y[j].unsigned_abs();
        if y[j] < 0 {
            i = i.wrapping_add(pvq_u((n - j) as u32, k + 1));
        }
        if j == 0 {
            break;
        }
    }
    i
}

/// The angle (0 to 16384) between two vectors' energies: for stereo, between mid and side.
pub(super) fn stereo_itheta(x: &[f32], y: &[f32], stereo: bool) -> i32 {
    let (mut mid, mut side) = (1e-15f32, 1e-15f32);
    if stereo {
        for (a, b) in x.iter().zip(y) {
            let (m, s) = (0.5 * (a + b), 0.5 * (a - b));
            mid += m * m;
            side += s * s;
        }
    } else {
        for (a, b) in x.iter().zip(y) {
            mid += a * a;
            side += b * b;
        }
    }
    (0.5 + 16384.0 * 0.636_62 * side.sqrt().atan2(mid.sqrt())).floor() as i32
}

/// Left and right to mid and side, in place.
pub(super) fn stereo_split(x: &mut [f32], y: &mut [f32]) {
    let h = std::f32::consts::FRAC_1_SQRT_2;
    for (a, b) in x.iter_mut().zip(y.iter_mut()) {
        let (l, r) = (h * *a, h * *b);
        *a = l + r;
        *b = r - l;
    }
}

/// Fold both channels into `x`, weighted by their energies (the one channel intensity stereo codes).
pub(super) fn intensity_stereo(x: &mut [f32], y: &[f32], amp: [f32; 2]) {
    let norm = 1e-15 + (1e-15 + amp[0] * amp[0] + amp[1] * amp[1]).sqrt();
    let (a1, a2) = (amp[0] / norm, amp[1] / norm);
    for (a, b) in x.iter_mut().zip(y) {
        *a = a1 * *a + a2 * *b;
    }
}

impl Celt {
    /// The MDCT of one channel's frame: `x` is `FRAME + LOOKAHEAD` pre-emphasised samples, the first
    /// of which is the frame's first. It inverts the decoder's inverse transform and window.
    fn forward(&mut self, x: &[f32], out: &mut [f32]) {
        let w = window;
        let mut z = [0f32; FRAME];
        // the rising edge folds the 120 samples around the frame's start into 60 values
        for jj in 0..OVERLAP / 2 {
            let ii = OVERLAP / 2 - 1 - jj;
            z[jj] = -w(ii) * x[ii] + w(OVERLAP - 1 - ii) * x[OVERLAP / 2 + jj];
        }
        z[OVERLAP / 2..FRAME - OVERLAP / 2].copy_from_slice(&x[OVERLAP..FRAME]);
        // and the falling edge, around the next frame's start
        for ii in 0..OVERLAP / 2 {
            z[FRAME - OVERLAP / 2 + ii] = w(OVERLAP - 1 - ii) * x[FRAME + ii] + w(ii) * x[FRAME + OVERLAP - 1 - ii];
        }
        let mut y = [0f32; FRAME];
        for k in 0..FRAME {
            y[k] = z[FRAME - 1 - k];
        }
        self.dct[SIZE as usize].run(&y, 1, out);
        for v in out.iter_mut() {
            *v *= KAPPA;
        }
    }

    /// Code the frame in `sig` (one slice of `FRAME + LOOKAHEAD` pre-emphasised samples per channel)
    /// into `rc`. `silent` marks a frame of digital silence; `intra` codes the band energies
    /// without prediction from the previous frame.
    pub fn encode_frame(&mut self, rc: &mut RangeEncoder, sig: &[&[f32]], silent: bool, intra: bool) {
        let channels = sig.len();
        self.flushed = false;
        {
            let p = &mut self.p;
            p.transient = false;
            p.coded_channels = channels as i32;
            p.start_band = 0;
            p.end_band = END;
            p.framebits = rc.frame_bits();
            p.size = SIZE;
            p.blocks = 1;
            p.blocksize = FRAME;
            p.apply_phase_inv = self.apply_phase_inv;
        }
        let mut consumed = rc.tell();
        if consumed == 1 {
            rc.log(15, silent as u32);
        }
        if silent {
            for b in self.block.iter_mut() {
                b.energy = [ENERGY_SILENCE; MAX_BANDS];
            }
            return;
        }
        // no pitch post-filter, no transient
        if consumed + 16 <= self.p.framebits {
            rc.log(1, 0);
            consumed = rc.tell();
        }
        if consumed + 3 <= self.p.framebits {
            rc.log(3, 0);
        }

        let mut log_e = [[0f32; MAX_BANDS]; 2];
        for c in 0..channels {
            let mut coeffs = std::mem::take(&mut self.block[c].coeffs);
            coeffs.fill(0.0);
            self.forward(sig[c], &mut coeffs[..FRAME]);
            for i in 0..END {
                let off = (CELT_FREQ_BANDS[i] as usize) << SIZE;
                let band = &mut coeffs[off..off + ((CELT_FREQ_RANGE[i] as usize) << SIZE)];
                let a = (band.iter().map(|v| v * v).sum::<f32>() + 1e-27).sqrt();
                self.enc_amp[c][i] = a;
                log_e[c][i] = (a.log2() - CELT_MEAN_ENERGY[i]).max(ENERGY_SILENCE);
                let g = 1.0 / a;
                for v in band.iter_mut() {
                    *v *= g;
                }
            }
            self.block[c].collapse_masks = [0; MAX_BANDS];
            self.block[c].coeffs = coeffs;
        }

        let mut error = [[0f32; MAX_BANDS]; 2];
        self.encode_coarse_energy(rc, &log_e, intra, &mut error);
        self.decode_tf_changes(rc);
        self.bitalloc(rc);
        self.encode_fine_energy(rc, &mut error);
        self.quant_bands(rc);
        self.encode_final_energy(rc, &mut error);

        if channels == 1 {
            let e = self.block[0].energy;
            self.block[1].energy = e;
        }
    }

    fn encode_coarse_energy(&mut self, rc: &mut RangeEncoder, log_e: &[[f32; MAX_BANDS]; 2], intra: bool, error: &mut [[f32; MAX_BANDS]; 2]) {
        let p = &self.p;
        let size = p.size as usize;
        let budget = p.framebits;
        let channels = p.coded_channels as usize;
        let mut prev = [0f32; 2];
        let mut alpha = CELT_ALPHA_COEF[size];
        let mut beta = CELT_BETA_COEF[size];
        let mut model = &CELT_COARSE_ENERGY_DIST[size][0];
        if rc.tell() + 3 <= budget {
            rc.log(3, intra as u32);
            if intra {
                alpha = 0.0;
                beta = 1.0 - 4915.0 / 32768.0;
                model = &CELT_COARSE_ENERGY_DIST[size][1];
            }
        }
        // a band's energy may not fall faster than this per frame, so a band that goes quiet is
        // not coded as an absurdly large step
        let mut max_decay = 16.0f32;
        if p.end_band - p.start_band > 10 {
            max_decay = max_decay.min(0.125 * (budget / 8) as f32);
        }
        for i in 0..MAX_BANDS {
            for c in 0..channels {
                let block = &mut self.block[c];
                if i < p.start_band || i >= p.end_band {
                    block.energy[i] = 0.0;
                    continue;
                }
                let x = log_e[c][i];
                let old = block.energy[i].max(-9.0);
                let f = x - alpha * old - prev[c];
                let mut qi = (0.5 + f).floor() as i32;
                let decay_bound = block.energy[i].max(-28.0) - max_decay;
                if qi < 0 && x < decay_bound {
                    qi += (decay_bound - x) as i32;
                    if qi > 0 {
                        qi = 0;
                    }
                }
                let tell = rc.tell();
                let bits_left = budget - tell - 3 * channels as i32 * (p.end_band - i) as i32;
                if i != p.start_band && bits_left < 30 {
                    if bits_left < 24 {
                        qi = qi.min(1);
                    }
                    if bits_left < 16 {
                        qi = qi.max(-1);
                    }
                }
                let available = budget - tell;
                if available >= 15 {
                    let k = i.min(20) << 1;
                    qi = rc.laplace(qi, (model[k] as u32) << 7, (model[k + 1] as u32) << 6);
                } else if available >= 2 {
                    qi = qi.clamp(-1, 1);
                    Coder::cdf(rc, &CELT_MODEL_TAPSET, ((2 * qi) ^ -((qi < 0) as i32)) as usize);
                } else if available >= 1 {
                    qi = qi.min(0);
                    Coder::log(rc, 1, (-qi) as u32);
                } else {
                    qi = -1;
                }
                error[c][i] = f - qi as f32;
                let q = qi as f32;
                block.energy[i] = old * alpha + prev[c] + q;
                prev[c] += beta * q;
            }
        }
    }

    fn encode_fine_energy(&mut self, rc: &mut RangeEncoder, error: &mut [[f32; MAX_BANDS]; 2]) {
        let p = &self.p;
        for i in p.start_band..p.end_band {
            let bits = p.fine_bits[i];
            if bits == 0 {
                continue;
            }
            for c in 0..p.coded_channels as usize {
                let extra = 1 << bits;
                let q2 = (((error[c][i] + 0.5) * extra as f32).floor() as i32).clamp(0, extra - 1);
                Coder::raw(rc, bits as u32, q2 as u32);
                let offset = (q2 as f32 + 0.5) * (1 << (14 - bits)) as f32 / 16384.0 - 0.5;
                self.block[c].energy[i] += offset;
                error[c][i] -= offset;
            }
        }
    }

    fn encode_final_energy(&mut self, rc: &mut RangeEncoder, error: &mut [[f32; MAX_BANDS]; 2]) {
        let p = &self.p;
        let mut bits_left = p.framebits - rc.tell();
        for priority in 0..2 {
            let mut i = p.start_band;
            while i < p.end_band && bits_left >= p.coded_channels {
                if p.fine_priority[i] != priority || p.fine_bits[i] >= MAX_FINE_BITS {
                    i += 1;
                    continue;
                }
                for c in 0..p.coded_channels as usize {
                    let q2 = (error[c][i] >= 0.0) as u32;
                    Coder::raw(rc, 1, q2);
                    let offset = (q2 as f32 - 0.5) * (1 << (14 - p.fine_bits[i] - 1)) as f32 / 16384.0;
                    self.block[c].energy[i] += offset;
                    error[c][i] -= offset;
                    bits_left -= 1;
                }
                i += 1;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(n: usize, seed: u32) -> Vec<f32> {
        let mut s = seed;
        (0..n)
            .map(|_| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                (s >> 8) as f32 / (1 << 23) as f32 - 1.0
            })
            .collect()
    }

    #[test]
    fn pulse_indexes_invert_cwrsi() {
        for (n, k) in [(2usize, 1u32), (3, 2), (8, 5), (16, 3), (22, 7), (40, 4), (10, 14), (12, 12), (4, 40), (3, 100)] {
            let x = noise(n, (n * 100 + k as usize) as u32);
            let mut iy = vec![0i32; n];
            pvq_search(&x, k as usize, &mut iy);
            assert_eq!(iy.iter().map(|v| v.unsigned_abs()).sum::<u32>(), k, "n {n} k {k}: {iy:?}");
            let idx = icwrs(n, &iy);
            assert!(idx < pvq_v(n as u32, k), "n {n} k {k}");
            let mut back = vec![0i32; n];
            cwrsi(n as u32, k, idx, &mut back);
            assert_eq!(back, iy, "n {n} k {k}");
        }
    }

    #[test]
    fn the_forward_transform_inverts_the_decoders_synthesis() {
        let frames = 5;
        let total = (frames + 1) * FRAME + LOOKAHEAD;
        let mut x = noise(total, 7);
        x[..OVERLAP].fill(0.0);
        let mut celt = Celt::new(1, true);
        let mut buf = vec![0f32; 2048];
        let mut tmp = vec![0f32; FRAME];
        for t in 0..frames {
            let mut coeffs = vec![0f32; FRAME];
            celt.forward(&x[t * FRAME..t * FRAME + FRAME + LOOKAHEAD], &mut coeffs);
            // the decoder's synthesis, as in `decode_frame`
            let dst = 1024;
            celt.dct[SIZE as usize].run(&coeffs, 1, &mut tmp);
            for i in 0..FRAME {
                buf[dst + OVERLAP / 2 + i] = IMDCT_SCALE * tmp[FRAME - 1 - i];
            }
            let len = OVERLAP / 2;
            for i in 0..len {
                let jj = len - 1 - i;
                let (s0, s1) = (buf[dst + i], buf[dst + len + jj]);
                let (wi, wj) = (window(i), window(len + jj));
                buf[dst + i] = s0 * wj - s1 * wi;
                buf[dst + len + jj] = s0 * wi + s1 * wj;
            }
            if t > 0 {
                for i in 0..FRAME {
                    let want = x[t * FRAME + i];
                    assert!((buf[dst + i] - want).abs() < 2e-4, "frame {t} sample {i}: {} vs {want}", buf[dst + i]);
                }
            }
            buf.copy_within(FRAME..FRAME + 1024 + OVERLAP / 2, 0);
        }
    }
}
