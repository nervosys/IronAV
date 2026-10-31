//! The syntax of a frame: each macroblock's modes and motion vectors in the first partition, and its
//! coefficient tokens in the token partition, written with the trees, probabilities and contexts the
//! decoder reads them with.

use super::boolenc::BoolEncoder;
use super::mb::Coded;
use crate::vp8::pred::{B_DC, B_HOR, B_TM, B_VERT, DC_PRED8, HOR_PRED8, VERT_PRED8};
use crate::vp8::tables::PRED4X4_PROB_INTRA;
use crate::vp8::{sort_near, Mb, Mv, Vp8Decoder, DCT_CAT_PROB, MODE_CONTEXTS, MODE_I4X4, MV_SPLIT, MV_ZERO, PRED16X16_PROB_INTRA, PRED16X16_TREE_INTER, PRED16X16_TREE_INTRA, PRED4X4_PROB_INTER, PRED4X4_TREE, PRED8X8C_PROB_INTRA, PRED8X8C_TREE, ZIGZAG};

/// The probabilities a frame's header carries: of a macroblock being skipped, intra, and last-frame.
#[derive(Clone, Copy)]
pub struct FrameProbs {
    pub skip: u8,
    pub intra: u8,
    pub last: u8,
}

/// Writes every macroblock's modes and motion vectors (the first partition), in raster order.
pub fn write_modes(e: &mut BoolEncoder, rec: &Vp8Decoder, key: bool, p: FrameProbs) {
    let (mbw, stride) = (rec.mbw, rec.mbw + 1);
    // the sub-block modes above each column and left of the row, as key frames predict them from
    let mut top = vec![B_DC; mbw * 4];
    for mb_y in 0..rec.mbh {
        let mut left = [B_DC; 4];
        for mb_x in 0..mbw {
            let idx = (mb_y + 1) * stride + mb_x + 1;
            let mb = &rec.mbs[idx];
            e.put(p.skip, mb.skip);
            if key {
                e.tree(&PRED16X16_TREE_INTRA, &PRED16X16_PROB_INTRA, mb.mode);
                if mb.mode == MODE_I4X4 {
                    for y in 0..4 {
                        for x in 0..4 {
                            let m = mb.i4[y * 4 + x];
                            e.tree(&PRED4X4_TREE, &PRED4X4_PROB_INTRA[top[mb_x * 4 + x] as usize][left[y] as usize], m);
                            left[y] = m;
                            top[mb_x * 4 + x] = m;
                        }
                    }
                } else {
                    let m = match mb.mode {
                        DC_PRED8 => B_DC,
                        VERT_PRED8 => B_VERT,
                        HOR_PRED8 => B_HOR,
                        _ => B_TM,
                    };
                    top[mb_x * 4..mb_x * 4 + 4].fill(m);
                    left = [m; 4];
                }
                e.tree(&PRED8X8C_TREE, &PRED8X8C_PROB_INTRA, mb.chroma_mode);
            } else {
                let inter = mb.ref_frame != 0;
                e.put(p.intra, inter);
                if inter {
                    e.put(p.last, false);
                    write_mv(e, rec, mb, idx, stride, (mb_x, mb_y));
                } else {
                    e.tree(&PRED16X16_TREE_INTER, &rec.probs.pred16x16, mb.mode);
                    if mb.mode == MODE_I4X4 {
                        for &m in &mb.i4 {
                            e.tree(&PRED4X4_TREE, &PRED4X4_PROB_INTER, m);
                        }
                    }
                    e.tree(&PRED8X8C_TREE, &rec.probs.pred8x8c, mb.chroma_mode);
                }
            }
        }
    }
}

/// An inter macroblock's mode and vector, as the decoder's `decode_mvs` reads them. The vector is
/// either a candidate (nearest, near) or the best candidate plus a coded difference.
fn write_mv(e: &mut BoolEncoder, rec: &Vp8Decoder, mb: &Mb, idx: usize, stride: usize, (mb_x, mb_y): (usize, usize)) {
    let (mut near, mut cnt) = rec.near_mvs(idx, stride, 1);
    sort_near(&mut near, &mut cnt);
    let clamp = |m: Mv| rec.clamp_mv(mb_x, mb_y, m);
    e.put(MODE_CONTEXTS[cnt[0]][0], mb.mode != MV_ZERO);
    if mb.mode == MV_ZERO {
        return;
    }
    let nearest = clamp(near[1]);
    e.put(MODE_CONTEXTS[cnt[1]][1], mb.mv != nearest);
    if mb.mv == nearest {
        return;
    }
    let nearc = clamp(near[2]);
    e.put(MODE_CONTEXTS[cnt[2]][2], mb.mv != nearc);
    if mb.mv == nearc {
        return;
    }
    // no macroblock is split, so the split context is zero
    e.put(MODE_CONTEXTS[0][3], false);
    let best = clamp(near[(cnt[1] >= cnt[0]) as usize]);
    write_mv_component(e, &rec.probs.mvc[0], mb.mv.y as i32 - best.y as i32);
    write_mv_component(e, &rec.probs.mvc[1], mb.mv.x as i32 - best.x as i32);
}

