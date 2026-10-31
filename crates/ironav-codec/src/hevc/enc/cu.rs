//! One slice of the encoder: the coding quadtree decisions (rate-distortion cost by the
//! reconstruction itself), intra and inter prediction, quantisation, and the CABAC syntax of the
//! coding tree. Predictions, transforms and the reconstruction are the decoder's, so the encoder's
//! pictures are the decoder's.

use super::super::frame::{CurPic, Frame, Geometry, MvField, EDGE_TU_H, EDGE_TU_V, PRED_INTER, PRED_INTRA};
use super::super::inter::{chroma_mc, luma_mc, scale_mv};
use super::super::intra;
use super::super::ps::{Pps, Sps};
use super::super::tables::*;
use super::super::transform::{dequantise, inverse, DST4, LEVEL_SCALE};
use super::cabac::CabacEnc;
use super::syntax::{residual, LOG2_CTB, LOG2_MIN_CB};
use crate::h264::enc::bitw::BitWriter;
use std::ops::Range;

/// The padded source planes (luma, then the two chroma planes at half size).
pub struct Source {
    pub planes: [Vec<u16>; 3],
}

/// The quantised levels of one transform block (row-major), and whether any is non-zero.
#[derive(Clone, Default)]
pub struct Blk {
    pub cbf: bool,
    pub lv: Vec<i32>,
}

/// One coding unit: its prediction and its residual, as the syntax carries them.
pub struct Cu {
    pub log2: u32,
    pub intra: bool,
    /// Intra 8x8 units code four 4x4 luma transform blocks.
    pub split_tu: bool,
    pub prev_flag: bool,
    pub mpm_idx: u32,
    pub rem: u32,
    pub luma_mode: u32,
    pub chroma_idc: u32,
    pub chroma_mode: u32,
    pub mvp: usize,
    pub mvd: [i32; 2],
    /// A skipped unit (merge, no residual), or a merged unit with its candidate index.
    pub skip: bool,
    pub merge_idx: Option<u32>,
    pub luma: Vec<Blk>,
    pub cb: Blk,
    pub cr: Blk,
}

pub enum Node {
    Split(Vec<Node>),
    Leaf(Box<Cu>),
}

/// The per-4x4 state of a region, saved while a candidate is tried.
#[derive(Clone, Copy)]
struct Meta {
    pred: u8,
    skip: u8,
    depth: u8,
    ipm: u8,
    qp: i8,
    nz: u8,
    edges: u8,
    mv: MvField,
}

struct Saved {
    planes: [Vec<u16>; 3],
    meta: Vec<Meta>,
}

/// A rate term: the bits a candidate costs, weighted by lambda into the distortion's units.
const SIDE_INTRA: f64 = 10.0;
const SIDE_INTER: f64 = 6.0;
/// Scales HM's lambda (for squared error) up: the decisions measure absolute differences, and the
/// bits are estimates, so a stronger rate weight measures smaller at equal PSNR.
const LAMBDA_SCALE: f64 = 2.0;

/// Chroma quantisation parameter for a luma one (Table 8-10, 8-bit 4:2:0).
pub fn chroma_qp(qp: i32) -> i32 {
    let q = qp.clamp(0, 57);
    if q < 30 {
        q
    } else if q >= 44 {
        q - 6
    } else {
        QP_C_TABLE[(q - 30) as usize] as i32
    }
}

/// The forward transform of an `n` x `n` residual, the inverse of `inverse` up to scale.
fn forward(r: &[f32], log2: u32, dst: bool) -> Vec<f32> {
    let n = 1usize << log2;
    let step = 32 >> log2;
    // m[j][i]: basis function j at sample i
    let mut m = [0f32; 1024];
    for j in 0..n {
        for i in 0..n {
            m[j * n + i] = if dst { DST4[j][i] as f32 } else { TRANSFORM[j * step][i] as f32 };
        }
    }
    let mut tmp = [0f32; 1024];
    for k in 0..n {
        for y in 0..n {
            let a = m[k * n + y];
            for x in 0..n {
                tmp[k * n + x] += a * r[y * n + x];
            }
        }
    }
    let mut out = vec![0f32; n * n];
    for k in 0..n {
        for l in 0..n {
            let s: f32 = (0..n).map(|x| m[l * n + x] * tmp[k * n + x]).sum();
            out[k * n + l] = s;
        }
    }
    out
}

/// The scan order of a block's coefficients (7.4.9.11).
fn scan_for(log2: u32, luma: bool, intra: bool, mode: u32) -> usize {
    if intra && (log2 == 2 || (log2 == 3 && luma)) {
        match mode {
            6..=14 => 2,
            22..=30 => 1,
            _ => 0,
        }
    } else {
        0
    }
}

pub struct SliceCoder<'a> {
    sps: &'a Sps,
    pps: &'a Pps,
    geo: &'a Geometry,
    src: &'a Source,
    refp: Option<&'a Frame>,
    poc: i32,
    ref_poc: i32,
    pub pic: CurPic,
    qp: i32,
    lambda: f64,
    slice_addr: i32,
    slice_idx: u16,
    p_slice: bool,
    w: usize,
    h: usize,
    w4: usize,
}

impl<'a> SliceCoder<'a> {
    #[allow(clippy::too_many_arguments)]
    pub fn new(sps: &'a Sps, pps: &'a Pps, geo: &'a Geometry, src: &'a Source, refp: Option<&'a Frame>, poc: i32, qp: i32, slice_addr: i32, slice_idx: u16) -> Self {
        let ref_poc = refp.map_or(0, |r| r.poc);
        SliceCoder {
            sps,
            pps,
            geo,
            src,
            refp,
            poc,
            ref_poc,
            pic: CurPic::new(sps, 0, poc),
            qp,
            lambda: LAMBDA_SCALE * 0.57 * 2f64.powf((qp as f64 - 12.0) / 3.0),
            slice_addr,
            slice_idx,
            p_slice: refp.is_some(),
            w: sps.width,
            h: sps.height,
            w4: sps.width.div_ceil(4),
        }
    }

