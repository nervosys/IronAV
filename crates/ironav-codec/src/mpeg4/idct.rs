//! The 8-bit simple IDCT of FFmpeg (`simple_idct_template.c`, Michael Niedermayer and Roman
//! Shishkov, LGPL-2.1-or-later), which the MPEG-4 decoder selects with `-idct simple`. The output
//! matches FFmpeg bit for bit, which is why the rounding and the DC-only row shortcut are kept.

const W1: i32 = 22725;
const W2: i32 = 21407;
const W3: i32 = 19266;
const W4: i32 = 16383;
const W5: i32 = 12873;
const W6: i32 = 8867;
const W7: i32 = 4520;
const ROW_SHIFT: u32 = 11;
const COL_SHIFT: u32 = 20;

/// The row transform of one row of `block` (raster order, 8 coefficients per row), in place.
fn row(b: &mut [i16]) {
    if b[1..8].iter().all(|&v| v == 0) {
        let dc = (b[0] as i32 * 8) as i16;
        b[..8].fill(dc);
        return;
    }
    let r: [i32; 8] = std::array::from_fn(|i| b[i] as i32);
    let mut a0 = W4.wrapping_mul(r[0]).wrapping_add(1 << (ROW_SHIFT - 1));
    let mut a1 = a0;
    let mut a2 = a0;
    let mut a3 = a0;
    a0 = a0.wrapping_add(W2.wrapping_mul(r[2]));
    a1 = a1.wrapping_add(W6.wrapping_mul(r[2]));
    a2 = a2.wrapping_sub(W6.wrapping_mul(r[2]));
    a3 = a3.wrapping_sub(W2.wrapping_mul(r[2]));
    let mut b0 = W1.wrapping_mul(r[1]).wrapping_add(W3.wrapping_mul(r[3]));
    let mut b1 = W3.wrapping_mul(r[1]).wrapping_sub(W7.wrapping_mul(r[3]));
    let mut b2 = W5.wrapping_mul(r[1]).wrapping_sub(W1.wrapping_mul(r[3]));
    let mut b3 = W7.wrapping_mul(r[1]).wrapping_sub(W5.wrapping_mul(r[3]));
    a0 = a0.wrapping_add(W4.wrapping_mul(r[4]).wrapping_add(W6.wrapping_mul(r[6])));
    a1 = a1.wrapping_sub(W4.wrapping_mul(r[4])).wrapping_sub(W2.wrapping_mul(r[6]));
    a2 = a2.wrapping_sub(W4.wrapping_mul(r[4])).wrapping_add(W2.wrapping_mul(r[6]));
    a3 = a3.wrapping_add(W4.wrapping_mul(r[4]).wrapping_sub(W6.wrapping_mul(r[6])));
    b0 = b0.wrapping_add(W5.wrapping_mul(r[5]).wrapping_add(W7.wrapping_mul(r[7])));
    b1 = b1.wrapping_sub(W1.wrapping_mul(r[5])).wrapping_sub(W5.wrapping_mul(r[7]));
    b2 = b2.wrapping_add(W7.wrapping_mul(r[5]).wrapping_add(W3.wrapping_mul(r[7])));
    b3 = b3.wrapping_add(W3.wrapping_mul(r[5]).wrapping_sub(W1.wrapping_mul(r[7])));
    let o = [a0 + b0, a1 + b1, a2 + b2, a3 + b3, a3 - b3, a2 - b2, a1 - b1, a0 - b0];
    for i in 0..8 {
        b[i] = (o[i] >> ROW_SHIFT) as i16;
    }
}

