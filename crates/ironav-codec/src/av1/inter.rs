//! Inter prediction (spec 7.11.3, with libaom's warp filter rounding): sub-sample motion
//! compensation with reference scaling, warped and global motion, compound averaging, distance
//! weights, wedge and difference masks, inter-intra and overlapped block motion compensation.

use std::sync::OnceLock;

use super::block::*;
use super::inter_tables::*;
use super::ipred::{predict_intra, Edges, Plane};
use super::mvpred::*;
use super::obu::{rel_dist, GM_TRANSLATION};
use super::tables::*;

#[inline]
fn round2(x: i32, n: u32) -> i32 {
    if n == 0 {
        x
    } else {
        (x + (1 << (n - 1))) >> n
    }
}

fn round2s(x: i64, n: u32) -> i64 {
    if n == 0 {
        x
    } else if x >= 0 {
        (x + (1 << (n - 1))) >> n
    } else {
        -((-x + (1 << (n - 1))) >> n)
    }
}

/// resolve_divisor: the shift and the multiplier that divide by `d`.
fn resolve_divisor(d: u64) -> (u32, i32) {
    let n = 63 - d.leading_zeros();
    let e = d - (1 << n);
    let f = if n > 8 { (e + (1 << (n - 9))) >> (n - 8) } else { e << (8 - n) };
    (n + 14, DIV_LUT[f as usize] as i32)
}

/// setupShear (libaom's av1_get_shear_params): alpha, beta, gamma and delta, if the model can
/// warp.
pub fn setup_shear(p: &[i32; 6]) -> Option<[i32; 4]> {
    if p[2] <= 0 {
        return None;
    }
    let c = |v: i64| v.clamp(-32768, 32767);
    let alpha = c(p[2] as i64 - (1 << 16));
    let beta = c(p[3] as i64);
    let (shift, f) = resolve_divisor(p[2] as u64);
    let gamma = c(round2s(((p[4] as i64) << 16) * f as i64, shift));
    let delta = c(p[5] as i64 - round2s(p[3] as i64 * p[4] as i64 * f as i64, shift) - (1 << 16));
    let r = |v: i64| (round2s(v, 6) << 6) as i32;
    let s = [r(alpha), r(beta), r(gamma), r(delta)];
    if 4 * s[0].abs() + 7 * s[1].abs() >= 1 << 16 || 4 * s[2].abs() + 4 * s[3].abs() >= 1 << 16 {
        return None;
    }
    Some(s)
}

/// warpEstimation (libaom's find_affine_int): the local warp model of a block from its samples.
pub fn warp_estimation(ws: &WarpSamples, mv: Mv, bs: usize, row: usize, col: usize) -> Option<[i32; 6]> {
    let (bw, bh) = (4i32 << MI_W_LOG2[bs], 4i32 << MI_H_LOG2[bs]);
    let mid_y = row as i32 * 4 + bh / 2 - 1;
    let mid_x = col as i32 * 4 + bw / 2 - 1;
    let (suy, sux) = (mid_y * 8, mid_x * 8);
    let (duy, dux) = (suy + mv[0], sux + mv[1]);
    let sq = |a: i32| (a * a * 4 + a * 4 * 8 + 128) >> 4;
    let p1 = |a: i32, b: i32| (a * b * 4 + (a + b) * 16 + 64) >> 4;
    let p2 = |a: i32, b: i32| (a * b * 4 + (a + b) * 16 + 128) >> 4;
    let (mut a, mut bx, mut by) = ([[0i32; 2]; 2], [0i32; 2], [0i32; 2]);
    for c in &ws.cand[..ws.num] {
        let (sy, sx, dy, dx) = (c[0] - suy, c[1] - sux, c[2] - duy, c[3] - dux);
        if (sx - dx).abs() < 256 && (sy - dy).abs() < 256 {
            a[0][0] += sq(sx);
            a[0][1] += p1(sx, sy);
            a[1][1] += sq(sy);
            bx[0] += p2(sx, dx);
            bx[1] += p1(sy, dx);
            by[0] += p1(sx, dy);
            by[1] += p2(sy, dy);
        }
    }
    let (a00, a01, a11) = (a[0][0] as i64, a[0][1] as i64, a[1][1] as i64);
    let det = a00 * a11 - a01 * a01;
    if det == 0 {
        return None;
    }
    let (shift, f) = resolve_divisor(det.unsigned_abs());
    // libaom keeps the inverse in 16 bits.
    let mut idet = (f * det.signum() as i32) as i16;
    let mut shift = shift as i32 - 16;
    if shift < 0 {
        idet = ((idet as i32) << -shift) as i16;
        shift = 0;
    }
    let idet = idet as i64;
    let px = [a11 * bx[0] as i64 - a01 * bx[1] as i64, -a01 * bx[0] as i64 + a00 * bx[1] as i64];
    let py = [a11 * by[0] as i64 - a01 * by[1] as i64, -a01 * by[0] as i64 + a00 * by[1] as i64];
    let nd = |v: i64| round2s(v * idet, shift as u32).clamp(-(1 << 13) + 1, (1 << 13) - 1) as i32;
    let dg = |v: i64| round2s(v * idet, shift as u32).clamp((1 << 16) - (1 << 13) + 1, (1 << 16) + (1 << 13) - 1) as i32;
    let mut p = [0, 0, dg(px[0]), nd(px[1]), nd(py[0]), dg(py[1])];
    let vx = mv[1] as i64 * (1 << 13) - (mid_x as i64 * (p[2] as i64 - (1 << 16)) + mid_y as i64 * p[3] as i64);
    let vy = mv[0] as i64 * (1 << 13) - (mid_x as i64 * p[4] as i64 + mid_y as i64 * (p[5] as i64 - (1 << 16)));
    p[0] = vx.clamp(-(1 << 23), (1 << 23) - 1) as i32;
    p[1] = vy.clamp(-(1 << 23), (1 << 23) - 1) as i32;
    Some(p)
}