    fn i4(&self, x: usize, y: usize) -> usize {
        (y >> 2) * self.w4 + (x >> 2)
    }

    /// Z-scan availability of the block at (`xn`, `yn`) for the block at (`xc`, `yc`) (6.4.1).
    fn avail(&self, xc: usize, yc: usize, xn: i32, yn: i32) -> bool {
        if xn < 0 || yn < 0 || xn as usize >= self.w || yn as usize >= self.h {
            return false;
        }
        let (xn, yn) = (xn as usize, yn as usize);
        let g = self.geo;
        if g.zs[(yn >> 2) * g.w4 + (xn >> 2)] > g.zs[(yc >> 2) * g.w4 + (xc >> 2)] {
            return false;
        }
        let l = self.sps.log2_ctb;
        let nrs = (yn >> l) * self.sps.ctb_w + (xn >> l);
        let crs = (yc >> l) * self.sps.ctb_w + (xc >> l);
        if self.pic.ctb_slice_addr[nrs] != self.slice_addr {
            return false;
        }
        g.tile_id[g.rs_to_ts[nrs]] == g.tile_id[g.rs_to_ts[crs]]
    }

    fn stride(&self, c: usize) -> usize {
        if c == 0 {
            self.w
        } else {
            self.w / 2
        }
    }

    fn save(&self, x: usize, y: usize, n: usize) -> Saved {
        let rect = |p: &[u16], stride: usize, x0: usize, y0: usize, s: usize| -> Vec<u16> {
            (0..s).flat_map(|r| p[(y0 + r) * stride + x0..(y0 + r) * stride + x0 + s].to_vec()).collect()
        };
        let mut meta = Vec::with_capacity((n / 4) * (n / 4));
        for yy in (y >> 2)..((y + n) >> 2) {
            for xx in (x >> 2)..((x + n) >> 2) {
                let i = yy * self.w4 + xx;
                let p = &self.pic;
                meta.push(Meta {
                    pred: p.pred_mode[i],
                    skip: p.skip[i],
                    depth: p.ct_depth[i],
                    ipm: p.ipm[i],
                    qp: p.qp[i],
                    nz: p.nz[i],
                    edges: p.edges[i],
                    mv: p.frame.mv[i],
                });
            }
        }
        let f = &self.pic.frame.planes;
        Saved {
            planes: [rect(&f[0], self.w, x, y, n), rect(&f[1], self.w / 2, x / 2, y / 2, n / 2), rect(&f[2], self.w / 2, x / 2, y / 2, n / 2)],
            meta,
        }
    }

    fn restore(&mut self, s: &Saved, x: usize, y: usize, n: usize) {
        for (c, (x0, y0, stride, sz)) in [(x, y, self.w, n), (x / 2, y / 2, self.w / 2, n / 2), (x / 2, y / 2, self.w / 2, n / 2)].into_iter().enumerate() {
            let p = &mut self.pic.frame.planes[c];
            for r in 0..sz {
                let o = (y0 + r) * stride + x0;
                p[o..o + sz].copy_from_slice(&s.planes[c][r * sz..(r + 1) * sz]);
            }
        }
        let mut k = 0;
        for yy in (y >> 2)..((y + n) >> 2) {
            for xx in (x >> 2)..((x + n) >> 2) {
                let i = yy * self.w4 + xx;
                let m = s.meta[k];
                k += 1;
                let p = &mut self.pic;
                p.pred_mode[i] = m.pred;
                p.skip[i] = m.skip;
                p.ct_depth[i] = m.depth;
                p.ipm[i] = m.ipm;
                p.qp[i] = m.qp;
                p.nz[i] = m.nz;
                p.edges[i] = m.edges;
                p.frame.mv[i] = m.mv;
            }
        }
    }

    /// Marks the transform block edges of a rectangle, left and top, as the decoder does.
    fn mark_edges(&mut self, x: usize, y: usize, w: usize, h: usize) {
        let w4 = self.w4;
        for yy in (y >> 2)..((y + h) >> 2) {
            self.pic.edges[yy * w4 + (x >> 2)] |= EDGE_TU_V;
        }
        for xx in (x >> 2)..((x + w) >> 2) {
            self.pic.edges[(y >> 2) * w4 + xx] |= EDGE_TU_H;
        }
    }

    /// Records a coding unit's state over its area: prediction mode, intra modes, motion, depth,
    /// the luma coefficient flags of its transform blocks and their edges.
    #[allow(clippy::too_many_arguments)]
    fn set_meta(&mut self, x: usize, y: usize, log2: u32, depth: u32, intra: bool, mode: u32, mv: MvField, tus: &[(usize, usize, u32, bool)], skip: bool) {
        let n = 1usize << log2;
        let (pred, ipm) = if intra { (PRED_INTRA, mode as u8) } else { (PRED_INTER, 1) };
        for yy in (y >> 2)..((y + n) >> 2) {
            for xx in (x >> 2)..((x + n) >> 2) {
                let i = yy * self.w4 + xx;
                let p = &mut self.pic;
                p.pred_mode[i] = pred;
                p.skip[i] = skip as u8;
                p.ct_depth[i] = depth as u8;
                p.ipm[i] = ipm;
                p.qp[i] = self.qp as i8;
                p.nz[i] = 0;
                p.edges[i] = 0;
                p.frame.mv[i] = mv;
            }
        }
        self.mark_edges(x, y, n, n);
        for &(tx, ty, tl, coded) in tus {
            let s = 1usize << tl;
            if coded {
                for yy in (ty >> 2)..((ty + s) >> 2) {
                    for xx in (tx >> 2)..((tx + s) >> 2) {
                        self.pic.nz[yy * self.w4 + xx] = 1;
                    }
                }
            }
            self.mark_edges(tx, ty, s, s);
        }
    }

