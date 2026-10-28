//! Encoding one picture as one slice: mode decision, motion search, the forward transform and
//! quantisation, reconstruction (with the decoder's own prediction, inverse transform and loop
//! filter, so the reference the encoder keeps is the one a decoder builds) and CAVLC macroblock syntax.

use super::bitw::{se_bits, BitWriter};
use super::cabac::{self, CabacWriter};
use super::cavlc::write_block;
use crate::h264::idct::{hadamard4, idct4_add};
use super::subpel::HalfPel;
use crate::h264::inter::chroma_mc;
use crate::h264::intra::{predict_16x16, predict_chroma, predict_nxn, Edges};
use crate::h264::mb::{chroma_qp, Kind, MbInfo, Picture};
use std::ops::Range;
use crate::h264::ps::ZIGZAG4;
use crate::h264::tables::{GOLOMB_TO_INTER_CBP, GOLOMB_TO_INTRA4X4_CBP};

/// Forward quantisation multipliers and dequantisation scales by QP % 6 and coefficient class
/// (8.5.12.1; class 0 at even/even positions, 1 at odd/odd, 2 elsewhere).
const MF: [[i32; 3]; 6] = [[13107, 5243, 8066], [11916, 4660, 7490], [10082, 4194, 6554], [9362, 3647, 5825], [8192, 3355, 5243], [7282, 2893, 4559]];
const V: [[i32; 3]; 6] = [[10, 16, 13], [11, 18, 14], [13, 20, 16], [14, 23, 18], [16, 25, 20], [18, 29, 23]];

fn class(i: usize) -> usize {
    match (i & 1, (i >> 2) & 1) {
        (0, 0) => 0,
        (1, 1) => 1,
        _ => 2,
    }
}

/// Raster (bx, by) of the n-th 4x4 luma block in decoding order.
fn block_pos(n: usize) -> (usize, usize) {
    (((n >> 2) & 1) * 2 + (n & 1), ((n >> 3) & 1) * 2 + ((n >> 1) & 1))
}

/// The 4x4 forward core transform (8.5.12's inverse), row-major.
fn fdct4(d: &[i32; 16]) -> [i32; 16] {
    let mut t = [0i32; 16];
    for r in 0..4 {
        let a = &d[r * 4..r * 4 + 4];
        let (s03, d03, s12, d12) = (a[0] + a[3], a[0] - a[3], a[1] + a[2], a[1] - a[2]);
        t[r * 4] = s03 + s12;
        t[r * 4 + 1] = 2 * d03 + d12;
        t[r * 4 + 2] = s03 - s12;
        t[r * 4 + 3] = d03 - 2 * d12;
    }
    let mut o = [0i32; 16];
    for c in 0..4 {
        let (s03, d03, s12, d12) = (t[c] + t[12 + c], t[c] - t[12 + c], t[4 + c] + t[8 + c], t[4 + c] - t[8 + c]);
        o[c] = s03 + s12;
        o[4 + c] = 2 * d03 + d12;
        o[8 + c] = s03 - s12;
        o[12 + c] = d03 - 2 * d12;
    }
    o
}

fn quant(c: i32, mf: i32, f: i32, qbits: u32) -> i32 {
    let a = ((c.unsigned_abs() as i64 * mf as i64 + f as i64) >> qbits) as i32;
    if c < 0 {
        -a
    } else {
        a
    }
}

/// The sum of absolute Hadamard-transformed differences of a 4x4 block, halved.
fn satd4(d: &[i32; 16]) -> i32 {
    let mut t = [0i32; 16];
    for r in 0..4 {
        let (a, b, c, e) = (d[r * 4], d[r * 4 + 1], d[r * 4 + 2], d[r * 4 + 3]);
        let (s0, s1, d0, d1) = (a + b, c + e, a - b, c - e);
        t[r * 4] = s0 + s1;
        t[r * 4 + 1] = s0 - s1;
        t[r * 4 + 2] = d0 - d1;
        t[r * 4 + 3] = d0 + d1;
    }
    let mut sum = 0;
    for c in 0..4 {
        let (a, b, cc, e) = (t[c], t[4 + c], t[8 + c], t[12 + c]);
        let (s0, s1, d0, d1) = (a + b, cc + e, a - b, cc - e);
        sum += (s0 + s1).abs() + (s0 - s1).abs() + (d0 - d1).abs() + (d0 + d1).abs();
    }
    sum >> 1
}

/// The Lagrange multiplier that weighs bits against SATD at `qp`.
fn lambda(qp: i32) -> i32 {
    (2f64.powf((qp as f64 - 12.0) / 6.0).round() as i32).max(1)
}

/// Bits of a motion vector difference.
fn mvd_bits(mv: [i16; 2], mvp: [i16; 2]) -> i32 {
    (se_bits(mv[0] as i32 - mvp[0] as i32) + se_bits(mv[1] as i32 - mvp[1] as i32)) as i32
}

fn median(a: i16, b: i16, c: i16) -> i16 {
    a.max(b).min(a.min(b).max(c))
}

/// What later macroblocks need to know about a coded one.
#[derive(Clone, Copy, Default)]
struct MbState {
    i4: bool,
    inter: bool,
    /// Intra 4x4 prediction modes, raster order (meaningful when `i4`).
    modes: [u8; 16],
    /// Coefficients coded per luma 4x4 block (raster) and per chroma AC block.
    nnz: [u8; 16],
    cnnz: [[u8; 4]; 2],
    mv: [i16; 2],
    // for the CABAC contexts
    skip: bool,
    kind: Kind,
    cbp: u8,
    chroma_mode: u8,
    /// Coded DC blocks: bit 0 Intra16x16 luma, bit 1 Cb, bit 2 Cr.
    cbf_dc: u8,
    /// Absolute motion vector difference, capped at 64.
    mvd: [u8; 2],
}

/// A macroblock's decisions and levels, ready to write.
struct MbCode {
    kind: Kind,
    skip: bool,
    i16_mode: u8,
    modes: [u8; 16],
    chroma_mode: u8,
    mv: [i16; 2],
    cbp: u8,
    /// Luma levels per raster 4x4 block, in scan order (the DC slot unused for Intra16x16).
    luma: [[i32; 16]; 16],
    dc: [i32; 16],
    cdc: [[i32; 4]; 2],
    cac: [[[i32; 16]; 4]; 2],
    nnz: [u8; 16],
    cnnz: [[u8; 4]; 2],
}

impl MbCode {
    fn new(kind: Kind) -> Self {
        MbCode { kind, skip: false, i16_mode: 0, modes: [2; 16], chroma_mode: 0, mv: [0; 2], cbp: 0, luma: [[0; 16]; 16], dc: [0; 16], cdc: [[0; 4]; 2], cac: [[[0; 16]; 4]; 2], nnz: [0; 16], cnnz: [[0; 4]; 2] }
    }
}

/// The input picture, padded to whole macroblocks by repeating its edges.
pub struct Source {
    pub planes: [Vec<u16>; 3],
}

