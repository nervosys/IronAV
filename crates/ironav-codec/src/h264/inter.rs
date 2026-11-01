//! Inter prediction sample interpolation (ITU-T H.264 8.4.2.2) and weighted prediction (8.4.2.3).

fn clip1(v: i32, maxv: i32) -> u16 {
    v.clamp(0, maxv) as u16
}

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

/// Predicts a `w` x `h` luma block whose top-left is at (`x`, `y`) in the reference, displaced by
/// the quarter-sample vector (`mx`, `my`). The reference is `pw` x `ph`; samples outside it repeat
/// the nearest edge sample.
#[cfg(test)]
#[allow(clippy::too_many_arguments)]
pub fn luma_mc(refp: &[u16], pw: usize, ph: usize, x: i32, y: i32, mx: i32, my: i32, w: usize, h: usize, out: &mut [u16], maxv: i32) {
    luma_mc_st(refp, pw, pw, ph, x, y, mx, my, w, h, out, maxv)
}

/// `luma_mc` from a reference whose rows are `stride` samples apart (a field of a frame: twice
/// the width, from the field's first row).
#[allow(clippy::too_many_arguments)]
pub fn luma_mc_st(refp: &[u16], stride: usize, pw: usize, ph: usize, x: i32, y: i32, mx: i32, my: i32, w: usize, h: usize, out: &mut [u16], maxv: i32) {
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
fn bilinear(out: &mut [u16], w: usize, h: usize, fx: i32, fy: i32, at: impl Fn(i32, i32) -> i32) {
    let (wa, wb, wc, wd) = ((8 - fx) * (8 - fy), fx * (8 - fy), (8 - fx) * fy, fx * fy);
    fill(out, w, h, |c, r| {
        let (c, r) = (c as i32, r as i32);
        (wa * at(c, r) + wb * at(c + 1, r) + wc * at(c, r + 1) + wd * at(c + 1, r + 1) + 32) >> 6
    });
}

/// Predicts a `w` x `h` chroma block; the vector is in eighth-sample units.
#[allow(clippy::too_many_arguments)]
pub fn chroma_mc(refp: &[u16], pw: usize, ph: usize, x: i32, y: i32, mx: i32, my: i32, w: usize, h: usize, out: &mut [u16]) {
    chroma_mc_st(refp, pw, pw, ph, x, y, mx, my, w, h, out)
}

/// `chroma_mc` from a reference whose rows are `stride` samples apart.
#[allow(clippy::too_many_arguments)]
pub fn chroma_mc_st(refp: &[u16], stride: usize, pw: usize, ph: usize, x: i32, y: i32, mx: i32, my: i32, w: usize, h: usize, out: &mut [u16]) {
    let (fx, fy) = (mx & 7, my & 7);
    let (x0, y0) = (x + (mx >> 3), y + (my >> 3));
    // one sample past the block on each axis is read when the vector is fractional
    if x0 >= 0 && y0 >= 0 && (x0 as usize + w) < pw && (y0 as usize + h) < ph {
        bilinear(out, w, h, fx, fy, |c, r| refp[(y0 + r) as usize * stride + (x0 + c) as usize] as i32);
    } else {
        let at = |c: i32, r: i32| refp[(r.clamp(0, ph as i32 - 1) as usize) * stride + c.clamp(0, pw as i32 - 1) as usize] as i32;
        bilinear(out, w, h, fx, fy, |c, r| at(x0 + c, y0 + r));
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
