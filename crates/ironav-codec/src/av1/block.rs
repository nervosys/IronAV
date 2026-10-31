//! Tile decoding of intra frames (spec 5.11 and 7.10 to 7.13): superblock partitions, intra mode
//! info, palettes, transform sizes and types, coefficients, prediction and reconstruction.

use super::ipred::{predict_cfl, predict_intra, Edges, Plane};
use super::itx::inverse_transform;
use super::obu::{seg_qidx, FrameHeader, SequenceHeader, TX_MODE_SELECT};
use super::quant_tables::*;
use super::scan_tables::*;
use super::symbol::{Cdfs, SymbolDecoder};
use super::tables::*;
use ironav_core::{Error, ErrorCode, Result};

const DC_PRED: u8 = 0;
const SMOOTH_PRED: u8 = 9;
const SMOOTH_H_PRED: u8 = 11;
const UV_CFL_PRED: u8 = 13;
const SEG_LVL_SKIP: usize = 6;

/// Block size by log2 width and height in 4-sample units (22 where there is none).
const BLOCK_BY_DIMS: [[u8; 6]; 6] = [
    [0, 1, 16, 22, 22, 22],
    [2, 3, 4, 18, 22, 22],
    [17, 5, 6, 7, 20, 22],
    [22, 19, 8, 9, 10, 22],
    [22, 22, 21, 11, 12, 13],
    [22, 22, 22, 22, 14, 15],
];

pub fn block_of(wl: i32, hl: i32) -> usize {
    BLOCK_BY_DIMS[wl.max(0) as usize][hl.max(0) as usize] as usize
}

/// get_plane_residual_size.
pub fn plane_block(b: usize, ssx: usize, ssy: usize) -> usize {
    block_of(MI_W_LOG2[b] as i32 - ssx as i32, MI_H_LOG2[b] as i32 - ssy as i32)
}

fn partition_subsize(p: usize, b: usize) -> usize {
    let (wl, hl) = (MI_W_LOG2[b] as i32, MI_H_LOG2[b] as i32);
    match p {
        0 => b,
        1 | 4 | 5 => block_of(wl, hl - 1),
        2 | 6 | 7 => block_of(wl - 1, hl),
        3 => block_of(wl - 1, hl - 1),
        8 => block_of(wl, hl - 2),
        _ => block_of(wl - 2, hl),
    }
}

const INTRA_MODE_CONTEXT: [usize; 13] = [0, 1, 2, 3, 4, 4, 4, 4, 3, 0, 1, 2, 0];
const FILTER_INTRA_DIR: [u8; 5] = [0, 1, 2, 6, 0];
const COEFF_BASE_POS_CTX_OFFSET: [usize; 3] = [26, 31, 36];

/// What is kept of each 4x4 block of the frame.
#[derive(Clone, Copy, Default)]
pub struct Mi {
    pub size: u8,
    pub y_mode: u8,
    pub uv_mode: u8,
    pub skip: bool,
    pub seg: u8,
    pub tx: u8,
    pub pal: [u8; 2],
}

/// The mode info of the block being decoded.
#[derive(Default)]
struct Block {
    row: usize,
    col: usize,
    size: usize,
    bw4: usize,
    bh4: usize,
    has_chroma: bool,
    avail_u: bool,
    avail_l: bool,
    avail_u_c: bool,
    avail_l_c: bool,
    skip: bool,
    seg: usize,
    lossless: bool,
    y_mode: u8,
    uv_mode: u8,
    angle_y: i32,
    angle_uv: i32,
    cfl: [i32; 2],
    filter: Option<u8>,
    pal_size: [usize; 2],
    pal: [[u16; 8]; 3],
    tx: usize,
    max_luma_w: usize,
    max_luma_h: usize,
    qidx: i32,
}

/// One tile's symbol decoder and CDFs.
pub struct Tile<'a> {
    pub sd: SymbolDecoder<'a>,
    pub cdf: Box<Cdfs>,
}

fn unsupported(msg: &str) -> Error {
    Error::new(ErrorCode::UnsupportedFormat, format!("AV1: {msg}"))
}

/// The decoding state of one frame: its pictures, the per-4x4 mode info and the tile contexts.
pub struct FrameDec {
    pub seq: SequenceHeader,
    pub h: FrameHeader,
    pub planes: [Plane; 3],
    pub mi: Vec<Mi>,
    pub mi_stride: usize,
    pal_colors: Vec<[u16; 16]>,
    /// cdef_idx per 64x64 block (-1 where none was read).
    pub cdef_idx: Vec<i8>,
    pub cdef_stride: usize,
    /// The transform size of each 4x4 of each plane, for the loop filter.
    pub lf_tx: [Vec<u8>; 3],
    pub lf_stride: [usize; 3],
    /// DeltaLF of each 4x4 block.
    pub delta_lf: Vec<[i8; 4]>,
    pub ss: [usize; 2],
    pub bd: u32,
    pub num_planes: usize,
    sb4: usize,
    above_lvl: [Vec<u8>; 3],
    above_dc: [Vec<u8>; 3],
    left_lvl: [Vec<u8>; 3],
    left_dc: [Vec<u8>; 3],
    tile: [usize; 4],
    current_q: i32,
    dlf: [i32; 4],
    read_deltas: bool,
    decoded: [Vec<bool>; 3],
    b: Block,
    color_map: [Vec<u8>; 2],
    quant: Vec<u32>,
    dq: Vec<i32>,
    res: Vec<i32>,
}

const DEC_STRIDE: usize = 35;

impl FrameDec {
    pub fn new(seq: SequenceHeader, h: FrameHeader) -> FrameDec {
        let sb = if seq.use_128x128_superblock { 128 } else { 64 };
        let ss = [seq.subsampling_x as usize, seq.subsampling_y as usize];
        let (aw, ah) = ((h.mi_cols * 4).div_ceil(sb) * sb, (h.mi_rows * 4).div_ceil(sb) * sb);
        let num_planes = seq.num_planes();
        let planes = [Plane::new(aw, ah), Plane::new(aw >> ss[0], ah >> ss[1]), Plane::new(aw >> ss[0], ah >> ss[1])];
        let mi_n = h.mi_cols * h.mi_rows;
        let cdef_stride = h.mi_cols.div_ceil(16);
        let lf_stride = [aw / 4, (aw >> ss[0]) / 4, (aw >> ss[0]) / 4];
        let lf_tx = [vec![0; lf_stride[0] * ah / 4], vec![0; lf_stride[1] * (ah >> ss[1]) / 4], vec![0; lf_stride[2] * (ah >> ss[1]) / 4]];
        let actx = || vec![0u8; aw / 4 + 32];
        let lctx = || vec![0u8; ah / 4 + 32];
        FrameDec {
            pal_colors: if h.allow_screen_content_tools { vec![[0; 16]; mi_n] } else { Vec::new() },
            cdef_idx: vec![-1; cdef_stride * h.mi_rows.div_ceil(16)],
            cdef_stride,
            delta_lf: if h.delta_lf_present { vec![[0; 4]; mi_n] } else { Vec::new() },
            mi: vec![Mi::default(); mi_n],
            mi_stride: h.mi_cols,
            lf_tx,
            lf_stride,
            planes,
            ss,
            bd: seq.bit_depth,
            num_planes,
            sb4: sb / 4,
            above_lvl: [actx(), actx(), actx()],
            above_dc: [actx(), actx(), actx()],
            left_lvl: [lctx(), lctx(), lctx()],
            left_dc: [lctx(), lctx(), lctx()],
            tile: [0; 4],
            current_q: h.base_q_idx,
            dlf: [0; 4],
            read_deltas: false,
            decoded: [vec![false; DEC_STRIDE * DEC_STRIDE], vec![false; DEC_STRIDE * DEC_STRIDE], vec![false; DEC_STRIDE * DEC_STRIDE]],
            b: Block::default(),
            color_map: [vec![0; 64 * 64], vec![0; 64 * 64]],
            quant: vec![0; 1024],
            dq: vec![0; 1024],
            res: vec![0; 4096],
            seq,
            h,
        }
    }