pub struct FrameCoder<'a> {
    src: &'a Source,
    refp: Option<&'a Picture>,
    half: Option<&'a HalfPel>,
    pub pic: Picture,
    /// The macroblock rows of this slice, and its 1-based id.
    rows: Range<usize>,
    slice_id: u16,
    mb_w: usize,
    mb_h: usize,
    qp: i32,
    qpc: i32,
    lambda: i32,
    p_slice: bool,
    state: Vec<MbState>,
    w: BitWriter,
    /// The CABAC engine once the slice data starts, when the slice uses CABAC.
    cabac: Option<CabacWriter>,
    use_cabac: bool,
    skip_run: u32,
    /// Macroblocks coded as intra, inter and skipped.
    pub counts: [usize; 3],
}

impl<'a> FrameCoder<'a> {
    /// A coder for the slice of macroblock rows `rows` against the reference `refp` (none for an I slice),
    /// whose half samples are `half`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(src: &'a Source, refp: Option<(&'a Picture, &'a HalfPel)>, mb_w: usize, mb_h: usize, qp: i32, rows: Range<usize>, slice_id: u16, use_cabac: bool) -> Self {
        FrameCoder {
            src,
            refp: refp.map(|r| r.0),
            half: refp.map(|r| r.1),
            pic: Picture::new(mb_w, mb_h, 1, 8),
            rows,
            slice_id,
            mb_w,
            mb_h,
            qp,
            qpc: chroma_qp(qp, 0, 8),
            lambda: lambda(qp),
            p_slice: refp.is_some(),
            state: vec![MbState::default(); mb_w * mb_h],
            w: BitWriter::new(),
            cabac: None,
            use_cabac,
            skip_run: 0,
            counts: [0; 3],
        }
    }

    /// Codes the slice; `header` writes the slice header first. Returns the slice RBSP and a picture
    /// holding the slice's reconstruction (not yet loop filtered) and macroblocks.
    pub fn encode(mut self, header: impl FnOnce(&mut BitWriter)) -> (Vec<u8>, Picture, [usize; 3]) {
        header(&mut self.w);
        if self.use_cabac {
            self.w.align_ones();
            let w = std::mem::take(&mut self.w);
            self.cabac = Some(CabacWriter::new(w, !self.p_slice, 0, self.qp));
        }
        for mby in self.rows.clone() {
            for mbx in 0..self.mb_w {
                let code = self.code_mb(mbx, mby);
                if self.use_cabac {
                    let last = mby + 1 == self.rows.end && mbx + 1 == self.mb_w;
                    self.write_mb_cabac(&code, mbx, mby, last);
                } else {
                    self.write_mb(&code, mbx, mby);
                }
                self.finish_mb(&code, mbx, mby);
            }
        }
        let counts = self.counts;
        if let Some(c) = self.cabac.take() {
            return (c.finish(), self.pic, counts);
        }
        if self.skip_run > 0 {
            self.w.ue(self.skip_run);
        }
        (self.w.finish(), self.pic, counts)
    }

    /// Whether the macroblock row above `mby` belongs to this slice.
    fn up(&self, mby: usize) -> bool {
        mby > self.rows.start
    }

    fn ls(&self) -> usize {
        self.mb_w * 16
    }

    fn cs(&self) -> usize {
        self.mb_w * 8
    }

    // ---- neighbours -------------------------------------------------------------------------------

    fn nb_state(&self, mbx: i32, mby: i32) -> Option<&MbState> {
        // macroblocks of earlier slices are not available for prediction
        if mbx < 0 || mby < self.rows.start as i32 || mbx as usize >= self.mb_w || mby as usize >= self.mb_h {
            return None;
        }
        Some(&self.state[mby as usize * self.mb_w + mbx as usize])
    }

    /// Reference index (0, or -1 for intra) and vector of a neighbouring macroblock, if it exists.
    fn nb_motion(&self, mbx: i32, mby: i32) -> Option<(i8, [i16; 2])> {
        self.nb_state(mbx, mby).map(|s| if s.inter { (0, s.mv) } else { (-1, [0, 0]) })
    }

    /// The motion vector predictor of a 16x16 partition (8.4.1.3).
    fn mv_pred(&self, mbx: usize, mby: usize) -> [i16; 2] {
        let (x, y) = (mbx as i32, mby as i32);
        let a = self.nb_motion(x - 1, y);
        let b = self.nb_motion(x, y - 1);
        let c = self.nb_motion(x + 1, y - 1).or_else(|| self.nb_motion(x - 1, y - 1));
        if b.is_none() && c.is_none() {
            if let Some(a) = a {
                return a.1;
            }
        }
        let [a, b, c] = [a, b, c].map(|n| n.unwrap_or((-1, [0, 0])));
        let same: Vec<_> = [a, b, c].into_iter().filter(|n| n.0 == 0).collect();
        if same.len() == 1 {
            return same[0].1;
        }
        [median(a.1[0], b.1[0], c.1[0]), median(a.1[1], b.1[1], c.1[1])]
    }

    /// The vector of a P_Skip macroblock (8.4.1.1).
    fn skip_mv(&self, mbx: usize, mby: usize) -> [i16; 2] {
        let (x, y) = (mbx as i32, mby as i32);
        match (self.nb_motion(x - 1, y), self.nb_motion(x, y - 1)) {
            (None, _) | (_, None) | (Some((0, [0, 0])), _) | (_, Some((0, [0, 0]))) => [0, 0],
            _ => self.mv_pred(mbx, mby),
        }
    }

    /// Coefficient counts of the luma 4x4 blocks left of and above (bx, by), for the CAVLC context.
    fn nc_luma(&self, cur: &[u8; 16], mbx: usize, mby: usize, bx: usize, by: usize) -> i32 {
        let a = if bx > 0 { Some(cur[by * 4 + bx - 1]) } else { self.nb_state(mbx as i32 - 1, mby as i32).map(|s| s.nnz[by * 4 + 3]) };
        let b = if by > 0 { Some(cur[(by - 1) * 4 + bx]) } else { self.nb_state(mbx as i32, mby as i32 - 1).map(|s| s.nnz[12 + bx]) };
        nc(a, b)
    }

    fn nc_chroma(&self, cur: &[u8; 4], comp: usize, mbx: usize, mby: usize, bx: usize, by: usize) -> i32 {
        let a = if bx > 0 { Some(cur[by * 2]) } else { self.nb_state(mbx as i32 - 1, mby as i32).map(|s| s.cnnz[comp][by * 2 + 1]) };
        let b = if by > 0 { Some(cur[bx]) } else { self.nb_state(mbx as i32, mby as i32 - 1).map(|s| s.cnnz[comp][2 + bx]) };
        nc(a, b)
    }

    /// The predicted Intra4x4 mode of raster block (bx, by) given the modes chosen so far in this macroblock.
    fn pred_i4_mode(&self, cur: &[u8; 16], mbx: usize, mby: usize, bx: usize, by: usize) -> u8 {
        let mode_of = |s: Option<&MbState>, i: usize| s.map(|s| if s.i4 { s.modes[i] } else { 2 });
        let a = if bx > 0 { Some(cur[by * 4 + bx - 1]) } else { mode_of(self.nb_state(mbx as i32 - 1, mby as i32), by * 4 + 3) };
        let b = if by > 0 { Some(cur[(by - 1) * 4 + bx]) } else { mode_of(self.nb_state(mbx as i32, mby as i32 - 1), 12 + bx) };
        match (a, b) {
            (Some(a), Some(b)) => a.min(b),
            _ => 2,
        }
    }

    // ---- samples -----------------------------------------------------------------------------------

    fn src_block(&self, plane: usize, x: usize, y: usize, n: usize, out: &mut [i32]) {
        let s = if plane == 0 { self.ls() } else { self.cs() };
        for r in 0..n {
            for c in 0..n {
                out[r * n + c] = self.src.planes[plane][(y + r) * s + x + c] as i32;
            }
        }
    }

    /// SATD of the `w` x `h` source block at (x, y) of `plane` against `pred` (row-major, `w` wide).
    fn satd(&self, plane: usize, x: usize, y: usize, w: usize, h: usize, pred: &[u16]) -> i32 {
        let s = if plane == 0 { self.ls() } else { self.cs() };
        let src = &self.src.planes[plane];
        let mut total = 0;
        for by in (0..h).step_by(4) {
            for bx in (0..w).step_by(4) {
                let mut d = [0i32; 16];
                for r in 0..4 {
                    let o = (y + by + r) * s + x + bx;
                    let (sr, pr) = (&src[o..o + 4], &pred[(by + r) * w + bx..(by + r) * w + bx + 4]);
                    for c in 0..4 {
                        d[r * 4 + c] = sr[c] as i32 - pr[c] as i32;
                    }
                }
                total += satd4(&d);
            }
        }
        total
    }

    fn sad16_int(&self, x0: usize, y0: usize, mv: [i16; 2]) -> i32 {
        let ls = self.ls();
        let rows = self.half.unwrap().rows16(x0 as i32 + (mv[0] >> 2) as i32, y0 as i32 + (mv[1] >> 2) as i32);
        let mut sad = 0;
        for (r, rrow) in rows.enumerate() {
            let srow: &[u16; 16] = self.src.planes[0][(y0 + r) * ls + x0..(y0 + r) * ls + x0 + 16].try_into().unwrap();
            let rrow: &[u8; 16] = rrow.try_into().unwrap();
            for c in 0..16 {
                sad += (srow[c] as i32 - rrow[c] as i32).abs();
            }
        }
        sad
    }

    fn luma_pred(&self, x0: usize, y0: usize, mv: [i16; 2], out: &mut [u16; 256]) {
        self.half.unwrap().predict(x0 as i32, y0 as i32, mv, 16, 16, out);
    }

    // ---- macroblock analysis -------------------------------------------------------------------------

    fn code_mb(&mut self, mbx: usize, mby: usize) -> MbCode {
        let (x0, y0) = (mbx * 16, mby * 16);
        let lam = self.lambda;
        let mut inter = None;
        if self.refp.is_some() {
            // P_Skip first: when the skip vector leaves nothing worth coding, nothing else can be cheaper.
            // A prediction whose differences average a quantiser step or more cannot be skipped.
            let skip_mv = self.skip_mv(mbx, mby);
            let mut pred = [0u16; 256];
            self.luma_pred(x0, y0, skip_mv, &mut pred);
            let ls = self.ls();
            let sad: i32 = (0..256).map(|i| (self.src.planes[0][(y0 + i / 16) * ls + x0 + i % 16] as i32 - pred[i] as i32).abs()).sum();
            if (sad as f64) < 256.0 * 0.625 * 2f64.powf(self.qp as f64 / 6.0) {
                let mut code = self.code_inter(mbx, mby, skip_mv);
                if code.cbp == 0 {
                    code.skip = true;
                    return code;
                }
            }
        }
        let i16 = self.best_i16(mbx, mby);
        let mut best_intra = i16.1 + lam * 4;
        if self.refp.is_some() {
            let mvp = self.mv_pred(mbx, mby);
            let (mv, cost) = self.motion_search(x0, y0, mvp, mbx, mby);
            inter = Some((mv, cost + lam * (mvd_bits(mv, mvp) + 1)));
        }
        // Intra4x4 is tried by coding it, which leaves its reconstruction in place; it is kept only if it wins
        let try_i4 = inter.is_none_or(|(_, c)| best_intra < c);
        let mut i4 = None;
        if try_i4 {
            let code = self.code_i4(mbx, mby);
            let cost = code.1 + lam * 6;
            if cost < best_intra {
                best_intra = cost;
                i4 = Some(code.0);
            }
        }
        if let Some((mv, cost)) = inter {
            if cost <= best_intra {
                let mut code = self.code_inter(mbx, mby, mv);
                if code.cbp == 0 && mv == self.skip_mv(mbx, mby) {
                    code.skip = true;
                }
                return code;
            }
        }
        let mut code = match i4 {
            Some(c) => c,
            None => self.code_i16(mbx, mby, i16.0),
        };
        self.code_intra_chroma(&mut code, mbx, mby);
        code
    }

    fn i16_neighbours(&self, mbx: usize, mby: usize) -> ([i32; 16], [i32; 16], i32) {
        let (x0, y0, ls) = (mbx * 16, mby * 16, self.ls());
        let p = &self.pic.planes[0];
        let mut top = [0; 16];
        let mut left = [0; 16];
        if self.up(mby) {
            for (i, t) in top.iter_mut().enumerate() {
                *t = p[(y0 - 1) * ls + x0 + i] as i32;
            }
        }
        if mbx > 0 {
            for (j, l) in left.iter_mut().enumerate() {
                *l = p[(y0 + j) * ls + x0 - 1] as i32;
            }
        }
        let tl = if mbx > 0 && self.up(mby) { p[(y0 - 1) * ls + x0 - 1] as i32 } else { 0 };
        (top, left, tl)
    }

    /// The best Intra16x16 mode and its SATD.
    fn best_i16(&self, mbx: usize, mby: usize) -> (u8, i32) {
        let (ta, la) = (self.up(mby), mbx > 0);
        let (top, left, tl) = self.i16_neighbours(mbx, mby);
        let mut best = (2, i32::MAX);
        for mode in 0..4u8 {
            let ok = match mode {
                0 => ta,
                1 => la,
                2 => true,
                _ => ta && la,
            };
            if !ok {
                continue;
            }
            let mut pred = [0u16; 256];
            predict_16x16(mode, &top, &left, tl, ta, la, &mut pred, 8);
            let cost = self.satd(0, mbx * 16, mby * 16, 16, 16, &pred) + self.lambda * if mode == 2 { 0 } else { 1 };
            if cost < best.1 {
                best = (mode, cost);
            }
        }
        best
    }

    /// Integer-sample diamond search from the best of a few candidates, then half- and quarter-sample refinement.
    fn motion_search(&self, x0: usize, y0: usize, mvp: [i16; 2], mbx: usize, mby: usize) -> ([i16; 2], i32) {
        let lam = self.lambda;
        let (lo_x, hi_x) = (-(x0 as i32 + 24) * 4, ((self.mb_w * 16 - x0) as i32 + 8) * 4);
        let (lo_y, hi_y) = (-(y0 as i32 + 24) * 4, ((self.mb_h * 16 - y0) as i32 + 8) * 4);
        let clamp = |mv: [i32; 2]| -> [i16; 2] { [mv[0].clamp(lo_x.max(-2048), hi_x.min(2047)) as i16, mv[1].clamp(lo_y.max(-512), hi_y.min(511)) as i16] };
        let int = |mv: [i16; 2]| -> [i16; 2] { [(mv[0] + 2) & !3, (mv[1] + 2) & !3] };
        let cost_int = |mv: [i16; 2]| self.sad16_int(x0, y0, mv) + lam * mvd_bits(mv, mvp);
        let mut cands = vec![int(clamp([mvp[0] as i32, mvp[1] as i32])), [0, 0]];
        for (dx, dy) in [(-1, 0), (0, -1), (1, -1)] {
            if let Some((0, mv)) = self.nb_motion(mbx as i32 + dx, mby as i32 + dy) {
                cands.push(int(clamp([mv[0] as i32, mv[1] as i32])));
            }
        }
        let (mut best, mut best_cost) = ([0i16; 2], i32::MAX);
        for c in cands {
            let cost = cost_int(c);
            if cost < best_cost {
                (best, best_cost) = (c, cost);
            }
        }
        // large diamond steps, then the small diamond
        for step in [16i32, 8, 4] {
            for _ in 0..if step == 4 { 32 } else { 4 } {
                let mut moved = false;
                for (dx, dy) in [(-step, 0), (step, 0), (0, -step), (0, step)] {
                    let c = clamp([best[0] as i32 + dx, best[1] as i32 + dy]);
                    let cost = cost_int(c);
                    if cost < best_cost {
                        (best, best_cost, moved) = (c, cost, true);
                    }
                }
                if !moved {
                    break;
                }
            }
        }
        // sub-sample refinement by SATD
        let mut pred = [0u16; 256];
        let cost_sub = |mv: [i16; 2], pred: &mut [u16; 256]| {
            self.luma_pred(x0, y0, mv, pred);
            self.satd(0, x0, y0, 16, 16, pred) + lam * mvd_bits(mv, mvp)
        };
        best_cost = cost_sub(best, &mut pred);
        for step in [2i32, 1] {
            let centre = best;
            for (dx, dy) in [(-1, -1), (0, -1), (1, -1), (-1, 0), (1, 0), (-1, 1), (0, 1), (1, 1)] {
                let c = clamp([centre[0] as i32 + dx * step, centre[1] as i32 + dy * step]);
                let cost = cost_sub(c, &mut pred);
                if cost < best_cost {
                    (best, best_cost) = (c, cost);
                }
            }
        }
        (best, best_cost - lam * mvd_bits(best, mvp))
    }

    // ---- coding ------------------------------------------------------------------------------------

    /// Transforms and quantises the residual of a 4x4 block; returns the raster coefficients'
    /// levels in scan order and the reconstructed residual's coefficients (raster, dequantised).
    fn quant_block(&self, src: &[i32; 16], pred: &[i32; 16], qp: i32, intra: bool, skip_dc: bool) -> ([i32; 16], [i32; 16], i32) {
        let mut d = [0i32; 16];
        for k in 0..16 {
            d[k] = src[k] - pred[k];
        }
        let c = fdct4(&d);
        let qbits = 15 + (qp / 6) as u32;
        let f = (1 << qbits) / if intra { 3 } else { 6 };
        let mut lv = [0i32; 16];
        let mut co = [0i32; 16];
        for (k, &p) in ZIGZAG4.iter().enumerate() {
            let p = p as usize;
            if skip_dc && p == 0 {
                continue;
            }
            let m = (qp % 6) as usize;
            let l = quant(c[p], MF[m][class(p)], f, qbits);
            lv[k] = l;
            co[p] = (l * V[m][class(p)]) << (qp / 6);
        }
        (lv, co, c[0])
    }

    /// Codes the luma of an Intra4x4 macroblock block by block into the reconstruction; returns
    /// the code and its cost.
    fn code_i4(&mut self, mbx: usize, mby: usize) -> (MbCode, i32) {
        let mut code = MbCode::new(Kind::I4x4);
        let ta = self.up(mby);
        let (la, tla, tra) = (mbx > 0, mbx > 0 && ta, ta && mbx + 1 < self.mb_w);
        let ls = self.ls();
        let mut cost = 0;
        for n in 0..16 {
            let (bx, by) = block_pos(n);
            let (x, y) = (mbx * 16 + bx * 4, mby * 16 + by * 4);
            let have_left = bx > 0 || la;
            let have_top = by > 0 || ta;
            let have_tl = match (bx > 0, by > 0) {
                (true, true) => true,
                (false, true) => la,
                (true, false) => ta,
                (false, false) => tla,
            };
            let have_tr = if by == 0 {
                if bx < 3 {
                    ta
                } else {
                    tra
                }
            } else if bx == 3 {
                false
            } else {
                zorder(bx + 1, by - 1) < zorder(bx, by)
            };
            let p = &self.pic.planes[0];
            let mut e = Edges { top: [0; 16], left: [0; 8], topleft: 0, have_top, have_left, have_topleft: have_tl };
            if have_top {
                for i in 0..4 {
                    e.top[i] = p[(y - 1) * ls + x + i] as i32;
                }
                for i in 4..8 {
                    e.top[i] = if have_tr { p[(y - 1) * ls + x + i] as i32 } else { e.top[3] };
                }
            }
            if have_left {
                for j in 0..4 {
                    e.left[j] = p[(y + j) * ls + x - 1] as i32;
                }
            }
            if have_tl {
                e.topleft = p[(y - 1) * ls + x - 1] as i32;
            }
            let predicted = self.pred_i4_mode(&code.modes, mbx, mby, bx, by);
            let mut best = (2u8, i32::MAX, [0u16; 16]);
            for mode in 0..9u8 {
                let ok = match mode {
                    0 | 3 | 7 => have_top,
                    1 | 8 => have_left,
                    2 => true,
                    _ => have_top && have_left && have_tl,
                };
                if !ok {
                    continue;
                }
                let mut pred = [0u16; 16];
                predict_nxn(4, mode, &e, &mut pred, 8);
                let c = self.satd(0, x, y, 4, 4, &pred) + self.lambda * if mode == predicted { 1 } else { 4 };
                if c < best.1 {
                    best = (mode, c, pred);
                }
            }
            cost += best.1;
            code.modes[by * 4 + bx] = best.0;
            let mut s = [0i32; 16];
            self.src_block(0, x, y, 4, &mut s);
            let pred: [i32; 16] = best.2.map(|v| v as i32);
            let (lv, co, _) = self.quant_block(&s, &pred, self.qp, true, false);
            let p = &mut self.pic.planes[0];
            for r in 0..4 {
                p[(y + r) * ls + x..(y + r) * ls + x + 4].copy_from_slice(&best.2[r * 4..r * 4 + 4]);
            }
            if lv.iter().any(|&l| l != 0) {
                idct4_add(&co, &mut p[y * ls + x..], ls, 255);
            }
            code.nnz[by * 4 + bx] = lv.iter().filter(|&&l| l != 0).count() as u8;
            code.luma[by * 4 + bx] = lv;
        }
        for i8 in 0..4 {
            let (bx, by) = ((i8 & 1) * 2, (i8 >> 1) * 2);
            if (0..2).any(|dy| (0..2).any(|dx| code.nnz[(by + dy) * 4 + bx + dx] > 0)) {
                code.cbp |= 1 << i8;
            }
        }
        (code, cost)
    }

    fn code_i16(&mut self, mbx: usize, mby: usize, mode: u8) -> MbCode {
        let mut code = MbCode::new(Kind::I16x16);
        code.i16_mode = mode;
        let (top, left, tl) = self.i16_neighbours(mbx, mby);
        let mut pred = [0u16; 256];
        predict_16x16(mode, &top, &left, tl, self.up(mby), mbx > 0, &mut pred, 8);
        let (x0, y0, ls) = (mbx * 16, mby * 16, self.ls());
        let qp = self.qp;
        let mut dcs = [0i32; 16];
        let mut cos = [[0i32; 16]; 16];
        for b in 0..16 {
            let (bx, by) = (b % 4, b / 4);
            let mut s = [0i32; 16];
            self.src_block(0, x0 + bx * 4, y0 + by * 4, 4, &mut s);
            let mut pr = [0i32; 16];
            for r in 0..4 {
                for c in 0..4 {
                    pr[r * 4 + c] = pred[(by * 4 + r) * 16 + bx * 4 + c] as i32;
                }
            }
            let (lv, co, dc) = self.quant_block(&s, &pr, qp, true, true);
            code.luma[b] = lv;
            cos[b] = co;
            dcs[b] = dc;
        }
        // the DC coefficients: Hadamard, halved, quantised with doubled precision
        let h = hadamard4(&dcs);
        let qbits = 16 + (qp / 6) as u32;
        let f = (1 << qbits) / 3;
        let m = (qp % 6) as usize;
        let mut dl = [0i32; 16];
        for (k, &p) in ZIGZAG4.iter().enumerate() {
            code.dc[k] = quant(h[p as usize] >> 1, MF[m][0], f, qbits);
            dl[p as usize] = code.dc[k];
        }
        let ac_coded = code.luma.iter().any(|b| b.iter().any(|&l| l != 0));
        if !ac_coded {
            code.luma = [[0; 16]; 16];
            cos = [[0; 16]; 16];
        }
        code.cbp = if ac_coded { 15 } else { 0 };
        // reconstruct as the decoder does
        let f = hadamard4(&dl);
        let lsd = 16 * V[m][0];
        for y in 0..16 {
            self.pic.planes[0][(y0 + y) * ls + x0..(y0 + y) * ls + x0 + 16].copy_from_slice(&pred[y * 16..y * 16 + 16]);
        }
        for b in 0..16 {
            let mut co = cos[b];
            co[0] = if qp >= 36 { (f[b] * lsd) << (qp / 6 - 6) } else { (f[b] * lsd + (1 << (5 - qp / 6))) >> (6 - qp / 6) };
            if co.iter().any(|&v| v != 0) {
                let (bx, by) = (b % 4, b / 4);
                idct4_add(&co, &mut self.pic.planes[0][(y0 + by * 4) * ls + x0 + bx * 4..], ls, 255);
            }
            code.nnz[b] = code.luma[b].iter().filter(|&&l| l != 0).count() as u8;
        }
        code
    }

    fn code_inter(&mut self, mbx: usize, mby: usize, mv: [i16; 2]) -> MbCode {
        let refp = self.refp.unwrap();
        let mut code = MbCode::new(Kind::Inter);
        code.mv = mv;
        let (x0, y0, ls) = (mbx * 16, mby * 16, self.ls());
        let mut pred = [0u16; 256];
        self.luma_pred(x0, y0, mv, &mut pred);
        let mut cos = [[0i32; 16]; 16];
        let mut score = [0i32; 4];
        for b in 0..16 {
            let (bx, by) = (b % 4, b / 4);
            let mut s = [0i32; 16];
            self.src_block(0, x0 + bx * 4, y0 + by * 4, 4, &mut s);
            let mut pr = [0i32; 16];
            for r in 0..4 {
                for c in 0..4 {
                    pr[r * 4 + c] = pred[(by * 4 + r) * 16 + bx * 4 + c] as i32;
                }
            }
            let (lv, co, _) = self.quant_block(&s, &pr, self.qp, false, false);
            score[(by / 2) * 2 + bx / 2] += decimate_score(&lv);
            code.luma[b] = lv;
            cos[b] = co;
        }
        // drop 8x8 blocks (and whole macroblocks) whose few small levels cost more than they give
        let total: i32 = score.iter().sum();
        for i8 in 0..4 {
            if score[i8] < 4 || total < 6 {
                for dy in 0..2 {
                    for dx in 0..2 {
                        let b = ((i8 >> 1) * 2 + dy) * 4 + (i8 & 1) * 2 + dx;
                        code.luma[b] = [0; 16];
                        cos[b] = [0; 16];
                    }
                }
            }
        }
        for y in 0..16 {
            self.pic.planes[0][(y0 + y) * ls + x0..(y0 + y) * ls + x0 + 16].copy_from_slice(&pred[y * 16..y * 16 + 16]);
        }
        for b in 0..16 {
            code.nnz[b] = code.luma[b].iter().filter(|&&l| l != 0).count() as u8;
            if code.nnz[b] > 0 {
                let (bx, by) = (b % 4, b / 4);
                idct4_add(&cos[b], &mut self.pic.planes[0][(y0 + by * 4) * ls + x0 + bx * 4..], ls, 255);
                code.cbp |= 1 << ((by / 2) * 2 + bx / 2);
            }
        }
        // chroma from the same vector, in eighth samples
        let cs = self.cs();
        let mut cpred = [[0u16; 64]; 2];
        for (c, pr) in cpred.iter_mut().enumerate() {
            chroma_mc(&refp.planes[1 + c], cs, self.mb_h * 8, (mbx * 8) as i32, (mby * 8) as i32, mv[0] as i32, mv[1] as i32, 8, 8, pr);
        }
        self.code_chroma(&mut code, mbx, mby, &cpred, false);
        code
    }

    fn code_intra_chroma(&mut self, code: &mut MbCode, mbx: usize, mby: usize) {
        let (la, ta) = (mbx > 0, self.up(mby));
        let (x0, y0, cs) = (mbx * 8, mby * 8, self.cs());
        let mut nbs = [([0i32; 8], [0i32; 16], 0i32); 2];
        for (c, nb) in nbs.iter_mut().enumerate() {
            let p = &self.pic.planes[1 + c];
            if ta {
                for i in 0..8 {
                    nb.0[i] = p[(y0 - 1) * cs + x0 + i] as i32;
                }
            }
            if la {
                for j in 0..8 {
                    nb.1[j] = p[(y0 + j) * cs + x0 - 1] as i32;
                }
            }
            if la && ta {
                nb.2 = p[(y0 - 1) * cs + x0 - 1] as i32;
            }
        }
        let mut best = (0u8, i32::MAX, [[0u16; 64]; 2]);
        for mode in 0..4u8 {
            let ok = match mode {
                0 => true,
                1 => la,
                2 => ta,
                _ => la && ta,
            };
            if !ok {
                continue;
            }
            let mut preds = [[0u16; 64]; 2];
            let mut cost = self.lambda * if mode == 0 { 1 } else { 3 };
            for c in 0..2 {
                let mut out = [0u16; 128];
                predict_chroma(mode, &nbs[c].0, &nbs[c].1, nbs[c].2, ta, la, &mut out, 8, 8);
                preds[c].copy_from_slice(&out[..64]);
                cost += self.satd(1 + c, x0, y0, 8, 8, &preds[c]);
            }
            if cost < best.1 {
                best = (mode, cost, preds);
            }
        }
        code.chroma_mode = best.0;
        self.code_chroma(code, mbx, mby, &best.2, true);
    }

    /// Codes both chroma components of a macroblock against `pred` and reconstructs them; sets
    /// the chroma part of the coded block pattern.
    fn code_chroma(&mut self, code: &mut MbCode, mbx: usize, mby: usize, pred: &[[u16; 64]; 2], intra: bool) {
        let (x0, y0, cs) = (mbx * 8, mby * 8, self.cs());
        let qp = self.qpc;
        let m = (qp % 6) as usize;
        let mut cos = [[[0i32; 16]; 4]; 2];
        let mut any_dc = false;
        let mut any_ac = false;
        for c in 0..2 {
            let mut dcs = [0i32; 4];
            for blk in 0..4 {
                let (bx, by) = (blk & 1, blk >> 1);
                let mut s = [0i32; 16];
                self.src_block(1 + c, x0 + bx * 4, y0 + by * 4, 4, &mut s);
                let mut pr = [0i32; 16];
                for r in 0..4 {
                    for k in 0..4 {
                        pr[r * 4 + k] = pred[c][(by * 4 + r) * 8 + bx * 4 + k] as i32;
                    }
                }
                let (lv, co, dc) = self.quant_block(&s, &pr, qp, intra, true);
                code.cac[c][blk] = lv;
                cos[c][blk] = co;
                dcs[blk] = dc;
                any_ac |= lv.iter().any(|&l| l != 0);
            }
            let f = [dcs[0] + dcs[1] + dcs[2] + dcs[3], dcs[0] - dcs[1] + dcs[2] - dcs[3], dcs[0] + dcs[1] - dcs[2] - dcs[3], dcs[0] - dcs[1] - dcs[2] + dcs[3]];
            let qbits = 16 + (qp / 6) as u32;
            let fr = (1 << qbits) / if intra { 3 } else { 6 };
            for k in 0..4 {
                code.cdc[c][k] = quant(f[k], MF[m][0], fr, qbits);
            }
            any_dc |= code.cdc[c].iter().any(|&l| l != 0);
        }
        // a lone small AC level is not worth signalling chroma AC for
        let ac_score: i32 = code.cac.iter().flat_map(|c| c.iter()).map(decimate_score).sum();
        if !intra && ac_score < 7 {
            any_ac = false;
        }
        if !any_ac {
            code.cac = [[[0; 16]; 4]; 2];
            cos = [[[0; 16]; 4]; 2];
        }
        let cbpc = if any_ac { 2 } else if any_dc { 1 } else { 0 };
        code.cbp |= cbpc << 4;
        let lsd = 16 * V[m][0];
        for c in 0..2 {
            let l = code.cdc[c];
            let f = [l[0] + l[1] + l[2] + l[3], l[0] - l[1] + l[2] - l[3], l[0] + l[1] - l[2] - l[3], l[0] - l[1] - l[2] + l[3]];
            let plane = &mut self.pic.planes[1 + c];
            for y in 0..8 {
                plane[(y0 + y) * cs + x0..(y0 + y) * cs + x0 + 8].copy_from_slice(&pred[c][y * 8..y * 8 + 8]);
            }
            for blk in 0..4 {
                let mut co = cos[c][blk];
                co[0] = if cbpc > 0 { ((f[blk] * lsd) << (qp / 6)) >> 5 } else { 0 };
                code.cnnz[c][blk] = code.cac[c][blk].iter().filter(|&&v| v != 0).count() as u8;
                if co.iter().any(|&v| v != 0) {
                    let (bx, by) = (blk & 1, blk >> 1);
                    idct4_add(&co, &mut plane[(y0 + by * 4) * cs + x0 + bx * 4..], cs, 255);
                }
            }
        }
    }

    // ---- syntax ------------------------------------------------------------------------------------

    fn write_mb(&mut self, code: &MbCode, mbx: usize, mby: usize) {
        if self.p_slice {
            if code.skip {
                self.skip_run += 1;
                return;
            }
            self.w.ue(self.skip_run);
            self.skip_run = 0;
        }
        let base = if self.p_slice { 5 } else { 0 };
        let cbpl = code.cbp & 15;
        let cbpc = code.cbp >> 4;
        match code.kind {
            Kind::I4x4 => {
                self.w.ue(base);
                let mut so_far = [2u8; 16];
                for n in 0..16 {
                    let (bx, by) = block_pos(n);
                    let mode = code.modes[by * 4 + bx];
                    let pred = self.pred_i4_mode(&so_far, mbx, mby, bx, by);
                    so_far[by * 4 + bx] = mode;
                    if mode == pred {
                        self.w.flag(true);
                    } else {
                        self.w.flag(false);
                        self.w.put(3, if mode < pred { mode } else { mode - 1 } as u32);
                    }
                }
                self.w.ue(code.chroma_mode as u32);
                self.w.ue(GOLOMB_TO_INTRA4X4_CBP.iter().position(|&c| c == code.cbp).unwrap() as u32);
            }
            Kind::I16x16 => {
                self.w.ue(base + 1 + code.i16_mode as u32 + 4 * cbpc as u32 + if cbpl != 0 { 12 } else { 0 });
                self.w.ue(code.chroma_mode as u32);
            }
            _ => {
                self.w.ue(0); // P_L0_16x16
                let mvp = self.mv_pred(mbx, mby);
                self.w.se(code.mv[0] as i32 - mvp[0] as i32);
                self.w.se(code.mv[1] as i32 - mvp[1] as i32);
                self.w.ue(GOLOMB_TO_INTER_CBP.iter().position(|&c| c == code.cbp).unwrap() as u32);
            }
        }
        if code.cbp == 0 && code.kind != Kind::I16x16 {
            return;
        }
        self.w.se(0); // mb_qp_delta
        if code.kind == Kind::I16x16 {
            let nc = self.nc_luma(&code.nnz, mbx, mby, 0, 0);
            write_block(&mut self.w, nc, &code.dc);
        }
        for n in 0..16 {
            let (bx, by) = block_pos(n);
            if cbpl & (1 << (n / 4)) == 0 {
                continue;
            }
            let nc = self.nc_luma(&code.nnz, mbx, mby, bx, by);
            let b = &code.luma[by * 4 + bx];
            if code.kind == Kind::I16x16 {
                write_block(&mut self.w, nc, &b[1..]);
            } else {
                write_block(&mut self.w, nc, b);
            }
        }
        if cbpc > 0 {
            for c in 0..2 {
                write_block(&mut self.w, -1, &code.cdc[c]);
            }
        }
        if cbpc == 2 {
            for c in 0..2 {
                for blk in 0..4 {
                    let nc = self.nc_chroma(&code.cnnz[c], c, mbx, mby, blk & 1, blk >> 1);
                    write_block(&mut self.w, nc, &code.cac[c][blk][1..]);
                }
            }
        }
    }

    /// Writes a macroblock with CABAC, followed by end_of_slice_flag; the contexts follow the
    /// decoder's (9.3.3.1.1 and 9.3.3.1.3).
    fn write_mb_cabac(&mut self, code: &MbCode, mbx: usize, mby: usize, last: bool) {
        let (x, y) = (mbx as i32, mby as i32);
        let left = self.nb_state(x - 1, y).copied();
        let top = self.nb_state(x, y - 1).copied();
        let mut c = self.cabac.take().unwrap();
        let cond = |n: Option<MbState>, f: &dyn Fn(&MbState) -> bool| n.as_ref().is_some_and(f) as usize;
        if self.p_slice {
            c.decision(11 + cond(left, &|m| !m.skip) + cond(top, &|m| !m.skip), code.skip);
            if code.skip {
                c.terminate(last);
                self.cabac = Some(c);
                return;
            }
        }
        let cbpl = code.cbp & 15;
        let cbpc = code.cbp >> 4;
        // mb_type
        let (prefix_ok, base) = (self.p_slice, if self.p_slice { 17 } else { 3 });
        match code.kind {
            Kind::Inter => {
                // P_L0_16x16
                c.decision(14, false);
                c.decision(15, false);
                c.decision(16, false);
            }
            kind => {
                let first = if prefix_ok {
                    c.decision(14, true);
                    17
                } else {
                    3 + cond(left, &|m| m.kind != Kind::I4x4) + cond(top, &|m| m.kind != Kind::I4x4)
                };
                if kind == Kind::I4x4 {
                    c.decision(first, false);
                } else {
                    c.decision(first, true);
                    c.terminate(false); // not I_PCM
                    let (b1, b2, b3) = if prefix_ok { (base + 1, base + 2, base + 3) } else { (base + 3, base + 4, base + 6) };
                    c.decision(b1, cbpl != 0);
                    c.decision(b2, cbpc != 0);
                    if cbpc != 0 {
                        c.decision(if prefix_ok { base + 2 } else { base + 5 }, cbpc == 2);
                    }
                    c.decision(b3, code.i16_mode >> 1 == 1);
                    c.decision(if prefix_ok { base + 3 } else { base + 7 }, code.i16_mode & 1 == 1);
                }
            }
        }
        if code.kind == Kind::I4x4 {
            let mut so_far = [2u8; 16];
            for n in 0..16 {
                let (bx, by) = block_pos(n);
                let mode = code.modes[by * 4 + bx];
                let pred = self.pred_i4_mode(&so_far, mbx, mby, bx, by);
                so_far[by * 4 + bx] = mode;
                c.decision(68, mode == pred);
                if mode != pred {
                    let rem = if mode < pred { mode } else { mode - 1 };
                    for k in 0..3 {
                        c.decision(69, (rem >> k) & 1 == 1);
                    }
                }
            }
        }
        if code.kind != Kind::Inter {
            let m = code.chroma_mode;
            let ch = |n: &MbState| !n.inter && n.chroma_mode != 0;
            c.decision(64 + cond(left, &ch) + cond(top, &ch), m > 0);
            if m > 0 {
                c.decision(67, m > 1);
                if m > 1 {
                    c.decision(67, m > 2);
                }
            }
        } else {
            let p = self.mv_pred(mbx, mby);
            for comp in 0..2 {
                let d = code.mv[comp] as i32 - p[comp] as i32;
                let sum = left.map_or(0, |m| m.mvd[comp] as u32) + top.map_or(0, |m| m.mvd[comp] as u32);
                let base = if comp == 0 { 40 } else { 47 };
                let inc = if sum < 3 { 0 } else if sum > 32 { 2 } else { 1 };
                let abs = d.unsigned_abs();
                c.decision(base + inc, abs > 0);
                if abs > 0 {
                    let mut ctx = base + 3;
                    for v in 1..abs.min(9) {
                        let _ = v;
                        c.decision(ctx, true);
                        if ctx < base + 6 {
                            ctx += 1;
                        }
                    }
                    if abs < 9 {
                        c.decision(ctx, false);
                    } else {
                        c.exp_golomb(abs - 9, 3);
                    }
                    c.bypass(d < 0);
                }
            }
        }
        if code.kind != Kind::I16x16 {
            let mut luma = 0u8;
            for b8 in 0..4usize {
                let cond_a = if b8 & 1 == 1 { (luma >> (b8 - 1)) & 1 == 0 } else { left.is_some_and(|m| (m.cbp >> (b8 + 1)) & 1 == 0) };
                let cond_b = if b8 >= 2 { (luma >> (b8 - 2)) & 1 == 0 } else { top.is_some_and(|m| (m.cbp >> (b8 + 2)) & 1 == 0) };
                let bit = (cbpl >> b8) & 1;
                c.decision(73 + cond_a as usize + 2 * cond_b as usize, bit == 1);
                luma |= bit << b8;
            }
            let ca = cond(left, &|m| m.cbp >> 4 != 0);
            let cb = cond(top, &|m| m.cbp >> 4 != 0);
            c.decision(77 + ca + 2 * cb, cbpc != 0);
            if cbpc != 0 {
                let ca = cond(left, &|m| m.cbp >> 4 == 2);
                let cb = cond(top, &|m| m.cbp >> 4 == 2);
                c.decision(77 + 4 + ca + 2 * cb, cbpc == 2);
            }
        }
        if code.cbp != 0 || code.kind == Kind::I16x16 {
            c.decision(60, false); // mb_qp_delta 0, after a macroblock without a change
            // an unavailable neighbour counts as a coded block for an intra macroblock
            let un = code.kind != Kind::Inter;
            let flag = |n: Option<u8>| n.map_or(un, |v| v > 0) as usize;
            if code.kind == Kind::I16x16 {
                let dc = |n: Option<MbState>| n.map_or(un, |m| m.cbf_dc & 1 != 0) as usize;
                cabac::write_block(&mut c, 0, 85 + dc(left) + 2 * dc(top), &code.dc);
            }
            let (cat, base) = if code.kind == Kind::I16x16 { (1, 85 + 4) } else { (2, 85 + 8) };
            for n in 0..16 {
                let (bx, by) = block_pos(n);
                if cbpl & (1 << (n / 4)) == 0 {
                    continue;
                }
                let a = if bx > 0 { Some(code.nnz[by * 4 + bx - 1]) } else { left.map(|m| m.nnz[by * 4 + 3]) };
                let b = if by > 0 { Some(code.nnz[(by - 1) * 4 + bx]) } else { top.map(|m| m.nnz[12 + bx]) };
                let blk = &code.luma[by * 4 + bx];
                cabac::write_block(&mut c, cat, base + flag(a) + 2 * flag(b), if cat == 1 { &blk[1..] } else { blk });
            }
            if cbpc > 0 {
                for comp in 0..2 {
                    let dc = |n: Option<MbState>| n.map_or(un, |m| m.cbf_dc & (2 << comp) != 0) as usize;
                    cabac::write_block(&mut c, 3, 85 + 12 + dc(left) + 2 * dc(top), &code.cdc[comp]);
                }
            }
            if cbpc == 2 {
                for comp in 0..2 {
                    for blk in 0..4 {
                        let (bx, by) = (blk & 1, blk >> 1);
                        let cur = &code.cnnz[comp];
                        let a = if bx > 0 { Some(cur[by * 2]) } else { left.map(|m| m.cnnz[comp][by * 2 + 1]) };
                        let b = if by > 0 { Some(cur[bx]) } else { top.map(|m| m.cnnz[comp][2 + bx]) };
                        cabac::write_block(&mut c, 4, 85 + 16 + flag(a) + 2 * flag(b), &code.cac[comp][blk][1..]);
                    }
                }
            }
        }
        c.terminate(last);
        self.cabac = Some(c);
    }

    fn finish_mb(&mut self, code: &MbCode, mbx: usize, mby: usize) {
        let idx = mby * self.mb_w + mbx;
        let inter = code.kind == Kind::Inter;
        let mvd = if inter && !code.skip {
            let p = self.mv_pred(mbx, mby);
            [0, 1].map(|c| (code.mv[c] as i32 - p[c] as i32).unsigned_abs().min(64) as u8)
        } else {
            [0, 0]
        };
        let cbpc = code.cbp >> 4;
        let cbf_dc = (code.kind == Kind::I16x16 && code.dc.iter().any(|&l| l != 0)) as u8
            | ((cbpc > 0 && code.cdc[0].iter().any(|&l| l != 0)) as u8) << 1
            | ((cbpc > 0 && code.cdc[1].iter().any(|&l| l != 0)) as u8) << 2;
        self.state[idx] = MbState {
            i4: code.kind == Kind::I4x4,
            inter,
            modes: code.modes,
            nnz: code.nnz,
            cnnz: code.cnnz,
            mv: code.mv,
            skip: code.skip,
            kind: code.kind,
            cbp: code.cbp,
            chroma_mode: if inter { 0 } else { code.chroma_mode },
            cbf_dc,
            mvd,
        };
        self.counts[if code.skip { 2 } else if inter { 1 } else { 0 }] += 1;
        let mut info = MbInfo { slice: self.slice_id, kind: code.kind, qp: self.qp as i8, ..MbInfo::default() };
        info.nnz[..16].copy_from_slice(&code.nnz);
        info.cbp = code.cbp;
        info.skip = code.skip;
        if inter {
            info.mv[0] = [code.mv; 16];
            info.ref_idx[0] = [0; 4];
            info.ref_id[0] = [1; 4];
        }
        self.pic.mbs[idx] = info;
    }
}


