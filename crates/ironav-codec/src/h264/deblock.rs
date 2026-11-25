//! The in-loop deblocking filter (ITU-T H.264 8.7).
//!
//! MBAFF frames are filtered as FFmpeg's h264_loopfilter.c (LGPL-2.1-or-later) does, which this
//! follows: the left edge between a frame and a field macroblock pair has a strength per line pair
//! of the current macroblock, and when its first strength is 4 every line of the group is filtered
//! strongly while otherwise lines of strength 4 are left as they are.

use super::mb::{chroma_qp, Kind, MbInfo, Picture, SliceParams};
use super::tables::{ALPHA, BETA, TC0};
use std::sync::atomic::{fence, AtomicUsize, Ordering};
use std::sync::{Condvar, Mutex, PoisonError};

fn clip1(v: i32, bd: u32) -> u16 {
    v.clamp(0, (1 << bd) - 1) as u16
}

/// The loop filter for samples of at most 10 bits, eight sample lines at a time in 16-bit lanes
/// (every sum in the filter fits 16 bits with up to 10-bit samples). SSE2 is part of x86_64, so
/// this needs no run-time check. It computes what `filter_line` does, line for line.
#[cfg(target_arch = "x86_64")]
mod simd {
    use super::TC0;
    use std::arch::x86_64::*;

    /// 8 samples of `b` from `o`; panics when they are not all there.
    #[target_feature(enable = "sse2")]
    #[inline]
    fn ld(b: &[u16], o: usize) -> __m128i {
        let a: &[u16; 8] = b[o..o + 8].try_into().unwrap();
        // SAFETY: `a` is 8 readable samples, and an unaligned load needs nothing more
        unsafe { _mm_loadu_si128(a.as_ptr().cast()) }
    }

    #[target_feature(enable = "sse2")]
    #[inline]
    fn st(b: &mut [u16], o: usize, v: __m128i) {
        let a: &mut [u16; 8] = (&mut b[o..o + 8]).try_into().unwrap();
        // SAFETY: `a` is 8 writable samples, and an unaligned store needs nothing more
        unsafe { _mm_storeu_si128(a.as_mut_ptr().cast(), v) }
    }

    #[target_feature(enable = "sse2")]
    #[inline]
    fn abs(x: __m128i) -> __m128i {
        _mm_max_epi16(x, _mm_sub_epi16(_mm_setzero_si128(), x))
    }

    /// `m` is all ones or all zeros per lane: `a` where it is set, else `b`.
    #[target_feature(enable = "sse2")]
    #[inline]
    fn sel(m: __m128i, a: __m128i, b: __m128i) -> __m128i {
        _mm_or_si128(_mm_and_si128(m, a), _mm_andnot_si128(m, b))
    }

    #[target_feature(enable = "sse2")]
    #[inline]
    fn sum(v: &[__m128i]) -> __m128i {
        v.iter().fold(_mm_setzero_si128(), |a, &b| _mm_add_epi16(a, b))
    }

    #[target_feature(enable = "sse2")]
    #[inline]
    fn sh1(v: __m128i) -> __m128i {
        _mm_slli_epi16(v, 1)
    }

    /// Transposes 8 rows of 8 samples.
    #[target_feature(enable = "sse2")]
    #[inline]
    fn transpose(r: [__m128i; 8]) -> [__m128i; 8] {
        let (t0, t1, t2, t3) = (_mm_unpacklo_epi16(r[0], r[1]), _mm_unpackhi_epi16(r[0], r[1]), _mm_unpacklo_epi16(r[2], r[3]), _mm_unpackhi_epi16(r[2], r[3]));
        let (t4, t5, t6, t7) = (_mm_unpacklo_epi16(r[4], r[5]), _mm_unpackhi_epi16(r[4], r[5]), _mm_unpacklo_epi16(r[6], r[7]), _mm_unpackhi_epi16(r[6], r[7]));
        let (u0, u1, u2, u3) = (_mm_unpacklo_epi32(t0, t2), _mm_unpackhi_epi32(t0, t2), _mm_unpacklo_epi32(t1, t3), _mm_unpackhi_epi32(t1, t3));
        let (u4, u5, u6, u7) = (_mm_unpacklo_epi32(t4, t6), _mm_unpackhi_epi32(t4, t6), _mm_unpacklo_epi32(t5, t7), _mm_unpackhi_epi32(t5, t7));
        [_mm_unpacklo_epi64(u0, u4), _mm_unpackhi_epi64(u0, u4), _mm_unpacklo_epi64(u1, u5), _mm_unpackhi_epi64(u1, u5), _mm_unpacklo_epi64(u2, u6), _mm_unpackhi_epi64(u2, u6), _mm_unpacklo_epi64(u3, u7), _mm_unpackhi_epi64(u3, u7)]
    }

    /// Per lane: the thresholds and strengths one group of eight lines is filtered with.
    struct Lanes {
        bs: __m128i,
        tc0: __m128i,
        alpha: __m128i,
        beta: __m128i,
        maxv: __m128i,
    }

    impl Lanes {
        /// The lanes where the line is filtered at all (8.7.2.2).
        #[target_feature(enable = "sse2")]
    #[inline]
        fn active(&self, p1: __m128i, p0: __m128i, q0: __m128i, q1: __m128i) -> __m128i {
            let lt = _mm_cmplt_epi16;
            let m = _mm_and_si128(_mm_cmpgt_epi16(self.bs, _mm_setzero_si128()), lt(abs(_mm_sub_epi16(p0, q0)), self.alpha));
            _mm_and_si128(m, _mm_and_si128(lt(abs(_mm_sub_epi16(p1, p0)), self.beta), lt(abs(_mm_sub_epi16(q1, q0)), self.beta)))
        }

        #[target_feature(enable = "sse2")]
    #[inline]
        fn clip(&self, v: __m128i) -> __m128i {
            _mm_min_epi16(_mm_max_epi16(v, _mm_setzero_si128()), self.maxv)
        }

        /// The new p0 and q0 of a line with strength below 4 and clipping threshold `tc`.
        #[target_feature(enable = "sse2")]
    #[inline]
        fn weak(&self, tc: __m128i, p1: __m128i, p0: __m128i, q0: __m128i, q1: __m128i) -> (__m128i, __m128i) {
            let d = _mm_srai_epi16(_mm_add_epi16(_mm_add_epi16(_mm_slli_epi16(_mm_sub_epi16(q0, p0), 2), _mm_sub_epi16(p1, q1)), _mm_set1_epi16(4)), 3);
            let d = _mm_min_epi16(_mm_max_epi16(d, _mm_sub_epi16(_mm_setzero_si128(), tc)), tc);
            (self.clip(_mm_add_epi16(p0, d)), self.clip(_mm_sub_epi16(q0, d)))
        }

