//! OBU framing, the sequence header and the frame header of AV1 (spec sections 5.3 to 5.9).

use ironav_core::{Error, ErrorCode, Result};

pub const OBU_SEQUENCE_HEADER: u8 = 1;
pub const OBU_TEMPORAL_DELIMITER: u8 = 2;
pub const OBU_FRAME_HEADER: u8 = 3;
pub const OBU_TILE_GROUP: u8 = 4;
pub const OBU_FRAME: u8 = 6;
pub const OBU_REDUNDANT_FRAME_HEADER: u8 = 7;

pub const KEY_FRAME: u8 = 0;
pub const INTER_FRAME: u8 = 1;
pub const INTRA_ONLY_FRAME: u8 = 2;
pub const SWITCH_FRAME: u8 = 3;

pub const SELECT: u8 = 2;
pub const ALL_FRAMES: u8 = 0xFF;
pub const TX_MODE_LARGEST: u8 = 1;
pub const TX_MODE_SELECT: u8 = 2;

pub fn invalid(msg: impl Into<String>) -> Error {
    Error::new(ErrorCode::InvalidInput, format!("AV1: {}", msg.into()))
}

fn unsupported(msg: impl Into<String>) -> Error {
    Error::new(ErrorCode::UnsupportedFormat, format!("AV1: {}", msg.into()))
}

/// MSB-first bit reader over a byte slice (the spec's f(n) and friends).
pub struct Bits<'a> {
    data: &'a [u8],
    pub pos: usize,
}

impl<'a> Bits<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Bits { data, pos: 0 }
    }
    pub fn bit(&mut self) -> Result<u32> {
        let byte = *self.data.get(self.pos >> 3).ok_or_else(|| invalid("header runs past its end"))?;
        let b = (byte >> (7 - (self.pos & 7))) & 1;
        self.pos += 1;
        Ok(b as u32)
    }
    pub fn flag(&mut self) -> Result<bool> {
        Ok(self.bit()? != 0)
    }
    pub fn f(&mut self, n: u32) -> Result<u32> {
        let mut x: u64 = 0;
        for _ in 0..n {
            x = (x << 1) | self.bit()? as u64;
        }
        Ok(x as u32)
    }
    pub fn su(&mut self, n: u32) -> Result<i32> {
        let v = self.f(n)? as i64;
        let sign = 1i64 << (n - 1);
        Ok(if v & sign != 0 { (v - 2 * sign) as i32 } else { v as i32 })
    }
    pub fn ns(&mut self, n: u32) -> Result<u32> {
        let w = 32 - n.leading_zeros();
        let m = (1u32 << w) - n;
        let v = self.f(w - 1)?;
        if v < m {
            return Ok(v);
        }
        let extra = self.bit()?;
        Ok((v << 1) - m + extra)
    }
    pub fn uvlc(&mut self) -> Result<u32> {
        let mut zeros = 0;
        while self.bit()? == 0 {
            zeros += 1;
            if zeros >= 32 {
                return Err(invalid("uvlc value too large"));
            }
        }
        if zeros == 0 {
            return Ok(0);
        }
        Ok(((1u64 << zeros) - 1 + self.f(zeros)? as u64) as u32)
    }
    pub fn byte_align(&mut self) {
        self.pos = (self.pos + 7) & !7;
    }
    pub fn byte_pos(&self) -> usize {
        self.pos >> 3
    }
}

/// Reads a leb128 value; returns the value and the number of bytes it took.
pub fn leb128(data: &[u8]) -> Result<(u64, usize)> {
    let mut v = 0u64;
    for i in 0..8 {
        let b = *data.get(i).ok_or_else(|| invalid("truncated leb128"))?;
        v |= ((b & 0x7F) as u64) << (7 * i);
        if b & 0x80 == 0 {
            return Ok((v, i + 1));
        }
    }
    Err(invalid("leb128 value too long"))
}

/// One OBU: its type, its temporal and spatial ids (when it has an extension), and its payload.
pub struct Obu<'a> {
    pub kind: u8,
    pub ext: Option<(u8, u8)>,
    pub payload: &'a [u8],
}

