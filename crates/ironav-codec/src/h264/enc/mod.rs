//! A software H.264 encoder: High profile with CABAC and the 8x8 transform (or Main without it, or
//! Constrained Baseline with CAVLC), 8-bit or 10-bit (High 10) 4:2:0 with even dimensions. Pictures are I and P, a P picture
//! predicting from up to three reference pictures (`with_refs`), and with CABAC two non-reference B
//! pictures between each pair of P pictures.
//! Macroblocks are coded as Intra4x4, Intra8x8, Intra16x16, P_L0_16x16, P_L0_L0_16x8,
//! P_L0_L0_8x16, P_8x8 (8x8 sub-macroblocks, split into 8x4, 4x8 or 4x4 with `with_sub8x8`, at QP 20 and below) or P_Skip, and in B pictures as B_Skip,
//! B_Direct_16x16 (spatial direct), or 16x16, 16x8, 8x16 or 8x8 (8x8 blocks possibly direct)
//! partitions each from list 0, list 1 or both; motion is searched to
//! quarter samples. Pictures are cut into slices of macroblock rows that are coded on separate
//! threads. Output is Annex B with SPS and PPS ahead of every IDR picture.

pub(crate) mod bitw;
mod cabac;
mod cavlc;
mod frame;
mod subpel;

use super::deblock::deblock_picture;
use super::mb::{Picture, SliceParams};
use bitw::{put_nal, BitWriter};
use frame::{FrameCoder, Source};
use subpel::HalfPel;
use crate::change::{change, LowRes, SCENE_CUT};
use ironav_core::{Encoder, Error, ErrorCode, Frame, Packet, PixelFormat, Rational, Result, StreamParams};

/// Pictures between IDR pictures.
const KEYINT: u64 = 250;
/// Bits per pixel at quantiser 26, roughly, for the rate control's first guess.
const BPP: f64 = 0.1;

/// The quantiser used when neither a quantiser nor a bit rate is set.
pub const DEFAULT_QP: u8 = 23;

/// Quantiser offsets of IDR, P and B pictures. B pictures go well above P ones: nothing predicts
/// from them, so their losses do not spread.
const QP_OFFSET: [i32; 3] = [-3, 0, 5];
/// B pictures between each pair of P pictures (with CABAC).
const BFRAMES: usize = 2;
/// Reference pictures a P picture may predict from by default (measured: `with_refs(3)` against
/// one saves 1 to 2 percent of the bytes on most settings, and raises the PSNR by up to 0.3 dB).
const REFS: usize = 3;

/// (level_idc, MaxFS in macroblocks, MaxMBPS) from Table A-1.
const LEVELS: [(u8, u32, u64); 19] = [
    (10, 99, 1485),
    (11, 396, 3000),
    (12, 396, 6000),
    (13, 396, 11880),
    (20, 396, 11880),
    (21, 792, 19800),
    (22, 1620, 20250),
    (30, 1620, 40500),
    (31, 3600, 108000),
    (32, 5120, 216000),
    (40, 8192, 245760),
    (41, 8192, 245760),
    (42, 8704, 522240),
    (50, 22080, 589824),
    (51, 36864, 983040),
    (52, 36864, 2073600),
    (60, 139264, 4177920),
    (61, 139264, 8355840),
    (62, 139264, 16711680),
];

pub struct H264Encoder {
    width: usize,
    height: usize,
    mb_w: usize,
    mb_h: usize,
    frame_rate: Rational,
    level: u8,
    /// A fixed quantiser, or a bit rate in bits per second to average.
    qp: Option<u8>,
    bitrate: Option<u32>,
    /// Rate control state: the quantiser for the next picture, and bits spent against bits allowed.
    rc_qp: f64,
    spent: f64,
    allowed: f64,
    /// The reference pictures as decoders reconstruct them, with their half samples and picture
    /// order counts, oldest first.
    refs: Vec<(Picture, HalfPel, i32)>,
    /// B pictures between the P pictures (with CABAC), and the pictures waiting to be coded as them.
    bframes: usize,
    /// The most reference pictures a P picture may predict from (and the stream keeps).
    nrefs: usize,
    /// Bits per sample: 8 (yuv420p), or 10 (yuv420p10le, High 10 profile).
    bd: u32,
    pending: Vec<(Source, i64, u64)>,
    /// Pictures between IDR pictures, and the seconds of bits the spending may run ahead or behind.
    keyint: u64,
    /// Leaves the reordering depth out of the VUI, as many cameras do.
    #[cfg(test)]
    no_reorder_info: bool,
    rc_window: f64,
    /// Display index of the last IDR picture.
    idr_at: u64,
    /// Set by `force_idr`: the next picture is coded as an IDR picture.
    force_idr: bool,
    /// The last picture's luma at a quarter of the size, to find scene cuts.
    last_low: Option<LowRes>,
    /// Every picture's reconstruction by its time stamp.
    #[cfg(test)]
    recon: Vec<(i64, [Vec<u16>; 3])>,
    /// Slices per picture, each coded on its own thread.
    slices: usize,
    /// CABAC (Main profile) rather than CAVLC (Constrained Baseline).
    cabac: bool,
    /// With CABAC, the 8x8 transform too (High profile).
    transform8: bool,
    /// P_8x8 macroblocks may use sub-partitions (see `with_sub8x8`).
    sub8x8: bool,
    count: u64,
    frame_num: u32,
    idr_id: u32,
}