/// The wedge master masks by direction (7.11.3.11).
fn masters() -> &'static [[[u8; 64]; 64]; 6] {
    static M: OnceLock<Box<[[[u8; 64]; 64]; 6]>> = OnceLock::new();
    M.get_or_init(|| {
        let mut m = Box::new([[[0u8; 64]; 64]; 6]);
        let (o63, v) = (WEDGE_OBLIQUE63 as usize, WEDGE_VERTICAL as usize);
        for j in 0..64 {
            let mut shift = 16i32;
            for i in (0..64).step_by(2) {
                m[o63][i][j] = WEDGE_MASTER_OBLIQUE_EVEN[(j as i32 - shift).clamp(0, 63) as usize];
                shift -= 1;
                m[o63][i + 1][j] = WEDGE_MASTER_OBLIQUE_ODD[(j as i32 - shift).clamp(0, 63) as usize];
                m[v][i][j] = WEDGE_MASTER_VERTICAL[j];
                m[v][i + 1][j] = WEDGE_MASTER_VERTICAL[j];
            }
        }
        for i in 0..64 {
            for j in 0..64 {
                let k = m[o63][i][j];
                m[WEDGE_OBLIQUE27 as usize][j][i] = k;
                m[WEDGE_OBLIQUE117 as usize][i][63 - j] = 64 - k;
                m[WEDGE_OBLIQUE153 as usize][63 - j][i] = 64 - k;
                m[WEDGE_HORIZONTAL as usize][j][i] = m[v][i][j];
            }
        }
        m
    })
}

/// The wedge mask of a block size, at its luma size, into `out`.
fn wedge_mask(bs: usize, sign: usize, idx: usize, out: &mut [u8]) {
    let (w, h) = (4usize << MI_W_LOG2[bs], 4usize << MI_H_LOG2[bs]);
    let cb = if h > w { 0 } else if h < w { 1 } else { 2 };
    let (dir, xo, yo) = WEDGE_CODEBOOK[cb][idx];
    let (xoff, yoff) = (32 - ((xo as usize * w) >> 3), 32 - ((yo as usize * h) >> 3));
    let m = &masters()[dir as usize];
    let sum: u32 = (0..w).map(|i| m[yoff][xoff + i] as u32).sum::<u32>() + (1..h).map(|i| m[yoff + i][xoff] as u32).sum::<u32>();
    let n = (w + h - 1) as u32;
    let flip = ((sum + (n - 1) / 2) / n < 32) as usize;
    let inv = sign ^ flip == 1;
    for i in 0..h {
        for j in 0..w {
            let v = m[yoff + i][xoff + j];
            out[i * w + j] = if inv { 64 - v } else { v };
        }
    }
}

