//! Decoded pictures and the per-picture side information the decoder keeps while a picture is
//! being reconstructed.

use super::ps::{Pps, Sps};

/// Motion of one 4x4 luma block. `pred` bit 0 / bit 1 say that list 0 / list 1 is used, bit 2 / bit 3
/// that the list 0 / list 1 reference is a long-term picture.
#[derive(Clone, Copy, Default, Debug, PartialEq, Eq)]
pub struct MvField {
    pub mv: [[i16; 2]; 2],
    pub ref_idx: [i8; 2],
    pub ref_poc: [i32; 2],
    pub pred: u8,
}

impl MvField {
    pub fn uses(&self, l: usize) -> bool {
        self.pred & (1 << l) != 0
    }

    pub fn long_term(&self, l: usize) -> bool {
        self.pred & (4 << l) != 0
    }
}

/// A reconstructed picture, kept as a reference and for output.
pub struct Frame {
    pub planes: [Vec<u16>; 3],
    pub width: usize,
    pub height: usize,
    /// Chroma plane size (zero for monochrome).
    pub cwidth: usize,
    pub cheight: usize,
    pub poc: i32,
    /// Motion on the 4x4 grid, for temporal motion vector prediction.
    pub mv: Vec<MvField>,
    pub w4: usize,
    pub pts: i64,
    /// Conformance window: left, right, top, bottom (luma samples).
    pub crop: [usize; 4],
    pub bit_depth: u32,
    pub chroma_format: u32,
}

/// Sample adaptive offset parameters of one CTB, per colour component.
#[derive(Clone, Copy, Default, Debug)]
pub struct Sao {
    /// 0 off, 1 band offset, 2 edge offset.
    pub kind: [u8; 3],
    pub band_pos: [u8; 3],
    pub eo_class: [u8; 3],
    pub offset: [[i16; 4]; 3],
}

/// Per-slice values the loop filters need.
#[derive(Clone, Debug)]
pub struct SliceInfo {
    pub deblocking_disabled: bool,
    pub beta_offset_div2: i32,
    pub tc_offset_div2: i32,
    pub loop_filter_across_slices: bool,
    pub cb_qp_offset: i32,
    pub cr_qp_offset: i32,
}

pub const PRED_NONE: u8 = 0;
pub const PRED_INTRA: u8 = 1;
pub const PRED_INTER: u8 = 2;

/// Edge flags on the 4x4 grid (bit set at the block to the right of / below the edge).
pub const EDGE_TU_V: u8 = 1;
pub const EDGE_TU_H: u8 = 2;
pub const EDGE_PU_V: u8 = 4;
pub const EDGE_PU_H: u8 = 8;

/// The picture being decoded.
pub struct CurPic {
    pub frame: Frame,
    pub w4: usize,
    pub h4: usize,
    pub pred_mode: Vec<u8>,
    pub skip: Vec<u8>,
    pub ct_depth: Vec<u8>,
    pub ipm: Vec<u8>,
    pub qp: Vec<i8>,
    /// Luma transform block has coded coefficients.
    pub nz: Vec<u8>,
    pub edges: Vec<u8>,
    /// Samples the loop filters must leave alone (PCM with loop filter disabled, transquant bypass).
    pub nofilter: Vec<u8>,
    pub ctb_slice_addr: Vec<i32>,
    pub ctb_slice_idx: Vec<u16>,
    pub slices: Vec<SliceInfo>,
    pub sao: Vec<Sao>,
    pub ctbs_done: usize,
}

impl CurPic {
    pub fn new(sps: &Sps, pts: i64, poc: i32) -> Self {
        let (w, h) = (sps.width, sps.height);
        let (cw, ch) = match sps.chroma_format_idc {
            0 => (0, 0),
            1 => (w / 2, h / 2),
            2 => (w / 2, h),
            _ => (w, h),
        };
        let (w4, h4) = (w.div_ceil(4), h.div_ceil(4));
        let n4 = w4 * h4;
        let nctb = sps.ctb_w * sps.ctb_h;
        let mid = |bd: u32| 1u16 << (bd - 1);
        let frame = Frame {
            planes: [vec![mid(sps.bit_depth_luma); w * h], vec![mid(sps.bit_depth_chroma); cw * ch], vec![mid(sps.bit_depth_chroma); cw * ch]],
            width: w,
            height: h,
            cwidth: cw,
            cheight: ch,
            poc,
            mv: vec![MvField::default(); n4],
            w4,
            pts,
            crop: sps.crop,
            bit_depth: sps.bit_depth_luma,
            chroma_format: sps.chroma_format_idc,
        };
        CurPic {
            frame,
            w4,
            h4,
            pred_mode: vec![PRED_NONE; n4],
            skip: vec![0; n4],
            ct_depth: vec![0; n4],
            ipm: vec![1; n4],
            qp: vec![0; n4],
            nz: vec![0; n4],
            edges: vec![0; n4],
            nofilter: vec![0; n4],
            ctb_slice_addr: vec![-1; nctb],
            ctb_slice_idx: vec![0; nctb],
            slices: Vec::new(),
            sao: vec![Sao::default(); nctb],
            ctbs_done: 0,
        }
    }
}