impl H264Encoder {
    /// An encoder for `params` at a constant quantiser (`qp`, 0-51) or an average bit rate in bits per second.
    pub fn new(params: &StreamParams, qp: Option<u8>, bitrate: Option<u32>) -> Result<Self> {
        let StreamParams::Video { width, height, pix_fmt, frame_rate } = *params else {
            return Err(Error::invalid_arg("the H.264 encoder takes video"));
        };
        let bd = match pix_fmt {
            PixelFormat::Yuv420p => 8,
            PixelFormat::Yuv420p10 => 10,
            _ => {
                return Err(Error::new(ErrorCode::UnsupportedCodec, format!("the software H.264 encoder takes yuv420p or yuv420p10 frames, not {}", pix_fmt.name()))
                    .hint("add a format filter to convert to yuv420p or yuv420p10"));
            }
        };
        if width == 0 || height == 0 || width % 2 != 0 || height % 2 != 0 {
            return Err(Error::invalid_arg(format!("the software H.264 encoder needs even dimensions, not {width}x{height}")).hint("scale or crop to an even width and height"));
        }
        let (mb_w, mb_h) = (width.div_ceil(16) as usize, height.div_ceil(16) as usize);
        if mb_w > 1024 || mb_h > 1024 {
            return Err(Error::invalid_arg(format!("{width}x{height} is larger than H.264 allows")));
        }
        let fs = (mb_w * mb_h) as u32;
        let fps = if frame_rate.num > 0 && frame_rate.den > 0 { frame_rate.num as f64 / frame_rate.den as f64 } else { 25.0 };
        let mbps = (fs as f64 * fps).ceil() as u64;
        let level = LEVELS.iter().find(|l| l.1 >= fs && l.2 >= mbps).or(LEVELS.iter().find(|l| l.1 >= fs)).map_or(62, |l| l.0);
        Ok(H264Encoder {
            width: width as usize,
            height: height as usize,
            mb_w,
            mb_h,
            frame_rate,
            level,
            qp: if bitrate.is_none() { Some(qp.unwrap_or(DEFAULT_QP).min(51)) } else { None },
            bitrate,
            // a first guess from the bits per pixel the rate allows: about a tenth at 26, half as
            // many for every 7 added (it varies a lot with the content)
            rc_qp: bitrate.map_or(26.0, |r| (26.0 - 7.0 * (r as f64 / (fps * width as f64 * height as f64) / BPP).log2()).clamp(10.0, 45.0)),
            spent: 0.0,
            allowed: 0.0,
            refs: Vec::new(),
            bframes: BFRAMES,
            nrefs: REFS,
            bd,
            pending: Vec::new(),
            keyint: KEYINT,
            rc_window: 1.0,
            idr_at: 0,
            force_idr: false,
            last_low: None,
            #[cfg(test)]
            recon: Vec::new(),
            #[cfg(test)]
            no_reorder_info: false,
            // a slice per eight rows or so: more slices lose too much prediction across their edges
            slices: std::thread::available_parallelism().map_or(1, |n| n.get()).min((mb_h / 8).max(1)),
            cabac: true,
            transform8: true,
            sub8x8: false,
            count: 0,
            frame_num: 0,
            idr_id: 0,
        })
    }

    /// Sets the encoder up for live delivery: no B pictures, so each picture is sent as soon as it
    /// is coded and none waits for a later one (the SPS then declares no reordering, so decoders
    /// show each at once too); an IDR picture every second, so a viewer joining late waits at most
    /// that long; and spending held to a quarter of a second's bits, so no burst outruns a small
    /// receive buffer.
    pub fn low_latency(mut self) -> Self {
        self.bframes = 0;
        self.keyint = self.fps().round().max(1.0) as u64;
        self.rc_window = 0.25;
        self
    }

    /// IDR pictures every `n` frames (1 for IDR pictures only), in place of the default.
    pub fn with_keyint(mut self, n: u64) -> Self {
        self.keyint = n.max(1);
        self
    }

    /// The next picture is coded as an IDR picture, with the parameter sets ahead of it, whatever the
    /// GOP count says, and the count starts again from it. This is for a viewer that asked for a
    /// keyframe. With B pictures, the pictures still waiting for their P picture are coded first, as
    /// they are before any IDR picture.
    pub fn force_idr(&mut self) {
        self.force_idr = true;
    }

    /// P pictures may predict from up to `n` reference pictures (1 to 16, by default `REFS`), and
    /// their macroblocks pick one with `ref_idx`. The B pictures' two anchors are always kept.
    pub fn with_refs(mut self, n: usize) -> Self {
        self.nrefs = n.clamp(1, 16);
        self
    }

    /// Lets P_8x8 macroblocks split their 8x8 blocks into sub-partitions (8x4, 4x8, 4x4), chosen by
    /// rate-distortion cost, at QP 20 and below. Off by default: it saved up to 1.1 percent of the bytes
    /// on one 720p clip and 1.5 percent more on another, and slowed encoding by 8 to 30 percent.
    pub fn with_sub8x8(mut self, on: bool) -> Self {
        self.sub8x8 = on;
        self
    }

    /// The reference pictures the stream keeps: those a P picture may use, and the two anchors of
    /// B pictures.
    fn ref_count(&self) -> usize {
        self.nrefs.max(if self.b_frames() > 0 { 2 } else { 1 })
    }

    fn fps(&self) -> f64 {
        let r = self.frame_rate;
        if r.num > 0 && r.den > 0 { r.num as f64 / r.den as f64 } else { 25.0 }
    }

