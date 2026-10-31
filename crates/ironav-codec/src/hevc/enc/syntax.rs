//! Parameter sets, slice segment headers, NAL unit framing and the residual coding syntax (ITU-T
//! H.265 7.3.2, 7.3.6, 7.3.8.11) of the encoder.

use super::super::slice::Scans;
use super::super::tables::*;
use super::cabac::CabacEnc;
use crate::h264::enc::bitw::BitWriter;

/// Sizes of the coding tree in this encoder: 32x32 CTBs, 8x8 to 32x32 coding units, 4x4 to 32x32 transforms.
pub const LOG2_CTB: u32 = 6;
pub const LOG2_MIN_CB: u32 = 3;
pub const LOG2_MAX_TB: u32 = 5;

/// Appends a NAL unit (Annex B, four-byte start code) of type `kind` with emulation prevention.
pub fn put_nal(out: &mut Vec<u8>, kind: u8, rbsp: &[u8]) {
    out.extend_from_slice(&[0, 0, 0, 1, kind << 1, 1]);
    let mut zeros = 0;
    for &b in rbsp {
        if zeros >= 2 && b <= 3 {
            out.push(3);
            zeros = 0;
        }
        out.push(b);
        zeros = if b == 0 { zeros + 1 } else { 0 };
    }
}

/// profile_tier_level(1, 0) for Main at `level` (general_level_idc).
fn ptl(w: &mut BitWriter, level: u8) {
    w.put(2, 0); // general_profile_space
    w.put(1, 0); // general_tier_flag
    w.put(5, 1); // general_profile_idc: Main
    w.put(32, 0x6000_0000); // compatibility flags 1 and 2
    w.put(4, 0b1001); // progressive, not interlaced, not packed, frame only
    w.put(32, 0);
    w.put(12, 0); // the 43 reserved bits and the one after them
    w.put(8, level as u32);
}

pub fn vps(level: u8) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.put(4, 0); // vps_video_parameter_set_id
    w.put(2, 3); // base layer internal and available
    w.put(6, 0); // vps_max_layers_minus1
    w.put(3, 0); // vps_max_sub_layers_minus1
    w.put(1, 1); // vps_temporal_id_nesting_flag
    w.put(16, 0xffff);
    ptl(&mut w, level);
    w.put(1, 1); // vps_sub_layer_ordering_info_present_flag
    w.ue(1); // vps_max_dec_pic_buffering_minus1
    w.ue(0); // vps_max_num_reorder_pics
    w.ue(0); // vps_max_latency_increase_plus1
    w.put(6, 0); // vps_max_layer_id
    w.ue(0); // vps_num_layer_sets_minus1
    w.put(1, 0); // vps_timing_info_present_flag
    w.put(1, 0); // vps_extension_flag
    w.finish()
}

/// The SPS for a `pw` x `ph` (padded to 8) picture cropped by `crop` (luma samples: left, right, top, bottom).
pub fn sps(pw: usize, ph: usize, crop: [usize; 4], level: u8) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.put(4, 0); // sps_video_parameter_set_id
    w.put(3, 0); // sps_max_sub_layers_minus1
    w.put(1, 1); // sps_temporal_id_nesting_flag
    ptl(&mut w, level);
    w.ue(0); // sps_seq_parameter_set_id
    w.ue(1); // chroma_format_idc: 4:2:0
    w.ue(pw as u32);
    w.ue(ph as u32);
    let cropped = crop.iter().any(|&c| c > 0);
    w.flag(cropped);
    if cropped {
        // the offsets are in chroma samples, two luma samples each
        for c in crop {
            w.ue((c / 2) as u32);
        }
    }
    w.ue(0); // bit_depth_luma_minus8
    w.ue(0); // bit_depth_chroma_minus8
    w.ue(4); // log2_max_pic_order_cnt_lsb_minus4: 8 bits
    w.flag(true); // sps_sub_layer_ordering_info_present_flag
    w.ue(1); // sps_max_dec_pic_buffering_minus1
    w.ue(0); // sps_max_num_reorder_pics
    w.ue(0); // sps_max_latency_increase_plus1
    w.ue(LOG2_MIN_CB - 3);
    w.ue(LOG2_CTB - LOG2_MIN_CB);
    w.ue(0); // log2_min_luma_transform_block_size_minus2
    w.ue(LOG2_MAX_TB - 2);
    w.ue(0); // max_transform_hierarchy_depth_inter
    w.ue(1); // max_transform_hierarchy_depth_intra
    w.flag(false); // scaling_list_enabled_flag
    w.flag(false); // amp_enabled_flag
    w.flag(true); // sample_adaptive_offset_enabled_flag
    w.flag(false); // pcm_enabled_flag
    w.ue(1); // num_short_term_ref_pic_sets: the previous picture
    w.ue(1); // num_negative_pics
    w.ue(0); // num_positive_pics
    w.ue(0); // delta_poc_s0_minus1
    w.flag(true); // used_by_curr_pic_s0_flag
    w.flag(false); // long_term_ref_pics_present_flag
    w.flag(false); // sps_temporal_mvp_enabled_flag
    w.flag(true); // strong_intra_smoothing_enabled_flag
    w.flag(false); // vui_parameters_present_flag
    w.flag(false); // sps_extension_present_flag
    w.finish()
}