        /// Luma lines `[p3, p2, p1, p0, q0, q1, q2, q3]`, filtered.
        #[target_feature(enable = "sse2")]
    #[inline]
        fn luma(&self, v: [__m128i; 8]) -> [__m128i; 8] {
            let [p3, p2, p1, p0, q0, q1, q2, q3] = v;
            let mf = self.active(p1, p0, q0, q1);
            if _mm_movemask_epi8(mf) == 0 {
                return v;
            }
            let lt = _mm_cmplt_epi16;
            let ap = lt(abs(_mm_sub_epi16(p2, p0)), self.beta);
            let aq = lt(abs(_mm_sub_epi16(q2, q0)), self.beta);
            let strong = _mm_cmpgt_epi16(self.bs, _mm_set1_epi16(3));
            // bS < 4: tc is tc0 plus one for each side that is smooth (a set mask is -1)
            let tc = _mm_sub_epi16(_mm_sub_epi16(self.tc0, ap), aq);
            let (p0w, q0w) = self.weak(tc, p1, p0, q0, q1);
            let avg = _mm_avg_epu16(p0, q0);
            let ntc0 = _mm_sub_epi16(_mm_setzero_si128(), self.tc0);
            let side = |s2: __m128i, s1: __m128i| _mm_add_epi16(s1, _mm_min_epi16(_mm_max_epi16(_mm_srai_epi16(_mm_sub_epi16(_mm_add_epi16(s2, avg), sh1(s1)), 1), ntc0), self.tc0));
            let p1w = sel(ap, side(p2, p1), p1);
            let q1w = sel(aq, side(q2, q1), q1);
            // bS == 4
            let small = lt(abs(_mm_sub_epi16(p0, q0)), _mm_add_epi16(_mm_srai_epi16(self.alpha, 2), _mm_set1_epi16(2)));
            let (sp, sq) = (_mm_and_si128(ap, small), _mm_and_si128(aq, small));
            let (c2, c4) = (_mm_set1_epi16(2), _mm_set1_epi16(4));
            let p0a = _mm_srai_epi16(sum(&[p2, sh1(p1), sh1(p0), sh1(q0), q1, c4]), 3);
            let p0b = _mm_srai_epi16(sum(&[sh1(p1), p0, q1, c2]), 2);
            let p0s = sel(sp, p0a, p0b);
            let p1s = sel(sp, _mm_srai_epi16(sum(&[p2, p1, p0, q0, c2]), 2), p1);
            let p2s = sel(sp, _mm_srai_epi16(sum(&[sh1(p3), sh1(p2), p2, p1, p0, q0, c4]), 3), p2);
            let q0a = _mm_srai_epi16(sum(&[p1, sh1(p0), sh1(q0), sh1(q1), q2, c4]), 3);
            let q0b = _mm_srai_epi16(sum(&[sh1(q1), q0, p1, c2]), 2);
            let q0s = sel(sq, q0a, q0b);
            let q1s = sel(sq, _mm_srai_epi16(sum(&[p0, q0, q1, q2, c2]), 2), q1);
            let q2s = sel(sq, _mm_srai_epi16(sum(&[sh1(q3), sh1(q2), q2, q1, q0, p0, c4]), 3), q2);
            let pick = |w: __m128i, s: __m128i, orig: __m128i| sel(mf, sel(strong, s, w), orig);
            [p3, pick(p2, p2s, p2), pick(p1w, p1s, p1), pick(p0w, p0s, p0), pick(q0w, q0s, q0), pick(q1w, q1s, q1), pick(q2, q2s, q2), q3]
        }

        /// Chroma lines `[p1, p0, q0, q1]`, filtered.
        #[target_feature(enable = "sse2")]
    #[inline]
        fn chroma(&self, v: [__m128i; 4]) -> [__m128i; 4] {
            let [p1, p0, q0, q1] = v;
            let mf = self.active(p1, p0, q0, q1);
            if _mm_movemask_epi8(mf) == 0 {
                return v;
            }
            let strong = _mm_cmpgt_epi16(self.bs, _mm_set1_epi16(3));
            let (p0w, q0w) = self.weak(_mm_add_epi16(self.tc0, _mm_set1_epi16(1)), p1, p0, q0, q1);
            let c2 = _mm_set1_epi16(2);
            let p0s = _mm_srai_epi16(sum(&[sh1(p1), p0, q1, c2]), 2);
            let q0s = _mm_srai_epi16(sum(&[sh1(q1), q0, p1, c2]), 2);
            [p1, sel(mf, sel(strong, p0s, p0w), p0), sel(mf, sel(strong, q0s, q0w), q0), q1]
        }
    }

    /// `filter_edge` for a plane held as a slice, without I_PCM or lossless samples to put back and
    /// with at most 10-bit samples.
    #[target_feature(enable = "sse2")]
    #[allow(clippy::too_many_arguments)]
    pub fn filter_edge(buf: &mut [u16], pos: usize, xs: usize, ys: usize, per_bs: usize, bs: &[u8; 4], alpha: i32, beta: i32, index_a: usize, chroma: bool, bd: u32) {
        for g in 0..per_bs / 2 {
            // the strength and clipping threshold of each of this group's eight lines
            let (mut bsl, mut tcl) = ([0i16; 8], [0i16; 8]);
            for l in 0..8 {
                let b = bs[(g * 8 + l) / per_bs];
                bsl[l] = b as i16;
                if (1..4).contains(&b) {
                    tcl[l] = ((TC0[index_a][b as usize].max(0) as i32) << (bd - 8)) as i16;
                }
            }
            if bsl.iter().all(|&b| b == 0) {
                continue;
            }
            let lanes = Lanes { bs: ld16(&bsl), tc0: ld16(&tcl), alpha: _mm_set1_epi16(alpha as i16), beta: _mm_set1_epi16(beta as i16), maxv: _mm_set1_epi16(((1u32 << bd) - 1) as i16) };
            if xs == 1 {
                // a vertical edge: the lines are rows, eight samples across the edge each
                let at = |r: usize| pos + (g * 8 + r) * ys - 4;
                let rows: [__m128i; 8] = std::array::from_fn(|r| ld(buf, at(r)));
                let mut c = transpose(rows);
                if chroma {
                    let o = lanes.chroma([c[2], c[3], c[4], c[5]]);
                    c[3] = o[1];
                    c[4] = o[2];
                } else {
                    c = lanes.luma(c);
                }
                for (r, v) in transpose(c).into_iter().enumerate() {
                    st(buf, at(r), v);
                }
            } else {
                // a horizontal edge: the lines are columns, one row of eight per sample position
                let at = |k: usize| pos + k * xs + g * 8 - 4 * xs;
                if chroma {
                    let o = lanes.chroma([ld(buf, at(2)), ld(buf, at(3)), ld(buf, at(4)), ld(buf, at(5))]);
                    st(buf, at(3), o[1]);
                    st(buf, at(4), o[2]);
                } else {
                    let o = lanes.luma(std::array::from_fn(|k| ld(buf, at(k))));
                    for (k, v) in o.into_iter().enumerate().take(7).skip(1) {
                        st(buf, at(k), v);
                    }
                }
            }
        }
    }