    // ------------------------------------------------------------ intra prediction

    /// The prediction of the `1 << log2` square at (`x`, `y`) of component `c` in `mode`.
    fn intra_pred(&self, c: usize, x: usize, y: usize, log2: u32, mode: u32) -> Vec<u16> {
        let n = 1usize << log2;
        let (sx, sy) = if c == 0 { (0, 0) } else { (1, 1) };
        let (xl, yl) = (x << sx, y << sy);
        let (ux, uy) = (4usize >> sx, 4usize >> sy);
        let plane = &self.pic.frame.planes[c];
        let stride = self.stride(c);
        let mut line = [0u16; 129];
        let mut av = [false; 129];
        let ok = |xn: i32, yn: i32| self.avail(xl, yl, xn, yn);
        let mut r = 0;
        while r < 2 * n {
            if ok((x as i32 - 1) << sx, ((y + r) << sy) as i32) {
                for k in r..(r + uy).min(2 * n) {
                    line[2 * n - 1 - k] = plane[(y + k) * stride + x - 1];
                    av[2 * n - 1 - k] = true;
                }
            }
            r += uy;
        }
        if ok((x as i32 - 1) << sx, (y as i32 - 1) << sy) {
            line[2 * n] = plane[(y - 1) * stride + x - 1];
            av[2 * n] = true;
        }
        let mut i = 0;
        while i < 2 * n {
            if ok(((x + i) << sx) as i32, (y as i32 - 1) << sy) {
                for k in i..(i + ux).min(2 * n) {
                    line[2 * n + 1 + k] = plane[(y - 1) * stride + x + k];
                    av[2 * n + 1 + k] = true;
                }
            }
            i += ux;
        }
        let len = 4 * n + 1;
        intra::substitute(&mut line[..len], &av[..len], 8);
        let mut out = [0u16; 32 * 32];
        intra::predict(&line[..len], log2, mode, c == 0, c == 0, c == 0 && self.sps.strong_intra_smoothing, 8, &mut out);
        out[..n * n].to_vec()
    }

    /// Writes a block of samples of component `c` into the picture.
    fn put(&mut self, c: usize, x: usize, y: usize, log2: u32, buf: &[u16]) {
        let n = 1usize << log2;
        let stride = self.stride(c);
        let p = &mut self.pic.frame.planes[c];
        for r in 0..n {
            p[(y + r) * stride + x..(y + r) * stride + x + n].copy_from_slice(&buf[r * n..r * n + n]);
        }
    }

    fn sad(&self, c: usize, x: usize, y: usize, log2: u32, pred: &[u16]) -> u64 {
        let n = 1usize << log2;
        let stride = self.stride(c);
        let src = &self.src.planes[c];
        let mut s = 0u64;
        for r in 0..n {
            for k in 0..n {
                s += (src[(y + r) * stride + x + k] as i32 - pred[r * n + k] as i32).unsigned_abs() as u64;
            }
        }
        s
    }

    fn pick_luma(&self, x: usize, y: usize, log2: u32) -> u32 {
        let mut best = 1u32;
        let mut bc = u64::MAX;
        for m in [0, 1, 10, 26] {
            let c = self.sad(0, x, y, log2, &self.intra_pred(0, x, y, log2, m));
            if c < bc {
                bc = c;
                best = m;
            }
        }
        for step in [8i32, 4, 2, 1] {
            for m in [best as i32 - step, best as i32 + step] {
                if (2..=34).contains(&m) {
                    let c = self.sad(0, x, y, log2, &self.intra_pred(0, x, y, log2, m as u32));
                    if c < bc {
                        bc = c;
                        best = m as u32;
                    }
                }
            }
        }
        best
    }

    /// The chroma prediction mode: (intra_chroma_pred_mode, mode) for the luma mode `luma`.
    fn pick_chroma(&self, x: usize, y: usize, log2: u32, luma: u32) -> (u32, u32) {
        let cost = |m: u32| -> u64 { self.sad(1, x, y, log2, &self.intra_pred(1, x, y, log2, m)) + self.sad(2, x, y, log2, &self.intra_pred(2, x, y, log2, m)) };
        let mut best = (4u32, luma);
        let mut bc = cost(luma);
        for (idc, m) in [(0u32, 0u32), (1, 26), (2, 10), (3, 1)] {
            if m != luma && cost(m) < bc {
                bc = cost(m);
                best = (idc, m);
            }
        }
        best
    }

    /// The most probable mode syntax of `mode` at (`x`, `y`): (prev_intra_luma_pred_flag, mpm_idx, rem).
    fn mpm(&self, x: usize, y: usize, mode: u32) -> (bool, u32, u32) {
        let cand = |xn: i32, yn: i32, above: bool| -> u32 {
            if !self.avail(x, y, xn, yn) {
                return 1;
            }
            let i = self.i4(xn as usize, yn as usize);
            if self.pic.pred_mode[i] != PRED_INTRA {
                return 1;
            }
            if above && (yn as usize) < ((y >> LOG2_CTB) << LOG2_CTB) {
                return 1;
            }
            self.pic.ipm[i] as u32
        };
        let a = cand(x as i32 - 1, y as i32, false);
        let b = cand(x as i32, y as i32 - 1, true);
        let list = if a == b {
            if a < 2 {
                [0, 1, 26]
            } else {
                [a, 2 + ((a + 29) % 32), 2 + ((a - 2 + 1) % 32)]
            }
        } else {
            let c = if a != 0 && b != 0 {
                0
            } else if a != 1 && b != 1 {
                1
            } else {
                26
            };
            [a, b, c]
        };
        match list.iter().position(|&m| m == mode) {
            Some(i) => (true, i as u32, 0),
            None => (false, 0, mode - list.iter().filter(|&&m| m < mode).count() as u32),
        }
    }