/// The filter of a direction for a block dimension (4-tap for 4 and under).
fn filter_idx(f: u8, n: usize) -> usize {
    if n <= 4 {
        match f {
            0 | 2 => 4,
            1 => 5,
            f => f as usize,
        }
    } else {
        f as usize
    }
}

impl FrameDec {
    pub(crate) fn sub_of(&self, plane: usize) -> (usize, usize) {
        if plane == 0 {
            (0, 0)
        } else {
            (self.ss[0], self.ss[1])
        }
    }

    /// compute_prediction for an inter block.
    pub(crate) fn predict_inter_block(&mut self) {
        self.predict_inter_planes(0..1 + 2 * self.b.has_chroma as usize);
    }

    /// The prediction of the current block into the planes in `planes` (the encoder's motion search
    /// reads luma alone).
    pub(crate) fn predict_inter_planes(&mut self, planes: std::ops::Range<usize>) {
        let (row, col, bs) = (self.b.row, self.b.col, self.b.size);
        for plane in planes {
            let (ssx, ssy) = self.sub_of(plane);
            let psz = plane_block(bs, ssx, ssy);
            let (n4w, n4h) = (NUM_4X4_W[psz] as usize, NUM_4X4_H[psz] as usize);
            let (base_x, base_y) = ((col >> ssx) * 4, (row >> ssy) * 4);
            if self.b.interintra {
                let mode = [DC_PRED, 1, 2, SMOOTH_PRED][self.b.ii_mode as usize];
                let e = Edges {
                    have_left: if plane == 0 { self.b.avail_l } else { self.b.avail_l_c },
                    have_above: if plane == 0 { self.b.avail_u } else { self.b.avail_u_c },
                    have_above_right: false,
                    have_below_left: false,
                    max_x: ((self.h.mi_cols * 4) >> ssx) as i32 - 1,
                    max_y: ((self.h.mi_rows * 4) >> ssy) as i32 - 1,
                };
                let (lw, lh) = (MI_W_LOG2[psz] as u32 + 2, MI_H_LOG2[psz] as u32 + 2);
                let ef = self.seq.enable_intra_edge_filter;
                predict_intra(&mut self.planes[plane], base_x, base_y, lw, lh, mode, 0, None, &e, false, ef, self.bd);
            }
            let (mut cr, mut cc) = ((row >> ssy) << ssy, (col >> ssx) << ssx);
            let (mut pw, mut ph) = ((4usize << MI_W_LOG2[bs]) >> ssx, (4usize << MI_H_LOG2[bs]) >> ssy);
            let sub8 = (self.b.bw4 == 1 && ssx == 1) || (self.b.bh4 == 1 && ssy == 1);
            let some_intra = !sub8 || (0..n4h << ssy).any(|r| (0..n4w << ssx).any(|c| self.mi_at(cr + r, cc + c).ref_frame[0] <= 0));
            if some_intra {
                (pw, ph, cr, cc) = (n4w * 4, n4h * 4, row, col);
            }
            for (ri, y) in (0..n4h * 4).step_by(ph).enumerate() {
                for (ci, x) in (0..n4w * 4).step_by(pw).enumerate() {
                    self.predict_inter(plane, base_x + x, base_y + y, pw, ph, cr + ri, cc + ci);
                }
            }
        }
    }

    fn rounding(&self, compound: bool) -> (u32, u32) {
        let r0 = if self.bd == 12 { 5 } else { 3 };
        let r1 = if compound {
            7
        } else if self.bd == 12 {
            9
        } else {
            11
        };
        (r0, r1)
    }