/// Splits a temporal unit (or any run of OBUs in the low-overhead format) into OBUs.
pub fn split_obus(data: &[u8]) -> Result<Vec<Obu<'_>>> {
    let mut out = Vec::new();
    let mut p = 0;
    while p < data.len() {
        let h = data[p];
        if h & 0x80 != 0 {
            return Err(invalid("forbidden bit set in an OBU header"));
        }
        let kind = (h >> 3) & 15;
        let has_ext = h & 4 != 0;
        let has_size = h & 2 != 0;
        p += 1;
        let mut ext = None;
        if has_ext {
            let e = *data.get(p).ok_or_else(|| invalid("truncated OBU extension"))?;
            ext = Some((e >> 5, (e >> 3) & 3));
            p += 1;
        }
        let size = if has_size {
            let (v, n) = leb128(&data[p..])?;
            p += n;
            v as usize
        } else {
            data.len() - p
        };
        let end = p.checked_add(size).filter(|&e| e <= data.len()).ok_or_else(|| invalid("OBU size runs past the packet"))?;
        out.push(Obu { kind, ext, payload: &data[p..end] });
        p = end;
    }
    Ok(out)
}

#[derive(Clone, Debug, Default)]
pub struct SequenceHeader {
    pub profile: u32,
    pub still_picture: bool,
    pub reduced_still_picture_header: bool,
    /// operating_point_idc of the operating point decoded (point 0).
    pub op_idc: u32,
    pub decoder_model_info_present: bool,
    pub equal_picture_interval: bool,
    pub buffer_removal_time_length: u32,
    pub frame_presentation_time_length: u32,
    pub op_decoder_model_present: Vec<bool>,
    pub op_idc_all: Vec<u32>,
    pub frame_width_bits: u32,
    pub frame_height_bits: u32,
    pub max_frame_width: u32,
    pub max_frame_height: u32,
    pub frame_id_numbers_present: bool,
    pub delta_frame_id_length: u32,
    pub additional_frame_id_length: u32,
    pub use_128x128_superblock: bool,
    pub enable_filter_intra: bool,
    pub enable_intra_edge_filter: bool,
    pub enable_order_hint: bool,
    pub order_hint_bits: u32,
    pub enable_superres: bool,
    pub enable_cdef: bool,
    pub enable_restoration: bool,
    pub seq_force_screen_content_tools: u8,
    pub seq_force_integer_mv: u8,
    pub bit_depth: u32,
    pub mono_chrome: bool,
    pub subsampling_x: bool,
    pub subsampling_y: bool,
    pub color_range: bool,
    pub separate_uv_delta_q: bool,
    /// matrix_coefficients is identity: the planes are G, B and R.
    pub rgb: bool,
    pub film_grain_params_present: bool,
}

impl SequenceHeader {
    pub fn num_planes(&self) -> usize {
        if self.mono_chrome {
            1
        } else {
            3
        }
    }
}

/// timing_info(); returns equal_picture_interval.
fn timing_info(b: &mut Bits) -> Result<bool> {
    b.f(32)?;
    b.f(32)?;
    let equal = b.flag()?;
    if equal {
        b.uvlc()?;
    }
    Ok(equal)
}