pub fn pps() -> Vec<u8> {
    let mut w = BitWriter::new();
    w.ue(0); // pps_pic_parameter_set_id
    w.ue(0); // pps_seq_parameter_set_id
    w.flag(false); // dependent_slice_segments_enabled_flag
    w.flag(false); // output_flag_present_flag
    w.put(3, 0); // num_extra_slice_header_bits
    w.flag(false); // sign_data_hiding_enabled_flag
    w.flag(false); // cabac_init_present_flag
    w.ue(0); // num_ref_idx_l0_default_active_minus1
    w.ue(0); // num_ref_idx_l1_default_active_minus1
    w.se(0); // init_qp_minus26
    w.flag(false); // constrained_intra_pred_flag
    w.flag(false); // transform_skip_enabled_flag
    w.flag(false); // cu_qp_delta_enabled_flag
    w.se(0); // pps_cb_qp_offset
    w.se(0); // pps_cr_qp_offset
    w.flag(false); // pps_slice_chroma_qp_offsets_present_flag
    w.flag(false); // weighted_pred_flag
    w.flag(false); // weighted_bipred_flag
    w.flag(false); // transquant_bypass_enabled_flag
    w.flag(false); // tiles_enabled_flag
    w.flag(false); // entropy_coding_sync_enabled_flag
    w.flag(true); // pps_loop_filter_across_slices_enabled_flag
    w.flag(false); // deblocking_filter_control_present_flag: deblocking on, no offsets
    w.flag(false); // pps_scaling_list_data_present_flag
    w.flag(false); // lists_modification_present_flag
    w.ue(0); // log2_parallel_merge_level_minus2
    w.flag(false); // slice_segment_header_extension_present_flag
    w.flag(false); // pps_extension_present_flag
    w.finish()
}

/// Bits of `slice_segment_address` for a picture of `nctb` CTBs: Ceil(Log2(nctb)).
fn addr_bits(nctb: usize) -> u32 {
    usize::BITS - (nctb - 1).leading_zeros()
}

/// A slice segment header for a slice of an IDR picture or of a P picture that refers to the
/// picture before it (its POC is `poc` modulo 256), ending with byte_alignment().
pub fn slice_header(first: bool, addr: usize, nctb: usize, idr: bool, poc: u32, qp: i32) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.flag(first);
    if idr {
        w.flag(false); // no_output_of_prior_pics_flag
    }
    w.ue(0); // slice_pic_parameter_set_id
    if !first {
        w.put(addr_bits(nctb), addr as u32);
    }
    w.ue(if idr { 2 } else { 1 }); // slice_type: I or P
    if !idr {
        w.put(8, poc & 255); // slice_pic_order_cnt_lsb
        w.flag(true); // short_term_ref_pic_set_sps_flag; the one set takes no index bits
    }
    w.flag(true); // slice_sao_luma_flag
    w.flag(true); // slice_sao_chroma_flag: the CTBs choose their offsets
    if !idr {
        w.flag(false); // num_ref_idx_active_override_flag
        w.ue(0); // five_minus_max_num_merge_cand
    }
    w.se(qp - 26); // slice_qp_delta
    w.flag(true); // slice_loop_filter_across_slices_enabled_flag
    w.finish()
}

