//! Macroblock decisions: intra modes, whole-macroblock motion, and the forward transforms and
//! quantisation. Each macroblock is reconstructed into the frame as it is decided, with the
//! decoder's own prediction and inverse transforms, so the encoder's pictures are the decoder's.

use super::{Source, Vp8Encoder};
use crate::vp8::dsp::{self, RefPlane};
use crate::vp8::pred::{predict4x4, predict_block};
use crate::vp8::{sort_near, Mb, Mv, Picture, Quant, MODE_I4X4, MV_MV, MV_ZERO, SPLIT_NONE};
use ironav_core::Result;

/// The quantised levels of one macroblock, in raster order within each block. The Y2 block is the
/// luma DC; `uv` holds the four U blocks then the four V blocks.
#[derive(Default)]
pub struct Coded {
    pub y2: [i16; 16],
    pub y: [[i16; 16]; 16],
    pub uv: [[i16; 16]; 8],
}

/// How much an intra macroblock must beat the best inter one by, in summed absolute differences.
const INTRA_BIAS: u32 = 24;
/// The largest token value (DCT_CAT6 tops out at 67 + 2047).
const MAX_LEVEL: i32 = 2114;

fn sad_block(a: &[u8], astride: usize, b: &[u8], bstride: usize, w: usize, h: usize) -> u32 {
    (0..h).map(|r| a[r * astride..r * astride + w].iter().zip(&b[r * bstride..r * bstride + w]).map(|(&p, &q)| p.abs_diff(q) as u32).sum::<u32>()).sum()
}

/// Source minus prediction for the 4x4 block at (`x`, `y`).
fn residual(src: &[u8], ss: usize, pred: &[u8], ps: usize, x: usize, y: usize) -> [i32; 16] {
    let mut r = [0i32; 16];
    for j in 0..4 {
        for i in 0..4 {
            r[j * 4 + i] = src[(y + j) * ss + x + i] as i32 - pred[(y + j) * ps + x + i] as i32;
        }
    }
    r
}

/// The forward DCT, in the layout the inverse transform reads (vertical frequency first).
fn fdct(x: &[i32; 16]) -> [i32; 16] {
    let mut t = [0i32; 16];
    for i in 0..4 {
        let r = &x[i * 4..i * 4 + 4];
        let a1 = (r[0] + r[3]) * 8;
        let b1 = (r[1] + r[2]) * 8;
        let c1 = (r[1] - r[2]) * 8;
        let d1 = (r[0] - r[3]) * 8;
        t[i * 4] = a1 + b1;
        t[i * 4 + 2] = a1 - b1;
        t[i * 4 + 1] = (c1 * 2217 + d1 * 5352 + 14500) >> 12;
        t[i * 4 + 3] = (d1 * 2217 - c1 * 5352 + 7500) >> 12;
    }
    let mut o = [0i32; 16];
    for i in 0..4 {
        let a1 = t[i] + t[12 + i];
        let b1 = t[4 + i] + t[8 + i];
        let c1 = t[4 + i] - t[8 + i];
        let d1 = t[i] - t[12 + i];
        o[i] = (a1 + b1 + 7) >> 4;
        o[8 + i] = (a1 - b1 + 7) >> 4;
        o[4 + i] = ((c1 * 2217 + d1 * 5352 + 12000) >> 16) + (d1 != 0) as i32;
        o[12 + i] = (d1 * 2217 - c1 * 5352 + 51000) >> 16;
    }
    o
}

/// The forward Walsh-Hadamard transform of the luma DCs: the inverse one's matrix, halved.
fn fwht(x: &[i32; 16]) -> [i32; 16] {
    const H: [[i32; 4]; 4] = [[1, 1, 1, 1], [1, 1, -1, -1], [1, -1, -1, 1], [1, -1, 1, -1]];
    let mut t = [0i32; 16];
    for r in 0..4 {
        for c in 0..4 {
            t[r * 4 + c] = (0..4).map(|k| H[r][k] * x[k * 4 + c]).sum();
        }
    }
    let mut o = [0i32; 16];
    for r in 0..4 {
        for c in 0..4 {
            o[r * 4 + c] = (0..4).map(|k| t[r * 4 + k] * H[k][c]).sum::<i32>() / 2;
        }
    }
    o
}

/// Quantises a block with `q` = [DC, AC] factors, rounding to nearest. `skip_dc` leaves position 0 out.
fn quantize(c: &[i32; 16], q: [i32; 2], skip_dc: bool) -> [i16; 16] {
    let mut l = [0i16; 16];
    for k in (skip_dc as usize)..16 {
        let qk = q[(k != 0) as usize];
        let mag = ((c[k].abs() + qk / 2) / qk).min(MAX_LEVEL).min(32767 / qk);
        l[k] = (if c[k] < 0 { -mag } else { mag }) as i16;
    }
    l
}

