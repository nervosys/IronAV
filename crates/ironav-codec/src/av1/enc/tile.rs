//! Block coding of the encoder's frames: superblock partitions, mode info of intra and inter
//! blocks, transform blocks and their coefficients, written with the decoder's contexts (spec 5.11
//! and 8.3).
//!
//! The encoder keeps a `FrameDec`, the decoder's own per-frame state, and drives it the way the
//! decoder's block loop does: `store_mi`, `find_mv_stack`, `predict_inter_block` and `save_mvs`
//! are the decoder's, and intra prediction and the inverse transform are `av1::ipred` and
//! `av1::itx`. So the reconstruction the encoder keeps (and the deblocking and CDEF run on it) is
//! the one the decoder makes. Only the symbol writing and the choices are the encoder's.
//!
//! Every block is one luma transform block of its own size (TX_MODE_LARGEST, at most 32x32) and
//! one transform block per chroma plane. Partitions are planned open-loop on the source, with
//! intra modes and a full-pel motion field from the previous source. Each block then chooses
//! between intra and inter by a rate-distortion cost: the squared error of the prediction plus
//! lambda times the bits that the mode syntax costs under the current CDFs. The inter candidates
//! are the reference MV stack's entries (NEARESTMV, NEARMV, GLOBALMV) and a NEWMV found by a
//! sub-pel search on the reconstructed reference.

use std::collections::HashMap;

use super::fwd::Bases;
use super::motion;
use super::rc::{sym_bits, SymbolEncoder};
use crate::av1::block::{base_ctx, br_ctx, partition_subsize, plane_block, scan_for, Block, FrameDec, Mi, COMPOUND_AVERAGE, DC_PRED, INTRA_MODE_CONTEXT};
use crate::av1::inter_tables::{TX_TYPE_INTER_INV_SET1, TX_TYPE_INTER_INV_SET2, TX_TYPE_INTER_INV_SET3};
use crate::av1::ipred::{predict_intra, Edges, Plane};
use crate::av1::itx::inverse_transform;
use crate::av1::mvpred::{Mv, MvStack, GLOBALMV, NEARESTMV, NEARMV, NEWMV};
use crate::av1::quant_tables::{AC_QLOOKUP_8, DC_QLOOKUP_8};
use crate::av1::symbol::Cdfs;
use crate::av1::tables::*;

const MAX_MODE: u8 = 12;
const SMOOTH_PRED: u8 = 9;
const SMOOTH_H_PRED: u8 = 11;
/// The modes the open-loop partition search tries: DC, V, H, SMOOTH and PAETH.
const OPEN_MODES: [u8; 5] = [0, 1, 2, 9, 12];

/// Lambda of the mode decisions, in squared samples per bit, from the AC quantiser step.
fn lambda_for(acq: u32) -> f64 {
    (acq as f64).powi(2) / 128.0
}

/// The quantised coefficients of one transform block: magnitudes and signs at each position of the
/// coefficient layout (`min(w, 32)` wide), and the transform type they were made for.
pub struct Coefs {
    tt: u8,
    any: bool,
    mags: Vec<u32>,
    negs: Vec<bool>,
}

/// An intra block's choice: the luma and chroma modes with their angle deltas.
#[derive(Clone, Copy)]
struct IntraChoice {
    y_mode: u8,
    ang_y: i32,
    uv_mode: u8,
    ang_uv: i32,
    cost: f64,
}

/// An inter block's choice: the mode, the reference MV stack index it codes against, the motion
/// vector (row, column, in 1/8 sample) and its cost.
#[derive(Clone, Copy)]
struct InterChoice {
    mode: u8,
    idx: usize,
    mv: Mv,
    cost: f64,
}