    #[inline]
    fn mi_at(&self, r: usize, c: usize) -> &Mi {
        &self.mi[r * self.mi_stride + c]
    }

    #[inline]
    fn inside(&self, r: isize, c: isize) -> bool {
        let [rs, re, cs, ce] = self.tile;
        c >= cs as isize && c < ce as isize && r >= rs as isize && r < re as isize
    }

    fn sub(&self, plane: usize) -> (usize, usize) {
        if plane == 0 {
            (0, 0)
        } else {
            (self.ss[0], self.ss[1])
        }
    }

    /// decode_tile: the tile at (`tile_row`, `tile_col`) from `data`, starting from `cdf`. Returns
    /// the CDFs as the tile leaves them.
    pub fn decode_tile(&mut self, data: &[u8], tile_row: usize, tile_col: usize, cdf: Box<Cdfs>) -> Result<Box<Cdfs>> {
        self.tile = [self.h.mi_row_starts[tile_row], self.h.mi_row_starts[tile_row + 1], self.h.mi_col_starts[tile_col], self.h.mi_col_starts[tile_col + 1]];
        let mut t = Tile { sd: SymbolDecoder::new(data), cdf };
        t.sd.adapt = !self.h.disable_cdf_update;
        self.current_q = self.h.base_q_idx;
        self.dlf = [0; 4];
        for p in 0..3 {
            self.above_lvl[p].fill(0);
            self.above_dc[p].fill(0);
        }
        let sb4 = self.sb4;
        let sb_size = if sb4 == 32 { BLOCK_128X128 } else { BLOCK_64X64 };
        let [rs, re, cs, ce] = self.tile;
        for r in (rs..re).step_by(sb4) {
            for p in 0..3 {
                self.left_lvl[p].fill(0);
                self.left_dc[p].fill(0);
            }
            for c in (cs..ce).step_by(sb4) {
                self.read_deltas = self.h.delta_q_present;
                self.clear_cdef(r, c);
                self.clear_block_decoded(r, c);
                self.decode_partition(&mut t, r, c, sb_size)?;
            }
        }
        Ok(t.cdf)
    }

    fn clear_cdef(&mut self, r: usize, c: usize) {
        let n = if self.sb4 == 32 { 2 } else { 1 };
        for y in (r >> 4)..((r >> 4) + n).min(self.h.mi_rows.div_ceil(16)) {
            for x in (c >> 4)..((c >> 4) + n).min(self.cdef_stride) {
                self.cdef_idx[y * self.cdef_stride + x] = -1;
            }
        }
    }

    fn clear_block_decoded(&mut self, r: usize, c: usize) {
        let sb4 = self.sb4 as isize;
        for plane in 0..self.num_planes {
            let (ssx, ssy) = self.sub(plane);
            let sb_w4 = ((self.tile[3] - c) >> ssx) as isize;
            let sb_h4 = ((self.tile[1] - r) >> ssy) as isize;
            let d = &mut self.decoded[plane];
            for y in -1..=(sb4 >> ssy) {
                for x in -1..=(sb4 >> ssx) {
                    d[((y + 1) as usize) * DEC_STRIDE + (x + 1) as usize] = (y < 0 && x < sb_w4) || (x < 0 && y < sb_h4);
                }
            }
            d[((sb4 >> ssy) + 1) as usize * DEC_STRIDE] = false;
        }
    }

    #[inline]
    fn is_decoded(&self, plane: usize, y: isize, x: isize) -> bool {
        self.decoded[plane][((y + 1) as usize) * DEC_STRIDE + (x + 1) as usize]
    }

    fn decode_partition(&mut self, t: &mut Tile, r: usize, c: usize, bs: usize) -> Result<()> {
        let (mi_rows, mi_cols) = (self.h.mi_rows, self.h.mi_cols);
        if r >= mi_rows || c >= mi_cols {
            return Ok(());
        }
        let avail_u = self.inside(r as isize - 1, c as isize);
        let avail_l = self.inside(r as isize, c as isize - 1);
        let half = NUM_4X4_W[bs] as usize >> 1;
        let quarter = half >> 1;
        let has_rows = r + half < mi_rows;
        let has_cols = c + half < mi_cols;
        let p = if bs < BLOCK_8X8 {
            0
        } else {
            let bsl = MI_W_LOG2[bs] as usize;
            let above = avail_u && (MI_W_LOG2[self.mi_at(r - 1, c).size as usize] as usize) < bsl;
            let left = avail_l && (MI_H_LOG2[self.mi_at(r, c - 1).size as usize] as usize) < bsl;
            let ctx = left as usize * 2 + above as usize;
            let cdf: &mut [u16] = match bsl {
                1 => &mut t.cdf.partition_w8[ctx],
                5 => &mut t.cdf.partition_w128[ctx],
                _ => &mut t.cdf.partition[(bsl - 2) * 4 + ctx],
            };
            if has_rows && has_cols {
                t.sd.symbol(cdf)
            } else if has_cols || has_rows {
                // split_or_horz / split_or_vert: the probability of the partitions that would split
                let prob = |e: usize| -> u32 {
                    let at = |i: usize| if i + 1 >= cdf.len() { 0 } else { cdf[i] as u32 };
                    (if e > 0 { at(e - 1) } else { 32768 }) - at(e)
                };
                let set: &[usize] = if has_cols { &[2, 3, 4, 6, 7, 9] } else { &[1, 3, 4, 5, 6, 8] };
                let n = if bs == BLOCK_128X128 { 5 } else { 6 };
                let psum: u32 = set[..n].iter().map(|&e| prob(e)).sum();
                let split = t.sd.bool_with_prob(psum as u16);
                if split {
                    3
                } else if has_cols {
                    1
                } else {
                    2
                }
            } else {
                3
            }
        };
        let sub = partition_subsize(p, bs);
        let split = partition_subsize(3, bs);
        match p {
            0 => self.decode_block(t, r, c, sub)?,
            1 => {
                self.decode_block(t, r, c, sub)?;
                if has_rows {
                    self.decode_block(t, r + half, c, sub)?;
                }
            }
            2 => {
                self.decode_block(t, r, c, sub)?;
                if has_cols {
                    self.decode_block(t, r, c + half, sub)?;
                }
            }
            3 => {
                self.decode_partition(t, r, c, sub)?;
                self.decode_partition(t, r, c + half, sub)?;
                self.decode_partition(t, r + half, c, sub)?;
                self.decode_partition(t, r + half, c + half, sub)?;
            }
            4 => {
                self.decode_block(t, r, c, split)?;
                self.decode_block(t, r, c + half, split)?;
                self.decode_block(t, r + half, c, sub)?;
            }
            5 => {
                self.decode_block(t, r, c, sub)?;
                self.decode_block(t, r + half, c, split)?;
                self.decode_block(t, r + half, c + half, split)?;
            }
            6 => {
                self.decode_block(t, r, c, split)?;
                self.decode_block(t, r + half, c, split)?;
                self.decode_block(t, r, c + half, sub)?;
            }
            7 => {
                self.decode_block(t, r, c, sub)?;
                self.decode_block(t, r, c + half, split)?;
                self.decode_block(t, r + half, c + half, split)?;
            }
            8 => {
                for i in 0..4 {
                    if r + quarter * i < mi_rows {
                        self.decode_block(t, r + quarter * i, c, sub)?;
                    }
                }
            }
            _ => {
                for i in 0..4 {
                    if c + quarter * i < mi_cols {
                        self.decode_block(t, r, c + quarter * i, sub)?;
                    }
                }
            }
        }
        Ok(())
    }

