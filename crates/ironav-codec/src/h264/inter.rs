//! Inter prediction sample interpolation (ITU-T H.264 8.4.2.2) and weighted prediction (8.4.2.3).

fn clip1(v: i32, maxv: i32) -> u16 {
    v.clamp(0, maxv) as u16
}

/// Predicts a `w` x `h` luma block whose top-left is at (`x`, `y`) in the reference, displaced by
/// the quarter-sample vector (`mx`, `my`). The reference is `pw` x `ph`; samples outside it repeat
/// the nearest edge sample.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub fn luma_mc(refp: &[u16], pw: usize, ph: usize, x: i32, y: i32, mx: i32, my: i32, w: usize, h: usize, out: &mut [u16], maxv: i32) {
    luma_mc_st(refp, pw, pw, ph, x, y, mx, my, w, h, out, w, maxv)
}

/// Asks the processor to fetch the cache lines of the `w` x `h` samples at (`x`, `y`) of a plane
/// of `pw` x `ph` samples (rows `stride` apart in `p`), clamped to the plane. A macroblock's
/// reference rows are a row apart each, too many streams for the hardware to follow, and they are
/// not in the cache the first time they are read.
pub fn prefetch(p: &[u16], stride: usize, pw: usize, ph: usize, x: i32, y: i32, w: usize, h: usize) {
    #[cfg(target_arch = "x86_64")]
    {
        let x0 = x.clamp(0, pw as i32 - 1) as usize;
        let x1 = (x + w as i32 - 1).clamp(0, pw as i32 - 1) as usize;
        for r in 0..h {
            let o = (y + r as i32).clamp(0, ph as i32 - 1) as usize * stride;
            for xx in [x0, x1] {
                if let Some(v) = p.get(o + xx) {
                    // SAFETY: the address is that of a sample of `p`, and a prefetch changes nothing
                    unsafe { std::arch::x86_64::_mm_prefetch::<{ std::arch::x86_64::_MM_HINT_T0 }>(std::ptr::from_ref(v).cast()) };
                }
            }
        }
    }
    #[cfg(not(target_arch = "x86_64"))]
    let _ = (p, stride, pw, ph, x, y, w, h);
}

/// `luma_mc` from a reference whose rows are `stride` samples apart (a field of a frame: twice
/// the width, from the field's first row).
#[allow(clippy::too_many_arguments)]
pub fn luma_mc_st(refp: &[u16], stride: usize, pw: usize, ph: usize, x: i32, y: i32, mx: i32, my: i32, w: usize, h: usize, out: &mut [u16], os: usize, maxv: i32) {
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") {
        // SAFETY: the CPU supports AVX2, checked just above; the function is otherwise safe code
        unsafe { luma_mc_avx2(refp, stride, pw, ph, x, y, mx, my, w, h, out, os, maxv) };
        return;
    }
    luma_mc_impl(refp, stride, pw, ph, x, y, mx, my, w, h, out, os, maxv)
}

/// `luma_mc_impl` compiled for AVX2, so that its loops are vectorised with it.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[allow(clippy::too_many_arguments)]
unsafe fn luma_mc_avx2(refp: &[u16], stride: usize, pw: usize, ph: usize, x: i32, y: i32, mx: i32, my: i32, w: usize, h: usize, out: &mut [u16], os: usize, maxv: i32) {
    luma_mc_impl(refp, stride, pw, ph, x, y, mx, my, w, h, out, os, maxv)
}

/// One of the sample planes the quarter-sample positions are made from (8.4.2.2.1), as a row of a block.
#[derive(Clone, Copy)]
enum Src {
    /// Full samples, `ro` rows down and `co` columns right of the block.
    G(usize, usize),
    /// Rounded horizontal half samples `b`, `ro` rows down.
    Bh(usize),
    /// Rounded vertical half samples `h`, `co` columns right.
    Hv(usize),
    /// The centre half samples `j`.
    Jj,
}

/// AVX2 interpolation of 8-bit samples (held in `u16`s), 16 columns at a time: the 6-tap sums of
/// 8-bit samples fit 16 bits, which is what makes 16 lanes possible. Blocks narrower than 16
/// compute the lanes past their width too (from samples of the window that exist) and drop them.
#[cfg(target_arch = "x86_64")]
mod avx2 {
    use super::Src;
    use std::arch::x86_64::*;

    /// 16 samples of `s` from `o`; panics when they are not all there.
    #[target_feature(enable = "avx2")]
    #[inline]
    fn ld(s: &[u16], o: usize) -> __m256i {
        let a: &[u16; 16] = s[o..o + 16].try_into().unwrap();
        // SAFETY: `a` is 16 readable samples, and an unaligned load needs nothing more
        unsafe { _mm256_loadu_si256(a.as_ptr().cast()) }
    }

