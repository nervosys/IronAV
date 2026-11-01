//! Block coding of the encoder's key frame: superblock partitions, intra mode info, transform
//! blocks and their coefficients, written with the decoder's contexts (spec 5.11 and 8.3). Blocks
//! are predicted, transformed and reconstructed with the decoder's own intra predictor and inverse
//! transform (`av1::ipred`, `av1::itx`), so the reconstruction the encoder keeps is the one the
//! decoder makes.
//!
//! Every block is one luma transform block of its own size (TX_MODE_LARGEST) and one transform
//! block per chroma plane. Partitions and modes are chosen by sum of absolute differences: the
//! partitions open-loop on the source, the modes closed-loop on the reconstruction.

use super::fwd::Bases;
use std::collections::HashMap;
use super::rc::SymbolEncoder;
use crate::av1::block::{base_ctx, br_ctx, partition_subsize, plane_block, scan_for, INTRA_MODE_CONTEXT};
use crate::av1::ipred::{predict_intra, Edges, Plane};
use crate::av1::itx::inverse_transform;
use crate::av1::quant_tables::{AC_QLOOKUP_8, DC_QLOOKUP_8};
use crate::av1::symbol::Cdfs;
use crate::av1::tables::*;

const DC_PRED: u8 = 0;
/// The last intra mode a block codes without CFL (CFL is never chosen by this encoder).
const MAX_MODE: u8 = 12;
const SMOOTH_PRED: u8 = 9;
const SMOOTH_H_PRED: u8 = 11;
/// The modes the open-loop partition search tries: DC, V, H, SMOOTH and PAETH.
const OPEN_MODES: [u8; 5] = [0, 1, 2, 9, 12];

/// What the encoder keeps of each 4x4 luma block for the contexts of later blocks.
#[derive(Clone, Copy, Default)]
struct Mi {
    size: u8,
    y_mode: u8,
    uv_mode: u8,
    skip: bool,
}

/// The quantised coefficients of one transform block: magnitudes and signs at each position of the
/// coefficient layout (`min(w, 32)` wide), and the transform type they were made for.
pub struct Coefs {
    tt: u8,
    any: bool,
    mags: Vec<u32>,
    negs: Vec<bool>,
}

/// The state of one frame's tile while it is coded.
pub struct FrameEnc {
    mi_rows: usize,
    mi_cols: usize,
    /// The reconstruction, which the encoder predicts from.
    pub planes: [Plane; 3],
    /// The source, extended past the picture to whole superblocks by repeating its last samples.
    src: [Plane; 3],
    mi: Vec<Mi>,
    above_lvl: [Vec<u8>; 3],
    above_dc: [Vec<u8>; 3],
    left_lvl: [Vec<u8>; 3],
    left_dc: [Vec<u8>; 3],
    /// Whether each 4x4 block of each plane has been coded (the availability of above-right and
    /// below-left samples), with the stride of `planes`.
    decoded: [Vec<bool>; 3],
    cdf: Box<Cdfs>,
    se: SymbolEncoder,
    bases: Bases,
    dcq: u32,
    acq: u32,
    /// Row, column and size of the block being coded.
    blk: (usize, usize, usize),
    /// The partition decided for each node (row, column, size) by `plan_node`.
    plan: HashMap<(usize, usize, usize), u8>,
}

fn bitlen(v: u32) -> usize {
    (32 - v.leading_zeros()) as usize
}

/// The dequantiser's extra shift for a transform of 2^(log2w + log2h) samples.
fn dq_shift(log2w: usize, log2h: usize) -> u32 {
    let area = 1usize << (log2w + log2h);
    (area > 256) as u32 + (area > 1024) as u32
}

/// The transform set of an intra block (get_tx_set) with reduced_tx_set off.
fn tx_set(tx: usize) -> u8 {
    if TX_SIZE_SQR_UP[tx] >= TX_32X32 {
        0
    } else if TX_SIZE_SQR[tx] == TX_16X16 {
        2
    } else {
        1
    }
}

