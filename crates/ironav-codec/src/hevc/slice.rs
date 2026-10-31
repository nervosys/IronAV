//! Slice segment data: the coding tree syntax (ITU-T H.265 7.3.8) with intra reconstruction.

use super::cabac::{Cabac, MAX_CONTEXTS};
use super::frame::*;
use super::intra;
use super::ps::{Pps, ScalingList, SliceHeader, SliceType, Sps};
use super::tables::*;
use super::transform::{self, ScalingFactors};
use crate::h264::bits::BitReader;
use ironav_core::{Error, Result};
use std::sync::OnceLock;

pub(super) fn bad(msg: &str) -> Error {
    Error::invalid_input(format!("HEVC: {msg}"))
}

pub const MODE_INTRA: u8 = 1;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum PartMode {
    P2Nx2N,
    P2NxN,
    PNx2N,
    PNxN,
    P2NxnU,
    P2NxnD,
    PnLx2N,
    PnRx2N,
}

/// Scan orders: `scan(log2_block, scan_idx)` lists (x, y) positions of a `1 << log2_block` square.
pub struct Scans {
    t: [[Vec<(u8, u8)>; 3]; 4],
}

impl Scans {
    fn build() -> Self {
        let diag = |n: usize| -> Vec<(u8, u8)> {
            match n {
                1 => vec![(0, 0)],
                2 => (0..4).map(|i| (DIAG_SCAN2X2_X[i], DIAG_SCAN2X2_Y[i])).collect(),
                4 => (0..16).map(|i| (DIAG_SCAN4X4_X[i], DIAG_SCAN4X4_Y[i])).collect(),
                _ => (0..64).map(|i| (DIAG_SCAN8X8_X[i], DIAG_SCAN8X8_Y[i])).collect(),
            }
        };
        let horiz = |n: usize| -> Vec<(u8, u8)> { (0..n * n).map(|i| ((i % n) as u8, (i / n) as u8)).collect() };
        let vert = |n: usize| -> Vec<(u8, u8)> { (0..n * n).map(|i| ((i / n) as u8, (i % n) as u8)).collect() };
        let t = std::array::from_fn(|l| {
            let n = 1usize << l;
            [diag(n), horiz(n), vert(n)]
        });
        Scans { t }
    }

    pub fn get() -> &'static Scans {
        static S: OnceLock<Scans> = OnceLock::new();
        S.get_or_init(Scans::build)
    }

    pub fn scan(&self, log2_block: u32, idx: usize) -> &[(u8, u8)] {
        &self.t[log2_block as usize][idx]
    }
}

/// State that outlives one slice segment: dependent segments continue from it.
#[derive(Default)]
pub struct Carry {
    pub end_ctx: Option<[u8; MAX_CONTEXTS]>,
    pub wpp_ctx: Option<[u8; MAX_CONTEXTS]>,
    pub last_qp_y: i32,
}

pub struct SliceCtx<'a> {
    pub sps: &'a Sps,
    pub pps: &'a Pps,
    pub hdr: &'a SliceHeader,
    pub geo: &'a Geometry,
    pub scaling: Option<&'a ScalingFactors>,
    pub slice_idx: u16,
    pub slice_addr_rs: i32,
    pub refs: &'a [Vec<super::RefPic>; 2],
    pub poc: i32,
}

/// The effective scaling list of a picture (7.4.5).
pub fn active_scaling_list<'a>(sps: &'a Sps, pps: &'a Pps) -> Option<ScalingList> {
    if !sps.scaling_list_enabled {
        return None;
    }
    Some(pps.scaling_list.clone().or_else(|| sps.scaling_list.clone()).unwrap_or_else(ScalingList::default_lists))
}

pub struct Dec<'a> {
    pub(super) sps: &'a Sps,
    pub(super) pps: &'a Pps,
    pub(super) hdr: &'a SliceHeader,
    pub(super) geo: &'a Geometry,
    pub(super) scaling: Option<&'a ScalingFactors>,
    pub(super) slice_idx: u16,
    pub(super) slice_addr_rs: i32,
    pub(super) refs: &'a [Vec<super::RefPic>; 2],
    pub(super) poc: i32,
    pub(super) cab: Cabac<'a>,
    pub(super) pic: &'a mut CurPic,
    scans: &'static Scans,
    init: &'static [u8; HEVC_CONTEXTS],
    slice_qp: i32,
    // quantisation group state
    qp_y: i32,
    last_qp_y: i32,
    qp_y_pred: i32,
    is_cu_qp_delta_coded: bool,
    cu_qp_delta_val: i32,
    // current coding unit
    cu_bypass: bool,
    pub(super) cu_pred_mode: u8,
    pub(super) part_mode: PartMode,
    /// The chroma prediction mode of the transform unit being decoded.
    chroma_mode: u32,
    /// The chroma prediction modes of the coding unit: one per prediction block in 4:4:4, else one.
    chroma_modes: [u32; 4],
    max_trafo_depth: u32,
    intra_split: bool,
    ctb_x: usize,
    ctb_y: usize,
    coef: Vec<i32>,
    pub(super) bd_y: u32,
    pub(super) bd_c: u32,
}

const CTX_IDX_MAP_4X4: [u8; 16] = [0, 1, 4, 5, 2, 3, 4, 5, 6, 6, 8, 8, 7, 7, 8, 0];