    fn sps(&self) -> Vec<u8> {
        let mut w = BitWriter::new();
        let high = (self.cabac && self.transform8) || self.bd > 8;
        if self.bd > 8 {
            w.put(8, 110); // High 10
            w.put(8, 0);
        } else if high {
            w.put(8, 100); // High
            w.put(8, 0);
        } else if self.cabac {
            w.put(8, 77); // Main
            w.put(8, 0b0100_0000); // constraint_set1: Main
        } else {
            w.put(8, 66); // Baseline
            w.put(8, 0b1100_0000); // constraint_set0 and constraint_set1: Constrained Baseline
        }
        w.put(8, self.level as u32);
        w.ue(0); // seq_parameter_set_id
        if high {
            w.ue(1); // chroma_format_idc: 4:2:0
            w.ue(self.bd - 8); // bit_depth_luma_minus8
            w.ue(self.bd - 8); // bit_depth_chroma_minus8
            w.flag(false); // qpprime_y_zero_transform_bypass_flag
            w.flag(false); // seq_scaling_matrix_present_flag
        }
        w.ue(4); // log2_max_frame_num_minus4: 8 bits
        if self.b_frames() > 0 {
            w.ue(0); // pic_order_cnt_type: counted in the slice headers
            w.ue(4); // log2_max_pic_order_cnt_lsb_minus4: 8 bits
        } else {
            w.ue(2); // pic_order_cnt_type: output order is decoding order
        }
        w.ue(self.ref_count() as u32); // max_num_ref_frames
        w.flag(false); // gaps_in_frame_num_value_allowed_flag
        w.ue(self.mb_w as u32 - 1);
        w.ue(self.mb_h as u32 - 1);
        w.flag(true); // frame_mbs_only_flag
        w.flag(true); // direct_8x8_inference_flag
        let (cr, cb) = ((self.mb_w * 16 - self.width) / 2, (self.mb_h * 16 - self.height) / 2);
        w.flag(cr > 0 || cb > 0);
        if cr > 0 || cb > 0 {
            w.ue(0);
            w.ue(cr as u32);
            w.ue(0);
            w.ue(cb as u32);
        }
        w.flag(true); // vui_parameters_present_flag
        w.flag(false); // aspect_ratio_info_present_flag
        w.flag(false); // overscan_info_present_flag
        w.flag(false); // video_signal_type_present_flag
        w.flag(false); // chroma_loc_info_present_flag
        let r = self.frame_rate;
        let timing = r.num > 0 && r.den > 0 && r.den <= u32::MAX as i64 && 2 * r.num <= u32::MAX as i64;
        w.flag(timing);
        if timing {
            w.put(32, r.den as u32); // num_units_in_tick
            w.put(32, 2 * r.num as u32); // time_scale: two ticks per frame
            w.flag(true); // fixed_frame_rate_flag
        }
        w.flag(false); // nal_hrd_parameters_present_flag
        w.flag(false); // vcl_hrd_parameters_present_flag
        w.flag(false); // pic_struct_present_flag
        #[cfg(not(test))]
        let restrict = true;
        #[cfg(test)]
        let restrict = !self.no_reorder_info;
        w.flag(restrict); // bitstream_restriction_flag
        if restrict {
            w.flag(true); // motion_vectors_over_pic_boundaries_flag
            w.ue(0); // max_bytes_per_pic_denom
            w.ue(0); // max_bits_per_mb_denom
            w.ue(15); // log2_max_mv_length_horizontal
            w.ue(15); // log2_max_mv_length_vertical
            // the P picture after B pictures is shown after them; B pictures are not kept
            let b = (self.b_frames() > 0) as u32;
            w.ue(b); // max_num_reorder_frames
            w.ue((1 + b).max(self.ref_count() as u32)); // max_dec_frame_buffering
        }
        w.finish()
    }

    fn pps(&self) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.ue(0); // pic_parameter_set_id
        w.ue(0); // seq_parameter_set_id
        w.flag(self.cabac); // entropy_coding_mode_flag
        w.flag(false); // bottom_field_pic_order_in_frame_present_flag
        w.ue(0); // num_slice_groups_minus1
        w.ue(0); // num_ref_idx_l0_default_active_minus1
        w.ue(0); // num_ref_idx_l1_default_active_minus1
        w.flag(false); // weighted_pred_flag
        w.put(2, 0); // weighted_bipred_idc
        w.se(0); // pic_init_qp_minus26
        w.se(0); // pic_init_qs_minus26
        w.se(0); // chroma_qp_index_offset
        w.flag(false); // deblocking_filter_control_present_flag
        w.flag(false); // constrained_intra_pred_flag
        w.flag(false); // redundant_pic_cnt_present_flag
        if self.cabac && self.transform8 {
            w.flag(true); // transform_8x8_mode_flag
            w.flag(false); // pic_scaling_matrix_present_flag
            w.se(0); // second_chroma_qp_index_offset
        }
        w.finish()
    }

    /// The quantiser for the next picture of `kind` (0 IDR, 1 P, 2 B). As in x264, IDR pictures are
    /// a little finer, since later pictures inherit their quality, and B pictures coarser.
    fn next_qp(&self, kind: u8) -> i32 {
        let off = QP_OFFSET[kind as usize];
        match self.qp {
            Some(q) => (q as i32 + off).clamp(0, 51),
            None => (self.rc_qp.round() as i32 + off).clamp(1, 51),
        }
    }

    /// Steers the quantiser so the bits spent follow the bits the rate allows. A picture's size
    /// roughly halves for every 6 added to the quantiser, which says what quantiser the last
    /// picture would have needed; the running surplus or deficit, against a second's worth of
    /// bits (a quarter of one at low latency), pulls the next one further.
    fn rate_update(&mut self, bits: usize, qp: i32, kind: u8) {
        let Some(rate) = self.bitrate else {
            return;
        };
        let per_frame = rate as f64 / self.fps();
        self.spent += bits as f64;
        self.allowed += per_frame;
        // a B picture's size relative to its P pictures varies too much with the content to say
        // what quantiser it wanted; it only counts towards the bits spent and follows the P pictures
        if kind == 2 {
            return;
        }
        let window = rate as f64 * self.rc_window;
        let budget = 6.0 * ((self.spent + window) / (self.allowed + window).max(1.0)).log2();
        // an IDR picture is expected to cost several P pictures, and a P picture more than one
        // picture's worth when B pictures, which cost much less, lie between them
        let share = match kind {
            0 => 4.0,
            _ => 1.0 + 0.25 * self.b_frames() as f64,
        };
        let expected = per_frame * share;
        let wanted = (qp - QP_OFFSET[kind as usize]) as f64 + 6.0 * (bits as f64 / expected).max(1e-3).log2();
        let next = 0.5 * self.rc_qp + 0.5 * wanted + budget;
        self.rc_qp = next.clamp(self.rc_qp - 4.0, self.rc_qp + 4.0).clamp(1.0, 51.0);
    }

    fn source(&self, v: &ironav_core::VideoFrame) -> Result<Source> {
        let want = if self.bd == 8 { PixelFormat::Yuv420p } else { PixelFormat::Yuv420p10 };
        if v.pix_fmt != want || v.width as usize != self.width || v.height as usize != self.height || v.planes.len() < 3 {
            return Err(Error::invalid_input(format!("the H.264 encoder was set up for {}x{} {} frames", self.width, self.height, want.name())));
        }
        // samples as u16 whatever the depth: two bytes each, little-endian, at 10 bits
        let unpack = |p: &[u8]| -> Vec<u16> {
            if self.bd == 8 {
                p.iter().map(|&b| b as u16).collect()
            } else {
                p.as_chunks::<2>().0.iter().map(|&c| u16::from_le_bytes(c)).collect()
            }
        };
        let pad = |p: &[u16], w: usize, h: usize, pw: usize, ph: usize| -> Result<Vec<u16>> {
            if p.len() < w * h {
                return Err(Error::invalid_input("a video frame plane is shorter than its dimensions"));
            }
            let mut out = vec![0u16; pw * ph];
            for y in 0..ph {
                let row = &p[y.min(h - 1) * w..y.min(h - 1) * w + w];
                for x in 0..pw {
                    out[y * pw + x] = row[x.min(w - 1)];
                }
            }
            Ok(out)
        };
        let (pw, ph) = (self.mb_w * 16, self.mb_h * 16);
        let (cw, ch) = (self.width / 2, self.height / 2);
        let (y, u, v2) = (unpack(&v.planes[0]), unpack(&v.planes[1]), unpack(&v.planes[2]));
        Ok(Source { planes: [pad(&y, self.width, self.height, pw, ph)?, pad(&u, cw, ch, pw / 2, ph / 2)?, pad(&v2, cw, ch, pw / 2, ph / 2)?], bd: self.bd })
    }
}