    #[target_feature(enable = "avx2")]
    #[inline]
    fn st(buf: &mut [u16; 16], v: __m256i) {
        // SAFETY: `buf` is 16 writable samples, and an unaligned store needs nothing more
        unsafe { _mm256_storeu_si256(buf.as_mut_ptr().cast(), v) }
    }

    /// a - 5 b + 20 c + 20 d - 5 e + f, in 16 bits.
    #[target_feature(enable = "avx2")]
    #[inline]
    fn tap6(a: __m256i, b: __m256i, c: __m256i, d: __m256i, e: __m256i, f: __m256i) -> __m256i {
        let outer = _mm256_add_epi16(a, f);
        let mid = _mm256_mullo_epi16(_mm256_add_epi16(c, d), _mm256_set1_epi16(20));
        let near = _mm256_mullo_epi16(_mm256_add_epi16(b, e), _mm256_set1_epi16(5));
        _mm256_sub_epi16(_mm256_add_epi16(outer, mid), near)
    }

    /// The same in 32 bits, for the sums of sums.
    #[target_feature(enable = "avx2")]
    #[inline]
    fn tap6_32(a: __m256i, b: __m256i, c: __m256i, d: __m256i, e: __m256i, f: __m256i) -> __m256i {
        let outer = _mm256_add_epi32(a, f);
        let mid = _mm256_mullo_epi32(_mm256_add_epi32(c, d), _mm256_set1_epi32(20));
        let near = _mm256_mullo_epi32(_mm256_add_epi32(b, e), _mm256_set1_epi32(5));
        _mm256_sub_epi32(_mm256_add_epi32(outer, mid), near)
    }

    /// Clip1((v + 16) >> 5) for 8-bit samples.
    #[target_feature(enable = "avx2")]
    #[inline]
    fn round5(v: __m256i) -> __m256i {
        let s = _mm256_srai_epi16(_mm256_add_epi16(v, _mm256_set1_epi16(16)), 5);
        _mm256_min_epi16(_mm256_max_epi16(s, _mm256_setzero_si256()), _mm256_set1_epi16(255))
    }

    /// Clip1((j + 512) >> 10) of the sums of the 16-bit horizontal sums `b[r..r + 6]`.
    #[target_feature(enable = "avx2")]
    #[inline]
    fn centre(b: &[__m256i]) -> __m256i {
        let lo = |x: __m256i| _mm256_cvtepi16_epi32(_mm256_castsi256_si128(x));
        let hi = |x: __m256i| _mm256_cvtepi16_epi32(_mm256_extracti128_si256(x, 1));
        let r = _mm256_set1_epi32(512);
        let l = _mm256_srai_epi32(_mm256_add_epi32(tap6_32(lo(b[0]), lo(b[1]), lo(b[2]), lo(b[3]), lo(b[4]), lo(b[5])), r), 10);
        let h = _mm256_srai_epi32(_mm256_add_epi32(tap6_32(hi(b[0]), hi(b[1]), hi(b[2]), hi(b[3]), hi(b[4]), hi(b[5])), r), 10);
        // packs works within 128-bit halves: put the columns back in order
        let p = _mm256_permute4x64_epi64(_mm256_packs_epi32(l, h), 0b11_01_10_00);
        _mm256_min_epi16(_mm256_max_epi16(p, _mm256_setzero_si256()), _mm256_set1_epi16(255))
    }

