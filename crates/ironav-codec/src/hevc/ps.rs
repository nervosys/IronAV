//! HEVC parameter sets and slice segment headers (ITU-T H.265 7.3).

use crate::h264::bits::BitReader;
use ironav_core::{Error, Result};

fn bad(msg: &str) -> Error {
    Error::invalid_input(format!("HEVC: {msg}"))
}

fn ceil_log2(v: u32) -> u32 {
    if v <= 1 {
        0
    } else {
        32 - (v - 1).leading_zeros()
    }
}

/// A short-term reference picture set (7.3.7 / 7.4.8): POC deltas and whether the current picture uses each.
#[derive(Clone, Default, Debug)]
pub struct StRps {
    pub delta_poc_s0: Vec<i32>,
    pub used_s0: Vec<bool>,
    pub delta_poc_s1: Vec<i32>,
    pub used_s1: Vec<bool>,
}

impl StRps {
    pub fn num_delta_pocs(&self) -> usize {
        self.delta_poc_s0.len() + self.delta_poc_s1.len()
    }

    /// Entries the current picture refers to.
    pub fn num_used(&self) -> usize {
        self.used_s0.iter().chain(&self.used_s1).filter(|&&u| u).count()
    }
}

fn parse_st_rps(r: &mut BitReader, idx: usize, num_sets: usize, sets: &[StRps]) -> Result<StRps> {
    let inter = if idx != 0 { r.flag()? } else { false };
    let mut out = StRps::default();
    if inter {
        let delta_idx = if idx == num_sets { r.ue()? as usize + 1 } else { 1 };
        if delta_idx > idx {
            return Err(bad("short-term RPS predicts from a set that does not exist"));
        }
        let rf = &sets[idx - delta_idx];
        let sign = r.bit()? as i32;
        let abs = r.ue()? as i32 + 1;
        let delta_rps = (1 - 2 * sign) * abs;
        let n = rf.num_delta_pocs();
        let mut used = vec![false; n + 1];
        let mut use_delta = vec![true; n + 1];
        for j in 0..=n {
            used[j] = r.flag()?;
            if !used[j] {
                use_delta[j] = r.flag()?;
            }
        }
        let (nneg, npos) = (rf.delta_poc_s0.len(), rf.delta_poc_s1.len());
        for j in (0..npos).rev() {
            let d = rf.delta_poc_s1[j] + delta_rps;
            if d < 0 && use_delta[nneg + j] {
                out.delta_poc_s0.push(d);
                out.used_s0.push(used[nneg + j]);
            }
        }
        if delta_rps < 0 && use_delta[n] {
            out.delta_poc_s0.push(delta_rps);
            out.used_s0.push(used[n]);
        }
        for j in 0..nneg {
            let d = rf.delta_poc_s0[j] + delta_rps;
            if d < 0 && use_delta[j] {
                out.delta_poc_s0.push(d);
                out.used_s0.push(used[j]);
            }
        }
        for j in (0..nneg).rev() {
            let d = rf.delta_poc_s0[j] + delta_rps;
            if d > 0 && use_delta[j] {
                out.delta_poc_s1.push(d);
                out.used_s1.push(used[j]);
            }
        }
        if delta_rps > 0 && use_delta[n] {
            out.delta_poc_s1.push(delta_rps);
            out.used_s1.push(used[n]);
        }
        for j in 0..npos {
            let d = rf.delta_poc_s1[j] + delta_rps;
            if d > 0 && use_delta[nneg + j] {
                out.delta_poc_s1.push(d);
                out.used_s1.push(used[nneg + j]);
            }
        }
    } else {
        let (nneg, npos) = (r.ue()? as usize, r.ue()? as usize);
        if nneg > 16 || npos > 16 {
            return Err(bad("short-term RPS has too many pictures"));
        }
        let mut prev = 0;
        for _ in 0..nneg {
            prev -= r.ue()? as i32 + 1;
            out.delta_poc_s0.push(prev);
            out.used_s0.push(r.flag()?);
        }
        prev = 0;
        for _ in 0..npos {
            prev += r.ue()? as i32 + 1;
            out.delta_poc_s1.push(prev);
            out.used_s1.push(r.flag()?);
        }
    }
    Ok(out)
}

/// Scaling lists (7.3.4): `lists[size_id][matrix_id]` hold 16 or 64 coefficients in up-right diagonal order
/// and `dc[size_id - 2][matrix_id]` the DC value of the 16x16 and 32x32 lists.
#[derive(Clone, Debug)]
pub struct ScalingList {
    pub lists: [[[u8; 64]; 6]; 4],
    pub dc: [[u8; 6]; 2],
}

fn default_list(size_id: usize, matrix_id: usize) -> [u8; 64] {
    let mut l = [16u8; 64];
    if size_id > 0 {
        // the default matrices are stored row by row; lists run along the up-right diagonal scan
        let m = if matrix_id < 3 { super::tables::DEFAULT_SCALING_LIST_INTRA } else { super::tables::DEFAULT_SCALING_LIST_INTER };
        for i in 0..64 {
            l[i] = m[super::tables::DIAG_SCAN8X8_Y[i] as usize * 8 + super::tables::DIAG_SCAN8X8_X[i] as usize];
        }
    }
    l
}

