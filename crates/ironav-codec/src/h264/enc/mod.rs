//! A software H.264 encoder: Constrained Baseline profile (CAVLC, I and P pictures, one reference
//! picture), 8-bit 4:2:0 with even dimensions. Macroblocks are coded as Intra4x4, Intra16x16,
//! P_L0_16x16 or P_Skip, chosen by SATD and an estimate of their bits; motion is searched to
//! quarter samples. Output is Annex B with SPS and PPS ahead of every IDR picture.

mod bitw;
mod cavlc;
mod frame;

use super::mb::Picture;
use bitw::{put_nal, BitWriter};
use frame::{FrameCoder, Source};
use ironav_core::{Encoder, Error, ErrorCode, Frame, Packet, PixelFormat, Rational, Result, StreamParams};

/// Pictures between IDR pictures.
const KEYINT: u64 = 250;

/// The quantiser used when neither a quantiser nor a bit rate is set.
pub const DEFAULT_QP: u8 = 23;

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
    refp: Option<Picture>,
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
        if pix_fmt != PixelFormat::Yuv420p {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("the software H.264 encoder takes yuv420p frames, not {}", pix_fmt.name()))
                .hint("add a format filter to convert to yuv420p"));
        }
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
            rc_qp: 26.0,
            spent: 0.0,
            allowed: 0.0,
            refp: None,
            count: 0,
            frame_num: 0,
            idr_id: 0,
        })
    }

    fn fps(&self) -> f64 {
        let r = self.frame_rate;
        if r.num > 0 && r.den > 0 { r.num as f64 / r.den as f64 } else { 25.0 }
    }

    fn sps(&self) -> Vec<u8> {
        let mut w = BitWriter::new();
        w.put(8, 66); // Baseline
        w.put(8, 0b1100_0000); // constraint_set0 and constraint_set1: Constrained Baseline
        w.put(8, self.level as u32);
        w.ue(0); // seq_parameter_set_id
        w.ue(4); // log2_max_frame_num_minus4: 8 bits
        w.ue(2); // pic_order_cnt_type: output order is decoding order
        w.ue(1); // max_num_ref_frames
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
        w.flag(true); // bitstream_restriction_flag
        w.flag(true); // motion_vectors_over_pic_boundaries_flag
        w.ue(0); // max_bytes_per_pic_denom
        w.ue(0); // max_bits_per_mb_denom
        w.ue(15); // log2_max_mv_length_horizontal
        w.ue(15); // log2_max_mv_length_vertical
        w.ue(0); // max_num_reorder_frames
        w.ue(1); // max_dec_frame_buffering
        w.finish()
    }

    fn pps() -> Vec<u8> {
        let mut w = BitWriter::new();
        w.ue(0); // pic_parameter_set_id
        w.ue(0); // seq_parameter_set_id
        w.flag(false); // entropy_coding_mode_flag: CAVLC
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
        w.finish()
    }

    /// The quantiser for the next picture.
    fn next_qp(&self, idr: bool) -> i32 {
        match self.qp {
            Some(q) => q as i32,
            None => (self.rc_qp.round() as i32 - if idr { 3 } else { 0 }).clamp(1, 51),
        }
    }

    /// Steers the quantiser so the bits spent follow the bits the rate allows. A picture's size
    /// roughly halves for every 6 added to the quantiser, which says what quantiser the last
    /// picture would have needed; the running surplus or deficit, against a second's worth of
    /// bits, pulls the next one further.
    fn rate_update(&mut self, bits: usize, qp: i32, idr: bool) {
        let Some(rate) = self.bitrate else {
            return;
        };
        let per_frame = rate as f64 / self.fps();
        self.spent += bits as f64;
        self.allowed += per_frame;
        let window = rate as f64;
        let budget = 6.0 * ((self.spent + window) / (self.allowed + window).max(1.0)).log2();
        // an IDR picture is expected to cost several P pictures, and was coded 3 lower
        let expected = if idr { per_frame * 4.0 } else { per_frame };
        let wanted = (qp + if idr { 3 } else { 0 }) as f64 + 6.0 * (bits as f64 / expected).max(1e-3).log2();
        let next = 0.5 * self.rc_qp + 0.5 * wanted + budget;
        self.rc_qp = next.clamp(self.rc_qp - 4.0, self.rc_qp + 4.0).clamp(1.0, 51.0);
    }

    fn source(&self, v: &ironav_core::VideoFrame) -> Result<Source> {
        if v.pix_fmt != PixelFormat::Yuv420p || v.width as usize != self.width || v.height as usize != self.height || v.planes.len() < 3 {
            return Err(Error::invalid_input(format!("the H.264 encoder was set up for {}x{} yuv420p frames", self.width, self.height)));
        }
        let pad = |p: &[u8], w: usize, h: usize, pw: usize, ph: usize| -> Result<Vec<u16>> {
            if p.len() < w * h {
                return Err(Error::invalid_input("a video frame plane is shorter than its dimensions"));
            }
            let mut out = vec![0u16; pw * ph];
            for y in 0..ph {
                let row = &p[y.min(h - 1) * w..y.min(h - 1) * w + w];
                for x in 0..pw {
                    out[y * pw + x] = row[x.min(w - 1)] as u16;
                }
            }
            Ok(out)
        };
        let (pw, ph) = (self.mb_w * 16, self.mb_h * 16);
        let (cw, ch) = (self.width / 2, self.height / 2);
        Ok(Source { planes: [pad(&v.planes[0], self.width, self.height, pw, ph)?, pad(&v.planes[1], cw, ch, pw / 2, ph / 2)?, pad(&v.planes[2], cw, ch, pw / 2, ph / 2)?] })
    }
}