impl FrameEnc {
    /// An encoder state for a frame of `mi_cols` x `mi_rows` mode units, with the source `y`, `u`,
    /// `v` planes (each `w` wide, tightly packed) extended to whole superblocks.
    pub fn new(mi_cols: usize, mi_rows: usize, qidx: usize, y: &[u8], u: &[u8], v: &[u8], pic: (usize, usize)) -> FrameEnc {
        let (aw, ah) = ((mi_cols * 4).div_ceil(64) * 64, (mi_rows * 4).div_ceil(64) * 64);
        let (cw, ch) = (pic.0.div_ceil(2), pic.1.div_ceil(2));
        let extend = |p: &[u8], pw: usize, ph: usize, bw: usize, bh: usize| -> Plane {
            let mut data = Vec::with_capacity(bw * bh);
            for yy in 0..bh {
                let row = &p[yy.min(ph - 1) * pw..][..pw];
                data.extend((0..bw).map(|xx| row[xx.min(pw - 1)] as u16));
            }
            Plane { data, stride: bw }
        };
        let src = [extend(y, pic.0, pic.1, aw, ah), extend(u, cw, ch, aw / 2, ah / 2), extend(v, cw, ch, aw / 2, ah / 2)];
        let planes = [Plane::new(aw, ah), Plane::new(aw / 2, ah / 2), Plane::new(aw / 2, ah / 2)];
        let decoded = [vec![false; (aw / 4) * (ah / 4)], vec![false; (aw / 8) * (ah / 8)], vec![false; (aw / 8) * (ah / 8)]];
        let ctx = || [vec![0u8; aw / 4 + 32], vec![0u8; aw / 4 + 32], vec![0u8; aw / 4 + 32]];
        let lctx = || [vec![0u8; ah / 4 + 32], vec![0u8; ah / 4 + 32], vec![0u8; ah / 4 + 32]];
        FrameEnc {
            mi_rows,
            mi_cols,
            planes,
            src,
            mi: vec![Mi::default(); mi_rows * mi_cols],
            above_lvl: ctx(),
            above_dc: ctx(),
            left_lvl: lctx(),
            left_dc: lctx(),
            decoded,
            cdf: Cdfs::new(qidx as i32),
            se: SymbolEncoder::new(true),
            bases: Bases::default(),
            dcq: DC_QLOOKUP_8[qidx] as u32,
            acq: AC_QLOOKUP_8[qidx] as u32,
            blk: (0, 0, 0),
            plan: HashMap::new(),
        }
    }

    /// Codes the tile: every superblock in raster order, and returns the tile's bytes.
    pub fn encode(mut self) -> (Vec<u8>, [Plane; 3]) {
        for r in (0..self.mi_rows).step_by(16) {
            for c in (0..self.mi_cols).step_by(16) {
                self.plan_node(r, c, BLOCK_64X64);
            }
        }
        for r in (0..self.mi_rows).step_by(16) {
            for p in 0..3 {
                self.left_lvl[p].fill(0);
                self.left_dc[p].fill(0);
            }
            for c in (0..self.mi_cols).step_by(16) {
                self.partition(r, c, BLOCK_64X64);
            }
        }
        (self.se.finish(), self.planes)
    }

    fn mi_at(&self, r: usize, c: usize) -> Mi {
        self.mi[r * self.mi_cols + c]
    }

    /// Whether the 4x4 block (`y4`, `x4`) of `plane` has been coded.
    fn is_decoded(&self, plane: usize, y4: isize, x4: isize) -> bool {
        let stride4 = self.planes[plane].stride / 4;
        if y4 < 0 || x4 < 0 || x4 as usize >= stride4 || y4 as usize >= self.planes[plane].data.len() / self.planes[plane].stride / 4 {
            return false;
        }
        self.decoded[plane][y4 as usize * stride4 + x4 as usize]
    }

    fn mark_decoded(&mut self, plane: usize, x: usize, y: usize, w: usize, h: usize) {
        let stride4 = self.planes[plane].stride / 4;
        for j in 0..h / 4 {
            for i in 0..w / 4 {
                self.decoded[plane][((y >> 2) + j) * stride4 + (x >> 2) + i] = true;
            }
        }
    }