/// The state of one frame while it is coded.
pub struct FrameEnc {
    /// The decoder's state for the frame: its reconstruction, mode info and reference frames.
    pub f: FrameDec,
    mi_rows: usize,
    mi_cols: usize,
    key: bool,
    /// The source, extended past the picture to whole superblocks by repeating its last samples.
    src: [Plane; 3],
    /// The previous picture's source luma, the open-loop motion field's reference.
    prev: Option<Plane>,
    /// The open-loop full-pel motion (dy, dx) of each 8x8 block, `mi_rows / 2` x `mi_cols / 2`.
    field: Vec<(i32, i32)>,
    above_lvl: [Vec<u8>; 3],
    above_dc: [Vec<u8>; 3],
    left_lvl: [Vec<u8>; 3],
    left_dc: [Vec<u8>; 3],
    /// Whether each 4x4 block of each plane has been coded (the availability of above-right and
    /// below-left samples), with the stride of the plane.
    decoded: [Vec<bool>; 3],
    cdf: Box<Cdfs>,
    se: SymbolEncoder,
    bases: Bases,
    dcq: u32,
    acq: u32,
    lambda: f64,
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

/// The transform set (get_tx_set) with reduced_tx_set off: 0 DCT only, 1 and 2 the intra sets,
/// 3 to 5 the inter sets.
fn tx_set(tx: usize, is_inter: bool) -> u8 {
    if is_inter {
        if TX_SIZE_SQR_UP[tx] > TX_32X32 {
            0
        } else if TX_SIZE_SQR_UP[tx] == TX_32X32 {
            5
        } else if TX_SIZE_SQR[tx] == TX_16X16 {
            4
        } else {
            3
        }
    } else if TX_SIZE_SQR_UP[tx] >= TX_32X32 {
        0
    } else if TX_SIZE_SQR[tx] == TX_16X16 {
        2
    } else {
        1
    }
}

/// count_refs' helper of the reference contexts (get_count_ctx).
fn count_ctx(a: u32, b: u32) -> usize {
    (a >= b) as usize + (a > b) as usize
}

/// A motion vector's rough cost in bits: the joint and the two components.
fn mv_bits(d: Mv) -> f64 {
    let comp = |v: i32| if v == 0 { 0.5 } else { 4.0 + 2.0 * (v.unsigned_abs() as f64).log2() };
    1.5 + comp(d[0]) + comp(d[1])
}

/// The full-pel block nearest to a motion vector, in 1/8 sample units.
fn round8(m: Mv) -> Mv {
    let r = |v: i32| ((v as f64) / 8.0).round() as i32 * 8;
    [r(m[0]), r(m[1])]
}

impl FrameEnc {
    /// An encoder state for `f`, a frame of the decoder's state, with the source `y`, `u`, `v`
    /// planes (`pic` wide, tightly packed) and the previous picture's source luma.
    pub fn new(f: FrameDec, y: &[u8], u: &[u8], v: &[u8], pic: (usize, usize), prev: Option<Plane>) -> FrameEnc {
        let (mi_cols, mi_rows) = (f.h.mi_cols, f.h.mi_rows);
        let qidx = f.h.base_q_idx as usize;
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
        let key = f.h.frame_is_intra();
        let field = match (&prev, key) {
            (Some(p), false) => motion::field(&src[0], p, mi_rows / 2, mi_cols / 2),
            _ => vec![(0, 0); (mi_rows / 2) * (mi_cols / 2)],
        };
        let decoded = [vec![false; (aw / 4) * (ah / 4)], vec![false; (aw / 8) * (ah / 8)], vec![false; (aw / 8) * (ah / 8)]];
        let ctx = || [vec![0u8; aw / 4 + 32], vec![0u8; aw / 4 + 32], vec![0u8; aw / 4 + 32]];
        let lctx = || [vec![0u8; ah / 4 + 32], vec![0u8; ah / 4 + 32], vec![0u8; ah / 4 + 32]];
        let acq = AC_QLOOKUP_8[qidx] as u32;
        FrameEnc {
            f,
            mi_rows,
            mi_cols,
            key,
            src,
            prev,
            field,
            above_lvl: ctx(),
            above_dc: ctx(),
            left_lvl: lctx(),
            left_dc: lctx(),
            decoded,
            cdf: Cdfs::new(qidx as i32),
            se: SymbolEncoder::new(true),
            bases: Bases::default(),
            dcq: DC_QLOOKUP_8[qidx] as u32,
            acq,
            lambda: lambda_for(acq),
            blk: (0, 0, 0),
            plan: HashMap::new(),
        }
    }

    /// Codes the tile: every superblock in raster order. Returns the tile's bytes, the frame's
    /// decoder state (reconstructed, not yet filtered) and the source luma for the next frame.
    pub fn encode(mut self) -> (Vec<u8>, FrameDec, Plane) {
        self.f.tile = [0, self.mi_rows, 0, self.mi_cols];
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
        let FrameEnc { f, se, src, .. } = self;
        let [luma, _, _] = src;
        (se.finish(), f, luma)
    }

    fn mi_at(&self, r: usize, c: usize) -> Mi {
        self.f.mi[r * self.mi_cols + c]
    }

    /// Whether the 4x4 block (`y4`, `x4`) of `plane` has been coded.
    fn is_decoded(&self, plane: usize, y4: isize, x4: isize) -> bool {
        let stride4 = self.f.planes[plane].stride / 4;
        if y4 < 0 || x4 < 0 || x4 as usize >= stride4 || y4 as usize >= self.f.planes[plane].data.len() / self.f.planes[plane].stride / 4 {
            return false;
        }
        self.decoded[plane][y4 as usize * stride4 + x4 as usize]
    }