/// One motion vector difference component: the inverse of the decoder's `read_mv_component`.
fn write_mv_component(e: &mut BoolEncoder, p: &[u8; 19], v: i32) {
    let a = v.unsigned_abs();
    if a < 8 {
        e.put(p[0], false);
        let b2 = ((a >> 2) & 1) as usize;
        e.put(p[2], b2 == 1);
        let b1 = ((a >> 1) & 1) as usize;
        e.put(p[3 + 3 * b2], b1 == 1);
        e.put(p[3 + 3 * b2 + 1 + b1], a & 1 == 1);
    } else {
        e.put(p[0], true);
        for i in 0..3 {
            e.put(p[9 + i], (a >> i) & 1 == 1);
        }
        for i in (4..=9).rev() {
            e.put(p[9 + i], (a >> i) & 1 == 1);
        }
        // bit 3 is implicit when no higher bit is set: the value is then at least 8
        if a & 0xfff0 != 0 {
            e.put(p[12], (a >> 3) & 1 == 1);
        }
    }
    if a != 0 {
        e.put(p[1], v < 0);
    }
}

/// Writes the coefficient tokens of every macroblock (the token partition), in raster order.
pub fn write_tokens(e: &mut BoolEncoder, rec: &Vp8Decoder, mbs: &[(Mb, Coded)]) {
    let mut top = vec![[0u8; 9]; rec.mbw];
    for row in mbs.chunks(rec.mbw) {
        let mut left = [0u8; 9];
        for (x, (mb, coded)) in row.iter().enumerate() {
            write_mb_tokens(e, rec, mb, coded, &mut top[x], &mut left);
        }
    }
}

/// One macroblock's tokens with the decoder's contexts: `t` above (per column) and `l` to the left.
fn write_mb_tokens(e: &mut BoolEncoder, rec: &Vp8Decoder, mb: &Mb, coded: &Coded, t: &mut [u8; 9], l: &mut [u8; 9]) {
    if mb.skip {
        // a skipped macroblock clears the contexts, except the Y2 one when it has no Y2 block
        let keep = (mb.mode == MODE_I4X4 || mb.mode == MV_SPLIT).then(|| (l[8], t[8]));
        *l = [0; 9];
        *t = [0; 9];
        if let Some((lv, tv)) = keep {
            l[8] = lv;
            t[8] = tv;
        }
        return;
    }
    let probs = &rec.probs.token;
    let (mut first, mut kind) = (0usize, 3usize);
    if mb.mode != MODE_I4X4 && mb.mode != MV_SPLIT {
        let nz = write_block(e, &probs[1], 0, (t[8] + l[8]) as usize, &coded.y2);
        t[8] = nz as u8;
        l[8] = t[8];
        first = 1;
        kind = 0;
    }
    for y in 0..4 {
        for x in 0..4 {
            let nz = write_block(e, &probs[kind], first, (l[y] + t[x]) as usize, &coded.y[y * 4 + x]);
            t[x] = nz as u8;
            l[y] = t[x];
        }
    }
    for i in 4..6 {
        for y in 0..2 {
            for x in 0..2 {
                let nz = write_block(e, &probs[2], 0, (l[i + 2 * y] + t[i + 2 * x]) as usize, &coded.uv[(i - 4) * 4 + y * 2 + x]);
                t[i + 2 * x] = nz as u8;
                l[i + 2 * y] = t[i + 2 * x];
            }
        }
    }
}

/// One block's tokens from its quantised `levels` (raster order): the inverse of the decoder's
/// `block_coeffs`. Returns whether the block has any non-zero level.
fn write_block(e: &mut BoolEncoder, p: &[[[u8; 11]; 3]; 16], start: usize, ctx: usize, levels: &[i16; 16]) -> bool {
    let at = |i: usize| levels[ZIGZAG[i]] as i32;
    let Some(last) = (start..16).rev().find(|&i| at(i) != 0) else {
        e.put(p[start][ctx][0], false);
        return false;
    };
    e.put(p[start][ctx][0], true);
    let (mut i, mut c) = (start, ctx);
    loop {
        let tp = &p[i][c];
        let v = at(i);
        if v == 0 {
            // a zero token is never followed by an end-of-block check
            e.put(tp[1], false);
            i += 1;
            c = 0;
            continue;
        }
        e.put(tp[1], true);
        let m = v.abs();
        put_magnitude(e, tp, m);
        e.put(128, v < 0);
        c = if m == 1 { 1 } else { 2 };
        i += 1;
        if i == 16 {
            break;
        }
        if i > last {
            e.put(p[i][c][0], false);
            break;
        }
        e.put(p[i][c][0], true);
    }
    true
}

/// The token for a magnitude of at least one, with the node probabilities `tp`, and its extra bits.
fn put_magnitude(e: &mut BoolEncoder, tp: &[u8; 11], m: i32) {
    if m == 1 {
        e.put(tp[2], false);
        return;
    }
    e.put(tp[2], true);
    if m <= 4 {
        e.put(tp[3], false);
        if m == 2 {
            e.put(tp[4], false);
        } else {
            e.put(tp[4], true);
            e.put(tp[5], m == 4);
        }
    } else if m <= 10 {
        e.put(tp[3], true);
        e.put(tp[6], false);
        if m <= 6 {
            e.put(tp[7], false);
            e.put(159, m == 6);
        } else {
            e.put(tp[7], true);
            let d = m - 7;
            e.put(165, (d >> 1) & 1 == 1);
            e.put(145, d & 1 == 1);
        }
    } else {
        e.put(tp[3], true);
        e.put(tp[6], true);
        let (cat, base) = if m < 19 {
            (0usize, 11)
        } else if m < 35 {
            (1, 19)
        } else if m < 67 {
            (2, 35)
        } else {
            (3, 67)
        };
        e.put(tp[8], cat >> 1 == 1);
        e.put(tp[9 + (cat >> 1)], cat & 1 == 1);
        let bits = [3, 4, 5, 11][cat];
        let d = m - base;
        for (j, &prob) in DCT_CAT_PROB[cat].iter().enumerate() {
            e.put(prob, (d >> (bits - 1 - j)) & 1 == 1);
        }
    }
}
