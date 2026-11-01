//! Sequence and picture parameter sets and slice headers (ITU-T H.264 7.3.2 and 7.3.3).

use super::bits::BitReader;
use super::tables::{DEFAULT_SCALING4, DEFAULT_SCALING8};
use ironav_core::{Error, ErrorCode, Result};

fn unsupported(msg: impl Into<String>) -> Error {
    Error::new(ErrorCode::UnsupportedCodec, msg.into())
}

/// The 4x4 zigzag scan as raster positions.
pub const ZIGZAG4: [u8; 16] = [0, 1, 4, 8, 5, 2, 3, 6, 9, 12, 13, 10, 7, 11, 14, 15];

/// The 4x4 field scan (of field macroblocks) as raster positions.
pub const FIELD4: [u8; 16] = [0, 4, 1, 8, 12, 5, 9, 13, 2, 6, 10, 14, 3, 7, 11, 15];

/// The 8x8 field scan as raster positions.
pub const FIELD8: [u8; 64] = [
    0, 8, 16, 1, 9, 24, 32, 17, 2, 25, 40, 48, 56, 33, 10, 3, 18, 41, 49, 57, 26, 11, 4, 19, 34, 42, 50, 58, 27, 12, 5, 20, 35, 43, 51, 59, 28, 13, 6, 21, 36, 44, 52, 60, 29, 14, 22, 37, 45, 53, 61, 30, 7, 15, 38, 46, 54, 62, 23, 31, 39, 47, 55, 63,
];

#[derive(Clone)]
pub struct Sps {
    #[allow(dead_code)]
    pub profile_idc: u8,
    pub level_idc: u8,
    pub id: u32,
    pub chroma_format_idc: u32,
    pub bit_depth_luma: u32,
    pub bit_depth_chroma: u32,
    pub transform_bypass: bool,
    pub matrix_present: bool,
    /// Six 4x4 and six 8x8 lists, in zigzag order as coded.
    pub lists4: [[u8; 16]; 6],
    pub lists8: [[u8; 64]; 6],
    pub log2_max_frame_num: u32,
    pub poc_type: u32,
    pub log2_max_poc_lsb: u32,
    pub delta_pic_order_always_zero: bool,
    pub offset_for_non_ref_pic: i32,
    pub offset_for_top_to_bottom_field: i32,
    pub offset_for_ref_frame: Vec<i32>,
    pub max_num_ref_frames: u32,
    #[allow(dead_code)]
    pub gaps_allowed: bool,
    pub mb_width: usize,
    pub mb_height: usize,
    pub frame_mbs_only: bool,
    pub mb_adaptive_frame_field: bool,
    pub direct_8x8_inference: bool,
    /// Cropping rectangle in luma samples: left, right, top, bottom.
    pub crop: [usize; 4],
    /// `max_num_reorder_frames` from the VUI, when the stream states it.
    pub num_reorder_frames: Option<u32>,
}

fn flat_lists() -> ([[u8; 16]; 6], [[u8; 64]; 6]) {
    ([[16; 16]; 6], [[16; 64]; 6])
}

/// Reads one scaling list; returns None when the "use default" escape was coded.
fn scaling_list(r: &mut BitReader, out: &mut [u8]) -> Result<bool> {
    let (mut last, mut next) = (8i32, 8i32);
    for j in 0..out.len() {
        if next != 0 {
            let delta = r.se()?;
            next = (last + delta + 256) % 256;
            if j == 0 && next == 0 {
                return Ok(false);
            }
        }
        out[j] = if next == 0 { last as u8 } else { next as u8 };
        last = out[j] as i32;
    }
    Ok(true)
}