    #[allow(clippy::too_many_arguments)]
    fn predict_inter(&mut self, plane: usize, x: usize, y: usize, w: usize, h: usize, cr: usize, cc: usize) {
        let m = *self.mi_at(cr, cc);
        let compound = m.ref_frame[1] > 0;
        let (r0, r1) = self.rounding(compound);
        let mode = self.b.y_mode;
        // intra block copy (RefFrame INTRA_FRAME of an inter block): the current frame, which the
        // frame's filters never touch, so it is predicted from the plane being decoded
        let intrabc = m.is_inter && m.ref_frame[0] == 0;
        for l in 0..1 + compound as usize {
            if intrabc {
                let cur = std::mem::replace(&mut self.planes[plane], Plane::new(0, 0));
                let (w_, h_) = (self.h.upscaled_width as usize, self.h.height as usize);
                self.block_inter(&cur, (w_, h_), l, plane, x, y, w, h, m.mv[l], m.interp, r0, r1);
                self.planes[plane] = cur;
                continue;
            }
            let rf = m.ref_frame[l] as usize;
            let Some(refp) = self.refs[rf].clone() else { continue };
            let warp = if w < 8 || h < 8 || self.h.force_integer_mv {
                None
            } else if self.b.motion_mode == MOTION_LOCALWARP && self.b.local_valid {
                setup_shear(&self.b.local_warp).map(|s| (self.b.local_warp, s))
            } else if (mode == GLOBALMV || mode == GLOBAL_GLOBALMV) && self.h.gm_type[rf] > GM_TRANSLATION && !self.is_scaled(rf as i8) {
                self.gm_shear[rf].map(|s| (self.h.gm_params[rf], s))
            } else {
                None
            };
            match warp {
                Some((p, s)) => self.block_warp(&refp, l, plane, x, y, w, h, &p, &s, r0, r1),
                None => self.block_inter(&refp.planes[plane], (refp.width, refp.height), l, plane, x, y, w, h, m.mv[l], m.interp, r0, r1),
            }
        }
        let (ssx, ssy) = self.sub_of(plane);
        let bs = self.b.size;
        let bw = 4usize << MI_W_LOG2[bs];
        let ct = self.b.compound_type;
        let ii = self.b.interintra;
        let post = if compound { 14 - r0 - r1 } else { 0 };
        let max = (1i32 << self.bd) - 1;
        let (fw, bw_) = if compound && ct == COMPOUND_DISTANCE { self.dist_weights(&m) } else { (0, 0) };
        if ct == COMPOUND_WEDGE && plane == 0 && (compound || ii) {
            wedge_mask(bs, self.b.wedge_sign, self.b.wedge_index, &mut self.mask);
        } else if ct == COMPOUND_DIFFWTD && plane == 0 && compound {
            let sh = self.bd - 8 + post;
            let inv = self.b.mask_type == 1;
            for i in 0..w * h {
                let d = round2((self.pred[0][i] - self.pred[1][i]).abs(), sh);
                let v = (38 + d / 16).clamp(0, 64) as u8;
                self.mask[i] = if inv { 64 - v } else { v };
            }
        } else if ct == COMPOUND_INTRA && ii {
            let scale = 128 / w.max(h);
            let mode = self.b.ii_mode;
            for i in 0..h {
                for j in 0..w {
                    self.mask[i * w + j] = match mode {
                        1 => II_WEIGHTS_1D[i * scale],
                        2 => II_WEIGHTS_1D[j * scale],
                        3 => II_WEIGHTS_1D[i.min(j) * scale],
                        _ => 32,
                    };
                }
            }
        }
        let (p0, p1) = (&self.pred[0], &self.pred[1]);
        let dst = &mut self.planes[plane];
        let stride = dst.stride;
        if !compound && !ii {
            for i in 0..h {
                let d = &mut dst.data[(y + i) * stride + x..][..w];
                for (o, &v) in d.iter_mut().zip(&p0[i * w..(i + 1) * w]) {
                    *o = v.clamp(0, max) as u16;
                }
            }
        } else if compound && ct == COMPOUND_AVERAGE {
            for i in 0..h {
                for j in 0..w {
                    let k = i * w + j;
                    dst.data[(y + i) * stride + x + j] = round2(p0[k] + p1[k], 1 + post).clamp(0, max) as u16;
                }
            }
        } else if compound && ct == COMPOUND_DISTANCE {
            for i in 0..h {
                for j in 0..w {
                    let k = i * w + j;
                    dst.data[(y + i) * stride + x + j] = round2(fw * p0[k] + bw_ * p1[k], 4 + post).clamp(0, max) as u16;
                }
            }
        } else {
            // mask_blend
            let full = (ssx == 0 && ssy == 0) || (ii && !self.b.wedge_ii);
            let ms = if ii && !self.b.wedge_ii { w } else { bw };
            let mk = &self.mask;
            for i in 0..h {
                for j in 0..w {
                    let mv = if full {
                        mk[i * ms + j] as i32
                    } else if ssy == 0 {
                        round2(mk[i * ms + 2 * j] as i32 + mk[i * ms + 2 * j + 1] as i32, 1)
                    } else if ssx == 0 {
                        round2(mk[2 * i * ms + j] as i32 + mk[(2 * i + 1) * ms + j] as i32, 1)
                    } else {
                        let (a, b) = (2 * i * ms + 2 * j, (2 * i + 1) * ms + 2 * j);
                        round2(mk[a] as i32 + mk[a + 1] as i32 + mk[b] as i32 + mk[b + 1] as i32, 2)
                    };
                    let k = i * w + j;
                    let o = &mut dst.data[(y + i) * stride + x + j];
                    *o = if ii {
                        round2(mv * *o as i32 + (64 - mv) * p0[k].clamp(0, max), 6) as u16
                    } else {
                        round2(mv * p0[k] + (64 - mv) * p1[k], 6 + post).clamp(0, max) as u16
                    };
                }
            }
        }
        if self.b.motion_mode == MOTION_OBMC {
            self.obmc(plane, w, h);
        }
    }