pub fn parse_sequence_header(data: &[u8]) -> Result<SequenceHeader> {
    let mut b = Bits::new(data);
    let mut s = SequenceHeader { profile: b.f(3)?, ..Default::default() };
    s.still_picture = b.flag()?;
    s.reduced_still_picture_header = b.flag()?;
    let mut buffer_delay_len = 0;
    if s.reduced_still_picture_header {
        b.f(5)?;
        s.op_decoder_model_present = vec![false];
        s.op_idc_all = vec![0];
    } else {
        let timing = b.flag()?;
        if timing {
            s.equal_picture_interval = timing_info(&mut b)?;
            s.decoder_model_info_present = b.flag()?;
            if s.decoder_model_info_present {
                buffer_delay_len = b.f(5)? + 1;
                b.f(32)?;
                s.buffer_removal_time_length = b.f(5)? + 1;
                s.frame_presentation_time_length = b.f(5)? + 1;
            }
        }
        let initial_display_delay = b.flag()?;
        let cnt = b.f(5)? + 1;
        for _ in 0..cnt {
            let idc = b.f(12)?;
            s.op_idc_all.push(idc);
            let level = b.f(5)?;
            if level > 7 {
                b.bit()?;
            }
            let mut present = false;
            if s.decoder_model_info_present {
                present = b.flag()?;
                if present {
                    b.f(buffer_delay_len)?;
                    b.f(buffer_delay_len)?;
                    b.bit()?;
                }
            }
            s.op_decoder_model_present.push(present);
            if initial_display_delay && b.flag()? {
                b.f(4)?;
            }
        }
    }
    s.op_idc = s.op_idc_all[0];
    s.frame_width_bits = b.f(4)? + 1;
    s.frame_height_bits = b.f(4)? + 1;
    s.max_frame_width = b.f(s.frame_width_bits)? + 1;
    s.max_frame_height = b.f(s.frame_height_bits)? + 1;
    if !s.reduced_still_picture_header {
        s.frame_id_numbers_present = b.flag()?;
    }
    if s.frame_id_numbers_present {
        s.delta_frame_id_length = b.f(4)? + 2;
        s.additional_frame_id_length = b.f(3)? + 1;
    }
    s.use_128x128_superblock = b.flag()?;
    s.enable_filter_intra = b.flag()?;
    s.enable_intra_edge_filter = b.flag()?;
    if s.reduced_still_picture_header {
        s.seq_force_screen_content_tools = SELECT;
        s.seq_force_integer_mv = SELECT;
    } else {
        b.flag()?; // interintra compound
        b.flag()?; // masked compound
        b.flag()?; // warped motion
        b.flag()?; // dual filter
        s.enable_order_hint = b.flag()?;
        if s.enable_order_hint {
            b.flag()?; // jnt comp
            b.flag()?; // ref frame mvs
        }
        s.seq_force_screen_content_tools = if b.flag()? { SELECT } else { b.bit()? as u8 };
        s.seq_force_integer_mv = SELECT;
        if s.seq_force_screen_content_tools > 0 && !b.flag()? {
            s.seq_force_integer_mv = b.bit()? as u8;
        }
        if s.enable_order_hint {
            s.order_hint_bits = b.f(3)? + 1;
        }
    }
    s.enable_superres = b.flag()?;
    s.enable_cdef = b.flag()?;
    s.enable_restoration = b.flag()?;
    parse_color_config(&mut b, &mut s)?;
    s.film_grain_params_present = b.flag()?;
    Ok(s)
}

fn parse_color_config(b: &mut Bits, s: &mut SequenceHeader) -> Result<()> {
    let high = b.flag()?;
    s.bit_depth = if s.profile == 2 && high {
        if b.flag()? {
            12
        } else {
            10
        }
    } else if high {
        10
    } else {
        8
    };
    s.mono_chrome = if s.profile == 1 { false } else { b.flag()? };
    let (mut cp, mut tc, mut mc) = (2, 2, 2);
    if b.flag()? {
        cp = b.f(8)?;
        tc = b.f(8)?;
        mc = b.f(8)?;
    }
    if s.mono_chrome {
        s.color_range = b.flag()?;
        s.subsampling_x = true;
        s.subsampling_y = true;
        return Ok(());
    }
    s.rgb = mc == 0;
    if cp == 1 && tc == 13 && mc == 0 {
        s.color_range = true;
        s.subsampling_x = false;
        s.subsampling_y = false;
    } else {
        s.color_range = b.flag()?;
        match s.profile {
            0 => {
                s.subsampling_x = true;
                s.subsampling_y = true;
            }
            1 => {
                s.subsampling_x = false;
                s.subsampling_y = false;
            }
            _ => {
                if s.bit_depth == 12 {
                    s.subsampling_x = b.flag()?;
                    s.subsampling_y = if s.subsampling_x { b.flag()? } else { false };
                } else {
                    s.subsampling_x = true;
                    s.subsampling_y = false;
                }
            }
        }
        if s.subsampling_x && s.subsampling_y {
            b.f(2)?;
        }
    }
    s.separate_uv_delta_q = b.flag()?;
    Ok(())
}

/// Segmentation feature bits, signedness and maxima, per feature (Segmentation_Feature_*).
pub const SEG_FEATURE_BITS: [u32; 8] = [8, 6, 6, 6, 6, 3, 0, 0];
pub const SEG_FEATURE_SIGNED: [bool; 8] = [true, true, true, true, true, false, false, false];
pub const SEG_FEATURE_MAX: [i32; 8] = [255, 63, 63, 63, 63, 7, 0, 0];