    #[target_feature(enable = "sse2")]
    #[inline]
    fn ld16(a: &[i16; 8]) -> __m128i {
        // SAFETY: `a` is 8 readable lanes, and an unaligned load needs nothing more
        unsafe { _mm_loadu_si128(a.as_ptr().cast()) }
    }
}

/// Filters the sample lines across one edge: four segments of `per_bs` lines, each with its own
/// boundary strength in `bs`. `pos` is the first q0 sample, `xs` the step across the edge and `ys`
/// the step along it.
#[allow(clippy::too_many_arguments)]
fn filter_edge<P: Px + ?Sized>(pl: &mut P, pos: usize, xs: usize, ys: usize, per_bs: usize, bs: &[u8; 4], alpha: i32, beta: i32, index_a: usize, chroma: bool, bd: u32, keep: (bool, bool)) {
    if alpha == 0 || beta == 0 {
        return;
    }
    #[cfg(target_arch = "x86_64")]
    if !(keep.0 || keep.1) && bd <= 10 {
        if let Some(buf) = pl.slice_mut() {
            // SAFETY: SSE2 is part of the x86_64 baseline
            unsafe { simd::filter_edge(buf, pos, xs, ys, per_bs, bs, alpha, beta, index_a, chroma, bd) };
            return;
        }
    }
    let n = if chroma { 1 } else { 3 };
    // I_PCM and lossless samples are put back after filtering; only then are they read out
    let restore = keep.0 || keep.1;
    for (s, &b) in bs.iter().enumerate() {
        if b == 0 {
            continue;
        }
        let tc0 = if b < 4 { (TC0[index_a][b as usize].max(0) as i32) << (bd - 8) } else { 0 };
        for k in s * per_bs..(s + 1) * per_bs {
            let q = pos + k * ys;
            let (mut sp, mut sq) = ([0u16; 3], [0u16; 3]);
            if restore {
                for i in 0..n {
                    sp[i] = pl.px(q - (i + 1) * xs);
                    sq[i] = pl.px(q + i * xs);
                }
            }
            filter_line(pl, q, xs, b, tc0, alpha, beta, chroma, bd);
            if restore {
                for i in 0..n {
                    if keep.0 {
                        pl.set(q - (i + 1) * xs, sp[i]);
                    }
                    if keep.1 {
                        pl.set(q + i * xs, sq[i]);
                    }
                }
            }
        }
    }
}

/// Filters the sample line through `q`, the first sample past the edge, with boundary strength `b`;
/// `tc0` is the clipping threshold for `b` below 4.
#[allow(clippy::too_many_arguments)]
fn filter_line<P: Px + ?Sized>(pl: &mut P, q: usize, xs: usize, b: u8, tc0: i32, alpha: i32, beta: i32, chroma: bool, bd: u32) {
    let p0 = pl.px(q - xs) as i32;
    let q0 = pl.px(q) as i32;
    let p1 = pl.px(q - 2 * xs) as i32;
    let q1 = pl.px(q + xs) as i32;
    if (p0 - q0).abs() >= alpha || (p1 - p0).abs() >= beta || (q1 - q0).abs() >= beta {
        return;
    }
    if chroma {
        if b < 4 {
            let tc = tc0 + 1;
            let delta = ((((q0 - p0) << 2) + (p1 - q1) + 4) >> 3).clamp(-tc, tc);
            pl.set(q - xs, clip1(p0 + delta, bd));
            pl.set(q, clip1(q0 - delta, bd));
        } else {
            pl.set(q - xs, ((2 * p1 + p0 + q1 + 2) >> 2) as u16);
            pl.set(q, ((2 * q1 + q0 + p1 + 2) >> 2) as u16);
        }
        return;
    }
    let p2 = pl.px(q - 3 * xs) as i32;
    let q2 = pl.px(q + 2 * xs) as i32;
    let ap = (p2 - p0).abs();
    let aq = (q2 - q0).abs();
    if b < 4 {
        let tc = tc0 + (ap < beta) as i32 + (aq < beta) as i32;
        let delta = ((((q0 - p0) << 2) + (p1 - q1) + 4) >> 3).clamp(-tc, tc);
        pl.set(q - xs, clip1(p0 + delta, bd));
        pl.set(q, clip1(q0 - delta, bd));
        if ap < beta {
            pl.set(q - 2 * xs, (p1 + ((p2 + ((p0 + q0 + 1) >> 1) - (p1 << 1)) >> 1).clamp(-tc0, tc0)) as u16);
        }
        if aq < beta {
            pl.set(q + xs, (q1 + ((q2 + ((p0 + q0 + 1) >> 1) - (q1 << 1)) >> 1).clamp(-tc0, tc0)) as u16);
        }
    } else {
        let small = (p0 - q0).abs() < (alpha >> 2) + 2;
        if ap < beta && small {
            let p3 = pl.px(q - 4 * xs) as i32;
            pl.set(q - xs, ((p2 + 2 * p1 + 2 * p0 + 2 * q0 + q1 + 4) >> 3) as u16);
            pl.set(q - 2 * xs, ((p2 + p1 + p0 + q0 + 2) >> 2) as u16);
            pl.set(q - 3 * xs, ((2 * p3 + 3 * p2 + p1 + p0 + q0 + 4) >> 3) as u16);
        } else {
            pl.set(q - xs, ((2 * p1 + p0 + q1 + 2) >> 2) as u16);
        }
        if aq < beta && small {
            let q3 = pl.px(q + 3 * xs) as i32;
            pl.set(q, ((p1 + 2 * p0 + 2 * q0 + 2 * q1 + q2 + 4) >> 3) as u16);
            pl.set(q + xs, ((p0 + q0 + q1 + q2 + 2) >> 2) as u16);
            pl.set(q + 2 * xs, ((2 * q3 + 3 * q2 + q1 + q0 + p0 + 4) >> 3) as u16);
        } else {
            pl.set(q, ((2 * q1 + q0 + p1 + 2) >> 2) as u16);
        }
    }
}