/// The column transform of column `c` of `block`, returning the 8 unshifted-out values.
fn col(block: &[i16], c: usize) -> [i32; 8] {
    let k: [i32; 8] = std::array::from_fn(|i| block[i * 8 + c] as i32);
    let mut a0 = W4.wrapping_mul(k[0].wrapping_add((1 << (COL_SHIFT - 1)) / W4));
    let mut a1 = a0;
    let mut a2 = a0;
    let mut a3 = a0;
    a0 = a0.wrapping_add(W2.wrapping_mul(k[2]));
    a1 = a1.wrapping_add(W6.wrapping_mul(k[2]));
    a2 = a2.wrapping_add(-W6.wrapping_mul(k[2]));
    a3 = a3.wrapping_add(-W2.wrapping_mul(k[2]));
    let mut b0 = W1.wrapping_mul(k[1]);
    let mut b1 = W3.wrapping_mul(k[1]);
    let mut b2 = W5.wrapping_mul(k[1]);
    let mut b3 = W7.wrapping_mul(k[1]);
    b0 = b0.wrapping_add(W3.wrapping_mul(k[3]));
    b1 = b1.wrapping_add(-W7.wrapping_mul(k[3]));
    b2 = b2.wrapping_add(-W1.wrapping_mul(k[3]));
    b3 = b3.wrapping_add(-W5.wrapping_mul(k[3]));
    if k[4] != 0 {
        a0 = a0.wrapping_add(W4.wrapping_mul(k[4]));
        a1 = a1.wrapping_add(-W4.wrapping_mul(k[4]));
        a2 = a2.wrapping_add(-W4.wrapping_mul(k[4]));
        a3 = a3.wrapping_add(W4.wrapping_mul(k[4]));
    }
    if k[5] != 0 {
        b0 = b0.wrapping_add(W5.wrapping_mul(k[5]));
        b1 = b1.wrapping_add(-W1.wrapping_mul(k[5]));
        b2 = b2.wrapping_add(W7.wrapping_mul(k[5]));
        b3 = b3.wrapping_add(W3.wrapping_mul(k[5]));
    }
    if k[6] != 0 {
        a0 = a0.wrapping_add(W6.wrapping_mul(k[6]));
        a1 = a1.wrapping_add(-W2.wrapping_mul(k[6]));
        a2 = a2.wrapping_add(W2.wrapping_mul(k[6]));
        a3 = a3.wrapping_add(-W6.wrapping_mul(k[6]));
    }
    if k[7] != 0 {
        b0 = b0.wrapping_add(W7.wrapping_mul(k[7]));
        b1 = b1.wrapping_add(-W5.wrapping_mul(k[7]));
        b2 = b2.wrapping_add(W3.wrapping_mul(k[7]));
        b3 = b3.wrapping_add(-W1.wrapping_mul(k[7]));
    }
    [
        a0.wrapping_add(b0),
        a1.wrapping_add(b1),
        a2.wrapping_add(b2),
        a3.wrapping_add(b3),
        a3.wrapping_sub(b3),
        a2.wrapping_sub(b2),
        a1.wrapping_sub(b1),
        a0.wrapping_sub(b0),
    ]
}

/// Transforms the dequantised coefficients in `block` (raster order) into residuals, in place
/// in the sense that the result is returned as the 8x8 array of 32-bit column outputs.
fn transform(block: &mut [i16; 64]) -> [[i32; 8]; 8] {
    for i in 0..8 {
        row(&mut block[i * 8..i * 8 + 8]);
    }
    let mut out = [[0i32; 8]; 8];
    for c in 0..8 {
        let v = col(block, c);
        for r in 0..8 {
            out[r][c] = v[r] >> COL_SHIFT;
        }
    }
    out
}

/// Writes the IDCT of `block` to the 8x8 block at `dst` (the intra case: no prediction).
pub(crate) fn put(block: &mut [i16; 64], dst: &mut [u8], stride: usize) {
    let out = transform(block);
    for r in 0..8 {
        for c in 0..8 {
            dst[r * stride + c] = out[r][c].clamp(0, 255) as u8;
        }
    }
}

/// Adds the IDCT of `block` to the prediction already in `dst` (the inter case).
pub(crate) fn add(block: &mut [i16; 64], dst: &mut [u8], stride: usize) {
    let out = transform(block);
    for r in 0..8 {
        for c in 0..8 {
            let p = dst[r * stride + c] as i32;
            dst[r * stride + c] = (p + out[r][c]).clamp(0, 255) as u8;
        }
    }
}
