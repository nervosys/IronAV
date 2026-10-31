//! The ProRes inverse transform: FFmpeg's simple IDCT with the 10-bit and 12-bit constants, as
//! `prores_idct_10` and `prores_idct_12` in `proresdsp.c` apply it. The integer arithmetic is the
//! same, wrapping where the 16-bit coefficients wrap, including the shortcut for rows whose only
//! nonzero coefficient is the DC, which rounds differently from the full transform.

const W10: [i32; 8] = [0, 22725, 21407, 19265, 16384, 12873, 8867, 4520];
const W12: [i32; 8] = [0, 45451, 42813, 38531, 32767, 25746, 17734, 9041];

/// `c * x` modulo 2^32, as the C code's unsigned products.
fn mul(c: i32, x: i32) -> u32 {
    (c as u32).wrapping_mul(x as u32)
}

/// One row of 8 coefficients, in place. `row_shift` is ROW_SHIFT, `extra` the extra shift the 10-bit
/// transform takes, and `dc` its DC_SHIFT.
fn row(r: &mut [i16], w: &[i32; 8], row_shift: i32, extra: i32, dc: i32) {
    let s = row_shift + extra;
    if r[1..].iter().all(|&x| x == 0) {
        let r0 = r[0] as i32;
        let t = if dc - extra >= 0 { r0 * (1 << (dc - extra)) } else { (r0 + (1 << (extra - dc - 1))) >> (extra - dc) };
        r.fill(t as i16);
        return;
    }
    let x = |i: usize| r[i] as i32;
    let rnd = 1u32 << (s - 1);
    let mut a0 = mul(w[4], x(0)).wrapping_add(rnd);
    let mut a1 = a0;
    let mut a2 = a0;
    let mut a3 = a0;
    a0 = a0.wrapping_add(mul(w[2], x(2)));
    a1 = a1.wrapping_add(mul(w[6], x(2)));
    a2 = a2.wrapping_sub(mul(w[6], x(2)));
    a3 = a3.wrapping_sub(mul(w[2], x(2)));
    let mut b0 = mul(w[1], x(1)).wrapping_add(mul(w[3], x(3)));
    let mut b1 = mul(w[3], x(1)).wrapping_add(mul(-w[7], x(3)));
    let mut b2 = mul(w[5], x(1)).wrapping_add(mul(-w[1], x(3)));
    let mut b3 = mul(w[7], x(1)).wrapping_add(mul(-w[5], x(3)));
    a0 = a0.wrapping_add(mul(w[4], x(4))).wrapping_add(mul(w[6], x(6)));
    a1 = a1.wrapping_add(mul(-w[4], x(4))).wrapping_add(mul(-w[2], x(6)));
    a2 = a2.wrapping_add(mul(-w[4], x(4))).wrapping_add(mul(w[2], x(6)));
    a3 = a3.wrapping_add(mul(w[4], x(4))).wrapping_add(mul(-w[6], x(6)));
    b0 = b0.wrapping_add(mul(w[5], x(5))).wrapping_add(mul(w[7], x(7)));
    b1 = b1.wrapping_add(mul(-w[1], x(5))).wrapping_add(mul(-w[5], x(7)));
    b2 = b2.wrapping_add(mul(w[7], x(5))).wrapping_add(mul(w[3], x(7)));
    b3 = b3.wrapping_add(mul(w[3], x(5))).wrapping_add(mul(-w[1], x(7)));
    let out = |v: u32| (v as i32 >> s) as i16;
    r[0] = out(a0.wrapping_add(b0));
    r[7] = out(a0.wrapping_sub(b0));
    r[1] = out(a1.wrapping_add(b1));
    r[6] = out(a1.wrapping_sub(b1));
    r[2] = out(a2.wrapping_add(b2));
    r[5] = out(a2.wrapping_sub(b2));
    r[3] = out(a3.wrapping_add(b3));
    r[4] = out(a3.wrapping_sub(b3));
}

