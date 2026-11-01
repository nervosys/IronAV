//! Encoding one picture as one slice: mode decision, motion search, the forward transform and
//! quantisation, reconstruction (with the decoder's own prediction, inverse transform and loop
//! filter, so the reference the encoder keeps is the one a decoder builds) and CAVLC or CABAC macroblock
//! syntax. With CABAC the macroblock type is chosen by rate-distortion cost, with CAVLC by SATD.

use super::bitw::{se_bits, BitWriter};
use super::cabac::{self, Bins, CabacWriter};
use super::cavlc::write_block;
use crate::h264::idct::{hadamard4, idct4_add, idct8_add};
use super::subpel::HalfPel;
use crate::h264::inter::chroma_mc;
use crate::h264::intra::{filter_8x8_edges, predict_16x16, predict_chroma, predict_nxn, Edges};
use crate::h264::mb::{chroma_qp, Kind, MbInfo, Picture};
use std::ops::Range;
use crate::h264::ps::ZIGZAG4;
use crate::h264::tables::{DEQUANT8_COEFF_INIT, GOLOMB_TO_INTER_CBP, GOLOMB_TO_INTRA4X4_CBP, ZIGZAG_DIRECT};

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

/// Forward quantisation multipliers of the 8x8 transform by QP % 6 and coefficient class (`class8`),
/// the counterparts of the decoder's `DEQUANT8_COEFF_INIT`.
const MF8: [[i32; 6]; 6] = [
    [13107, 11428, 20972, 12222, 16777, 15481],
    [11916, 10826, 19174, 11058, 14980, 14290],
    [10082, 8943, 15978, 9675, 12710, 11985],
    [9362, 8228, 14913, 8931, 11984, 11259],
    [8192, 7346, 13159, 7740, 10486, 9777],
    [7282, 6428, 11570, 6830, 9118, 8640],
];

/// The class of raster position `p` of an 8x8 block, as the decoder's level scale numbers them.
fn class8(p: usize) -> usize {
    let (i, j) = (p / 8, p % 8);
    if i % 4 == 0 && j % 4 == 0 {
        0
    } else if i % 2 == 1 && j % 2 == 1 {
        1
    } else if i % 4 == 2 && j % 4 == 2 {
        2
    } else if (i % 4 == 0 && j % 2 == 1) || (i % 2 == 1 && j % 4 == 0) {
        3
    } else if (i % 4 == 0 && j % 4 == 2) || (i % 4 == 2 && j % 4 == 0) {
        4
    } else {
        5
    }
}

/// One dimension of the 8x8 forward core transform.
fn fdct8_1d(d: [i32; 8]) -> [i32; 8] {
    let (s07, s16, s25, s34) = (d[0] + d[7], d[1] + d[6], d[2] + d[5], d[3] + d[4]);
    let (a0, a1, a2, a3) = (s07 + s34, s16 + s25, s07 - s34, s16 - s25);
    let (d07, d16, d25, d34) = (d[0] - d[7], d[1] - d[6], d[2] - d[5], d[3] - d[4]);
    let a4 = d16 + d25 + (d07 + (d07 >> 1));
    let a5 = d07 - d34 - (d25 + (d25 >> 1));
    let a6 = d07 + d34 - (d16 + (d16 >> 1));
    let a7 = d16 - d25 + (d34 + (d34 >> 1));
    [a0 + a1, a4 + (a7 >> 2), a2 + (a3 >> 1), a5 + (a6 >> 2), a0 - a1, a6 - (a5 >> 2), (a2 >> 1) - a3, (a4 >> 2) - a7]
}

/// The 8x8 forward core transform, row-major.
fn fdct8(d: &[i32; 64]) -> [i32; 64] {
    let mut t = [0i32; 64];
    for r in 0..8 {
        t[r * 8..r * 8 + 8].copy_from_slice(&fdct8_1d(d[r * 8..r * 8 + 8].try_into().unwrap()));
    }
    let mut o = [0i32; 64];
    for c in 0..8 {
        let v = fdct8_1d(std::array::from_fn(|r| t[r * 8 + c]));
        for r in 0..8 {
            o[r * 8 + c] = v[r];
        }
    }
    o
}