    fn mark_decoded(&mut self, plane: usize, x: usize, y: usize, w: usize, h: usize) {
        let stride4 = self.f.planes[plane].stride / 4;
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
    /// Chroma ignores inter neighbours.
    fn smooth_neighbour(&self, plane: usize, r: usize, c: usize) -> bool {
        let mode = |rr: usize, cc: usize| -> bool {
            let m = self.mi_at(rr, cc);
            if plane > 0 && m.ref_frame[0] > 0 {
                return false;
            }
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
        predict_intra(&mut self.f.planes[plane], x, y, log2w, log2h, mode, angle, None, &e, smooth, true, 8);
    }

    /// The squared error of the reconstruction against the source over a `w` x `h` block of `plane`.
    fn sse(&self, plane: usize, x: usize, y: usize, w: usize, h: usize) -> f64 {
        let (s, p) = (&self.src[plane], &self.f.planes[plane]);
        let mut total = 0f64;
        for j in 0..h {
            for i in 0..w {
                let d = (s.at(x + i, y + j) - p.at(x + i, y + j)) as f64;
                total += d * d;
            }
        }
        total
    }

    /// The squared error of the block (`x`, `y`, `w`, `h`) of luma, and of its chroma.
    fn sse_block(&self, x: usize, y: usize, w: usize, h: usize) -> f64 {
        self.sse(0, x, y, w, h) + self.sse(1, x / 2, y / 2, w / 2, h / 2) + self.sse(2, x / 2, y / 2, w / 2, h / 2)
    }

    /// The rate-distortion cost of predicting the `tx` block at (`x`, `y`) of `planes` with each
    /// mode of the given angle: the squared error plus lambda times the mode's bits.
    fn mode_cost(&mut self, planes: &[usize], x: usize, y: usize, tx: usize, mode: u8, angle: i32) -> f64 {
        let (w, h) = (1usize << TX_W_LOG2[tx], 1usize << TX_H_LOG2[tx]);
        let mut total = 0f64;
        for &p in planes {
            self.predict(p, x, y, tx, mode, angle);
            total += self.sse(p, x, y, w, h);
        }
        total
    }

    /// The intra mode and angle delta of least cost for the `tx` block at (`x`, `y`) of `planes`,
    /// given the bits of each luma or chroma mode (`mb`) and of each angle delta (`ab`).
    fn choose(&mut self, planes: &[usize], x: usize, y: usize, tx: usize, mb: &[f64; 13], ab: &[[f64; 7]; 13]) -> (u8, i32, f64) {
        let lam = self.lambda;
        let mut best = (f64::MAX, DC_PRED, 0i32);
        for mode in 0..=MAX_MODE {
            let c = self.mode_cost(planes, x, y, tx, mode, 0) + lam * (mb[mode as usize] + ab[mode as usize][3]);
            if c < best.0 {
                best = (c, mode, 0);
            }
        }
        let mode = best.1;
        if (1..=8).contains(&mode) {
            for angle in [-3, -2, -1, 1, 2, 3] {
                let c = self.mode_cost(planes, x, y, tx, mode, angle) + lam * (mb[mode as usize] + ab[mode as usize][(angle + 3) as usize]);
                if c < best.0 {
                    best = (c, mode, angle);
                }
            }
        }
        (best.1, best.2, best.0)
    }

    /// The bits of each intra mode (luma, or chroma given the luma mode `ymode`) and of each
    /// directional mode's angle deltas, under the current CDFs.
    fn mode_costs(&self, base: &Block, bs: usize, luma: bool, ymode: u8) -> ([f64; 13], [[f64; 7]; 13]) {
        let mut mb = [0f64; 13];
        let mut ab = [[0f64; 7]; 13];
        let (r, c) = (base.row, base.col);
        let above = if base.avail_u { self.mi_at(r - 1, c).y_mode } else { DC_PRED };
        let left = if base.avail_l { self.mi_at(r, c - 1).y_mode } else { DC_PRED };
        for m in 0..=MAX_MODE as usize {
            mb[m] = if !luma {
                sym_bits(&self.cdf.uv_mode_cfl[ymode as usize], m)
            } else if self.key {
                sym_bits(&self.cdf.kf_y_mode[INTRA_MODE_CONTEXT[above as usize]][INTRA_MODE_CONTEXT[left as usize]], m)
            } else {
                sym_bits(&self.cdf.y_mode[SIZE_GROUP[bs] as usize], m)
            };
        }
        if bs >= BLOCK_8X8 {
            for m in 1..=8 {
                for a in 0..7 {
                    ab[m][a] = sym_bits(&self.cdf.angle_delta[m - 1], a);
                }
            }
        }
        (mb, ab)
    }

    /// The block's base decoder state: its position, size and availability, an intra block with
    /// the transform size of the block (TX_MODE_LARGEST).
    fn base_block(&self, r: usize, c: usize, bs: usize) -> Block {
        let (ri, ci) = (r as isize, c as isize);
        let avail_u = self.f.inside(ri - 1, ci);
        let avail_l = self.f.inside(ri, ci - 1);
        Block {
            row: r,
            col: c,
            size: bs,
            bw4: NUM_4X4_W[bs] as usize,
            bh4: NUM_4X4_H[bs] as usize,
            has_chroma: true,
            avail_u,
            avail_l,
            avail_u_c: avail_u,
            avail_l_c: avail_l,
            ref_frame: [0, -1],
            compound_idx: 1,
            compound_type: COMPOUND_AVERAGE,
            tx: MAX_TX_SIZE_RECT[bs],
            ..Default::default()
        }
    }

    /// The best intra modes of the block at (`r`, `c`) of size `bs`, by cost on the prediction.
    fn intra_search(&mut self, r: usize, c: usize, bs: usize) -> IntraChoice {
        let (x, y, cx, cy) = (c * 4, r * 4, c * 2, r * 2);
        let tx_y = MAX_TX_SIZE_RECT[bs];
        let tx_uv = MAX_TX_SIZE_RECT[plane_block(bs, 1, 1)];
        let base = self.base_block(r, c, bs);
        let (yb, ya) = self.mode_costs(&base, bs, true, 0);
        let (ym, ay, lc) = self.choose(&[0], x, y, tx_y, &yb, &ya);
        let (ub, ua) = self.mode_costs(&base, bs, false, ym);
        let (um, au, cc) = self.choose(&[1, 2], cx, cy, tx_uv, &ub, &ua);
        IntraChoice { y_mode: ym, ang_y: ay, uv_mode: um, ang_uv: au, cost: lc + cc }
    }

    /// The transform type of a chroma block: the one its intra mode implies when the set has it.
    fn chroma_tt(&self, tx: usize, uv_mode: u8) -> u8 {
        let t = MODE_TO_TXFM[uv_mode as usize];
        if tx_type_in_intra_set(tx_set(tx, false), t) {
            t
        } else {
            DCT_DCT
        }
    }

    /// Quantises the residual of the `tx` block at (`x`, `y`) of `plane` (the prediction is in the
    /// reconstruction already), and adds the dequantised residual back into the reconstruction.
    fn residual(&mut self, plane: usize, x: usize, y: usize, tx: usize, tt: u8) -> Coefs {
        let (log2w, log2h) = (TX_W_LOG2[tx] as usize, TX_H_LOG2[tx] as usize);
        let (w, h) = (1usize << log2w, 1usize << log2h);
        let (tw, th) = (w.min(32), h.min(32));
        let mut resid = vec![0i32; w * h];
        for j in 0..h {
            for i in 0..w {
                resid[j * w + i] = self.src[plane].at(x + i, y + j) - self.f.planes[plane].at(x + i, y + j);
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
            let p = &mut self.f.planes[plane];
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

    /// The least cost of the block (x, y, w, h) on the source: the sum of absolute differences of
    /// the best intra mode (predicted from the source around it), or of the open-loop motion
    /// compensated block in an inter frame, with a penalty per block so that splitting is not free.
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
        if let (false, Some(prev)) = (self.key, &self.prev) {
            let (rows, cols) = (self.mi_rows / 2, self.mi_cols / 2);
            let (dy, dx) = self.field[(y / 8).min(rows - 1) * cols + (x / 8).min(cols - 1)];
            best = best.min(motion::sad(&self.src[0], prev, x, y, w, h, dx, dy) + 4);
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
        let e = Edges { have_left: x > 0, have_above: y > 0, have_above_right: y > 0, have_below_left: false, max_x: lw as i32 - 1, max_y: lh as i32 - 1 };
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

    /// The bits of coding the block as intra (the first) and as inter (the second): the is_inter flag.
    fn flag_bits(&self) -> (f64, f64) {
        let ctx = self.intra_inter_ctx();
        (sym_bits(&self.cdf.intra_inter[ctx], 0), sym_bits(&self.cdf.intra_inter[ctx], 1))
    }

    /// Codes one block: the decision between intra and inter, the mode info, the transform blocks
    /// and their coefficients, and the state the next blocks read.
    fn block(&mut self, r: usize, c: usize, bs: usize) {
        self.blk = (r, c, bs);
        let (x, y, cx, cy) = (c * 4, r * 4, c * 2, r * 2);
        let (bw4, bh4) = (NUM_4X4_W[bs] as usize, NUM_4X4_H[bs] as usize);
        let tx_y = MAX_TX_SIZE_RECT[bs];
        let tx_uv = MAX_TX_SIZE_RECT[plane_block(bs, 1, 1)];

        self.f.b = self.base_block(r, c, bs);
        let intra = self.intra_search(r, c, bs);
        let (bits_intra, bits_inter) = self.flag_bits();
        let inter = if self.key { None } else { self.inter_search(r, c, bs) };
        let intra_cost = intra.cost + self.lambda * bits_intra;
        let mut picked = None;
        let coefs = match inter {
            Some(ic) if ic.cost + self.lambda * bits_inter < intra_cost => {
                picked = Some(ic);
                self.set_inter_block(r, c, bs, ic);
                self.f.store_mi();
                self.f.predict_inter_block();
                let luma = self.residual(0, x, y, tx_y, DCT_DCT);
                self.mark_decoded(0, x, y, 4 * bw4, 4 * bh4);
                let cb = self.residual(1, cx, cy, tx_uv, DCT_DCT);
                let cr = self.residual(2, cx, cy, tx_uv, DCT_DCT);
                self.mark_decoded(1, cx, cy, 2 * bw4, 2 * bh4);
                self.mark_decoded(2, cx, cy, 2 * bw4, 2 * bh4);
                [luma, cb, cr]
            }
            _ => {
                self.set_intra_block(r, c, bs, intra);
                self.predict(0, x, y, tx_y, intra.y_mode, intra.ang_y);
                let luma = self.residual(0, x, y, tx_y, DCT_DCT);
                self.mark_decoded(0, x, y, 4 * bw4, 4 * bh4);
                let tt_uv = self.chroma_tt(tx_uv, intra.uv_mode);
                self.predict(1, cx, cy, tx_uv, intra.uv_mode, intra.ang_uv);
                let cb = self.residual(1, cx, cy, tx_uv, tt_uv);
                self.predict(2, cx, cy, tx_uv, intra.uv_mode, intra.ang_uv);
                let cr = self.residual(2, cx, cy, tx_uv, tt_uv);
                self.mark_decoded(1, cx, cy, 2 * bw4, 2 * bh4);
                self.mark_decoded(2, cx, cy, 2 * bw4, 2 * bh4);
                [luma, cb, cr]
            }
        };
        let skip = !(coefs[0].any || coefs[1].any || coefs[2].any);
        self.f.b.skip = skip;

        self.write_mode_info(r, c, bs, skip, picked);
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
            self.write_coefs(0, x, y, tx_y, &coefs[0]);
            self.write_coefs(1, cx, cy, tx_uv, &coefs[1]);
            self.write_coefs(2, cx, cy, tx_uv, &coefs[2]);
            let i = (r >> 4) * self.f.cdef_stride + (c >> 4);
            if self.f.cdef_idx[i] == -1 {
                self.f.cdef_idx[i] = 0;
            }
        }

        self.f.store_mi();
        if !self.key {
            self.f.save_mvs();
        }
        // the transform size of each plane, for the loop filter
        self.set_lf_tx(0, x, y, tx_y);
        self.set_lf_tx(1, cx, cy, tx_uv);
        self.set_lf_tx(2, cx, cy, tx_uv);
    }

    fn set_lf_tx(&mut self, plane: usize, x: usize, y: usize, tx: usize) {
        let ls = self.f.lf_stride[plane];
        for i in 0..(1usize << TX_H_LOG2[tx]) >> 2 {
            for j in 0..(1usize << TX_W_LOG2[tx]) >> 2 {
                if let Some(v) = self.f.lf_tx[plane].get_mut(((y >> 2) + i) * ls + (x >> 2) + j) {
                    *v = tx as u8;
                }
            }
        }
    }

    /// Sets the decoder's block state for an intra block with the modes of `ch`.
    fn set_intra_block(&mut self, r: usize, c: usize, bs: usize, ch: IntraChoice) {
        let mut b = self.base_block(r, c, bs);
        b.y_mode = ch.y_mode;
        b.angle_y = ch.ang_y;
        b.uv_mode = ch.uv_mode;
        b.angle_uv = ch.ang_uv;
        self.f.b = b;
    }

    /// Sets the decoder's block state for an inter block with the mode and motion of `ic`.
    fn set_inter_block(&mut self, r: usize, c: usize, bs: usize, ic: InterChoice) {
        let mut b = self.base_block(r, c, bs);
        b.is_inter = true;
        b.ref_frame = [1, -1];
        b.y_mode = ic.mode;
        b.mv = [ic.mv, [0, 0]];
        b.interp = [0, 0];
        self.f.b = b;
    }

    /// The above and left blocks' mode info, where they are available.
    fn above_mi(&self) -> Option<Mi> {
        let b = &self.f.b;
        b.avail_u.then(|| self.mi_at(b.row - 1, b.col))
    }

    fn left_mi(&self) -> Option<Mi> {
        let b = &self.f.b;
        b.avail_l.then(|| self.mi_at(b.row, b.col - 1))
    }

    /// The context of the is_inter flag (read_is_inter).
    fn intra_inter_ctx(&self) -> usize {
        let a = self.above_mi().map(|m| m.ref_frame[0] <= 0);
        let l = self.left_mi().map(|m| m.ref_frame[0] <= 0);
        match (a, l) {
            (Some(a), Some(l)) => {
                if a && l {
                    3
                } else {
                    (a || l) as usize
                }
            }
            (Some(x), None) | (None, Some(x)) => 2 * x as usize,
            _ => 0,
        }
    }

    /// The reference frames of the above and left blocks, counted (count_refs).
    fn ref_counts(&self) -> [u32; 8] {
        let mut n = [0; 8];
        for m in [self.above_mi(), self.left_mi()].into_iter().flatten() {
            for rf in m.ref_frame {
                if rf > 0 {
                    n[rf as usize] += 1;
                }
            }
        }
        n
    }

    /// The contexts of the single reference symbols for LAST_FRAME: the three symbols and their
    /// CDF indices (fwd_bwd, then the counts that the other two use).
    fn ref_ctxs(&self) -> [(usize, usize); 3] {
        let n = self.ref_counts();
        let fwd_bwd = count_ctx(n[1] + n[2] + n[3] + n[4], n[5] + n[6] + n[7]);
        [(fwd_bwd, 0), (count_ctx(n[1] + n[2], n[3] + n[4]), 2), (count_ctx(n[1], n[2]), 3)]
    }

    /// The bits of the reference frame symbols of an inter block (LAST_FRAME).
    fn ref_bits(&self) -> f64 {
        self.ref_ctxs().iter().map(|&(ctx, idx)| sym_bits(&self.cdf.single_ref[ctx][idx], 0)).sum()
    }

    /// The bits of the inter mode, the drl index and the motion vector difference of an inter block.
    fn inter_mode_bits(&self, st: &MvStack, ic: InterChoice) -> f64 {
        let c = &self.cdf;
        let mut bits = 0.0;
        if ic.mode == NEWMV {
            bits += sym_bits(&c.newmv[st.new_ctx], 0);
        } else {
            bits += sym_bits(&c.newmv[st.new_ctx], 1);
            if ic.mode == GLOBALMV {
                bits += sym_bits(&c.zeromv[st.zero_ctx], 0);
            } else {
                bits += sym_bits(&c.zeromv[st.zero_ctx], 1);
                bits += sym_bits(&c.refmv[st.ref_ctx], (ic.mode == NEARMV) as usize);
            }
        }
        if ic.mode == NEWMV || ic.mode == NEARMV {
            let s = if ic.mode == NEWMV { 0 } else { 1 };
            for i in s..s + 2 {
                if st.num > i + 1 {
                    let bit = (i != ic.idx) as usize;
                    bits += sym_bits(&c.drl[st.drl_ctx[i]], bit);
                    if bit == 0 {
                        break;
                    }
                }
            }
        }
        if ic.mode == NEWMV {
            let pos = if st.num <= 1 { 0 } else { ic.idx };
            let p = st.mv[pos][0];
            bits += mv_bits([ic.mv[0] - p[0], ic.mv[1] - p[1]]);
        }
        bits
    }

    /// The squared error and bits of predicting the block (`x`, `y`, `w`, `h`) with the inter mode
    /// `mode` and motion `mv`, with the decoder's block state set to it.
    fn eval_inter(&mut self, st: &MvStack, x: usize, y: usize, w: usize, h: usize, mode: u8, idx: usize, mv: Mv) -> InterChoice {
        self.f.b.y_mode = mode;
        self.f.b.mv = [mv, [0, 0]];
        self.f.store_mi();
        self.f.predict_inter_block();
        let ic = InterChoice { mode, idx, mv, cost: 0.0 };
        let d = self.sse_block(x, y, w, h);
        InterChoice { cost: d + self.lambda * self.inter_mode_bits(st, ic), ..ic }
    }

    /// The sum of absolute differences of the luma block (`x`, `y`, `w`, `h`) predicted with `mv`.
    fn luma_sad_at(&mut self, x: usize, y: usize, w: usize, h: usize, mv: Mv) -> u64 {
        self.f.b.mv = [mv, [0, 0]];
        self.f.store_mi();
        self.f.predict_inter_block();
        let (p, s) = (&self.f.planes[0], &self.src[0]);
        let mut total = 0u64;
        for j in 0..h {
            for i in 0..w {
                total += (s.at(x + i, y + j) - p.at(x + i, y + j)).unsigned_abs() as u64;
            }
        }
        total
    }

    /// A NEWMV search: from the best of the candidates (the open-loop motion, the predictor and
    /// zero), by the sum of absolute differences with a penalty on the difference from the
    /// predictor, down through whole, half and quarter samples.
    fn search(&mut self, x: usize, y: usize, w: usize, h: usize, pred: Mv, start: Mv) -> Mv {
        let pen = |m: Mv| -> u64 { 2 * ((m[0] - pred[0]).unsigned_abs() + (m[1] - pred[1]).unsigned_abs()) as u64 };
        let mut best = (u64::MAX, pred);
        for m in [round8(start), round8(pred), [0, 0]] {
            let c = self.luma_sad_at(x, y, w, h, m) + pen(m);
            if c < best.0 {
                best = (c, m);
            }
        }
        for step in [8, 4, 2] {
            for _ in 0..16 {
                let centre = best.1;
                let mut moved = false;
                for (dy, dx) in [(-1, 0), (1, 0), (0, -1), (0, 1), (-1, -1), (-1, 1), (1, -1), (1, 1)] {
                    let m = [centre[0] + dy * step, centre[1] + dx * step];
                    let c = self.luma_sad_at(x, y, w, h, m) + pen(m);
                    if c < best.0 {
                        best = (c, m);
                        moved = true;
                    }
                }
                if !moved {
                    break;
                }
            }
        }
        best.1
    }

    /// The best inter candidate of the block at (`r`, `c`) of size `bs`: GLOBALMV, NEARESTMV and
    /// NEARMV on the reference MV stack, and a NEWMV from a search, each by its cost.
    fn inter_search(&mut self, r: usize, c: usize, bs: usize) -> Option<InterChoice> {
        let (x, y) = (c * 4, r * 4);
        let (w, h) = (4 * NUM_4X4_W[bs] as usize, 4 * NUM_4X4_H[bs] as usize);
        self.f.b = self.base_block(r, c, bs);
        self.f.b.is_inter = true;
        self.f.b.ref_frame = [1, -1];
        self.f.b.y_mode = NEWMV;
        let st = self.f.find_mv_stack();

        // the open-loop motion of the block, in 1/8 sample units
        let (rows, cols) = (self.mi_rows / 2, self.mi_cols / 2);
        let (dy, dx) = self.field[(y / 8).min(rows - 1) * cols + (x / 8).min(cols - 1)];
        let start = [dy * 8, dx * 8];

        // the NEWMV's stack index is the candidate nearest the open-loop motion
        let mut nidx = 0;
        let mut nd = i64::MAX;
        for idx in 0..3 {
            if idx == 0 || idx < st.num {
                let m = st.mv[idx][0];
                let d = (m[0] as i64 - start[0] as i64).abs() + (m[1] as i64 - start[1] as i64).abs();
                if d < nd {
                    nd = d;
                    nidx = idx;
                }
            }
        }
        let pred = st.mv[nidx][0];
        let mv = self.search(x, y, w, h, pred, start);

        let mut cands: Vec<(u8, usize, Mv)> = vec![(GLOBALMV, 0, st.global[0]), (NEARESTMV, 0, st.mv[0][0])];
        for idx in 1..=3 {
            if idx == 1 || idx < st.num {
                cands.push((NEARMV, idx, st.mv[idx][0]));
            }
        }
        cands.push((NEWMV, nidx, mv));

        let ref_bits = self.ref_bits();
        let mut best: Option<InterChoice> = None;
        for (mode, idx, m) in cands {
            let mut ic = self.eval_inter(&st, x, y, w, h, mode, idx, m);
            ic.cost += self.lambda * ref_bits;
            if best.is_none_or(|b| ic.cost < b.cost) {
                best = Some(ic);
            }
        }
        best
    }

    /// Codes the mode info of the block: the skip flag, then the inter or intra mode info.
    fn write_mode_info(&mut self, r: usize, c: usize, bs: usize, skip: bool, inter: Option<InterChoice>) {
        let ctx = self.above_mi().is_some_and(|m| m.skip) as usize + self.left_mi().is_some_and(|m| m.skip) as usize;
        self.se.symbol(skip as usize, &mut self.cdf.skip[ctx]);
        let is_inter = self.f.b.is_inter;
        if !self.key {
            let ctx = self.intra_inter_ctx();
            self.se.symbol(is_inter as usize, &mut self.cdf.intra_inter[ctx]);
        }
        if is_inter {
            for (ctx, idx) in self.ref_ctxs() {
                self.se.symbol(0, &mut self.cdf.single_ref[ctx][idx]);
            }
            let st = self.f.find_mv_stack();
            let ic = inter.expect("an inter block has its choice");
            self.write_inter_mode(&st, ic);
            return;
        }
        let (y_mode, ang_y, uv_mode, ang_uv) = (self.f.b.y_mode, self.f.b.angle_y, self.f.b.uv_mode, self.f.b.angle_uv);
        if self.key {
            let above = if r > 0 { self.mi_at(r - 1, c).y_mode } else { DC_PRED };
            let left = if c > 0 { self.mi_at(r, c - 1).y_mode } else { DC_PRED };
            self.se.symbol(y_mode as usize, &mut self.cdf.kf_y_mode[INTRA_MODE_CONTEXT[above as usize]][INTRA_MODE_CONTEXT[left as usize]]);
        } else {
            self.se.symbol(y_mode as usize, &mut self.cdf.y_mode[SIZE_GROUP[bs] as usize]);
        }
        if bs >= BLOCK_8X8 && (1..=8).contains(&y_mode) {
            self.se.symbol((ang_y + 3) as usize, &mut self.cdf.angle_delta[y_mode as usize - 1]);
        }
        self.se.symbol(uv_mode as usize, &mut self.cdf.uv_mode_cfl[y_mode as usize]);
        if bs >= BLOCK_8X8 && (1..=8).contains(&uv_mode) {
            self.se.symbol((ang_uv + 3) as usize, &mut self.cdf.angle_delta[uv_mode as usize - 1]);
        }
    }

    /// The inter mode, the drl index and the motion vector difference (inter_block_mode_info).
    fn write_inter_mode(&mut self, st: &MvStack, ic: InterChoice) {
        let mode = ic.mode;
        self.se.symbol((mode != NEWMV) as usize, &mut self.cdf.newmv[st.new_ctx]);
        if mode != NEWMV {
            self.se.symbol((mode != GLOBALMV) as usize, &mut self.cdf.zeromv[st.zero_ctx]);
            if mode != GLOBALMV {
                self.se.symbol((mode == NEARMV) as usize, &mut self.cdf.refmv[st.ref_ctx]);
            }
        }
        if mode == NEWMV || mode == NEARMV {
            let s = if mode == NEWMV { 0 } else { 1 };
            for i in s..s + 2 {
                if st.num > i + 1 {
                    let bit = (i != ic.idx) as usize;
                    self.se.symbol(bit, &mut self.cdf.drl[st.drl_ctx[i]]);
                    if bit == 0 {
                        break;
                    }
                }
            }
        }
        if mode == NEWMV {
            let pos = if st.num <= 1 { 0 } else { ic.idx };
            let p = st.mv[pos][0];
            self.write_mv([ic.mv[0] - p[0], ic.mv[1] - p[1]]);
        }
    }

    /// A motion vector difference (read_mv): the joint, then each non-zero component.
    fn write_mv(&mut self, d: Mv) {
        let joint = 2 * (d[0] != 0) as usize + (d[1] != 0) as usize;
        self.se.symbol(joint, &mut self.cdf.mv[0].joint);
        if d[0] != 0 {
            self.write_mv_component(0, d[0]);
        }
        if d[1] != 0 {
            self.write_mv_component(1, d[1]);
        }
    }

    /// One component of a motion vector difference, in quarter samples (read_mv_component with
    /// high precision off): a sign, a class, and the integer and fraction bits of the magnitude.
    fn write_mv_component(&mut self, comp: usize, v: i32) {
        let sign = (v < 0) as usize;
        let x = v.unsigned_abs() - 1;
        let c = &mut self.cdf.mv[0].comp[comp];
        self.se.symbol(sign, &mut c.sign);
        if x < 16 {
            self.se.symbol(0, &mut c.class);
            let b0 = (x >> 3) as usize;
            let fr = ((x >> 1) & 3) as usize;
            self.se.symbol(b0, &mut c.class0);
            self.se.symbol(fr, &mut c.class0_fr[b0]);
        } else {
            let class = (31 - x.leading_zeros()) as usize - 3;
            let off = x - (1 << (class + 3));
            let d = off >> 3;
            let fr = ((off >> 1) & 3) as usize;
            self.se.symbol(class, &mut c.class);
            for i in 0..class {
                self.se.symbol(((d >> i) & 1) as usize, &mut c.bits[i]);
            }
            self.se.symbol(fr, &mut c.fr);
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
    fn write_coefs(&mut self, plane: usize, x: usize, y: usize, tx: usize, cf: &Coefs) {
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
            let is_inter = self.f.b.is_inter;
            let y_mode = self.f.b.y_mode as usize;
            // luma's transform type is coded when its set has more than one type
            if plane == 0 {
                let sqr = TX_SIZE_SQR[tx];
                match tx_set(tx, is_inter) {
                    1 => {
                        let s = TX_TYPE_INTRA_INV_SET1.iter().position(|&t| t == tt).unwrap();
                        self.se.symbol(s, &mut self.cdf.intra_tx_1[sqr][y_mode]);
                    }
                    2 => {
                        let s = TX_TYPE_INTRA_INV_SET2.iter().position(|&t| t == tt).unwrap();
                        self.se.symbol(s, &mut self.cdf.intra_tx_2[sqr][y_mode]);
                    }
                    3 => {
                        let s = TX_TYPE_INTER_INV_SET1.iter().position(|&t| t == tt).unwrap();
                        self.se.symbol(s, &mut self.cdf.inter_tx_1[sqr]);
                    }
                    4 => {
                        let s = TX_TYPE_INTER_INV_SET2.iter().position(|&t| t == tt).unwrap();
                        self.se.symbol(s, &mut self.cdf.inter_tx_2[sqr]);
                    }
                    5 => {
                        let s = TX_TYPE_INTER_INV_SET3.iter().position(|&t| t == tt).unwrap();
                        self.se.symbol(s, &mut self.cdf.inter_tx_3[sqr]);
                    }
                    _ => {}
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mv_components_are_quarter_samples() {
        assert_eq!(round8([13, -5]), [16, -8]);
        assert!(mv_bits([0, 0]) < mv_bits([64, 2]));
    }
}