/// Reads the `count` lists of a scaling matrix, applying fall-back rule A (sequence level, `base`
/// is None) or B (picture level, `base` holds the sequence's lists).
fn scaling_matrix(r: &mut BitReader, count: usize, base: Option<(&[[u8; 16]; 6], &[[u8; 64]; 6])>) -> Result<([[u8; 16]; 6], [[u8; 64]; 6])> {
    let (mut l4, mut l8) = flat_lists();
    for i in 0..count {
        let present = r.flag()?;
        if i < 6 {
            let fallback: [u8; 16] = match (i, base) {
                (0 | 3, None) => zig4(&DEFAULT_SCALING4[i / 3]),
                (0 | 3, Some((b4, _))) => b4[i],
                _ => l4[i - 1],
            };
            l4[i] = fallback;
            if present {
                let mut tmp = [0u8; 16];
                l4[i] = if scaling_list(r, &mut tmp)? { tmp } else { zig4(&DEFAULT_SCALING4[i / 3]) };
            }
        } else {
            let k = i - 6;
            let fallback: [u8; 64] = match (k, base) {
                (0 | 1, None) => zig8(&DEFAULT_SCALING8[k]),
                (0 | 1, Some((_, b8))) => b8[k],
                _ => l8[k - 2],
            };
            l8[k] = fallback;
            if present {
                let mut tmp = [0u8; 64];
                l8[k] = if scaling_list(r, &mut tmp)? { tmp } else { zig8(&DEFAULT_SCALING8[k]) };
            }
        }
    }
    // with 4:2:0 only two 8x8 lists are coded; the rest repeat them
    for k in count.saturating_sub(6).max(2)..6 {
        l8[k] = l8[k - 2];
    }
    Ok((l4, l8))
}

/// Default lists are kept in raster order; scaling lists are coded in zigzag order.
fn zig4(raster: &[u8; 16]) -> [u8; 16] {
    let mut o = [0u8; 16];
    for (k, &p) in ZIGZAG4.iter().enumerate() {
        o[k] = raster[p as usize];
    }
    o
}

fn zig8(raster: &[u8; 64]) -> [u8; 64] {
    let mut o = [0u8; 64];
    for (k, &p) in super::tables::ZIGZAG_DIRECT.iter().enumerate() {
        o[k] = raster[p as usize];
    }
    o
}