    /// The intra edge information of a transform block at (`x`, `y`) of `plane`.
    fn edges(&self, plane: usize, x: usize, y: usize, log2w: u32, log2h: u32) -> Edges {
        let ss = (plane > 0) as usize;
        let (w4, h4) = ((1isize << log2w) >> 2, (1isize << log2h) >> 2);
        let (x4, y4) = ((x >> 2) as isize, (y >> 2) as isize);
        Edges {
            have_left: x > 0,
            have_above: y > 0,
            have_above_right: self.is_decoded(plane, y4 - 1, x4 + w4),
            have_below_left: self.is_decoded(plane, y4 + h4, x4 - 1),
            max_x: ((self.mi_cols * 4) >> ss) as i32 - 1,
            max_y: ((self.mi_rows * 4) >> ss) as i32 - 1,
        }
    }

    /// Whether a neighbour of the block at (`r`, `c`) uses a smooth predictor (get_filter_type).
    fn smooth_neighbour(&self, plane: usize, r: usize, c: usize) -> bool {
        let mode = |rr: usize, cc: usize| -> bool {
            let m = self.mi_at(rr, cc);
            let mode = if plane == 0 { m.y_mode } else { m.uv_mode };
            (SMOOTH_PRED..=SMOOTH_H_PRED).contains(&mode)
        };
        let mut s = false;
        if r > 0 {
            let (mut rr, mut cc) = (r - 1, c);
            if plane > 0 {
                if c & 1 == 0 {
                    cc += 1;
                }
                if r & 1 == 1 {
                    rr -= 1;
                }
            }
            s |= mode(rr, cc);
        }
        if c > 0 {
            let (mut rr, mut cc) = (r, c - 1);
            if plane > 0 {
                if c & 1 == 1 {
                    cc -= 1;
                }
                if r & 1 == 0 {
                    rr += 1;
                }
            }
            s |= mode(rr, cc);
        }
        s
    }

    /// Predicts the transform block at (`x`, `y`) of `plane` into the reconstruction.
    fn predict(&mut self, plane: usize, x: usize, y: usize, tx: usize, mode: u8, angle: i32) {
        let (log2w, log2h) = (TX_W_LOG2[tx] as u32, TX_H_LOG2[tx] as u32);
        let e = self.edges(plane, x, y, log2w, log2h);
        let smooth = self.smooth_neighbour(plane, self.blk.0, self.blk.1);
        predict_intra(&mut self.planes[plane], x, y, log2w, log2h, mode, angle, None, &e, smooth, true, 8);
    }

    /// The sum of absolute differences of the source and the prediction, over the planes `planes`.
    fn mode_cost(&mut self, planes: &[usize], x: usize, y: usize, tx: usize, mode: u8, angle: i32) -> u64 {
        let (w, h) = (1usize << TX_W_LOG2[tx], 1usize << TX_H_LOG2[tx]);
        let mut total = 0u64;
        for &p in planes {
            self.predict(p, x, y, tx, mode, angle);
            for j in 0..h {
                for i in 0..w {
                    total += (self.src[p].at(x + i, y + j) - self.planes[p].at(x + i, y + j)).unsigned_abs() as u64;
                }
            }
        }
        total
    }

    /// The intra mode and angle delta with the least difference: the modes, then the angle deltas
    /// of the best directional one.
    fn choose(&mut self, planes: &[usize], x: usize, y: usize, tx: usize) -> (u8, i32) {
        let mut best = (u64::MAX, DC_PRED, 0i32);
        for mode in 0..=MAX_MODE {
            let c = self.mode_cost(planes, x, y, tx, mode, 0);
            if c < best.0 {
                best = (c, mode, 0);
            }
        }
        let mode = best.1;
        if (1..=8).contains(&mode) {
            for angle in [-3, -2, -1, 1, 2, 3] {
                let c = self.mode_cost(planes, x, y, tx, mode, angle);
                if c < best.0 {
                    best = (c, mode, angle);
                }
            }
        }
        (best.1, best.2)
    }

    /// The transform type of a chroma block: the one its intra mode implies when the set has it.
    fn chroma_tt(&self, tx: usize, uv_mode: u8) -> u8 {
        let t = MODE_TO_TXFM[uv_mode as usize];
        if tx_type_in_intra_set(tx_set(tx), t) {
            t
        } else {
            DCT_DCT
        }
    }

