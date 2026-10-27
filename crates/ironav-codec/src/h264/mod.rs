//! A software H.264 decoder (ITU-T H.264 / ISO 14496-10) for progressive video: 8- and 10-bit 4:2:0, 8-bit 4:2:2 and monochrome.
//!
//! Intra pictures (I slices, CAVLC and CABAC, 4x4 / 8x8 / 16x16 prediction, I_PCM, 8x8 transform,
//! scaling matrices, multiple slices, the loop filter), P slices and B slices (all partition sizes,
//! skip and direct modes, multiple reference frames, bi-prediction, weighted prediction) are decoded
//! with CAVLC and CABAC, with reference picture marking and output reordering.

mod bits;
mod cabac;
mod cavlc;
mod deblock;
mod dpb;
mod idct;
mod inter;
mod intra;
mod mb;
mod ps;
mod tables;

use bits::{split_annexb, unescape};
use dpb::{DecodedPic, Dpb, Pending, RefList};
use ironav_core::{Decoder, Error, ErrorCode, Frame, Packet, PixelFormat, Result, VideoFrame};
use mb::{LevelScale, Picture, SliceDec, SliceParams};
use ps::{parse_pps, parse_slice_header, parse_sps, Pps, SliceHeader, SliceType, Sps};
use std::sync::Arc;

pub struct H264Decoder {
    sps: Vec<Option<Sps>>,
    pps: Vec<Option<Pps>>,
    cur: Option<Current>,
    dpb: Dpb,
    out: Vec<Frame>,
}

struct Current {
    pic: Picture,
    sps_id: usize,
    hdr: SliceHeader,
    pts: i64,
    poc: i32,
    mbs_done: usize,
}

fn unsupported(msg: &str) -> Error {
    Error::new(ErrorCode::UnsupportedCodec, msg.to_string())
}

impl H264Decoder {
    pub fn new() -> Self {
        H264Decoder { sps: vec![None; 32], pps: vec![None; 256], cur: None, dpb: Dpb::default(), out: Vec::new() }
    }

    /// Whether a slice starts a new picture (7.4.1.2.4, for frame pictures).
    fn new_picture(prev: &SliceHeader, h: &SliceHeader) -> bool {
        prev.frame_num != h.frame_num
            || prev.pps_id != h.pps_id
            || (prev.nal_ref_idc == 0) != (h.nal_ref_idc == 0)
            || prev.idr != h.idr
            || (h.idr && prev.idr_pic_id != h.idr_pic_id)
            || prev.poc_lsb != h.poc_lsb
            || prev.delta_poc_bottom != h.delta_poc_bottom
            || prev.delta_poc != h.delta_poc
    }

    fn emit(&mut self, p: Pending) {
        let pic = &p.pic;
        let [cl, cr, ct, cb] = pic.crop;
        let (w, h) = (pic.mb_w * 16 - cl - cr, pic.mb_h * 16 - ct - cb);
        let c422 = pic.cf == 2;
        let (cw, ch) = (w.div_ceil(2), if c422 { h } else { h.div_ceil(2) });
        let wide = pic.bit_depth > 8;
        // 8-bit samples go out as bytes, deeper ones as little-endian 16-bit words
        let put = |v: &mut Vec<u8>, src: &[u16]| {
            if wide {
                for s in src {
                    v.extend_from_slice(&s.to_le_bytes());
                }
            } else {
                v.extend(src.iter().map(|&s| s as u8));
            }
        };
        let gray = pic.cf == 0 && !wide;
        let mut planes = Vec::with_capacity(3);
        let ys = pic.mb_w * 16;
        let mut y = Vec::with_capacity(w * h);
        for r in 0..h {
            put(&mut y, &pic.planes[0][(ct + r) * ys + cl..(ct + r) * ys + cl + w]);
        }
        planes.push(y);
        let cs = pic.mb_w * 8;
        for pl in 1..if gray { 1 } else { 3 } {
            let mut v = Vec::with_capacity(cw * ch);
            for r in 0..ch {
                let o = ((if c422 { ct } else { ct / 2 }) + r) * cs + cl / 2;
                put(&mut v, &pic.planes[pl][o..o + cw]);
            }
            planes.push(v);
        }
        let pix_fmt = match (c422, wide) {
            _ if gray => PixelFormat::Gray8,
            (true, _) => PixelFormat::Yuv422p,
            (false, true) => PixelFormat::Yuv420p10,
            (false, false) => PixelFormat::Yuv420p,
        };
        self.out.push(Frame::Video(VideoFrame { width: w as u32, height: h as u32, pix_fmt, pts: p.pts, planes }));
    }