    // ------------------------------------------------------------ residuals

    /// Quantises the residual of the block at (`x`, `y`) of component `c` against the prediction
    /// already in the picture, and reconstructs it there. Returns the levels, the squared error of
    /// the reconstruction and an estimate of the bits the levels take.
    fn residual_block(&mut self, c: usize, x: usize, y: usize, log2: u32, intra: bool) -> (Blk, f64, f64) {
        let n = 1usize << log2;
        let stride = self.stride(c);
        let dst = intra && c == 0 && log2 == 2;
        let pred: Vec<i32> = (0..n * n).map(|i| self.pic.frame.planes[c][(y + i / n) * stride + x + i % n] as i32).collect();
        let src = &self.src.planes[c];
        let r: Vec<f32> = (0..n * n).map(|i| (src[(y + i / n) * stride + x + i % n] as i32 - pred[i]) as f32).collect();
        let f = forward(&r, log2, dst);
        let qp = if c == 0 { self.qp } else { chroma_qp(self.qp) };
        let step = 16.0 * LEVEL_SCALE[(qp % 6) as usize] as f32 * (1u32 << (qp / 6)) as f32 / (1u32 << (log2 + 3)) as f32;
        let scale = 1.0 / (32.0 * (n * n) as f32);
        // HM's rounding offsets: a dead zone for intra (1/3) and inter (1/6) residuals
        let off = if intra { 1.0 / 3.0 } else { 1.0 / 6.0 };
        let lv: Vec<i32> = f
            .iter()
            .map(|&v| {
                let q = (v * scale / step).abs();
                let a = (q + off).floor().min(32767.0) as i32;
                if v < 0.0 {
                    -a
                } else {
                    a
                }
            })
            .collect();
        if lv.iter().all(|&v| v == 0) {
            let d = (0..n * n).map(|i| (r[i] as f64).powi(2)).sum();
            return (Blk { cbf: false, lv }, d, 0.0);
        }
        let bits: f64 = lv.iter().filter(|&&v| v != 0).map(|&v| 3.0 + 2.0 * (v.unsigned_abs() as f64).log2().max(0.0)).sum::<f64>() + 1.0;
        let mut co = lv.clone();
        dequantise(&mut co, log2, qp, 8, None, n - 1, n - 1);
        inverse(&mut co, log2, dst, 8, n - 1, n - 1);
        let p = &mut self.pic.frame.planes[c];
        let mut d = 0f64;
        for i in 0..n * n {
            let o = (y + i / n) * stride + x + i % n;
            let v = (pred[i] + co[i]).clamp(0, 255);
            p[o] = v as u16;
            let e = (src[o] as i32 - v) as f64;
            d += e * e;
        }
        (Blk { cbf: true, lv }, d, bits)
    }

    // ------------------------------------------------------------ coding units

    /// The best of the coding unit at (`x`, `y`) of size `1 << log2` and the split of its area,
    /// with the reconstruction of the choice left in the picture.
    fn quad(&mut self, x: usize, y: usize, log2: u32, depth: u32) -> (f64, Node) {
        let n = 1usize << log2;
        let inside = x + n <= self.w && y + n <= self.h;
        let can_split = log2 > LOG2_MIN_CB;
        if !inside {
            return self.split(x, y, log2, depth, 0.0);
        }
        let before = self.save(x, y, n);
        let (lc, leaf) = self.eval_cu(x, y, log2, depth);
        if !can_split {
            return (lc, Node::Leaf(Box::new(leaf)));
        }
        // a skipped unit is not split further
        if matches!(&leaf, Cu { skip: true, .. }) {
            return (lc, Node::Leaf(Box::new(leaf)));
        }
        let after_leaf = self.save(x, y, n);
        self.restore(&before, x, y, n);
        let (sc, node) = self.split(x, y, log2, depth, self.lambda);
        if lc <= sc {
            self.restore(&after_leaf, x, y, n);
            (lc, Node::Leaf(Box::new(leaf)))
        } else {
            (sc, node)
        }
    }

    fn split(&mut self, x: usize, y: usize, log2: u32, depth: u32, bits: f64) -> (f64, Node) {
        let h = 1usize << (log2 - 1);
        let mut cost = bits;
        let mut kids = Vec::with_capacity(4);
        for (dx, dy) in [(0, 0), (h, 0), (0, h), (h, h)] {
            if x + dx < self.w && y + dy < self.h {
                let (c, node) = self.quad(x + dx, y + dy, log2 - 1, depth + 1);
                cost += c;
                kids.push(node);
            }
        }
        (cost, Node::Split(kids))
    }

    fn eval_cu(&mut self, x: usize, y: usize, log2: u32, depth: u32) -> (f64, Cu) {
        let n = 1usize << log2;
        let before = self.save(x, y, n);
        let (ci, cu_i) = self.eval_intra(x, y, log2, depth);
        if !self.p_slice {
            return (ci, cu_i);
        }
        let after_intra = self.save(x, y, n);
        self.restore(&before, x, y, n);
        match self.eval_inter(x, y, log2, depth) {
            Some((cp, cu_p, after)) if cp < ci => {
                self.restore(&after, x, y, n);
                (cp, cu_p)
            }
            _ => {
                self.restore(&after_intra, x, y, n);
                (ci, cu_i)
            }
        }
    }

    /// An intra unit, unsplit or (for 8x8 units) with four 4x4 luma transforms, whichever costs less.
    fn eval_intra(&mut self, x: usize, y: usize, log2: u32, depth: u32) -> (f64, Cu) {
        if log2 > LOG2_MIN_CB {
            return self.intra_cu(x, y, log2, depth, false);
        }
        let n = 1usize << log2;
        let before = self.save(x, y, n);
        let (c0, cu0) = self.intra_cu(x, y, log2, depth, false);
        let after = self.save(x, y, n);
        self.restore(&before, x, y, n);
        let (c1, cu1) = self.intra_cu(x, y, log2, depth, true);
        if c0 <= c1 {
            self.restore(&after, x, y, n);
            (c0, cu0)
        } else {
            (c1, cu1)
        }
    }