    fn decode_block(&mut self, t: &mut Tile, r: usize, c: usize, bs: usize) -> Result<()> {
        let bw4 = NUM_4X4_W[bs] as usize;
        let bh4 = NUM_4X4_H[bs] as usize;
        let [ssx, ssy] = self.ss;
        let has_chroma = self.num_planes > 1 && !(ssy == 1 && bh4 == 1 && r & 1 == 0) && !(ssx == 1 && bw4 == 1 && c & 1 == 0);
        let (ri, ci) = (r as isize, c as isize);
        let avail_u = self.inside(ri - 1, ci);
        let avail_l = self.inside(ri, ci - 1);
        let (mut avail_u_c, mut avail_l_c) = (avail_u, avail_l);
        if has_chroma {
            if ssy == 1 && bh4 == 1 {
                avail_u_c = self.inside(ri - 2, ci);
            }
            if ssx == 1 && bw4 == 1 {
                avail_l_c = self.inside(ri, ci - 2);
            }
        } else {
            avail_u_c = false;
            avail_l_c = false;
        }
        self.b = Block { row: r, col: c, size: bs, bw4, bh4, has_chroma, avail_u, avail_l, avail_u_c, avail_l_c, ..Default::default() };
        self.mode_info(t)?;
        self.b.qidx = seg_qidx(&self.h, self.b.seg, false, self.current_q);
        self.palette_tokens(t);
        self.read_tx_size(t);
        if self.b.skip {
            self.reset_block_context();
        }
        self.store_mi();
        self.residual(t)
    }

    fn seg_feature(&self, seg: usize, f: usize) -> bool {
        self.h.seg_enabled && self.h.feature_enabled[seg][f]
    }

    fn mode_info(&mut self, t: &mut Tile) -> Result<()> {
        let (r, c) = (self.b.row, self.b.col);
        if self.h.seg_id_pre_skip {
            self.intra_segment_id(t);
        }
        self.b.skip = if self.h.seg_id_pre_skip && self.seg_feature(self.b.seg, SEG_LVL_SKIP) {
            true
        } else {
            let ctx = (self.b.avail_u && self.mi_at(r - 1, c).skip) as usize + (self.b.avail_l && self.mi_at(r, c - 1).skip) as usize;
            t.sd.symbol(&mut t.cdf.skip[ctx]) == 1
        };
        if !self.h.seg_id_pre_skip {
            self.intra_segment_id(t);
        }
        self.b.lossless = self.h.lossless[self.b.seg];
        self.read_cdef(t);
        self.read_delta_q(t);
        self.read_delta_lf(t);
        self.read_deltas = false;
        if self.h.allow_intrabc && t.sd.symbol(&mut t.cdf.intrabc) == 1 {
            return Err(unsupported("intra block copy is not decoded yet"));
        }
        let above = if self.b.avail_u { self.mi_at(r - 1, c).y_mode } else { DC_PRED };
        let left = if self.b.avail_l { self.mi_at(r, c - 1).y_mode } else { DC_PRED };
        let y = t.sd.symbol(&mut t.cdf.kf_y_mode[INTRA_MODE_CONTEXT[above as usize]][INTRA_MODE_CONTEXT[left as usize]]) as u8;
        self.b.y_mode = y;
        let bs = self.b.size;
        if bs >= BLOCK_8X8 && (1..=8).contains(&y) {
            self.b.angle_y = t.sd.symbol(&mut t.cdf.angle_delta[y as usize - 1]) as i32 - 3;
        }
        let (bw, bh) = (4usize << MI_W_LOG2[bs], 4usize << MI_H_LOG2[bs]);
        if self.b.has_chroma {
            let cfl_allowed = if self.b.lossless { plane_block(bs, self.ss[0], self.ss[1]) == BLOCK_4X4 } else { bw.max(bh) <= 32 };
            let uv = if cfl_allowed { t.sd.symbol(&mut t.cdf.uv_mode_cfl[y as usize]) } else { t.sd.symbol(&mut t.cdf.uv_mode[y as usize]) } as u8;
            self.b.uv_mode = uv;
            if uv == UV_CFL_PRED {
                let signs = t.sd.symbol(&mut t.cdf.cfl_sign) + 1;
                let (su, sv) = (signs / 3, signs % 3);
                for (i, (s, o)) in [(su, sv), (sv, su)].into_iter().enumerate() {
                    if s != 0 {
                        let a = t.sd.symbol(&mut t.cdf.cfl_alpha[(s - 1) * 3 + o]) as i32 + 1;
                        self.b.cfl[i] = if s == 1 { -a } else { a };
                    }
                }
            }
            if bs >= BLOCK_8X8 && (1..=8).contains(&uv) {
                self.b.angle_uv = t.sd.symbol(&mut t.cdf.angle_delta[uv as usize - 1]) as i32 - 3;
            }
        }
        if bs >= BLOCK_8X8 && bw <= 64 && bh <= 64 && self.h.allow_screen_content_tools {
            self.palette_mode_info(t);
        }
        if self.seq.enable_filter_intra && y == DC_PRED && self.b.pal_size[0] == 0 && bw.max(bh) <= 32 && t.sd.symbol(&mut t.cdf.filter_intra[bs]) == 1 {
            self.b.filter = Some(t.sd.symbol(&mut t.cdf.filter_intra_mode) as u8);
        }
        Ok(())
    }