impl Encoder for H264Encoder {
    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>> {
        let Frame::Video(v) = frame else {
            return Err(Error::invalid_input("the H.264 encoder received an audio frame"));
        };
        let src = self.source(v)?;
        let idr = self.count % KEYINT == 0;
        if idr {
            self.frame_num = 0;
        }
        let qp = self.next_qp(idr);
        let (frame_num, idr_id) = (self.frame_num, self.idr_id);
        let coder = FrameCoder::new(&src, if idr { None } else { self.refp.as_ref() }, self.mb_w, self.mb_h, qp);
        let (rbsp, pic, _) = coder.encode(|w| {
            w.ue(0); // first_mb_in_slice
            w.ue(if idr { 7 } else { 5 }); // slice_type: I or P, all slices of the picture alike
            w.ue(0); // pic_parameter_set_id
            w.put(8, frame_num);
            if idr {
                w.ue(idr_id);
            } else {
                w.flag(false); // num_ref_idx_active_override_flag
                w.flag(false); // ref_pic_list_modification_flag_l0
            }
            if idr {
                w.flag(false); // no_output_of_prior_pics_flag
                w.flag(false); // long_term_reference_flag
            } else {
                w.flag(false); // adaptive_ref_pic_marking_mode_flag
            }
            w.se(qp - 26); // slice_qp_delta
        });
        let mut data = Vec::with_capacity(rbsp.len() + 64);
        if idr {
            put_nal(&mut data, 3, 7, &self.sps());
            put_nal(&mut data, 3, 8, &Self::pps());
            self.idr_id = (self.idr_id + 1) % 65536;
        }
        put_nal(&mut data, if idr { 3 } else { 2 }, if idr { 5 } else { 1 }, &rbsp);
        self.rate_update(data.len() * 8, qp, idr);
        self.refp = Some(pic);
        self.count += 1;
        self.frame_num = (self.frame_num + 1) % 256;
        Ok(vec![Packet { stream: 0, pts: v.pts, duration: 1, data }])
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

    fn run(w: usize, h: usize, frames: usize, qp: Option<u8>, bitrate: Option<u32>) -> (Vec<VideoFrame>, Vec<VideoFrame>, usize) {
        let params = StreamParams::Video { width: w as u32, height: h as u32, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(25, 1) };
        let mut enc = H264Encoder::new(&params, qp, bitrate).unwrap();
        let mut dec = H264Decoder::new();
        let (mut src, mut out, mut bytes) = (Vec::new(), Vec::new(), 0);
        for t in 0..frames {
            let f = scene(w, h, t);
            let pkts = enc.encode(&Frame::Video(f.clone())).unwrap();
            // the decoder sees the reference the encoder keeps
            let recon = enc.refp.as_ref().unwrap();
            for p in &pkts {
                bytes += p.data.len();
                for d in dec.decode(p).unwrap() {
                    out.push(d);
                }
            }
            let Some(Frame::Video(d)) = out.last().map(|f| Frame::Video(match f { Frame::Video(v) => v.clone(), _ => unreachable!() })) else { panic!("frame {t} was not output at once") };
            assert_eq!(out.len(), t + 1);
            for r in 0..h {
                assert_eq!(&d.planes[0][r * w..r * w + w], recon.planes[0][r * recon.mb_w * 16..r * recon.mb_w * 16 + w].iter().map(|&v| v as u8).collect::<Vec<_>>().as_slice(), "{w}x{h} frame {t} luma row {r}");
            }
            for c in 1..3 {
                for r in 0..h / 2 {
                    let s = recon.mb_w * 8;
                    assert_eq!(&d.planes[c][r * w / 2..r * w / 2 + w / 2], recon.planes[c][r * s..r * s + w / 2].iter().map(|&v| v as u8).collect::<Vec<_>>().as_slice(), "{w}x{h} frame {t} plane {c} row {r}");
                }
            }
            src.push(f);
        }
        out.extend(dec.flush().unwrap());
        let out = out.into_iter().map(|f| match f { Frame::Video(v) => v, _ => unreachable!() }).collect();
        (src, out, bytes)
    }

    #[test]
    fn decoder_rebuilds_the_encoders_reference() {
        for (w, h) in [(64, 48), (200, 120), (16, 16), (34, 18)] {
            for qp in [4, 20, 30, 45] {
                let (src, out, _) = run(w, h, 6, Some(qp), None);
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
    fn a_bit_rate_is_followed_on_average() {
        let target = 300_000;
        let (_, _, bytes) = run(160, 96, 75, None, Some(target));
        let rate = bytes as f64 * 8.0 / 3.0;
        assert!((rate / target as f64 - 1.0).abs() < 0.25, "{rate:.0} bit/s against {target}");
    }
}