    /// Interpolates the quarter-sample position (`fx`, `fy`) (not 0, 0) of a `w` x `h` block (at
    /// most 16 x 16) from the window of (w + 5) x (h + 5) samples at `base` of `src`, whose rows are
    /// `stride` apart, into `out`.
    ///
    /// SAFETY: the CPU must support AVX2, and `src` must hold 21 samples from the start of the
    /// window's last row.
    #[target_feature(enable = "avx2")]
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn luma8(src: &[u16], stride: usize, base: usize, fx: usize, fy: usize, w: usize, h: usize, out: &mut [u16], os: usize) {
        let wrow = |r: usize, c: usize| base + r * stride + c;
        let jj_case = (fx == 2 && fy != 0) || (fy == 2 && fx != 0);
        let mut b1 = [_mm256_setzero_si256(); 21];
        if fx != 0 {
            let (rb, re) = if jj_case { (0, h + 5) } else { (2, h + 3) };
            for (rr, b) in b1.iter_mut().enumerate().take(re).skip(rb) {
                let o = wrow(rr, 0);
                *b = tap6(ld(src, o), ld(src, o + 1), ld(src, o + 2), ld(src, o + 3), ld(src, o + 4), ld(src, o + 5));
            }
        }
        let (a, b) = match (fx, fy) {
            (1, 0) => (Src::G(0, 0), Some(Src::Bh(0))),
            (2, 0) => (Src::Bh(0), None),
            (3, 0) => (Src::G(0, 1), Some(Src::Bh(0))),
            (0, 1) => (Src::G(0, 0), Some(Src::Hv(0))),
            (0, 2) => (Src::Hv(0), None),
            (0, 3) => (Src::G(1, 0), Some(Src::Hv(0))),
            (1, 1) => (Src::Bh(0), Some(Src::Hv(0))),
            (3, 1) => (Src::Bh(0), Some(Src::Hv(1))),
            (1, 3) => (Src::Bh(1), Some(Src::Hv(0))),
            (3, 3) => (Src::Bh(1), Some(Src::Hv(1))),
            (2, 1) => (Src::Bh(0), Some(Src::Jj)),
            (2, 3) => (Src::Bh(1), Some(Src::Jj)),
            (1, 2) => (Src::Hv(0), Some(Src::Jj)),
            (3, 2) => (Src::Hv(1), Some(Src::Jj)),
            _ => (Src::Jj, None),
        };
        let fetch = |s: Src, r: usize| -> __m256i {
            match s {
                Src::G(ro, co) => ld(src, wrow(r + 2 + ro, 2 + co)),
                Src::Bh(ro) => round5(b1[r + 2 + ro]),
                Src::Hv(co) => {
                    let o = |k: usize| wrow(r + k, 2 + co);
                    round5(tap6(ld(src, o(0)), ld(src, o(1)), ld(src, o(2)), ld(src, o(3)), ld(src, o(4)), ld(src, o(5))))
                }
                Src::Jj => centre(&b1[r..r + 6]),
            }
        };
        let mut buf = [0u16; 16];
        for r in 0..h {
            let mut v = fetch(a, r);
            if let Some(b) = b {
                v = _mm256_avg_epu16(v, fetch(b, r));
            }
            st(&mut buf, v);
            out[r * os..r * os + w].copy_from_slice(&buf[..w]);
        }
    }

    /// Eighth-sample bilinear interpolation of a `w` x `h` block (`w` at most 16) from the window of
    /// (w + 1) x (h + 1) samples at `base` of `src`, for samples of at most 10 bits.
    ///
    /// SAFETY: the CPU must support AVX2, and `src` must hold 17 samples from the start of the
    /// window's last row.
    #[target_feature(enable = "avx2")]
    #[allow(clippy::too_many_arguments)]
    pub unsafe fn bilinear(out: &mut [u16], os: usize, w: usize, h: usize, fx: i32, fy: i32, src: &[u16], stride: usize, base: usize) {
        let k = |v: i32| _mm256_set1_epi16(v as i16);
        let (wa, wb, wc, wd) = (k((8 - fx) * (8 - fy)), k(fx * (8 - fy)), k((8 - fx) * fy), k(fx * fy));
        let round = k(32);
        let mut buf = [0u16; 16];
        for r in 0..h {
            let (o0, o1) = (base + r * stride, base + (r + 1) * stride);
            // the weights sum to 64, so with 10-bit samples the sum is below 2^16: no overflow
            let mut v = _mm256_mullo_epi16(ld(src, o0), wa);
            v = _mm256_add_epi16(v, _mm256_mullo_epi16(ld(src, o0 + 1), wb));
            v = _mm256_add_epi16(v, _mm256_mullo_epi16(ld(src, o1), wc));
            v = _mm256_add_epi16(v, _mm256_mullo_epi16(ld(src, o1 + 1), wd));
            v = _mm256_srli_epi16(_mm256_add_epi16(v, round), 6);
            st(&mut buf, v);
            out[r * os..r * os + w].copy_from_slice(&buf[..w]);
        }
    }
}

/// Horizontal 6-tap sums, unrounded, of the outputs of one window row.
#[inline(always)]
fn hsum(row: &[u16], out: &mut [i32]) {
    let w = out.len();
    let (s0, s1, s2, s3, s4, s5) = (&row[..w], &row[1..w + 1], &row[2..w + 2], &row[3..w + 3], &row[4..w + 4], &row[5..w + 5]);
    for c in 0..w {
        out[c] = s0[c] as i32 - 5 * s1[c] as i32 + 20 * (s2[c] as i32 + s3[c] as i32) - 5 * s4[c] as i32 + s5[c] as i32;
    }
}