/// The coefficients a decoder rebuilds from `levels`: exactly its arithmetic.
fn dequant(levels: &[i16; 16], q: [i32; 2]) -> [i16; 16] {
    let mut d = [0i16; 16];
    for k in 0..16 {
        d[k] = (levels[k] as i32 * q[(k != 0) as usize]) as i16;
    }
    d
}

/// Codes the luma of a macroblock predicted in `pred` with a Y2 block. Returns the Y2 and luma levels
/// and leaves the reconstruction in `pred`.
fn code_luma16(pred: &mut [u8], src: &[u8], ys: usize, mb_x: usize, mb_y: usize, q: &Quant) -> ([i16; 16], [[i16; 16]; 16]) {
    let (x0, y0) = (mb_x * 16, mb_y * 16);
    let pos = |b: usize| (x0 + 4 * (b % 4), y0 + 4 * (b / 4));
    let mut coef = [[0i32; 16]; 16];
    let mut dc = [0i32; 16];
    for (b, c) in coef.iter_mut().enumerate() {
        let (x, y) = pos(b);
        *c = fdct(&residual(src, ys, pred, ys, x, y));
        dc[b] = c[0];
    }
    let y2 = quantize(&fwht(&dc), q.y2, false);
    let y2d = dequant(&y2, q.y2);
    let lv: [[i16; 16]; 16] = std::array::from_fn(|b| quantize(&coef[b], q.y, true));
    let mut blocks: [[i16; 16]; 16] = std::array::from_fn(|b| dequant(&lv[b], q.y));
    dsp::luma_dc_wht(&mut blocks, &y2d);
    for (b, blk) in blocks.iter().enumerate() {
        let (x, y) = pos(b);
        dsp::idct_add(pred, y * ys + x, ys, blk);
    }
    (y2, lv)
}

/// Codes one chroma plane (four 4x4 blocks) predicted in `pred`, leaving the reconstruction there.
fn code_chroma(pred: &mut [u8], src: &[u8], cs: usize, mb_x: usize, mb_y: usize, q: [i32; 2]) -> [[i16; 16]; 4] {
    let mut lv = [[0i16; 16]; 4];
    for (b, l) in lv.iter_mut().enumerate() {
        let (x, y) = (mb_x * 8 + 4 * (b % 2), mb_y * 8 + 4 * (b / 2));
        *l = quantize(&fdct(&residual(src, cs, pred, cs, x, y)), q, false);
        dsp::idct_add(pred, y * cs + x, cs, &dequant(l, q));
    }
    lv
}

/// The 16x16 luma mode with the least error against the source, which is left predicted in `plane`.
fn best_intra16(plane: &mut [u8], src: &[u8], ys: usize, mb_x: usize, mb_y: usize) -> (u8, u32) {
    let (x0, y0) = (mb_x * 16, mb_y * 16);
    let mut best = (0u8, u32::MAX);
    for m in 0..4u8 {
        predict_block(plane, ys, x0, y0, 16, m, mb_x, mb_y);
        let s = sad_block(&src[y0 * ys + x0..], ys, &plane[y0 * ys + x0..], ys, 16, 16);
        if s < best.1 {
            best = (m, s);
        }
    }
    predict_block(plane, ys, x0, y0, 16, best.0, mb_x, mb_y);
    best
}

/// The chroma mode for both planes with the least error, left predicted in `u` and `v`.
fn best_chroma(u: &mut [u8], v: &mut [u8], src: (&[u8], &[u8]), cs: usize, mb_x: usize, mb_y: usize) -> u8 {
    let (x0, y0) = (mb_x * 8, mb_y * 8);
    let mut best = (0u8, u32::MAX);
    for m in 0..4u8 {
        predict_block(u, cs, x0, y0, 8, m, mb_x, mb_y);
        predict_block(v, cs, x0, y0, 8, m, mb_x, mb_y);
        let s = sad_block(&src.0[y0 * cs + x0..], cs, &u[y0 * cs + x0..], cs, 8, 8) + sad_block(&src.1[y0 * cs + x0..], cs, &v[y0 * cs + x0..], cs, 8, 8);
        if s < best.1 {
            best = (m, s);
        }
    }
    predict_block(u, cs, x0, y0, 8, best.0, mb_x, mb_y);
    predict_block(v, cs, x0, y0, 8, best.0, mb_x, mb_y);
    best.0
}