    fn intra_segment_id(&mut self, t: &mut Tile) {
        if !self.h.seg_enabled {
            self.b.seg = 0;
            return;
        }
        let (r, c) = (self.b.row, self.b.col);
        let (au, al) = (self.b.avail_u, self.b.avail_l);
        let u = if au { self.mi_at(r - 1, c).seg as i32 } else { -1 };
        let l = if al { self.mi_at(r, c - 1).seg as i32 } else { -1 };
        let ul = if au && al { self.mi_at(r - 1, c - 1).seg as i32 } else { -1 };
        let pred = if u == -1 {
            l.max(0)
        } else if l == -1 || ul == u {
            u
        } else {
            l
        };
        if self.b.skip {
            self.b.seg = pred as usize;
            return;
        }
        let ctx = if ul < 0 {
            0
        } else if ul == u && ul == l {
            2
        } else if ul == u || ul == l || u == l {
            1
        } else {
            0
        };
        let v = t.sd.symbol(&mut t.cdf.spatial_seg[ctx]) as i32;
        let last = self.h.last_active_seg_id as i32;
        self.b.seg = neg_deinterleave(v, pred, last + 1).clamp(0, last) as usize;
    }

    fn read_cdef(&mut self, t: &mut Tile) {
        if self.b.skip || self.h.coded_lossless || !self.seq.enable_cdef || self.h.allow_intrabc {
            return;
        }
        let (r, c) = (self.b.row, self.b.col);
        let i = (r >> 4) * self.cdef_stride + (c >> 4);
        if self.cdef_idx[i] == -1 {
            let v = t.sd.literal(self.h.cdef_bits) as i8;
            self.cdef_idx[i] = v;
            let (bw4, bh4) = (self.b.bw4, self.b.bh4);
            for y in (r..r + bh4).step_by(16).filter(|&y| y < self.h.mi_rows) {
                for x in (c..c + bw4).step_by(16).filter(|&x| x < self.h.mi_cols) {
                    self.cdef_idx[(y >> 4) * self.cdef_stride + (x >> 4)] = v;
                }
            }
        }
    }

    fn sb_skip(&self) -> bool {
        let sb_size = if self.sb4 == 32 { BLOCK_128X128 } else { BLOCK_64X64 };
        self.b.size == sb_size && self.b.skip
    }

    fn read_delta_q(&mut self, t: &mut Tile) {
        if self.sb_skip() || !self.read_deltas {
            return;
        }
        let mut abs = t.sd.symbol(&mut t.cdf.delta_q) as i32;
        if abs == 3 {
            let bits = t.sd.literal(3) + 1;
            abs = t.sd.literal(bits) as i32 + (1 << bits) + 1;
        }
        if abs != 0 {
            let v = if t.sd.literal(1) == 1 { -abs } else { abs };
            self.current_q = (self.current_q + (v << self.h.delta_q_res)).clamp(1, 255);
        }
    }

    fn read_delta_lf(&mut self, t: &mut Tile) {
        if self.sb_skip() || !self.read_deltas || !self.h.delta_lf_present {
            return;
        }
        let n = if self.h.delta_lf_multi { if self.num_planes > 1 { 4 } else { 2 } } else { 1 };
        for i in 0..n {
            let cdf: &mut [u16] = if self.h.delta_lf_multi { &mut t.cdf.delta_lf_multi[i] } else { &mut t.cdf.delta_lf };
            let mut abs = t.sd.symbol(cdf) as i32;
            if abs == 3 {
                let bits = t.sd.literal(3) + 1;
                abs = t.sd.literal(bits) as i32 + (1 << bits) + 1;
            }
            if abs != 0 {
                let v = if t.sd.literal(1) == 1 { -abs } else { abs };
                self.dlf[i] = (self.dlf[i] + (v << self.h.delta_lf_res)).clamp(-63, 63);
            }
        }
    }

    /// get_palette_cache: the sorted, distinct colours of the above and left palettes.
    fn palette_cache(&self, plane: usize, cache: &mut [u16; 16]) -> usize {
        let (r, c) = (self.b.row, self.b.col);
        let above_n = if (r * 4) % 64 != 0 && self.b.avail_u { self.mi_at(r - 1, c).pal[plane] as usize } else { 0 };
        let left_n = if self.b.avail_l { self.mi_at(r, c - 1).pal[plane] as usize } else { 0 };
        let above = if above_n > 0 { &self.pal_colors[(r - 1) * self.mi_stride + c][plane * 8..] } else { &[][..] };
        let left = if left_n > 0 { &self.pal_colors[r * self.mi_stride + c - 1][plane * 8..] } else { &[][..] };
        let (mut ai, mut li, mut n) = (0, 0, 0);
        let mut push = |v: u16, n: &mut usize| {
            if *n == 0 || v != cache[*n - 1] {
                cache[*n] = v;
                *n += 1;
            }
        };
        while ai < above_n && li < left_n {
            let (a, l) = (above[ai], left[li]);
            if l < a {
                push(l, &mut n);
                li += 1;
            } else {
                push(a, &mut n);
                ai += 1;
                if l == a {
                    li += 1;
                }
            }
        }
        for &v in &above[ai..above_n] {
            push(v, &mut n);
        }
        for &v in &left[li..left_n] {
            push(v, &mut n);
        }
        n
    }

    /// The colours of a luma or U palette: cached ones, then ascending deltas (`plus` is 1 for luma).
    fn palette_colors(&mut self, t: &mut Tile, plane: usize, n: usize, plus: u32) {
        let mut cache = [0u16; 16];
        let cn = self.palette_cache(plane, &mut cache);
        let bd = self.bd;
        let mut colors = [0u16; 8];
        let mut idx = 0;
        for &v in &cache[..cn] {
            if idx >= n {
                break;
            }
            if t.sd.literal(1) == 1 {
                colors[idx] = v;
                idx += 1;
            }
        }
        if idx < n {
            colors[idx] = t.sd.literal(bd) as u16;
            idx += 1;
        }
        if idx < n {
            let mut bits = bd - 3 + t.sd.literal(2);
            while idx < n {
                let delta = t.sd.literal(bits) + plus;
                colors[idx] = (colors[idx - 1] as u32 + delta).min((1 << bd) - 1) as u16;
                let range = (1i32 << bd) - colors[idx] as i32 - plus as i32;
                bits = bits.min(ceil_log2(range));
                idx += 1;
            }
        }
        colors[..n].sort_unstable();
        self.b.pal[plane] = colors;
    }

    fn palette_mode_info(&mut self, t: &mut Tile) {
        let (r, c, bs) = (self.b.row, self.b.col, self.b.size);
        let bsize_ctx = MI_W_LOG2[bs] as usize + MI_H_LOG2[bs] as usize - 2;
        let bd = self.bd;
        if self.b.y_mode == DC_PRED {
            let ctx = (self.b.avail_u && self.mi_at(r - 1, c).pal[0] > 0) as usize + (self.b.avail_l && self.mi_at(r, c - 1).pal[0] > 0) as usize;
            if t.sd.symbol(&mut t.cdf.palette_y_mode[bsize_ctx][ctx]) == 1 {
                let n = t.sd.symbol(&mut t.cdf.palette_y_size[bsize_ctx]) + 2;
                self.b.pal_size[0] = n;
                self.palette_colors(t, 0, n, 1);
            }
        }
        if self.b.has_chroma && self.b.uv_mode == DC_PRED {
            let ctx = (self.b.pal_size[0] > 0) as usize;
            if t.sd.symbol(&mut t.cdf.palette_uv_mode[ctx]) == 1 {
                let n = t.sd.symbol(&mut t.cdf.palette_uv_size[bsize_ctx]) + 2;
                self.b.pal_size[1] = n;
                self.palette_colors(t, 1, n, 0);
                let mut v = [0u16; 8];
                if t.sd.literal(1) == 1 {
                    let max = 1i32 << bd;
                    let bits = bd - 4 + t.sd.literal(2);
                    v[0] = t.sd.literal(bd) as u16;
                    for i in 1..n {
                        let mut d = t.sd.literal(bits) as i32;
                        if d != 0 && t.sd.literal(1) == 1 {
                            d = -d;
                        }
                        let mut val = v[i - 1] as i32 + d;
                        if val < 0 {
                            val += max;
                        }
                        if val >= max {
                            val -= max;
                        }
                        v[i] = val.clamp(0, max - 1) as u16;
                    }
                } else {
                    for x in v.iter_mut().take(n) {
                        *x = t.sd.literal(bd) as u16;
                    }
                }
                self.b.pal[2] = v;
            }
        }
    }