impl<'a> Dec<'a> {
    pub fn new(ctx: &SliceCtx<'a>, pic: &'a mut CurPic, rbsp: &'a [u8], carry: &Carry) -> Result<Self> {
        let hdr = ctx.hdr;
        let init_type = match hdr.slice_type {
            SliceType::I => 0,
            SliceType::P => {
                if hdr.cabac_init {
                    2
                } else {
                    1
                }
            }
            SliceType::B => {
                if hdr.cabac_init {
                    1
                } else {
                    2
                }
            }
        };
        let slice_qp = ctx.pps.init_qp + hdr.qp_delta;
        let qp_bd = 6 * (ctx.sps.bit_depth_luma as i32 - 8);
        if slice_qp < -qp_bd || slice_qp > 51 {
            return Err(bad("slice QP is out of range"));
        }
        let init: &'static [u8; HEVC_CONTEXTS] = &INIT_VALUES[init_type];
        let mut cab = Cabac::new(rbsp, hdr.data_offset, init, slice_qp);
        let mut last_qp_y = slice_qp;
        if hdr.dependent {
            if let Some(c) = &carry.end_ctx {
                cab.set_contexts(c);
            }
            last_qp_y = carry.last_qp_y;
        }
        Ok(Dec {
            sps: ctx.sps,
            pps: ctx.pps,
            hdr,
            geo: ctx.geo,
            scaling: ctx.scaling,
            slice_idx: ctx.slice_idx,
            slice_addr_rs: ctx.slice_addr_rs,
            refs: ctx.refs,
            poc: ctx.poc,
            cab,
            pic,
            scans: Scans::get(),
            init,
            slice_qp,
            qp_y: slice_qp,
            last_qp_y,
            qp_y_pred: slice_qp,
            is_cu_qp_delta_coded: false,
            cu_qp_delta_val: 0,
            cu_bypass: false,
            cu_pred_mode: MODE_INTRA,
            part_mode: PartMode::P2Nx2N,
            chroma_mode: 1,
            chroma_modes: [1; 4],
            max_trafo_depth: 0,
            intra_split: false,
            ctb_x: 0,
            ctb_y: 0,
            coef: vec![0; 32 * 32],
            bd_y: ctx.sps.bit_depth_luma,
            bd_c: ctx.sps.bit_depth_chroma,
        })
    }

    #[inline]
    pub(super) fn i4(&self, x: usize, y: usize) -> usize {
        (y >> 2) * self.pic.w4 + (x >> 2)
    }