/// Sum of absolute differences of the macroblock at (`x0`, `y0`) against the luma reference `r`
/// displaced by `mv` (quarter pixels), with the decoder's six-tap filter.
fn sad_mv(src: &[u8], ys: usize, x0: usize, y0: usize, r: &RefPlane, mv: Mv) -> u32 {
    let mut blk = [0u8; 256];
    let (mx, my) = (((mv.x as i32 * 2) & 7) as usize, ((mv.y as i32 * 2) & 7) as usize);
    let x = x0 as i64 + ((mv.x as i32) >> 2) as i64;
    let y = y0 as i64 + ((mv.y as i32) >> 2) as i64;
    dsp::mc(&mut blk, 0, 16, r, x, y, mx, my, 16, 16, false);
    sad_block(&src[y0 * ys + x0..], ys, &blk, 16, 16, 16)
}

/// Finds a motion vector for the macroblock at (`x0`, `y0`): the best of `starts` (whole pixels) and
/// zero, refined by a diamond search and then to quarter pixels. Vectors stay within reach of the
/// coded difference from `base`.
fn search(src: &[u8], ys: usize, (x0, y0): (usize, usize), r: &RefPlane, base: Mv, starts: [Mv; 3]) -> (Mv, u32) {
    let ok = |m: Mv| (m.x as i32 - base.x as i32).abs() <= 1000 && (m.y as i32 - base.y as i32).abs() <= 1000;
    let sad = |m: Mv| sad_mv(src, ys, x0, y0, r, m);
    let mut best = (Mv::default(), sad(Mv::default()));
    for s in starts {
        let m = Mv { x: (((s.x as i32) + 2) >> 2 << 2) as i16, y: (((s.y as i32) + 2) >> 2 << 2) as i16 };
        if ok(m) {
            let c = sad(m);
            if c < best.1 {
                best = (m, c);
            }
        }
    }
    for _ in 0..64 {
        let c0 = best.0;
        let mut moved = false;
        for (dx, dy) in [(4, 0), (-4, 0), (0, 4), (0, -4)] {
            let m = Mv { x: c0.x + dx, y: c0.y + dy };
            if ok(m) {
                let c = sad(m);
                if c < best.1 {
                    best = (m, c);
                    moved = true;
                }
            }
        }
        if !moved {
            break;
        }
    }
    for step in [2i16, 1] {
        let c0 = best.0;
        for dy in -1..=1 {
            for dx in -1..=1 {
                let m = Mv { x: c0.x + dx * step, y: c0.y + dy * step };
                if (dx, dy) != (0, 0) && ok(m) {
                    let c = sad(m);
                    if c < best.1 {
                        best = (m, c);
                    }
                }
            }
        }
    }
    best
}

impl Vp8Encoder {
    /// The best inter prediction for a macroblock: the mode (zero or whole-macroblock motion), its
    /// vector and its cost. None when there is no reference frame yet.
    fn inter_choice(&self, src: &[u8], mb_x: usize, mb_y: usize, idx: usize, stride: usize) -> Option<(u8, Mv, u32)> {
        let refp = self.rec.refs[1].as_deref()?;
        let (ys, h) = (self.mbw * 16, self.mbh * 16);
        let r = RefPlane { data: &refp.y, stride: ys, w: ys, h };
        let (mut near, mut cnt) = self.rec.near_mvs(idx, stride, 1);
        sort_near(&mut near, &mut cnt);
        let clamp = |m: Mv| self.rec.clamp_mv(mb_x, mb_y, m);
        let (nearest, nearc) = (clamp(near[1]), clamp(near[2]));
        let base = clamp(near[(cnt[1] >= cnt[0]) as usize]);
        let (x0, y0) = (mb_x * 16, mb_y * 16);
        let sad = |m: Mv| sad_mv(src, ys, x0, y0, &r, m);
        let (mv, s) = search(src, ys, (x0, y0), &r, base, [base, nearest, nearc]);
        let new_cost = s + 8 + ((mv.x as i32 - base.x as i32).unsigned_abs() + (mv.y as i32 - base.y as i32).unsigned_abs()) / 2;
        let options = [(MV_ZERO, Mv::default(), sad(Mv::default())), (MV_MV, nearest, sad(nearest) + 2), (MV_MV, nearc, sad(nearc) + 4), (MV_MV, mv, new_cost)];
        options.into_iter().min_by_key(|o| o.2)
    }