    fn intra_cu(&mut self, x: usize, y: usize, log2: u32, depth: u32, split: bool) -> (f64, Cu) {
        let mode = self.pick_luma(x, y, log2);
        let cl = if split { 2 } else { log2 - 1 };
        let (cidc, cmode) = self.pick_chroma(x / 2, y / 2, cl, mode);
        let tus: Vec<(usize, usize, u32)> = if split { (0..4).map(|k| (x + (k & 1) * 4, y + (k >> 1) * 4, 2)).collect() } else { vec![(x, y, log2)] };
        let (mut dist, mut bits) = (0.0, 0.0);
        let mut luma = Vec::new();
        for &(tx, ty, tl) in &tus {
            let p = self.intra_pred(0, tx, ty, tl, mode);
            self.put(0, tx, ty, tl, &p);
            let (b, d, r) = self.residual_block(0, tx, ty, tl, true);
            dist += d;
            bits += r;
            luma.push(b);
        }
        let mut chroma = Vec::new();
        for c in 1..3 {
            let p = self.intra_pred(c, x / 2, y / 2, cl, cmode);
            self.put(c, x / 2, y / 2, cl, &p);
            let (b, d, r) = self.residual_block(c, x / 2, y / 2, cl, true);
            dist += d;
            bits += r;
            chroma.push(b);
        }
        let (prev_flag, mpm_idx, rem) = self.mpm(x, y, mode);
        let rects: Vec<(usize, usize, u32, bool)> = tus.iter().zip(&luma).map(|(&(tx, ty, tl), b)| (tx, ty, tl, b.cbf)).collect();
        self.set_meta(x, y, log2, depth, true, mode, MvField::default(), &rects, false);
        let cost = dist + self.lambda * (bits + SIDE_INTRA);
        let cr = chroma.pop().unwrap_or_default();
        let cb = chroma.pop().unwrap_or_default();
        (
            cost,
            Cu { log2, intra: true, split_tu: split, prev_flag, mpm_idx, rem, luma_mode: mode, chroma_idc: cidc, chroma_mode: cmode, mvp: 0, mvd: [0, 0], skip: false, merge_idx: None, luma, cb, cr },
        )
    }

    // ------------------------------------------------------------ inter prediction

    /// The motion vector predictor candidates for the block at (`x`, `y`) of width `w` (8.5.3.2.6, without
    /// the temporal candidate, which the stream turns off).
    fn amvp(&self, x: usize, y: usize, w: usize) -> [[i16; 2]; 2] {
        let (xi, yi, wi) = (x as i32, y as i32, w as i32);
        let cur_poc = self.ref_poc;
        let nb = |xn: i32, yn: i32| -> Option<MvField> {
            if self.avail(x, y, xn, yn) && self.pic.pred_mode[self.i4(xn as usize, yn as usize)] == PRED_INTER {
                Some(self.pic.frame.mv[self.i4(xn as usize, yn as usize)])
            } else {
                None
            }
        };
        let a = [nb(xi - 1, yi + wi), nb(xi - 1, yi + wi - 1)];
        let b = [nb(xi + wi, yi - 1), nb(xi + wi - 1, yi - 1), nb(xi - 1, yi - 1)];
        let is_scaled = a[0].is_some() || a[1].is_some();
        let direct = |m: &MvField| -> Option<[i16; 2]> { (m.uses(0) && m.ref_poc[0] == cur_poc).then_some(m.mv[0]) };
        let scaled = |m: &MvField| -> Option<[i16; 2]> {
            if m.uses(0) && !m.long_term(0) {
                Some(scale_mv(m.mv[0], self.poc - m.ref_poc[0], self.poc - cur_poc))
            } else {
                None
            }
        };
        let mut mva = a.iter().flatten().find_map(direct);
        if mva.is_none() {
            mva = a.iter().flatten().find_map(scaled);
        }
        let mut mvb = b.iter().flatten().find_map(direct);
        if !is_scaled {
            if mvb.is_some() {
                mva = mvb;
            }
            mvb = b.iter().flatten().find_map(scaled);
        }
        let mut cands = Vec::with_capacity(2);
        if let Some(v) = mva {
            cands.push(v);
        }
        if let Some(v) = mvb {
            if mva != Some(v) {
                cands.push(v);
            }
        }
        [cands.first().copied().unwrap_or([0, 0]), cands.get(1).copied().unwrap_or([0, 0])]
    }

    /// The cost of predicting the `n` square at (`x`, `y`) with `mv`: the SAD of the rounded
    /// interpolation, and a penalty for the vector difference.
    fn motion_cost(&self, x: usize, y: usize, n: usize, mv: [i16; 2], pred: [i16; 2]) -> u64 {
        let refp = self.refp.expect("inter prediction has a reference");
        let src = &self.src.planes[0];
        let mut s = 0u64;
        if mv[0] & 3 == 0 && mv[1] & 3 == 0 {
            // a whole-sample vector: the reference samples themselves, at clamped positions
            let (ix, iy) = (x as i32 + (mv[0] >> 2) as i32, y as i32 + (mv[1] >> 2) as i32);
            for r in 0..n {
                let ry = (iy + r as i32).clamp(0, self.h as i32 - 1) as usize;
                for k in 0..n {
                    let rx = (ix + k as i32).clamp(0, self.w as i32 - 1) as usize;
                    s += (refp.planes[0][ry * self.w + rx] as i32 - src[(y + r) * self.w + x + k] as i32).unsigned_abs() as u64;
                }
            }
        } else {
            let mut out = [0i16; 32 * 32];
            luma_mc(&refp.planes[0], self.w, self.h, x as i32, y as i32, mv, n, n, 8, &mut out);
            for r in 0..n {
                for k in 0..n {
                    let p = ((out[r * n + k] as i32 + 32) >> 6).clamp(0, 255);
                    s += (p - src[(y + r) * self.w + x + k] as i32).unsigned_abs() as u64;
                }
            }
        }
        let pen = (mv[0] as i32 - pred[0] as i32).unsigned_abs() + (mv[1] as i32 - pred[1] as i32).unsigned_abs();
        s + (pen / 2) as u64
    }