const CTX_IDX_MAP_4X4: [u8; 16] = [0, 1, 4, 5, 2, 3, 4, 5, 6, 6, 8, 8, 7, 7, 8, 0];

/// The context increment of sig_coeff_flag (9.3.4.2.5).
#[allow(clippy::too_many_arguments)]
fn sig_inc(log2: u32, luma: bool, scan: usize, xs: usize, ys: usize, pos: usize, xp: usize, yp: usize, prev: usize) -> usize {
    let sig = if log2 == 2 {
        CTX_IDX_MAP_4X4[(yp << 2) + xp] as usize
    } else if xs == 0 && ys == 0 && pos == 0 {
        0
    } else {
        let mut s = match prev {
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
                if scan == 0 {
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
    if luma {
        sig
    } else {
        27 + sig
    }
}

/// The prefix and suffix of a last significant coefficient coordinate `v`, and the suffix's length.
fn last_bin(v: u32, cmax: u32) -> (u32, u32, u32) {
    if v < 4 {
        return (v, 0, 0);
    }
    for p in 4..=cmax {
        let nb = (p >> 1) - 1;
        let base = (1 << nb) * (2 + (p & 1));
        if v >= base && v < base + (1 << nb) {
            return (p, v - base, nb);
        }
    }
    unreachable!("a coordinate of a block is within its last-position prefixes")
}

/// coeff_abs_level_remaining with Rice parameter `k`.
fn write_remaining(e: &mut CabacEnc, r: u32, k: u32) {
    if (r >> k) < 4 {
        for _ in 0..r >> k {
            e.bypass(1);
        }
        e.bypass(0);
        e.bypass_bits(r & ((1 << k) - 1), k);
        return;
    }
    let mut p = 4;
    loop {
        let pm3 = p - 3;
        let base = ((1u32 << pm3) + 2) << k;
        if r < base + (1 << (pm3 + k)) {
            for _ in 0..p {
                e.bypass(1);
            }
            e.bypass(0);
            e.bypass_bits(r - base, pm3 + k);
            return;
        }
        p += 1;
    }
}

/// residual_coding() of the `1 << log2` square of levels `lv` (row-major), with scan order `scan`
/// (0 diagonal, 1 horizontal, 2 vertical); sign data hiding and transform skip are off.
pub fn residual(e: &mut CabacEnc, lv: &[i32], log2: u32, scan: usize, luma: bool) {
    let n = 1usize << log2;
    let scans = Scans::get();
    let sb_scan = scans.scan(log2 - 2, scan);
    let pos_scan = scans.scan(2, scan);
    let nsb = 1usize << (log2 - 2);
    let at = |s: usize, p: usize| -> usize {
        let ((xs, ys), (xp, yp)) = (sb_scan[s], pos_scan[p]);
        ((ys as usize) * 4 + yp as usize) * n + xs as usize * 4 + xp as usize
    };
    let mut last = None;
    'find: for s in (0..sb_scan.len()).rev() {
        for p in (0..16).rev() {
            if lv[at(s, p)] != 0 {
                last = Some((s, p));
                break 'find;
            }
        }
    }
    let Some((last_sb, last_pos)) = last else {
        return;
    };
    let (lx, ly) = {
        let ((xs, ys), (xp, yp)) = (sb_scan[last_sb], pos_scan[last_pos]);
        (xs as u32 * 4 + xp as u32, ys as u32 * 4 + yp as u32)
    };
    // a vertical scan codes the coordinates swapped
    let (cx, cy) = if scan == 2 { (ly, lx) } else { (lx, ly) };
    let cmax = 2 * log2 - 1;
    let (ctx_off, ctx_shift) = if luma { (3 * (log2 as usize - 2) + ((log2 as usize - 1) >> 2), (log2 as usize + 1) >> 2) } else { (15, log2 as usize - 2) };
    let (px, sx, nbx) = last_bin(cx, cmax);
    let (py, sy, nby) = last_bin(cy, cmax);
    for (base, p) in [(LAST_SIGNIFICANT_COEFF_X_PREFIX_OFFSET, px), (LAST_SIGNIFICANT_COEFF_Y_PREFIX_OFFSET, py)] {
        for i in 0..p {
            e.decision(base + ctx_off + (i as usize >> ctx_shift), 1);
        }
        if p < cmax {
            e.decision(base + ctx_off + (p as usize >> ctx_shift), 0);
        }
    }
    if px > 3 {
        e.bypass_bits(sx, nbx);
    }
    if py > 3 {
        e.bypass_bits(sy, nby);
    }
    let mut csbf = vec![false; nsb * nsb];
    for s in 0..sb_scan.len() {
        let (xs, ys) = (sb_scan[s].0 as usize, sb_scan[s].1 as usize);
        csbf[ys * nsb + xs] = (0..16).any(|p| lv[at(s, p)] != 0);
    }
    let mut g1ctx = 1usize;
    for i in (0..=last_sb).rev() {
        let (xs, ys) = (sb_scan[i].0 as usize, sb_scan[i].1 as usize);
        let right = xs + 1 < nsb && csbf[ys * nsb + xs + 1];
        let below = ys + 1 < nsb && csbf[(ys + 1) * nsb + xs];
        let mut infer_dc = false;
        if i < last_sb && i > 0 {
            let inc = (right || below) as usize + if luma { 0 } else { 2 };
            e.decision(SIGNIFICANT_COEFF_GROUP_FLAG_OFFSET + inc, csbf[ys * nsb + xs] as u32);
            infer_dc = true;
            if !csbf[ys * nsb + xs] {
                continue;
            }
        }
        let prev = right as usize | ((below as usize) << 1);
        let start = if i == last_sb { last_pos as i32 - 1 } else { 15 };
        for pos in (0..=start).rev() {
            let pos = pos as usize;
            let v = lv[at(i, pos)] != 0;
            if pos > 0 || !infer_dc {
                let (xp, yp) = (pos_scan[pos].0 as usize, pos_scan[pos].1 as usize);
                let inc = sig_inc(log2, luma, scan, xs, ys, pos, xp, yp, prev);

                e.decision(SIGNIFICANT_COEFF_FLAG_OFFSET + inc, v as u32);
                if v {
                    infer_dc = false;
                }
            }
        }
        // the significant positions, highest first
        let sig: Vec<usize> = (0..16).rev().filter(|&p| lv[at(i, p)] != 0).collect();
        if sig.is_empty() {
            continue;
        }
        let nsig = sig.len();
        let abs = |k: usize| lv[at(i, sig[k])].unsigned_abs();
        let mut ctx_set = if i > 0 && luma { 2 } else { 0 };
        if i != last_sb && g1ctx == 0 {
            ctx_set += 1;
        }
        g1ctx = 1;
        let mut g1 = [false; 16];
        let mut first_g1 = None;
        for k in 0..nsig.min(8) {
            let f = abs(k) > 1;
            e.decision(COEFF_ABS_LEVEL_GREATER1_FLAG_OFFSET + ctx_set * 4 + g1ctx + if luma { 0 } else { 16 }, f as u32);
            g1[k] = f;
            if f {
                g1ctx = 0;
                first_g1.get_or_insert(k);
            } else if g1ctx > 0 && g1ctx < 3 {
                g1ctx += 1;
            }
        }
        let mut g2 = false;
        if let Some(k) = first_g1 {
            g2 = abs(k) > 2;
            e.decision(COEFF_ABS_LEVEL_GREATER2_FLAG_OFFSET + ctx_set + if luma { 0 } else { 4 }, g2 as u32);
        }
        for k in 0..nsig {
            e.bypass((lv[at(i, sig[k])] < 0) as u32);
        }
        let mut rice = 0u32;
        for k in 0..nsig {
            let a = abs(k);
            let mut base = 1 + g1[k] as u32;
            if first_g1 == Some(k) {
                base += g2 as u32;
            }
            let thresh = if k < 8 {
                if first_g1 == Some(k) {
                    3
                } else {
                    2
                }
            } else {
                1
            };
            if base == thresh {
                write_remaining(e, a - base, rice);
                if a > 3 << rice {
                    rice = (rice + 1).min(4);
                }
            }
        }
    }
}