    /// Predicts, transforms, quantises and reconstructs one transform block of `plane` at (`x`, `y`).
    fn code_plane(&mut self, plane: usize, x: usize, y: usize, tx: usize, mode: u8, angle: i32, tt: u8) -> Coefs {
        self.predict(plane, x, y, tx, mode, angle);
        let (log2w, log2h) = (TX_W_LOG2[tx] as usize, TX_H_LOG2[tx] as usize);
        let (w, h) = (1usize << log2w, 1usize << log2h);
        let (tw, th) = (w.min(32), h.min(32));
        let mut resid = vec![0i32; w * h];
        for j in 0..h {
            for i in 0..w {
                resid[j * w + i] = self.src[plane].at(x + i, y + j) - self.planes[plane].at(x + i, y + j);
            }
        }
        let mut coef = vec![0f64; tw * th];
        self.bases.get(tx, tt).forward(&resid, &mut coef);

        let shift = dq_shift(log2w, log2h);
        let scale = (1u64 << shift) as f64;
        let lim = 1i64 << (7 + 8);
        let mut mags = vec![0u32; tw * th];
        let mut negs = vec![false; tw * th];
        let mut dq = vec![0i32; tw * th];
        let mut any = false;
        for pos in 0..tw * th {
            let q = if pos == 0 { self.dcq } else { self.acq };
            let m = (coef[pos].abs() * scale / q as f64 + 0.5).floor().min((1 << 20) as f64) as u32;
            mags[pos] = m;
            negs[pos] = coef[pos] < 0.0;
            any |= m > 0;
            // the decoder's dequantisation of the level
            let d = ((m as u64 * q as u64) & 0xFF_FFFF) >> shift;
            let d = if negs[pos] { -(d as i64) } else { d as i64 };
            dq[pos] = d.clamp(-lim, lim - 1) as i32;
        }
        if any {
            let mut res = vec![0i32; w * h];
            inverse_transform(&dq, tx, tt, 8, false, &mut res);
            let p = &mut self.planes[plane];
            for j in 0..h {
                for i in 0..w {
                    let v = &mut p.data[(y + j) * p.stride + x + i];
                    *v = (*v as i32 + res[j * w + i]).clamp(0, 255) as u16;
                }
            }
        }
        Coefs { tt, any, mags, negs }
    }

    /// Decides the partition of every block of the frame, bottom-up over the source (open-loop): each
    /// node's decision is the cheapest of none, horizontal, vertical and split, and its cost is that
    /// decision's. Blocks at the picture's edge, and 64x64 blocks, always split.
    fn plan_node(&mut self, r: usize, c: usize, bs: usize) -> u64 {
        if r >= self.mi_rows || c >= self.mi_cols {
            return 0;
        }
        let half = NUM_4X4_W[bs] as usize / 2;
        let sub = partition_subsize(3, bs);
        if bs == BLOCK_8X8 {
            self.plan.insert((r, c, bs), 0);
            return self.open_cost(c * 4, r * 4, 8, 8);
        }
        let split: u64 = [(r, c), (r, c + half), (r + half, c), (r + half, c + half)].iter().map(|&(rr, cc)| self.plan_node(rr, cc, sub)).sum();
        let full = r + half < self.mi_rows && c + half < self.mi_cols;
        if !full || bs == BLOCK_64X64 {
            self.plan.insert((r, c, bs), 3);
            return split;
        }
        let (x, y) = (c * 4, r * 4);
        let (w, h) = (4 * NUM_4X4_W[bs] as usize, 4 * NUM_4X4_H[bs] as usize);
        let none = self.open_cost(x, y, w, h);
        let horz = self.open_cost(x, y, w, h / 2) + self.open_cost(x, y + h / 2, w, h / 2);
        let vert = self.open_cost(x, y, w / 2, h) + self.open_cost(x + w / 2, y, w / 2, h);
        let costs = [none, horz, vert, split];
        let p = (0..4).min_by_key(|&p| costs[p]).unwrap();
        self.plan.insert((r, c, bs), p as u8);
        costs[p]
    }

