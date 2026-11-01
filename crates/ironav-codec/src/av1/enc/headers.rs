//! The OBUs of the encoder's stream: temporal delimiter, sequence header and the frame OBU of a
//! key or inter frame (spec 5.3 to 5.9), and the av1C record that carries the sequence header in a
//! container.

use crate::av1::superres::downscaled_width;

/// seq_level_idx: level 5.1, which covers every size this encoder takes.
const SEQ_LEVEL: u32 = 13;

pub const OBU_SEQUENCE_HEADER: u8 = 1;
pub const OBU_TEMPORAL_DELIMITER: u8 = 2;
pub const OBU_FRAME: u8 = 6;

/// MSB-first bit writer.
#[derive(Default)]
pub struct BitWriter {
    pub buf: Vec<u8>,
    nbits: usize,
}

impl BitWriter {
    fn bit(&mut self, b: u32) {
        if self.nbits.is_multiple_of(8) {
            self.buf.push(0);
        }
        if b != 0 {
            *self.buf.last_mut().unwrap() |= 0x80 >> (self.nbits % 8);
        }
        self.nbits += 1;
    }

    /// f(n): the low `n` bits of `v`, most significant first.
    fn put(&mut self, v: u32, n: u32) {
        for i in (0..n).rev() {
            self.bit((v >> i) & 1);
        }
    }

    fn byte_align(&mut self) {
        while !self.nbits.is_multiple_of(8) {
            self.bit(0);
        }
    }

    fn trailing_bits(&mut self) {
        self.bit(1);
        self.byte_align();
    }
}

/// The number of bits that code `v` (at least one).
fn bits_for(v: u32) -> u32 {
    (32 - v.leading_zeros()).max(1)
}