/// Whether the transform block holding 4x4 block (bx, by) has non-zero coefficients.
fn coded(m: &MbInfo, bx: usize, by: usize) -> bool {
    if m.t8x8 {
        let (x0, y0) = (bx & 2, by & 2);
        (0..2).any(|dy| (0..2).any(|dx| m.nnz[(y0 + dy) * 4 + x0 + dx] > 0))
    } else {
        m.nnz[by * 4 + bx] > 0
    }
}

/// Vectors a whole sample apart; `mvy` is the vertical limit in quarter samples (2 between field
/// macroblocks, whose rows are frame rows apart).
fn far(a: [i16; 2], b: [i16; 2], mvy: i32) -> bool {
    (a[0] as i32 - b[0] as i32).abs() >= 4 || (a[1] as i32 - b[1] as i32).abs() >= mvy
}

/// Different reference pictures, a different number of vectors or vectors a whole sample apart.
fn motion_differs(p: &MbInfo, pb: (usize, usize), q: &MbInfo, qb: (usize, usize), mvy: i32) -> bool {
    let far = |a, b| far(a, b, mvy);
    let (pi, qi) = (pb.1 * 4 + pb.0, qb.1 * 4 + qb.0);
    let (p8, q8) = ((pb.1 >> 1) * 2 + (pb.0 >> 1), (qb.1 >> 1) * 2 + (qb.0 >> 1));
    let pr = [p.ref_id[0][p8], p.ref_id[1][p8]];
    let qr = [q.ref_id[0][q8], q.ref_id[1][q8]];
    let pm = [p.mv[0][pi], p.mv[1][pi]];
    let qm = [q.mv[0][qi], q.mv[1][qi]];
    let np = pr.iter().filter(|&&r| r >= 0).count();
    let nq = qr.iter().filter(|&&r| r >= 0).count();
    if np != nq {
        return true;
    }
    if np == 1 {
        let (lp, lq) = (if pr[0] >= 0 { 0 } else { 1 }, if qr[0] >= 0 { 0 } else { 1 });
        return pr[lp] != qr[lq] || far(pm[lp], qm[lq]);
    }
    if !((pr[0] == qr[0] && pr[1] == qr[1]) || (pr[0] == qr[1] && pr[1] == qr[0])) {
        return true;
    }
    if pr[0] != pr[1] {
        if pr[0] == qr[0] {
            far(pm[0], qm[0]) || far(pm[1], qm[1])
        } else {
            far(pm[0], qm[1]) || far(pm[1], qm[0])
        }
    } else {
        (far(pm[0], qm[0]) || far(pm[1], qm[1])) && (far(pm[0], qm[1]) || far(pm[1], qm[0]))
    }
}

/// Whether every 4x4 block of the macroblock has the same vectors and every 8x8 block the same
/// references.
fn uniform_motion(m: &MbInfo) -> bool {
    (0..2).all(|l| m.ref_idx[l].iter().all(|&r| r == m.ref_idx[l][0]) && m.ref_id[l].iter().all(|&r| r == m.ref_id[l][0]) && m.mv[l].iter().all(|&v| v == m.mv[l][0]))
}

/// Boundary strength of each 4-sample segment along edge `e` of the macroblock `m`; `other` is the
/// macroblock on the far side of a macroblock edge (and `m` itself for inner edges).
fn strength(m: &MbInfo, other: &MbInfo, dir: usize, e: usize, mvy: i32) -> [u8; 4] {
    let mb_edge = e == 0;
    if m.kind != Kind::Inter || other.kind != Kind::Inter {
        return [if mb_edge { 4 } else { 3 }; 4];
    }
    let mut out = [0u8; 4];
    for (i, o) in out.iter_mut().enumerate() {
        let (q, p) = if dir == 0 { ((e, i), if mb_edge { (3, i) } else { (e - 1, i) }) } else { ((i, e), if mb_edge { (i, 3) } else { (i, e - 1) }) };
        *o = if coded(m, q.0, q.1) || coded(other, p.0, p.1) {
            2
        } else if motion_differs(other, p, m, q, mvy) {
            1
        } else {
            0
        };
    }
    out
}

pub fn deblock_picture(pic: &mut Picture, bd: u32) {
    deblock_rows(pic, bd, 0..pic.mb_h);
}

/// Filters the macroblock rows `rows`; in raster order with the rows above, this filters the picture.
/// Filtering a row changes up to three luma rows at the bottom of the row above.
pub fn deblock_rows(pic: &mut Picture, bd: u32, rows: std::ops::Range<usize>) {
    if pic.mbaff {
        deblock_mbaff(pic, bd, rows.start / 2..rows.end.div_ceil(2));
        return;
    }
    let Picture { mb_w, cf, ch, planes, mbs, slices, .. } = pic;
    for mby in rows {
        for mbx in 0..*mb_w {
            deblock_mb(planes, mbs, slices, (*mb_w, *cf, *ch), bd, mbx, mby);
        }
    }
}

/// A sample plane the loop filter reads and writes by index. Besides slices, the planes that
/// `deblock_picture_par` shares among threads are one (see `SharedPlane`).
trait Px {
    fn px(&self, i: usize) -> u16;
    fn set(&mut self, i: usize, v: u16);
    /// The plane as a slice, when it is one (it is not when several threads share it).
    fn slice_mut(&mut self) -> Option<&mut [u16]> {
        None
    }
}

impl Px for [u16] {
    fn slice_mut(&mut self) -> Option<&mut [u16]> {
        Some(self)
    }

    #[inline]
    fn px(&self, i: usize) -> u16 {
        self[i]
    }

    #[inline]
    fn set(&mut self, i: usize, v: u16) {
        self[i] = v;
    }
}

impl Px for Vec<u16> {
    fn slice_mut(&mut self) -> Option<&mut [u16]> {
        Some(self)
    }

    #[inline]
    fn px(&self, i: usize) -> u16 {
        self[i]
    }

    #[inline]
    fn set(&mut self, i: usize, v: u16) {
        self[i] = v;
    }
}