    /// Z-scan order availability of the block at (`xn`, `yn`) for the block at (`xc`, `yc`) (6.4.1).
    pub(super) fn avail(&self, xc: usize, yc: usize, xn: i32, yn: i32) -> bool {
        if xn < 0 || yn < 0 || xn as usize >= self.sps.width || yn as usize >= self.sps.height {
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
        if self.pic.ctb_slice_addr[nrs] != self.slice_addr_rs {
            return false;
        }
        g.tile_id[g.rs_to_ts[nrs]] == g.tile_id[g.rs_to_ts[crs]]
    }

    fn fill4<T: Copy>(v: &mut [T], w4: usize, x: usize, y: usize, w: usize, h: usize, val: T) {
        for yy in (y >> 2)..((y + h) >> 2) {
            let row = yy * w4;
            for e in &mut v[row + (x >> 2)..row + ((x + w) >> 2)] {
                *e = val;
            }
        }
    }

    // ---------------------------------------------------------------- slice data loop

    pub fn run(&mut self, carry: &mut Carry) -> Result<()> {
        let sps = self.sps;
        let pps = self.pps;
        let nctb = sps.ctb_w * sps.ctb_h;
        let mut ts = self.geo.rs_to_ts[self.hdr.segment_address];
        let first_ts = ts;
        loop {
            let rs = self.geo.ts_to_rs[ts];
            self.ctb_x = rs % sps.ctb_w;
            self.ctb_y = rs / sps.ctb_w;
            let tile = self.geo.tile_id[ts];
            let tile_start = ts == 0 || self.geo.tile_id[ts - 1] != tile;
            let row_start_in_tile = pps.entropy_sync && (self.ctb_x == 0 || self.geo.tile_id[self.geo.rs_to_ts[rs - 1]] != tile);
            self.pic.ctb_slice_addr[rs] = self.slice_addr_rs;
            self.pic.ctb_slice_idx[rs] = self.slice_idx;
            if tile_start && (ts != first_ts || self.hdr.dependent) {
                self.cab.init_contexts(self.init, self.slice_qp);
                self.last_qp_y = self.slice_qp;
            } else if row_start_in_tile {
                // wavefront: continue from the contexts after the second CTB of the row above, when it exists
                let (x0, y0) = (self.ctb_x << sps.log2_ctb, self.ctb_y << sps.log2_ctb);
                let ctb = 1i32 << sps.log2_ctb;
                // (9.3.1: this comes before a dependent slice segment's carrying on from the one
                // before it, so a row start with no CTB above right, as in a picture one CTB wide,
                // starts from the initial contexts)
                match carry.wpp_ctx {
                    Some(c) if self.avail(x0, y0, x0 as i32 + ctb, y0 as i32 - ctb) => self.cab.set_contexts(&c),
                    _ => self.cab.init_contexts(self.init, self.slice_qp),
                }
                self.last_qp_y = self.slice_qp;
            }
            if self.hdr.sao_luma || self.hdr.sao_chroma {
                self.parse_sao(rs)?;
            }
            let (x0, y0) = (self.ctb_x << sps.log2_ctb, self.ctb_y << sps.log2_ctb);
            self.coding_quadtree(x0, y0, sps.log2_ctb, 0)?;
            self.pic.ctbs_done += 1;
            let end = self.cab.terminate() == 1;
            if pps.entropy_sync && (self.ctb_x == 1 || (self.ctb_x > 1 && self.geo.tile_id[self.geo.rs_to_ts[rs - 2]] != tile && self.geo.tile_id[self.geo.rs_to_ts[rs - 1]] == tile)) {
                carry.wpp_ctx = Some(self.cab.contexts());
            }
            ts += 1;
            if end {
                break;
            }
            if ts >= nctb {
                return Err(bad("slice data runs past the end of the picture"));
            }
            let nrs = self.geo.ts_to_rs[ts];
            let new_tile = self.geo.tile_id[ts] != tile;
            let new_row = pps.entropy_sync && (nrs.is_multiple_of(sps.ctb_w) || new_tile);
            if (pps.tiles && new_tile) || new_row {
                if self.cab.terminate() != 1 {
                    return Err(bad("missing end_of_subset_one_bit"));
                }
                let next = self.cab.pcm_byte_pos();
                self.cab.restart_at(next);
            }
        }
        carry.end_ctx = Some(self.cab.contexts());
        carry.last_qp_y = self.last_qp_y;
        Ok(())
    }

    // ---------------------------------------------------------------- SAO

    fn parse_sao(&mut self, rs: usize) -> Result<()> {
        let sps = self.sps;
        let (rx, ry) = (self.ctb_x, self.ctb_y);
        let g = self.geo;
        let tile = g.tile_id[g.rs_to_ts[rs]];
        let mut merge_left = false;
        let mut merge_up = false;
        if rx > 0 {
            let left_in_slice = rs as i32 > self.slice_addr_rs && self.pic.ctb_slice_addr[rs - 1] == self.slice_addr_rs;
            let left_in_tile = g.tile_id[g.rs_to_ts[rs - 1]] == tile;
            if left_in_slice && left_in_tile {
                merge_left = self.cab.decision(SAO_MERGE_FLAG_OFFSET) == 1;
            }
        }
        if ry > 0 && !merge_left {
            let up = rs - sps.ctb_w;
            let up_in_slice = up as i32 >= self.slice_addr_rs && self.pic.ctb_slice_addr[up] == self.slice_addr_rs;
            let up_in_tile = g.tile_id[g.rs_to_ts[up]] == tile;
            if up_in_slice && up_in_tile {
                merge_up = self.cab.decision(SAO_MERGE_FLAG_OFFSET) == 1;
            }
        }
        if merge_left {
            self.pic.sao[rs] = self.pic.sao[rs - 1];
            return Ok(());
        }
        if merge_up {
            self.pic.sao[rs] = self.pic.sao[rs - sps.ctb_w];
            return Ok(());
        }
        let mut s = Sao::default();
        let ncomp = if sps.chroma_format_idc != 0 { 3 } else { 1 };
        for c in 0..ncomp {
            if (c == 0 && !self.hdr.sao_luma) || (c > 0 && !self.hdr.sao_chroma) {
                continue;
            }
            if c == 0 || c == 1 {
                s.kind[c] = if self.cab.decision(SAO_TYPE_IDX_OFFSET) == 0 { 0 } else { 1 + self.cab.bypass() as u8 };
            } else {
                s.kind[2] = s.kind[1];
            }
            if s.kind[c] == 0 {
                continue;
            }
            let bd = if c == 0 { self.bd_y } else { self.bd_c };
            let cmax = (1u32 << (bd.min(10) - 5)) - 1;
            let mut abs = [0i16; 4];
            for a in abs.iter_mut() {
                let mut v = 0;
                while v < cmax && self.cab.bypass() == 1 {
                    v += 1;
                }
                *a = v as i16;
            }
            if s.kind[c] == 1 {
                for a in abs.iter_mut() {
                    if *a != 0 && self.cab.bypass() == 1 {
                        *a = -*a;
                    }
                }
                s.band_pos[c] = self.cab.bypass_bits(5) as u8;
            } else {
                if c == 0 || c == 1 {
                    s.eo_class[c] = self.cab.bypass_bits(2) as u8;
                } else {
                    s.eo_class[2] = s.eo_class[1];
                }
                abs[2] = -abs[2];
                abs[3] = -abs[3];
            }
            s.offset[c] = abs;
        }
        self.pic.sao[rs] = s;
        Ok(())
    }

    // ---------------------------------------------------------------- coding quadtree

    fn coding_quadtree(&mut self, x0: usize, y0: usize, log2: u32, depth: u32) -> Result<()> {
        let sps = self.sps;
        let size = 1usize << log2;
        let split = if x0 + size <= sps.width && y0 + size <= sps.height && log2 > sps.log2_min_cb {
            let mut inc = 0;
            if self.avail(x0, y0, x0 as i32 - 1, y0 as i32) && self.pic.ct_depth[self.i4(x0 - 1, y0)] as u32 > depth {
                inc += 1;
            }
            if self.avail(x0, y0, x0 as i32, y0 as i32 - 1) && self.pic.ct_depth[self.i4(x0, y0 - 1)] as u32 > depth {
                inc += 1;
            }
            self.cab.decision(SPLIT_CODING_UNIT_FLAG_OFFSET + inc) == 1
        } else {
            log2 > sps.log2_min_cb
        };
        if self.pps.cu_qp_delta_enabled && log2 >= sps.log2_ctb - self.pps.diff_cu_qp_delta_depth {
            self.is_cu_qp_delta_coded = false;
            self.cu_qp_delta_val = 0;
            self.start_quant_group(x0, y0);
        }
        if split {
            let h = size / 2;
            for (dx, dy) in [(0, 0), (h, 0), (0, h), (h, h)] {
                if x0 + dx < sps.width && y0 + dy < sps.height {
                    self.coding_quadtree(x0 + dx, y0 + dy, log2 - 1, depth + 1)?;
                }
            }
            Ok(())
        } else {
            self.coding_unit(x0, y0, log2, depth)
        }
    }

    /// QP prediction for a new quantisation group (8.6.1).
    fn start_quant_group(&mut self, xq: usize, yq: usize) {
        let prev = self.last_qp_y;
        let l = self.sps.log2_ctb;
        let same_ctb = |x: usize, y: usize| (x >> l) == (xq >> l) && (y >> l) == (yq >> l);
        let a = if xq > 0 && self.avail(xq, yq, xq as i32 - 1, yq as i32) && same_ctb(xq - 1, yq) { self.pic.qp[self.i4(xq - 1, yq)] as i32 } else { prev };
        let b = if yq > 0 && self.avail(xq, yq, xq as i32, yq as i32 - 1) && same_ctb(xq, yq - 1) { self.pic.qp[self.i4(xq, yq - 1)] as i32 } else { prev };
        self.qp_y_pred = (a + b + 1) >> 1;
        self.qp_y = self.qp_y_pred;
    }

    fn update_qp(&mut self) {
        let off = 6 * (self.bd_y as i32 - 8);
        self.qp_y = ((self.qp_y_pred + self.cu_qp_delta_val + 52 + 2 * off) % (52 + off)) - off;
    }

    // ---------------------------------------------------------------- coding unit

    fn coding_unit(&mut self, x0: usize, y0: usize, log2: u32, depth: u32) -> Result<()> {
        let sps = self.sps;
        let size = 1usize << log2;
        let w4 = self.pic.w4;
        if !self.pps.cu_qp_delta_enabled {
            self.qp_y = self.slice_qp;
            self.qp_y_pred = self.slice_qp;
        }
        self.cu_bypass = self.pps.transquant_bypass && self.cab.decision(CU_TRANSQUANT_BYPASS_FLAG_OFFSET) == 1;
        let mut skip = false;
        if self.hdr.slice_type != SliceType::I {
            let mut inc = 0;
            if self.avail(x0, y0, x0 as i32 - 1, y0 as i32) && self.pic.skip[self.i4(x0 - 1, y0)] != 0 {
                inc += 1;
            }
            if self.avail(x0, y0, x0 as i32, y0 as i32 - 1) && self.pic.skip[self.i4(x0, y0 - 1)] != 0 {
                inc += 1;
            }
            skip = self.cab.decision(SKIP_FLAG_OFFSET + inc) == 1;
        }
        Self::fill4(&mut self.pic.skip, w4, x0, y0, size, size, skip as u8);
        Self::fill4(&mut self.pic.ct_depth, w4, x0, y0, size, size, depth as u8);
        Self::fill4(&mut self.pic.nofilter, w4, x0, y0, size, size, self.cu_bypass as u8);
        self.mark_edges(x0, y0, size, size, EDGE_TU_V | EDGE_TU_H);
        self.part_mode = PartMode::P2Nx2N;
        self.intra_split = false;
        let mut pcm = false;
        let mut merge_2nx2n = false;
        let mut rqt_root_cbf = true;
        if skip {
            self.cu_pred_mode = PRED_INTER;
            Self::fill4(&mut self.pic.pred_mode, w4, x0, y0, size, size, PRED_INTER);
            self.prediction_unit(x0, y0, size, size, x0, y0, size, 0, true)?;
            rqt_root_cbf = false;
        } else {
            self.cu_pred_mode = if self.hdr.slice_type != SliceType::I && self.cab.decision(PRED_MODE_FLAG_OFFSET) == 0 { PRED_INTER } else { PRED_INTRA };
            Self::fill4(&mut self.pic.pred_mode, w4, x0, y0, size, size, self.cu_pred_mode);
            if self.cu_pred_mode != PRED_INTRA || log2 == sps.log2_min_cb {
                self.part_mode = self.parse_part_mode(log2)?;
            }
            if self.cu_pred_mode == PRED_INTRA {
                self.intra_split = self.part_mode == PartMode::PNxN;
                if self.part_mode == PartMode::P2Nx2N && sps.pcm && log2 >= sps.log2_min_pcm_cb && log2 <= sps.log2_max_pcm_cb {
                    pcm = self.cab.terminate() == 1;
                }
                if pcm {
                    self.pcm_samples(x0, y0, log2)?;
                } else {
                    self.parse_intra_modes(x0, y0, size)?;
                }
            } else {
                merge_2nx2n = self.parse_inter_pus(x0, y0, size)?;
            }
            if !pcm && self.cu_pred_mode != PRED_INTRA && !(self.part_mode == PartMode::P2Nx2N && merge_2nx2n) {
                rqt_root_cbf = self.cab.decision(NO_RESIDUAL_DATA_FLAG_OFFSET) == 1;
            }
        }
        if pcm {
            Self::fill4(&mut self.pic.ipm, w4, x0, y0, size, size, 1);
            if sps.pcm_loop_filter_disabled {
                Self::fill4(&mut self.pic.nofilter, w4, x0, y0, size, size, 1);
            }
        } else if rqt_root_cbf {
            self.max_trafo_depth = if self.cu_pred_mode == PRED_INTRA { sps.max_th_depth_intra + self.intra_split as u32 } else { sps.max_th_depth_inter };
            self.transform_tree(x0, y0, x0, y0, log2, 0, 0, [true, true], [false, false])?;
        }
        self.update_qp();
        Self::fill4(&mut self.pic.qp, w4, x0, y0, size, size, self.qp_y as i8);
        self.last_qp_y = self.qp_y;
        Ok(())
    }

    pub(super) fn mark_edges(&mut self, x0: usize, y0: usize, w: usize, h: usize, flags: u8) {
        let w4 = self.pic.w4;
        for yy in (y0 >> 2)..((y0 + h) >> 2) {
            if flags & EDGE_TU_V != 0 {
                self.pic.edges[yy * w4 + (x0 >> 2)] |= EDGE_TU_V;
            }
            if flags & EDGE_PU_V != 0 {
                self.pic.edges[yy * w4 + (x0 >> 2)] |= EDGE_PU_V;
            }
        }
        for xx in (x0 >> 2)..((x0 + w) >> 2) {
            if flags & EDGE_TU_H != 0 {
                self.pic.edges[(y0 >> 2) * w4 + xx] |= EDGE_TU_H;
            }
            if flags & EDGE_PU_H != 0 {
                self.pic.edges[(y0 >> 2) * w4 + xx] |= EDGE_PU_H;
            }
        }
    }

    fn parse_part_mode(&mut self, log2: u32) -> Result<PartMode> {
        use PartMode::*;
        if self.cab.decision(PART_MODE_OFFSET) == 1 {
            return Ok(P2Nx2N);
        }
        if self.cu_pred_mode == PRED_INTRA {
            return Ok(PNxN);
        }
        let sps = self.sps;
        if log2 == sps.log2_min_cb {
            if self.cab.decision(PART_MODE_OFFSET + 1) == 1 {
                return Ok(P2NxN);
            }
            if log2 == 3 {
                return Ok(PNx2N);
            }
            return Ok(if self.cab.decision(PART_MODE_OFFSET + 2) == 1 { PNx2N } else { PNxN });
        }
        if !sps.amp {
            return Ok(if self.cab.decision(PART_MODE_OFFSET + 1) == 1 { P2NxN } else { PNx2N });
        }
        let horizontal = self.cab.decision(PART_MODE_OFFSET + 1) == 1;
        if self.cab.decision(PART_MODE_OFFSET + 3) == 1 {
            return Ok(if horizontal { P2NxN } else { PNx2N });
        }
        let second = self.cab.bypass() == 1;
        Ok(match (horizontal, second) {
            (true, false) => P2NxnU,
            (true, true) => P2NxnD,
            (false, false) => PnLx2N,
            (false, true) => PnRx2N,
        })
    }

    // ---------------------------------------------------------------- intra modes and PCM

    fn parse_intra_modes(&mut self, x0: usize, y0: usize, size: usize) -> Result<()> {
        let nparts = if self.part_mode == PartMode::PNxN { 4 } else { 1 };
        let pb = if nparts == 4 { size / 2 } else { size };
        let mut prev_flag = [false; 4];
        for f in prev_flag.iter_mut().take(nparts) {
            *f = self.cab.decision(PREV_INTRA_LUMA_PRED_FLAG_OFFSET) == 1;
        }
        let w4 = self.pic.w4;
        let mut modes = [0u32; 4];
        for p in 0..nparts {
            let (xp, yp) = (x0 + (p & 1) * pb, y0 + (p >> 1) * pb);
            let (mpm_idx, rem) = if prev_flag[p] {
                let idx = if self.cab.bypass() == 0 { 0 } else { 1 + self.cab.bypass() };
                (idx, 0)
            } else {
                (0, self.cab.bypass_bits(5))
            };
            // candidate modes from the left and above neighbours
            let cand = |s: &Self, xn: i32, yn: i32, above: bool| -> u32 {
                if !s.avail(xp, yp, xn, yn) {
                    return 1;
                }
                let i = s.i4(xn as usize, yn as usize);
                if s.pic.pred_mode[i] != PRED_INTRA {
                    return 1;
                }
                if above && (yn as usize) < ((yp >> s.sps.log2_ctb) << s.sps.log2_ctb) {
                    return 1;
                }
                s.pic.ipm[i] as u32
            };
            let a = cand(self, xp as i32 - 1, yp as i32, false);
            let b = cand(self, xp as i32, yp as i32 - 1, true);
            let mut list = if a == b {
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
            let mode = if prev_flag[p] {
                list[mpm_idx as usize]
            } else {
                list.sort_unstable();
                let mut m = rem;
                for l in list {
                    if m >= l {
                        m += 1;
                    }
                }
                m
            };
            modes[p] = mode;
            Self::fill4(&mut self.pic.ipm, w4, xp, yp, pb, pb, mode as u8);
        }
        // intra_chroma_pred_mode: one per prediction block in 4:4:4, one per coding unit otherwise
        let chroma = self.sps.chroma_format_idc;
        for p in 0..if chroma == 3 { nparts } else { (chroma != 0) as usize } {
            let idc = if self.cab.decision(INTRA_CHROMA_PRED_MODE_OFFSET) == 0 { 4 } else { self.cab.bypass_bits(2) };
            let mode = if idc == 4 {
                modes[p]
            } else {
                let m = [0, 26, 10, 1][idc as usize];
                if m == modes[p] {
                    34
                } else {
                    m
                }
            };
            // 4:2:2 chroma is twice as tall as it is wide, so its angles are remapped (Table 8-3)
            self.chroma_modes[p] = if chroma == 2 { MODE_422[mode as usize] as u32 } else { mode };
        }
        Ok(())
    }

    fn pcm_samples(&mut self, x0: usize, y0: usize, log2: u32) -> Result<()> {
        let sps = self.sps;
        let start = self.cab.pcm_byte_pos();
        let data = self.cab.data();
        if start > data.len() {
            return Err(bad("PCM data is past the end of the slice"));
        }
        let mut r = BitReader::new(&data[start..]);
        let n = 1usize << log2;
        let (bl, bc) = (sps.pcm_bit_depth_luma, sps.pcm_bit_depth_chroma);
        let fr = &mut self.pic.frame;
        for y in 0..n {
            for x in 0..n {
                let v = r.bits(bl)? << (sps.bit_depth_luma - bl);
                fr.planes[0][(y0 + y) * fr.width + x0 + x] = v as u16;
            }
        }
        if sps.chroma_format_idc != 0 {
            let (sx, sy) = sps.chroma_shift();
            let (cw, ch) = (n >> sx, n >> sy);
            for c in 1..3 {
                for y in 0..ch {
                    for x in 0..cw {
                        let v = r.bits(bc)? << (sps.bit_depth_chroma - bc);
                        fr.planes[c][((y0 >> sy) + y) * fr.cwidth + (x0 >> sx) + x] = v as u16;
                    }
                }
            }
        }
        let used = r.pos().div_ceil(8);
        self.cab.restart_at(start + used);
        Ok(())
    }

    // ---------------------------------------------------------------- transform tree

    #[allow(clippy::too_many_arguments)]
    fn transform_tree(&mut self, x0: usize, y0: usize, xb: usize, yb: usize, log2: u32, depth: u32, blk: usize, parent_cb: [bool; 2], parent_cr: [bool; 2]) -> Result<()> {
        let sps = self.sps;
        let chroma = sps.chroma_format_idc;
        if depth == 0 {
            self.chroma_mode = self.chroma_modes[0];
        } else if depth == 1 && self.intra_split && chroma == 3 {
            self.chroma_mode = self.chroma_modes[blk];
        }
        let split = if log2 <= sps.log2_max_tb && log2 > sps.log2_min_tb && depth < self.max_trafo_depth && !(self.intra_split && depth == 0) {
            self.cab.decision(SPLIT_TRANSFORM_FLAG_OFFSET + (5 - log2 as usize)) == 1
        } else {
            let inter_split = sps.max_th_depth_inter == 0 && self.cu_pred_mode == PRED_INTER && self.part_mode != PartMode::P2Nx2N && depth == 0;
            log2 > sps.log2_max_tb || (self.intra_split && depth == 0) || inter_split
        };
        let mut cbf_cb = [false; 2];
        let mut cbf_cr = [false; 2];
        if (log2 > 2 && chroma != 0) || chroma == 3 {
            // 4:2:2 chroma blocks are two squares, one above the other, each with its own flag
            let two = chroma == 2 && (!split || log2 == 3);
            for (cbf, parent) in [(&mut cbf_cb, parent_cb), (&mut cbf_cr, parent_cr)] {
                if depth == 0 || parent[0] {
                    cbf[0] = self.cab.decision(CBF_CB_CR_OFFSET + depth as usize) == 1;
                    if two {
                        cbf[1] = self.cab.decision(CBF_CB_CR_OFFSET + depth as usize) == 1;
                    }
                }
            }
        } else if chroma != 0 && log2 == 2 && depth > 0 {
            cbf_cb = parent_cb;
            cbf_cr = parent_cr;
        }
        if split {
            let h = 1usize << (log2 - 1);
            for (i, (dx, dy)) in [(0, 0), (h, 0), (0, h), (h, h)].into_iter().enumerate() {
                self.transform_tree(x0 + dx, y0 + dy, x0, y0, log2 - 1, depth + 1, i, cbf_cb, cbf_cr)?;
            }
            Ok(())
        } else {
            let any_chroma = cbf_cb[0] || cbf_cr[0] || cbf_cb[1] || cbf_cr[1];
            let cbf_luma = if self.cu_pred_mode == PRED_INTRA || depth != 0 || any_chroma { self.cab.decision(CBF_LUMA_OFFSET + if depth == 0 { 1 } else { 0 }) == 1 } else { true };
            self.transform_unit(x0, y0, xb, yb, log2, blk, cbf_luma, cbf_cb, cbf_cr)
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn transform_unit(&mut self, x0: usize, y0: usize, xb: usize, yb: usize, log2: u32, blk: usize, cbf_luma: bool, cbf_cb: [bool; 2], cbf_cr: [bool; 2]) -> Result<()> {
        let sps = self.sps;
        let n = 1usize << log2;
        let intra = self.cu_pred_mode == PRED_INTRA;
        let chroma = sps.chroma_format_idc != 0;
        self.mark_edges(x0, y0, n, n, EDGE_TU_V | EDGE_TU_H);
        let w4 = self.pic.w4;
        if cbf_luma {
            Self::fill4(&mut self.pic.nz, w4, x0, y0, n, n, 1);
        }
        let cbf_chroma = chroma && (cbf_cb[0] || cbf_cr[0] || cbf_cb[1] || cbf_cr[1]);
        if (cbf_luma || cbf_chroma) && self.pps.cu_qp_delta_enabled && !self.is_cu_qp_delta_coded {
            let mut v = 0;
            while v < 5 && self.cab.decision(CU_QP_DELTA_OFFSET + if v == 0 { 0 } else { 1 }) == 1 {
                v += 1;
            }
            if v == 5 {
                let mut k = 0;
                while self.cab.bypass() == 1 {
                    v += 1 << k;
                    k += 1;
                    if k > 30 {
                        return Err(bad("cu_qp_delta_abs is too long"));
                    }
                }
                if k > 0 {
                    v += self.cab.bypass_bits(k) as i32;
                }
            }
            if v != 0 && self.cab.bypass() == 1 {
                v = -v;
            }
            let off = 6 * (self.bd_y as i32 - 8);
            if v < -(26 + off / 2) || v > 25 + off / 2 {
                return Err(bad("CuQpDeltaVal is out of range"));
            }
            self.is_cu_qp_delta_coded = true;
            self.cu_qp_delta_val = v;
            self.update_qp();
        }
        // luma
        if intra {
            let mode = self.pic.ipm[self.i4(x0, y0)] as u32;
            self.intra_predict(0, x0, y0, log2, mode);
        }
        if cbf_luma {
            self.residual_block(0, x0, y0, log2, intra, if intra { self.pic.ipm[self.i4(x0, y0)] as u32 } else { 0 })?;
        }
        // chroma: with the luma block, or (4x4 luma outside 4:4:4) once for the four of them, after the last;
        // 4:2:2 has two square blocks, the second below the first
        let (sx, sy) = sps.chroma_shift();
        let at = if log2 > 2 || sps.chroma_format_idc == 3 {
            Some((x0 >> sx, y0 >> sy, log2 - sx as u32))
        } else if blk == 3 {
            Some((xb >> sx, yb >> sy, 2))
        } else {
            None
        };
        if let (true, Some((xc, yc, l))) = (chroma, at) {
            let blocks = if sps.chroma_format_idc == 2 { 2 } else { 1 };
            for (c, cbf) in [(1usize, cbf_cb), (2, cbf_cr)] {
                for (i, &coded) in cbf.iter().enumerate().take(blocks) {
                    let y = yc + (i << l);
                    if intra {
                        self.intra_predict(c, xc, y, l, self.chroma_mode);
                    }
                    if coded {
                        self.residual_block(c, xc, y, l, intra, self.chroma_mode)?;
                    }
                }
            }
        }
        Ok(())
    }

    // ---------------------------------------------------------------- intra prediction

    /// Predicts the `1 << log2` square at (`x`, `y`) of component `c` into the picture.
    fn intra_predict(&mut self, c: usize, x: usize, y: usize, log2: u32, mode: u32) {
        let n = 1usize << log2;
        let (sx, sy) = if c == 0 { (0, 0) } else { self.sps.chroma_shift() };
        let bd = if c == 0 { self.bd_y } else { self.bd_c };
        let (xl, yl) = (x << sx, y << sy);
        let (ux, uy) = (4usize >> sx, 4usize >> sy);
        let mut line = [0u16; 129];
        let mut av = [false; 129];
        let constrained = self.pps.constrained_intra_pred;
        let ok = |s: &Self, xn: i32, yn: i32| -> bool {
            if !s.avail(xl, yl, xn, yn) {
                return false;
            }
            !constrained || s.pic.pred_mode[s.i4(xn as usize, yn as usize)] == PRED_INTRA
        };
        let (stride, plane) = if c == 0 { (self.pic.frame.width, 0) } else { (self.pic.frame.cwidth, c) };
        // left column, bottom to top
        let mut r = 0;
        while r < 2 * n {
            let a = ok(self, (x as i32 - 1) << sx, ((y + r) << sy) as i32);
            if a {
                for k in r..(r + uy).min(2 * n) {
                    line[2 * n - 1 - k] = self.pic.frame.planes[plane][(y + k) * stride + x - 1];
                    av[2 * n - 1 - k] = true;
                }
            }
            r += uy;
        }
        if ok(self, (x as i32 - 1) << sx, (y as i32 - 1) << sy) {
            line[2 * n] = self.pic.frame.planes[plane][(y - 1) * stride + x - 1];
            av[2 * n] = true;
        }
        let mut i = 0;
        while i < 2 * n {
            if ok(self, ((x + i) << sx) as i32, (y as i32 - 1) << sy) {
                for k in i..(i + ux).min(2 * n) {
                    line[2 * n + 1 + k] = self.pic.frame.planes[plane][(y - 1) * stride + x + k];
                    av[2 * n + 1 + k] = true;
                }
            }
            i += ux;
        }
        let len = 4 * n + 1;
        intra::substitute(&mut line[..len], &av[..len], bd);
        let mut out = [0u16; 32 * 32];
        let filter = c == 0 || self.sps.chroma_format_idc == 3;
        intra::predict(&line[..len], log2, mode, filter, c == 0, c == 0 && self.sps.strong_intra_smoothing, bd, &mut out);
        let p = &mut self.pic.frame.planes[plane];
        for yy in 0..n {
            p[(y + yy) * stride + x..(y + yy) * stride + x + n].copy_from_slice(&out[yy * n..yy * n + n]);
        }
    }

    // ---------------------------------------------------------------- residual

    /// Parses a residual block and adds its reconstruction to the picture.
    fn residual_block(&mut self, c: usize, x: usize, y: usize, log2: u32, intra: bool, pred_mode: u32) -> Result<()> {
        let n = 1usize << log2;
        let scan_idx = if intra && (log2 == 2 || (log2 == 3 && (c == 0 || self.sps.chroma_format_idc == 3))) {
            match pred_mode {
                6..=14 => 2,
                22..=30 => 1,
                _ => 0,
            }
        } else {
            0
        };
        let (skip, max_x, max_y) = self.residual_coding(log2, c, scan_idx)?;
        let bd = if c == 0 { self.bd_y } else { self.bd_c };
        if !self.cu_bypass {
            let qp = self.block_qp(c);
            let m: Option<&[u8]> = match self.scaling {
                Some(s) if !(skip && log2 > 2) => Some(s.get(log2, if intra { c } else { 3 + c })),
                _ => None,
            };
            transform::dequantise(&mut self.coef, log2, qp, bd, m, max_x, max_y);
            if skip {
                transform::skip(&mut self.coef, log2, bd);
            } else {
                let dst = intra && c == 0 && log2 == 2;
                transform::inverse(&mut self.coef, log2, dst, bd, max_x, max_y);
            }
        }
        let (stride, plane) = if c == 0 { (self.pic.frame.width, 0) } else { (self.pic.frame.cwidth, c) };
        let maxv = (1i32 << bd) - 1;
        let p = &mut self.pic.frame.planes[plane];
        for yy in 0..n {
            for xx in 0..n {
                let v = p[(y + yy) * stride + x + xx] as i32 + self.coef[yy * n + xx];
                p[(y + yy) * stride + x + xx] = v.clamp(0, maxv) as u16;
            }
        }
        Ok(())
    }

    /// The quantisation parameter qP of a component (8.6.1), including the bit depth offset.
    fn block_qp(&self, c: usize) -> i32 {
        if c == 0 {
            return self.qp_y + 6 * (self.bd_y as i32 - 8);
        }
        let off_c = 6 * (self.bd_c as i32 - 8);
        let o = if c == 1 { self.pps.cb_qp_offset + self.hdr.cb_qp_offset } else { self.pps.cr_qp_offset + self.hdr.cr_qp_offset };
        let qpi = (self.qp_y + o).clamp(-off_c, 57);
        let qpc = if self.sps.chroma_format_idc == 1 {
            if qpi < 30 {
                qpi
            } else if qpi >= 44 {
                qpi - 6
            } else {
                QP_C_TABLE[(qpi - 30) as usize] as i32
            }
        } else {
            qpi.min(51)
        };
        qpc + off_c
    }

    /// residual_coding() (7.3.8.11): fills `self.coef` with the coefficient levels of an `n` x `n`
    /// block and returns (transform_skip, max_x, max_y).
    fn residual_coding(&mut self, log2: u32, c: usize, scan_idx: usize) -> Result<(bool, usize, usize)> {
        let n = 1usize << log2;
        self.coef[..n * n].fill(0);
        let luma = c == 0;
        let skip = self.pps.transform_skip && !self.cu_bypass && log2 <= 2 && self.cab.decision(TRANSFORM_SKIP_FLAG_OFFSET + if luma { 0 } else { 1 }) == 1;
        let (ctx_off, ctx_shift) = if luma { (3 * (log2 as usize - 2) + ((log2 as usize - 1) >> 2), (log2 as usize + 1) >> 2) } else { (15, log2 as usize - 2) };
        let cmax = (log2 as usize) * 2 - 1;
        let mut px = 0;
        while px < cmax && self.cab.decision(LAST_SIGNIFICANT_COEFF_X_PREFIX_OFFSET + ctx_off + (px >> ctx_shift)) == 1 {
            px += 1;
        }
        let mut py = 0;
        while py < cmax && self.cab.decision(LAST_SIGNIFICANT_COEFF_Y_PREFIX_OFFSET + ctx_off + (py >> ctx_shift)) == 1 {
            py += 1;
        }
        let suffix = |s: &mut Self, p: usize| -> usize {
            if p > 3 {
                let nb = (p >> 1) as u32 - 1;
                (1usize << nb) * (2 + (p & 1)) + s.cab.bypass_bits(nb) as usize
            } else {
                p
            }
        };
        let mut last_x = suffix(self, px);
        let mut last_y = suffix(self, py);
        if scan_idx == 2 {
            std::mem::swap(&mut last_x, &mut last_y);
        }
        if last_x >= n || last_y >= n {
            return Err(bad("last significant coefficient is outside the block"));
        }
        let sb_log2 = log2 - 2;
        let nsbw = 1usize << sb_log2;
        let sb_scan = self.scans.scan(sb_log2, scan_idx);
        let pos_scan = self.scans.scan(2, scan_idx);
        let last_sb = sb_scan.iter().position(|&p| p == ((last_x >> 2) as u8, (last_y >> 2) as u8)).ok_or_else(|| bad("bad last sub-block"))?;
        let last_pos = pos_scan.iter().position(|&p| p == ((last_x & 3) as u8, (last_y & 3) as u8)).ok_or_else(|| bad("bad last position"))?;
        let mut csbf = [[false; 8]; 8];
        let mut greater1_ctx = 1usize;
        let (mut max_x, mut max_y) = (0usize, 0usize);
        let sign_hiding = self.pps.sign_data_hiding && !self.cu_bypass;
        for i in (0..=last_sb).rev() {
            let (xs, ys) = (sb_scan[i].0 as usize, sb_scan[i].1 as usize);
            let mut infer_dc = false;
            let right = xs + 1 < nsbw && csbf[ys][xs + 1];
            let below = ys + 1 < nsbw && csbf[ys + 1][xs];
            let coded = if i < last_sb && i > 0 {
                let inc = (right || below) as usize + if luma { 0 } else { 2 };
                infer_dc = true;
                self.cab.decision(SIGNIFICANT_COEFF_GROUP_FLAG_OFFSET + inc) == 1
            } else {
                true
            };
            csbf[ys][xs] = coded;
            if !coded {
                continue;
            }
            let prev_csbf = right as usize | ((below as usize) << 1);
            // significant positions in decreasing scan order
            let mut sig = [0u8; 16];
            let mut nsig = 0;
            let start = if i == last_sb {
                sig[0] = last_pos as u8;
                nsig = 1;
                last_pos as i32 - 1
            } else {
                15
            };
            for pos in (0..=start).rev() {
                let (xp, yp) = (pos_scan[pos as usize].0 as usize, pos_scan[pos as usize].1 as usize);
                let is_sig = if pos > 0 || !infer_dc {
                    let sig_ctx = if log2 == 2 {
                        CTX_IDX_MAP_4X4[(yp << 2) + xp] as usize
                    } else if xs == 0 && ys == 0 && pos == 0 {
                        0
                    } else {
                        let mut s = match prev_csbf {
                            0 => {
                                if xp + yp == 0 {
                                    2
                                } else if xp + yp < 3 {
                                    1
                                } else {
                                    0
                                }
                            }
                            1 => {
                                if yp == 0 {
                                    2
                                } else if yp == 1 {
                                    1
                                } else {
                                    0
                                }
                            }
                            2 => {
                                if xp == 0 {
                                    2
                                } else if xp == 1 {
                                    1
                                } else {
                                    0
                                }
                            }
                            _ => 2,
                        };
                        if luma {
                            if xs > 0 || ys > 0 {
                                s += 3;
                            }
                            s += if log2 == 3 {
                                if scan_idx == 0 {
                                    9
                                } else {
                                    15
                                }
                            } else {
                                21
                            };
                        } else {
                            s += if log2 == 3 { 9 } else { 12 };
                        }
                        s
                    };
                    let inc = if luma { sig_ctx } else { 27 + sig_ctx };
                    let v = self.cab.decision(SIGNIFICANT_COEFF_FLAG_OFFSET + inc) == 1;
                    if v {
                        infer_dc = false;
                    }
                    v
                } else {
                    // position 0 with no other significant coefficient in a coded sub-block
                    true
                };
                if is_sig {
                    sig[nsig] = pos as u8;
                    nsig += 1;
                }
            }
            if nsig == 0 {
                continue;
            }
            // greater-than-1 flags for the first eight
            let mut ctx_set = if i > 0 && luma { 2 } else { 0 };
            if i != last_sb && greater1_ctx == 0 {
                ctx_set += 1;
            }
            greater1_ctx = 1;
            let mut g1 = [false; 16];
            let mut first_g1 = usize::MAX;
            for k in 0..nsig.min(8) {
                let inc = ctx_set * 4 + greater1_ctx + if luma { 0 } else { 16 };
                let f = self.cab.decision(COEFF_ABS_LEVEL_GREATER1_FLAG_OFFSET + inc) == 1;
                g1[k] = f;
                if f {
                    greater1_ctx = 0;
                    if first_g1 == usize::MAX {
                        first_g1 = k;
                    }
                } else if greater1_ctx > 0 && greater1_ctx < 3 {
                    greater1_ctx += 1;
                }
            }
            let mut g2 = false;
            if first_g1 != usize::MAX {
                g2 = self.cab.decision(COEFF_ABS_LEVEL_GREATER2_FLAG_OFFSET + ctx_set + if luma { 0 } else { 4 }) == 1;
            }
            let hidden = sign_hiding && (sig[0] as i32 - sig[nsig - 1] as i32) > 3;
            let nsigns = nsig - hidden as usize;
            let signs = self.cab.bypass_bits(nsigns as u32) << (32 - nsigns as u32).min(31);
            let signs = if nsigns == 0 { 0 } else { signs };
            let mut rice = 0u32;
            let mut sum_abs = 0i32;
            for k in 0..nsig {
                let mut base = 1 + g1[k] as i32;
                if k == first_g1 {
                    base += g2 as i32;
                }
                let thresh = if k < 8 {
                    if k == first_g1 {
                        3
                    } else {
                        2
                    }
                } else {
                    1
                };
                let mut abs = base;
                if base == thresh {
                    let mut prefix = 0u32;
                    while prefix < 32 && self.cab.bypass() == 1 {
                        prefix += 1;
                    }
                    let rem = if prefix < 4 {
                        ((prefix << rice) + self.cab.bypass_bits(rice)) as i64
                    } else {
                        let pm3 = prefix - 3;
                        if pm3 + rice > 32 {
                            return Err(bad("coefficient level is too large"));
                        }
                        ((((1u64 << pm3) + 3 - 1) << rice) + self.cab.bypass_bits(pm3 + rice) as u64) as i64
                    };
                    abs = (base as i64 + rem).min(1 << 20) as i32;
                    if abs > 3 * (1 << rice) {
                        rice = (rice + 1).min(4);
                    }
                }
                sum_abs += abs;
                let neg = if k < nsigns { (signs >> (31 - k)) & 1 == 1 } else { sum_abs & 1 == 1 };
                let pos = sig[k] as usize;
                let (xc, yc) = ((xs << 2) + pos_scan[pos].0 as usize, (ys << 2) + pos_scan[pos].1 as usize);
                let v = if neg { -abs } else { abs };
                self.coef[yc * n + xc] = v.clamp(-32768, 32767);
                max_x = max_x.max(xc);
                max_y = max_y.max(yc);
            }
        }
        Ok((skip, max_x, max_y))
    }
}