    fn palette_tokens(&mut self, t: &mut Tile) {
        let (r, c, bs) = (self.b.row, self.b.col, self.b.size);
        let (bw, bh) = (4usize << MI_W_LOG2[bs], 4usize << MI_H_LOG2[bs]);
        let oh = bh.min((self.h.mi_rows - r) * 4);
        let ow = bw.min((self.h.mi_cols - c) * 4);
        if self.b.pal_size[0] > 0 {
            self.color_map(t, 0, bw, bh, ow, oh);
        }
        if self.b.pal_size[1] > 0 {
            let [ssx, ssy] = self.ss;
            let (mut bw, mut bh, mut ow, mut oh) = (bw >> ssx, bh >> ssy, ow >> ssx, oh >> ssy);
            if bw < 4 {
                bw += 2;
                ow += 2;
            }
            if bh < 4 {
                bh += 2;
                oh += 2;
            }
            self.color_map(t, 1, bw, bh, ow, oh);
        }
    }

    fn color_map(&mut self, t: &mut Tile, pt: usize, bw: usize, bh: usize, ow: usize, oh: usize) {
        let n = self.b.pal_size[pt];
        let map = &mut self.color_map[pt];
        map[0] = t.sd.ns(n as u32) as u8;
        for i in 1..(oh + ow - 1) {
            let lo = (i + 1).saturating_sub(oh);
            for j in (lo..=i.min(ow - 1)).rev() {
                let (rr, cc) = (i - j, j);
                let mut scores = [0u32; 8];
                let mut order = [0u8, 1, 2, 3, 4, 5, 6, 7];
                if cc > 0 {
                    scores[map[rr * 64 + cc - 1] as usize] += 2;
                }
                if rr > 0 && cc > 0 {
                    scores[map[(rr - 1) * 64 + cc - 1] as usize] += 1;
                }
                if rr > 0 {
                    scores[map[(rr - 1) * 64 + cc] as usize] += 2;
                }
                for k in 0..3 {
                    let mut best = k;
                    for m in k + 1..n {
                        if scores[m] > scores[best] {
                            best = m;
                        }
                    }
                    if best != k {
                        let (s, o) = (scores[best], order[best]);
                        for m in (k + 1..=best).rev() {
                            scores[m] = scores[m - 1];
                            order[m] = order[m - 1];
                        }
                        scores[k] = s;
                        order[k] = o;
                    }
                }
                let hash = scores[0] + 2 * scores[1] + 2 * scores[2];
                let ctx = PALETTE_COLOR_CONTEXT[hash as usize] as usize;
                let cdf = if pt == 0 { &mut t.cdf.palette_y_color[n - 2][ctx][..n] } else { &mut t.cdf.palette_uv_color[n - 2][ctx][..n] };
                map[rr * 64 + cc] = order[t.sd.symbol(cdf)];
            }
        }
        for i in 0..oh {
            let v = map[i * 64 + ow - 1];
            map[i * 64 + ow..i * 64 + bw].fill(v);
        }
        for i in oh..bh {
            map.copy_within((oh - 1) * 64..(oh - 1) * 64 + bw, i * 64);
        }
    }

    fn read_tx_size(&mut self, t: &mut Tile) {
        let bs = self.b.size;
        if self.b.lossless {
            self.b.tx = TX_4X4;
            return;
        }
        let max_rect = MAX_TX_SIZE_RECT[bs];
        let mut tx = max_rect;
        if bs > BLOCK_4X4 && self.h.tx_mode == TX_MODE_SELECT {
            let (r, c) = (self.b.row, self.b.col);
            let above_w = if self.b.avail_u { 1usize << TX_W_LOG2[self.mi_at(r - 1, c).tx as usize] } else { 0 };
            let left_h = if self.b.avail_l { 1usize << TX_H_LOG2[self.mi_at(r, c - 1).tx as usize] } else { 0 };
            let ctx = (above_w >= 1 << TX_W_LOG2[max_rect]) as usize + (left_h >= 1 << TX_H_LOG2[max_rect]) as usize;
            let cat = MAX_TX_DEPTH_OF[bs] as usize - 1;
            let depth = if cat == 0 { t.sd.symbol(&mut t.cdf.tx_size_8x8[ctx]) } else { t.sd.symbol(&mut t.cdf.tx_size[cat - 1][ctx]) };
            for _ in 0..depth {
                tx = SPLIT_TX_SIZE[tx];
            }
        }
        self.b.tx = tx;
    }

    fn reset_block_context(&mut self) {
        let (r, c) = (self.b.row, self.b.col);
        for plane in 0..1 + 2 * self.b.has_chroma as usize {
            let (ssx, ssy) = self.sub(plane);
            for i in (c >> ssx)..((c + self.b.bw4) >> ssx) {
                self.above_lvl[plane][i] = 0;
                self.above_dc[plane][i] = 0;
            }
            for i in (r >> ssy)..((r + self.b.bh4) >> ssy) {
                self.left_lvl[plane][i] = 0;
                self.left_dc[plane][i] = 0;
            }
        }
    }

    fn store_mi(&mut self) {
        let b = &self.b;
        let (r, c) = (b.row, b.col);
        let rows = b.bh4.min(self.h.mi_rows - r);
        let cols = b.bw4.min(self.h.mi_cols - c);
        let pal = b.pal_size[0] > 0 || b.pal_size[1] > 0;
        let mut colors = [0u16; 16];
        colors[..8].copy_from_slice(&b.pal[0]);
        colors[8..].copy_from_slice(&b.pal[1]);
        let dlf = [self.dlf[0] as i8, self.dlf[1] as i8, self.dlf[2] as i8, self.dlf[3] as i8];
        for y in r..r + rows {
            for x in c..c + cols {
                let i = y * self.mi_stride + x;
                let m = &mut self.mi[i];
                m.size = b.size as u8;
                m.y_mode = b.y_mode;
                if b.has_chroma {
                    m.uv_mode = b.uv_mode;
                }
                m.skip = b.skip;
                m.seg = b.seg as u8;
                m.tx = b.tx as u8;
                m.pal = [b.pal_size[0] as u8, b.pal_size[1] as u8];
                if pal {
                    self.pal_colors[i] = colors;
                }
                if !self.delta_lf.is_empty() {
                    self.delta_lf[i] = dlf;
                }
            }
        }
    }