/// A plane that several loop-filter threads read and write at once, through a raw pointer: no thread
/// ever holds a reference to the plane (or to any part of it), only this pointer, and each access is
/// of one sample.
///
/// Macroblock rows are filtered as a wave: macroblock (x, r) starts only once row r - 1 has finished
/// macroblock x + 1. Filtering a macroblock reads and writes only samples of itself, of the macroblock
/// to its left (up to three columns), and of the one above it (up to three rows, with a fourth read);
/// the macroblock above and to the right changes the above one's right-hand columns through its own
/// left edge, which is why it must be done first. So macroblocks filtered at the same time (those of
/// different rows, at least two columns apart) never touch the same sample, and the progress
/// counters, set with Release after a macroblock and read with Acquire before the one that follows
/// it, make every write to a sample happen before any later read of it by another thread.
#[derive(Clone, Copy)]
struct SharedPlane<'a> {
    ptr: *mut u16,
    len: usize,
    _plane: std::marker::PhantomData<&'a mut [u16]>,
}

// SAFETY: concurrent use is limited to the disjoint macroblocks described above; the pointer is into
// a plane that outlives `'a` and is not otherwise used meanwhile (it is borrowed mutably for `'a`).
unsafe impl Send for SharedPlane<'_> {}
unsafe impl Sync for SharedPlane<'_> {}

impl<'a> SharedPlane<'a> {
    fn new(plane: &'a mut [u16]) -> Self {
        SharedPlane { ptr: plane.as_mut_ptr(), len: plane.len(), _plane: std::marker::PhantomData }
    }
}

impl Px for SharedPlane<'_> {
    #[inline]
    fn px(&self, i: usize) -> u16 {
        assert!(i < self.len);
        // SAFETY: in bounds, and no other thread is writing this sample (see above).
        unsafe { *self.ptr.add(i) }
    }

    #[inline]
    fn set(&mut self, i: usize, v: u16) {
        assert!(i < self.len);
        // SAFETY: in bounds, and no other thread is using this sample (see above).
        unsafe { *self.ptr.add(i) = v }
    }
}

/// The progress of each macroblock row of a wave (see `SharedPlane`), with a blocking wait for it.
///
/// A row that has to wait for the one above spins briefly, then sleeps on a condition variable. It does
/// not spin or yield for long, because on a machine busy with other work the thread filtering the row
/// above may be descheduled for many milliseconds, and the waiters spinning would take the very cores
/// it needs to come back.
struct Wave {
    /// Per row: macroblocks finished, or `usize::MAX` once the row is over (also by a panic).
    done: Vec<AtomicUsize>,
    /// Threads asleep or about to sleep in `wait`.
    waiters: AtomicUsize,
    lock: Mutex<()>,
    wake: Condvar,
}

impl Wave {
    fn new(rows: usize) -> Self {
        Wave { done: (0..rows).map(|_| AtomicUsize::new(0)).collect(), waiters: AtomicUsize::new(0), lock: Mutex::new(()), wake: Condvar::new() }
    }

    /// Records that row `row` has finished `n` macroblocks, and wakes the sleepers when there are any.
    fn set(&self, row: usize, n: usize) {
        self.done[row].store(n, Ordering::Release);
        // pairs with the fence in `wait`: either the sleeper-to-be sees the new count, or we see it
        fence(Ordering::SeqCst);
        if self.waiters.load(Ordering::Relaxed) > 0 {
            // taking the lock makes a waiter that has checked the count but not yet gone to sleep
            // reach `Condvar::wait` (which releases the lock) before we notify
            drop(self.lock.lock().unwrap_or_else(PoisonError::into_inner));
            self.wake.notify_all();
        }
    }

    /// Returns once row `row` has finished at least `need` macroblocks.
    fn wait(&self, row: usize, need: usize) {
        let ready = || self.done[row].load(Ordering::Acquire) >= need;
        for _ in 0..200 {
            if ready() {
                return;
            }
            std::hint::spin_loop();
        }
        self.waiters.fetch_add(1, Ordering::SeqCst);
        fence(Ordering::SeqCst);
        let mut guard = self.lock.lock().unwrap_or_else(PoisonError::into_inner);
        while !ready() {
            guard = self.wake.wait(guard).unwrap_or_else(PoisonError::into_inner);
        }
        drop(guard);
        self.waiters.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Marks a macroblock row finished when dropped, also when filtering it panicked, so that the rows
/// waiting on it do not wait for ever.
struct RowDone<'a>(&'a Wave, usize);

impl Drop for RowDone<'_> {
    fn drop(&mut self) {
        self.0.set(self.1, usize::MAX);
    }
}

/// `deblock_picture` for live coding: each macroblock row is a task, filtered in a staggered wave
/// (see `SharedPlane`), to the very samples the raster order gives.
pub fn deblock_picture_par(pic: &mut Picture, bd: u32) {
    if pic.mbaff || pic.mb_h < 2 {
        return deblock_picture(pic, bd);
    }
    let Picture { mb_w, mb_h, cf, ch, planes, mbs, slices, .. } = pic;
    let (w, h, dims) = (*mb_w, *mb_h, (*mb_w, *cf, *ch));
    let [a, b, c] = planes;
    let shared = [SharedPlane::new(a), SharedPlane::new(b), SharedPlane::new(c)];
    let (mbs, slices) = (&*mbs, &*slices);
    let wave = Wave::new(h);
    crate::pool::run(h, |mby| {
        let _row = RowDone(&wave, mby);
        let mut planes = shared;
        for mbx in 0..w {
            if mby > 0 {
                wave.wait(mby - 1, (mbx + 2).min(w));
            }
            deblock_mb(&mut planes, mbs, slices, dims, bd, mbx, mby);
            wave.set(mby, mbx + 1);
        }
    });
}