#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn luma_mc_impl(refp: &[u16], stride: usize, pw: usize, ph: usize, x: i32, y: i32, mx: i32, my: i32, w: usize, h: usize, out: &mut [u16], os: usize, maxv: i32) {
    let (fx, fy) = ((mx & 3) as usize, (my & 3) as usize);
    let (x0, y0) = (x + (mx >> 2), y + (my >> 2));
    if fx == 0 && fy == 0 {
        if x0 >= 0 && y0 >= 0 && x0 as usize + w <= pw && y0 as usize + h <= ph {
            for r in 0..h {
                let o = (y0 as usize + r) * stride + x0 as usize;
                out[r * os..r * os + w].copy_from_slice(&refp[o..o + w]);
            }
        } else {
            for r in 0..h {
                let yy = (y0 + r as i32).clamp(0, ph as i32 - 1) as usize;
                for c in 0..w {
                    let xx = (x0 + c as i32).clamp(0, pw as i32 - 1) as usize;
                    out[r * os + c] = refp[yy * stride + xx];
                }
            }
        }
        return;
    }
    // window of (w + 5) x (h + 5) samples starting 2 left of and 2 above the block; inside the
    // picture it is read in place from the reference (stride `st`, offset `base`)
    let (ww, wh) = (w + 5, h + 5);
    let inside = x0 >= 2 && y0 >= 2 && (x0 as usize + w + 3) <= pw && (y0 as usize + h + 3) <= ph;
    let mut win = [0u16; 21 * 21];
    let (src, st, base) = if inside {
        (refp, stride, (y0 as usize - 2) * stride + x0 as usize - 2)
    } else {
        for r in 0..wh {
            let yy = (y0 - 2 + r as i32).clamp(0, ph as i32 - 1) as usize;
            for c in 0..ww {
                let xx = (x0 - 2 + c as i32).clamp(0, pw as i32 - 1) as usize;
                win[r * ww + c] = refp[yy * stride + xx];
            }
        }
        (&win[..], ww, 0)
    };
    #[cfg(target_arch = "x86_64")]
    if maxv == 255 && w <= 16 && h <= 16 && base + (wh - 1) * st + 21 <= src.len() && std::arch::is_x86_feature_detected!("avx2") {
        // SAFETY: AVX2 is supported (checked above), and the window's last row has 21 samples
        unsafe { avx2::luma8(src, st, base, fx, fy, w, h, out, os) };
        return;
    }
    let wrow = |r: usize| &src[base + r * st..base + r * st + ww];
    // unrounded horizontal sums b1 per window row; only the rows the case reads
    let jj_case = (fx == 2 && fy != 0) || (fy == 2 && fx != 0);
    let mut b1 = [[0i32; 16]; 21];
    if fx != 0 {
        let (rb, re) = if jj_case { (0, wh) } else { (2, h + 3) };
        for rr in rb..re {
            hsum(wrow(rr), &mut b1[rr][..w]);
        }
    }
    let (a, b) = match (fx, fy) {
        (1, 0) => (Src::G(0, 0), Some(Src::Bh(0))),
        (2, 0) => (Src::Bh(0), None),
        (3, 0) => (Src::G(0, 1), Some(Src::Bh(0))),
        (0, 1) => (Src::G(0, 0), Some(Src::Hv(0))),
        (0, 2) => (Src::Hv(0), None),
        (0, 3) => (Src::G(1, 0), Some(Src::Hv(0))),
        (1, 1) => (Src::Bh(0), Some(Src::Hv(0))),
        (3, 1) => (Src::Bh(0), Some(Src::Hv(1))),
        (1, 3) => (Src::Bh(1), Some(Src::Hv(0))),
        (3, 3) => (Src::Bh(1), Some(Src::Hv(1))),
        (2, 1) => (Src::Bh(0), Some(Src::Jj)),
        (2, 3) => (Src::Bh(1), Some(Src::Jj)),
        (1, 2) => (Src::Hv(0), Some(Src::Jj)),
        (3, 2) => (Src::Hv(1), Some(Src::Jj)),
        _ => (Src::Jj, None),
    };
    // block row `r`, as the samples of `s`
    let fetch = |s: Src, r: usize, buf: &mut [u16]| match s {
        Src::G(ro, co) => buf.copy_from_slice(&wrow(r + 2 + ro)[2 + co..2 + co + w]),
        Src::Bh(ro) => {
            let b = &b1[r + 2 + ro][..w];
            for c in 0..w {
                buf[c] = ((b[c] + 16) >> 5).clamp(0, maxv) as u16;
            }
        }
        Src::Hv(co) => {
            let o = 2 + co;
            let (s0, s1, s2, s3, s4, s5) = (&wrow(r)[o..o + w], &wrow(r + 1)[o..o + w], &wrow(r + 2)[o..o + w], &wrow(r + 3)[o..o + w], &wrow(r + 4)[o..o + w], &wrow(r + 5)[o..o + w]);
            for c in 0..w {
                let v = s0[c] as i32 - 5 * s1[c] as i32 + 20 * (s2[c] as i32 + s3[c] as i32) - 5 * s4[c] as i32 + s5[c] as i32;
                buf[c] = ((v + 16) >> 5).clamp(0, maxv) as u16;
            }
        }
        Src::Jj => {
            let (s0, s1, s2, s3, s4, s5) = (&b1[r][..w], &b1[r + 1][..w], &b1[r + 2][..w], &b1[r + 3][..w], &b1[r + 4][..w], &b1[r + 5][..w]);
            for c in 0..w {
                let v = s0[c] - 5 * s1[c] + 20 * (s2[c] + s3[c]) - 5 * s4[c] + s5[c];
                buf[c] = ((v + 512) >> 10).clamp(0, maxv) as u16;
            }
        }
    };
    let mut ba = [0u16; 16];
    let mut bb = [0u16; 16];
    for r in 0..h {
        let o = &mut out[r * os..r * os + w];
        fetch(a, r, &mut ba[..w]);
        match b {
            None => o.copy_from_slice(&ba[..w]),
            Some(b) => {
                fetch(b, r, &mut bb[..w]);
                for c in 0..w {
                    o[c] = (ba[c] + bb[c] + 1) >> 1;
                }
            }
        }
    }
}