/// The frame header fields the decoder uses. Inter prediction fields are absent: inter frames are
/// rejected before their header is parsed.
#[derive(Clone, Debug, Default)]
pub struct FrameHeader {
    pub show_existing_frame: bool,
    pub frame_to_show: usize,
    pub frame_type: u8,
    pub show_frame: bool,
    pub showable_frame: bool,
    pub error_resilient_mode: bool,
    pub disable_cdf_update: bool,
    pub allow_screen_content_tools: bool,
    pub frame_size_override: bool,
    pub order_hint: u32,
    pub primary_ref_frame: u8,
    pub refresh_frame_flags: u8,
    pub width: u32,
    pub height: u32,
    pub upscaled_width: u32,
    pub mi_cols: usize,
    pub mi_rows: usize,
    pub allow_intrabc: bool,
    pub disable_frame_end_update_cdf: bool,
    pub tile_cols_log2: u32,
    pub tile_rows_log2: u32,
    pub tile_cols: usize,
    pub tile_rows: usize,
    pub mi_col_starts: Vec<usize>,
    pub mi_row_starts: Vec<usize>,
    pub context_update_tile_id: usize,
    pub tile_size_bytes: usize,
    pub base_q_idx: i32,
    pub delta_q_y_dc: i32,
    pub delta_q_u_dc: i32,
    pub delta_q_u_ac: i32,
    pub delta_q_v_dc: i32,
    pub delta_q_v_ac: i32,
    pub using_qmatrix: bool,
    pub qm_y: u32,
    pub qm_u: u32,
    pub qm_v: u32,
    pub seg_enabled: bool,
    pub seg_update_map: bool,
    pub seg_temporal_update: bool,
    pub feature_enabled: [[bool; 8]; 8],
    pub feature_data: [[i32; 8]; 8],
    pub seg_id_pre_skip: bool,
    pub last_active_seg_id: u8,
    pub delta_q_present: bool,
    pub delta_q_res: u32,
    pub delta_lf_present: bool,
    pub delta_lf_res: u32,
    pub delta_lf_multi: bool,
    pub coded_lossless: bool,
    pub all_lossless: bool,
    pub lossless: [bool; 8],
    pub lf_level: [u8; 4],
    pub lf_sharpness: u8,
    pub lf_delta_enabled: bool,
    pub lf_ref_deltas: [i32; 8],
    pub lf_mode_deltas: [i32; 2],
    pub cdef_damping: u32,
    pub cdef_bits: u32,
    pub cdef_y_pri: [u8; 8],
    pub cdef_y_sec: [u8; 8],
    pub cdef_uv_pri: [u8; 8],
    pub cdef_uv_sec: [u8; 8],
    /// Per plane: 0 none, 1 Wiener, 2 self-guided, 3 switchable.
    pub lr_type: [u8; 3],
    pub lr_size: [u32; 3],
    pub uses_lr: bool,
    pub tx_mode: u8,
    pub reduced_tx_set: bool,
    pub superres_denom: u32,
    /// Where the tile data starts (byte offset into the frame header OBU's payload, when the
    /// header is followed by its tile group in the same OBU), after byte alignment.
    pub header_bytes: usize,
}

impl FrameHeader {
    pub fn frame_is_intra(&self) -> bool {
        self.frame_type == KEY_FRAME || self.frame_type == INTRA_ONLY_FRAME
    }
    pub fn num_tiles(&self) -> usize {
        self.tile_cols * self.tile_rows
    }
}

fn tile_log2(blk: usize, target: usize) -> u32 {
    let mut k = 0;
    while (blk << k) < target {
        k += 1;
    }
    k
}

/// The reference slots' frame types and order hints, which the frame header reads.
pub struct RefInfo {
    pub valid: [bool; 8],
    pub order_hint: [u32; 8],
    pub frame_type: [u8; 8],
}