    /// get_tx_size for a plane.
    fn plane_tx(&self, plane: usize) -> usize {
        if self.b.lossless {
            return TX_4X4;
        }
        if plane == 0 {
            return self.b.tx;
        }
        let uv = MAX_TX_SIZE_RECT[plane_block(self.b.size, self.ss[0], self.ss[1])];
        if TX_W_LOG2[uv] == 6 || TX_H_LOG2[uv] == 6 {
            return if TX_W_LOG2[uv] == 4 {
                9
            } else if TX_H_LOG2[uv] == 4 {
                10
            } else {
                TX_32X32
            };
        }
        uv
    }

    fn residual(&mut self, t: &mut Tile) -> Result<()> {
        let (r, c, bs) = (self.b.row, self.b.col, self.b.size);
        let (bw, bh) = (4usize << MI_W_LOG2[bs], 4usize << MI_H_LOG2[bs]);
        let (wc, hc) = ((bw >> 6).max(1), (bh >> 6).max(1));
        let size_chunk = if wc > 1 || hc > 1 { BLOCK_64X64 } else { bs };
        for cy in 0..hc {
            for cx in 0..wc {
                for plane in 0..1 + 2 * self.b.has_chroma as usize {
                    let tx = self.plane_tx(plane);
                    let step_x = 1usize << (TX_W_LOG2[tx] - 2);
                    let step_y = 1usize << (TX_H_LOG2[tx] - 2);
                    let (ssx, ssy) = self.sub(plane);
                    let psz = plane_block(size_chunk, ssx, ssy);
                    let (n4w, n4h) = (NUM_4X4_W[psz] as usize, NUM_4X4_H[psz] as usize);
                    let (base_x, base_y) = ((c >> ssx) * 4, (r >> ssy) * 4);
                    for y in (0..n4h).step_by(step_y) {
                        for x in (0..n4w).step_by(step_x) {
                            self.transform_block(t, plane, base_x, base_y, tx, x + ((cx << 4) >> ssx), y + ((cy << 4) >> ssy))?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// get_filter_type: whether a neighbour of the block uses a smooth predictor.
    fn filter_type(&self, plane: usize) -> bool {
        let (r, c) = (self.b.row, self.b.col);
        let [ssx, ssy] = self.ss;
        let smooth = |r: usize, c: usize| {
            let m = self.mi_at(r, c);
            let mode = if plane == 0 { m.y_mode } else { m.uv_mode };
            (SMOOTH_PRED..=SMOOTH_H_PRED).contains(&mode)
        };
        let mut s = false;
        if if plane == 0 { self.b.avail_u } else { self.b.avail_u_c } {
            let (mut rr, mut cc) = (r - 1, c);
            if plane > 0 {
                if ssx == 1 && c & 1 == 0 {
                    cc += 1;
                }
                if ssy == 1 && r & 1 == 1 {
                    rr -= 1;
                }
            }
            s |= smooth(rr, cc);
        }
        if if plane == 0 { self.b.avail_l } else { self.b.avail_l_c } {
            let (mut rr, mut cc) = (r, c - 1);
            if plane > 0 {
                if ssx == 1 && c & 1 == 1 {
                    cc -= 1;
                }
                if ssy == 1 && r & 1 == 0 {
                    rr += 1;
                }
            }
            s |= smooth(rr, cc);
        }
        s
    }

    #[allow(clippy::too_many_arguments)]
    fn transform_block(&mut self, t: &mut Tile, plane: usize, base_x: usize, base_y: usize, tx: usize, x: usize, y: usize) -> Result<()> {
        let (sx, sy) = (base_x + 4 * x, base_y + 4 * y);
        let (ssx, ssy) = self.sub(plane);
        let (max_x, max_y) = ((self.h.mi_cols * 4) >> ssx, (self.h.mi_rows * 4) >> ssy);
        if sx >= max_x || sy >= max_y {
            return Ok(());
        }
        let row = (sy << ssy) >> 2;
        let col = (sx << ssx) >> 2;
        let mask = self.sb4 - 1;
        let (dr, dc) = ((row & mask) >> ssy, (col & mask) >> ssx);
        let (log2w, log2h) = (TX_W_LOG2[tx] as u32, TX_H_LOG2[tx] as u32);
        let (step_x, step_y) = (1usize << (log2w - 2), 1usize << (log2h - 2));
        let pt = (plane > 0) as usize;
        if self.b.pal_size[pt] > 0 {
            let (w, h) = (1usize << log2w, 1usize << log2h);
            let map = &self.color_map[pt];
            let pal = &self.b.pal[plane];
            let p = &mut self.planes[plane];
            for i in 0..h {
                let row = (sy + i) * p.stride + sx;
                for j in 0..w {
                    p.data[row + j] = pal[map[(y * 4 + i) * 64 + x * 4 + j] as usize];
                }
            }
        } else {
            let is_cfl = plane > 0 && self.b.uv_mode == UV_CFL_PRED;
            let mode = if plane == 0 {
                self.b.y_mode
            } else if is_cfl {
                DC_PRED
            } else {
                self.b.uv_mode
            };
            let e = Edges {
                have_left: (if plane == 0 { self.b.avail_l } else { self.b.avail_l_c }) || x > 0,
                have_above: (if plane == 0 { self.b.avail_u } else { self.b.avail_u_c }) || y > 0,
                have_above_right: self.is_decoded(plane, dr as isize - 1, (dc + step_x) as isize),
                have_below_left: self.is_decoded(plane, (dr + step_y) as isize, dc as isize - 1),
                max_x: max_x as i32 - 1,
                max_y: max_y as i32 - 1,
            };
            let smooth = self.filter_type(plane);
            let (angle, filter) = if plane == 0 { (self.b.angle_y, self.b.filter) } else { (self.b.angle_uv, None) };
            let edge_filter = self.seq.enable_intra_edge_filter;
            let bd = self.bd;
            predict_intra(&mut self.planes[plane], sx, sy, log2w, log2h, mode, angle, filter, &e, smooth, edge_filter, bd);
            if is_cfl {
                let (luma, chroma) = self.planes.split_at_mut(1);
                predict_cfl(&mut chroma[plane - 1], sx, sy, log2w, log2h, ssx as u32, ssy as u32, self.b.cfl[plane - 1], &luma[0], self.b.max_luma_w, self.b.max_luma_h, bd);
            }
        }
        if plane == 0 {
            self.b.max_luma_w = sx + step_x * 4;
            self.b.max_luma_h = sy + step_y * 4;
        }
        if !self.b.skip {
            let (eob, tx_type) = self.coeffs(t, plane, sx, sy, tx)?;
            if eob > 0 {
                let lossless = self.b.lossless;
                inverse_transform(&self.dq, tx, tx_type, self.bd, lossless, &mut self.res);
                let (w, h) = (1usize << log2w, 1usize << log2h);
                let max = (1i32 << self.bd) - 1;
                let p = &mut self.planes[plane];
                for i in 0..h {
                    let row = (sy + i) * p.stride + sx;
                    for j in 0..w {
                        let v = &mut p.data[row + j];
                        *v = (*v as i32 + self.res[i * w + j]).clamp(0, max) as u16;
                    }
                }
            }
        }
        let ls = self.lf_stride[plane];
        for i in 0..step_y {
            for j in 0..step_x {
                if let Some(v) = self.lf_tx[plane].get_mut(((row >> ssy) + i) * ls + (col >> ssx) + j) {
                    *v = tx as u8;
                }
                self.decoded[plane][(dr + i + 1) * DEC_STRIDE + dc + j + 1] = true;
            }
        }
        Ok(())
    }

    /// get_tx_set for an intra block: 0 DCT only, 1 intra set 1, 2 intra set 2.
    fn tx_set(&self, tx: usize) -> u8 {
        let up = TX_SIZE_SQR_UP[tx];
        if up >= TX_32X32 {
            0
        } else if self.h.reduced_tx_set || TX_SIZE_SQR[tx] == TX_16X16 {
            2
        } else {
            1
        }
    }

    /// The transform type of a transform block: read for luma (transform_type), derived for chroma.
    fn tx_type(&mut self, t: &mut Tile, plane: usize, tx: usize) -> u8 {
        let set = self.tx_set(tx);
        if plane == 0 {
            let q = if self.h.seg_enabled { seg_qidx(&self.h, self.b.seg, true, 0) } else { self.h.base_q_idx };
            if set == 0 || q == 0 {
                return DCT_DCT;
            }
            let dir = match self.b.filter {
                Some(m) => FILTER_INTRA_DIR[m as usize],
                None => self.b.y_mode,
            } as usize;
            let sqr = TX_SIZE_SQR[tx];
            let tt = if set == 1 {
                TX_TYPE_INTRA_INV_SET1[t.sd.symbol(&mut t.cdf.intra_tx_1[sqr][dir])]
            } else {
                TX_TYPE_INTRA_INV_SET2[t.sd.symbol(&mut t.cdf.intra_tx_2[sqr][dir])]
            };
            return if self.b.lossless { DCT_DCT } else { tt };
        }
        if self.b.lossless || TX_SIZE_SQR_UP[tx] > TX_32X32 {
            return DCT_DCT;
        }
        let tt = MODE_TO_TXFM[self.b.uv_mode as usize];
        if tx_type_in_intra_set(set, tt) {
            tt
        } else {
            DCT_DCT
        }
    }

    /// coeffs: reads a transform block's coefficients, dequantises them into `dq` and updates the
    /// above and left contexts. Returns the end of block and the transform type.
    fn coeffs(&mut self, t: &mut Tile, plane: usize, sx: usize, sy: usize, tx: usize) -> Result<(usize, u8)> {
        let (x4, y4) = (sx >> 2, sy >> 2);
        let (log2w, log2h) = (TX_W_LOG2[tx] as usize, TX_H_LOG2[tx] as usize);
        let (w4, h4) = (1usize << (log2w - 2), 1usize << (log2h - 2));
        let (ssx, ssy) = self.sub(plane);
        let (max_x4, max_y4) = (self.h.mi_cols >> ssx, self.h.mi_rows >> ssy);
        let tx_ctx = (TX_SIZE_SQR[tx] + TX_SIZE_SQR_UP[tx] + 1) >> 1;
        let ptype = (plane > 0) as usize;
        let bsize = plane_block(self.b.size, ssx, ssy);
        let (bw, bh) = (4usize << MI_W_LOG2[bsize], 4usize << MI_H_LOG2[bsize]);
        let ax = x4..(x4 + w4).min(max_x4);
        let ly = y4..(y4 + h4).min(max_y4);
        let ctx = if plane == 0 {
            let top = self.above_lvl[0][ax.clone()].iter().copied().max().unwrap_or(0);
            let left = self.left_lvl[0][ly.clone()].iter().copied().max().unwrap_or(0);
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
        let all_zero = t.sd.symbol(&mut t.cdf.txb_skip[tx_ctx][ctx]) == 1;
        let mut cul = 0u32;
        let mut dc_cat = 0u8;
        let mut eob = 0;
        let mut tt = DCT_DCT;
        if !all_zero {
            tt = self.tx_type(t, plane, tx);
            let class = tx_class(tt);
            let scan = scan_for(tx, tt);
            let eob_multi = log2w.min(5) + log2h.min(5) - 4;
            let ectx = (class != 0) as usize;
            let c = &mut t.cdf;
            let eob_pt = 1 + match eob_multi {
                0 => t.sd.symbol(&mut c.eob16[ptype][ectx]),
                1 => t.sd.symbol(&mut c.eob32[ptype][ectx]),
                2 => t.sd.symbol(&mut c.eob64[ptype][ectx]),
                3 => t.sd.symbol(&mut c.eob128[ptype][ectx]),
                4 => t.sd.symbol(&mut c.eob256[ptype][ectx]),
                5 => t.sd.symbol(&mut c.eob512[ptype][ectx]),
                _ => t.sd.symbol(&mut c.eob1024[ptype][ectx]),
            };
            eob = if eob_pt < 2 { eob_pt } else { (1 << (eob_pt - 2)) + 1 };
            if eob_pt >= 3 {
                if t.sd.symbol(&mut c.eob_extra[tx_ctx][ptype][eob_pt - 3]) == 1 {
                    eob += 1 << (eob_pt - 3);
                }
                for i in 1..eob_pt - 2 {
                    if t.sd.literal(1) == 1 {
                        eob += 1 << (eob_pt - 3 - i);
                    }
                }
            }
            let adj = ADJUSTED_TX_SIZE[tx];
            let bwl = TX_W_LOG2[adj] as usize;
            let txh = 1usize << TX_H_LOG2[adj];
            let n = txh << bwl;
            let quant = &mut self.quant;
            quant[..n].fill(0);
            let br_cdf = tx_ctx.min(TX_32X32);
            for ci in (0..eob).rev() {
                let pos = scan[ci] as usize;
                let mut level = if ci == eob - 1 {
                    let ctx = if ci == 0 {
                        0
                    } else if ci <= n / 8 {
                        1
                    } else if ci <= n / 4 {
                        2
                    } else {
                        3
                    };
                    t.sd.symbol(&mut c.coeff_base_eob[tx_ctx][ptype][ctx]) as u32 + 1
                } else {
                    let ctx = base_ctx(quant, tx, class, bwl, txh, pos);
                    t.sd.symbol(&mut c.coeff_base[tx_ctx][ptype][ctx]) as u32
                };
                if level > 2 {
                    let ctx = br_ctx(quant, class, bwl, txh, pos);
                    for _ in 0..4 {
                        let k = t.sd.symbol(&mut c.coeff_br[br_cdf][ptype][ctx]) as u32;
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
            let (dcq, acq) = self.plane_quant(plane);
            let dq_shift = (1usize << (log2w + log2h) > 256) as u32 + (1usize << (log2w + log2h) > 1024) as u32;
            let lim = 1i64 << (7 + self.bd);
            self.dq[..n].fill(0);
            for ci in 0..eob {
                let pos = scan[ci] as usize;
                let mut level = self.quant[pos];
                if level == 0 {
                    continue;
                }
                let sign = if ci == 0 { t.sd.symbol(&mut t.cdf.dc_sign[ptype][dc_ctx]) == 1 } else { t.sd.literal(1) == 1 };
                if level > 14 {
                    let mut length = 0;
                    loop {
                        length += 1;
                        if t.sd.literal(1) == 1 {
                            break;
                        }
                        if length > 32 {
                            return Err(super::obu::invalid("coefficient Golomb code too long"));
                        }
                    }
                    let mut x = 1u32;
                    for _ in 0..length - 1 {
                        x = (x << 1) | t.sd.literal(1);
                    }
                    level = x + 14;
                }
                if pos == 0 {
                    dc_cat = if sign { 1 } else { 2 };
                }
                level &= 0xFFFFF;
                cul += level;
                let q = if ci == 0 { dcq } else { acq };
                let d = (((level as u64 * q as u64) & 0xFFFFFF) >> dq_shift) as i64;
                self.dq[pos] = (if sign { -d } else { d }).clamp(-lim, lim - 1) as i32;
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
        Ok((eob, tt))
    }

    /// The DC and AC quantisers of the block for a plane.
    fn plane_quant(&self, plane: usize) -> (u32, u32) {
        let h = &self.h;
        let (dd, ad) = match plane {
            0 => (h.delta_q_y_dc, 0),
            1 => (h.delta_q_u_dc, h.delta_q_u_ac),
            _ => (h.delta_q_v_dc, h.delta_q_v_ac),
        };
        let (dc, ac) = match self.bd {
            8 => (&DC_QLOOKUP_8, &AC_QLOOKUP_8),
            10 => (&DC_QLOOKUP_10, &AC_QLOOKUP_10),
            _ => (&DC_QLOOKUP_12, &AC_QLOOKUP_12),
        };
        let q = self.b.qidx;
        (dc[(q + dd).clamp(0, 255) as usize] as u32, ac[(q + ad).clamp(0, 255) as usize] as u32)
    }
}

fn ceil_log2(x: i32) -> u32 {
    if x < 2 {
        0
    } else {
        32 - (x as u32 - 1).leading_zeros()
    }
}

fn neg_deinterleave(diff: i32, r: i32, max: i32) -> i32 {
    if r == 0 {
        return diff;
    }
    if r >= max - 1 {
        return max - diff - 1;
    }
    if 2 * r < max {
        if diff <= 2 * r {
            return if diff & 1 == 1 { r + ((diff + 1) >> 1) } else { r - (diff >> 1) };
        }
        return diff;
    }
    if diff <= 2 * (max - r - 1) {
        return if diff & 1 == 1 { r + ((diff + 1) >> 1) } else { r - (diff >> 1) };
    }
    max - (diff + 1)
}

/// The coefficient context of a non-final coefficient (get_coeff_base_ctx).
#[inline]
fn base_ctx(q: &[u32], tx: usize, class: u8, bwl: usize, txh: usize, pos: usize) -> usize {
    let row = pos >> bwl;
    let col = pos - (row << bwl);
    let mut mag = 0;
    for &(dr, dc) in &SIG_REF_DIFF_OFFSET[class as usize] {
        let (rr, cc) = (row + dr, col + dc);
        if rr < txh && cc < 1 << bwl {
            mag += q[(rr << bwl) + cc].min(3);
        }
    }
    let ctx = ((mag + 1) >> 1).min(4) as usize;
    match class {
        0 => {
            if row == 0 && col == 0 {
                0
            } else {
                ctx + COEFF_BASE_CTX_OFFSET[tx][row.min(4)][col.min(4)] as usize
            }
        }
        2 => ctx + COEFF_BASE_POS_CTX_OFFSET[row.min(2)],
        _ => ctx + COEFF_BASE_POS_CTX_OFFSET[col.min(2)],
    }
}

/// The context of the coefficient's base range symbols (get_br_ctx).
#[inline]
fn br_ctx(q: &[u32], class: u8, bwl: usize, txh: usize, pos: usize) -> usize {
    let row = pos >> bwl;
    let col = pos - (row << bwl);
    let mut mag = 0;
    for &(dr, dc) in &MAG_REF_OFFSET[class as usize] {
        let (rr, cc) = (row + dr, col + dc);
        if rr < txh && cc < 1 << bwl {
            mag += q[(rr << bwl) + cc].min(15);
        }
    }
    let mag = ((mag + 1) >> 1).min(6) as usize;
    if pos == 0 {
        return mag;
    }
    let near = match class {
        0 => row < 2 && col < 2,
        1 => col == 0,
        _ => row == 0,
    };
    mag + if near { 7 } else { 14 }
}

/// get_scan.
fn scan_for(tx: usize, tt: u8) -> &'static [u16] {
    if tx == TX_16X64 {
        return &DEFAULT_16X32;
    }
    if tx == TX_64X16 {
        return &DEFAULT_32X16;
    }
    if TX_SIZE_SQR_UP[tx] == TX_64X64 {
        return &DEFAULT_32X32;
    }
    let kind = if tt == IDTX { 0 } else { tx_class(tt) };
    // kind 0 default, 1 (horizontal class) column scan, 2 (vertical class) row scan
    match (tx, kind) {
        (0, 0) => &DEFAULT_4X4,
        (0, 1) => &MCOL_4X4,
        (0, _) => &MROW_4X4,
        (1, 0) => &DEFAULT_8X8,
        (1, 1) => &MCOL_8X8,
        (1, _) => &MROW_8X8,
        (2, 0) => &DEFAULT_16X16,
        (2, 1) => &MCOL_16X16,
        (2, _) => &MROW_16X16,
        (3, 0) => &DEFAULT_32X32,
        (3, 1) => &MCOL_32X32,
        (3, _) => &MROW_32X32,
        (5, 0) => &DEFAULT_4X8,
        (5, 1) => &MCOL_4X8,
        (5, _) => &MROW_4X8,
        (6, 0) => &DEFAULT_8X4,
        (6, 1) => &MCOL_8X4,
        (6, _) => &MROW_8X4,
        (7, 0) => &DEFAULT_8X16,
        (7, 1) => &MCOL_8X16,
        (7, _) => &MROW_8X16,
        (8, 0) => &DEFAULT_16X8,
        (8, 1) => &MCOL_16X8,
        (8, _) => &MROW_16X8,
        (9, 0) => &DEFAULT_16X32,
        (9, 1) => &MCOL_16X32,
        (9, _) => &MROW_16X32,
        (10, 0) => &DEFAULT_32X16,
        (10, 1) => &MCOL_32X16,
        (10, _) => &MROW_32X16,
        (13, 0) => &DEFAULT_4X16,
        (13, 1) => &MCOL_4X16,
        (13, _) => &MROW_4X16,
        (14, 0) => &DEFAULT_16X4,
        (14, 1) => &MCOL_16X4,
        (14, _) => &MROW_16X4,
        (15, 0) => &DEFAULT_8X32,
        (15, 1) => &MCOL_8X32,
        (15, _) => &MROW_8X32,
        (16, 0) => &DEFAULT_32X8,
        (16, 1) => &MCOL_32X8,
        _ => &MROW_32X8,
    }
}