/// Predicts a `w` x `h` chroma block; the vector is in eighth-sample units.
#[allow(clippy::too_many_arguments)]
pub fn chroma_mc(refp: &[u16], pw: usize, ph: usize, x: i32, y: i32, mx: i32, my: i32, w: usize, h: usize, out: &mut [u16]) {
    chroma_mc_st(refp, pw, pw, ph, x, y, mx, my, w, h, out, w)
}

/// `chroma_mc` from a reference whose rows are `stride` samples apart.
#[allow(clippy::too_many_arguments)]
pub fn chroma_mc_st(refp: &[u16], stride: usize, pw: usize, ph: usize, x: i32, y: i32, mx: i32, my: i32, w: usize, h: usize, out: &mut [u16], os: usize) {
    #[cfg(target_arch = "x86_64")]
    if std::arch::is_x86_feature_detected!("avx2") {
        // SAFETY: the CPU supports AVX2, checked just above; the function is otherwise safe code
        unsafe { chroma_mc_avx2(refp, stride, pw, ph, x, y, mx, my, w, h, out, os) };
        return;
    }
    chroma_mc_impl(refp, stride, pw, ph, x, y, mx, my, w, h, out, os)
}

/// `chroma_mc_impl` compiled for AVX2, so that its loops are vectorised with it.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[allow(clippy::too_many_arguments)]
unsafe fn chroma_mc_avx2(refp: &[u16], stride: usize, pw: usize, ph: usize, x: i32, y: i32, mx: i32, my: i32, w: usize, h: usize, out: &mut [u16], os: usize) {
    chroma_mc_impl(refp, stride, pw, ph, x, y, mx, my, w, h, out, os)
}

/// Eighth-sample bilinear interpolation of a `w` x `h` block from the window of (w + 1) x (h + 1)
/// samples at `base` of `src`, whose rows are `st` apart.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn bilinear(out: &mut [u16], os: usize, w: usize, h: usize, fx: i32, fy: i32, src: &[u16], st: usize, base: usize) {
    let (wa, wb, wc, wd) = ((8 - fx) * (8 - fy), fx * (8 - fy), (8 - fx) * fy, fx * fy);
    for r in 0..h {
        let (s0, s1) = (&src[base + r * st..base + r * st + w + 1], &src[base + (r + 1) * st..base + (r + 1) * st + w + 1]);
        let o = &mut out[r * os..r * os + w];
        for c in 0..w {
            o[c] = ((wa * s0[c] as i32 + wb * s0[c + 1] as i32 + wc * s1[c] as i32 + wd * s1[c + 1] as i32 + 32) >> 6) as u16;
        }
    }
}

/// `bilinear`, 16 samples at a time where AVX2 allows.
#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn bilinear_fast(out: &mut [u16], os: usize, w: usize, h: usize, fx: i32, fy: i32, src: &[u16], st: usize, base: usize) {
    #[cfg(target_arch = "x86_64")]
    if w <= 16 && base + h * st + 17 <= src.len() && std::arch::is_x86_feature_detected!("avx2") {
        // SAFETY: AVX2 is supported (checked above), and the window's last row has 17 samples; the
        // decoder has no samples of more than 10 bits, whose weighted sums fit 16 bits
        unsafe { avx2::bilinear(out, os, w, h, fx, fy, src, st, base) };
        return;
    }
    bilinear(out, os, w, h, fx, fy, src, st, base)
}