/// Column `i` of the block (entries `i`, `i + 8`, ..., `i + 56`), in place.
fn col(b: &mut [i16; 64], i: usize, w: &[i32; 8], col_shift: i32) {
    let c = |k: usize| b[i + 8 * k] as i32;
    let rnd = (1 << (col_shift - 1)) / w[4];
    let mut a0 = mul(w[4], c(0) + rnd);
    let mut a1 = a0;
    let mut a2 = a0;
    let mut a3 = a0;
    a0 = a0.wrapping_add(mul(w[2], c(2)));
    a1 = a1.wrapping_add(mul(w[6], c(2)));
    a2 = a2.wrapping_add(mul(-w[6], c(2)));
    a3 = a3.wrapping_add(mul(-w[2], c(2)));
    let mut b0 = mul(w[1], c(1)).wrapping_add(mul(w[3], c(3)));
    let mut b1 = mul(w[3], c(1)).wrapping_add(mul(-w[7], c(3)));
    let mut b2 = mul(w[5], c(1)).wrapping_add(mul(-w[1], c(3)));
    let mut b3 = mul(w[7], c(1)).wrapping_add(mul(-w[5], c(3)));
    a0 = a0.wrapping_add(mul(w[4], c(4)));
    a1 = a1.wrapping_add(mul(-w[4], c(4)));
    a2 = a2.wrapping_add(mul(-w[4], c(4)));
    a3 = a3.wrapping_add(mul(w[4], c(4)));
    b0 = b0.wrapping_add(mul(w[5], c(5)));
    b1 = b1.wrapping_add(mul(-w[1], c(5)));
    b2 = b2.wrapping_add(mul(w[7], c(5)));
    b3 = b3.wrapping_add(mul(w[3], c(5)));
    a0 = a0.wrapping_add(mul(w[6], c(6)));
    a1 = a1.wrapping_add(mul(-w[2], c(6)));
    a2 = a2.wrapping_add(mul(w[2], c(6)));
    a3 = a3.wrapping_add(mul(-w[6], c(6)));
    b0 = b0.wrapping_add(mul(w[7], c(7)));
    b1 = b1.wrapping_add(mul(-w[5], c(7)));
    b2 = b2.wrapping_add(mul(w[3], c(7)));
    b3 = b3.wrapping_add(mul(-w[1], c(7)));
    let out = |v: u32| ((v as i32) >> col_shift) as i16;
    b[i] = out(a0.wrapping_add(b0));
    b[i + 8] = out(a1.wrapping_add(b1));
    b[i + 16] = out(a2.wrapping_add(b2));
    b[i + 24] = out(a3.wrapping_add(b3));
    b[i + 32] = out(a3.wrapping_sub(b3));
    b[i + 40] = out(a2.wrapping_sub(b2));
    b[i + 48] = out(a1.wrapping_sub(b1));
    b[i + 56] = out(a0.wrapping_sub(b0));
}

/// Dequantises one 8x8 block (raster order) with `qmat`, inverse transforms it and writes the
/// clipped samples to `out`, `stride` samples apart. `twelve` selects the 12-bit transform
/// (4:4:4 ProRes), otherwise 10-bit.
pub fn put(block: &mut [i16; 64], qmat: &[i16; 64], twelve: bool, out: &mut [u16], stride: usize) {
    for (b, q) in block.iter_mut().zip(qmat) {
        *b = (*b as i32 * *q as i32) as i16;
    }
    let (w, row_shift, extra, dc, col_shift) = if twelve { (&W12, 16, 0, -1, 17) } else { (&W10, 13, 2, 1, 18) };
    for i in 0..8 {
        row(&mut block[i * 8..i * 8 + 8], w, row_shift, extra, dc);
    }
    for i in 0..8 {
        block[i] = block[i].wrapping_add(8192);
        col(block, i, w, col_shift);
    }
    let (lo, hi) = (4, if twelve { 4091 } else { 1019 });
    for y in 0..8 {
        for x in 0..8 {
            out[y * stride + x] = (block[y * 8 + x] as i32).clamp(lo, hi) as u16;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_dc_only_row_takes_the_shortcut_rounding() {
        // 12-bit: the shortcut gives (v + 1) >> 1, where the full transform gives (32767 v + 32768) >> 16
        let mut r = [0i16; 8];
        r[0] = 1;
        row(&mut r, &W12, 16, 0, -1);
        assert_eq!(r, [1; 8]);
        let mut r = [0i16; 8];
        r[0] = 1;
        row(&mut r, &W10, 13, 2, 1);
        assert_eq!(r, [1; 8]);
    }

    #[test]
    fn a_zero_block_is_mid_grey() {
        let mut b = [0i16; 64];
        let mut out = [0u16; 64];
        put(&mut b, &[4; 64], false, &mut out, 8);
        assert!(out.iter().all(|&v| v == 512));
        let mut b = [0i16; 64];
        put(&mut b, &[4; 64], true, &mut out, 8);
        assert!(out.iter().all(|&v| v == 2048));
    }
}
