//! State shared by the block parser, reconstruction and loop filter while one frame is decoded.

use std::sync::Arc;

use super::header::*;
use super::rac::Rac;

pub const DC_PRED: u8 = 2;
pub const NEARESTMV: u8 = 10;
pub const NEARMV: u8 = 11;
pub const ZEROMV: u8 = 12;
pub const NEWMV: u8 = 13;

pub const BS_8X8: usize = 9;
pub const BS_8X4: usize = 10;
pub const BS_4X8: usize = 11;
pub const BS_4X4: usize = 12;

#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Mv {
    pub x: i16,
    pub y: i16,
}

/// Motion of one 8x8 block, kept for the next frame's motion vector candidates.
#[derive(Clone, Copy, Debug)]
pub struct MvRef {
    pub r: [i8; 2],
    pub mv: [Mv; 2],
}

impl Default for MvRef {
    fn default() -> Self {
        MvRef { r: [-1, -1], mv: [Mv::default(); 2] }
    }
}

/// A decoded picture. Planes are allocated in whole superblocks; only `w` x `h` is meaningful.
pub struct Pic {
    pub w: usize,
    pub h: usize,
    pub ss_h: bool,
    pub ss_v: bool,
    pub bpp: u32,
    pub planes: [Vec<u16>; 3],
    pub stride: [usize; 3],
    pub mv: Vec<MvRef>,
    pub segmap: Vec<u8>,
    pub pts: i64,
}

impl Pic {
    pub fn new(w: usize, h: usize, ss_h: bool, ss_v: bool, bpp: u32, pts: i64) -> Pic {
        let (sbc, sbr) = (w.div_ceil(64), h.div_ceil(64));
        let (ys, yr) = (sbc * 64, sbr * 64);
        let (cs, cr) = (ys >> ss_h as usize, yr >> ss_v as usize);
        let mid = 1u16 << (bpp - 1);
        Pic {
            w,
            h,
            ss_h,
            ss_v,
            bpp,
            planes: [vec![mid; ys * yr], vec![mid; cs * cr], vec![mid; cs * cr]],
            stride: [ys, cs, cs],
            mv: vec![MvRef::default(); sbc * 8 * sbr * 8],
            segmap: vec![0; sbc * 8 * sbr * 8],
            pts,
        }
    }

    pub fn info(&self) -> RefInfo {
        RefInfo { w: self.w, h: self.h, ss_h: self.ss_h, ss_v: self.ss_v, bpp: self.bpp }
    }
}

#[derive(Default, Clone)]
pub struct MvCompCounts {
    pub sign: [u32; 2],
    pub classes: [u32; 11],
    pub class0: [u32; 2],
    pub bits: [[u32; 2]; 10],
    pub class0_fp: [[u32; 4]; 2],
    pub fp: [u32; 4],
    pub class0_hp: [u32; 2],
    pub hp: [u32; 2],
}

#[derive(Default, Clone)]
pub struct Counts {
    pub coef: [[[[[[u32; 3]; 6]; 6]; 2]; 2]; 4],
    pub eob: [[[[[[u32; 2]; 6]; 6]; 2]; 2]; 4],
    pub skip: [[u32; 2]; 3],
    pub intra: [[u32; 2]; 4],
    pub comp: [[u32; 2]; 5],
    pub comp_ref: [[u32; 2]; 5],
    pub single_ref: [[[u32; 2]; 2]; 5],
    pub partition: [[[u32; 4]; 4]; 4],
    pub tx8p: [[u32; 2]; 2],
    pub tx16p: [[u32; 3]; 2],
    pub tx32p: [[u32; 4]; 2],
    pub filter: [[u32; 3]; 4],
    pub mv_mode: [[u32; 4]; 7],
    pub mv_joint: [u32; 4],
    pub y_mode: [[u32; 10]; 4],
    pub uv_mode: [[u32; 10]; 10],
    pub mv_comp: [MvCompCounts; 2],
}

#[derive(Clone, Copy, Default)]
pub struct Blk {
    pub bs: usize,
    pub bl: usize,
    pub bp: usize,
    pub seg_id: usize,
    pub skip: bool,
    pub intra: bool,
    pub comp: bool,
    pub tx: usize,
    pub uvtx: usize,
    pub mode: [u8; 4],
    pub uvmode: u8,
    pub refs: [usize; 2],
    pub filter: u8,
    pub mv: [[Mv; 2]; 4],
}

/// Loop filter levels and edge masks of one superblock.
#[derive(Clone, Copy)]
pub struct Filt {
    pub level: [u8; 64],
    /// [luma/chroma][columns/rows][row][0=16, 1=8, 2=4, 3=inner 4]
    pub mask: [[[[u8; 4]; 8]; 2]; 2],
}

impl Default for Filt {
    fn default() -> Self {
        Filt { level: [0; 64], mask: [[[[0; 4]; 8]; 2]; 2] }
    }
}

