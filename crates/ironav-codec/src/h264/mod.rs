//! A software H.264 decoder (ITU-T H.264 / ISO 14496-10) for 8-bit 4:2:0 progressive video.
//!
//! Intra pictures (I slices, CAVLC and CABAC, 4x4 / 8x8 / 16x16 prediction, I_PCM, 8x8 transform,
//! scaling matrices, multiple slices, the loop filter) and CAVLC P slices (all partition sizes,
//! P_Skip, multiple reference frames, explicit weighted prediction) are decoded, with reference
//! picture marking and output reordering. B slices and CABAC P slices are not here yet.

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
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let mut planes = Vec::with_capacity(3);
        let ys = pic.mb_w * 16;
        let mut y = Vec::with_capacity(w * h);
        for r in 0..h {
            y.extend_from_slice(&pic.planes[0][(ct + r) * ys + cl..(ct + r) * ys + cl + w]);
        }
        planes.push(y);
        let cs = pic.mb_w * 8;
        for pl in 1..3 {
            let mut v = Vec::with_capacity(cw * ch);
            for r in 0..ch {
                let o = (ct / 2 + r) * cs + cl / 2;
                v.extend_from_slice(&pic.planes[pl][o..o + cw]);
            }
            planes.push(v);
        }
        self.out.push(Frame::Video(VideoFrame { width: w as u32, height: h as u32, pix_fmt: PixelFormat::Yuv420p, pts: p.pts, planes }));
    }

    fn finish_picture(&mut self) {
        let Some(mut c) = self.cur.take() else { return };
        deblock::deblock_picture(&mut c.pic);
        let sps = self.sps[c.sps_id].as_ref().expect("the sequence parameter set outlives its picture");
        let id = self.dpb.new_id();
        let pic = Arc::new(DecodedPic {
            planes: std::mem::take(&mut c.pic.planes),
            mb_w: c.pic.mb_w,
            mb_h: c.pic.mb_h,
            mbs: std::mem::take(&mut c.pic.mbs),
            poc: c.poc,
            id,
            crop: sps.crop,
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
        if sps.chroma_format_idc != 1 || sps.bit_depth_luma != 8 || sps.bit_depth_chroma != 8 {
            return Err(unsupported("this H.264 decoder handles 8-bit 4:2:0 video only"));
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
            self.cur = Some(Current { pic: Picture::new(sps.mb_width, sps.mb_height), sps_id, hdr: hdr.clone(), pts, poc, mbs_done: 0 });
        }
        let cur = self.cur.as_mut().unwrap();
        let sps = self.sps[sps_id].as_ref().unwrap();
        let pps = self.pps[hdr.pps_id as usize].as_ref().unwrap();
        let refs: [RefList; 2] = if hdr.slice_type == SliceType::I { [Vec::new(), Vec::new()] } else { self.dpb.ref_lists(sps, &hdr, cur.poc)? };
        let scale = LevelScale::new(pps);
        cur.pic.slices.push(SliceParams { disable_deblocking: hdr.disable_deblocking, alpha_offset: hdr.alpha_offset, beta_offset: hdr.beta_offset, chroma_qp_offset: pps.chroma_qp_offset });
        let id = cur.pic.slices.len() as u16;
        let mut dec = SliceDec::new(sps, pps, &hdr, &mut cur.pic, &scale, id, rbsp, &refs)?;
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