    /// distance_weights: FwdWeight and BckWeight.
    fn dist_weights(&self, m: &Mi) -> (i32, i32) {
        let d = |l: usize| rel_dist(&self.seq, self.h.order_hints[m.ref_frame[l] as usize], self.h.order_hint).abs().clamp(0, 31);
        let (d0, d1) = (d(1), d(0));
        let order = (d0 <= d1) as usize;
        if d0 == 0 || d1 == 0 {
            return (QUANT_DIST_LOOKUP[3][order], QUANT_DIST_LOOKUP[3][1 - order]);
        }
        let mut i = 0;
        while i < 3 {
            let (c0, c1) = (QUANT_DIST_WEIGHT[i][order], QUANT_DIST_WEIGHT[i][1 - order]);
            if (d0 > d1 && d0 * c0 < d1 * c1) || (d0 <= d1 && d0 * c0 > d1 * c1) {
                break;
            }
            i += 1;
        }
        (QUANT_DIST_LOOKUP[i][order], QUANT_DIST_LOOKUP[i][1 - order])
    }

    /// motion_vector_scaling and block_inter_prediction into `pred[l]` (stride `w`).
    #[allow(clippy::too_many_arguments)]
    fn block_inter(&mut self, rp: &Plane, (ref_w, ref_h): (usize, usize), l: usize, plane: usize, x: usize, y: usize, w: usize, h: usize, mv: Mv, filt: [u8; 2], r0: u32, r1: u32) {
        let (ssx, ssy) = self.sub_of(plane);
        let last_x = ((ref_w + ssx) >> ssx) as i32 - 1;
        let last_y = ((ref_h + ssy) >> ssy) as i32 - 1;
        let (fw, fh) = (self.h.width as i64, self.h.height as i64);
        let x_scale = (((ref_w as i64) << 14) + fw / 2) / fw;
        let y_scale = (((ref_h as i64) << 14) + fh / 2) / fh;
        let fx = &SUBPEL_FILTERS[filter_idx(filt[1], w)];
        let fy = &SUBPEL_FILTERS[filter_idx(filt[0], h)];
        let (tmp, out) = (&mut self.tmp, &mut self.pred[l]);
        if x_scale == 1 << 14 && y_scale == 1 << 14 {
            let px = x as i32 * 16 + ((2 * mv[1]) >> ssx);
            let py = y as i32 * 16 + ((2 * mv[0]) >> ssy);
            let (ix, iy) = (px >> 4, py >> 4);
            let (hf, vf) = (&fx[(px & 15) as usize], &fy[(py & 15) as usize]);
            let (hcopy, vcopy) = (px & 15 == 0, py & 15 == 0);
            let mut line = [0i32; 128 + 7];
            let inside = ix - 3 >= 0 && ix + w as i32 + 4 <= last_x;
            // Phase 0 is a copy (Subpel_Filters[..][0] is 128 at tap 3): only the rows it reads.
            for r in if vcopy { 3..h + 3 } else { 0..h + 7 } {
                let yy = (iy + r as i32 - 3).clamp(0, last_y) as usize;
                let src = &rp.data[yy * rp.stride..];
                if inside {
                    let s = &src[(ix - 3) as usize..];
                    for (c, v) in line[..w + 7].iter_mut().enumerate() {
                        *v = s[c] as i32;
                    }
                } else {
                    for (c, v) in line[..w + 7].iter_mut().enumerate() {
                        *v = src[(ix - 3 + c as i32).clamp(0, last_x) as usize] as i32;
                    }
                }
                let t = &mut tmp[r * w..(r + 1) * w];
                if hcopy {
                    for (o, &v) in t.iter_mut().zip(&line[3..w + 3]) {
                        *o = v << (7 - r0);
                    }
                } else {
                    for (c, o) in t.iter_mut().enumerate() {
                        let s: i32 = (0..8).map(|k| hf[k] as i32 * line[c + k]).sum();
                        *o = round2(s, r0);
                    }
                }
            }
            if vcopy {
                for (o, &v) in out[..w * h].iter_mut().zip(&tmp[3 * w..(h + 3) * w]) {
                    *o = round2(v << 7, r1);
                }
            } else {
                for r in 0..h {
                    for c in 0..w {
                        let s: i32 = (0..8).map(|k| vf[k] as i32 * tmp[(r + k) * w + c]).sum();
                        out[r * w + c] = round2(s, r1);
                    }
                }
            }
            return;
        }
        let ox = ((x as i64) << 4) + ((2 * mv[1]) >> ssx) as i64 + 8;
        let oy = ((y as i64) << 4) + ((2 * mv[0]) >> ssy) as i64 + 8;
        let sx = round2s(ox * x_scale - (8 << 14), 8) + 32;
        let sy = round2s(oy * y_scale - (8 << 14), 8) + 32;
        let (xs, ys) = (round2s(x_scale, 4), round2s(y_scale, 4));
        let ih = ((((h - 1) as i64 * ys + 1023) >> 10) + 8) as usize;
        for r in 0..ih {
            let yy = ((sy >> 10) as i32 + r as i32 - 3).clamp(0, last_y) as usize;
            let src = &rp.data[yy * rp.stride..];
            for c in 0..w {
                let p = sx + xs * c as i64;
                let f = &fx[((p >> 6) & 15) as usize];
                let base = (p >> 10) as i32 - 3;
                let s: i32 = (0..8).map(|k| f[k] as i32 * src[(base + k as i32).clamp(0, last_x) as usize] as i32).sum();
                tmp[r * w + c] = round2(s, r0);
            }
        }
        for r in 0..h {
            let p = (sy & 1023) + ys * r as i64;
            let f = &fy[((p >> 6) & 15) as usize];
            let base = (p >> 10) as usize;
            for c in 0..w {
                let s: i32 = (0..8).map(|k| f[k] as i32 * tmp[(base + k) * w + c]).sum();
                out[r * w + c] = round2(s, r1);
            }
        }
    }