pub struct Dec {
    pub h: FrameHeader,
    pub p: Probs,
    pub coef: CoefProbs,
    pub counts: Box<Counts>,
    pub cur: Pic,
    /// The three active references, by reference slot 0..2.
    pub refs: [Option<Arc<Pic>>; 3],
    /// The previous frame's motion, when it can be used for candidates.
    pub prev: Option<Arc<Pic>>,
    pub segmap_ref: Option<Vec<u8>>,
    pub c: Rac,
    pub cols: usize,
    pub rows: usize,
    pub sb_cols: usize,
    pub sb_rows: usize,
    pub ss_h: usize,
    pub ss_v: usize,
    pub bpp: u32,
    pub tile_col_start: usize,
    pub row: usize,
    pub col: usize,
    pub b: Blk,
    pub min_mv: Mv32,
    pub max_mv: Mv32,

    pub above_partition: Vec<u8>,
    pub above_skip: Vec<u8>,
    pub above_txfm: Vec<u8>,
    pub above_mode: Vec<u8>,
    pub above_y_nnz: Vec<u8>,
    pub above_uv_nnz: [Vec<u8>; 2],
    pub above_intra: Vec<u8>,
    pub above_comp: Vec<u8>,
    pub above_ref: Vec<u8>,
    pub above_filter: Vec<u8>,
    pub above_segpred: Vec<u8>,
    pub above_mv: Vec<[Mv; 2]>,

    pub left_partition: [u8; 8],
    pub left_skip: [u8; 8],
    pub left_txfm: [u8; 8],
    pub left_mode: [u8; 16],
    pub left_y_nnz: [u8; 16],
    pub left_uv_nnz: [[u8; 16]; 2],
    pub left_intra: [u8; 8],
    pub left_comp: [u8; 8],
    pub left_ref: [u8; 8],
    pub left_filter: [u8; 8],
    pub left_segpred: [u8; 8],
    pub left_mv: [[Mv; 2]; 16],

    pub block: Vec<i32>,
    pub uvblock: [Vec<i32>; 2],
    pub eob: Vec<u16>,
    pub uveob: [Vec<u16>; 2],
    pub lflvl: Vec<Filt>,
}

#[derive(Clone, Copy, Default)]
pub struct Mv32 {
    pub x: i32,
    pub y: i32,
}

impl Dec {
    pub fn new(h: FrameHeader, p: Probs, coef: CoefProbs, cur: Pic) -> Dec {
        let ss_h = h.ss_h as usize;
        let ss_v = h.ss_v as usize;
        let cols = h.width.div_ceil(8);
        let rows = h.height.div_ceil(8);
        let sb_cols = cols.div_ceil(8);
        let sb_rows = rows.div_ceil(8);
        let n = sb_cols * 8 + 16;
        Dec {
            bpp: h.bpp,
            h,
            p,
            coef,
            counts: Box::default(),
            cur,
            refs: [None, None, None],
            prev: None,
            segmap_ref: None,
            c: Rac::new(&[0]).unwrap(),
            cols,
            rows,
            sb_cols,
            sb_rows,
            ss_h,
            ss_v,
            tile_col_start: 0,
            row: 0,
            col: 0,
            b: Blk::default(),
            min_mv: Mv32::default(),
            max_mv: Mv32::default(),
            above_partition: vec![0; n],
            above_skip: vec![0; n],
            above_txfm: vec![0; n],
            above_mode: vec![0; 2 * n],
            above_y_nnz: vec![0; 2 * n],
            above_uv_nnz: [vec![0; 2 * n], vec![0; 2 * n]],
            above_intra: vec![0; n],
            above_comp: vec![0; n],
            above_ref: vec![0; n],
            above_filter: vec![0; n],
            above_segpred: vec![0; n],
            above_mv: vec![[Mv::default(); 2]; 2 * n],
            left_partition: [0; 8],
            left_skip: [0; 8],
            left_txfm: [0; 8],
            left_mode: [0; 16],
            left_y_nnz: [0; 16],
            left_uv_nnz: [[0; 16]; 2],
            left_intra: [0; 8],
            left_comp: [0; 8],
            left_ref: [0; 8],
            left_filter: [0; 8],
            left_segpred: [0; 8],
            left_mv: [[Mv::default(); 2]; 16],
            block: vec![0; 64 * 64],
            uvblock: [vec![0; 64 * 64], vec![0; 64 * 64]],
            eob: vec![0; 256],
            uveob: [vec![0; 256], vec![0; 256]],
            lflvl: vec![Filt::default(); sb_cols * sb_rows],
        }
    }

    pub fn keyintra(&self) -> bool {
        self.h.keyframe || self.h.intraonly
    }
}

pub fn splat(a: &mut [u8], off: usize, n: usize, v: u8) {
    for x in &mut a[off..off + n] {
        *x = v;
    }
}