/// Parses an uncompressed frame header. `temporal_id` and `spatial_id` are the OBU extension's
/// (zero without one). Returns the header; `header_bytes` is set to the byte length the header
/// takes, which for OBU_FRAME is where the tile group starts.
pub fn parse_frame_header(data: &[u8], seq: &SequenceHeader, refs: &RefInfo, temporal_id: u32, spatial_id: u32) -> Result<FrameHeader> {
    let mut b = Bits::new(data);
    let mut h = FrameHeader::default();
    let id_len = if seq.frame_id_numbers_present { seq.additional_frame_id_length + seq.delta_frame_id_length } else { 0 };
    if seq.reduced_still_picture_header {
        h.frame_type = KEY_FRAME;
        h.show_frame = true;
        h.error_resilient_mode = true;
    } else {
        h.show_existing_frame = b.flag()?;
        if h.show_existing_frame {
            h.frame_to_show = b.f(3)? as usize;
            if seq.decoder_model_info_present && !seq.equal_picture_interval {
                b.f(seq.frame_presentation_time_length)?;
            }
            if seq.frame_id_numbers_present {
                b.f(id_len)?;
            }
            if !refs.valid[h.frame_to_show] {
                return Err(invalid("show_existing_frame of an empty reference slot"));
            }
            h.frame_type = refs.frame_type[h.frame_to_show];
            if h.frame_type == KEY_FRAME {
                h.refresh_frame_flags = ALL_FRAMES;
            }
            b.byte_align();
            h.header_bytes = b.byte_pos();
            return Ok(h);
        }
        h.frame_type = b.f(2)? as u8;
        h.show_frame = b.flag()?;
        if h.show_frame && seq.decoder_model_info_present && !seq.equal_picture_interval {
            b.f(seq.frame_presentation_time_length)?;
        }
        h.showable_frame = if h.show_frame { h.frame_type != KEY_FRAME } else { b.flag()? };
        h.error_resilient_mode = if h.frame_type == SWITCH_FRAME || (h.frame_type == KEY_FRAME && h.show_frame) { true } else { b.flag()? };
    }
    if h.frame_type != KEY_FRAME && h.frame_type != INTRA_ONLY_FRAME {
        return Err(unsupported("inter frames are not decoded yet (stage 1 decodes key and intra-only frames)"));
    }
    h.disable_cdf_update = b.flag()?;
    h.allow_screen_content_tools = if seq.seq_force_screen_content_tools == SELECT { b.flag()? } else { seq.seq_force_screen_content_tools != 0 };
    if h.allow_screen_content_tools && seq.seq_force_integer_mv == SELECT {
        b.flag()?;
    }
    if seq.frame_id_numbers_present {
        b.f(id_len)?;
    }
    h.frame_size_override = if h.frame_type == SWITCH_FRAME {
        true
    } else if seq.reduced_still_picture_header {
        false
    } else {
        b.flag()?
    };
    h.order_hint = b.f(seq.order_hint_bits)?;
    h.primary_ref_frame = if h.frame_is_intra() || h.error_resilient_mode { 7 } else { b.f(3)? as u8 };
    if seq.decoder_model_info_present {
        let present = b.flag()?;
        if present {
            for (op, &idc) in seq.op_idc_all.iter().enumerate() {
                if seq.op_decoder_model_present[op] {
                    let in_t = (idc >> temporal_id) & 1 != 0;
                    let in_s = (idc >> (spatial_id + 8)) & 1 != 0;
                    if idc == 0 || (in_t && in_s) {
                        b.f(seq.buffer_removal_time_length)?;
                    }
                }
            }
        }
    }
    h.refresh_frame_flags = if h.frame_type == SWITCH_FRAME || (h.frame_type == KEY_FRAME && h.show_frame) { ALL_FRAMES } else { b.f(8)? as u8 };
    if (!h.frame_is_intra() || h.refresh_frame_flags != ALL_FRAMES) && h.error_resilient_mode && seq.enable_order_hint {
        for _ in 0..8 {
            b.f(seq.order_hint_bits)?;
        }
    }
    // frame_size, superres_params, render_size (intra frames only)
    if h.frame_size_override {
        h.width = b.f(seq.frame_width_bits)? + 1;
        h.height = b.f(seq.frame_height_bits)? + 1;
    } else {
        h.width = seq.max_frame_width;
        h.height = seq.max_frame_height;
    }
    h.superres_denom = 8;
    h.upscaled_width = h.width;
    if seq.enable_superres && b.flag()? {
        h.superres_denom = b.f(3)? + 9;
    }
    h.upscaled_width = h.width;
    h.width = (h.upscaled_width * 8 + h.superres_denom / 2) / h.superres_denom;
    if b.flag()? {
        b.f(16)?;
        b.f(16)?;
    }
    h.mi_cols = 2 * ((h.width as usize + 7) >> 3);
    h.mi_rows = 2 * ((h.height as usize + 7) >> 3);
    if h.allow_screen_content_tools && h.upscaled_width == h.width {
        h.allow_intrabc = b.flag()?;
    }
    h.disable_frame_end_update_cdf = if seq.reduced_still_picture_header || h.disable_cdf_update { true } else { b.flag()? };
    tile_info(&mut b, seq, &mut h)?;
    quantization_params(&mut b, seq, &mut h)?;
    segmentation_params(&mut b, &mut h)?;
    // delta_q_params
    if h.base_q_idx > 0 {
        h.delta_q_present = b.flag()?;
    }
    if h.delta_q_present {
        h.delta_q_res = b.f(2)?;
    }
    // delta_lf_params
    if h.delta_q_present {
        if !h.allow_intrabc {
            h.delta_lf_present = b.flag()?;
        }
        if h.delta_lf_present {
            h.delta_lf_res = b.f(2)?;
            h.delta_lf_multi = b.flag()?;
        }
    }
    h.coded_lossless = true;
    for seg in 0..8 {
        let q = seg_qidx(&h, seg, true, 0);
        let l = q == 0 && h.delta_q_y_dc == 0 && h.delta_q_u_ac == 0 && h.delta_q_u_dc == 0 && h.delta_q_v_ac == 0 && h.delta_q_v_dc == 0;
        h.lossless[seg] = l;
        if !l {
            h.coded_lossless = false;
        }
    }
    h.all_lossless = h.coded_lossless && h.width == h.upscaled_width;
    loop_filter_params(&mut b, seq, &mut h)?;
    cdef_params(&mut b, seq, &mut h)?;
    lr_params(&mut b, seq, &mut h)?;
    // read_tx_mode
    h.tx_mode = if h.coded_lossless {
        0
    } else if b.flag()? {
        TX_MODE_SELECT
    } else {
        TX_MODE_LARGEST
    };
    h.reduced_tx_set = b.flag()?;
    if seq.film_grain_params_present && (h.show_frame || h.showable_frame) {
        film_grain_params(&mut b)?;
    }
    b.byte_align();
    h.header_bytes = b.byte_pos();
    Ok(h)
}