/// Filters macroblock (`mbx`, `mby`) of a picture that is not MBAFF; `dims` is its width in
/// macroblocks, chroma_format_idc and the height of a macroblock's chroma.
fn deblock_mb<P: Px>(planes: &mut [P; 3], mbs: &[MbInfo], slices: &[SliceParams], (mb_w, cf, chh): (usize, u8, usize), bd: u32, mbx: usize, mby: usize) {
    let w = mb_w;
    let ls = w * 16;
    let cs = w * 8;
    let idx = mby * w + mbx;
    let m = &mbs[idx];
    if m.slice == 0 {
        return;
    }
    let sp = slices[m.slice as usize - 1];
    if sp.disable_deblocking == 1 {
        return;
    }
    let usable = |o: &MbInfo| o.slice != 0 && (sp.disable_deblocking == 0 || o.slice == m.slice);
    let left_ok = mbx > 0 && usable(&mbs[idx - 1]);
    let top_ok = mby > 0 && usable(&mbs[idx - w]);
    // the macroblocks on the far side of the left and top edges (`m` itself at the picture edge)
    let edge_mb = [if mbx > 0 { &mbs[idx - 1] } else { m }, if mby > 0 { &mbs[idx - w] } else { m }];
    // boundary strengths depend on the macroblocks only, so all planes and chroma share them
    let mut bs = [[[0u8; 4]; 4]; 2];
    // inner edges of an inter macroblock that moves as a whole and has no coefficients are not filtered
    let still = m.kind == Kind::Inter && uniform_motion(m) && m.nnz[..16].iter().all(|&n| n == 0);
    for dir in 0..2 {
        for e in 0..4usize {
            if e > 0 && still {
                continue;
            }
            let other = if e == 0 { edge_mb[dir] } else { m };
            bs[dir][e] = strength(m, other, dir, e, 4);
        }
    }
    // luma, and Cb and Cr filtered the same way in 4:4:4
    for pl in 0..if cf == 3 { 3 } else { 1 } {
        let off = if pl > 0 { sp.chroma_qp_offset[pl - 1] } else { 0 };
        for dir in 0..2 {
            for e in 0..4usize {
                if e == 0 && !(if dir == 0 { left_ok } else { top_ok }) {
                    continue;
                }
                if (m.t8x8 && e & 1 == 1) || bs[dir][e] == [0; 4] {
                    continue;
                }
                let other = if e == 0 { edge_mb[dir] } else { m };
                let qpav = if pl == 0 { (other.qp as i32 + m.qp as i32 + 1) >> 1 } else { (chroma_qp(other.qp as i32, off, bd) + chroma_qp(m.qp as i32, off, bd) + 1) >> 1 };
                let ia = (qpav + sp.alpha_offset).clamp(0, 51) as usize;
                let ib = (qpav + sp.beta_offset).clamp(0, 51) as usize;
                let (alpha, beta) = ((ALPHA[ia] as i32) << (bd - 8), (BETA[ib] as i32) << (bd - 8));
                let (x0, y0) = (mbx * 16, mby * 16);
                if dir == 0 {
                    filter_edge(&mut planes[pl], y0 * ls + x0 + e * 4, 1, ls, 4, &bs[dir][e], alpha, beta, ia, false, bd, (other.keep, m.keep));
                } else {
                    filter_edge(&mut planes[pl], (y0 + e * 4) * ls + x0, ls, 1, 4, &bs[dir][e], alpha, beta, ia, false, bd, (other.keep, m.keep));
                }
            }
        }
    }
    // chroma: vertical edges at 0 and 4, horizontal edges every 4 rows (every 4 chroma rows of
    // 4:2:2, every 8 luma rows of 4:2:0); each takes the strength of the luma edge it lines up with
    if matches!(cf, 0 | 3) {
        return;
    }
    for comp in 0..2usize {
        let off = sp.chroma_qp_offset[comp];
        for dir in 0..2 {
            let edges: &[usize] = if dir == 1 && chh == 16 { &[0, 1, 2, 3] } else { &[0, 2] };
            for &e in edges {
                if (e == 0 && !(if dir == 0 { left_ok } else { top_ok })) || bs[dir][e] == [0; 4] {
                    continue;
                }
                let other = if e == 0 { edge_mb[dir] } else { m };
                let qpav = (chroma_qp(other.qp as i32, off, bd) + chroma_qp(m.qp as i32, off, bd) + 1) >> 1;
                let ia = (qpav + sp.alpha_offset).clamp(0, 51) as usize;
                let ib = (qpav + sp.beta_offset).clamp(0, 51) as usize;
                let (alpha, beta) = ((ALPHA[ia] as i32) << (bd - 8), (BETA[ib] as i32) << (bd - 8));
                let (x0, y0) = (mbx * 8, mby * chh);
                if dir == 0 {
                    filter_edge(&mut planes[1 + comp], y0 * cs + x0 + e * 2, 1, cs, chh / 4, &bs[dir][e], alpha, beta, ia, true, bd, (other.keep, m.keep));
                } else if chh == 16 {
                    filter_edge(&mut planes[1 + comp], (y0 + e * 4) * cs + x0, cs, 1, 2, &bs[dir][e], alpha, beta, ia, true, bd, (other.keep, m.keep));
                } else {
                    filter_edge(&mut planes[1 + comp], (y0 + e * 2) * cs + x0, cs, 1, 2, &bs[dir][e], alpha, beta, ia, true, bd, (other.keep, m.keep));
                }
            }
        }
    }
}

/// Filters one sample line across an edge, putting back I_PCM and lossless samples after.
#[allow(clippy::too_many_arguments)]
fn filter_one<P: Px + ?Sized>(pl: &mut P, q: usize, xs: usize, b: u8, index_a: usize, alpha: i32, beta: i32, chroma: bool, bd: u32, keep: (bool, bool)) {
    if b == 0 || alpha == 0 || beta == 0 {
        return;
    }
    let tc0 = if b < 4 { (TC0[index_a][b as usize].max(0) as i32) << (bd - 8) } else { 0 };
    let n = if chroma { 1 } else { 3 };
    let (mut sp, mut sq) = ([0u16; 3], [0u16; 3]);
    for i in 0..n {
        sp[i] = pl.px(q - (i + 1) * xs);
        sq[i] = pl.px(q + i * xs);
    }
    filter_line(pl, q, xs, b, tc0, alpha, beta, chroma, bd);
    for i in 0..n {
        if keep.0 {
            pl.set(q - (i + 1) * xs, sp[i]);
        }
        if keep.1 {
            pl.set(q + i * xs, sq[i]);
        }
    }
}

/// alpha, beta and indexA for the average QP `qpav`.
fn thresholds(qpav: i32, sp: &super::mb::SliceParams, bd: u32) -> (i32, i32, usize) {
    let ia = (qpav + sp.alpha_offset).clamp(0, 51) as usize;
    let ib = (qpav + sp.beta_offset).clamp(0, 51) as usize;
    ((ALPHA[ia] as i32) << (bd - 8), (BETA[ib] as i32) << (bd - 8), ia)
}