#[inline(always)]
#[allow(clippy::too_many_arguments)]
fn chroma_mc_impl(refp: &[u16], stride: usize, pw: usize, ph: usize, x: i32, y: i32, mx: i32, my: i32, w: usize, h: usize, out: &mut [u16], os: usize) {
    let (fx, fy) = (mx & 7, my & 7);
    let (x0, y0) = (x + (mx >> 3), y + (my >> 3));
    // one sample past the block on each axis is read when the vector is fractional
    if x0 >= 0 && y0 >= 0 && (x0 as usize + w) < pw && (y0 as usize + h) < ph {
        bilinear_fast(out, os, w, h, fx, fy, refp, stride, y0 as usize * stride + x0 as usize);
    } else {
        let mut win = [0u16; 17 * 17];
        for r in 0..=h {
            let yy = (y0 + r as i32).clamp(0, ph as i32 - 1) as usize;
            for c in 0..=w {
                let xx = (x0 + c as i32).clamp(0, pw as i32 - 1) as usize;
                win[r * 17 + c] = refp[yy * stride + xx];
            }
        }
        bilinear_fast(out, os, w, h, fx, fy, &win, 17, 0);
    }
}

/// Explicit weighting of one prediction.
pub fn weight_uni(p: &mut [u16], log_wd: u32, w: i32, o: i32, maxv: i32) {
    for v in p.iter_mut() {
        let s = *v as i32;
        *v = clip1(if log_wd >= 1 { ((s * w + (1 << (log_wd - 1))) >> log_wd) + o } else { s * w + o }, maxv);
    }
}

/// Weighted bi-prediction of two blocks into `a`.
pub fn weight_bi(a: &mut [u16], b: &[u16], log_wd: u32, w0: i32, w1: i32, o: i32, maxv: i32) {
    for (x, &y) in a.iter_mut().zip(b) {
        *x = clip1(((*x as i32 * w0 + y as i32 * w1 + (1 << log_wd)) >> (log_wd + 1)) + o, maxv);
    }
}