fn tile_info(b: &mut Bits, seq: &SequenceHeader, h: &mut FrameHeader) -> Result<()> {
    let sb_shift = if seq.use_128x128_superblock { 5 } else { 4 };
    let sb_cols = (h.mi_cols + (1 << sb_shift) - 1) >> sb_shift;
    let sb_rows = (h.mi_rows + (1 << sb_shift) - 1) >> sb_shift;
    let sb_size = sb_shift + 2;
    let max_tile_width_sb = 4096usize >> sb_size;
    let mut max_tile_area_sb = (4096 * 2304usize) >> (2 * sb_size);
    let min_log2_tile_cols = tile_log2(max_tile_width_sb, sb_cols);
    let max_log2_tile_cols = tile_log2(1, sb_cols.min(64));
    let max_log2_tile_rows = tile_log2(1, sb_rows.min(64));
    let min_log2_tiles = min_log2_tile_cols.max(tile_log2(max_tile_area_sb, sb_rows * sb_cols));
    h.mi_col_starts.clear();
    h.mi_row_starts.clear();
    if b.flag()? {
        h.tile_cols_log2 = min_log2_tile_cols;
        while h.tile_cols_log2 < max_log2_tile_cols && b.flag()? {
            h.tile_cols_log2 += 1;
        }
        let tile_width_sb = (sb_cols + (1 << h.tile_cols_log2) - 1) >> h.tile_cols_log2;
        let mut start = 0;
        while start < sb_cols {
            h.mi_col_starts.push(start << sb_shift);
            start += tile_width_sb;
        }
        h.tile_cols = h.mi_col_starts.len();
        let min_log2_tile_rows = (min_log2_tiles as i32 - h.tile_cols_log2 as i32).max(0) as u32;
        h.tile_rows_log2 = min_log2_tile_rows;
        while h.tile_rows_log2 < max_log2_tile_rows && b.flag()? {
            h.tile_rows_log2 += 1;
        }
        let tile_height_sb = (sb_rows + (1 << h.tile_rows_log2) - 1) >> h.tile_rows_log2;
        let mut start = 0;
        while start < sb_rows {
            h.mi_row_starts.push(start << sb_shift);
            start += tile_height_sb;
        }
        h.tile_rows = h.mi_row_starts.len();
    } else {
        let mut widest = 0;
        let mut start = 0;
        while start < sb_cols {
            h.mi_col_starts.push(start << sb_shift);
            let max_w = (sb_cols - start).min(max_tile_width_sb);
            let size = b.ns(max_w as u32)? as usize + 1;
            widest = widest.max(size);
            start += size;
        }
        h.tile_cols = h.mi_col_starts.len();
        h.tile_cols_log2 = tile_log2(1, h.tile_cols);
        if min_log2_tiles > 0 {
            max_tile_area_sb = (sb_rows * sb_cols) >> (min_log2_tiles + 1);
        } else {
            max_tile_area_sb = sb_rows * sb_cols;
        }
        let max_tile_height_sb = (max_tile_area_sb / widest).max(1);
        let mut start = 0;
        while start < sb_rows {
            h.mi_row_starts.push(start << sb_shift);
            let max_h = (sb_rows - start).min(max_tile_height_sb);
            let size = b.ns(max_h as u32)? as usize + 1;
            start += size;
        }
        h.tile_rows = h.mi_row_starts.len();
        h.tile_rows_log2 = tile_log2(1, h.tile_rows);
    }
    h.mi_col_starts.push(h.mi_cols);
    h.mi_row_starts.push(h.mi_rows);
    if h.tile_cols_log2 > 0 || h.tile_rows_log2 > 0 {
        h.context_update_tile_id = b.f(h.tile_rows_log2 + h.tile_cols_log2)? as usize;
        h.tile_size_bytes = b.f(2)? as usize + 1;
    }
    Ok(())
}