/// Filters the macroblock pair rows `pairs` of an MBAFF frame: pair by pair, the top macroblock
/// and then the bottom one.
fn deblock_mbaff(pic: &mut Picture, bd: u32, pairs: std::ops::Range<usize>) {
    let w = pic.mb_w;
    let (cw, chh) = (pic.cw, pic.ch);
    let (ls, cs) = (w * 16, w * cw);
    let ncomp = if pic.cf == 3 { 3 } else { 1 };
    let chroma = matches!(pic.cf, 1 | 2);
    let Picture { planes, mbs, slices, .. } = pic;
    for py in pairs {
        for mbx in 0..w {
            for bot in 0..2usize {
                let mby = 2 * py + bot;
                let idx = mby * w + mbx;
                let m = &mbs[idx];
                if m.slice == 0 {
                    continue;
                }
                let sp = slices[m.slice as usize - 1];
                if sp.disable_deblocking == 1 {
                    continue;
                }
                let usable = |o: &MbInfo| o.slice != 0 && (sp.disable_deblocking == 0 || o.slice == m.slice);
                let fld = m.field;
                let mvy = if fld { 2 } else { 4 };
                // the macroblock's first sample and the distance between its rows, luma and chroma
                let (lo, lrs, co, crs) = if fld {
                    (2 * py * 16 * ls + bot * ls + mbx * 16, 2 * ls, 2 * py * chh * cs + bot * cs + mbx * cw, 2 * cs)
                } else {
                    (mby * 16 * ls + mbx * 16, ls, mby * chh * cs + mbx * cw, cs)
                };
                let intra = |o: &MbInfo| o.kind != Kind::Inter;
                let cqp = |q: i8, c: usize| chroma_qp(q as i32, sp.chroma_qp_offset[c], bd);
                // QP average with `o` for luma-style component `pl`
                let qpav = |o: &MbInfo, pl: usize| if pl == 0 { (o.qp as i32 + m.qp as i32 + 1) >> 1 } else { (cqp(o.qp, pl - 1) + cqp(m.qp, pl - 1) + 1) >> 1 };

                // ---- left edge
                let lt = 2 * py * w + mbx;
                let left_ok = mbx > 0 && usable(&mbs[lt - 1]);
                let mut first_done = false;
                if left_ok && mbs[lt - 1].field != fld {
                    // the left pair is coded the other way: a strength per two lines of the
                    // current macroblock (one line of 4:2:0 chroma), from the left macroblock
                    // covering them
                    first_done = true;
                    let nb = |i: usize| &mbs[lt - 1 + if fld { i >> 2 } else { i & 1 } * w];
                    let mut bs8 = [0u8; 8];
                    for (i, b) in bs8.iter_mut().enumerate() {
                        let n = nb(i);
                        let nrow = if fld { i & 3 } else { bot * 2 + (i >> 2) };
                        *b = if intra(m) || intra(n) { 4 } else { 1 + (coded(m, 0, i >> 1) || coded(n, 3, nrow)) as u8 };
                    }
                    // the strength of line pair i, filtered as FFmpeg's groups of lines do
                    let eff = |i: usize| {
                        let first = if fld { bs8[i & 4] } else { bs8[i & 1] };
                        match (first, bs8[i]) {
                            (4, _) => 4,
                            (_, 4) => 0,
                            (_, b) => b,
                        }
                    };
                    // line t of a component whose macroblock has `n` lines: its strength index
                    let bsi = |t: usize, n: usize| if n == 8 { t } else if fld { t >> 1 } else { 2 * (t >> 2) + (t & 1) };
                    for pl in 0..ncomp {
                        for t in 0..16 {
                            let i = bsi(t, 16);
                            let n = nb(i);
                            let (alpha, beta, ia) = thresholds(qpav(n, pl), &sp, bd);
                            filter_one(&mut planes[pl], lo + t * lrs, 1, eff(i), ia, alpha, beta, false, bd, (n.keep, m.keep));
                        }
                    }
                    if chroma {
                        for comp in 0..2 {
                            for t in 0..chh {
                                let i = bsi(t, chh);
                                let n = nb(i);
                                let (alpha, beta, ia) = thresholds((cqp(n.qp, comp) + cqp(m.qp, comp) + 1) >> 1, &sp, bd);
                                filter_one(&mut planes[1 + comp], co + t * crs, 1, eff(i), ia, alpha, beta, true, bd, (n.keep, m.keep));
                            }
                        }
                    }
                }

                // ---- top edge: the macroblock above in the same field, or the frame row above
                let top = if fld {
                    if py == 0 {
                        None
                    } else if bot == 1 || mbs[idx - 2 * w].field {
                        Some(idx - 2 * w)
                    } else {
                        Some(idx - w)
                    }
                } else if bot == 1 || py > 0 {
                    Some(idx - w)
                } else {
                    None
                };
                let top = top.filter(|&t| usable(&mbs[t]));
                // a frame macroblock under a field pair: its top edge is filtered once for each field
                let split = !fld && bot == 0 && top.is_some_and(|t| mbs[t].field);

                // ---- strengths of the other edges
                let mut bs = [[[0u8; 4]; 4]; 2];
                for e in 0..4usize {
                    if e > 0 || (left_ok && !first_done) {
                        bs[0][e] = strength(m, if e == 0 { &mbs[idx - 1] } else { m }, 0, e, mvy);
                    }
                    if e > 0 {
                        bs[1][e] = strength(m, m, 1, e, mvy);
                    }
                }
                if let (Some(t), false) = (top, split) {
                    let n = &mbs[t];
                    bs[1][0] = if intra(m) || intra(n) {
                        [if fld || n.field { 3 } else { 4 }; 4]
                    } else if fld != n.field {
                        std::array::from_fn(|i| 1 + (coded(m, i, 0) || coded(n, i, 3)) as u8)
                    } else {
                        strength(m, n, 1, 0, mvy)
                    };
                }
                let edge_mb = [if left_ok { Some(&mbs[idx - 1]) } else { None }, if split { None } else { top.map(|t| &mbs[t]) }];
                let other = |dir: usize, e: usize| if e == 0 { edge_mb[dir] } else { Some(m) };
                // the two top edges of a frame macroblock under a field pair, after the vertical edges
                let split_b = |j: usize| -> (&MbInfo, [u8; 4]) {
                    let n = &mbs[idx - 2 * w + j * w];
                    (n, if intra(m) || intra(n) { [3; 4] } else { std::array::from_fn(|i| 1 + (coded(m, i, 0) || coded(n, i, 3)) as u8) })
                };

                // ---- filtering
                for pl in 0..ncomp {
                    for dir in 0..2 {
                        if dir == 1 && split {
                            for j in 0..2usize {
                                let (n, b) = split_b(j);
                                let (alpha, beta, ia) = thresholds(qpav(n, pl), &sp, bd);
                                filter_edge(&mut planes[pl], lo + j * ls, 2 * ls, 1, 4, &b, alpha, beta, ia, false, bd, (n.keep, m.keep));
                            }
                        }
                        for e in 0..4usize {
                            if (dir == 0 && e == 0 && first_done) || (m.t8x8 && e & 1 == 1) {
                                continue;
                            }
                            let Some(o) = other(dir, e) else { continue };
                            let (alpha, beta, ia) = thresholds(qpav(o, pl), &sp, bd);
                            if dir == 0 {
                                filter_edge(&mut planes[pl], lo + e * 4, 1, lrs, 4, &bs[dir][e], alpha, beta, ia, false, bd, (o.keep, m.keep));
                            } else {
                                filter_edge(&mut planes[pl], lo + e * 4 * lrs, lrs, 1, 4, &bs[dir][e], alpha, beta, ia, false, bd, (o.keep, m.keep));
                            }
                        }
                    }
                }
                if !chroma {
                    continue;
                }
                for comp in 0..2usize {
                    for dir in 0..2 {
                        if dir == 1 && split {
                            for j in 0..2usize {
                                let (n, b) = split_b(j);
                                let (alpha, beta, ia) = thresholds((cqp(n.qp, comp) + cqp(m.qp, comp) + 1) >> 1, &sp, bd);
                                filter_edge(&mut planes[1 + comp], co + j * cs, 2 * cs, 1, 2, &b, alpha, beta, ia, true, bd, (n.keep, m.keep));
                            }
                        }
                        let edges: &[usize] = if dir == 1 && chh == 16 { &[0, 1, 2, 3] } else { &[0, 2] };
                        for &e in edges {
                            if dir == 0 && e == 0 && first_done {
                                continue;
                            }
                            let Some(o) = other(dir, e) else { continue };
                            let (alpha, beta, ia) = thresholds((cqp(o.qp, comp) + cqp(m.qp, comp) + 1) >> 1, &sp, bd);
                            if dir == 0 {
                                filter_edge(&mut planes[1 + comp], co + e * 2, 1, crs, chh / 4, &bs[dir][e], alpha, beta, ia, true, bd, (o.keep, m.keep));
                            } else if chh == 16 {
                                filter_edge(&mut planes[1 + comp], co + e * 4 * crs, crs, 1, 2, &bs[dir][e], alpha, beta, ia, true, bd, (o.keep, m.keep));
                            } else {
                                filter_edge(&mut planes[1 + comp], co + e * 2 * crs, crs, 1, 2, &bs[dir][e], alpha, beta, ia, true, bd, (o.keep, m.keep));
                            }
                        }
                    }
                }
            }
        }
    }
}