    /// Motion search: integer positions around zero and the predictors, then half and quarter samples.
    fn motion_search(&self, x: usize, y: usize, n: usize, cands: &[[i16; 2]; 2]) -> [i16; 2] {
        let pred = cands[0];
        let mut best = [0i16, 0];
        let mut bc = self.motion_cost(x, y, n, best, pred);
        let try_mv = |mv: [i16; 2], best: &mut [i16; 2], bc: &mut u64| {
            let c = self.motion_cost(x, y, n, mv, pred);
            if c < *bc {
                *bc = c;
                *best = mv;
            }
        };
        for c in cands {
            try_mv([(c[0] >> 2) << 2, (c[1] >> 2) << 2], &mut best, &mut bc);
        }
        for dy in -2i16..=2 {
            for dx in -2i16..=2 {
                try_mv([dx * 4, dy * 4], &mut best, &mut bc);
            }
        }
        for _ in 0..16 {
            let centre = best;
            for (dx, dy) in [(4i16, 0i16), (-4, 0), (0, 4), (0, -4)] {
                try_mv([centre[0] + dx, centre[1] + dy], &mut best, &mut bc);
            }
            if best == centre {
                break;
            }
        }
        for step in [2i16, 1] {
            let centre = best;
            for dy in -1..=1i16 {
                for dx in -1..=1i16 {
                    if dx != 0 || dy != 0 {
                        try_mv([centre[0] + dx * step, centre[1] + dy * step], &mut best, &mut bc);
                    }
                }
            }
        }
        best
    }

    /// Writes the motion-compensated prediction of the `n` square at (`x`, `y`) with `mv` into the picture.
    fn inter_pred(&mut self, x: usize, y: usize, n: usize, mv: [i16; 2]) {
        let refp = self.refp.expect("inter prediction has a reference");
        let mut out = [0i16; 32 * 32];
        for c in 0..3 {
            let (bx, by, bn) = if c == 0 { (x, y, n) } else { (x / 2, y / 2, n / 2) };
            if c == 0 {
                luma_mc(&refp.planes[0], self.w, self.h, bx as i32, by as i32, mv, bn, bn, 8, &mut out);
            } else {
                chroma_mc(&refp.planes[c], self.w / 2, self.h / 2, bx as i32, by as i32, mv, (1, 1), bn, bn, 8, &mut out);
            }
            let block: Vec<u16> = out[..bn * bn].iter().map(|&o| ((o as i32 + 32) >> 6).clamp(0, 255) as u16).collect();
            self.put(c, bx, by, bn.trailing_zeros(), &block);
        }
    }

    /// The merge candidates' motion vectors for the `n` square at (`x`, `y`): for a P slice of one
    /// reference, the spatial candidates (8.5.3.2.3 to 8.5.3.2.5) then zero candidates, five in all.
    fn merge_mvs(&self, x: usize, y: usize, n: usize) -> Vec<[i16; 2]> {
        let (xi, yi, ni) = (x as i32, y as i32, n as i32);
        let get = |xn: i32, yn: i32| -> Option<[i16; 2]> {
            if self.avail(x, y, xn, yn) && self.pic.pred_mode[self.i4(xn as usize, yn as usize)] == PRED_INTER {
                Some(self.pic.frame.mv[self.i4(xn as usize, yn as usize)].mv[0])
            } else {
                None
            }
        };
        let a1 = get(xi - 1, yi + ni - 1);
        let b1 = get(xi + ni - 1, yi - 1);
        let b0 = get(xi + ni, yi - 1);
        let a0 = get(xi - 1, yi + ni);
        let b2 = get(xi - 1, yi - 1);
        let mut list: Vec<[i16; 2]> = Vec::with_capacity(5);
        list.extend(a1);
        if let Some(c) = b1.filter(|&c| a1 != Some(c)) {
            list.push(c);
        }
        if let Some(c) = b0.filter(|&c| b1 != Some(c)) {
            list.push(c);
        }
        if let Some(c) = a0.filter(|&c| a1 != Some(c)) {
            list.push(c);
        }
        if let Some(c) = b2.filter(|&c| list.len() < 4 && a1 != Some(c) && b1 != Some(c)) {
            list.push(c);
        }
        list.resize(5, [0, 0]);
        list
    }

    /// Motion-compensated prediction and residual of a unit with motion `mvf`, costed. `merge` is the
    /// merge index; otherwise the AMVP predictor index `mvp`, difference `mvd` and their `mvbits`.
    #[allow(clippy::too_many_arguments)]
    fn code_inter(&mut self, x: usize, y: usize, log2: u32, depth: u32, mvf: MvField, merge: Option<u32>, mvp: usize, mvd: [i32; 2], mvbits: f64) -> (f64, Cu) {
        let n = 1usize << log2;
        self.inter_pred(x, y, n, mvf.mv[0]);
        let (mut dist, mut bits) = (0.0, 0.0);
        let (lb, d, r) = self.residual_block(0, x, y, log2, false);
        dist += d;
        bits += r;
        let mut chroma = Vec::new();
        for c in 1..3 {
            let (b, d, r) = self.residual_block(c, x / 2, y / 2, log2 - 1, false);
            dist += d;
            bits += r;
            chroma.push(b);
        }
        let cr = chroma.pop().unwrap_or_default();
        let cb = chroma.pop().unwrap_or_default();
        let skip = merge.is_some() && !(lb.cbf || cb.cbf || cr.cbf);
        let tus = [(x, y, log2, lb.cbf)];
        self.set_meta(x, y, log2, depth, false, 1, mvf, &tus, skip);
        let cost = dist + self.lambda * (bits + mvbits + SIDE_INTER);
        (cost, Cu { log2, intra: false, split_tu: false, prev_flag: false, mpm_idx: 0, rem: 0, luma_mode: 1, chroma_idc: 4, chroma_mode: 1, mvp, mvd, skip, merge_idx: merge, luma: vec![lb], cb, cr })
    }

