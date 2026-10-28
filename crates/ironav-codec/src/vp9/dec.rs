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

    /// A picture shaped like `like`, reusing one from `pool` when one fits. A reused picture still holds
    /// an earlier frame: `clear_cols` the columns that are to be decoded into it.
    pub fn from_pool(pool: &mut Vec<Pic>, like: &Pic) -> Pic {
        while let Some(mut p) = pool.pop() {
            if (p.w, p.h, p.ss_h, p.ss_v, p.bpp) == (like.w, like.h, like.ss_h, like.ss_v, like.bpp) {
                p.pts = like.pts;
                return p;
            }
        }
        Pic::new(like.w, like.h, like.ss_h, like.ss_v, like.bpp, like.pts)
    }

    /// Sets the 8x8 columns `col_start..col_end` back to what `Pic::new` puts there.
    pub fn clear_cols(&mut self, col_start: usize, col_end: usize) {
        let mid = 1u16 << (self.bpp - 1);
        for pl in 0..3 {
            let sh = if pl == 0 { 0 } else { self.ss_h as usize };
            let stride = self.stride[pl];
            let (x0, x1) = ((col_start * 8) >> sh, ((col_end * 8) >> sh).min(stride));
            for row in self.planes[pl].chunks_exact_mut(stride) {
                row[x0..x1].fill(mid);
            }
        }
        let w8 = self.stride[0] / 8;
        for row in self.mv.chunks_exact_mut(w8) {
            row[col_start..col_end].fill(MvRef::default());
        }
        for row in self.segmap.chunks_exact_mut(w8) {
            row[col_start..col_end].fill(0);
        }
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

/// Element-wise addition, for merging the counts of tile columns decoded on separate threads.
trait Acc {
    fn acc(&mut self, o: &Self);
}

impl Acc for u32 {
    fn acc(&mut self, o: &Self) {
        *self += *o;
    }
}

impl<T: Acc, const N: usize> Acc for [T; N] {
    fn acc(&mut self, o: &Self) {
        for (a, b) in self.iter_mut().zip(o) {
            a.acc(b);
        }
    }
}

impl Acc for MvCompCounts {
    fn acc(&mut self, o: &Self) {
        let MvCompCounts { sign, classes, class0, bits, class0_fp, fp, class0_hp, hp } = o;
        self.sign.acc(sign);
        self.classes.acc(classes);
        self.class0.acc(class0);
        self.bits.acc(bits);
        self.class0_fp.acc(class0_fp);
        self.fp.acc(fp);
        self.class0_hp.acc(class0_hp);
        self.hp.acc(hp);
    }
}

impl Counts {
    pub fn add(&mut self, o: &Counts) {
        let Counts { coef, eob, skip, intra, comp, comp_ref, single_ref, partition, tx8p, tx16p, tx32p, filter, mv_mode, mv_joint, y_mode, uv_mode, mv_comp } = o;
        self.coef.acc(coef);
        self.eob.acc(eob);
        self.skip.acc(skip);
        self.intra.acc(intra);
        self.comp.acc(comp);
        self.comp_ref.acc(comp_ref);
        self.single_ref.acc(single_ref);
        self.partition.acc(partition);
        self.tx8p.acc(tx8p);
        self.tx16p.acc(tx16p);
        self.tx32p.acc(tx32p);
        self.filter.acc(filter);
        self.mv_mode.acc(mv_mode);
        self.mv_joint.acc(mv_joint);
        self.y_mode.acc(y_mode);
        self.uv_mode.acc(uv_mode);
        self.mv_comp.acc(mv_comp);
    }
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
    /// Pictures of earlier forks, kept so that forking does not allocate a new picture every frame.
    pub pool: Vec<Pic>,
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
            pool: Vec::new(),
        }
    }

    pub fn keyintra(&self) -> bool {
        self.h.keyframe || self.h.intraonly
    }

    /// A decoder for other tile columns of the same frame: the same headers, probabilities, references
    /// and above contexts, with its own picture, left contexts and counts.
    pub fn fork(&mut self) -> Dec {
        let cur = Pic::from_pool(&mut self.pool, &self.cur);
        let mut d = Dec::new(self.h.clone(), self.p, self.coef, cur);
        d.refs = self.refs.clone();
        d.prev = self.prev.clone();
        d.segmap_ref = self.segmap_ref.clone();
        d.above_partition.clone_from(&self.above_partition);
        d.above_skip.clone_from(&self.above_skip);
        d.above_txfm.clone_from(&self.above_txfm);
        d.above_mode.clone_from(&self.above_mode);
        d.above_y_nnz.clone_from(&self.above_y_nnz);
        d.above_uv_nnz.clone_from(&self.above_uv_nnz);
        d.above_intra.clone_from(&self.above_intra);
        d.above_comp.clone_from(&self.above_comp);
        d.above_ref.clone_from(&self.above_ref);
        d.above_filter.clone_from(&self.above_filter);
        d.above_segpred.clone_from(&self.above_segpred);
        d.above_mv.clone_from(&self.above_mv);
        d
    }

    /// Takes the 8x8 columns `col_start..col_end` (whole tile columns) of the frame from `t`, a fork
    /// that decoded them, with their motion, segment map, loop filter levels and counts.
    pub fn merge_cols(&mut self, t: &Dec, col_start: usize, col_end: usize) {
        for pl in 0..3 {
            let sh = if pl == 0 { 0 } else { self.ss_h };
            let stride = self.cur.stride[pl];
            let (x0, x1) = ((col_start * 8) >> sh, ((col_end * 8) >> sh).min(stride));
            for (d, s) in self.cur.planes[pl].chunks_exact_mut(stride).zip(t.cur.planes[pl].chunks_exact(stride)) {
                d[x0..x1].copy_from_slice(&s[x0..x1]);
            }
        }
        let w8 = self.sb_cols * 8;
        for (d, s) in self.cur.mv.chunks_exact_mut(w8).zip(t.cur.mv.chunks_exact(w8)) {
            d[col_start..col_end].copy_from_slice(&s[col_start..col_end]);
        }
        for (d, s) in self.cur.segmap.chunks_exact_mut(w8).zip(t.cur.segmap.chunks_exact(w8)) {
            d[col_start..col_end].copy_from_slice(&s[col_start..col_end]);
        }
        let (sb0, sb1) = (col_start / 8, col_end.div_ceil(8));
        for (d, s) in self.lflvl.chunks_exact_mut(self.sb_cols).zip(t.lflvl.chunks_exact(self.sb_cols)) {
            d[sb0..sb1].copy_from_slice(&s[sb0..sb1]);
        }
        self.counts.add(&t.counts);
    }
}

pub fn splat(a: &mut [u8], off: usize, n: usize, v: u8) {
    for x in &mut a[off..off + n] {
        *x = v;
    }
}