    /// block_warp over the 8x8 blocks of a prediction, into `pred[l]`.
    #[allow(clippy::too_many_arguments)]
    fn block_warp(&mut self, refp: &Picture, l: usize, plane: usize, x: usize, y: usize, w: usize, h: usize, p: &[i32; 6], s: &[i32; 4], r0: u32, r1: u32) {
        let (ssx, ssy) = self.sub_of(plane);
        let rp = &refp.planes[plane];
        let last_x = ((refp.width + ssx) >> ssx) as i32 - 1;
        let last_y = ((refp.height + ssy) >> ssy) as i32 - 1;
        let [alpha, beta, gamma, delta] = *s;
        let out = &mut self.pred[l];
        let mut tmp = [0i32; 15 * 8];
        for i8 in 0..h.div_ceil(8) {
            for j8 in 0..w.div_ceil(8) {
                let src_x = ((x + j8 * 8 + 4) << ssx) as i64;
                let src_y = ((y + i8 * 8 + 4) << ssy) as i64;
                let dst_x = p[2] as i64 * src_x + p[3] as i64 * src_y + p[0] as i64;
                let dst_y = p[4] as i64 * src_x + p[5] as i64 * src_y + p[1] as i64;
                let (x4, y4) = (dst_x >> ssx, dst_y >> ssy);
                let (ix4, iy4) = ((x4 >> 16) as i32, (y4 >> 16) as i32);
                let sx4 = (((x4 & 0xffff) as i32) - 4 * alpha - 4 * beta) & !63;
                let sy4 = (((y4 & 0xffff) as i32) - 4 * gamma - 4 * delta) & !63;
                for k in -7i32..8 {
                    let iy = (iy4 + k).clamp(0, last_y) as usize;
                    let src = &rp.data[iy * rp.stride..];
                    let mut sx = sx4 + beta * (k + 4);
                    for li in -4i32..4 {
                        let f = &WARPED_FILTERS[(((sx + 512) >> 10) + 64) as usize];
                        let ix = ix4 + li - 3;
                        let v: i32 = (0..8).map(|m| f[m] as i32 * src[(ix + m as i32).clamp(0, last_x) as usize] as i32).sum();
                        tmp[((k + 7) * 8 + li + 4) as usize] = round2(v, r0);
                        sx += alpha;
                    }
                }
                for k in -4i32..4.min(h as i32 - i8 as i32 * 8 - 4) {
                    let mut sy = sy4 + delta * (k + 4);
                    for li in -4i32..4.min(w as i32 - j8 as i32 * 8 - 4) {
                        let f = &WARPED_FILTERS[(((sy + 512) >> 10) + 64) as usize];
                        let v: i32 = (0..8).map(|m| f[m] as i32 * tmp[((k + m as i32 + 4) * 8 + li + 4) as usize]).sum();
                        out[(i8 * 8 + (k + 4) as usize) * w + j8 * 8 + (li + 4) as usize] = round2(v, r1);
                        sy += gamma;
                    }
                }
            }
        }
    }