    /// The best inter unit at (`x`, `y`) among the merge candidates and the AMVP motion search, with
    /// its reconstruction saved.
    fn eval_inter(&mut self, x: usize, y: usize, log2: u32, depth: u32) -> Option<(f64, Cu, Saved)> {
        self.refp?;
        let n = 1usize << log2;
        let mut best: Option<(f64, Cu, Saved)> = None;
        let mut tried: Vec<[i16; 2]> = Vec::with_capacity(5);
        for (k, mv) in self.merge_mvs(x, y, n).into_iter().enumerate() {
            if tried.contains(&mv) {
                continue;
            }
            tried.push(mv);
            let mvf = MvField { mv: [mv, [0, 0]], ref_idx: [0, -1], ref_poc: [self.ref_poc, 0], pred: 1 };
            let mvbits = if k == 0 { 1.0 } else { k as f64 + 1.0 };
            let (c, cu) = self.code_inter(x, y, log2, depth, mvf, Some(k as u32), 0, [0, 0], mvbits);
            if best.as_ref().is_none_or(|b| c < b.0) {
                best = Some((c, cu, self.save(x, y, n)));
            }
        }
        // early out: a skipped merge candidate is the cheapest unit to code, so no motion search
        if best.as_ref().is_some_and(|b| b.1.skip) {
            return best;
        }
        let cands = self.amvp(x, y, n);
        let mv = self.motion_search(x, y, n, &cands);
        let dist_to = |c: [i16; 2]| (mv[0] as i32 - c[0] as i32).abs() + (mv[1] as i32 - c[1] as i32).abs();
        let mvp = if dist_to(cands[0]) <= dist_to(cands[1]) { 0 } else { 1 };
        let mvd = [mv[0] as i32 - cands[mvp][0] as i32, mv[1] as i32 - cands[mvp][1] as i32];
        let mvbits: f64 = mvd.iter().map(|&v| if v == 0 { 1.0 } else { 2.0 + 2.0 * (v.unsigned_abs() as f64).log2() }).sum();
        let mvf = MvField { mv: [mv, [0, 0]], ref_idx: [0, -1], ref_poc: [self.ref_poc, 0], pred: 1 };
        let (c, cu) = self.code_inter(x, y, log2, depth, mvf, None, mvp, mvd, mvbits);
        if best.as_ref().is_none_or(|b| c < b.0) {
            best = Some((c, cu, self.save(x, y, n)));
        }
        best
    }

    // ------------------------------------------------------------ syntax

    /// The bins of a coding quadtree node at (`x`, `y`) of size `1 << log2`.
    fn write_node(&self, e: &mut CabacEnc, node: &Node, x: usize, y: usize, log2: u32, depth: u32) {
        let n = 1usize << log2;
        let inside = x + n <= self.w && y + n <= self.h;
        let coded = inside && log2 > LOG2_MIN_CB;
        match node {
            Node::Split(kids) => {
                if coded {
                    e.decision(SPLIT_CODING_UNIT_FLAG_OFFSET + self.split_inc(x, y, depth), 1);
                }
                let h = n / 2;
                let mut k = 0;
                for (dx, dy) in [(0, 0), (h, 0), (0, h), (h, h)] {
                    if x + dx < self.w && y + dy < self.h {
                        self.write_node(e, &kids[k], x + dx, y + dy, log2 - 1, depth + 1);
                        k += 1;
                    }
                }
            }
            Node::Leaf(cu) => {
                if coded {
                    e.decision(SPLIT_CODING_UNIT_FLAG_OFFSET + self.split_inc(x, y, depth), 0);
                }
                self.write_cu(e, cu, x, y);
            }
        }
    }

    fn split_inc(&self, x: usize, y: usize, depth: u32) -> usize {
        let mut inc = 0;
        if self.avail(x, y, x as i32 - 1, y as i32) && self.pic.ct_depth[self.i4(x - 1, y)] as u32 > depth {
            inc += 1;
        }
        if self.avail(x, y, x as i32, y as i32 - 1) && self.pic.ct_depth[self.i4(x, y - 1)] as u32 > depth {
            inc += 1;
        }
        inc
    }

    /// The merge index: truncated unary over five candidates, the first bin in its own context.
    fn write_merge_idx(&self, e: &mut CabacEnc, idx: u32) {
        if idx == 0 {
            e.decision(MERGE_IDX_OFFSET, 0);
            return;
        }
        e.decision(MERGE_IDX_OFFSET, 1);
        for _ in 1..idx {
            e.bypass(1);
        }
        if idx < 4 {
            e.bypass(0);
        }
    }

    fn skip_inc(&self, x: usize, y: usize) -> usize {
        let mut inc = 0;
        if self.avail(x, y, x as i32 - 1, y as i32) && self.pic.skip[self.i4(x - 1, y)] != 0 {
            inc += 1;
        }
        if self.avail(x, y, x as i32, y as i32 - 1) && self.pic.skip[self.i4(x, y - 1)] != 0 {
            inc += 1;
        }
        inc
    }