/// Tile and scan layout of a picture, derived from the active parameter sets (6.5.1, 6.5.2).
pub struct Geometry {
    pub rs_to_ts: Vec<usize>,
    pub ts_to_rs: Vec<usize>,
    /// Tile index per CTB in tile scan.
    pub tile_id: Vec<usize>,
    /// Z-scan order address on the 4x4 grid.
    pub zs: Vec<u32>,
    pub w4: usize,
    pub h4: usize,
}

impl Geometry {
    pub fn new(sps: &Sps, pps: &Pps) -> Self {
        let (wc, hc) = (sps.ctb_w, sps.ctb_h);
        let (nc, nr) = (pps.num_tile_cols, pps.num_tile_rows);
        let mut col_w = vec![0usize; nc];
        let mut row_h = vec![0usize; nr];
        if pps.uniform_spacing {
            for i in 0..nc {
                col_w[i] = ((i + 1) * wc) / nc - (i * wc) / nc;
            }
            for j in 0..nr {
                row_h[j] = ((j + 1) * hc) / nr - (j * hc) / nr;
            }
        } else {
            let mut sum = 0;
            for i in 0..nc - 1 {
                col_w[i] = pps.col_widths_minus1[i] as usize + 1;
                sum += col_w[i];
            }
            col_w[nc - 1] = wc.saturating_sub(sum);
            sum = 0;
            for j in 0..nr - 1 {
                row_h[j] = pps.row_heights_minus1[j] as usize + 1;
                sum += row_h[j];
            }
            row_h[nr - 1] = hc.saturating_sub(sum);
        }
        let mut col_bd = vec![0usize; nc + 1];
        let mut row_bd = vec![0usize; nr + 1];
        for i in 0..nc {
            col_bd[i + 1] = col_bd[i] + col_w[i];
        }
        for j in 0..nr {
            row_bd[j + 1] = row_bd[j] + row_h[j];
        }
        let n = wc * hc;
        let mut rs_to_ts = vec![0usize; n];
        let mut tile_of_rs = vec![0usize; n];
        for rs in 0..n {
            let (tx, ty) = (rs % wc, rs / wc);
            let tile_x = (0..nc).rev().find(|&i| tx >= col_bd[i]).unwrap_or(0);
            let tile_y = (0..nr).rev().find(|&j| ty >= row_bd[j]).unwrap_or(0);
            let mut ts = 0;
            for i in 0..tile_x {
                ts += row_h[tile_y] * col_w[i];
            }
            for j in 0..tile_y {
                ts += wc * row_h[j];
            }
            ts += (ty - row_bd[tile_y]) * col_w[tile_x] + tx - col_bd[tile_x];
            rs_to_ts[rs] = ts;
            tile_of_rs[rs] = tile_y * nc + tile_x;
        }
        let mut ts_to_rs = vec![0usize; n];
        let mut tile_id = vec![0usize; n];
        for rs in 0..n {
            ts_to_rs[rs_to_ts[rs]] = rs;
            tile_id[rs_to_ts[rs]] = tile_of_rs[rs];
        }
        let per = 1usize << (sps.log2_ctb - 2);
        let (w4, h4) = (sps.width.div_ceil(4), sps.height.div_ceil(4));
        let mut zs = vec![0u32; w4 * h4];
        for y in 0..h4 {
            for x in 0..w4 {
                let rs = (y / per) * wc + x / per;
                let mut z = 0u32;
                let (lx, ly) = (x % per, y % per);
                for b in 0..sps.log2_ctb - 2 {
                    z |= (((lx >> b) & 1) as u32) << (2 * b);
                    z |= (((ly >> b) & 1) as u32) << (2 * b + 1);
                }
                zs[y * w4 + x] = ((rs_to_ts[rs] as u32) << (2 * (sps.log2_ctb - 2))) + z;
            }
        }
        Geometry { rs_to_ts, ts_to_rs, tile_id, zs, w4, h4 }
    }
}