    /// The least sum of absolute differences of the block (x, y, w, h) over the intra modes, predicted
    /// from the source (open-loop) and with a penalty per block so that splitting is not free.
    fn open_cost(&self, x: usize, y: usize, w: usize, h: usize) -> u64 {
        let (log2w, log2h) = (w.trailing_zeros(), h.trailing_zeros());
        let mut best = u64::MAX;
        for mode in OPEN_MODES {
            let pred = self.open_pred(x, y, log2w, log2h, mode);
            let mut sad = 0u64;
            for j in 0..h {
                for i in 0..w {
                    sad += (self.src[0].at(x + i, y + j) - pred[j * w + i]).unsigned_abs() as u64;
                }
            }
            best = best.min(sad);
        }
        best + 2 * self.acq as u64
    }

    /// The prediction of a block from the source around it, built in a scratch plane so that the
    /// source itself is untouched.
    fn open_pred(&self, x: usize, y: usize, log2w: u32, log2h: u32, mode: u8) -> Vec<i32> {
        let (w, h) = (1usize << log2w, 1usize << log2h);
        let (lw, lh) = (2 * w + 2, 2 * h + 2);
        let s = &self.src[0];
        let mut data = vec![0u16; lw * lh];
        for j in 0..lh {
            let sy = (y + j).saturating_sub(1).min(s.data.len() / s.stride - 1);
            for i in 0..lw {
                let sx = (x + i).saturating_sub(1).min(s.stride - 1);
                data[j * lw + i] = s.data[sy * s.stride + sx];
            }
        }
        let mut lp = Plane { data, stride: lw };
        let e = Edges {
            have_left: x > 0,
            have_above: y > 0,
            have_above_right: y > 0,
            have_below_left: false,
            max_x: lw as i32 - 1,
            max_y: lh as i32 - 1,
        };
        predict_intra(&mut lp, 1, 1, log2w, log2h, mode, 0, None, &e, false, true, 8);
        (0..h).flat_map(|j| (0..w).map(move |i| (j, i))).map(|(j, i)| lp.data[(1 + j) * lw + 1 + i] as i32).collect()
    }

    /// Codes the partition of the block at (`r`, `c`) of size `bs`, and the blocks it holds.
    fn partition(&mut self, r: usize, c: usize, bs: usize) {
        if r >= self.mi_rows || c >= self.mi_cols {
            return;
        }
        let half = NUM_4X4_W[bs] as usize >> 1;
        let has_rows = r + half < self.mi_rows;
        let has_cols = c + half < self.mi_cols;
        let p = self.plan.get(&(r, c, bs)).map_or(3, |&p| p as usize);
        let bsl = MI_W_LOG2[bs] as usize;
        let above = r > 0 && (MI_W_LOG2[self.mi_at(r - 1, c).size as usize] as usize) < bsl;
        let left = c > 0 && (MI_H_LOG2[self.mi_at(r, c - 1).size as usize] as usize) < bsl;
        let ctx = left as usize * 2 + above as usize;
        if has_rows && has_cols {
            let cdf: &mut [u16] = match bsl {
                1 => &mut self.cdf.partition_w8[ctx],
                5 => &mut self.cdf.partition_w128[ctx],
                _ => &mut self.cdf.partition[(bsl - 2) * 4 + ctx],
            };
            self.se.symbol(p, cdf);
        } else if has_rows || has_cols {
            // split_or_horz and split_or_vert: a bool whose probability the partition CDF gives
            let cdf: &[u16] = match bsl {
                1 => &self.cdf.partition_w8[ctx],
                5 => &self.cdf.partition_w128[ctx],
                _ => &self.cdf.partition[(bsl - 2) * 4 + ctx],
            };
            let at = |i: usize| -> u32 {
                if i + 1 >= cdf.len() {
                    0
                } else {
                    cdf[i] as u32
                }
            };
            let prob = |e: usize| -> u32 { (if e > 0 { at(e - 1) } else { 32768 }) - at(e) };
            let set: &[usize] = if has_cols { &[2, 3, 4, 6, 7, 9] } else { &[1, 3, 4, 5, 6, 8] };
            let n = if bs == BLOCK_128X128 { 5 } else { 6 };
            let psum: u32 = set[..n].iter().map(|&e| prob(e)).sum();
            self.se.encode((p == 3) as usize, &[psum as u16, 0]);
        }
        let sub = partition_subsize(p, bs);
        let split = partition_subsize(3, bs);
        let _ = split;
        match p {
            0 => self.block(r, c, bs),
            1 => {
                self.block(r, c, sub);
                if has_rows {
                    self.block(r + half, c, sub);
                }
            }
            2 => {
                self.block(r, c, sub);
                if has_cols {
                    self.block(r, c + half, sub);
                }
            }
            _ => {
                self.partition(r, c, sub);
                self.partition(r, c + half, sub);
                self.partition(r + half, c, sub);
                self.partition(r + half, c + half, sub);
            }
        }
    }