    fn write_cu(&self, e: &mut CabacEnc, cu: &Cu, x: usize, y: usize) {
        if self.p_slice {
            e.decision(SKIP_FLAG_OFFSET + self.skip_inc(x, y), cu.skip as u32);
            if cu.skip {
                self.write_merge_idx(e, cu.merge_idx.unwrap_or(0));
                return;
            }
            e.decision(PRED_MODE_FLAG_OFFSET, cu.intra as u32);
        }
        if !cu.intra || cu.log2 == LOG2_MIN_CB {
            e.decision(PART_MODE_OFFSET, 1);
        }
        if cu.intra {
            e.decision(PREV_INTRA_LUMA_PRED_FLAG_OFFSET, cu.prev_flag as u32);
            if cu.prev_flag {
                match cu.mpm_idx {
                    0 => e.bypass(0),
                    1 => {
                        e.bypass(1);
                        e.bypass(0);
                    }
                    _ => {
                        e.bypass(1);
                        e.bypass(1);
                    }
                }
            } else {
                e.bypass_bits(cu.rem, 5);
            }
            if cu.chroma_idc == 4 {
                e.decision(INTRA_CHROMA_PRED_MODE_OFFSET, 0);
            } else {
                e.decision(INTRA_CHROMA_PRED_MODE_OFFSET, 1);
                e.bypass_bits(cu.chroma_idc, 2);
            }
        } else if let Some(idx) = cu.merge_idx {
            // a merged 2Nx2N unit with a residual: rqt_root_cbf is inferred to be 1
            e.decision(MERGE_FLAG_OFFSET, 1);
            self.write_merge_idx(e, idx);
        } else {
            e.decision(MERGE_FLAG_OFFSET, 0);
            self.write_mvd(e, cu.mvd);
            e.decision(MVP_LX_FLAG_OFFSET, cu.mvp as u32);
            let rqt = cu.luma[0].cbf || cu.cb.cbf || cu.cr.cbf;
            e.decision(NO_RESIDUAL_DATA_FLAG_OFFSET, rqt as u32);
            if !rqt {
                return;
            }
        }
        self.write_tree(e, cu);
    }

    fn write_mvd(&self, e: &mut CabacEnc, mvd: [i32; 2]) {
        let g0 = [mvd[0] != 0, mvd[1] != 0];
        let g1 = [mvd[0].abs() > 1, mvd[1].abs() > 1];
        e.decision(ABS_MVD_GREATER0_FLAG_OFFSET, g0[0] as u32);
        e.decision(ABS_MVD_GREATER0_FLAG_OFFSET, g0[1] as u32);
        for c in 0..2 {
            if g0[c] {
                e.decision(ABS_MVD_GREATER1_FLAG_OFFSET + 1, g1[c] as u32);
            }
        }
        for c in 0..2 {
            if !g0[c] {
                continue;
            }
            if g1[c] {
                // the first-order Exp-Golomb code of |mvd| - 2
                let mut u = mvd[c].unsigned_abs() - 2;
                let mut k = 1;
                while u >= 1 << k {
                    e.bypass(1);
                    u -= 1 << k;
                    k += 1;
                }
                e.bypass(0);
                e.bypass_bits(u, k);
            }
            e.bypass((mvd[c] < 0) as u32);
        }
    }

    fn write_tree(&self, e: &mut CabacEnc, cu: &Cu) {
        let log2 = cu.log2;
        if cu.intra {
            e.decision(SPLIT_TRANSFORM_FLAG_OFFSET + (5 - log2 as usize), cu.split_tu as u32);
        }
        e.decision(CBF_CB_CR_OFFSET, cu.cb.cbf as u32);
        e.decision(CBF_CB_CR_OFFSET, cu.cr.cbf as u32);
        if cu.split_tu {
            for b in &cu.luma {
                e.decision(CBF_LUMA_OFFSET, b.cbf as u32);
                if b.cbf {
                    residual(e, &b.lv, 2, scan_for(2, true, true, cu.luma_mode), true);
                }
            }
        } else {
            let coded = cu.intra || cu.cb.cbf || cu.cr.cbf;
            if coded {
                e.decision(CBF_LUMA_OFFSET + 1, cu.luma[0].cbf as u32);
            }
            if cu.luma[0].cbf {
                residual(e, &cu.luma[0].lv, log2, scan_for(log2, true, cu.intra, cu.luma_mode), true);
            }
        }
        let cl = if cu.split_tu { 2 } else { log2 - 1 };
        for b in [&cu.cb, &cu.cr] {
            if b.cbf {
                residual(e, &b.lv, cl, scan_for(cl, false, cu.intra, cu.chroma_mode), false);
            }
        }
    }

    /// Codes the CTBs of `rows` (CTB rows of the picture) as one slice segment's data; returns the
    /// picture coded and the slice data bytes (the slice header is the caller's).
    pub fn run(mut self, rows: Range<usize>) -> (CurPic, Vec<u8>) {
        let ctb_w = self.sps.ctb_w;
        let (first, end) = (rows.start * ctb_w, (rows.end * ctb_w).min(self.sps.ctb_w * self.sps.ctb_h));
        for rs in first..end {
            self.pic.ctb_slice_addr[rs] = self.slice_addr;
            self.pic.ctb_slice_idx[rs] = self.slice_idx;
        }
        let init = &INIT_VALUES[if self.p_slice { 1 } else { 0 }];
        let mut e = CabacEnc::new(BitWriter::new(), init, self.qp);
        for rs in first..end {
            let (x0, y0) = ((rs % ctb_w) << LOG2_CTB, (rs / ctb_w) << LOG2_CTB);
            let (_, node) = self.quad(x0, y0, LOG2_CTB, 0);
            self.write_node(&mut e, &node, x0, y0, LOG2_CTB, 0);
            e.terminate((rs + 1 == end) as u32);
        }
        (self.pic, e.w.into_bytes())
    }
}