pub fn parse_sps(rbsp: &[u8]) -> Result<Sps> {
    let mut r = BitReader::new(rbsp);
    let profile_idc = r.bits(8)? as u8;
    r.bits(8)?; // constraint flags and reserved bits
    let level_idc = r.bits(8)? as u8;
    let id = r.ue()?;
    if id > 31 {
        return Err(Error::invalid_input("H.264 SPS id is out of range"));
    }
    let (mut chroma_format_idc, mut bit_depth_luma, mut bit_depth_chroma) = (1, 8, 8);
    let (mut transform_bypass, mut matrix_present) = (false, false);
    let (mut lists4, mut lists8) = flat_lists();
    if matches!(profile_idc, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        chroma_format_idc = r.ue()?;
        if chroma_format_idc > 3 {
            return Err(Error::invalid_input("H.264 chroma_format_idc is out of range"));
        }
        if chroma_format_idc == 3 && r.flag()? {
            return Err(unsupported("H.264 with separate colour planes is not supported"));
        }
        bit_depth_luma = r.ue()? + 8;
        bit_depth_chroma = r.ue()? + 8;
        transform_bypass = r.flag()?;
        matrix_present = r.flag()?;
        if matrix_present {
            let (a, b) = scaling_matrix(&mut r, if chroma_format_idc != 3 { 8 } else { 12 }, None)?;
            lists4 = a;
            lists8 = b;
        }
    }
    let log2_max_frame_num = r.ue()? + 4;
    let poc_type = r.ue()?;
    let (mut log2_max_poc_lsb, mut delta_pic_order_always_zero, mut offset_for_non_ref_pic, mut offset_for_top_to_bottom_field) = (0, false, 0, 0);
    let mut offset_for_ref_frame = Vec::new();
    match poc_type {
        0 => log2_max_poc_lsb = r.ue()? + 4,
        1 => {
            delta_pic_order_always_zero = r.flag()?;
            offset_for_non_ref_pic = r.se()?;
            offset_for_top_to_bottom_field = r.se()?;
            let n = r.ue()?;
            if n > 255 {
                return Err(Error::invalid_input("H.264 SPS lists too many reference frame offsets"));
            }
            for _ in 0..n {
                offset_for_ref_frame.push(r.se()?);
            }
        }
        2 => {}
        _ => return Err(Error::invalid_input("H.264 pic_order_cnt_type is out of range")),
    }
    let max_num_ref_frames = r.ue()?;
    let gaps_allowed = r.flag()?;
    let mb_width = r.ue()? as usize + 1;
    let map_units_height = r.ue()? as usize + 1;
    let frame_mbs_only = r.flag()?;
    let mut mb_adaptive_frame_field = false;
    if !frame_mbs_only {
        mb_adaptive_frame_field = r.flag()?;
    }
    let mb_height = map_units_height * if frame_mbs_only { 1 } else { 2 };
    let direct_8x8_inference = r.flag()?;
    let mut crop = [0usize; 4];
    if r.flag()? {
        let (cl, cr, ct, cb) = (r.ue()? as usize, r.ue()? as usize, r.ue()? as usize, r.ue()? as usize);
        let (cx, cy) = match chroma_format_idc {
            1 => (2, 2),
            2 => (2, 1),
            _ => (1, 1),
        };
        let cy = cy * if frame_mbs_only { 1 } else { 2 };
        crop = [cl * cx, cr * cx, ct * cy, cb * cy];
    }
    if mb_width * mb_height > 139_264 {
        return Err(unsupported("H.264 pictures wider or taller than 16384 samples are not supported"));
    }
    if crop[0] + crop[1] >= mb_width * 16 || crop[2] + crop[3] >= mb_height * 16 {
        return Err(Error::invalid_input("H.264 cropping leaves no picture"));
    }
    // the VUI is optional; only the reordering depth is of interest
    let mut num_reorder_frames = None;
    if r.flag().unwrap_or(false) {
        num_reorder_frames = parse_vui_reorder(&mut r).unwrap_or(None);
    }
    Ok(Sps {
        profile_idc,
        level_idc,
        id,
        chroma_format_idc,
        bit_depth_luma,
        bit_depth_chroma,
        transform_bypass,
        matrix_present,
        lists4,
        lists8,
        log2_max_frame_num,
        poc_type,
        log2_max_poc_lsb,
        delta_pic_order_always_zero,
        offset_for_non_ref_pic,
        offset_for_top_to_bottom_field,
        offset_for_ref_frame,
        max_num_ref_frames,
        gaps_allowed,
        mb_width,
        mb_height,
        frame_mbs_only,
        mb_adaptive_frame_field,
        direct_8x8_inference,
        crop,
        num_reorder_frames,
    })
}

/// Walks the VUI far enough to read bitstream_restriction's max_num_reorder_frames.
fn parse_vui_reorder(r: &mut BitReader) -> Result<Option<u32>> {
    if r.flag()? && r.bits(8)? == 255 {
        r.bits(32)?; // sar width and height
    }
    if r.flag()? {
        r.flag()?; // overscan_appropriate
    }
    if r.flag()? {
        r.bits(4)?; // video_format, full range
        if r.flag()? {
            r.bits(24)?;
        }
    }
    if r.flag()? {
        r.ue()?;
        r.ue()?;
    }
    if r.flag()? {
        r.bits(32)?;
        r.bits(32)?;
        r.flag()?;
    }
    let nal_hrd = r.flag()?;
    if nal_hrd {
        skip_hrd(r)?;
    }
    let vcl_hrd = r.flag()?;
    if vcl_hrd {
        skip_hrd(r)?;
    }
    if nal_hrd || vcl_hrd {
        r.flag()?;
    }
    r.flag()?; // pic_struct_present
    if r.flag()? {
        r.flag()?;
        r.ue()?;
        r.ue()?;
        r.ue()?;
        r.ue()?;
        let reorder = r.ue()?;
        return Ok(Some(reorder));
    }
    Ok(None)
}

fn skip_hrd(r: &mut BitReader) -> Result<()> {
    let n = r.ue()? + 1;
    r.bits(8)?;
    for _ in 0..n {
        r.ue()?;
        r.ue()?;
        r.flag()?;
    }
    r.bits(20)?;
    Ok(())
}