    /// Intra 4x4 coding: each sub-block takes the mode with the least error, is reconstructed before the
    /// next one is predicted, and its levels are kept. Returns the modes, levels and error.
    fn code_bpred(&self, cur_y: &mut [u8], src: &[u8], mb_x: usize, mb_y: usize, q: [i32; 2]) -> ([u8; 16], [[i16; 16]; 16], u32) {
        let ys = self.mbw * 16;
        let mut modes = [0u8; 16];
        let mut lv = [[0i16; 16]; 16];
        let mut total = 0u32;
        for y in 0..4 {
            for x in 0..4 {
                let (px, py) = (mb_x * 16 + 4 * x, mb_y * 16 + 4 * y);
                let (a, l, tl) = self.rec.subblock_edges(cur_y, mb_x, mb_y, x, y);
                let mut best = (0u8, [0u8; 16], u32::MAX);
                for m in 0..10u8 {
                    let p = predict4x4(m, a, l, tl);
                    let s = sad_block(&src[py * ys + px..], ys, &p, 4, 4, 4);
                    if s < best.2 {
                        best = (m, p, s);
                    }
                }
                total += best.2;
                modes[y * 4 + x] = best.0;
                for r in 0..4 {
                    cur_y[(py + r) * ys + px..(py + r) * ys + px + 4].copy_from_slice(&best.1[r * 4..r * 4 + 4]);
                }
                let b = y * 4 + x;
                lv[b] = quantize(&fdct(&residual(src, ys, cur_y, ys, px, py)), q, false);
                dsp::idct_add(cur_y, py * ys + px, ys, &dequant(&lv[b], q));
            }
        }
        (modes, lv, total)
    }

    /// Decides and reconstructs one macroblock of the picture `cur`, which the decoder will see
    /// before its loop filter. Returns the macroblock (its skip flag set when nothing is coded) and
    /// its levels.
    pub(super) fn code_mb(&self, cur: &mut Picture, src: &Source, key: bool, mb_x: usize, mb_y: usize, q: &Quant) -> Result<(Mb, Coded)> {
        let (ys, cs) = (self.mbw * 16, self.mbw * 8);
        let stride = self.mbw + 1;
        let idx = (mb_y + 1) * stride + mb_x + 1;
        let mut mb = Mb { partitioning: SPLIT_NONE, ..Mb::default() };
        let mut coded = Coded::default();
        let inter = if key { None } else { self.inter_choice(&src.y, mb_x, mb_y, idx, stride) };
        // the best intra mode is needed to choose against inter; its prediction is then in the luma
        let intra16 = inter.is_some().then(|| best_intra16(&mut cur.y, &src.y, ys, mb_x, mb_y));
        let inter = inter.filter(|&(_, _, cost)| cost <= intra16.map_or(u32::MAX, |(_, s)| s).saturating_add(INTRA_BIAS));
        match inter {
            Some((mode, mv, _)) => {
                mb.mode = mode;
                mb.ref_frame = 1;
                mb.mv = mv;
                mb.bmv[0] = mv;
                self.rec.inter_predict(cur, &mb, mb_x, mb_y, 0)?;
                let (y2, lv) = code_luma16(&mut cur.y, &src.y, ys, mb_x, mb_y, q);
                coded.y2 = y2;
                coded.y = lv;
            }
            None => {
                let (m16, sad16) = intra16.unwrap_or_else(|| best_intra16(&mut cur.y, &src.y, ys, mb_x, mb_y));
                let (bmodes, blv, sadb) = self.code_bpred(&mut cur.y, &src.y, mb_x, mb_y, q.y);
                if sadb < sad16 {
                    mb.mode = MODE_I4X4;
                    mb.i4 = bmodes;
                    coded.y = blv;
                } else {
                    mb.mode = m16;
                    predict_block(&mut cur.y, ys, mb_x * 16, mb_y * 16, 16, m16, mb_x, mb_y);
                    let (y2, lv) = code_luma16(&mut cur.y, &src.y, ys, mb_x, mb_y, q);
                    coded.y2 = y2;
                    coded.y = lv;
                }
                mb.chroma_mode = best_chroma(&mut cur.u, &mut cur.v, (&src.u[..], &src.v[..]), cs, mb_x, mb_y);
            }
        }
        let lu = code_chroma(&mut cur.u, &src.u, cs, mb_x, mb_y, q.uv);
        let lw = code_chroma(&mut cur.v, &src.v, cs, mb_x, mb_y, q.uv);
        coded.uv[..4].copy_from_slice(&lu);
        coded.uv[4..].copy_from_slice(&lw);
        let nonzero = coded.y2.iter().any(|&v| v != 0) || coded.y.iter().flatten().any(|&v| v != 0) || coded.uv.iter().flatten().any(|&v| v != 0);
        mb.skip = !nonzero;
        Ok((mb, coded))
    }
}