    /// Codes one block: its modes, its transform blocks, and the mode info and coefficients as the
    /// decoder reads them.
    fn block(&mut self, r: usize, c: usize, bs: usize) {
        self.blk = (r, c, bs);
        let (x, y) = (c * 4, r * 4);
        let (bw4, bh4) = (NUM_4X4_W[bs] as usize, NUM_4X4_H[bs] as usize);
        let tx_y = MAX_TX_SIZE_RECT[bs];
        let (y_mode, ang_y) = self.choose(&[0], x, y, tx_y);
        let luma = self.code_plane(0, x, y, tx_y, y_mode, ang_y, DCT_DCT);
        self.mark_decoded(0, x, y, 4 * bw4, 4 * bh4);

        let (cx, cy) = (c * 2, r * 2);
        let tx_uv = MAX_TX_SIZE_RECT[plane_block(bs, 1, 1)];
        let (uv_mode, ang_uv) = self.choose(&[1, 2], cx, cy, tx_uv);
        let tt_uv = self.chroma_tt(tx_uv, uv_mode);
        let cb = self.code_plane(1, cx, cy, tx_uv, uv_mode, ang_uv, tt_uv);
        self.mark_decoded(1, cx, cy, 2 * bw4, 2 * bh4);
        let cr = self.code_plane(2, cx, cy, tx_uv, uv_mode, ang_uv, tt_uv);
        self.mark_decoded(2, cx, cy, 2 * bw4, 2 * bh4);

        let skip = !(luma.any || cb.any || cr.any);
        self.write_mode_info(r, c, bs, skip, y_mode, ang_y, uv_mode, ang_uv);
        if skip {
            for p in 0..3 {
                let ss = (p > 0) as usize;
                for i in (c >> ss)..((c + bw4) >> ss) {
                    self.above_lvl[p][i] = 0;
                    self.above_dc[p][i] = 0;
                }
                for i in (r >> ss)..((r + bh4) >> ss) {
                    self.left_lvl[p][i] = 0;
                    self.left_dc[p][i] = 0;
                }
            }
        } else {
            self.write_coefs(0, x, y, tx_y, &luma, y_mode);
            self.write_coefs(1, cx, cy, tx_uv, &cb, uv_mode);
            self.write_coefs(2, cx, cy, tx_uv, &cr, uv_mode);
        }

        let rows = bh4.min(self.mi_rows - r);
        let cols = bw4.min(self.mi_cols - c);
        for rr in r..r + rows {
            for cc in c..c + cols {
                self.mi[rr * self.mi_cols + cc] = Mi { size: bs as u8, y_mode, uv_mode, skip };
            }
        }
    }

    /// intra_frame_mode_info: the skip flag, the luma mode and angle delta, the chroma mode and angle delta.
    #[allow(clippy::too_many_arguments)]
    fn write_mode_info(&mut self, r: usize, c: usize, bs: usize, skip: bool, y_mode: u8, ang_y: i32, uv_mode: u8, ang_uv: i32) {
        let (avail_u, avail_l) = (r > 0, c > 0);
        let ctx = (avail_u && self.mi_at(r - 1, c).skip) as usize + (avail_l && self.mi_at(r, c - 1).skip) as usize;
        self.se.symbol(skip as usize, &mut self.cdf.skip[ctx]);
        let above = if avail_u { self.mi_at(r - 1, c).y_mode } else { DC_PRED };
        let left = if avail_l { self.mi_at(r, c - 1).y_mode } else { DC_PRED };
        self.se.symbol(y_mode as usize, &mut self.cdf.kf_y_mode[INTRA_MODE_CONTEXT[above as usize]][INTRA_MODE_CONTEXT[left as usize]]);
        if bs >= BLOCK_8X8 && (1..=8).contains(&y_mode) {
            self.se.symbol((ang_y + 3) as usize, &mut self.cdf.angle_delta[y_mode as usize - 1]);
        }
        self.se.symbol(uv_mode as usize, &mut self.cdf.uv_mode_cfl[y_mode as usize]);
        if bs >= BLOCK_8X8 && (1..=8).contains(&uv_mode) {
            self.se.symbol((ang_uv + 3) as usize, &mut self.cdf.angle_delta[uv_mode as usize - 1]);
        }
    }