    /// overlapped_motion_compensation.
    fn obmc(&mut self, plane: usize, w: usize, h: usize) {
        let (ssx, ssy) = self.sub_of(plane);
        let (row, col, bs) = (self.b.row, self.b.col, self.b.size);
        if self.b.avail_u && plane_block(bs, ssx, ssy) >= BLOCK_8X8 {
            let limit = (MI_W_LOG2[bs] as usize).min(4);
            let (mut n, mut x4) = (0, col);
            while n < limit && x4 < self.h.mi_cols.min(col + self.b.bw4) {
                let cm = *self.mi_at(row - 1, x4 | 1);
                let step = (NUM_4X4_W[cm.size as usize] as usize).clamp(2, 16);
                if cm.ref_frame[0] > 0 {
                    n += 1;
                    let (pw, ph) = (w.min((step * 4) >> ssx), (h >> 1).min(32 >> ssy));
                    self.predict_overlap(plane, &cm, x4, row, pw, ph, false);
                }
                x4 += step;
            }
        }
        if self.b.avail_l {
            let limit = (MI_H_LOG2[bs] as usize).min(4);
            let (mut n, mut y4) = (0, row);
            while n < limit && y4 < self.h.mi_rows.min(row + self.b.bh4) {
                let cm = *self.mi_at(y4 | 1, col - 1);
                let step = (NUM_4X4_H[cm.size as usize] as usize).clamp(2, 16);
                if cm.ref_frame[0] > 0 {
                    n += 1;
                    let (pw, ph) = ((w >> 1).min(32 >> ssx), h.min((step * 4) >> ssy));
                    self.predict_overlap(plane, &cm, col, y4, pw, ph, true);
                }
                y4 += step;
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn predict_overlap(&mut self, plane: usize, cm: &Mi, x4: usize, y4: usize, pw: usize, ph: usize, left: bool) {
        let (ssx, ssy) = self.sub_of(plane);
        let (px, py) = ((x4 * 4) >> ssx, (y4 * 4) >> ssy);
        let Some(refp) = self.refs[cm.ref_frame[0] as usize].clone() else { return };
        let (r0, r1) = self.rounding(false);
        self.block_inter(&refp.planes[plane], (refp.width, refp.height), 0, plane, px, py, pw, ph, cm.mv[0], cm.interp, r0, r1);
        let mask = obmc_mask(if left { pw } else { ph });
        let max = (1i32 << self.bd) - 1;
        let dst = &mut self.planes[plane];
        for i in 0..ph {
            for j in 0..pw {
                let o = self.pred[0][i * pw + j].clamp(0, max);
                let m = mask[if left { j } else { i }] as i32;
                let d = &mut dst.data[(py + i) * dst.stride + px + j];
                *d = round2(m * *d as i32 + (64 - m) * o, 6) as u16;
            }
        }
    }
}