#[cfg(all(test, target_arch = "x86_64"))]
mod simd_tests {
    use super::*;

    /// A plane that is read and written by index only, which takes `filter_edge` down its scalar path.
    struct Scalar<'a>(&'a mut [u16]);
    impl Px for Scalar<'_> {
        fn px(&self, i: usize) -> u16 {
            self.0[i]
        }
        fn set(&mut self, i: usize, v: u16) {
            self.0[i] = v;
        }
    }

    #[test]
    fn simd_edges_match_scalar_ones() {
        let (w, h) = (64usize, 64usize);
        let mut seed = 77u32;
        let mut rnd = move || {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (seed >> 8) as usize
        };
        for bd in [8u32, 10] {
            let maxv = (1usize << bd) - 1;
            for round in 0..4000 {
                // smooth samples with some noise, so that most lines are filtered, else wild ones
                let noise = [1, 3, 12, maxv][round % 4];
                let base = rnd() % (maxv + 1);
                let plane: Vec<u16> = (0..w * h).map(|_| (base + rnd() % (noise + 1)).min(maxv) as u16).collect();
                let chroma = rnd() % 2 == 0;
                let vertical = rnd() % 2 == 0;
                let per_bs = if chroma { [2, 4][rnd() % 2] } else { 4 };
                let bs: [u8; 4] = std::array::from_fn(|_| (rnd() % 5) as u8);
                let index_a = rnd() % 52;
                let (alpha, beta) = ((ALPHA[index_a] as i32) << (bd - 8), (BETA[rnd() % 52] as i32) << (bd - 8));
                // an edge with room for four samples on each side and 16 lines
                let (pos, xs, ys) = if vertical { (rnd() % 8 * 4 + 4 + (8 + rnd() % 4) * w, 1, w) } else { (4 * w + rnd() % 3 * 16 + rnd() % 4 * 4 * w, w, 1) };
                let (mut a, mut b) = (plane.clone(), plane);
                filter_edge(&mut a, pos, xs, ys, per_bs, &bs, alpha, beta, index_a, chroma, bd, (false, false));
                filter_edge(&mut Scalar(&mut b), pos, xs, ys, per_bs, &bs, alpha, beta, index_a, chroma, bd, (false, false));
                assert_eq!(a, b, "bd {bd} round {round} chroma {chroma} vertical {vertical} per_bs {per_bs} bs {bs:?} alpha {alpha} beta {beta}");
            }
        }
    }
}

#[cfg(test)]
mod wave_tests {
    use super::*;

    /// Rows that wait for the one above, with far more threads than cores and a row that panics: nobody hangs
    /// and every row sees the whole row above it.
    #[test]
    fn waiting_rows_wake_up_also_when_a_row_panics() {
        for panic_row in [None, Some(3)] {
            for _ in 0..20 {
                let (rows, w) = (40, 50);
                let wave = Wave::new(rows);
                let seen = AtomicUsize::new(0);
                let r = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    std::thread::scope(|s| {
                        for row in 0..rows {
                            let (wave, seen) = (&wave, &seen);
                            s.spawn(move || {
                                let _done = RowDone(wave, row);
                                for x in 0..w {
                                    if row > 0 {
                                        wave.wait(row - 1, (x + 2).min(w));
                                    }
                                    assert!(panic_row != Some(row) || x < 7, "row {row}");
                                    wave.set(row, x + 1);
                                    seen.fetch_add(1, Ordering::Relaxed);
                                }
                            });
                        }
                    })
                }));
                assert_eq!(r.is_err(), panic_row.is_some());
                if panic_row.is_none() {
                    assert_eq!(seen.load(Ordering::Relaxed), rows * w);
                }
            }
        }
    }
}