    /// An Elias-gamma style code of `x` (at least 1), as the decoder reads a Golomb-coded level.
    fn golomb(&mut self, x: u32) {
        let n = bitlen(x);
        for _ in 0..n - 1 {
            self.se.bool(false);
        }
        self.se.bool(true);
        for i in (0..n - 1).rev() {
            self.se.bool((x >> i) & 1 == 1);
        }
    }

    /// The coefficients of a transform block at (`x`, `y`) of `plane` (coeffs in the decoder): the
    /// all-zero flag, the transform type, the end of block, the levels from the last coefficient
    /// back, then the signs and the Golomb-coded remainders of the large levels.
    fn write_coefs(&mut self, plane: usize, x: usize, y: usize, tx: usize, cf: &Coefs, y_mode: u8) {
        let (x4, y4) = (x >> 2, y >> 2);
        let (log2w, log2h) = (TX_W_LOG2[tx] as usize, TX_H_LOG2[tx] as usize);
        let (w4, h4) = (1usize << (log2w - 2), 1usize << (log2h - 2));
        let ss = (plane > 0) as usize;
        let (max_x4, max_y4) = (self.mi_cols >> ss, self.mi_rows >> ss);
        let tx_ctx = (TX_SIZE_SQR[tx] + TX_SIZE_SQR_UP[tx] + 1) >> 1;
        let ptype = (plane > 0) as usize;
        let bsize = plane_block(self.blk.2, ss, ss);
        let (bw, bh) = (4usize << MI_W_LOG2[bsize], 4usize << MI_H_LOG2[bsize]);
        let ax = x4..(x4 + w4).min(max_x4);
        let ly = y4..(y4 + h4).min(max_y4);
        let ctx = if plane == 0 {
            let top = ax.clone().map(|i| self.above_lvl[0][i]).max().unwrap_or(0);
            let left = ly.clone().map(|i| self.left_lvl[0][i]).max().unwrap_or(0);
            if bw == 1 << log2w && bh == 1 << log2h {
                0
            } else if top == 0 && left == 0 {
                1
            } else if top == 0 || left == 0 {
                2 + (top.max(left) > 3) as usize
            } else if top.max(left) <= 3 {
                4
            } else if top.min(left) <= 3 {
                5
            } else {
                6
            }
        } else {
            let above = ax.clone().any(|i| self.above_lvl[plane][i] | self.above_dc[plane][i] != 0);
            let left = ly.clone().any(|i| self.left_lvl[plane][i] | self.left_dc[plane][i] != 0);
            7 + above as usize + left as usize + if bw * bh > 1 << (log2w + log2h) { 3 } else { 0 }
        };
        self.se.symbol(!cf.any as usize, &mut self.cdf.txb_skip[tx_ctx][ctx]);

        let (mut cul, mut dc_cat) = (0u32, 0u8);
        if cf.any {
            let tt = cf.tt;
            let class = tx_class(tt);
            // luma's transform type is coded when its set has more than one type
            if plane == 0 {
                let set = tx_set(tx);
                if set == 1 {
                    let s = TX_TYPE_INTRA_INV_SET1.iter().position(|&t| t == tt).unwrap();
                    self.se.symbol(s, &mut self.cdf.intra_tx_1[TX_SIZE_SQR[tx]][y_mode as usize]);
                } else if set == 2 {
                    let s = TX_TYPE_INTRA_INV_SET2.iter().position(|&t| t == tt).unwrap();
                    self.se.symbol(s, &mut self.cdf.intra_tx_2[TX_SIZE_SQR[tx]][y_mode as usize]);
                }
            }
            let scan = scan_for(tx, tt);
            let mut eob = 0;
            for (ci, &pos) in scan.iter().enumerate() {
                if cf.mags[pos as usize] > 0 {
                    eob = ci + 1;
                }
            }
            let adj = ADJUSTED_TX_SIZE[tx];
            let bwl = TX_W_LOG2[adj] as usize;
            let txh = 1usize << TX_H_LOG2[adj];
            let n = txh << bwl;

            let eob_multi = log2w.min(5) + log2h.min(5) - 4;
            let ectx = (class != 0) as usize;
            let eob_pt = bitlen(eob as u32 - 1) + 1;
            let c = &mut self.cdf;
            let s = eob_pt - 1;
            match eob_multi {
                0 => self.se.symbol(s, &mut c.eob16[ptype][ectx]),
                1 => self.se.symbol(s, &mut c.eob32[ptype][ectx]),
                2 => self.se.symbol(s, &mut c.eob64[ptype][ectx]),
                3 => self.se.symbol(s, &mut c.eob128[ptype][ectx]),
                4 => self.se.symbol(s, &mut c.eob256[ptype][ectx]),
                5 => self.se.symbol(s, &mut c.eob512[ptype][ectx]),
                _ => self.se.symbol(s, &mut c.eob1024[ptype][ectx]),
            }
            if eob_pt >= 3 {
                let o = eob - ((1 << (eob_pt - 2)) + 1);
                self.se.symbol((o >> (eob_pt - 3)) & 1, &mut self.cdf.eob_extra[tx_ctx][ptype][eob_pt - 3]);
                for i in 1..eob_pt - 2 {
                    self.se.bool((o >> (eob_pt - 3 - i)) & 1 == 1);
                }
            }

            let mut quant = vec![0u32; n];
            let br_cdf = tx_ctx.min(TX_32X32);
            for ci in (0..eob).rev() {
                let pos = scan[ci] as usize;
                let m = cf.mags[pos];
                let mut level = m.min(3);
                if ci == eob - 1 {
                    let ctx = if ci == 0 {
                        0
                    } else if ci <= n / 8 {
                        1
                    } else if ci <= n / 4 {
                        2
                    } else {
                        3
                    };
                    self.se.symbol(level as usize - 1, &mut self.cdf.coeff_base_eob[tx_ctx][ptype][ctx]);
                } else {
                    let ctx = base_ctx(&quant, tx, class, bwl, txh, pos);
                    self.se.symbol(level as usize, &mut self.cdf.coeff_base[tx_ctx][ptype][ctx]);
                }
                if level > 2 {
                    let ctx = br_ctx(&quant, class, bwl, txh, pos);
                    let mut rem = m.min(15) - 3;
                    for _ in 0..4 {
                        let k = rem.min(3);
                        self.se.symbol(k as usize, &mut self.cdf.coeff_br[br_cdf][ptype][ctx]);
                        rem -= k;
                        level += k;
                        if k < 3 {
                            break;
                        }
                    }
                }
                quant[pos] = level;
            }

            let mut dc_sign = 0i32;
            for i in ax.clone() {
                match self.above_dc[plane][i] {
                    1 => dc_sign -= 1,
                    2 => dc_sign += 1,
                    _ => {}
                }
            }
            for i in ly.clone() {
                match self.left_dc[plane][i] {
                    1 => dc_sign -= 1,
                    2 => dc_sign += 1,
                    _ => {}
                }
            }
            let dc_ctx = match dc_sign {
                ..=-1 => 1,
                0 => 0,
                _ => 2,
            };
            for ci in 0..eob {
                let pos = scan[ci] as usize;
                if quant[pos] == 0 {
                    continue;
                }
                let m = cf.mags[pos];
                let sign = cf.negs[pos];
                if ci == 0 {
                    self.se.symbol(sign as usize, &mut self.cdf.dc_sign[ptype][dc_ctx]);
                } else {
                    self.se.bool(sign);
                }
                if quant[pos] > 14 {
                    self.golomb(m - 14);
                }
                cul += m;
                if pos == 0 {
                    dc_cat = if sign { 1 } else { 2 };
                }
            }
            cul = cul.min(63);
        }
        for i in x4..x4 + w4 {
            self.above_lvl[plane][i] = cul as u8;
            self.above_dc[plane][i] = dc_cat;
        }
        for i in y4..y4 + h4 {
            self.left_lvl[plane][i] = cul as u8;
            self.left_dc[plane][i] = dc_cat;
        }
    }
}