fn read_delta_q(b: &mut Bits) -> Result<i32> {
    if b.flag()? {
        b.su(7)
    } else {
        Ok(0)
    }
}

fn quantization_params(b: &mut Bits, seq: &SequenceHeader, h: &mut FrameHeader) -> Result<()> {
    h.base_q_idx = b.f(8)? as i32;
    h.delta_q_y_dc = read_delta_q(b)?;
    if seq.num_planes() > 1 {
        let diff_uv = if seq.separate_uv_delta_q { b.flag()? } else { false };
        h.delta_q_u_dc = read_delta_q(b)?;
        h.delta_q_u_ac = read_delta_q(b)?;
        if diff_uv {
            h.delta_q_v_dc = read_delta_q(b)?;
            h.delta_q_v_ac = read_delta_q(b)?;
        } else {
            h.delta_q_v_dc = h.delta_q_u_dc;
            h.delta_q_v_ac = h.delta_q_u_ac;
        }
    }
    h.using_qmatrix = b.flag()?;
    if h.using_qmatrix {
        h.qm_y = b.f(4)?;
        h.qm_u = b.f(4)?;
        h.qm_v = if seq.separate_uv_delta_q { b.f(4)? } else { h.qm_u };
    }
    Ok(())
}

fn segmentation_params(b: &mut Bits, h: &mut FrameHeader) -> Result<()> {
    h.seg_enabled = b.flag()?;
    if h.seg_enabled {
        let update_data = if h.primary_ref_frame == 7 {
            h.seg_update_map = true;
            h.seg_temporal_update = false;
            true
        } else {
            h.seg_update_map = b.flag()?;
            h.seg_temporal_update = if h.seg_update_map { b.flag()? } else { false };
            b.flag()?
        };
        if update_data {
            for i in 0..8 {
                for j in 0..8 {
                    let enabled = b.flag()?;
                    h.feature_enabled[i][j] = enabled;
                    let mut v = 0;
                    if enabled {
                        let bits = SEG_FEATURE_BITS[j];
                        let limit = SEG_FEATURE_MAX[j];
                        v = if SEG_FEATURE_SIGNED[j] { b.su(1 + bits)?.clamp(-limit, limit) } else { (b.f(bits)? as i32).clamp(0, limit) };
                    }
                    h.feature_data[i][j] = v;
                }
            }
        }
    } else {
        h.feature_enabled = [[false; 8]; 8];
        h.feature_data = [[0; 8]; 8];
    }
    h.seg_id_pre_skip = false;
    h.last_active_seg_id = 0;
    for i in 0..8 {
        for j in 0..8 {
            if h.feature_enabled[i][j] {
                h.last_active_seg_id = i as u8;
                if j >= 5 {
                    h.seg_id_pre_skip = true;
                }
            }
        }
    }
    Ok(())
}