/// Default bi-prediction: the rounded average.
pub fn average(a: &mut [u16], b: &[u16]) {
    for (x, &y) in a.iter_mut().zip(b) {
        *x = (*x + y + 1) >> 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // the straightforward per-sample versions, which the row-wise ones must match
fn tap6(a: i32, b: i32, c: i32, d: i32, e: i32, f: i32) -> i32 {
    a - 5 * b + 20 * c + 20 * d - 5 * e + f
}

/// Fills the `w` x `h` block `out` with `f(c, r)`.
fn fill(out: &mut [u16], w: usize, h: usize, f: impl Fn(usize, usize) -> i32) {
    for r in 0..h {
        for c in 0..w {
            out[r * w + c] = f(c, r) as u16;
        }
    }
}

/// `luma_mc` from a reference whose rows are `stride` samples apart (a field of a frame: twice
/// the width, from the field's first row).
#[allow(clippy::too_many_arguments)]
pub fn luma_mc_ref(refp: &[u16], stride: usize, pw: usize, ph: usize, x: i32, y: i32, mx: i32, my: i32, w: usize, h: usize, out: &mut [u16], maxv: i32) {
    let (fx, fy) = ((mx & 3) as usize, (my & 3) as usize);
    let (x0, y0) = (x + (mx >> 2), y + (my >> 2));
    if fx == 0 && fy == 0 {
        if x0 >= 0 && y0 >= 0 && x0 as usize + w <= pw && y0 as usize + h <= ph {
            for r in 0..h {
                let o = (y0 as usize + r) * stride + x0 as usize;
                out[r * w..r * w + w].copy_from_slice(&refp[o..o + w]);
            }
        } else {
            for r in 0..h {
                let yy = (y0 + r as i32).clamp(0, ph as i32 - 1) as usize;
                for c in 0..w {
                    let xx = (x0 + c as i32).clamp(0, pw as i32 - 1) as usize;
                    out[r * w + c] = refp[yy * stride + xx];
                }
            }
        }
        return;
    }
    // window of (w + 5) x (h + 5) samples starting 2 left of and 2 above the block; inside the
    // picture it is read in place from the reference (stride `st`, offset `base`)
    let (ww, wh) = (w + 5, h + 5);
    let inside = x0 >= 2 && y0 >= 2 && (x0 as usize + w + 3) <= pw && (y0 as usize + h + 3) <= ph;
    let mut win = [0u16; 21 * 21];
    let (src, st, base) = if inside {
        (refp, stride, (y0 as usize - 2) * stride + x0 as usize - 2)
    } else {
        for r in 0..wh {
            let yy = (y0 - 2 + r as i32).clamp(0, ph as i32 - 1) as usize;
            for c in 0..ww {
                let xx = (x0 - 2 + c as i32).clamp(0, pw as i32 - 1) as usize;
                win[r * ww + c] = refp[yy * stride + xx];
            }
        }
        (&win[..], ww, 0)
    };
    // block sample (c, r) is window sample (c + 2, r + 2)
    let g = |c: usize, r: usize| src[base + (r + 2) * st + c + 2] as i32;
    // unrounded horizontal sums b1 at (window column c, window row); only the rows the case reads
    let jj_case = (fx == 2 && fy != 0) || (fy == 2 && fx != 0);
    let mut b1 = [0i32; 21 * 21];
    if fx != 0 {
        let (rb, re) = if jj_case { (0, wh) } else { (2, h + 3) };
        for rr in rb..re {
            for c in 0..w {
                let o = base + rr * st + c;
                let t = &src[o..o + 6];
                b1[rr * ww + c] = tap6(t[0] as i32, t[1] as i32, t[2] as i32, t[3] as i32, t[4] as i32, t[5] as i32);
            }
        }
    }
    let bh = |c: usize, r: usize| clip1((b1[(r + 2) * ww + c] + 16) >> 5, maxv) as i32;
    let hv = |c: usize, r: usize| {
        let o = base + r * st + c + 2;
        let t = |k: usize| src[o + k * st] as i32;
        clip1((tap6(t(0), t(1), t(2), t(3), t(4), t(5)) + 16) >> 5, maxv) as i32
    };
    let jj = |c: usize, r: usize| {
        let o = r * ww + c;
        let t = |k: usize| b1[o + k * ww];
        clip1((tap6(t(0), t(1), t(2), t(3), t(4), t(5)) + 512) >> 10, maxv) as i32
    };
    match (fx, fy) {
        (1, 0) => fill(out, w, h, |c, r| (g(c, r) + bh(c, r) + 1) >> 1),
        (2, 0) => fill(out, w, h, bh),
        (3, 0) => fill(out, w, h, |c, r| (g(c + 1, r) + bh(c, r) + 1) >> 1),
        (0, 1) => fill(out, w, h, |c, r| (g(c, r) + hv(c, r) + 1) >> 1),
        (0, 2) => fill(out, w, h, hv),
        (0, 3) => fill(out, w, h, |c, r| (g(c, r + 1) + hv(c, r) + 1) >> 1),
        (1, 1) => fill(out, w, h, |c, r| (bh(c, r) + hv(c, r) + 1) >> 1),
        (3, 1) => fill(out, w, h, |c, r| (bh(c, r) + hv(c + 1, r) + 1) >> 1),
        (1, 3) => fill(out, w, h, |c, r| (hv(c, r) + bh(c, r + 1) + 1) >> 1),
        (3, 3) => fill(out, w, h, |c, r| (hv(c + 1, r) + bh(c, r + 1) + 1) >> 1),
        (2, 1) => fill(out, w, h, |c, r| (bh(c, r) + jj(c, r) + 1) >> 1),
        (2, 3) => fill(out, w, h, |c, r| (jj(c, r) + bh(c, r + 1) + 1) >> 1),
        (1, 2) => fill(out, w, h, |c, r| (hv(c, r) + jj(c, r) + 1) >> 1),
        (3, 2) => fill(out, w, h, |c, r| (jj(c, r) + hv(c + 1, r) + 1) >> 1),
        _ => fill(out, w, h, jj),
    }
}

/// Eighth-sample bilinear interpolation of a `w` x `h` block; `at` fetches a reference sample.
fn bilinear_ref(out: &mut [u16], w: usize, h: usize, fx: i32, fy: i32, at: impl Fn(i32, i32) -> i32) {
    let (wa, wb, wc, wd) = ((8 - fx) * (8 - fy), fx * (8 - fy), (8 - fx) * fy, fx * fy);
    fill(out, w, h, |c, r| {
        let (c, r) = (c as i32, r as i32);
        (wa * at(c, r) + wb * at(c + 1, r) + wc * at(c, r + 1) + wd * at(c + 1, r + 1) + 32) >> 6
    });
}

/// `chroma_mc` from a reference whose rows are `stride` samples apart.
#[allow(clippy::too_many_arguments)]
pub fn chroma_mc_ref(refp: &[u16], stride: usize, pw: usize, ph: usize, x: i32, y: i32, mx: i32, my: i32, w: usize, h: usize, out: &mut [u16]) {
    let (fx, fy) = (mx & 7, my & 7);
    let (x0, y0) = (x + (mx >> 3), y + (my >> 3));
    // one sample past the block on each axis is read when the vector is fractional
    if x0 >= 0 && y0 >= 0 && (x0 as usize + w) < pw && (y0 as usize + h) < ph {
        bilinear_ref(out, w, h, fx, fy, |c, r| refp[(y0 + r) as usize * stride + (x0 + c) as usize] as i32);
    } else {
        let at = |c: i32, r: i32| refp[(r.clamp(0, ph as i32 - 1) as usize) * stride + c.clamp(0, pw as i32 - 1) as usize] as i32;
        bilinear_ref(out, w, h, fx, fy, |c, r| at(x0 + c, y0 + r));
    }
}


    struct Rng(u32);
    impl Rng {
        fn next(&mut self) -> u32 {
            self.0 = self.0.wrapping_mul(1_103_515_245).wrapping_add(12345);
            self.0 >> 8
        }
    }

    #[test]
    fn luma_matches_reference() {
        let (pw, ph) = (48usize, 40usize);
        let mut rng = Rng(5);
        for (maxv, extreme) in [(255, false), (255, true), (1023, false), (1023, true)] {
            // random samples, or only the largest and smallest ones, which make the sums largest
            let refp: Vec<u16> = (0..pw * ph).map(|_| if extreme { (rng.next() & 1) as u16 * maxv as u16 } else { (rng.next() % (maxv as u32 + 1)) as u16 }).collect();
            for &(w, h) in &[(4, 4), (8, 8), (16, 16), (8, 4), (4, 8), (16, 8), (8, 16)] {
                for _ in 0..600 {
                    let (x, y) = (rng.next() as i32 % (pw as i32 + 8) - 4, rng.next() as i32 % (ph as i32 + 8) - 4);
                    let (mx, my) = (rng.next() as i32 % 80 - 40, rng.next() as i32 % 80 - 40);
                    let (mut a, mut b) = (vec![0u16; w * h], vec![0u16; w * h]);
                    luma_mc_ref(&refp, pw, pw, ph, x, y, mx, my, w, h, &mut a, maxv);
                    luma_mc_st(&refp, pw, pw, ph, x, y, mx, my, w, h, &mut b, w, maxv);
                    assert_eq!(a, b, "{w}x{h} at ({x},{y}) mv ({mx},{my})");
                }
            }
        }
    }

    #[test]
    fn chroma_matches_reference() {
        let (pw, ph) = (24usize, 20usize);
        let mut rng = Rng(9);
        let refp: Vec<u16> = (0..pw * ph).map(|i| if i % 3 == 0 { 1023 } else { (rng.next() % 1024) as u16 }).collect();
        for &(w, h) in &[(2, 2), (4, 4), (8, 8), (4, 2), (2, 4), (8, 16), (4, 8)] {
            for _ in 0..600 {
                let (x, y) = (rng.next() as i32 % (pw as i32 + 6) - 3, rng.next() as i32 % (ph as i32 + 6) - 3);
                let (mx, my) = (rng.next() as i32 % 80 - 40, rng.next() as i32 % 80 - 40);
                let (mut a, mut b) = (vec![0u16; w * h], vec![0u16; w * h]);
                chroma_mc_ref(&refp, pw, pw, ph, x, y, mx, my, w, h, &mut a);
                chroma_mc_st(&refp, pw, pw, ph, x, y, mx, my, w, h, &mut b, w);
                assert_eq!(a, b, "{w}x{h} at ({x},{y}) mv ({mx},{my})");
            }
        }
    }

    /// `cargo test --release -p ironav-codec --lib h264::inter -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn mc_speed() {
        let (pw, ph) = (256usize, 64usize);
        let mut rng = Rng(3);
        let refp: Vec<u16> = (0..pw * ph).map(|_| (rng.next() % 256) as u16).collect();
        let mut out = vec![0u16; 256];
        let n = 200_000;
        for (fx, fy) in [(0, 0), (1, 0), (2, 0), (0, 2), (1, 1), (2, 1), (2, 2), (3, 3)] {
            for (name, new) in [("ref", false), ("new", true)] {
                let t = std::time::Instant::now();
                for i in 0..n {
                    let x = 16 + i % 100;
                    if new {
                        luma_mc_st(std::hint::black_box(&refp), pw, pw, ph, x, 16, fx, fy, 16, 16, &mut out, 16, 255);
                    } else {
                        luma_mc_ref(std::hint::black_box(&refp), pw, pw, ph, x, 16, fx, fy, 16, 16, &mut out, 255);
                    }
                    std::hint::black_box(&out);
                }
                eprintln!("luma ({fx},{fy}) {name}: {:.0} ns per 16x16", t.elapsed().as_nanos() as f64 / n as f64);
            }
        }
        for (name, new) in [("ref", false), ("new", true)] {
            let t = std::time::Instant::now();
            for i in 0..n {
                let x = 8 + i % 100;
                if new {
                    chroma_mc_st(std::hint::black_box(&refp), pw, pw, ph, x, 8, 3, 5, 8, 8, &mut out, 8);
                } else {
                    chroma_mc_ref(std::hint::black_box(&refp), pw, pw, ph, x, 8, 3, 5, 8, 8, &mut out);
                }
                std::hint::black_box(&out);
            }
            eprintln!("chroma {name}: {:.0} ns per 8x8", t.elapsed().as_nanos() as f64 / n as f64);
        }
    }
}