impl H264Encoder {
    /// B pictures between P pictures (CABAC streams only).
    fn b_frames(&self) -> usize {
        if self.cabac { self.bframes } else { 0 }
    }

    /// Codes one picture of `kind` (0 IDR, 1 P, 2 B) shown at display index `d`.
    fn code_picture(&mut self, src: &Source, pts: i64, kind: u8, d: u64) -> Packet {
        let (idr, b) = (kind == 0, kind == 2);
        if idr {
            self.frame_num = 0;
            self.refs.clear();
            self.idr_at = d;
        }
        let poc = (2 * (d - self.idr_at)) as i32;
        let qp = self.next_qp(kind);
        let (frame_num, idr_id, poc_type0) = (self.frame_num, self.idr_id, self.b_frames() > 0);
        let (mb_w, mb_h, n, use_cabac, t8, sub8) = (self.mb_w, self.mb_h, self.slices.clamp(1, self.mb_h), self.cabac, self.transform8, self.sub8x8);
        // P: the last reference; B: the references before and after it
        let (l0, l1) = match kind {
            0 => (None, None),
            1 => (self.refs.last(), None),
            _ => (self.refs.iter().filter(|r| r.2 < poc).max_by_key(|r| r.2), self.refs.iter().filter(|r| r.2 > poc).min_by_key(|r| r.2)),
        };
        let refp = l0.map(|r| (&r.0, &r.1));
        let refp1 = l1.map(|r| (&r.0, &r.1));
        // P: the references, most recent first (the order of list 0), as many as the stream uses
        let plist: Vec<(&Picture, &HalfPel)> = if kind == 1 { self.refs.iter().rev().take(self.nrefs).map(|r| (&r.0, &r.1)).collect() } else { Vec::new() };
        let nact = plist.len();
        // a B picture's distances from its list 0 reference and between its references
        let dist = match (l0, l1) {
            (Some(a), Some(c)) => (poc - a.2, c.2 - a.2),
            _ => (1, 2),
        };
        let header = |w: &mut BitWriter, first_mb: usize| {
            w.ue(first_mb as u32); // first_mb_in_slice
            w.ue([7, 5, 6][kind as usize]); // slice_type: I, P or B, all slices of the picture alike
            w.ue(0); // pic_parameter_set_id
            w.put(8, frame_num);
            if idr {
                w.ue(idr_id);
            }
            if poc_type0 {
                w.put(8, poc as u32 & 255); // pic_order_cnt_lsb
            }
            if b {
                w.flag(true); // direct_spatial_mv_pred_flag
            }
            if !idr {
                if nact > 1 {
                    w.flag(true); // num_ref_idx_active_override_flag
                    w.ue(nact as u32 - 1); // num_ref_idx_l0_active_minus1
                } else {
                    w.flag(false); // num_ref_idx_active_override_flag
                }
                w.flag(false); // ref_pic_list_modification_flag_l0
                if b {
                    w.flag(false); // ref_pic_list_modification_flag_l1
                }
            }
            if idr {
                w.flag(false); // no_output_of_prior_pics_flag
                w.flag(false); // long_term_reference_flag
            } else if !b {
                w.flag(false); // adaptive_ref_pic_marking_mode_flag
            }
            if use_cabac && !idr {
                w.ue(0); // cabac_init_idc
            }
            w.se(qp - 26); // slice_qp_delta
        };
        let bands: Vec<_> = (0..n).map(|i| i * mb_h / n..(i + 1) * mb_h / n).collect();
        let plist = &plist;
        let coded: Vec<_> = std::thread::scope(|s| {
            let jobs: Vec<_> = bands
                .iter()
                .enumerate()
                .map(|(i, rows)| {
                    let (src, rows) = (src, rows.clone());
                    s.spawn(move || {
                        let mut coder = FrameCoder::new(src, refp, mb_w, mb_h, qp, rows.clone(), i as u16 + 1, use_cabac, t8).with_sub8x8(sub8);
                        if kind == 1 {
                            coder = coder.with_refs(plist.clone());
                        }
                        if let Some(r1) = refp1 {
                            coder = coder.with_b(r1, dist);
                        }
                        coder.encode(|w| header(w, rows.start * mb_w))
                    })
                })
                .collect();
            jobs.into_iter().map(|j| j.join().expect("an H.264 slice thread panicked")).collect()
        });
        // the slices' rows and macroblocks make up the picture, which is then loop filtered across them
        let mut pic = Picture::new(mb_w, mb_h, 1, self.bd);
        let mut data = Vec::new();
        if idr {
            put_nal(&mut data, 3, 7, &self.sps());
            put_nal(&mut data, 3, 8, &self.pps());
            self.idr_id = (self.idr_id + 1) % 65536;
        }
        for (rows, (rbsp, part, _)) in bands.iter().zip(coded) {
            // B pictures are not references
            put_nal(&mut data, [3, 2, 0][kind as usize], if idr { 5 } else { 1 }, &rbsp);
            for (c, unit) in [(0, mb_w * 256), (1, mb_w * 64), (2, mb_w * 64)] {
                let span = rows.start * unit..rows.end * unit;
                pic.planes[c][span.clone()].copy_from_slice(&part.planes[c][span]);
            }
            pic.mbs[rows.start * mb_w..rows.end * mb_w].copy_from_slice(&part.mbs[rows.start * mb_w..rows.end * mb_w]);
        }
        pic.slices = vec![SliceParams { disable_deblocking: 0, alpha_offset: 0, beta_offset: 0, chroma_qp_offset: [0, 0] }; n];
        deblock_picture(&mut pic, self.bd);
        self.rate_update(data.len() * 8, qp, kind);
        #[cfg(test)]
        self.recon.push((pts, pic.planes.clone()));
        if !b {
            let half = HalfPel::new(&pic.planes[0], mb_w * 16, mb_h * 16, self.bd);
            // the references kept: the pictures either side of the B pictures, and as many more as
            // P pictures may use
            if self.refs.len() == self.ref_count() {
                self.refs.remove(0);
            }
            self.refs.push((pic, half, poc));
            self.frame_num = (self.frame_num + 1) % 256;
        }
        Packet { stream: 0, pts, duration: 1, data }
    }
}