impl ScalingList {
    /// The default lists, used when `scaling_list_enabled_flag` is set without explicit data.
    pub fn default_lists() -> Self {
        let mut s = ScalingList { lists: [[[16; 64]; 6]; 4], dc: [[16; 6]; 2] };
        for size_id in 0..4 {
            for m in 0..6 {
                s.lists[size_id][m] = default_list(size_id, m);
            }
        }
        s
    }

    fn parse(r: &mut BitReader) -> Result<Self> {
        let mut s = Self::default_lists();
        for size_id in 0..4usize {
            let step = if size_id == 3 { 3 } else { 1 };
            let mut m = 0;
            while m < 6 {
                let pred_mode = r.flag()?;
                if !pred_mode {
                    let delta = r.ue()? as usize * step;
                    if delta > m {
                        return Err(bad("scaling list refers to a matrix that does not exist"));
                    }
                    if delta == 0 {
                        s.lists[size_id][m] = default_list(size_id, m);
                        if size_id > 1 {
                            s.dc[size_id - 2][m] = 16;
                        }
                    } else {
                        s.lists[size_id][m] = s.lists[size_id][m - delta];
                        if size_id > 1 {
                            s.dc[size_id - 2][m] = s.dc[size_id - 2][m - delta];
                        }
                    }
                } else {
                    let n = 64.min(1 << (4 + (size_id << 1)));
                    let mut next = 8i32;
                    if size_id > 1 {
                        next = r.se()? + 8;
                        s.dc[size_id - 2][m] = next as u8;
                    }
                    for i in 0..n {
                        next = (next + r.se()? + 256) & 255;
                        s.lists[size_id][m][i] = next as u8;
                    }
                }
                m += step;
            }
        }
        // 32x32 chroma lists (4:4:4 only) derive from the 16x16 ones
        for m in [1, 2, 4, 5] {
            s.lists[3][m] = s.lists[2][m];
            s.dc[1][m] = s.dc[0][m];
        }
        Ok(s)
    }
}

/// Reads past hrd_parameters() (E.2.2).
fn skip_hrd(r: &mut BitReader, max_sub_layers_minus1: u32) -> Result<()> {
    let (nal, vcl) = (r.flag()?, r.flag()?);
    let mut sub_pic = false;
    if nal || vcl {
        sub_pic = r.flag()?;
        if sub_pic {
            r.skip(19); // tick_divisor_minus2, du_cpb_removal_delay_increment_length_minus1, sub_pic_cpb_params_in_pic_timing_sei_flag, dpb_output_delay_du_length_minus1
        }
        r.skip(8); // bit_rate_scale, cpb_size_scale
        if sub_pic {
            r.skip(4);
        }
        r.skip(15); // the three delay lengths
    }
    for _ in 0..=max_sub_layers_minus1 {
        let fixed_general = r.flag()?;
        let fixed_cvs = fixed_general || r.flag()?;
        let mut low_delay = false;
        if fixed_cvs {
            r.ue()?; // elemental_duration_in_tc_minus1
        } else {
            low_delay = r.flag()?;
        }
        let cpb = if low_delay { 1 } else { r.ue()? + 1 };
        if cpb > 32 {
            return Err(bad("too many CPB specifications"));
        }
        for _ in 0..(nal as u32 + vcl as u32) * cpb {
            r.ue()?; // bit_rate_value_minus1
            r.ue()?; // cpb_size_value_minus1
            if sub_pic {
                r.ue()?;
                r.ue()?;
            }
            r.skip(1); // cbr_flag
        }
    }
    Ok(())
}

/// Reads past vui_parameters() (E.2.1): nothing in it changes decoding.
fn skip_vui(r: &mut BitReader, max_sub_layers_minus1: u32) -> Result<()> {
    if r.flag()? && r.bits(8)? == 255 {
        r.skip(32); // sar_width, sar_height
    }
    if r.flag()? {
        r.skip(1); // overscan_appropriate_flag
    }
    if r.flag()? {
        r.skip(4); // video_format, video_full_range_flag
        if r.flag()? {
            r.skip(24); // colour_primaries, transfer_characteristics, matrix_coeffs
        }
    }
    if r.flag()? {
        r.ue()?; // chroma_sample_loc_type_top_field
        r.ue()?;
    }
    r.skip(3); // neutral_chroma_indication_flag, field_seq_flag, frame_field_info_present_flag
    if r.flag()? {
        for _ in 0..4 {
            r.ue()?; // default display window offsets
        }
    }
    if r.flag()? {
        r.skip(64); // vui_num_units_in_tick, vui_time_scale
        if r.flag()? {
            r.ue()?; // vui_num_ticks_poc_diff_one_minus1
        }
        if r.flag()? {
            skip_hrd(r, max_sub_layers_minus1)?;
        }
    }
    if r.flag()? {
        r.skip(3); // tiles_fixed_structure_flag, motion_vectors_over_pic_boundaries_flag, restricted_ref_pic_lists_flag
        for _ in 0..5 {
            r.ue()?;
        }
    }
    if r.bits_left() == 0 {
        return Err(bad("the VUI runs to the end of the SPS"));
    }
    Ok(())
}