#[derive(Clone)]
pub struct Pps {
    pub id: u32,
    pub sps_id: u32,
    pub cabac: bool,
    pub bottom_field_pic_order_in_frame_present: bool,
    pub num_ref_idx_default: [u32; 2],
    pub weighted_pred: bool,
    pub weighted_bipred_idc: u32,
    pub init_qp: i32,
    pub chroma_qp_offset: [i32; 2],
    pub deblocking_control_present: bool,
    pub constrained_intra_pred: bool,
    pub redundant_pic_cnt_present: bool,
    pub transform_8x8: bool,
    /// Scaling lists in zigzag order: the picture's own when it codes them, the sequence's otherwise.
    pub lists4: [[u8; 16]; 6],
    pub lists8: [[u8; 64]; 6],
}

pub fn parse_pps(rbsp: &[u8], spss: &[Option<Sps>]) -> Result<Pps> {
    let mut r = BitReader::new(rbsp);
    let id = r.ue()?;
    let sps_id = r.ue()?;
    if id > 255 || sps_id > 31 {
        return Err(Error::invalid_input("H.264 PPS id is out of range"));
    }
    let sps = spss[sps_id as usize].as_ref().ok_or_else(|| Error::invalid_input("H.264 PPS refers to a sequence parameter set that has not been seen"))?;
    let cabac = r.flag()?;
    let bottom_field_pic_order_in_frame_present = r.flag()?;
    if r.ue()? > 0 {
        return Err(unsupported("H.264 slice groups (FMO) are not supported"));
    }
    let n0 = r.ue()? + 1;
    let n1 = r.ue()? + 1;
    if n0 > 32 || n1 > 32 {
        return Err(Error::invalid_input("H.264 default reference count is out of range"));
    }
    let weighted_pred = r.flag()?;
    let weighted_bipred_idc = r.bits(2)?;
    let init_qp = 26 + r.se()?;
    r.se()?; // pic_init_qs
    let c0 = r.se()?;
    let deblocking_control_present = r.flag()?;
    let constrained_intra_pred = r.flag()?;
    let redundant_pic_cnt_present = r.flag()?;
    let mut transform_8x8 = false;
    let (mut lists4, mut lists8) = (sps.lists4, sps.lists8);
    let mut c1 = c0;
    if r.more_rbsp_data() {
        transform_8x8 = r.flag()?;
        if r.flag()? {
            let count = 6 + if sps.chroma_format_idc != 3 { 2 } else { 6 } * transform_8x8 as usize;
            let base = if sps.matrix_present { Some((&sps.lists4, &sps.lists8)) } else { None };
            let (a, b) = scaling_matrix(&mut r, count, base)?;
            lists4 = a;
            lists8 = b;
        }
        c1 = r.se()?;
    }
    Ok(Pps {
        id,
        sps_id,
        cabac,
        bottom_field_pic_order_in_frame_present,
        num_ref_idx_default: [n0, n1],
        weighted_pred,
        weighted_bipred_idc,
        init_qp,
        chroma_qp_offset: [c0, c1],
        deblocking_control_present,
        constrained_intra_pred,
        redundant_pic_cnt_present,
        transform_8x8,
        lists4,
        lists8,
    })
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SliceType {
    P,
    B,
    I,
}

#[derive(Clone, Debug, Default)]
pub struct RefMod {
    /// (modification_of_pic_nums_idc, value) pairs for each list.
    pub ops: [Vec<(u32, u32)>; 2],
}

#[derive(Clone, Debug, Default)]
pub struct PredWeights {
    pub luma_log2_denom: u32,
    pub chroma_log2_denom: u32,
    /// Per list and reference index: (luma weight, luma offset, [cb weight, cb offset], [cr weight, cr offset]).
    pub entries: [Vec<[i32; 6]>; 2],
}

#[derive(Clone, Debug, Default)]
pub struct Marking {
    pub no_output_of_prior_pics: bool,
    pub long_term_reference: bool,
    pub adaptive: bool,
    /// (memory_management_control_operation, arg1, arg2)
    pub ops: Vec<(u32, u32, u32)>,
}

#[derive(Clone)]
pub struct SliceHeader {
    pub first_mb: usize,
    pub slice_type: SliceType,
    pub pps_id: u32,
    pub frame_num: u32,
    pub idr: bool,
    pub nal_ref_idc: u32,
    pub idr_pic_id: u32,
    pub poc_lsb: u32,
    pub delta_poc_bottom: i32,
    pub delta_poc: [i32; 2],
    pub redundant_pic_cnt: u32,
    pub direct_spatial_mv_pred: bool,
    pub num_ref_idx_active: [u32; 2],
    pub ref_mod: RefMod,
    pub weights: Option<PredWeights>,
    pub marking: Marking,
    pub cabac_init_idc: usize,
    pub qp: i32,
    pub disable_deblocking: u32,
    pub alpha_offset: i32,
    pub beta_offset: i32,
    /// Bit position in the unescaped payload where the slice data begins.
    pub data_pos: usize,
}

pub fn parse_slice_header(rbsp: &[u8], nal_type: u8, nal_ref_idc: u32, spss: &[Option<Sps>], ppss: &[Option<Pps>]) -> Result<SliceHeader> {
    let mut r = BitReader::new(rbsp);
    let first_mb = r.ue()? as usize;
    let st = r.ue()?;
    let slice_type = match st % 5 {
        0 => SliceType::P,
        1 => SliceType::B,
        2 => SliceType::I,
        _ => return Err(unsupported("H.264 SP and SI slices are not supported")),
    };
    let pps_id = r.ue()?;
    let pps = ppss.get(pps_id as usize).and_then(|p| p.as_ref()).ok_or_else(|| Error::invalid_input("H.264 slice refers to a picture parameter set that has not been seen"))?;
    let sps = spss[pps.sps_id as usize].as_ref().ok_or_else(|| Error::invalid_input("H.264 slice refers to a missing sequence parameter set"))?;
    let idr = nal_type == 5;
    let frame_num = r.bits(sps.log2_max_frame_num)?;
    // field_pic_flag: frames, MBAFF ones included, are decoded; single fields are not
    if !sps.frame_mbs_only && r.flag()? {
        return Err(unsupported("H.264 field pictures are not supported (frame and MBAFF pictures are)"));
    }
    let idr_pic_id = if idr { r.ue()? } else { 0 };
    let (mut poc_lsb, mut delta_poc_bottom, mut delta_poc) = (0, 0, [0, 0]);
    if sps.poc_type == 0 {
        poc_lsb = r.bits(sps.log2_max_poc_lsb)?;
        if pps.bottom_field_pic_order_in_frame_present {
            delta_poc_bottom = r.se()?;
        }
    }
    if sps.poc_type == 1 && !sps.delta_pic_order_always_zero {
        delta_poc[0] = r.se()?;
        if pps.bottom_field_pic_order_in_frame_present {
            delta_poc[1] = r.se()?;
        }
    }
    let redundant_pic_cnt = if pps.redundant_pic_cnt_present { r.ue()? } else { 0 };
    let direct_spatial_mv_pred = if slice_type == SliceType::B { r.flag()? } else { false };
    let mut num_ref_idx_active = [0u32; 2];
    if slice_type != SliceType::I {
        num_ref_idx_active = pps.num_ref_idx_default;
        if r.flag()? {
            num_ref_idx_active[0] = r.ue()? + 1;
            if slice_type == SliceType::B {
                num_ref_idx_active[1] = r.ue()? + 1;
            }
        }
        if slice_type != SliceType::B {
            num_ref_idx_active[1] = 0;
        }
        if num_ref_idx_active[0] > 32 || num_ref_idx_active[1] > 32 {
            return Err(Error::invalid_input("H.264 reference index count is out of range"));
        }
    }
    let mut ref_mod = RefMod::default();
    if slice_type != SliceType::I {
        for list in 0..if slice_type == SliceType::B { 2 } else { 1 } {
            if r.flag()? {
                loop {
                    let idc = r.ue()?;
                    if idc == 3 {
                        break;
                    }
                    if idc > 5 {
                        return Err(Error::invalid_input("H.264 reference list modification code is out of range"));
                    }
                    let v = r.ue()?;
                    ref_mod.ops[list].push((idc, v));
                    if ref_mod.ops[list].len() > 66 {
                        return Err(Error::invalid_input("H.264 reference list modification is too long"));
                    }
                }
            }
        }
    }
    let mut weights = None;
    if (pps.weighted_pred && slice_type == SliceType::P) || (pps.weighted_bipred_idc == 1 && slice_type == SliceType::B) {
        let mut w = PredWeights { luma_log2_denom: r.ue()?, chroma_log2_denom: 0, entries: [Vec::new(), Vec::new()] };
        let chroma = sps.chroma_format_idc != 0;
        if chroma {
            w.chroma_log2_denom = r.ue()?;
        }
        if w.luma_log2_denom > 7 || w.chroma_log2_denom > 7 {
            return Err(Error::invalid_input("H.264 weight denominator is out of range"));
        }
        for list in 0..if slice_type == SliceType::B { 2 } else { 1 } {
            for _ in 0..num_ref_idx_active[list] {
                // defaults apply when a flag is clear: weight 2^denom, offset 0
                let mut e = [1 << w.luma_log2_denom, 0, 1 << w.chroma_log2_denom, 0, 1 << w.chroma_log2_denom, 0];
                if r.flag()? {
                    e[0] = r.se()?;
                    e[1] = r.se()?;
                }
                if chroma && r.flag()? {
                    e[2] = r.se()?;
                    e[3] = r.se()?;
                    e[4] = r.se()?;
                    e[5] = r.se()?;
                }
                w.entries[list].push(e);
            }
        }
        weights = Some(w);
    }
    let mut marking = Marking::default();
    if nal_ref_idc != 0 {
        if idr {
            marking.no_output_of_prior_pics = r.flag()?;
            marking.long_term_reference = r.flag()?;
        } else {
            marking.adaptive = r.flag()?;
            if marking.adaptive {
                loop {
                    let op = r.ue()?;
                    if op == 0 {
                        break;
                    }
                    let (mut a, mut b) = (0, 0);
                    match op {
                        1 | 3 => a = r.ue()?,
                        2 => a = r.ue()?,
                        _ => {}
                    }
                    if op == 3 || op == 6 {
                        b = r.ue()?;
                    }
                    if op == 4 {
                        a = r.ue()?;
                    }
                    if op > 6 {
                        return Err(Error::invalid_input("H.264 memory management operation is out of range"));
                    }
                    marking.ops.push((op, a, b));
                    if marking.ops.len() > 66 {
                        return Err(Error::invalid_input("H.264 memory management list is too long"));
                    }
                }
            }
        }
    }
    let cabac_init_idc = if pps.cabac && slice_type != SliceType::I { r.ue()? as usize } else { 0 };
    if cabac_init_idc > 2 {
        return Err(Error::invalid_input("H.264 cabac_init_idc is out of range"));
    }
    let qp = pps.init_qp + r.se()?;
    if !(-6 * (sps.bit_depth_luma as i32 - 8)..=51).contains(&qp) {
        return Err(Error::invalid_input("H.264 slice QP is out of range"));
    }
    let (mut disable_deblocking, mut alpha_offset, mut beta_offset) = (0, 0, 0);
    if pps.deblocking_control_present {
        disable_deblocking = r.ue()?;
        if disable_deblocking > 2 {
            return Err(Error::invalid_input("H.264 disable_deblocking_filter_idc is out of range"));
        }
        if disable_deblocking != 1 {
            alpha_offset = r.se()? * 2;
            beta_offset = r.se()? * 2;
        }
    }
    Ok(SliceHeader {
        first_mb,
        slice_type,
        pps_id,
        frame_num,
        idr,
        nal_ref_idc,
        idr_pic_id,
        poc_lsb,
        delta_poc_bottom,
        delta_poc,
        redundant_pic_cnt,
        direct_spatial_mv_pred,
        num_ref_idx_active,
        ref_mod,
        weights,
        marking,
        cabac_init_idc,
        qp,
        disable_deblocking,
        alpha_offset,
        beta_offset,
        data_pos: r.pos(),
    })
}
