//! Inter prediction sample interpolation (ITU-T H.264 8.4.2.2) and weighted prediction (8.4.2.3).

fn clip1(v: i32, maxv: i32) -> u16 {
    v.clamp(0, maxv) as u16
}

fn tap6(a: i32, b: i32, c: i32, d: i32, e: i32, f: i32) -> i32 {
    a - 5 * b + 20 * c + 20 * d - 5 * e + f
}

/// Predicts a `w` x `h` luma block whose top-left is at (`x`, `y`) in the reference, displaced by
/// the quarter-sample vector (`mx`, `my`). The reference is `pw` x `ph`; samples outside it repeat
/// the nearest edge sample.
#[allow(clippy::too_many_arguments)]
pub fn luma_mc(refp: &[u16], pw: usize, ph: usize, x: i32, y: i32, mx: i32, my: i32, w: usize, h: usize, out: &mut [u16], maxv: i32) {
    let (fx, fy) = ((mx & 3) as usize, (my & 3) as usize);
    let (x0, y0) = (x + (mx >> 2), y + (my >> 2));
    // window of (w + 5) x (h + 5) samples starting 2 left of and 2 above the block
    let (ww, wh) = (w + 5, h + 5);
    let mut win = [0u16; 21 * 21];
    let inside = x0 >= 2 && y0 >= 2 && (x0 as usize + w + 3) <= pw && (y0 as usize + h + 3) <= ph;
    for r in 0..wh {
        if inside {
            let o = (y0 as usize - 2 + r) * pw + x0 as usize - 2;
            win[r * ww..r * ww + ww].copy_from_slice(&refp[o..o + ww]);
        } else {
            let yy = (y0 - 2 + r as i32).clamp(0, ph as i32 - 1) as usize;
            for c in 0..ww {
                let xx = (x0 - 2 + c as i32).clamp(0, pw as i32 - 1) as usize;
                win[r * ww + c] = refp[yy * pw + xx];
            }
        }
    }
    let g = |c: i32, r: i32| win[(r + 2) as usize * ww + (c + 2) as usize] as i32;
    // unrounded horizontal / vertical half-sample sums
    let b1 = |c: i32, r: i32| tap6(g(c - 2, r), g(c - 1, r), g(c, r), g(c + 1, r), g(c + 2, r), g(c + 3, r));
    let h1 = |c: i32, r: i32| tap6(g(c, r - 2), g(c, r - 1), g(c, r), g(c, r + 1), g(c, r + 2), g(c, r + 3));
    let bh = |c: i32, r: i32| clip1((b1(c, r) + 16) >> 5, maxv) as i32;
    let hv = |c: i32, r: i32| clip1((h1(c, r) + 16) >> 5, maxv) as i32;
    let jj = |c: i32, r: i32| clip1((tap6(b1(c, r - 2), b1(c, r - 1), b1(c, r), b1(c, r + 1), b1(c, r + 2), b1(c, r + 3)) + 512) >> 10, maxv) as i32;
    for r in 0..h as i32 {
        for c in 0..w as i32 {
            let v = match (fx, fy) {
                (0, 0) => g(c, r),
                (1, 0) => (g(c, r) + bh(c, r) + 1) >> 1,
                (2, 0) => bh(c, r),
                (3, 0) => (g(c + 1, r) + bh(c, r) + 1) >> 1,
                (0, 1) => (g(c, r) + hv(c, r) + 1) >> 1,
                (0, 2) => hv(c, r),
                (0, 3) => (g(c, r + 1) + hv(c, r) + 1) >> 1,
                (1, 1) => (bh(c, r) + hv(c, r) + 1) >> 1,
                (3, 1) => (bh(c, r) + hv(c + 1, r) + 1) >> 1,
                (1, 3) => (hv(c, r) + bh(c, r + 1) + 1) >> 1,
                (3, 3) => (hv(c + 1, r) + bh(c, r + 1) + 1) >> 1,
                (2, 1) => (bh(c, r) + jj(c, r) + 1) >> 1,
                (2, 3) => (jj(c, r) + bh(c, r + 1) + 1) >> 1,
                (1, 2) => (hv(c, r) + jj(c, r) + 1) >> 1,
                (3, 2) => (jj(c, r) + hv(c + 1, r) + 1) >> 1,
                _ => jj(c, r),
            };
            out[r as usize * w + c as usize] = v as u16;
        }
    }
}

/// Predicts a `w` x `h` chroma block; the vector is in eighth-sample units.
#[allow(clippy::too_many_arguments)]
pub fn chroma_mc(refp: &[u16], pw: usize, ph: usize, x: i32, y: i32, mx: i32, my: i32, w: usize, h: usize, out: &mut [u16]) {
    let (fx, fy) = ((mx & 7) as i32, (my & 7) as i32);
    let (x0, y0) = (x + (mx >> 3), y + (my >> 3));
    let at = |c: i32, r: i32| refp[(r.clamp(0, ph as i32 - 1) as usize) * pw + c.clamp(0, pw as i32 - 1) as usize] as i32;
    for r in 0..h as i32 {
        for c in 0..w as i32 {
            let (xx, yy) = (x0 + c, y0 + r);
            let v = ((8 - fx) * (8 - fy) * at(xx, yy) + fx * (8 - fy) * at(xx + 1, yy) + (8 - fx) * fy * at(xx, yy + 1) + fx * fy * at(xx + 1, yy + 1) + 32) >> 6;
            out[r as usize * w + c as usize] = v as u16;
        }
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