fn leb128(mut v: usize, out: &mut Vec<u8>) {
    loop {
        let b = (v & 0x7F) as u8;
        v >>= 7;
        if v == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// One OBU with its size field.
pub fn obu(kind: u8, payload: &[u8]) -> Vec<u8> {
    let mut out = vec![(kind << 3) | 0x02];
    leb128(payload.len(), &mut out);
    out.extend_from_slice(payload);
    out
}

/// The sequence header payload for 8-bit 4:2:0 profile 0 pictures of `width` x `height`: one
/// operating point, no reduced still picture header, no order hints (so no skip mode, compound
/// prediction or temporal motion), screen content tools off, CDEF on, loop restoration off, no
/// superres, no film grain.
pub fn sequence_header(width: u32, height: u32) -> Vec<u8> {
    sequence_header_with(width, height, false, false)
}

/// The sequence header payload of `sequence_header`, with superres (`enable_superres`) and loop
/// restoration (`enable_restoration`) allowed or not.
pub fn sequence_header_with(width: u32, height: u32, superres: bool, restoration: bool) -> Vec<u8> {
    let mut b = BitWriter::default();
    b.put(0, 3); // seq_profile
    b.put(0, 1); // still_picture
    b.put(0, 1); // reduced_still_picture_header
    b.put(0, 1); // timing_info_present_flag
    b.put(0, 1); // initial_display_delay_present_flag
    b.put(0, 5); // operating_points_cnt_minus_1
    b.put(0, 12); // operating_point_idc[0]
    b.put(SEQ_LEVEL, 5); // seq_level_idx[0]
    b.put(0, 1); // seq_tier[0] (levels above 7 carry it)
    let (wb, hb) = (bits_for(width - 1), bits_for(height - 1));
    b.put(wb - 1, 4);
    b.put(hb - 1, 4);
    b.put(width - 1, wb);
    b.put(height - 1, hb);
    b.put(0, 1); // frame_id_numbers_present_flag
    b.put(0, 1); // use_128x128_superblock
    b.put(0, 1); // enable_filter_intra
    b.put(1, 1); // enable_intra_edge_filter
    b.put(0, 1); // enable_interintra_compound
    b.put(0, 1); // enable_masked_compound
    b.put(0, 1); // enable_warped_motion
    b.put(0, 1); // enable_dual_filter
    b.put(0, 1); // enable_order_hint
    b.put(0, 1); // seq_choose_screen_content_tools
    b.put(0, 1); // seq_force_screen_content_tools (no integer MV syntax follows)
    b.put(superres as u32, 1); // enable_superres
    b.put(1, 1); // enable_cdef
    b.put(restoration as u32, 1); // enable_restoration
    // color_config: 8 bits, not monochrome, no colour description, limited range, 4:2:0
    b.put(0, 1); // high_bitdepth
    b.put(0, 1); // mono_chrome
    b.put(0, 1); // color_description_present_flag
    b.put(0, 1); // color_range
    b.put(0, 2); // chroma_sample_position
    b.put(0, 1); // separate_uv_delta_q
    b.put(0, 1); // film_grain_params_present
    b.trailing_bits();
    b.buf
}

/// The tile layout of a single tile: the uniform spacing syntax with every increment left at zero.
/// Frames the encoder cannot code in one tile (more superblocks than the minimum tile count allows)
/// are rejected by the caller, so the minimum log2 tile counts are zero here.
fn tile_info(b: &mut BitWriter, mi_cols: usize, mi_rows: usize) {
    let sb_cols = mi_cols.div_ceil(16);
    let sb_rows = mi_rows.div_ceil(16);
    let tile_log2 = |blk: usize, target: usize| {
        let mut k = 0;
        while (blk << k) < target {
            k += 1;
        }
        k
    };
    b.put(1, 1); // uniform_tile_spacing_flag
    if tile_log2(1, sb_cols.min(64)) > 0 {
        b.put(0, 1); // increment_tile_cols_log2
    }
    if tile_log2(1, sb_rows.min(64)) > 0 {
        b.put(0, 1); // increment_tile_rows_log2
    }
}

/// What a frame's header says about the frame: its type, size, quantiser and filter strengths.
/// Every frame has one tile, default CDFs (primary_ref_frame NONE), TX_MODE_LARGEST, the regular
/// 8-tap interpolation filter, and single references (reference_select off).
#[derive(Clone, Copy, Debug)]
pub struct FrameParams {
    pub key: bool,
    pub width: u32,
    pub height: u32,
    pub base_q_idx: u32,
    /// The deblocking level of the luma edges (chroma takes the same level).
    pub lf_level: u8,
    /// The CDEF strengths of the one CDEF strength set: primary and secondary, luma and chroma.
    pub cdef: [u8; 4],
    /// The superres denominator (9 to 16, or 8 for none) when the sequence header allows superres;
    /// `None` when it does not, and then no superres syntax is written.
    pub superres: Option<u32>,
    /// Whether the sequence header allows loop restoration, and then whether the frame uses it: the
    /// Wiener filter on every plane, with 64-sample luma units and 32-sample chroma units.
    pub restoration: bool,
    /// The reference slots of LAST_FRAME, LAST2_FRAME and LAST3_FRAME (the other four references
    /// name the slot of LAST_FRAME), and the slots the frame refreshes (inter frames only).
    pub ref_slots: [u8; 3],
    pub refresh: u8,
    /// Whether inter frames may code compound blocks (reference_select).
    pub reference_select: bool,
}

/// The uncompressed header of a shown frame, byte aligned, as the frame OBU starts with it. A key
/// frame refreshes every slot; an inter frame refreshes the slots `p.refresh` names, and reads its
/// LAST, LAST2 and LAST3 references from `p.ref_slots`. `p.width` is the upscaled width: with superres
/// the frame is coded at the downscaled width that `superres_params` derives from it.
pub fn frame_header(p: &FrameParams) -> Vec<u8> {
    let coded_width = match p.superres {
        Some(d) => downscaled_width(p.width, d),
        None => p.width,
    };
    let (mi_cols, mi_rows) = (2 * (coded_width as usize).div_ceil(8), 2 * (p.height as usize).div_ceil(8));
    let mut b = BitWriter::default();
    b.put(0, 1); // show_existing_frame
    b.put(if p.key { 0 } else { 1 }, 2); // frame_type: KEY_FRAME or INTER_FRAME
    b.put(1, 1); // show_frame
    if !p.key {
        b.put(0, 1); // error_resilient_mode
    }
    b.put(0, 1); // disable_cdf_update
    b.put(0, 1); // frame_size_override_flag: the size is the sequence's maximum
    if !p.key {
        b.put(7, 3); // primary_ref_frame: none, so the CDFs are the defaults
        b.put(p.refresh as u32, 8); // refresh_frame_flags
        for i in 0..7 {
            // LAST_FRAME, LAST2_FRAME and LAST3_FRAME from their slots; the other references name LAST_FRAME's
            let slot = if i < 3 { p.ref_slots[i] } else { p.ref_slots[0] };
            b.put(slot as u32, 3); // ref_frame_idx
        }
    }
    if let Some(denom) = p.superres {
        let on = denom != 8;
        b.put(on as u32, 1); // use_superres
        if on {
            b.put(denom - 9, 3); // coded_denom
        }
    }
    b.put(0, 1); // render_and_frame_size_different
    if !p.key {
        b.put(0, 1); // allow_high_precision_mv: quarter-sample motion
        b.put(0, 1); // is_filter_switchable
        b.put(0, 2); // interpolation_filter: EIGHTTAP (regular)
        b.put(0, 1); // is_motion_mode_switchable
    }
    b.put(1, 1); // disable_frame_end_update_cdf
    tile_info(&mut b, mi_cols, mi_rows);
    b.put(p.base_q_idx, 8);
    b.put(0, 1); // DeltaQYDc: delta_coded
    b.put(0, 1); // DeltaQUDc
    b.put(0, 1); // DeltaQUAc
    b.put(0, 1); // using_qmatrix
    b.put(0, 1); // segmentation_enabled
    b.put(0, 1); // delta_q_present (base_q_idx is above zero)
    let lf = p.lf_level as u32;
    b.put(lf, 6); // loop_filter_level[0]
    b.put(lf, 6); // loop_filter_level[1]
    if lf != 0 {
        b.put(lf, 6); // loop_filter_level[2]: U takes the luma level
        b.put(lf, 6); // loop_filter_level[3]: V takes the luma level
    }
    b.put(0, 3); // loop_filter_sharpness
    b.put(0, 1); // loop_filter_delta_enabled: the levels are not adjusted per reference
    // cdef_params: one strength set, so cdef_idx is never coded (cdef_bits 0)
    b.put(0, 2); // cdef_damping_minus_3
    b.put(0, 2); // cdef_bits
    let sec = |s: u8| -> u32 {
        match s {
            4 => 3,
            s => s as u32,
        }
    };
    b.put(p.cdef[0] as u32, 4); // cdef_y_pri_strength[0]
    b.put(sec(p.cdef[1]), 2); // cdef_y_sec_strength[0]
    b.put(p.cdef[2] as u32, 4); // cdef_uv_pri_strength[0]
    b.put(sec(p.cdef[3]), 2); // cdef_uv_sec_strength[0]
    if p.restoration {
        // lr_params: lr_type 2 (Remap_Lr_Type gives WIENER) on each plane, lr_unit_shift 0 (64
        // samples) and lr_uv_shift 1 (32 chroma samples)
        for _ in 0..3 {
            b.put(2, 2); // lr_type
        }
        b.put(0, 1); // lr_unit_shift
        b.put(1, 1); // lr_uv_shift
    }
    b.put(0, 1); // read_tx_mode: TX_MODE_LARGEST
    if !p.key {
        b.put(p.reference_select as u32, 1); // reference_select
    }
    b.put(0, 1); // reduced_tx_set
    if !p.key {
        for _ in 0..7 {
            b.put(0, 1); // is_global: identity motion
        }
    }
    b.byte_align();
    b.buf
}

/// The av1C configuration record for a stream of this encoder: the sequence header as its config OBU.
pub fn config_record(width: u32, height: u32) -> Vec<u8> {
    let mut out = vec![
        0x81,            // marker and version 1
        SEQ_LEVEL as u8, // seq_profile 0, seq_level_idx_0
        0x0C,            // seq_tier 0, 8 bits, 4:2:0 (subsampling x and y set), no chroma position
        0x00,            // no initial presentation delay
    ];
    out.extend(obu(OBU_SEQUENCE_HEADER, &sequence_header(width, height)));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::av1::obu::{parse_sequence_header, split_obus, Bits};

    #[test]
    fn sequence_header_parses_back() {
        let payload = sequence_header(177, 99);
        let s = parse_sequence_header(&payload).unwrap();
        assert_eq!((s.max_frame_width, s.max_frame_height), (177, 99));
        assert_eq!((s.bit_depth, s.subsampling_x, s.subsampling_y), (8, true, true));
        assert!(!s.reduced_still_picture_header && s.enable_cdef && !s.enable_restoration);
    }

    #[test]
    fn frame_headers_read_back() {
        let p = FrameParams { key: true, width: 352, height: 288, base_q_idx: 115, lf_level: 10, cdef: [3, 1, 1, 0], superres: None, restoration: false, ref_slots: [0; 3], refresh: 0xff, reference_select: false };
        let hdr = frame_header(&p);
        let mut b = Bits::new(&hdr);
        assert_eq!(b.f(1).unwrap(), 0); // show_existing_frame
        assert_eq!(b.f(2).unwrap(), 0); // KEY_FRAME
        assert_eq!(b.f(1).unwrap(), 1); // show_frame
        let record = config_record(352, 288);
        let obus = split_obus(&record[4..]).unwrap();
        assert_eq!(obus[0].kind, OBU_SEQUENCE_HEADER);
    }
}