/// Quantises the residual of an 8x8 block at `qp`; returns the levels in 8x8 scan order and the
/// dequantised coefficients (raster) as the decoder scales them.
fn quant8(src: &[i32; 64], pred: &[i32; 64], qp: i32, intra: bool) -> ([i32; 64], [i32; 64]) {
    let c = fdct8(&std::array::from_fn(|k| src[k] - pred[k]));
    let (m, qbits) = ((qp % 6) as usize, 16 + (qp / 6) as u32);
    let f = (1 << qbits) / if intra { 3 } else { 6 };
    let (mut lv, mut co) = ([0i32; 64], [0i32; 64]);
    for (k, &p) in ZIGZAG_DIRECT.iter().enumerate() {
        let p = p as usize;
        let l = quant(c[p], MF8[m][class8(p)], f, qbits);
        lv[k] = l;
        if l != 0 {
            let v = l * 16 * DEQUANT8_COEFF_INIT[m][class8(p)] as i32;
            co[p] = if qp >= 36 { v << (qp / 6 - 6) } else { (v + (1 << (5 - qp / 6))) >> (6 - qp / 6) };
        }
    }
    (lv, co)
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

/// The partitions of each inter partitioning (16x16, 16x8, 8x16, 8x8), as (x, y, width, height) in
/// 4x4 blocks, in decoding order.
/// The reference indices of blocks that use none (for intra and B neighbours' in-macroblock lookups).
const NO_REF: [i8; 16] = [0; 16];

const PARTS: [&[(usize, usize, usize, usize)]; 4] = [&[(0, 0, 4, 4)], &[(0, 0, 4, 2), (0, 2, 4, 2)], &[(0, 0, 2, 4), (2, 0, 2, 4)], &[(0, 0, 2, 2), (2, 0, 2, 2), (0, 2, 2, 2), (2, 2, 2, 2)]];

/// A value per 4x4 block, raster order, from one per partition of `part`.
fn spread<T: Copy + Default>(part: u8, v: &[T; 4]) -> [T; 16] {
    let mut out = [T::default(); 16];
    for (k, &(x, y, w, h)) in PARTS[part as usize].iter().enumerate() {
        for by in y..y + h {
            for bx in x..x + w {
                out[by * 4 + bx] = v[k];
            }
        }
    }
    out
}

/// The absolute vector differences per 4x4 block of partitioning `part`, capped for the CABAC contexts.
fn mvd_grid(part: u8, mvds: &[[i32; 2]; 4]) -> [[u8; 2]; 16] {
    spread(part, &mvds.map(|d| d.map(|v| v.unsigned_abs().min(64) as u8)))
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
    /// Vectors per 4x4 block, raster order, in lists 0 and 1, and the reference index per list and
    /// 8x8 block (-1 for none).
    mvs: [[i16; 2]; 16],
    mvs1: [[i16; 2]; 16],
    refs: [[i8; 4]; 2],
    /// B_Skip or B_Direct_16x16.
    bdirect: bool,
    // for the CABAC contexts
    t8x8: bool,
    skip: bool,
    kind: Kind,
    cbp: u8,
    chroma_mode: u8,
    /// Coded DC blocks: bit 0 Intra16x16 luma, bit 1 Cb, bit 2 Cr.
    cbf_dc: u8,
    /// Absolute motion vector differences per 4x4 block and list, capped at 64.
    mvd: [[u8; 2]; 16],
    mvd1: [[u8; 2]; 16],
}

/// A macroblock's decisions and levels, ready to write.
struct MbCode {
    kind: Kind,
    skip: bool,
    i16_mode: u8,
    modes: [u8; 16],
    chroma_mode: u8,
    /// Inter partitioning (an index of `PARTS`), and each partition's vector and its difference
    /// from the predicted one.
    part: u8,
    mvs: [[i16; 2]; 4],
    mvds: [[i32; 2]; 4],
    /// The reference index of each partition in a P macroblock.
    refs: [u8; 4],
    /// In a B slice, per partition of `part`: the direction (0 direct, as B_Skip, B_Direct_16x16 or
    /// B_Direct_8x8; bit 0 list 0, bit 1 list 1); per 8x8 block the lists used; the motion per 4x4
    /// block of each list; and each list's vector difference per partition.
    bdir: [u8; 4],
    buse: [[bool; 2]; 4],
    bmv: [[[i16; 2]; 16]; 2],
    bmvd: [[[i32; 2]; 4]; 2],
    /// The 8x8 transform, with the luma levels per 8x8 block in 8x8 scan order.
    t8x8: bool,
    luma8: [[i32; 64]; 4],
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
        MbCode { kind, skip: false, i16_mode: 0, modes: [2; 16], chroma_mode: 0, part: 0, mvs: [[0; 2]; 4], mvds: [[0; 2]; 4], refs: [0; 4], bdir: [0; 4], buse: [[false; 2]; 4], bmv: [[[0; 2]; 16]; 2], bmvd: [[[0; 2]; 4]; 2], t8x8: false, luma8: [[0; 64]; 4], cbp: 0, luma: [[0; 16]; 16], dc: [0; 16], cdc: [[0; 4]; 2], cac: [[[0; 16]; 4]; 2], nnz: [0; 16], cnnz: [[0; 4]; 2] }
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
    /// For a P slice: its list 0, the reference pictures with their half samples, in reference
    /// index order (entry 0 is `refp`).
    plist: Vec<(&'a Picture, &'a HalfPel)>,
    /// For a B slice: the list 1 reference with its half samples.
    refp1: Option<&'a Picture>,
    half1: Option<&'a HalfPel>,
    /// The picture order distances from the list 0 reference to this picture and to the list 1 reference.
    dist: (i32, i32),
    b_slice: bool,
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
    /// transform_8x8_mode_flag (with CABAC only).
    transform8: bool,
    skip_run: u32,
    /// Macroblocks coded as intra, inter and skipped.
    pub counts: [usize; 3],
}

impl<'a> FrameCoder<'a> {
    /// A coder for the slice of macroblock rows `rows` against the reference `refp` (none for an I slice),
    /// whose half samples are `half`.
    #[allow(clippy::too_many_arguments)]
    pub fn new(src: &'a Source, refp: Option<(&'a Picture, &'a HalfPel)>, mb_w: usize, mb_h: usize, qp: i32, rows: Range<usize>, slice_id: u16, use_cabac: bool, transform8: bool) -> Self {
        FrameCoder {
            src,
            refp: refp.map(|r| r.0),
            half: refp.map(|r| r.1),
            plist: refp.into_iter().collect(),
            refp1: None,
            half1: None,
            dist: (1, 2),
            b_slice: false,
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
            transform8: use_cabac && transform8,
            skip_run: 0,
            counts: [0; 3],
        }
    }

    /// Gives a P slice its whole list 0 (`list[0]` being the reference given to `new`), in reference
    /// index order: `ref_idx` in the macroblocks picks among them.
    pub fn with_refs(mut self, list: Vec<(&'a Picture, &'a HalfPel)>) -> Self {
        self.plist = list;
        self
    }

    /// Makes the slice a B slice predicting from the list 0 reference given to `new` and `l1`,
    /// `dist` picture order counts after the first and before the second.
    pub fn with_b(mut self, l1: (&'a Picture, &'a HalfPel), dist: (i32, i32)) -> Self {
        self.refp1 = Some(l1.0);
        self.half1 = Some(l1.1);
        self.dist = dist;
        self.b_slice = true;
        self
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

    /// Reference index in list `l` (0, or -1 for intra or an unused list) and vector of the 4x4 block at (x, y) relative to
    /// macroblock (mbx, mby), in 4x4 blocks and possibly outside it; inside it, `cur` holds the
    /// vectors of the blocks set in the low 16 bits of `done`, and the high 16 bits mark those of
    /// them not using the list. None when the block is not available.
    #[allow(clippy::too_many_arguments)]
    fn nb_motion(&self, l: usize, mbx: usize, mby: usize, cur: &[[i16; 2]; 16], cref: &[i8; 16], done: u32, x: i32, y: i32) -> Option<(i8, [i16; 2])> {
        let (dx, dy, bx, by) = match (x < 0, x >= 4, y < 0) {
            (true, _, true) => (-1, -1, 3, 3),
            (true, _, false) => (-1, 0, 3, y),
            (_, true, true) => (1, -1, 0, 3),
            (_, true, false) => return None,
            (_, _, true) => (0, -1, x, 3),
            _ => {
                let b = y * 4 + x;
                return ((done >> b) & 1 == 1).then(|| if (done >> (16 + b)) & 1 == 1 { (-1, [0, 0]) } else { (cref[b as usize], cur[b as usize]) });
            }
        };
        self.nb_state(mbx as i32 + dx, mby as i32 + dy).map(|s| match s.refs[l][(by / 2 * 2 + bx / 2) as usize] {
            r if s.inter && r >= 0 => (r, if l == 0 { s.mvs } else { s.mvs1 }[(by * 4 + bx) as usize]),
            _ => (-1, [0, 0]),
        })
    }

    /// The motion vector predictor of the partition at (bx, by), `w` blocks wide (8.4.1.3); `dir`
    /// is 1 for a 16x8 and 2 for an 8x16 partition.
    #[allow(clippy::too_many_arguments)]
    fn pred_mv(&self, l: usize, mbx: usize, mby: usize, cur: &[[i16; 2]; 16], cref: &[i8; 16], done: u32, bx: i32, by: i32, w: i32, dir: u8, rf: i8) -> [i16; 2] {
        let nb = |x, y| self.nb_motion(l, mbx, mby, cur, cref, done, x, y);
        let a = nb(bx - 1, by);
        let b = nb(bx, by - 1);
        let c = nb(bx + w, by - 1).or_else(|| nb(bx - 1, by - 1));
        let r = |n: Option<(i8, [i16; 2])>| n.map_or(-1, |v| v.0);
        let mv = |n: Option<(i8, [i16; 2])>| n.map_or([0, 0], |v| v.1);
        match dir {
            1 if by == 0 && r(b) == rf => return mv(b),
            1 if by != 0 && r(a) == rf => return mv(a),
            2 if bx == 0 && r(a) == rf => return mv(a),
            2 if bx != 0 && r(c) == rf => return mv(c),
            _ => {}
        }
        if b.is_none() && c.is_none() && a.is_some() {
            return mv(a);
        }
        let (ra, rb, rc) = (r(a) == rf, r(b) == rf, r(c) == rf);
        match (ra as u8 + rb as u8 + rc as u8, ra, rb) {
            (1, true, _) => mv(a),
            (1, _, true) => mv(b),
            (1, _, _) => mv(c),
            _ => {
                let (a, b, c) = (mv(a), mv(b), mv(c));
                [median(a[0], b[0], c[0]), median(a[1], b[1], c[1])]
            }
        }
    }

    /// The motion vector predictor of a 16x16 partition in list `l`.
    fn mv_pred(&self, l: usize, mbx: usize, mby: usize, rf: i8) -> [i16; 2] {
        self.pred_mv(l, mbx, mby, &[[0; 2]; 16], &NO_REF, 0, 0, 0, 4, 0, rf)
    }

    /// The predictors of the partitions of `part` whose vectors are `mvs` and references `refs`
    /// (each depends on the partitions before it).
    fn part_mvps(&self, mbx: usize, mby: usize, part: u8, mvs: &[[i16; 2]; 4], refs: &[u8; 4]) -> [[i16; 2]; 4] {
        let (mut cur, mut cref, mut done, mut out) = ([[0i16; 2]; 16], [0i8; 16], 0u32, [[0i16; 2]; 4]);
        let dir = [0, 1, 2, 0][part as usize];
        for (k, &(x, y, w, h)) in PARTS[part as usize].iter().enumerate() {
            out[k] = self.pred_mv(0, mbx, mby, &cur, &cref, done, x as i32, y as i32, w as i32, dir, refs[k] as i8);
            for by in y..y + h {
                for bx in x..x + w {
                    cur[by * 4 + bx] = mvs[k];
                    cref[by * 4 + bx] = refs[k] as i8;
                    done |= 1 << (by * 4 + bx);
                }
            }
        }
        out
    }

    /// The vector of a P_Skip macroblock (8.4.1.1).
    fn skip_mv(&self, mbx: usize, mby: usize) -> [i16; 2] {
        let none = [[0i16; 2]; 16];
        match (self.nb_motion(0, mbx, mby, &none, &NO_REF, 0, -1, 0), self.nb_motion(0, mbx, mby, &none, &NO_REF, 0, 0, -1)) {
            (None, _) | (_, None) | (Some((0, [0, 0])), _) | (_, Some((0, [0, 0]))) => [0, 0],
            _ => self.mv_pred(0, mbx, mby, 0),
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

    /// SAD of the `w` x `h` source block at (x, y) against the reference at the integer part of `mv`.
    fn sad_int(&self, l: usize, r: usize, x: usize, y: usize, w: usize, h: usize, mv: [i16; 2]) -> i32 {
        let ls = self.ls();
        let rows = self.ref_half(l, r).rows(x as i32 + (mv[0] >> 2) as i32, y as i32 + (mv[1] >> 2) as i32, w, h);
        let mut sad = 0;
        for (r, rrow) in rows.enumerate() {
            let srow = &self.src.planes[0][(y + r) * ls + x..(y + r) * ls + x + w];
            sad += srow.iter().zip(rrow).map(|(&s, &p)| (s as i32 - p as i32).abs()).sum::<i32>();
        }
        sad
    }

    fn luma_pred(&self, l: usize, r: usize, x: usize, y: usize, w: usize, h: usize, mv: [i16; 2], out: &mut [u16]) {
        self.ref_half(l, r).predict(x as i32, y as i32, mv, w, h, out);
    }

    /// The half-sample planes of reference `r` of list `l`: a P slice's references are its list 0
    /// (`plist`), a B slice has one reference in each of its two lists.
    fn ref_half(&self, l: usize, r: usize) -> &HalfPel {
        if self.b_slice {
            if l == 0 {
                self.half.unwrap()
            } else {
                self.half1.unwrap()
            }
        } else {
            self.plist[r].1
        }
    }

    /// The picture of reference `r` of list `l` (see `ref_half`).
    fn ref_pic(&self, l: usize, r: usize) -> &Picture {
        if self.b_slice {
            if l == 0 {
                self.refp.unwrap()
            } else {
                self.refp1.unwrap()
            }
        } else {
            self.plist[r].0
        }
    }

    /// Bits for coding reference index `r` (ref_idx_l0 with `plist` longer than one).
    fn ref_bits(&self, r: u8) -> i32 {
        match self.plist.len() {
            0 | 1 => 0,
            2 => 1,
            _ => 2 * (r as u32 + 1).ilog2() as i32 + 1,
        }
    }

    // ---- macroblock analysis -------------------------------------------------------------------------

    fn code_mb(&mut self, mbx: usize, mby: usize) -> MbCode {
        if self.b_slice {
            return self.decide_b(mbx, mby);
        }
        let (x0, y0) = (mbx * 16, mby * 16);
        let lam = self.lambda;
        let mut inter = None;
        if self.refp.is_some() {
            // P_Skip first: when the skip vector leaves nothing worth coding, nothing else can be cheaper.
            // A prediction whose differences average a quantiser step or more cannot be skipped.
            let skip_mv = self.skip_mv(mbx, mby);
            let mut pred = [0u16; 256];
            self.luma_pred(0, 0, x0, y0, 16, 16, skip_mv, &mut pred);
            let ls = self.ls();
            let sad: i32 = (0..256).map(|i| (self.src.planes[0][(y0 + i / 16) * ls + x0 + i % 16] as i32 - pred[i] as i32).abs()).sum();
            if (sad as f64) < 256.0 * 0.625 * 2f64.powf(self.qp as f64 / 6.0) {
                let mut code = self.code_inter(mbx, mby, 0, [skip_mv; 4], [0; 4], false);
                if code.cbp == 0 {
                    code.skip = true;
                    return code;
                }
            }
        }
        let i16 = self.best_i16(mbx, mby);
        let mut best_intra = i16.1 + lam * 4;
        if self.refp.is_some() {
            inter = Some(self.best_inter(mbx, mby));
        }
        if self.use_cabac {
            return self.decide_rd(mbx, mby, i16.0, inter);
        }
        // Intra4x4 is tried by coding it, which leaves its reconstruction in place; it is kept only if it wins
        let try_i4 = inter.is_none_or(|(_, _, _, c)| best_intra < c);
        let mut i4 = None;
        if try_i4 {
            let code = self.code_i4(mbx, mby);
            let cost = code.1 + lam * 6;
            if cost < best_intra {
                best_intra = cost;
                i4 = Some(code.0);
            }
        }
        if let Some((part, mvs, refs, cost)) = inter {
            if cost <= best_intra {
                let mut code = self.code_inter(mbx, mby, part, mvs, refs, false);
                if code.cbp == 0 && part == 0 && mvs[0] == self.skip_mv(mbx, mby) && refs[0] == 0 {
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

    /// The best partitioning of an inter macroblock, a vector per partition, and their cost.
    fn best_inter(&self, mbx: usize, mby: usize) -> (u8, [[i16; 2]; 4], [u8; 4], i32) {
        let mut best: Option<(u8, [[i16; 2]; 4], [u8; 4], i32)> = None;
        // every reference of list 0 is searched with the whole macroblock; the cheapest wins
        for r in 0..self.plist.len() as u8 {
            let (part, mvs, cost) = self.best_inter_ref(mbx, mby, r);
            if best.is_none_or(|b| cost < b.3) {
                best = Some((part, mvs, [r; 4], cost));
            }
        }
        best.unwrap()
    }

    /// `best_inter` with every partition predicting from reference `r`.
    fn best_inter_ref(&self, mbx: usize, mby: usize, r: u8) -> (u8, [[i16; 2]; 4], i32) {
        let lam = self.lambda;
        let refs = [r; 4];
        let rb = lam * self.ref_bits(r);
        let mvp = self.mv_pred(0, mbx, mby, r as i8);
        let col: Vec<_> = self.col_mv(self.refp.unwrap(), mbx, mby).into_iter().collect();
        let (mv, cost) = self.motion_search(0, r as usize, mbx, mby, PARTS[0][0], mvp, &col);
        // costs carry the mb_type and sub_mb_type bits (as CAVLC codes them)
        let mut best = (0u8, [mv; 4], cost + lam * (mvd_bits(mv, mvp) + 1) + rb);
        let mut mvs8 = [mv; 4];
        let mut cost8 = lam * 9;
        for k in 0..4 {
            let p = self.part_mvps(mbx, mby, 3, &mvs8, &refs)[k];
            let (m, c) = self.motion_search(0, r as usize, mbx, mby, PARTS[3][k], p, &[mv]);
            mvs8[k] = m;
            cost8 += c + lam * mvd_bits(m, p) + rb;
            // costs only add up: once 8x8 costs more than 16x16 it cannot win
            if cost8 >= best.2 {
                break;
            }
        }
        // 16x8 and 8x16 halves, searched from the 8x8 vectors they cover, when 8x8 beats 16x16
        if cost8 < best.2 {
            best = (3, mvs8, cost8);
            for part in 1..3u8 {
                let mut mvs = [[0i16; 2]; 4];
                let mut cost = lam * 3;
                for k in 0..2 {
                    let p = self.part_mvps(mbx, mby, part, &mvs, &refs)[k];
                    let cands = if part == 1 { [mvs8[2 * k], mvs8[2 * k + 1]] } else { [mvs8[k], mvs8[k + 2]] };
                    let (m, c) = self.motion_search(0, r as usize, mbx, mby, PARTS[part as usize][k], p, &cands);
                    mvs[k] = m;
                    cost += c + lam * mvd_bits(m, p) + rb;
                    if cost >= best.2 {
                        break;
                    }
                }
                if cost < best.2 {
                    best = (part, mvs, cost);
                }
            }
        }
        best
    }

    /// Integer-sample diamond search in list `l` for the partition `(bx, by, w, h)` (in 4x4 blocks) of
    /// macroblock (mbx, mby), from the best of a few candidates, then half- and quarter-sample
    /// refinement. Returns the vector and its SATD, without the vector's bits.
    /// The vector of the middle of the co-located macroblock in `pic`, if it is inter coded.
    fn col_mv(&self, pic: &Picture, mbx: usize, mby: usize) -> Option<[i16; 2]> {
        let col = &pic.mbs[mby * self.mb_w + mbx];
        (col.kind == Kind::Inter).then(|| col.mv[0][5])
    }

    fn motion_search(&self, l: usize, r: usize, mbx: usize, mby: usize, (bx, by, w, h): (usize, usize, usize, usize), mvp: [i16; 2], extra: &[[i16; 2]]) -> ([i16; 2], i32) {
        let lam = self.lambda;
        let (x0, y0, w, h) = (mbx * 16 + bx * 4, mby * 16 + by * 4, w * 4, h * 4);
        let (lo_x, hi_x) = (-(x0 as i32 + 24) * 4, ((self.mb_w * 16 - x0) as i32 + 8) * 4);
        let (lo_y, hi_y) = (-(y0 as i32 + 24) * 4, ((self.mb_h * 16 - y0) as i32 + 8) * 4);
        let clamp = |mv: [i32; 2]| -> [i16; 2] { [mv[0].clamp(lo_x.max(-2048), hi_x.min(2047)) as i16, mv[1].clamp(lo_y.max(-512), hi_y.min(511)) as i16] };
        let int = |mv: [i16; 2]| -> [i16; 2] { [(mv[0] + 2) & !3, (mv[1] + 2) & !3] };
        let cost_int = |mv: [i16; 2]| self.sad_int(l, r, x0, y0, w, h, mv) + lam * mvd_bits(mv, mvp);
        let none = [[0i16; 2]; 16];
        let mut cands = vec![int(clamp([mvp[0] as i32, mvp[1] as i32])), [0, 0]];
        for (dx, dy) in [(-1, 0), (0, -1), (4, -1)] {
            if let Some((_, mv)) = self.nb_motion(l, mbx, mby, &none, &NO_REF, 0, dx, dy).filter(|n| n.0 == r as i8) {
                cands.push(int(clamp([mv[0] as i32, mv[1] as i32])));
            }
        }
        cands.extend(extra.iter().map(|mv| int(clamp([mv[0] as i32, mv[1] as i32]))));
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
        let mut buf = [0u16; 256];
        let pred = &mut buf[..w * h];
        let cost_sub = |mv: [i16; 2], pred: &mut [u16]| {
            self.luma_pred(l, r, x0, y0, w, h, mv, pred);
            self.satd(0, x0, y0, w, h, pred) + lam * mvd_bits(mv, mvp)
        };
        best_cost = cost_sub(best, pred);
        // the eight half samples around, then the four nearest quarter samples (the diagonal ones
        // gain nothing for their time)
        let square: &[(i32, i32)] = &[(-1, -1), (0, -1), (1, -1), (-1, 0), (1, 0), (-1, 1), (0, 1), (1, 1)];
        let diamond: &[(i32, i32)] = &[(0, -1), (-1, 0), (1, 0), (0, 1)];
        for (step, pts) in [(2i32, square), (1, diamond)] {
            let centre = best;
            for &(dx, dy) in pts {
                let c = clamp([centre[0] as i32 + dx * step, centre[1] as i32 + dy * step]);
                let cost = cost_sub(c, pred);
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

    /// Codes an inter macroblock of partitioning `part` with a vector per partition, its luma with
    /// the 8x8 transform when `t8`.
    fn code_inter(&mut self, mbx: usize, mby: usize, part: u8, mvs: [[i16; 2]; 4], refs: [u8; 4], t8: bool) -> MbCode {
        let mut code = MbCode::new(Kind::Inter);
        code.part = part;
        code.mvs = mvs;
        code.refs = refs;
        let mvps = self.part_mvps(mbx, mby, part, &mvs, &refs);
        for k in 0..4 {
            code.mvds[k] = [0, 1].map(|c| mvs[k][c] as i32 - mvps[k][c] as i32);
        }
        let (pred, cpred) = self.inter_pred(mbx, mby, part, &mvs, &refs);
        self.code_inter_residual(code, mbx, mby, &pred, &cpred, t8)
    }

    /// Codes the residual of an inter macroblock against its prediction, into the reconstruction.
    fn code_inter_residual(&mut self, mut code: MbCode, mbx: usize, mby: usize, pred: &[u16; 256], cpred: &[[u16; 64]; 2], t8: bool) -> MbCode {
        let (x0, y0, ls) = (mbx * 16, mby * 16, self.ls());
        if t8 {
            self.code_luma8(&mut code, mbx, mby, pred);
        } else {
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
        }
        self.code_chroma(&mut code, mbx, mby, cpred, false);
        code
    }

    /// Codes the luma residual of an inter macroblock against `pred` with the 8x8 transform, into
    /// the reconstruction. The transform is kept only when some luma is coded.
    fn code_luma8(&mut self, code: &mut MbCode, mbx: usize, mby: usize, pred: &[u16; 256]) {
        let (x0, y0, ls) = (mbx * 16, mby * 16, self.ls());
        let mut cos = [[0i32; 64]; 4];
        let mut score = [0i32; 4];
        for i8 in 0..4 {
            let (bx, by) = ((i8 & 1) * 8, (i8 >> 1) * 8);
            let mut s = [0i32; 64];
            self.src_block(0, x0 + bx, y0 + by, 8, &mut s);
            let pr: [i32; 64] = std::array::from_fn(|k| pred[(by + k / 8) * 16 + bx + k % 8] as i32);
            let (lv, co) = quant8(&s, &pr, self.qp, false);
            score[i8] = decimate_score8(&lv);
            code.luma8[i8] = lv;
            cos[i8] = co;
        }
        for y in 0..16 {
            self.pic.planes[0][(y0 + y) * ls + x0..(y0 + y) * ls + x0 + 16].copy_from_slice(&pred[y * 16..y * 16 + 16]);
        }
        let total: i32 = score.iter().sum();
        for i8 in 0..4 {
            if score[i8] < 4 || total < 6 {
                code.luma8[i8] = [0; 64];
                continue;
            }
            let n = code.luma8[i8].iter().filter(|&&l| l != 0).count() as u8;
            if n > 0 {
                idct8_add(&cos[i8], &mut self.pic.planes[0][(y0 + (i8 >> 1) * 8) * ls + x0 + (i8 & 1) * 8..], ls, 255);
                code.cbp |= 1 << i8;
            }
            // CABAC counts an 8x8 block's levels in each of its 4x4 blocks
            for j in 0..4 {
                code.nnz[((i8 >> 1) * 2 + (j >> 1)) * 4 + (i8 & 1) * 2 + (j & 1)] = n;
            }
        }
        code.t8x8 = code.cbp & 15 != 0;
    }

    /// Codes the luma of an Intra8x8 macroblock block by block into the reconstruction.
    fn code_i8(&mut self, mbx: usize, mby: usize) -> MbCode {
        let mut code = MbCode::new(Kind::I8x8);
        code.t8x8 = true;
        let ta = self.up(mby);
        let (la, tla, tra) = (mbx > 0, mbx > 0 && ta, ta && mbx + 1 < self.mb_w);
        let ls = self.ls();
        for i8 in 0..4 {
            let (bx, by) = (i8 & 1, i8 >> 1);
            let (x, y) = (mbx * 16 + bx * 8, mby * 16 + by * 8);
            let have_left = bx > 0 || la;
            let have_top = by > 0 || ta;
            let have_tl = match (bx > 0, by > 0) {
                (true, true) => true,
                (false, true) => la,
                (true, false) => ta,
                (false, false) => tla,
            };
            let have_tr = match i8 {
                0 => ta,
                1 => tra,
                2 => true,
                _ => false,
            };
            let p = &self.pic.planes[0];
            let mut e = Edges { top: [0; 16], left: [0; 8], topleft: 0, have_top, have_left, have_topleft: have_tl };
            if have_top {
                for i in 0..8 {
                    e.top[i] = p[(y - 1) * ls + x + i] as i32;
                }
                for i in 8..16 {
                    e.top[i] = if have_tr { p[(y - 1) * ls + x + i] as i32 } else { e.top[7] };
                }
            }
            if have_left {
                for j in 0..8 {
                    e.left[j] = p[(y + j) * ls + x - 1] as i32;
                }
            }
            if have_tl {
                e.topleft = p[(y - 1) * ls + x - 1] as i32;
            }
            filter_8x8_edges(&mut e);
            let predicted = self.pred_i4_mode(&code.modes, mbx, mby, bx * 2, by * 2);
            let mut best = (2u8, i32::MAX, [0u16; 64]);
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
                let mut pred = [0u16; 64];
                predict_nxn(8, mode, &e, &mut pred, 8);
                let c = self.satd(0, x, y, 8, 8, &pred) + self.lambda * if mode == predicted { 1 } else { 4 };
                if c < best.1 {
                    best = (mode, c, pred);
                }
            }
            let mut s = [0i32; 64];
            self.src_block(0, x, y, 8, &mut s);
            let (lv, co) = quant8(&s, &best.2.map(|v| v as i32), self.qp, true);
            let p = &mut self.pic.planes[0];
            for r in 0..8 {
                p[(y + r) * ls + x..(y + r) * ls + x + 8].copy_from_slice(&best.2[r * 8..r * 8 + 8]);
            }
            let n = lv.iter().filter(|&&l| l != 0).count() as u8;
            if n > 0 {
                idct8_add(&co, &mut p[y * ls + x..], ls, 255);
                code.cbp |= 1 << i8;
            }
            for j in 0..4 {
                let b = (by * 2 + (j >> 1)) * 4 + bx * 2 + (j & 1);
                code.modes[b] = best.0;
                code.nnz[b] = n;
            }
            code.luma8[i8] = lv;
        }
        code
    }

    /// Writes the inter prediction of a macroblock as its reconstruction (P_Skip's).
    fn put_inter_pred(&mut self, mbx: usize, mby: usize, mv: [i16; 2]) {
        let (pred, cpred) = self.inter_pred(mbx, mby, 0, &[mv; 4], &[0; 4]);
        self.put_pred(mbx, mby, &pred, &cpred);
    }

    /// Writes a prediction as a macroblock's reconstruction.
    fn put_pred(&mut self, mbx: usize, mby: usize, pred: &[u16; 256], cpred: &[[u16; 64]; 2]) {
        let mut rec = [0u16; 384];
        rec[..256].copy_from_slice(pred);
        rec[256..320].copy_from_slice(&cpred[0]);
        rec[320..].copy_from_slice(&cpred[1]);
        self.restore_mb(mbx, mby, &rec);
    }

    /// The reconstructed samples of a macroblock: luma, then Cb and Cr.
    fn save_mb(&self, mbx: usize, mby: usize) -> [u16; 384] {
        let (ls, cs) = (self.ls(), self.cs());
        let mut rec = [0u16; 384];
        for r in 0..16 {
            rec[r * 16..r * 16 + 16].copy_from_slice(&self.pic.planes[0][(mby * 16 + r) * ls + mbx * 16..][..16]);
        }
        for c in 0..2 {
            for r in 0..8 {
                rec[256 + c * 64 + r * 8..][..8].copy_from_slice(&self.pic.planes[1 + c][(mby * 8 + r) * cs + mbx * 8..][..8]);
            }
        }
        rec
    }

    fn restore_mb(&mut self, mbx: usize, mby: usize, rec: &[u16; 384]) {
        let (ls, cs) = (self.ls(), self.cs());
        for r in 0..16 {
            self.pic.planes[0][(mby * 16 + r) * ls + mbx * 16..][..16].copy_from_slice(&rec[r * 16..r * 16 + 16]);
        }
        for c in 0..2 {
            for r in 0..8 {
                self.pic.planes[1 + c][(mby * 8 + r) * cs + mbx * 8..][..8].copy_from_slice(&rec[256 + c * 64 + r * 8..][..8]);
            }
        }
    }

    /// Squared error of a macroblock's reconstruction, luma and chroma.
    fn ssd_mb(&self, mbx: usize, mby: usize) -> i64 {
        let rec = self.save_mb(mbx, mby);
        let (ls, cs) = (self.ls(), self.cs());
        let mut ssd = 0i64;
        for (i, &v) in rec.iter().enumerate() {
            let s = match i {
                0..256 => self.src.planes[0][(mby * 16 + i / 16) * ls + mbx * 16 + i % 16],
                _ => {
                    let (c, j) = ((i - 256) / 64, (i - 256) % 64);
                    self.src.planes[1 + c][(mby * 8 + j / 8) * cs + mbx * 8 + j % 8]
                }
            };
            let d = s as i64 - v as i64;
            ssd += d * d;
        }
        ssd
    }

    /// Rate-distortion cost of a coded macroblock whose reconstruction is in place: squared error
    /// plus `lam2` times the bits CABAC would spend on it.
    fn rd_cost(&self, code: &MbCode, mbx: usize, mby: usize, lam2: f64) -> f64 {
        let mut c = self.cabac.as_ref().unwrap().cost();
        self.mb_bins(&mut c, code, mbx, mby);
        self.ssd_mb(mbx, mby) as f64 + lam2 * c.bits()
    }

    /// Codes P_Skip, the best inter partitioning (by SATD), Intra16x16 with its best mode and
    /// Intra4x4, and keeps the one of least rate-distortion cost. SATD is a poor judge between intra
    /// and inter, so intra is always tried.
    fn decide_rd(&mut self, mbx: usize, mby: usize, i16: u8, inter: Option<(u8, [[i16; 2]; 4], [u8; 4], i32)>) -> MbCode {
        // larger than the usual 0.85 * 2^((qp - 12) / 3): the residual decisions are not RD optimised
        let lam2 = 1.6 * 2f64.powf((self.qp as f64 - 12.0) / 3.0);
        let mut best: Option<(f64, MbCode, [u16; 384])> = None;
        let mut keep = |this: &Self, j: f64, code: MbCode| {
            if best.as_ref().is_none_or(|b| j < b.0) {
                best = Some((j, code, this.save_mb(mbx, mby)));
            }
        };
        if let Some((part, mvs, refs, _)) = inter {
            let sm = self.skip_mv(mbx, mby);
            let mut code = MbCode::new(Kind::Inter);
            code.skip = true;
            code.mvs = [sm; 4];
            self.put_inter_pred(mbx, mby, sm);
            let j = self.rd_cost(&code, mbx, mby, lam2);
            keep(self, j, code);
            let code = self.code_inter(mbx, mby, part, mvs, refs, false);
            // with nothing coded and the skip vector, this is P_Skip at a higher price
            if !(code.cbp == 0 && part == 0 && mvs[0] == sm && refs[0] == 0) {
                let j = self.rd_cost(&code, mbx, mby, lam2);
                keep(self, j, code);
            }
            if self.transform8 {
                // without luma levels it would be the 4x4 candidate again
                let code = self.code_inter(mbx, mby, part, mvs, refs, true);
                if code.t8x8 {
                    let j = self.rd_cost(&code, mbx, mby, lam2);
                    keep(self, j, code);
                }
            }
        }
        let mut code = self.code_i16(mbx, mby, i16);
        self.code_intra_chroma(&mut code, mbx, mby);
        let j = self.rd_cost(&code, mbx, mby, lam2);
        keep(self, j, code);
        // Intra4x4 predicts from its own reconstruction, block by block, so it can overwrite the others'
        let mut code = self.code_i4(mbx, mby).0;
        self.code_intra_chroma(&mut code, mbx, mby);
        let j = self.rd_cost(&code, mbx, mby, lam2);
        keep(self, j, code);
        if self.transform8 {
            let mut code = self.code_i8(mbx, mby);
            self.code_intra_chroma(&mut code, mbx, mby);
            let j = self.rd_cost(&code, mbx, mby, lam2);
            keep(self, j, code);
        }
        let (_, code, rec) = best.unwrap();
        self.restore_mb(mbx, mby, &rec);
        code
    }

    /// Chooses a B macroblock by rate-distortion cost among B_Skip and B_Direct_16x16 (spatial
    /// direct), the best by SATD of L0, L1 and Bi 16x16, the best smaller partitioning if it beats
    /// that, and Intra16x16 and Intra4x4.
    fn decide_b(&mut self, mbx: usize, mby: usize) -> MbCode {
        let lam = self.lambda;
        let lam2 = 1.6 * 2f64.powf((self.qp as f64 - 12.0) / 3.0);
        let mut best: Option<(f64, MbCode, [u16; 384])> = None;
        let mut keep = |this: &Self, j: f64, code: MbCode| {
            if best.as_ref().is_none_or(|b| j < b.0) {
                best = Some((j, code, this.save_mb(mbx, mby)));
            }
        };
        let t8s = if self.transform8 { &[false, true][..] } else { &[false][..] };
        let (direct, duse) = self.direct_mvs(mbx, mby);
        let (dpred, dcpred) = self.b_pred(mbx, mby, &direct, &[duse; 4]);
        let mut code = MbCode::new(Kind::Inter);
        code.skip = true;
        code.buse = [duse; 4];
        code.bmv = direct;
        self.put_pred(mbx, mby, &dpred, &dcpred);
        let j = self.rd_cost(&code, mbx, mby, lam2);
        keep(self, j, code);
        for &t8 in t8s {
            let mut code = MbCode::new(Kind::Inter);
            code.buse = [duse; 4];
            code.bmv = direct;
            let code = self.code_inter_residual(code, mbx, mby, &dpred, &dcpred, t8);
            // without residual, B_Direct_16x16 is B_Skip at a higher price
            if code.cbp != 0 && (!t8 || code.t8x8) {
                let j = self.rd_cost(&code, mbx, mby, lam2);
                keep(self, j, code);
            }
        }
        // 16x16 from either list or both; the costs carry an estimate of the mb_type bits
        let (x0, y0) = (mbx * 16, mby * 16);
        let mvps = [self.mv_pred(0, mbx, mby, 0), self.mv_pred(1, mbx, mby, 0)];
        let mut mvs = [[0i16; 2]; 2];
        let mut costs = [0i32; 2];
        let mut preds = [[0u16; 256]; 2];
        // the co-located vector spans both lists' references, split by the picture's distances from them
        let (tb, td) = self.dist;
        let scale = |m: [i16; 2], num: i32, den: i32| m.map(|v| (v as i32 * num / den) as i16);
        let col = self.col_mv(self.refp1.unwrap(), mbx, mby);
        for l in 0..2 {
            let mut extra: Vec<_> = col.map(|m| if l == 0 { scale(m, tb, td) } else { scale(m, tb - td, td) }).into_iter().collect();
            if l == 1 {
                extra.push(scale(mvs[0], tb - td, tb));
            }
            let (mv, c) = self.motion_search(l, 0, mbx, mby, PARTS[0][0], mvps[l], &extra);
            mvs[l] = mv;
            costs[l] = c + lam * (mvd_bits(mv, mvps[l]) + 3);
            self.luma_pred(l, 0, x0, y0, 16, 16, mv, &mut preds[l]);
        }
        let avg: [u16; 256] = std::array::from_fn(|i| (preds[0][i] + preds[1][i] + 1) >> 1);
        let bi = self.satd(0, x0, y0, 16, 16, &avg) + lam * (mvd_bits(mvs[0], mvps[0]) + mvd_bits(mvs[1], mvps[1]) + 5);
        let dir = if bi < costs[0].min(costs[1]) { 3 } else if costs[0] <= costs[1] { 1 } else { 2 };
        let mut cands = vec![(0u8, [dir, 0, 0, 0], [[mvs[0]; 4], [mvs[1]; 4]])];
        if let Some((part, bdir, pmv)) = self.best_b_parts(mbx, mby, mvs, mvps, bi.min(costs[0]).min(costs[1]), &dpred) {
            cands.push((part, bdir, pmv));
        }
        for (part, bdir, pmv) in cands {
            // per 8x8 block, the lists used and each list's vector per 4x4 block
            let mut buse = [[false; 2]; 4];
            let mut bmv = [[[0i16; 2]; 16]; 2];
            for (k, &(x, y, w, h)) in PARTS[part as usize].iter().enumerate() {
                for b in (y..y + h).flat_map(|by| (x..x + w).map(move |bx| by * 4 + bx)) {
                    let b8 = (b / 8) * 2 + (b % 4) / 2;
                    for l in 0..2 {
                        buse[b8][l] = if bdir[k] == 0 { duse[l] } else { bdir[k] >> l & 1 == 1 };
                        if buse[b8][l] {
                            bmv[l][b] = if bdir[k] == 0 { direct[l][b] } else { pmv[l][k] };
                        }
                    }
                }
            }
            let bmvd = self.b_mvds(mbx, mby, part, &bdir, &bmv, &buse);
            let (pred, cpred) = self.b_pred(mbx, mby, &bmv, &buse);
            for &t8 in t8s {
                let mut code = MbCode::new(Kind::Inter);
                code.part = part;
                code.bdir = bdir;
                code.buse = buse;
                code.bmv = bmv;
                code.bmvd = bmvd;
                let code = self.code_inter_residual(code, mbx, mby, &pred, &cpred, t8);
                if !t8 || code.t8x8 {
                    let j = self.rd_cost(&code, mbx, mby, lam2);
                    keep(self, j, code);
                }
            }
        }
        let i16 = self.best_i16(mbx, mby).0;
        let mut code = self.code_i16(mbx, mby, i16);
        self.code_intra_chroma(&mut code, mbx, mby);
        let j = self.rd_cost(&code, mbx, mby, lam2);
        keep(self, j, code);
        // Intra8x8 rarely beats Intra4x4 in a B picture, and costs as much to try
        let mut code = self.code_i4(mbx, mby).0;
        self.code_intra_chroma(&mut code, mbx, mby);
        let j = self.rd_cost(&code, mbx, mby, lam2);
        keep(self, j, code);
        let (_, code, rec) = best.unwrap();
        self.restore_mb(mbx, mby, &rec);
        code
    }

    /// The best B partitioning below 16x16 by SATD plus an estimate of the bits, when it costs less
    /// than `cost16`: each 8x8 block direct or predicted from list 0, list 1 or both, and, when that
    /// wins, 16x8 and 8x16 likewise (without direct). Gives the partitioning, each partition's
    /// direction (0 direct, bit 0 list 0, bit 1 list 1) and vectors per list.
    #[allow(clippy::type_complexity)]
    fn best_b_parts(&self, mbx: usize, mby: usize, mvs16: [[i16; 2]; 2], mvps: [[i16; 2]; 2], cost16: i32, dpred: &[u16; 256]) -> Option<(u8, [u8; 4], [[[i16; 2]; 4]; 2])> {
        let lam = self.lambda;
        // a partition's best direction, its vectors and cost; `direct` also weighs direct prediction
        let search = |rect: (usize, usize, usize, usize), extra: [&[[i16; 2]]; 2], direct: bool| -> (u8, [[i16; 2]; 2], i32) {
            let (bx, by, w, h) = rect;
            let (x0, y0, pw, ph) = (mbx * 16 + bx * 4, mby * 16 + by * 4, w * 4, h * 4);
            let mut mv = [[0i16; 2]; 2];
            let mut cost = [0i32; 2];
            let mut preds = [[0u16; 128]; 2];
            for l in 0..2 {
                (mv[l], cost[l]) = self.motion_search(l, 0, mbx, mby, rect, mvps[l], extra[l]);
                self.luma_pred(l, 0, x0, y0, pw, ph, mv[l], &mut preds[l][..pw * ph]);
            }
            let avg: [u16; 128] = std::array::from_fn(|i| (preds[0][i] + preds[1][i] + 1) >> 1);
            let bi = self.satd(0, x0, y0, pw, ph, &avg[..pw * ph]) + lam * (mvd_bits(mv[0], mvps[0]) + mvd_bits(mv[1], mvps[1]) + 2);
            let mut best = if bi < cost[0].min(cost[1]) { (3, bi) } else if cost[0] <= cost[1] { (1, cost[0]) } else { (2, cost[1]) };
            if direct {
                // sub_mb_type takes about one bin for direct and three for the others
                best.1 += lam * 3;
                let sub: Vec<u16> = (0..pw * ph).map(|i| dpred[(by * 4 + i / pw) * 16 + bx * 4 + i % pw]).collect();
                let d = self.satd(0, x0, y0, pw, ph, &sub) + lam;
                if d < best.1 {
                    best = (0, d);
                }
            }
            (best.0, mv, best.1)
        };
        let mut dirs = [0u8; 4];
        let mut pmv = [[[0i16; 2]; 4]; 2];
        let mut total = lam * 6;
        // costs only add up, so a partitioning stops being searched once it costs more than the best
        for (k, &rect) in PARTS[3].iter().enumerate() {
            let (d, mv, c) = search(rect, [&[mvs16[0]][..], &[mvs16[1]][..]], true);
            dirs[k] = d;
            (pmv[0][k], pmv[1][k]) = (mv[0], mv[1]);
            total += c;
            if total >= cost16 {
                return None;
            }
        }
        let mut best = (3u8, dirs, pmv, total);
        for part in 1..3u8 {
            let (mut dirs, mut mvs, mut total) = ([0u8; 4], [[[0i16; 2]; 4]; 2], lam * 7);
            for (k, &rect) in PARTS[part as usize].iter().enumerate() {
                let extra = [&[pmv[0][0], pmv[0][1], pmv[0][2], pmv[0][3], mvs16[0]][..], &[pmv[1][0], pmv[1][1], pmv[1][2], pmv[1][3], mvs16[1]][..]];
                let (d, mv, c) = search(rect, extra, false);
                dirs[k] = d;
                (mvs[0][k], mvs[1][k]) = (mv[0], mv[1]);
                total += c;
                if total >= best.3 {
                    break;
                }
            }
            if total < best.3 {
                best = (part, dirs, mvs, total);
            }
        }
        Some((best.0, best.1, best.2))
    }

    /// Each list's vector differences per partition of a B macroblock (8.4.1.3): partitions are
    /// predicted in order from the earlier ones that use the list, a direct 8x8 block counting with
    /// its derived motion.
    fn b_mvds(&self, mbx: usize, mby: usize, part: u8, bdir: &[u8; 4], bmv: &[[[i16; 2]; 16]; 2], buse: &[[bool; 2]; 4]) -> [[[i32; 2]; 4]; 2] {
        let mut out = [[[0i32; 2]; 4]; 2];
        let dir = [0, 1, 2, 0][part as usize];
        for l in 0..2 {
            let (mut cur, mut done) = ([[0i16; 2]; 16], 0u32);
            for (k, &(x, y, w, h)) in PARTS[part as usize].iter().enumerate() {
                if bdir[k] >> l & 1 == 1 {
                    let mvp = self.pred_mv(l, mbx, mby, &cur, &NO_REF, done, x as i32, y as i32, w as i32, dir, 0);
                    out[l][k] = [0, 1].map(|c| bmv[l][y * 4 + x][c] as i32 - mvp[c] as i32);
                }
                for b in (y..y + h).flat_map(|by| (x..x + w).map(move |bx| by * 4 + bx)) {
                    cur[b] = bmv[l][b];
                    done |= 1 << b;
                    if !buse[(b / 8) * 2 + (b % 4) / 2][l] {
                        done |= 1 << (16 + b);
                    }
                }
            }
        }
        out
    }

    /// Spatial direct motion of a macroblock (8.4.1.2.2): per list, the neighbours' smallest
    /// reference index and the predicted vector, zeroed in the 8x8 blocks whose co-located block in
    /// the list 1 picture barely moves; also the lists used. Without a neighbour using either list,
    /// both are used with zero vectors.
    fn direct_mvs(&self, mbx: usize, mby: usize) -> ([[[i16; 2]; 16]; 2], [bool; 2]) {
        let none = [[0i16; 2]; 16];
        let mut refs = [-1i8; 2];
        for (l, rf) in refs.iter_mut().enumerate() {
            let nb = |x, y| self.nb_motion(l, mbx, mby, &none, &NO_REF, 0, x, y);
            let r = |n: Option<(i8, [i16; 2])>| n.map_or(-1, |v| v.0);
            let minpos = |x: i8, y: i8| if x >= 0 && y >= 0 { x.min(y) } else { x.max(y) };
            *rf = minpos(r(nb(-1, 0)), minpos(r(nb(0, -1)), r(nb(4, -1).or_else(|| nb(-1, -1)))));
        }
        let mut d = [[[0i16; 2]; 16]; 2];
        if refs == [-1, -1] {
            return (d, [true, true]);
        }
        for l in (0..2).filter(|&l| refs[l] >= 0) {
            d[l] = [self.mv_pred(l, mbx, mby, 0); 16];
        }
        let col = &self.refp1.unwrap().mbs[mby * self.mb_w + mbx];
        if col.kind == Kind::Inter {
            for b in 0..16 {
                // the corner 4x4 block of each 8x8 block stands for it (direct_8x8_inference_flag)
                let (cx, cy) = (((b % 4) >> 1) * 3, ((b / 4) >> 1) * 3);
                let m = col.mv[0][cy * 4 + cx];
                if col.ref_idx[0][(cy >> 1) * 2 + (cx >> 1)] == 0 && m[0].abs() <= 1 && m[1].abs() <= 1 {
                    for l in (0..2).filter(|&l| refs[l] == 0) {
                        d[l][b] = [0, 0];
                    }
                }
            }
        }
        (d, [refs[0] >= 0, refs[1] >= 0])
    }

    /// The prediction of a B macroblock, 8x8 block by 8x8 block from the lists `used` there with
    /// the block's vectors; with both lists the two predictions are averaged.
    fn b_pred(&self, mbx: usize, mby: usize, mv: &[[[i16; 2]; 16]; 2], used: &[[bool; 2]; 4]) -> ([u16; 256], [[u16; 64]; 2]) {
        let refs = [self.refp.unwrap(), self.refp1.unwrap()];
        let (x0, y0, cs) = (mbx * 16, mby * 16, self.cs());
        let mut pred = [0u16; 256];
        let mut cpred = [[0u16; 64]; 2];
        for i8 in 0..4 {
            let (bx, by) = ((i8 & 1) * 8, (i8 >> 1) * 8);
            let mut lp = [[0u16; 64]; 2];
            let mut cp = [[[0u16; 16]; 2]; 2];
            for l in (0..2).filter(|&l| used[i8][l]) {
                let v = mv[l][(by / 4) * 4 + bx / 4];
                self.luma_pred(l, 0, x0 + bx, y0 + by, 8, 8, v, &mut lp[l]);
                for c in 0..2 {
                    chroma_mc(&refs[l].planes[1 + c], cs, self.mb_h * 8, ((x0 + bx) / 2) as i32, ((y0 + by) / 2) as i32, v[0] as i32, v[1] as i32, 4, 4, &mut cp[l][c]);
                }
            }
            let mix = |a: u16, b: u16| match used[i8] {
                [true, true] => (a + b + 1) >> 1,
                [true, false] => a,
                _ => b,
            };
            for k in 0..64 {
                pred[(by + k / 8) * 16 + bx + k % 8] = mix(lp[0][k], lp[1][k]);
            }
            for c in 0..2 {
                for k in 0..16 {
                    cpred[c][(by / 2 + k / 4) * 8 + bx / 2 + k % 4] = mix(cp[0][c][k], cp[1][c][k]);
                }
            }
        }
        (pred, cpred)
    }

    /// The luma and chroma prediction of an inter macroblock of partitioning `part`.
    fn inter_pred(&self, mbx: usize, mby: usize, part: u8, mvs: &[[i16; 2]; 4], refs: &[u8; 4]) -> ([u16; 256], [[u16; 64]; 2]) {
        let (x0, y0, cs) = (mbx * 16, mby * 16, self.cs());
        let mut pred = [0u16; 256];
        let mut cpred = [[0u16; 64]; 2];
        let mut tmp = [0u16; 256];
        for (k, &(bx, by, w, h)) in PARTS[part as usize].iter().enumerate() {
            let mv = mvs[k];
            let r = refs[k] as usize;
            let refp = self.ref_pic(0, r);
            self.luma_pred(0, r, x0 + bx * 4, y0 + by * 4, w * 4, h * 4, mv, &mut tmp);
            for r in 0..h * 4 {
                pred[(by * 4 + r) * 16 + bx * 4..][..w * 4].copy_from_slice(&tmp[r * w * 4..][..w * 4]);
            }
            // chroma from the same vector, in eighth samples
            for (c, pr) in cpred.iter_mut().enumerate() {
                chroma_mc(&refp.planes[1 + c], cs, self.mb_h * 8, (mbx * 8 + bx * 2) as i32, (mby * 8 + by * 2) as i32, mv[0] as i32, mv[1] as i32, w * 2, h * 2, &mut tmp);
                for r in 0..h * 2 {
                    pr[(by * 2 + r) * 8 + bx * 2..][..w * 2].copy_from_slice(&tmp[r * w * 2..][..w * 2]);
                }
            }
        }
        (pred, cpred)
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
                self.w.ue(code.part as u32); // P_L0_16x16, P_L0_L0_16x8, P_L0_L0_8x16 or P_8x8
                if code.part == 3 {
                    for _ in 0..4 {
                        self.w.ue(0); // P_L0_8x8
                    }
                }
                // ref_idx_l0 of each partition, as te(v) with the range of the list
                let nref = self.plist.len() as u32;
                if nref > 1 {
                    for &r in &code.refs[..PARTS[code.part as usize].len()] {
                        if nref == 2 {
                            self.w.flag(r == 0);
                        } else {
                            self.w.ue(r as u32);
                        }
                    }
                }
                for d in &code.mvds[..PARTS[code.part as usize].len()] {
                    self.w.se(d[0]);
                    self.w.se(d[1]);
                }
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
        let mut c = self.cabac.take().unwrap();
        self.mb_bins(&mut c, code, mbx, mby);
        c.terminate(last);
        self.cabac = Some(c);
    }

    /// The bins of a macroblock, up to end_of_slice_flag.
    fn mb_bins(&self, c: &mut impl Bins, code: &MbCode, mbx: usize, mby: usize) {
        let (x, y) = (mbx as i32, mby as i32);
        let left = self.nb_state(x - 1, y).copied();
        let top = self.nb_state(x, y - 1).copied();
        let cond = |n: Option<MbState>, f: &dyn Fn(&MbState) -> bool| n.as_ref().is_some_and(f) as usize;
        if self.p_slice {
            let ctx = if self.b_slice { 24 } else { 11 };
            c.decision(ctx + cond(left, &|m| !m.skip) + cond(top, &|m| !m.skip), code.skip);
            if code.skip {
                return;
            }
        }
        let cbpl = code.cbp & 15;
        let cbpc = code.cbp >> 4;
        // mb_type
        let (prefix_ok, base) = (self.p_slice, if self.b_slice { 32 } else if self.p_slice { 17 } else { 3 });
        let not_direct = |m: &MbState| !m.bdirect;
        let b_ctx = 27 + cond(left, &not_direct) + cond(top, &not_direct);
        match code.kind {
            // B_Direct_16x16 0, B_L0_16x16 100, B_L1_16x16 101, the others 11 and four or five more
            // bins (Table 9-37)
            Kind::Inter if self.b_slice => {
                let t = b_mb_type(code);
                c.decision(b_ctx, t != 0);
                if t == 1 || t == 2 {
                    c.decision(30, false);
                    c.decision(32, t == 2);
                } else if t != 0 {
                    c.decision(30, true);
                    let bits = match t {
                        3..=10 => t - 3,
                        11 => 14,
                        22 => 15,
                        _ => (t + 4) >> 1,
                    };
                    c.decision(31, bits >> 3 & 1 == 1);
                    for s in [2, 1, 0] {
                        c.decision(32, bits >> s & 1 == 1);
                    }
                    if (12..=21).contains(&t) {
                        c.decision(32, (t + 4) & 1 == 1);
                    }
                }
                if t == 22 {
                    // sub_mb_type: B_Direct_8x8 0, B_L0_8x8 100, B_L1_8x8 101, B_Bi_8x8 11000
                    for &d in &code.bdir {
                        c.decision(36, d != 0);
                        if d == 1 || d == 2 {
                            c.decision(37, false);
                            c.decision(39, d == 2);
                        } else if d == 3 {
                            for (ctx, bin) in [(37, true), (38, false), (39, false), (39, false)] {
                                c.decision(ctx, bin);
                            }
                        }
                    }
                }
            }
            Kind::Inter => {
                // P_L0_16x16 000, P_L0_L0_16x8 011, P_L0_L0_8x16 010, P_8x8 001
                let (b1, b2) = [(false, false), (true, true), (true, false), (false, true)][code.part as usize];
                c.decision(14, false);
                c.decision(15, b1);
                c.decision(if b1 { 17 } else { 16 }, b2);
                if code.part == 3 {
                    for _ in 0..4 {
                        c.decision(21, true); // P_L0_8x8
                    }
                }
            }
            kind => {
                let first = if self.b_slice {
                    // 111101: an intra type follows, with the suffix contexts
                    c.decision(b_ctx, true);
                    for (ctx, bin) in [(30, true), (31, true), (32, true), (32, false), (32, true)] {
                        c.decision(ctx, bin);
                    }
                    32
                } else if prefix_ok {
                    c.decision(14, true);
                    17
                } else {
                    let nxn = |m: &MbState| !matches!(m.kind, Kind::I4x4 | Kind::I8x8);
                    3 + cond(left, &nxn) + cond(top, &nxn)
                };
                if matches!(kind, Kind::I4x4 | Kind::I8x8) {
                    c.decision(first, false);
                    if self.transform8 {
                        c.decision(399 + cond(left, &|m| m.t8x8) + cond(top, &|m| m.t8x8), kind == Kind::I8x8);
                    }
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
        if matches!(code.kind, Kind::I4x4 | Kind::I8x8) {
            let t8 = code.kind == Kind::I8x8;
            let mut so_far = [2u8; 16];
            for n in 0..if t8 { 4 } else { 16 } {
                let (bx, by) = if t8 { ((n & 1) * 2, (n >> 1) * 2) } else { block_pos(n) };
                let mode = code.modes[by * 4 + bx];
                let pred = self.pred_i4_mode(&so_far, mbx, mby, bx, by);
                for b in [by * 4 + bx, by * 4 + bx + 1, (by + 1) * 4 + bx, (by + 1) * 4 + bx + 1].into_iter().take(if t8 { 4 } else { 1 }) {
                    so_far[b] = mode;
                }
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
            // ref_idx_l0 of each partition of a P macroblock with more than one reference (unary, with
            // the contexts of 9.3.3.1.1.6: a neighbouring partition's index above 0 counts)
            if !self.b_slice && self.plist.len() > 1 {
                let mut grid = [0u8; 4];
                for (k, &(bx, by, w, h)) in PARTS[code.part as usize].iter().enumerate() {
                    let a = if bx > 0 { grid[(by / 2) * 2 + (bx - 1) / 2] > 0 } else { left.is_some_and(|m| m.refs[0][(by / 2) * 2 + 1] > 0) };
                    let b = if by > 0 { grid[((by - 1) / 2) * 2 + bx / 2] > 0 } else { top.is_some_and(|m| m.refs[0][2 + bx / 2] > 0) };
                    let r = code.refs[k] as usize;
                    let base = 54 + a as usize + 2 * b as usize;
                    for i in 0..=r {
                        let ctx = match i {
                            0 => base,
                            1 => 58,
                            _ => 59,
                        };
                        c.decision(ctx, i < r);
                    }
                    for y8 in by / 2..=(by + h - 1) / 2 {
                        for x8 in bx / 2..=(bx + w - 1) / 2 {
                            grid[y8 * 2 + x8] = code.refs[k];
                        }
                    }
                }
            }
            // list 0's differences for the partitions using it, then list 1's; within the
            // macroblock, the blocks left of and above a partition belong to earlier ones
            for l in 0..if self.b_slice { 2 } else { 1 } {
                let mvds = if self.b_slice { &code.bmvd[l] } else { &code.mvds };
                let grid = mvd_grid(code.part, mvds);
                let nb = |m: &MbState, i: usize, comp: usize| (if l == 0 { m.mvd[i] } else { m.mvd1[i] })[comp];
                for (k, &(bx, by, _, _)) in PARTS[code.part as usize].iter().enumerate() {
                    if self.b_slice && code.bdir[k] >> l & 1 == 0 {
                        continue;
                    }
                    for comp in 0..2 {
                        let a = if bx > 0 { grid[by * 4 + bx - 1][comp] } else { left.map_or(0, |m| nb(&m, by * 4 + 3, comp)) };
                        let b = if by > 0 { grid[(by - 1) * 4 + bx][comp] } else { top.map_or(0, |m| nb(&m, 12 + bx, comp)) };
                        mvd_bins(c, comp, mvds[k][comp], a as u32 + b as u32);
                    }
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
        if code.kind == Kind::Inter && self.transform8 && cbpl != 0 {
            c.decision(399 + cond(left, &|m| m.t8x8) + cond(top, &|m| m.t8x8), code.t8x8);
        }
        if code.cbp != 0 || code.kind == Kind::I16x16 {
            c.decision(60, false); // mb_qp_delta 0, after a macroblock without a change
            // an unavailable neighbour counts as a coded block for an intra macroblock
            let un = code.kind != Kind::Inter;
            let flag = |n: Option<u8>| n.map_or(un, |v| v > 0) as usize;
            if code.kind == Kind::I16x16 {
                let dc = |n: Option<MbState>| n.map_or(un, |m| m.cbf_dc & 1 != 0) as usize;
                cabac::write_block(c, 0, Some(85 + dc(left) + 2 * dc(top)), &code.dc);
            }
            let (cat, base) = if code.kind == Kind::I16x16 { (1, 85 + 4) } else { (2, 85 + 8) };
            for i8 in (0..4).filter(|&i8| code.t8x8 && cbpl & (1 << i8) != 0) {
                cabac::write_block(c, 5, None, &code.luma8[i8]);
            }
            for n in 0..16 {
                let (bx, by) = block_pos(n);
                if code.t8x8 || cbpl & (1 << (n / 4)) == 0 {
                    continue;
                }
                let a = if bx > 0 { Some(code.nnz[by * 4 + bx - 1]) } else { left.map(|m| m.nnz[by * 4 + 3]) };
                let b = if by > 0 { Some(code.nnz[(by - 1) * 4 + bx]) } else { top.map(|m| m.nnz[12 + bx]) };
                let blk = &code.luma[by * 4 + bx];
                cabac::write_block(c, cat, Some(base + flag(a) + 2 * flag(b)), if cat == 1 { &blk[1..] } else { blk });
            }
            if cbpc > 0 {
                for comp in 0..2 {
                    let dc = |n: Option<MbState>| n.map_or(un, |m| m.cbf_dc & (2 << comp) != 0) as usize;
                    cabac::write_block(c, 3, Some(85 + 12 + dc(left) + 2 * dc(top)), &code.cdc[comp]);
                }
            }
            if cbpc == 2 {
                for comp in 0..2 {
                    for blk in 0..4 {
                        let (bx, by) = (blk & 1, blk >> 1);
                        let cur = &code.cnnz[comp];
                        let a = if bx > 0 { Some(cur[by * 2]) } else { left.map(|m| m.cnnz[comp][by * 2 + 1]) };
                        let b = if by > 0 { Some(cur[bx]) } else { top.map(|m| m.cnnz[comp][2 + bx]) };
                        cabac::write_block(c, 4, Some(85 + 16 + flag(a) + 2 * flag(b)), &code.cac[comp][blk][1..]);
                    }
                }
            }
        }
    }

    fn finish_mb(&mut self, code: &MbCode, mbx: usize, mby: usize) {
        let idx = mby * self.mb_w + mbx;
        let inter = code.kind == Kind::Inter;
        let none = [[0; 2]; 16];
        let b = inter && self.b_slice;
        let used = if !inter { [[false; 2]; 4] } else if b { code.buse } else { [[true, false]; 4] };
        let (mvd, mvd1) = match (inter && !code.skip, b) {
            (false, _) => (none, none),
            (true, false) => (mvd_grid(code.part, &code.mvds), none),
            (true, true) => (mvd_grid(code.part, &code.bmvd[0]), mvd_grid(code.part, &code.bmvd[1])),
        };
        let all = if b { code.bmv } else if inter { [spread(code.part, &code.mvs), [[0; 2]; 16]] } else { [[[0; 2]; 16]; 2] };
        let [mvs, mvs1] = [0, 1].map(|l| std::array::from_fn(|i| if used[(i / 8) * 2 + (i % 4) / 2][l] { all[l][i] } else { [0, 0] }));
        let bdirect = b && code.part == 0 && code.bdir[0] == 0;
        let cbpc = code.cbp >> 4;
        let cbf_dc = (code.kind == Kind::I16x16 && code.dc.iter().any(|&l| l != 0)) as u8
            | ((cbpc > 0 && code.cdc[0].iter().any(|&l| l != 0)) as u8) << 1
            | ((cbpc > 0 && code.cdc[1].iter().any(|&l| l != 0)) as u8) << 2;
        // a P macroblock's reference index per 8x8 block: that of the partition covering it
        let r16 = spread(code.part, &code.refs.map(|r| r as i8));
        let r8: [i8; 4] = std::array::from_fn(|b8| r16[(b8 / 2) * 8 + (b8 % 2) * 2]);
        self.state[idx] = MbState {
            i4: matches!(code.kind, Kind::I4x4 | Kind::I8x8),
            t8x8: code.t8x8,
            inter,
            modes: code.modes,
            nnz: code.nnz,
            cnnz: code.cnnz,
            mvs,
            mvs1,
            refs: [0, 1].map(|l| if l == 0 && inter && !b { r8 } else { used.map(|u| if u[l] { 0 } else { -1 }) }),
            bdirect,
            skip: code.skip,
            kind: code.kind,
            cbp: code.cbp,
            chroma_mode: if inter { 0 } else { code.chroma_mode },
            cbf_dc,
            mvd,
            mvd1,
        };
        self.counts[if code.skip { 2 } else if inter { 1 } else { 0 }] += 1;
        let mut info = MbInfo { slice: self.slice_id, kind: code.kind, qp: self.qp as i8, ..MbInfo::default() };
        info.nnz[..16].copy_from_slice(&code.nnz);
        info.cbp = code.cbp;
        info.skip = code.skip;
        info.t8x8 = code.t8x8;
        info.mv = [mvs, mvs1];
        for (l, b8) in (0..2).flat_map(|l| (0..4).map(move |b8| (l, b8))).filter(|&(l, b8)| used[b8][l]) {
            // the reference index as the decoder's list gives it; the identity tells the deblocking
            // filter which picture it is (B pictures use one reference per list)
            let r = if b { 0 } else { r8[b8] };
            info.ref_idx[l][b8] = r;
            info.ref_id[l][b8] = if b { 1 + l as i32 } else { 1 + r as i32 };
        }
        info.bdirect = bdirect;
        info.direct8 = if bdirect { 15 } else if b && code.part == 3 { (0..4).filter(|&k| code.bdir[k] == 0).fold(0, |m, k| m | 1 << k) } else { 0 };
        self.pic.mbs[idx] = info;
    }
}


/// The mb_type of a B macroblock (Table 7-14).
fn b_mb_type(code: &MbCode) -> u8 {
    const PAIRS: [(u8, u8); 9] = [(1, 1), (2, 2), (1, 2), (2, 1), (1, 3), (2, 3), (3, 1), (3, 2), (3, 3)];
    match code.part {
        0 => code.bdir[0],
        3 => 22,
        p => 4 + 2 * PAIRS.iter().position(|&d| d == (code.bdir[0], code.bdir[1])).unwrap() as u8 + (p == 2) as u8,
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

/// The bins of one component of a motion vector difference; `sum` is the neighbours' absolute
/// differences in that component (9.3.3.1.1.7).
fn mvd_bins(c: &mut impl Bins, comp: usize, d: i32, sum: u32) {
    let base = if comp == 0 { 40 } else { 47 };
    let inc = if sum < 3 { 0 } else if sum > 32 { 2 } else { 1 };
    let abs = d.unsigned_abs();
    c.decision(base + inc, abs > 0);
    if abs > 0 {
        let mut ctx = base + 3;
        for _ in 1..abs.min(9) {
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

/// `decimate_score` for the 64 levels of an 8x8 block.
fn decimate_score8(lv: &[i32; 64]) -> i32 {
    const RUN: [i32; 25] = [3, 3, 3, 3, 2, 2, 2, 2, 2, 2, 2, 2, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 1, 0];
    let mut score = 0;
    let mut i = 63i32;
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
        score += RUN[run.min(24)];
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
    fn the_8x8_transform_round_trips_at_low_qp() {
        for qp in [0, 1, 5, 6] {
            let src: [i32; 64] = std::array::from_fn(|i| ((i * 37) % 23) as i32 * 9 + (i / 8) as i32 * 5);
            let pred = [100i32; 64];
            let (_, co) = quant8(&src, &pred, qp, true);
            let mut out = [100u16; 64];
            idct8_add(&co, &mut out, 8, 255);
            for k in 0..64 {
                assert!((out[k] as i32 - src[k]).abs() <= 1 + qp / 3, "qp {qp} {k}: {} vs {}", out[k], src[k]);
            }
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