/// The quantiser index of a segment (get_qidx): `ignore_delta` is the spec's ignoreDeltaQ and
/// `current` the CurrentQIndex used when delta quantisers are present.
pub fn seg_qidx(h: &FrameHeader, seg: usize, ignore_delta: bool, current: i32) -> i32 {
    let delta = !ignore_delta && h.delta_q_present;
    if h.seg_enabled && h.feature_enabled[seg][0] {
        let data = h.feature_data[seg][0];
        let q = if delta { current + data } else { h.base_q_idx + data };
        return q.clamp(0, 255);
    }
    if delta {
        current
    } else {
        h.base_q_idx
    }
}

fn loop_filter_params(b: &mut Bits, seq: &SequenceHeader, h: &mut FrameHeader) -> Result<()> {
    h.lf_ref_deltas = [1, 0, 0, 0, -1, 0, -1, -1];
    h.lf_mode_deltas = [0, 0];
    if h.coded_lossless || h.allow_intrabc {
        return Ok(());
    }
    h.lf_level[0] = b.f(6)? as u8;
    h.lf_level[1] = b.f(6)? as u8;
    if seq.num_planes() > 1 && (h.lf_level[0] != 0 || h.lf_level[1] != 0) {
        h.lf_level[2] = b.f(6)? as u8;
        h.lf_level[3] = b.f(6)? as u8;
    }
    h.lf_sharpness = b.f(3)? as u8;
    h.lf_delta_enabled = b.flag()?;
    if h.lf_delta_enabled && b.flag()? {
        for i in 0..8 {
            if b.flag()? {
                h.lf_ref_deltas[i] = b.su(7)?;
            }
        }
        for i in 0..2 {
            if b.flag()? {
                h.lf_mode_deltas[i] = b.su(7)?;
            }
        }
    }
    Ok(())
}

fn cdef_params(b: &mut Bits, seq: &SequenceHeader, h: &mut FrameHeader) -> Result<()> {
    if h.coded_lossless || h.allow_intrabc || !seq.enable_cdef {
        h.cdef_damping = 3;
        return Ok(());
    }
    h.cdef_damping = b.f(2)? + 3;
    h.cdef_bits = b.f(2)?;
    for i in 0..(1usize << h.cdef_bits) {
        h.cdef_y_pri[i] = b.f(4)? as u8;
        h.cdef_y_sec[i] = b.f(2)? as u8;
        if h.cdef_y_sec[i] == 3 {
            h.cdef_y_sec[i] += 1;
        }
        if seq.num_planes() > 1 {
            h.cdef_uv_pri[i] = b.f(4)? as u8;
            h.cdef_uv_sec[i] = b.f(2)? as u8;
            if h.cdef_uv_sec[i] == 3 {
                h.cdef_uv_sec[i] += 1;
            }
        }
    }
    Ok(())
}

fn lr_params(b: &mut Bits, seq: &SequenceHeader, h: &mut FrameHeader) -> Result<()> {
    h.lr_type = [0; 3];
    if h.all_lossless || h.allow_intrabc || !seq.enable_restoration {
        return Ok(());
    }
    // Remap_Lr_Type: 0 none, 1 switchable, 2 Wiener, 3 self-guided.
    const REMAP: [u8; 4] = [0, 3, 1, 2];
    let mut uses_chroma = false;
    for i in 0..seq.num_planes() {
        h.lr_type[i] = REMAP[b.f(2)? as usize];
        if h.lr_type[i] != 0 {
            h.uses_lr = true;
            if i > 0 {
                uses_chroma = true;
            }
        }
    }
    if h.uses_lr {
        let shift = if seq.use_128x128_superblock {
            b.f(1)? + 1
        } else {
            let s = b.f(1)?;
            if s != 0 {
                s + b.f(1)?
            } else {
                0
            }
        };
        h.lr_size[0] = 256 >> (2 - shift);
        let uv_shift = if seq.subsampling_x && seq.subsampling_y && uses_chroma { b.f(1)? } else { 0 };
        h.lr_size[1] = h.lr_size[0] >> uv_shift;
        h.lr_size[2] = h.lr_size[0] >> uv_shift;
    }
    Ok(())
}

/// film_grain_params: only its apply_grain flag is read; grain synthesis is not implemented.
fn film_grain_params(b: &mut Bits) -> Result<()> {
    if b.flag()? {
        return Err(unsupported("film grain synthesis is not implemented"));
    }
    Ok(())
}