impl H264Encoder {
    /// Codes the waiting pictures with no P picture after them: the last as P, the others as B.
    fn code_pending(&mut self) -> Vec<Packet> {
        let mut out = Vec::new();
        if let Some((s, pts, d)) = self.pending.pop() {
            out.push(self.code_picture(&s, pts, 1, d));
        }
        for (s, pts, d) in std::mem::take(&mut self.pending) {
            out.push(self.code_picture(&s, pts, 2, d));
        }
        out
    }
}

impl H264Encoder {
    /// The picture's luma averaged over 4x4 blocks.
    fn low_res(&self, src: &Source) -> LowRes {
        let pw = self.mb_w * 16;
        let y = &src.planes[0];
        LowRes::new(self.mb_w * 4, self.mb_h * 4, |x, yy| y[yy * pw + x] as i32)
    }
}

impl Encoder for H264Encoder {
    /// Pictures are coded I P B B P B B ...: pictures that will be B pictures wait for the P picture
    /// after them, which is coded first. The last picture before an IDR picture, or of the stream,
    /// is coded as P instead.
    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>> {
        let Frame::Video(v) = frame else {
            return Err(Error::invalid_input("the H.264 encoder received an audio frame"));
        };
        let src = self.source(v)?;
        let d = self.count;
        self.count += 1;
        let low = self.low_res(&src);
        let cut = self.last_low.as_ref().is_some_and(|prev| {
            let (inter, intra) = change(prev, &low);
            inter as f64 > SCENE_CUT * intra as f64
        });
        self.last_low = Some(low);
        let forced = std::mem::take(&mut self.force_idr);
        let idr = forced || d == 0 || d - self.idr_at >= self.keyint || cut;
        let mut out = Vec::new();
        if idr || self.pending.len() == self.b_frames() {
            if idr {
                out = self.code_pending();
            }
            out.push(self.code_picture(&src, v.pts, if idr { 0 } else { 1 }, d));
            for (s, pts, pd) in std::mem::take(&mut self.pending) {
                out.push(self.code_picture(&s, pts, 2, pd));
            }
        } else {
            self.pending.push((src, v.pts, d));
        }
        Ok(out)
    }