/// The neighbour-derived CAVLC context nC (9.2.1).
fn nc(a: Option<u8>, b: Option<u8>) -> i32 {
    match (a, b) {
        (Some(a), Some(b)) => (a as i32 + b as i32 + 1) >> 1,
        (Some(a), None) => a as i32,
        (None, Some(b)) => b as i32,
        (None, None) => 0,
    }
}

fn zorder(bx: usize, by: usize) -> usize {
    ((by >> 1) << 3) | ((bx >> 1) << 2) | ((by & 1) << 1) | (bx & 1)
}

/// How much a block's levels are worth keeping: large for any level above 1, else a little per
/// lone 1 weighted by how close it is to the next (x264's decimation score).
fn decimate_score(lv: &[i32; 16]) -> i32 {
    const RUN: [i32; 16] = [3, 2, 2, 1, 1, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];
    let mut score = 0;
    let mut i = 15i32;
    while i >= 0 && lv[i as usize] == 0 {
        i -= 1;
    }
    while i >= 0 {
        if lv[i as usize].abs() > 1 {
            return 9;
        }
        i -= 1;
        let mut run = 0;
        while i >= 0 && lv[i as usize] == 0 {
            i -= 1;
            run += 1;
        }
        score += RUN[run.min(15)];
    }
    score
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn forward_and_inverse_transforms_round_trip_at_low_qp() {
        // quantising at QP 0 and reconstructing returns the residual to within rounding
        let src: [i32; 16] = std::array::from_fn(|i| ((i * 37) % 23) as i32 * 9);
        let pred = [100i32; 16];
        let mf = |p: usize| MF[0][class(p)];
        let c = {
            let mut d = [0i32; 16];
            for k in 0..16 {
                d[k] = src[k] - pred[k];
            }
            fdct4(&d)
        };
        let mut co = [0i32; 16];
        for p in 0..16 {
            let l = quant(c[p], mf(p), 1 << 14, 15);
            co[p] = l * V[0][class(p)];
        }
        let mut out = [100u16; 16];
        idct4_add(&co, &mut out, 4, 255);
        for k in 0..16 {
            assert!((out[k] as i32 - src[k]).abs() <= 1, "{k}: {} vs {}", out[k], src[k]);
        }
    }

    #[test]
    fn decimation_scores_lone_ones_low_and_big_levels_high() {
        let mut lv = [0i32; 16];
        assert_eq!(decimate_score(&lv), 0);
        lv[15] = 1;
        assert_eq!(decimate_score(&lv), 0, "a lone level after a long run of zeros");
        lv[15] = 0;
        lv[0] = 1;
        assert_eq!(decimate_score(&lv), 3);
        lv[3] = -2;
        assert_eq!(decimate_score(&lv), 9);
    }
}
