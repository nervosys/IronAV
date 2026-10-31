//! The 2D inverse transform (spec 7.13.3) over the one-dimensional transforms of `itx1d.rs`.

use super::itx1d::*;
use super::tables::*;

pub(super) fn clamp_value(value: i32, bit: usize) -> i32 {
    let max = ((1i64 << (bit - 1)) - 1) as i32;
    let min = (-(1i64 << (bit - 1))) as i32;
    value.clamp(min, max)
}

pub(super) fn half_btf(w0: i32, in0: i32, w1: i32, in1: i32, bit: usize) -> i32 {
    let result = (w0 as i64 * in0 as i64 + w1 as i64 * in1 as i64) as i32;
    round_shift(result, bit)
}

pub(super) fn round_shift(x: i32, bit: usize) -> i32 {
    if bit == 0 {
        x
    } else {
        (x + (1 << (bit - 1))) >> bit
    }
}

fn tx1d(kind: u8, n: u32, input: &[i32], output: &mut [i32], range: usize) {
    match (kind, n) {
        (0, 2) => av1_idct4(input, output, range),
        (0, 3) => av1_idct8(input, output, range),
        (0, 4) => av1_idct16(input, output, range),
        (0, 5) => av1_idct32(input, output, range),
        (0, 6) => av1_idct64(input, output, range),
        (1, 2) => av1_iadst4(input, output, range),
        (1, 3) => av1_iadst8(input, output, range),
        (1, 4) => av1_iadst16(input, output, range),
        (2, 2) => av1_iflipadst4(input, output, range),
        (2, 3) => av1_iflipadst8(input, output, range),
        (2, 4) => av1_iflipadst16(input, output, range),
        (3, 2) => av1_iidentity4(input, output, range),
        (3, 3) => av1_iidentity8(input, output, range),
        (3, 4) => av1_iidentity16(input, output, range),
        _ => av1_iidentity32(input, output, range),
    }
}

/// Inverse-transforms the dequantised coefficients `dq` (row-major, `min(w, 32)` wide) of a
/// transform of size `tx` and type `tx_type` into the residual `res` (row-major, `w` wide).
pub fn inverse_transform(dq: &[i32], tx: usize, tx_type: u8, bd: u32, lossless: bool, res: &mut [i32]) {
    let log2w = TX_W_LOG2[tx] as u32;
    let log2h = TX_H_LOG2[tx] as u32;
    let (w, h) = (1usize << log2w, 1usize << log2h);
    let (tw, th) = (w.min(32), h.min(32));
    let row_clamp = bd as usize + 8;
    let col_clamp = (bd as usize + 6).max(16);
    let (col_kind, row_kind) = TX_1D_KINDS[tx_type as usize];
    let rect = (log2w as i32 - log2h as i32).abs() == 1;
    let mut buf = vec![0i32; w * h];
    let mut out = [0i32; 64];
    for i in 0..h {
        let mut t = [0i32; 64];
        if i < th {
            for j in 0..tw {
                t[j] = dq[i * tw + j];
            }
        }
        if rect {
            for v in t.iter_mut().take(w) {
                *v = ((*v as i64 * 2896 + 2048) >> 12) as i32;
            }
        }
        if lossless {
            for v in t.iter_mut().take(4) {
                *v >>= 2;
            }
            av1_iwht4(&t, &mut out, 0);
        } else {
            for v in t.iter_mut().take(w) {
                *v = clamp_value(*v, row_clamp);
            }
            tx1d(row_kind, log2w, &t, &mut out, row_clamp);
        }
        let shift = TRANSFORM_ROW_SHIFT[tx] as usize;
        for j in 0..w {
            buf[i * w + j] = round_shift(out[j], shift);
        }
    }
    if !lossless {
        for v in buf.iter_mut() {
            *v = clamp_value(*v, col_clamp);
        }
    }
    for j in 0..w {
        let mut tc = [0i32; 64];
        for i in 0..h {
            tc[i] = buf[i * w + j];
        }
        if lossless {
            av1_iwht4(&tc, &mut out, 0);
            for i in 0..h {
                res[i * w + j] = out[i];
            }
        } else {
            tx1d(col_kind, log2h, &tc, &mut out, col_clamp);
            for i in 0..h {
                res[i * w + j] = round_shift(out[i], 4);
            }
        }
    }
}