    fn flush(&mut self) -> Result<Vec<Packet>> {
        Ok(self.code_pending())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::h264::H264Decoder;
    use ironav_core::{Decoder, VideoFrame};

    /// Moving content: a gradient, two squares sliding at different speeds and a little noise.
    fn scene(w: usize, h: usize, t: usize) -> VideoFrame {
        let mut seed = 7u32 + t as u32 * 1000;
        let mut noise = move || {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            ((seed >> 16) % 9) as i32 - 4
        };
        let mut y = vec![0u8; w * h];
        for r in 0..h {
            for c in 0..w {
                let mut v = (c * 160 / w + r * 60 / h) as i32 + 20;
                if (c + 3 * t) % w < w / 4 && r > h / 5 && r < h / 2 {
                    v = 220 - ((c + r) % 7) as i32 * 10;
                }
                if (c + w - (2 * t) % w) % w > w / 2 && (c + w - (2 * t) % w) % w < w / 2 + w / 6 && r > h / 2 + 4 {
                    v = 30 + ((r * 5) % 40) as i32;
                }
                y[r * w + c] = (v + noise()).clamp(0, 255) as u8;
            }
        }
        let (cw, ch) = (w / 2, h / 2);
        let u = (0..cw * ch).map(|i| (100 + (i % cw) * 50 / cw + t) as u8).collect();
        let v = (0..cw * ch).map(|i| (150 - (i / cw) * 40 / ch) as u8).collect();
        VideoFrame { width: w as u32, height: h as u32, pix_fmt: PixelFormat::Yuv420p, pts: t as i64, planes: vec![y, u, v] }
    }

    fn psnr(a: &[u8], b: &[u8]) -> f64 {
        let se: f64 = a.iter().zip(b).map(|(x, y)| (*x as f64 - *y as f64).powi(2)).sum::<f64>() / a.len() as f64;
        10.0 * (255.0f64 * 255.0 / se.max(1e-9)).log10()
    }

    fn run(w: usize, h: usize, frames: usize, qp: Option<u8>, bitrate: Option<u32>, slices: usize) -> (Vec<VideoFrame>, Vec<VideoFrame>, usize) {
        run_with(w, h, frames, qp, bitrate, slices, 2)
    }

    /// `mode`: 0 CAVLC (Constrained Baseline), 1 CABAC (Main), 2 CABAC, the 8x8 transform and B
    /// pictures (High), 3 High without B pictures.
    #[allow(clippy::too_many_arguments)]
    fn run_with(w: usize, h: usize, frames: usize, qp: Option<u8>, bitrate: Option<u32>, slices: usize, mode: u8) -> (Vec<VideoFrame>, Vec<VideoFrame>, usize) {
        run_refs(w, h, frames, qp, bitrate, slices, mode, 1, false)
    }
    /// `run_with` with up to `refs` reference pictures.
    #[allow(clippy::too_many_arguments)]
    fn run_refs(w: usize, h: usize, frames: usize, qp: Option<u8>, bitrate: Option<u32>, slices: usize, mode: u8, refs: usize, sub8: bool) -> (Vec<VideoFrame>, Vec<VideoFrame>, usize) {
        let params = StreamParams::Video { width: w as u32, height: h as u32, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(25, 1) };
        let mut enc = H264Encoder::new(&params, qp, bitrate).unwrap().with_refs(refs).with_sub8x8(sub8);
        enc.slices = slices;
        enc.cabac = mode > 0;
        enc.transform8 = mode > 1;
        enc.bframes = if mode == 2 { BFRAMES } else { 0 };
        let mut dec = H264Decoder::new();
        let (mut src, mut pkts, mut bytes) = (Vec::new(), Vec::new(), 0);
        for t in 0..frames {
            let f = scene(w, h, t);
            pkts.extend(enc.encode(&Frame::Video(f.clone())).unwrap());
            src.push(f);
        }
        pkts.extend(enc.flush().unwrap());
        let mut out = Vec::new();
        for p in &pkts {
            bytes += p.data.len();
            out.extend(dec.decode(p).unwrap());
        }
        out.extend(dec.flush().unwrap());
        let out: Vec<VideoFrame> = out.into_iter().map(|f| match f { Frame::Video(v) => v, _ => unreachable!() }).collect();
        // the decoder rebuilds each picture as the encoder reconstructed it, in display order
        assert_eq!(out.len(), frames);
        let s = enc.mb_w * 16;
        for (t, d) in out.iter().enumerate() {
            assert_eq!(d.pts, t as i64, "{w}x{h}: output order");
            let recon = &enc.recon.iter().find(|r| r.0 == d.pts).unwrap().1;
            for r in 0..h {
                assert_eq!(&d.planes[0][r * w..r * w + w], recon[0][r * s..r * s + w].iter().map(|&v| v as u8).collect::<Vec<_>>().as_slice(), "{w}x{h} mode {mode} frame {t} luma row {r}");
            }
            for c in 1..3 {
                for r in 0..h / 2 {
                    assert_eq!(&d.planes[c][r * w / 2..r * w / 2 + w / 2], recon[c][r * s / 2..r * s / 2 + w / 2].iter().map(|&v| v as u8).collect::<Vec<_>>().as_slice(), "{w}x{h} mode {mode} frame {t} plane {c} row {r}");
                }
            }
        }
        (src, out, bytes)
    }

    #[test]
    fn decoder_rebuilds_the_encoders_reference() {
        for (w, h, slices, mode) in [(64, 48, 1, 2), (200, 120, 3, 2), (16, 16, 1, 2), (34, 18, 2, 2), (96, 80, 5, 2), (64, 48, 1, 3), (64, 48, 1, 1), (200, 120, 3, 1), (64, 48, 1, 0), (200, 120, 3, 0)] {
            for qp in [0, 4, 20, 30, 45, 51] {
                let (src, out, _) = run_with(w, h, 6, Some(qp), None, slices, mode);
                assert_eq!(out.len(), src.len());
                if qp == 20 {
                    for (s, d) in src.iter().zip(&out) {
                        let p = psnr(&s.planes[0], &d.planes[0]);
                        assert!(p > 36.0, "{w}x{h} qp {qp}: luma PSNR {p:.1}");
                    }
                }
            }
        }
    }

    #[test]
    fn sub8x8_partitions_decode_to_the_encoders_reference() {
        // with the option on, each run asserts that the decoder rebuilds the encoder's pictures; at QP
        // 4 and 20 (the QPs it applies to), its stream must differ from the one without the option
        for (w, h, slices) in [(64, 48, 1), (200, 120, 3)] {
            for qp in [4, 20] {
                for mode in [0, 1, 2] {
                    let (_, _, on) = run_refs(w, h, 8, Some(qp), None, slices, mode, 3, true);
                    let (_, _, off) = run_refs(w, h, 8, Some(qp), None, slices, mode, 3, false);
                    assert_ne!(on, off, "{w}x{h} qp {qp} mode {mode}: no sub-partitions were coded");
                }
            }
        }
    }

    #[test]
    fn several_references_decode_to_the_encoders_reference() {
        for refs in [2, 3, 4] {
            for (w, h, slices) in [(64, 48, 1), (200, 120, 3)] {
                for mode in [0, 1, 2] {
                    // the picture decoded with several references differs from the one decoded with one, at some QP
                    let used = [4, 26, 40].into_iter().any(|qp| {
                        let (_, multi, _) = run_refs(w, h, 8, Some(qp), None, slices, mode, refs, false);
                        let (_, single, _) = run_refs(w, h, 8, Some(qp), None, slices, mode, 1, false);
                        multi.iter().zip(&single).any(|(m, s)| m.planes != s.planes)
                    });
                    assert!(used, "{w}x{h} mode {mode}: {refs} references are never used");
                }
            }
        }
    }

    /// `scene` at 10 bits: its samples scaled up, with the low two bits changing from picture to picture.
    fn scene10(w: usize, h: usize, t: usize) -> VideoFrame {
        let f8 = scene(w, h, t);
        let planes = f8.planes.iter().map(|p| p.iter().flat_map(|&v| ((v as u16) << 2 | (t as u16 & 3)).to_le_bytes()).collect::<Vec<u8>>()).collect();
        VideoFrame { width: f8.width, height: f8.height, pix_fmt: PixelFormat::Yuv420p10, pts: f8.pts, planes }
    }

    #[test]
    fn ten_bit_pictures_decode_to_the_encoders_reference() {
        // CABAC with B pictures and the 8x8 transform (High 10), and CAVLC (High 10 without them)
        for (w, h, slices, cabac) in [(64, 48, 1, true), (200, 120, 3, true), (64, 48, 1, false), (200, 120, 3, false)] {
            for qp in [4, 20, 36] {
                let params = StreamParams::Video { width: w as u32, height: h as u32, pix_fmt: PixelFormat::Yuv420p10, frame_rate: Rational::new(25, 1) };
                let mut enc = H264Encoder::new(&params, Some(qp), None).unwrap();
                enc.slices = slices;
                enc.cabac = cabac;
                enc.transform8 = cabac;
                enc.bframes = if cabac { BFRAMES } else { 0 };
                let (mut src, mut pkts) = (Vec::new(), Vec::new());
                for t in 0..6 {
                    let f = scene10(w, h, t);
                    pkts.extend(enc.encode(&Frame::Video(f.clone())).unwrap());
                    src.push(f);
                }
                pkts.extend(enc.flush().unwrap());
                let mut dec = H264Decoder::new();
                let mut out = Vec::new();
                for p in &pkts {
                    out.extend(dec.decode(p).unwrap());
                }
                out.extend(dec.flush().unwrap());
                assert_eq!(out.len(), 6, "{w}x{h} qp {qp}");
                let s = enc.mb_w * 16;
                let samples = |p: &[u8]| -> Vec<u16> { p.as_chunks::<2>().0.iter().map(|&c| u16::from_le_bytes(c)).collect() };
                for f in out {
                    let Frame::Video(d) = f else { unreachable!() };
                    assert_eq!(d.pix_fmt, PixelFormat::Yuv420p10);
                    let recon = &enc.recon.iter().find(|r| r.0 == d.pts).unwrap().1;
                    let luma = samples(&d.planes[0]);
                    for r in 0..h {
                        assert_eq!(&luma[r * w..r * w + w], &recon[0][r * s..r * s + w], "{w}x{h} qp {qp} frame {} luma row {r}", d.pts);
                    }
                    for c in 1..3 {
                        let plane = samples(&d.planes[c]);
                        for r in 0..h / 2 {
                            assert_eq!(&plane[r * w / 2..r * w / 2 + w / 2], &recon[c][r * s / 2..r * s / 2 + w / 2], "{w}x{h} qp {qp} frame {} plane {c} row {r}", d.pts);
                        }
                    }
                    let se: f64 = samples(&src[d.pts as usize].planes[0]).iter().zip(&luma).map(|(&a, &b)| (a as f64 - b as f64).powi(2)).sum::<f64>() / (w * h) as f64;
                    let p = 10.0 * (1023.0f64 * 1023.0 / se.max(1e-9)).log10();
                    // measured 31 to 61 dB across these settings (the low bits change from picture to picture)
                    let want = match qp {
                        0..=10 => 48.0,
                        11..=28 => 36.0,
                        _ => 28.0,
                    };
                    assert!(p > want, "{w}x{h} qp {qp} frame {}: luma PSNR {p:.1}", d.pts);
                }
            }
        }
    }

    #[test]
    fn low_delay_decoding_learns_the_reordering_a_stream_does_not_declare() {
        let (w, h) = (64, 48);
        let params = StreamParams::Video { width: w as u32, height: h as u32, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(25, 1) };
        // B pictures, as cameras send them, without saying how deep the reordering goes
        let mut enc = H264Encoder::new(&params, Some(26), None).unwrap();
        enc.no_reorder_info = true;
        let mut pkts = Vec::new();
        for t in 0..20 {
            pkts.extend(enc.encode(&Frame::Video(scene(w, h, t))).unwrap());
        }
        pkts.extend(enc.flush().unwrap());
        assert!(pkts.iter().any(|p| p.data.windows(5).any(|b| b[..3] == [0, 0, 1] && b[3] & 0x1F == 1 && b[3] >> 5 == 0)), "the stream has unreferenced (B) pictures");
        let run = |mut dec: H264Decoder| {
            let (mut early, mut all) = (0, Vec::new());
            for p in &pkts {
                let shown = dec.decode(p).unwrap();
                early += shown.len();
                all.extend(shown);
            }
            all.extend(dec.flush().unwrap());
            let pts: Vec<i64> = all.iter().map(|f| match f { Frame::Video(v) => v.pts, _ => unreachable!() }).collect();
            (early, pts, dec.late_pictures())
        };
        // as the level allows, the usual decoder holds many pictures back (all 20, at this size)
        let (early, pts, _) = run(H264Decoder::new());
        assert_eq!(pts, (0..20).collect::<Vec<i64>>());
        assert!(early <= 4, "{early} shown before the end");
        // low delay shows them at once, and leaves out the first B pictures, which taught it the depth
        let (early, pts, late) = run(H264Decoder::low_delay());
        assert_eq!(late, BFRAMES as u64);
        assert_eq!(pts.len(), 20 - BFRAMES);
        assert!(pts.windows(2).all(|p| p[0] < p[1]), "shown in order: {pts:?}");
        assert!(early >= 20 - BFRAMES - 2, "only {early} shown before the end");
    }

    #[test]
    fn low_latency_sends_each_picture_as_it_is_coded() {
        let (w, h) = (64, 48);
        let params = StreamParams::Video { width: w as u32, height: h as u32, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(25, 1) };
        let mut enc = H264Encoder::new(&params, None, Some(150_000)).unwrap().low_latency();
        // on one thread: decoding on several holds pictures back while others are decoded
        let mut dec = H264Decoder::with_threads(1);
        let mut idrs = Vec::new();
        for t in 0..60 {
            let pkts = enc.encode(&Frame::Video(scene(w, h, t))).unwrap();
            assert_eq!(pkts.len(), 1, "picture {t} leaves as soon as it is coded");
            assert_eq!(pkts[0].pts, t as i64);
            if pkts[0].data.windows(4).any(|b| b[..3] == [0, 0, 1] && b[3] & 0x1F == 5) {
                idrs.push(t);
            }
            // no reordering is declared, so the decoder shows each picture at once too
            assert_eq!(dec.decode(&pkts[0]).unwrap().len(), 1, "picture {t} decoded at once");
        }
        assert!(enc.flush().unwrap().is_empty());
        assert!([0, 25, 50].iter().all(|i| idrs.contains(i)), "an IDR picture each second: {idrs:?}");
    }

    #[test]
    fn a_scene_cut_starts_an_idr_picture() {
        let (w, h) = (96, 64);
        let params = StreamParams::Video { width: w as u32, height: h as u32, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(25, 1) };
        let mut enc = H264Encoder::new(&params, Some(26), None).unwrap();
        let mut dec = H264Decoder::new();
        let mut pkts = Vec::new();
        for t in 0..10 {
            // from the sixth picture on, a vertical gradient instead
            let mut f = scene(w, h, t);
            if t >= 5 {
                for (i, v) in f.planes[0].iter_mut().enumerate() {
                    *v = (255 - i / w * 3) as u8;
                }
            }
            pkts.extend(enc.encode(&Frame::Video(f)).unwrap());
        }
        pkts.extend(enc.flush().unwrap());
        // NAL units of type 5 (IDR slices)
        let idr = |p: &Packet| p.data.windows(4).any(|w| w[..3] == [0, 0, 1] && w[3] & 31 == 5);
        let idrs: Vec<i64> = pkts.iter().filter(|p| idr(p)).map(|p| p.pts).collect();
        assert_eq!(idrs, [0, 5]);
        let mut out = Vec::new();
        for p in &pkts {
            out.extend(dec.decode(p).unwrap());
        }
        out.extend(dec.flush().unwrap());
        assert_eq!(out.len(), 10);
        for f in out {
            let Frame::Video(d) = f else { unreachable!() };
            let recon = &enc.recon.iter().find(|r| r.0 == d.pts).unwrap().1;
            let s = enc.mb_w * 16;
            for r in 0..h {
                assert_eq!(&d.planes[0][r * w..r * w + w], recon[0][r * s..r * s + w].iter().map(|&v| v as u8).collect::<Vec<_>>().as_slice(), "frame {} row {r}", d.pts);
            }
        }
    }

    #[test]
    fn a_forced_idr_picture_starts_a_new_gop_that_the_pictures_after_it_predict_from() {
        let (w, h) = (96, 64);
        let params = StreamParams::Video { width: w as u32, height: h as u32, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(25, 1) };
        // without B pictures, and with them (the pictures waiting for their P picture are coded first)
        for bframes in [0, BFRAMES] {
            // an IDR picture every 5 pictures, and one forced at picture 3: the count starts again from it
            let mut enc = H264Encoder::new(&params, Some(26), None).unwrap().with_keyint(5);
            enc.bframes = bframes;
            let mut pkts = Vec::new();
            for t in 0..12 {
                if t == 3 {
                    enc.force_idr();
                }
                pkts.extend(enc.encode(&Frame::Video(scene(w, h, t))).unwrap());
            }
            pkts.extend(enc.flush().unwrap());
            // NAL units of a given type (5 IDR slices, 7 SPS, 8 PPS, 1 P slices)
            let has = |p: &Packet, want: u8| p.data.windows(4).any(|w| w[..3] == [0, 0, 1] && w[3] & 31 == want);
            let idrs: Vec<i64> = pkts.iter().filter(|p| has(p, 5)).map(|p| p.pts).collect();
            assert_eq!(idrs, [0, 3, 8], "bframes {bframes}");
            for t in [0, 3, 8] {
                let p = pkts.iter().find(|p| p.pts == t).unwrap();
                assert!(has(p, 7) && has(p, 8), "bframes {bframes}: the IDR picture at {t} has its parameter sets");
            }
            if bframes == 0 {
                // the pictures after the forced one are P pictures, predicted from it
                for t in 4..8 {
                    let p = pkts.iter().find(|p| p.pts == t).unwrap();
                    assert!(has(p, 1) && !has(p, 5), "picture {t} is a P picture");
                }
            }
            let mut dec = H264Decoder::new();
            let mut out = Vec::new();
            for p in &pkts {
                out.extend(dec.decode(p).unwrap());
            }
            out.extend(dec.flush().unwrap());
            assert_eq!(out.len(), 12, "bframes {bframes}");
            // each decoded picture is what the encoder reconstructed, so the pictures after the IDR picture
            // were predicted from the references it kept
            let s = enc.mb_w * 16;
            for f in out {
                let Frame::Video(d) = f else { unreachable!() };
                let recon = &enc.recon.iter().find(|r| r.0 == d.pts).unwrap().1;
                for r in 0..h {
                    assert_eq!(&d.planes[0][r * w..r * w + w], recon[0][r * s..r * s + w].iter().map(|&v| v as u8).collect::<Vec<_>>().as_slice(), "bframes {bframes} frame {} row {r}", d.pts);
                }
            }
        }
    }

    #[test]
    fn a_bit_rate_is_followed_on_average() {
        let target = 300_000;
        let (_, _, bytes) = run(160, 96, 75, None, Some(target), 2);
        let rate = bytes as f64 * 8.0 / 3.0;
        assert!((rate / target as f64 - 1.0).abs() < 0.25, "{rate:.0} bit/s against {target}");
    }
}