    fn finish_picture(&mut self) {
        let Some(mut c) = self.cur.take() else { return };
        let bd = self.sps[c.sps_id].as_ref().map_or(8, |s| s.bit_depth_luma);
        deblock::deblock_picture(&mut c.pic, bd);
        let sps = self.sps[c.sps_id].as_ref().expect("the sequence parameter set outlives its picture");
        let id = self.dpb.new_id();
        let pic = Arc::new(DecodedPic {
            planes: std::mem::take(&mut c.pic.planes),
            mb_w: c.pic.mb_w,
            mb_h: c.pic.mb_h,
            cf: c.pic.cf,
            mbs: std::mem::take(&mut c.pic.mbs),
            poc: c.poc,
            id,
            crop: sps.crop,
            bit_depth: sps.bit_depth_luma,
        });
        for p in self.dpb.finish(sps, &c.hdr, pic, c.pts) {
            self.emit(p);
        }
    }

    fn slice(&mut self, nal_type: u8, nal_ref_idc: u32, rbsp: &[u8], pts: i64) -> Result<()> {
        let hdr = parse_slice_header(rbsp, nal_type, nal_ref_idc, &self.sps, &self.pps)?;
        if hdr.redundant_pic_cnt > 0 {
            return Ok(());
        }
        let sps_id = self.pps[hdr.pps_id as usize].as_ref().map(|p| p.sps_id as usize).unwrap_or(0);
        let sps = self.sps[sps_id].as_ref().ok_or_else(|| Error::invalid_input("H.264 slice has no sequence parameter set"))?;
        if sps.chroma_format_idc > 2 || sps.bit_depth_luma != sps.bit_depth_chroma || !(8..=10).contains(&sps.bit_depth_luma) {
            return Err(unsupported("this H.264 decoder handles monochrome, 4:2:0 and 4:2:2 video with 8 or 10 bits per sample only"));
        }
        if sps.chroma_format_idc == 2 && sps.bit_depth_luma != 8 {
            return Err(unsupported("this H.264 decoder handles 4:2:2 video with 8 bits per sample only"));
        }
        if sps.transform_bypass {
            return Err(unsupported("H.264 lossless transform bypass is not supported"));
        }
        let starts_new = match &self.cur {
            Some(c) => Self::new_picture(&c.hdr, &hdr),
            None => true,
        };
        if starts_new {
            self.finish_picture();
            let sps = self.sps[sps_id].as_ref().unwrap();
            let poc = self.dpb.compute_poc(sps, &hdr);
            self.cur = Some(Current { pic: Picture::new(sps.mb_width, sps.mb_height, sps.chroma_format_idc as u8, sps.bit_depth_luma), sps_id, hdr: hdr.clone(), pts, poc, mbs_done: 0 });
        }
        let cur = self.cur.as_mut().unwrap();
        let sps = self.sps[sps_id].as_ref().unwrap();
        let pps = self.pps[hdr.pps_id as usize].as_ref().unwrap();
        let refs: [RefList; 2] = if hdr.slice_type == SliceType::I { [Vec::new(), Vec::new()] } else { self.dpb.ref_lists(sps, &hdr, cur.poc)? };
        let scale = LevelScale::new(pps);
        cur.pic.slices.push(SliceParams { disable_deblocking: hdr.disable_deblocking, alpha_offset: hdr.alpha_offset, beta_offset: hdr.beta_offset, chroma_qp_offset: pps.chroma_qp_offset });
        let id = cur.pic.slices.len() as u16;
        let mut dec = SliceDec::new(sps, pps, &hdr, &mut cur.pic, &scale, id, rbsp, &refs, cur.poc)?;
        let r = dec.decode();
        cur.mbs_done = cur.pic.mbs.iter().filter(|m| m.slice != 0).count();
        cur.hdr = hdr;
        r
    }
}

impl Default for H264Decoder {
    fn default() -> Self {
        Self::new()
    }
}

impl Decoder for H264Decoder {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<Frame>> {
        let mut err = None;
        for nal in split_annexb(&packet.data) {
            if nal.is_empty() {
                continue;
            }
            let (ref_idc, ty) = (((nal[0] >> 5) & 3) as u32, nal[0] & 31);
            let rbsp = unescape(&nal[1..]);
            let r = match ty {
                7 => parse_sps(&rbsp).map(|s| {
                    let id = s.id as usize;
                    self.sps[id] = Some(s);
                }),
                8 => parse_pps(&rbsp, &self.sps).map(|p| {
                    let id = p.id as usize;
                    self.pps[id] = Some(p);
                }),
                1 | 5 => self.slice(ty, ref_idc, &rbsp, packet.pts),
                _ => Ok(()),
            };
            if let Err(e) = r {
                err = Some(e);
                break;
            }
        }
        // a packet holds whole access units, so a complete picture is ready now
        if let Some(c) = &self.cur {
            if c.mbs_done == c.pic.mb_w * c.pic.mb_h {
                self.finish_picture();
            }
        }
        if let Some(e) = err {
            return Err(e);
        }
        Ok(std::mem::take(&mut self.out))
    }

    fn flush(&mut self) -> Result<Vec<Frame>> {
        self.finish_picture();
        for p in self.dpb.drain_all() {
            self.emit(p);
        }
        Ok(std::mem::take(&mut self.out))
    }
}