/// The first range extension coding tool an SPS turns on (7.3.2.2.2), if any.
fn sps_range_tool(r: &mut BitReader) -> Result<Option<&'static str>> {
    const TOOLS: [&str; 9] = [
        "transform skip rotation",
        "transform skip context",
        "implicit RDPCM",
        "explicit RDPCM",
        "extended precision processing",
        "disabled intra smoothing",
        "high precision weighted prediction offsets",
        "persistent Rice adaptation",
        "CABAC bypass alignment",
    ];
    let mut first = None;
    for t in TOOLS {
        if r.flag()? && first.is_none() {
            first = Some(t);
        }
    }
    Ok(first)
}

fn skip_ptl(r: &mut BitReader, max_sub_layers_minus1: u32) -> Result<()> {
    r.skip(96);
    let mut prof = [false; 8];
    let mut lev = [false; 8];
    for i in 0..max_sub_layers_minus1 as usize {
        prof[i] = r.flag()?;
        lev[i] = r.flag()?;
    }
    if max_sub_layers_minus1 > 0 {
        for _ in max_sub_layers_minus1..8 {
            r.skip(2);
        }
    }
    for i in 0..max_sub_layers_minus1 as usize {
        if prof[i] {
            r.skip(88);
        }
        if lev[i] {
            r.skip(8);
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
pub struct Sps {
    pub id: usize,
    pub chroma_format_idc: u32,
    pub width: usize,
    pub height: usize,
    /// Conformance window: left, right, top, bottom, in luma samples.
    pub crop: [usize; 4],
    pub bit_depth_luma: u32,
    pub bit_depth_chroma: u32,
    pub log2_max_poc_lsb: u32,
    pub max_dec_pic_buffering: u32,
    pub max_num_reorder: u32,
    pub max_latency_increase_plus1: u32,
    pub log2_min_cb: u32,
    pub log2_ctb: u32,
    pub log2_min_tb: u32,
    pub log2_max_tb: u32,
    pub max_th_depth_inter: u32,
    pub max_th_depth_intra: u32,
    pub scaling_list_enabled: bool,
    pub scaling_list: Option<ScalingList>,
    pub amp: bool,
    pub sao: bool,
    pub pcm: bool,
    pub pcm_bit_depth_luma: u32,
    pub pcm_bit_depth_chroma: u32,
    pub log2_min_pcm_cb: u32,
    pub log2_max_pcm_cb: u32,
    pub pcm_loop_filter_disabled: bool,
    pub st_rps: Vec<StRps>,
    pub long_term_present: bool,
    pub lt_ref_pic_poc_lsb: Vec<u32>,
    pub lt_used_by_curr: Vec<bool>,
    pub temporal_mvp: bool,
    pub strong_intra_smoothing: bool,
    pub ctb_w: usize,
    pub ctb_h: usize,
    /// A range extension coding tool the SPS turns on, which this decoder does not implement.
    pub range_tool: Option<&'static str>,
}

impl Sps {
    /// Horizontal and vertical chroma subsampling, as shifts.
    pub fn chroma_shift(&self) -> (usize, usize) {
        match self.chroma_format_idc {
            2 => (1, 0),
            3 => (0, 0),
            _ => (1, 1),
        }
    }
}

pub fn parse_sps(rbsp: &[u8]) -> Result<Sps> {
    let mut r = BitReader::new(rbsp);
    r.skip(4);
    let max_sub_layers_minus1 = r.bits(3)?;
    r.skip(1);
    skip_ptl(&mut r, max_sub_layers_minus1)?;
    let id = r.ue()? as usize;
    if id > 15 {
        return Err(bad("SPS id is out of range"));
    }
    let chroma_format_idc = r.ue()?;
    if chroma_format_idc > 3 {
        return Err(bad("chroma_format_idc is out of range"));
    }
    if chroma_format_idc == 3 && r.flag()? {
        return Err(bad("separate colour planes are not supported"));
    }
    let width = r.ue()? as usize;
    let height = r.ue()? as usize;
    if width == 0 || height == 0 || width > 16384 || height > 16384 {
        return Err(bad("picture size is out of range"));
    }
    let mut crop = [0usize; 4];
    if r.flag()? {
        let (sw, sh) = match chroma_format_idc {
            1 => (2, 2),
            2 => (2, 1),
            _ => (1, 1),
        };
        crop = [r.ue()? as usize * sw, r.ue()? as usize * sw, r.ue()? as usize * sh, r.ue()? as usize * sh];
    }
    let bit_depth_luma = r.ue()? + 8;
    let bit_depth_chroma = r.ue()? + 8;
    if bit_depth_luma > 16 || bit_depth_chroma > 16 {
        return Err(bad("bit depth is out of range"));
    }
    let log2_max_poc_lsb = r.ue()? + 4;
    if log2_max_poc_lsb > 16 {
        return Err(bad("log2_max_pic_order_cnt_lsb is out of range"));
    }
    let present = r.flag()?;
    let (mut mdpb, mut mreorder, mut mlat) = (0, 0, 0);
    for _ in if present { 0 } else { max_sub_layers_minus1 }..=max_sub_layers_minus1 {
        mdpb = r.ue()? + 1;
        mreorder = r.ue()?;
        mlat = r.ue()?;
    }
    let log2_min_cb = r.ue()? + 3;
    let log2_ctb = log2_min_cb + r.ue()?;
    let log2_min_tb = r.ue()? + 2;
    let log2_max_tb = log2_min_tb + r.ue()?;
    if !(4..=6).contains(&log2_ctb) || log2_max_tb > 5 || log2_min_tb >= log2_min_cb {
        return Err(bad("block size limits are out of range"));
    }
    let max_th_depth_inter = r.ue()?;
    let max_th_depth_intra = r.ue()?;
    let scaling_list_enabled = r.flag()?;
    let mut scaling_list = None;
    if scaling_list_enabled && r.flag()? {
        scaling_list = Some(ScalingList::parse(&mut r)?);
    }
    let amp = r.flag()?;
    let sao = r.flag()?;
    let pcm = r.flag()?;
    let (mut pbl, mut pbc, mut lminp, mut lmaxp, mut plf) = (0, 0, 0, 0, false);
    if pcm {
        pbl = r.bits(4)? + 1;
        pbc = r.bits(4)? + 1;
        lminp = r.ue()? + 3;
        lmaxp = lminp + r.ue()?;
        plf = r.flag()?;
    }
    let nsets = r.ue()? as usize;
    if nsets > 64 {
        return Err(bad("too many short-term reference picture sets"));
    }
    let mut st_rps: Vec<StRps> = Vec::with_capacity(nsets);
    for i in 0..nsets {
        let s = parse_st_rps(&mut r, i, nsets, &st_rps)?;
        st_rps.push(s);
    }
    let long_term_present = r.flag()?;
    let (mut lt_lsb, mut lt_used) = (Vec::new(), Vec::new());
    if long_term_present {
        let n = r.ue()?;
        if n > 32 {
            return Err(bad("too many long-term reference pictures"));
        }
        for _ in 0..n {
            lt_lsb.push(r.bits(log2_max_poc_lsb)?);
            lt_used.push(r.flag()?);
        }
    }
    let temporal_mvp = r.flag()?;
    let strong_intra_smoothing = r.flag()?;
    // a VUI that does not parse (some encoders write it wrongly) leaves the extensions unread
    let vui_ok = !r.flag()? || skip_vui(&mut r, max_sub_layers_minus1).is_ok();
    let mut range_tool = None;
    if vui_ok && r.flag().unwrap_or(false) {
        let range = r.flag()?;
        r.skip(7); // multilayer, 3D, SCC and the 4 reserved extension flags
        if range {
            range_tool = sps_range_tool(&mut r)?;
        }
    }
    let ctb = 1usize << log2_ctb;
    Ok(Sps {
        id,
        chroma_format_idc,
        width,
        height,
        crop,
        bit_depth_luma,
        bit_depth_chroma,
        log2_max_poc_lsb,
        max_dec_pic_buffering: mdpb,
        max_num_reorder: mreorder,
        max_latency_increase_plus1: mlat,
        log2_min_cb,
        log2_ctb,
        log2_min_tb,
        log2_max_tb,
        max_th_depth_inter,
        max_th_depth_intra,
        scaling_list_enabled,
        scaling_list,
        amp,
        sao,
        pcm,
        pcm_bit_depth_luma: pbl,
        pcm_bit_depth_chroma: pbc,
        log2_min_pcm_cb: lminp,
        log2_max_pcm_cb: lmaxp,
        pcm_loop_filter_disabled: plf,
        st_rps,
        long_term_present,
        lt_ref_pic_poc_lsb: lt_lsb,
        lt_used_by_curr: lt_used,
        temporal_mvp,
        strong_intra_smoothing,
        ctb_w: width.div_ceil(ctb),
        ctb_h: height.div_ceil(ctb),
        range_tool,
    })
}

#[derive(Clone, Debug)]
pub struct Pps {
    pub id: usize,
    pub sps_id: usize,
    pub dependent_slices: bool,
    pub output_flag_present: bool,
    pub extra_slice_header_bits: u32,
    pub sign_data_hiding: bool,
    pub cabac_init_present: bool,
    pub num_ref_idx_default: [u32; 2],
    pub init_qp: i32,
    pub constrained_intra_pred: bool,
    pub transform_skip: bool,
    pub cu_qp_delta_enabled: bool,
    pub diff_cu_qp_delta_depth: u32,
    pub cb_qp_offset: i32,
    pub cr_qp_offset: i32,
    pub slice_chroma_qp_offsets_present: bool,
    pub weighted_pred: bool,
    pub weighted_bipred: bool,
    pub transquant_bypass: bool,
    pub tiles: bool,
    pub entropy_sync: bool,
    pub col_widths_minus1: Vec<u32>,
    pub row_heights_minus1: Vec<u32>,
    pub uniform_spacing: bool,
    pub num_tile_cols: usize,
    pub num_tile_rows: usize,
    pub loop_filter_across_tiles: bool,
    pub loop_filter_across_slices: bool,
    pub deblocking_override_enabled: bool,
    pub deblocking_disabled: bool,
    pub beta_offset_div2: i32,
    pub tc_offset_div2: i32,
    pub scaling_list: Option<ScalingList>,
    pub lists_modification_present: bool,
    pub log2_parallel_merge_level: u32,
    pub slice_header_extension_present: bool,
    /// A range extension coding tool the PPS turns on, which this decoder does not implement.
    pub range_tool: Option<&'static str>,
}

pub fn parse_pps(rbsp: &[u8]) -> Result<Pps> {
    let mut r = BitReader::new(rbsp);
    let id = r.ue()? as usize;
    let sps_id = r.ue()? as usize;
    if id > 63 || sps_id > 15 {
        return Err(bad("PPS id is out of range"));
    }
    let dependent_slices = r.flag()?;
    let output_flag_present = r.flag()?;
    let extra_slice_header_bits = r.bits(3)?;
    let sign_data_hiding = r.flag()?;
    let cabac_init_present = r.flag()?;
    let n0 = r.ue()? + 1;
    let n1 = r.ue()? + 1;
    let init_qp = 26 + r.se()?;
    let constrained_intra_pred = r.flag()?;
    let transform_skip = r.flag()?;
    let cu_qp_delta_enabled = r.flag()?;
    let diff_cu_qp_delta_depth = if cu_qp_delta_enabled { r.ue()? } else { 0 };
    let cb_qp_offset = r.se()?;
    let cr_qp_offset = r.se()?;
    let slice_chroma_qp_offsets_present = r.flag()?;
    let weighted_pred = r.flag()?;
    let weighted_bipred = r.flag()?;
    let transquant_bypass = r.flag()?;
    let tiles = r.flag()?;
    let entropy_sync = r.flag()?;
    let (mut cw, mut rh) = (Vec::new(), Vec::new());
    let (mut ncols, mut nrows, mut uniform, mut lf_tiles) = (1usize, 1usize, true, true);
    if tiles {
        ncols = r.ue()? as usize + 1;
        nrows = r.ue()? as usize + 1;
        if ncols > 20 || nrows > 22 {
            return Err(bad("too many tiles"));
        }
        uniform = r.flag()?;
        if !uniform {
            for _ in 0..ncols - 1 {
                cw.push(r.ue()?);
            }
            for _ in 0..nrows - 1 {
                rh.push(r.ue()?);
            }
        }
        lf_tiles = r.flag()?;
    }
    let loop_filter_across_slices = r.flag()?;
    let (mut override_enabled, mut disabled, mut beta, mut tc) = (false, false, 0, 0);
    if r.flag()? {
        override_enabled = r.flag()?;
        disabled = r.flag()?;
        if !disabled {
            beta = r.se()?;
            tc = r.se()?;
        }
    }
    let scaling_list = if r.flag()? { Some(ScalingList::parse(&mut r)?) } else { None };
    let lists_modification_present = r.flag()?;
    let log2_parallel_merge_level = r.ue()? + 2;
    let slice_header_extension_present = r.flag()?;
    // pps_range_extension() (7.3.2.3.2)
    let mut range_tool = None;
    if r.flag().unwrap_or(false) {
        let range = r.flag()?;
        r.skip(7);
        if range {
            if transform_skip && r.ue()? != 0 {
                range_tool = Some("transform skip of blocks larger than 4x4");
            }
            if r.flag()? {
                range_tool = range_tool.or(Some("cross-component prediction"));
            }
            if r.flag()? {
                range_tool = range_tool.or(Some("chroma QP offset lists"));
            } else if r.ue()? != 0 || r.ue()? != 0 {
                range_tool = range_tool.or(Some("scaled SAO offsets"));
            }
        }
    }
    Ok(Pps {
        id,
        sps_id,
        dependent_slices,
        output_flag_present,
        extra_slice_header_bits,
        sign_data_hiding,
        cabac_init_present,
        num_ref_idx_default: [n0, n1],
        init_qp,
        constrained_intra_pred,
        transform_skip,
        cu_qp_delta_enabled,
        diff_cu_qp_delta_depth,
        cb_qp_offset,
        cr_qp_offset,
        slice_chroma_qp_offsets_present,
        weighted_pred,
        weighted_bipred,
        transquant_bypass,
        tiles,
        entropy_sync,
        col_widths_minus1: cw,
        row_heights_minus1: rh,
        uniform_spacing: uniform,
        num_tile_cols: ncols,
        num_tile_rows: nrows,
        loop_filter_across_tiles: lf_tiles,
        loop_filter_across_slices,
        deblocking_override_enabled: override_enabled,
        deblocking_disabled: disabled,
        beta_offset_div2: beta,
        tc_offset_div2: tc,
        scaling_list,
        lists_modification_present,
        log2_parallel_merge_level,
        slice_header_extension_present,
        range_tool,
    })
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SliceType {
    B,
    P,
    I,
}

/// Explicit weighted-prediction parameters of one reference picture (luma, Cb, Cr).
#[derive(Clone, Copy, Default, Debug)]
pub struct PredWeight {
    pub weight: [i32; 3],
    pub offset: [i32; 3],
}

#[derive(Clone, Debug, Default)]
pub struct PredWeightTable {
    pub luma_log2_denom: u32,
    pub chroma_log2_denom: u32,
    pub l: [Vec<PredWeight>; 2],
}

#[derive(Clone, Debug)]
pub struct SliceHeader {
    pub first_slice_segment: bool,
    pub no_output_of_prior_pics: bool,
    pub pps_id: usize,
    pub dependent: bool,
    pub segment_address: usize,
    pub slice_type: SliceType,
    pub pic_output: bool,
    pub poc_lsb: u32,
    pub st_rps: StRps,
    /// Long-term pictures: (POC LSB or full POC, whether the MSB is known, used by the current picture).
    pub lt: Vec<(i32, bool, bool)>,
    pub temporal_mvp: bool,
    pub sao_luma: bool,
    pub sao_chroma: bool,
    pub num_ref_idx: [usize; 2],
    pub list_mod: [Option<Vec<u32>>; 2],
    pub mvd_l1_zero: bool,
    pub cabac_init: bool,
    pub collocated_from_l0: bool,
    pub collocated_ref_idx: usize,
    pub pwt: Option<PredWeightTable>,
    pub max_num_merge_cand: usize,
    pub qp_delta: i32,
    pub cb_qp_offset: i32,
    pub cr_qp_offset: i32,
    pub deblocking_disabled: bool,
    pub beta_offset_div2: i32,
    pub tc_offset_div2: i32,
    pub loop_filter_across_slices: bool,
    /// Entry point offsets in bytes of the escaped slice segment data.
    pub entry_points: Vec<u32>,
    /// Offset in the unescaped RBSP where the slice segment data begins.
    pub data_offset: usize,
    pub nal_type: u8,
    pub temporal_id: u8,
    pub num_pic_total_curr: usize,
}

impl SliceHeader {
    pub fn idr(&self) -> bool {
        matches!(self.nal_type, 19 | 20)
    }

    pub fn irap(&self) -> bool {
        (16..=23).contains(&self.nal_type)
    }
}

fn parse_pwt(r: &mut BitReader, sps: &Sps, st: SliceType, n: [usize; 2]) -> Result<PredWeightTable> {
    let chroma = sps.chroma_format_idc != 0;
    let luma_log2_denom = r.ue()?;
    if luma_log2_denom > 7 {
        return Err(bad("luma_log2_weight_denom is out of range"));
    }
    let chroma_log2_denom = if chroma { (luma_log2_denom as i32 + r.se()?).clamp(0, 7) as u32 } else { 0 };
    let mut t = PredWeightTable { luma_log2_denom, chroma_log2_denom, l: [Vec::new(), Vec::new()] };
    let half = 128;
    for l in 0..if st == SliceType::B { 2 } else { 1 } {
        let lf: Vec<bool> = (0..n[l]).map(|_| r.flag()).collect::<Result<_>>()?;
        let cf: Vec<bool> = if chroma { (0..n[l]).map(|_| r.flag()).collect::<Result<_>>()? } else { vec![false; n[l]] };
        for i in 0..n[l] {
            let mut w = PredWeight { weight: [1 << luma_log2_denom, 1 << chroma_log2_denom, 1 << chroma_log2_denom], offset: [0; 3] };
            if lf[i] {
                w.weight[0] += r.se()?;
                w.offset[0] = r.se()?;
            }
            if cf[i] {
                for j in 1..3 {
                    w.weight[j] += r.se()?;
                    let d = r.se()?;
                    w.offset[j] = (half + d - ((half * w.weight[j]) >> chroma_log2_denom)).clamp(-half, half - 1);
                }
            }
            t.l[l].push(w);
        }
    }
    Ok(t)
}

/// Parses a slice segment header. A dependent slice segment inherits the fields it does not carry from `prev`.
pub fn parse_slice_header(rbsp: &[u8], nal_type: u8, temporal_id: u8, sps_tab: &[Option<std::sync::Arc<Sps>>], pps_tab: &[Option<std::sync::Arc<Pps>>], prev: Option<&SliceHeader>) -> Result<SliceHeader> {
    let mut r = BitReader::new(rbsp);
    let first = r.flag()?;
    let no_output_of_prior_pics = if (16..=23).contains(&nal_type) { r.flag()? } else { false };
    let pps_id = r.ue()? as usize;
    let pps = pps_tab.get(pps_id).and_then(|p| p.as_ref()).ok_or_else(|| bad("slice refers to a picture parameter set that was not sent"))?;
    let sps = sps_tab.get(pps.sps_id).and_then(|p| p.as_ref()).ok_or_else(|| bad("slice refers to a sequence parameter set that was not sent"))?;
    let mut dependent = false;
    let mut segment_address = 0;
    if !first {
        if pps.dependent_slices {
            dependent = r.flag()?;
        }
        segment_address = r.bits(ceil_log2((sps.ctb_w * sps.ctb_h) as u32))? as usize;
        if segment_address >= sps.ctb_w * sps.ctb_h {
            return Err(bad("slice segment address is outside the picture"));
        }
    }
    let mut h = if dependent {
        let p = prev.ok_or_else(|| bad("dependent slice segment without a preceding slice"))?;
        let mut h = p.clone();
        h.first_slice_segment = false;
        h.dependent = true;
        h.segment_address = segment_address;
        h.entry_points.clear();
        h
    } else {
        SliceHeader {
            first_slice_segment: first,
            no_output_of_prior_pics,
            pps_id,
            dependent: false,
            segment_address,
            slice_type: SliceType::I,
            pic_output: true,
            poc_lsb: 0,
            st_rps: StRps::default(),
            lt: Vec::new(),
            temporal_mvp: false,
            sao_luma: false,
            sao_chroma: false,
            num_ref_idx: [0, 0],
            list_mod: [None, None],
            mvd_l1_zero: false,
            cabac_init: false,
            collocated_from_l0: true,
            collocated_ref_idx: 0,
            pwt: None,
            max_num_merge_cand: 5,
            qp_delta: 0,
            cb_qp_offset: 0,
            cr_qp_offset: 0,
            deblocking_disabled: pps.deblocking_disabled,
            beta_offset_div2: pps.beta_offset_div2,
            tc_offset_div2: pps.tc_offset_div2,
            loop_filter_across_slices: pps.loop_filter_across_slices,
            entry_points: Vec::new(),
            data_offset: 0,
            nal_type,
            temporal_id,
            num_pic_total_curr: 0,
        }
    };
    h.nal_type = nal_type;
    h.temporal_id = temporal_id;
    h.no_output_of_prior_pics = no_output_of_prior_pics;
    if !dependent {
        r.skip(pps.extra_slice_header_bits as usize);
        h.slice_type = match r.ue()? {
            0 => SliceType::B,
            1 => SliceType::P,
            2 => SliceType::I,
            _ => return Err(bad("slice_type is out of range")),
        };
        if pps.output_flag_present {
            h.pic_output = r.flag()?;
        }
        if !matches!(nal_type, 19 | 20) {
            h.poc_lsb = r.bits(sps.log2_max_poc_lsb)?;
            let sps_flag = r.flag()?;
            if !sps_flag {
                h.st_rps = parse_st_rps(&mut r, sps.st_rps.len(), sps.st_rps.len(), &sps.st_rps)?;
            } else {
                let idx = if sps.st_rps.len() > 1 { r.bits(ceil_log2(sps.st_rps.len() as u32))? as usize } else { 0 };
                h.st_rps = sps.st_rps.get(idx).cloned().ok_or_else(|| bad("short-term RPS index is out of range"))?;
            }
            if sps.long_term_present {
                let nsps = sps.lt_ref_pic_poc_lsb.len();
                let n_lt_sps = if nsps > 0 { r.ue()? as usize } else { 0 };
                let n_lt_pics = r.ue()? as usize;
                if n_lt_sps + n_lt_pics > 32 {
                    return Err(bad("too many long-term pictures in a slice"));
                }
                let mut prev_msb = 0i32;
                for i in 0..n_lt_sps + n_lt_pics {
                    let (lsb, used) = if i < n_lt_sps {
                        let idx = if nsps > 1 { r.bits(ceil_log2(nsps as u32))? as usize } else { 0 };
                        if idx >= nsps {
                            return Err(bad("long-term picture index is out of range"));
                        }
                        (sps.lt_ref_pic_poc_lsb[idx], sps.lt_used_by_curr[idx])
                    } else {
                        (r.bits(sps.log2_max_poc_lsb)?, r.flag()?)
                    };
                    let msb_present = r.flag()?;
                    let mut cycle = if msb_present { r.ue()? as i32 } else { 0 };
                    if i != 0 && i != n_lt_sps {
                        cycle += prev_msb;
                    }
                    prev_msb = cycle;
                    // with the MSB known the entry is the POC relative to the current picture's MSB part;
                    // without, the LSB alone (the cycle still accumulates for the entries after it)
                    let poc = if msb_present { lsb as i32 - (cycle << sps.log2_max_poc_lsb) } else { lsb as i32 };
                    h.lt.push((poc, msb_present, used));
                }
            }
            if sps.temporal_mvp {
                h.temporal_mvp = r.flag()?;
            }
        }
        if sps.sao {
            h.sao_luma = r.flag()?;
            if sps.chroma_format_idc != 0 {
                h.sao_chroma = r.flag()?;
            }
        }
        h.num_pic_total_curr = h.st_rps.num_used() + h.lt.iter().filter(|l| l.2).count();
        if h.slice_type != SliceType::I {
            h.num_ref_idx = [pps.num_ref_idx_default[0] as usize, if h.slice_type == SliceType::B { pps.num_ref_idx_default[1] as usize } else { 0 }];
            if r.flag()? {
                h.num_ref_idx[0] = r.ue()? as usize + 1;
                if h.slice_type == SliceType::B {
                    h.num_ref_idx[1] = r.ue()? as usize + 1;
                }
            }
            if h.num_ref_idx[0] > 15 || h.num_ref_idx[1] > 15 {
                return Err(bad("too many active reference pictures"));
            }
            if h.num_pic_total_curr == 0 {
                return Err(bad("an inter slice has no reference pictures"));
            }
            if pps.lists_modification_present && h.num_pic_total_curr > 1 {
                let bitsn = ceil_log2(h.num_pic_total_curr as u32);
                for l in 0..if h.slice_type == SliceType::B { 2 } else { 1 } {
                    if r.flag()? {
                        let v: Vec<u32> = (0..h.num_ref_idx[l]).map(|_| r.bits(bitsn)).collect::<Result<_>>()?;
                        h.list_mod[l] = Some(v);
                    }
                }
            }
            if h.slice_type == SliceType::B {
                h.mvd_l1_zero = r.flag()?;
            }
            if pps.cabac_init_present {
                h.cabac_init = r.flag()?;
            }
            if h.temporal_mvp {
                if h.slice_type == SliceType::B {
                    h.collocated_from_l0 = r.flag()?;
                }
                let n = if h.collocated_from_l0 { h.num_ref_idx[0] } else { h.num_ref_idx[1] };
                if n > 1 {
                    h.collocated_ref_idx = r.ue()? as usize;
                    if h.collocated_ref_idx >= n {
                        return Err(bad("collocated_ref_idx is out of range"));
                    }
                }
            }
            if (pps.weighted_pred && h.slice_type == SliceType::P) || (pps.weighted_bipred && h.slice_type == SliceType::B) {
                h.pwt = Some(parse_pwt(&mut r, sps, h.slice_type, h.num_ref_idx)?);
            }
            let m = 5 - r.ue()? as i32;
            if !(1..=5).contains(&m) {
                return Err(bad("five_minus_max_num_merge_cand is out of range"));
            }
            h.max_num_merge_cand = m as usize;
        }
        h.qp_delta = r.se()?;
        if pps.slice_chroma_qp_offsets_present {
            h.cb_qp_offset = r.se()?;
            h.cr_qp_offset = r.se()?;
        }
        let mut over = false;
        if pps.deblocking_override_enabled {
            over = r.flag()?;
        }
        if over {
            h.deblocking_disabled = r.flag()?;
            if !h.deblocking_disabled {
                h.beta_offset_div2 = r.se()?;
                h.tc_offset_div2 = r.se()?;
            }
        }
        if pps.loop_filter_across_slices && (h.sao_luma || h.sao_chroma || !h.deblocking_disabled) {
            h.loop_filter_across_slices = r.flag()?;
        }
    }
    if pps.tiles || pps.entropy_sync {
        let n = r.ue()? as usize;
        if n > sps.ctb_w * sps.ctb_h {
            return Err(bad("too many entry points"));
        }
        if n > 0 {
            let len = r.ue()? + 1;
            if len > 32 {
                return Err(bad("entry point offsets are too long"));
            }
            for _ in 0..n {
                h.entry_points.push(r.bits(len)? + 1);
            }
        }
    }
    if pps.slice_header_extension_present {
        let n = r.ue()? as usize;
        r.skip(n * 8);
    }
    // byte_alignment(): a one bit, then zero bits up to the next byte boundary
    r.skip(1);
    r.align();
    h.data_offset = r.pos() / 8;
    if h.data_offset >= rbsp.len() {
        return Err(bad("slice segment has no data"));
    }
    Ok(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_extension_tools_are_named() {
        // the nine sps_range_extension() flags, the third (implicit RDPCM) and the eighth set
        let mut r = BitReader::new(&[0b0010_0001, 0b0000_0000]);
        assert_eq!(sps_range_tool(&mut r).unwrap(), Some("implicit RDPCM"));
        assert_eq!(r.pos(), 9);
        assert_eq!(sps_range_tool(&mut BitReader::new(&[0, 0])).unwrap(), None);
    }

    #[test]
    fn vui_with_timing_and_hrd_is_skipped_to_its_end() {
        // aspect ratio idc 1; no overscan; video signal (format 5, limited range) with colour description;
        // no chroma location; three zero flags; no display window; timing (1/25) with HRD (NAL, one CPB,
        // fixed rate); bitstream restriction with five zero codes; then a marker bit
        let mut bits = String::new();
        bits.push('1');
        bits.push_str("00000001");
        bits.push('0');
        bits.push('1');
        bits.push_str("1010");
        bits.push('1');
        bits.push_str("000000010000000100000001");
        bits.push('0');
        bits.push_str("000");
        bits.push('0');
        bits.push('1');
        bits.push_str(&format!("{:032b}{:032b}", 1, 25));
        bits.push('0');
        bits.push('1');
        bits.push_str("10"); // nal_hrd_parameters_present, vcl absent
        bits.push('0'); // sub_pic_hrd_params_present_flag
        bits.push_str("00000000"); // bit_rate_scale, cpb_size_scale
        bits.push_str("000000000000000"); // delay lengths
        bits.push('1'); // fixed_pic_rate_general_flag
        bits.push('1'); // elemental_duration_in_tc_minus1 = 0
        bits.push('1'); // cpb_cnt_minus1 = 0
        bits.push('1'); // bit_rate_value_minus1
        bits.push('1'); // cpb_size_value_minus1
        bits.push('0'); // cbr_flag
        bits.push('1');
        bits.push_str("000");
        bits.push_str("11111");
        let at = bits.len();
        bits.push('1');
        while !bits.len().is_multiple_of(8) {
            bits.push('0');
        }
        let bytes: Vec<u8> = bits.as_bytes().chunks(8).map(|c| u8::from_str_radix(std::str::from_utf8(c).unwrap(), 2).unwrap()).collect();
        let mut r = BitReader::new(&bytes);
        skip_vui(&mut r, 0).unwrap();
        assert_eq!(r.pos(), at);
    }
}
