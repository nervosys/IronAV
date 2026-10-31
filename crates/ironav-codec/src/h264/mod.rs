//! A software H.264 decoder (ITU-T H.264 / ISO 14496-10) for progressive video: 8- and 10-bit 4:2:0, 4:2:2 and 4:4:4, and monochrome.
//!
//! Intra pictures (I slices, CAVLC and CABAC, 4x4 / 8x8 / 16x16 prediction, I_PCM, 8x8 transform,
//! scaling matrices, multiple slices, the loop filter), P slices and B slices (all partition sizes,
//! skip and direct modes, multiple reference frames, bi-prediction, weighted prediction) are decoded
//! with CAVLC and CABAC, with reference picture marking and output reordering.

pub(crate) mod bits;
pub(crate) mod cabac;
mod cavlc;
mod deblock;
mod dpb;
mod enc;
mod idct;
mod inter;
mod intra;
mod mb;
pub(crate) mod ps;
mod shared;
pub(crate) mod tables;

use bits::{split_annexb, unescape};
pub use enc::H264Encoder;
use dpb::{DecodedPic, Dpb, Pending, RefList};
use ironav_core::{Decoder, Error, ErrorCode, Frame, Packet, PixelFormat, Result, VideoFrame};
use mb::{LevelScale, Picture, SliceDec, SliceParams};
use ps::{parse_pps, parse_slice_header, parse_sps, Pps, SliceHeader, SliceType, Sps};
use shared::{Published, Publisher};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;

pub struct H264Decoder {
    sps: Vec<Option<Sps>>,
    pps: Vec<Option<Pps>>,
    cur: Option<Current>,
    dpb: Dpb,
    out: Vec<Frame>,
    /// Pictures decoded at once, each on its own thread; 1 decodes on the caller's thread.
    threads: usize,
    /// Pictures being decoded on other threads, in decoding order.
    running: VecDeque<JoinHandle<(Picture, Option<Error>)>>,
    /// Pictures in output order, waiting for their decoding to finish.
    ready: VecDeque<Pending>,
    /// An error of a picture decoded on another thread, for the next call to return.
    failed: Option<Error>,
}

struct Current {
    /// The picture being decoded, on this thread.
    pic: Option<Picture>,
    /// The slices to decode on another thread.
    slices: Vec<SliceJob>,
    sps: Sps,
    pps: Pps,
    hdr: SliceHeader,
    pts: i64,
    poc: i32,
    mbs_done: usize,
}

struct SliceJob {
    rbsp: Vec<u8>,
    hdr: SliceHeader,
    refs: [RefList; 2],
}

/// The pixel work of a picture on another thread: its slices, then the loop filter, a macroblock
/// row behind the decoding, publishing each row as it becomes final.
fn decode_picture(sps: Sps, pps: Pps, slices: Vec<SliceJob>, poc: i32, mut pic: Picture, out: Publisher) -> (Picture, Option<Error>) {
    struct Rows {
        out: Publisher,
        bd: u32,
        /// Macroblock rows decoded, and rows filtered.
        decoded: usize,
        filtered: usize,
    }
    impl Rows {
        fn advance(&mut self, pic: &mut Picture) {
            let w = pic.mb_w;
            while self.decoded < pic.mb_h && pic.mbs[self.decoded * w..][..w].iter().all(|m| m.slice != 0) {
                self.decoded += 1;
            }
            // a row is filtered once the row below is decoded, as intra prediction there reads it
            // unfiltered; it is final once the row below is filtered too
            if self.decoded > self.filtered + 1 {
                deblock::deblock_rows(pic, self.bd, self.filtered..self.decoded - 1);
                self.filtered = self.decoded - 1;
                self.out.publish(pic, self.filtered - 1);
            }
        }
    }
    let mut rows = Rows { out, bd: sps.bit_depth_luma, decoded: 0, filtered: 0 };
    let scale = LevelScale::new(&pps);
    let mut err = None;
    for s in &slices {
        let hdr = &s.hdr;
        pic.slices.push(SliceParams { disable_deblocking: hdr.disable_deblocking, alpha_offset: hdr.alpha_offset, beta_offset: hdr.beta_offset, chroma_qp_offset: pps.chroma_qp_offset });
        let id = pic.slices.len() as u16;
        let mut on_row = |p: &mut Picture| rows.advance(p);
        let r = SliceDec::new(&sps, &pps, hdr, &mut pic, &scale, id, &s.rbsp, &s.refs, poc, Some(&mut on_row)).and_then(|mut d| d.decode());
        if let Err(e) = r {
            err.get_or_insert(e);
        }
    }
    let h = pic.mb_h;
    deblock::deblock_rows(&mut pic, rows.bd, rows.filtered..h);
    // the output frame too, here rather than on the caller's thread
    let shared = rows.out.pic();
    let frame = output(shared, [&pic.planes[0][..], &pic.planes[1], &pic.planes[2]]);
    *shared.frame.lock().unwrap_or_else(PoisonError::into_inner) = Some(frame);
    rows.out.publish(&pic, h);
    (pic, err)
}

/// The `w` x `h` samples at (x0, y0) of a plane `stride` samples wide, as bytes for 8-bit video and
/// little-endian 16-bit words otherwise.
fn window(src: &[u16], stride: usize, (x0, y0): (usize, usize), (w, h): (usize, usize), wide: bool) -> Vec<u8> {
    let mut v = Vec::with_capacity(w * h * if wide { 2 } else { 1 });
    for r in 0..h {
        let row = &src[(y0 + r) * stride + x0..][..w];
        if wide {
            for s in row {
                v.extend_from_slice(&s.to_le_bytes());
            }
        } else {
            v.extend(row.iter().map(|&s| s as u8));
        }
    }
    v
}

/// The output frame of a picture whose samples are `planes`.
fn output(pic: &DecodedPic, planes: [&[u16]; 3]) -> VideoFrame {
    let [cl, cr, ct, cb] = pic.crop;
    let (w, h) = (pic.mb_w * 16 - cl - cr, pic.mb_h * 16 - ct - cb);
    let c422 = pic.cf == 2;
    let c444 = pic.cf == 3;
    let (cw, ch) = (if c444 { w } else { w.div_ceil(2) }, if c422 || c444 { h } else { h.div_ceil(2) });
    let wide = pic.bit_depth > 8;
    let gray = pic.cf == 0 && !wide;
    let mut out = vec![window(planes[0], pic.mb_w * 16, (cl, ct), (w, h), wide)];
    let cs = pic.mb_w * pic.cw;
    let (cx, cy) = (if c444 { cl } else { cl / 2 }, if c422 || c444 { ct } else { ct / 2 });
    for pl in 1..if gray { 1 } else { 3 } {
        out.push(window(planes[pl], cs, (cx, cy), (cw, ch), wide));
    }
    let pix_fmt = match (c422, wide) {
        _ if gray => PixelFormat::Gray8,
        _ if c444 && wide => PixelFormat::Yuv444p10,
        _ if c444 => PixelFormat::Yuv444p,
        (true, true) => PixelFormat::Yuv422p10,
        (true, false) => PixelFormat::Yuv422p,
        (false, true) => PixelFormat::Yuv420p10,
        (false, false) => PixelFormat::Yuv420p,
    };
    VideoFrame { width: w as u32, height: h as u32, pix_fmt, pts: 0, planes: out }
}

fn unsupported(msg: &str) -> Error {
    Error::new(ErrorCode::UnsupportedCodec, msg.to_string())
}

impl H264Decoder {
    /// Decodes up to as many pictures at once as there are cores, at most 12. Pictures then come
    /// out a few calls later than with one thread.
    pub fn new() -> Self {
        Self::with_threads(std::thread::available_parallelism().map_or(1, |n| n.get()).min(12))
    }

    /// Decodes up to `n` pictures at once, each on its own thread; 1 decodes on the caller's thread.
    pub fn with_threads(n: usize) -> Self {
        let n = n.max(1);
        let mut dpb = Dpb::default();
        // a picture in flight holds two buffers: the one it is decoded in and the one it is shared in
        dpb.spare_extra = if n > 1 { n } else { 0 };
        H264Decoder { sps: vec![None; 32], pps: vec![None; 256], cur: None, dpb, out: Vec::new(), threads: n, running: VecDeque::new(), ready: VecDeque::new(), failed: None }
    }

    /// For live sources: when the stream does not declare how deep its reordering goes (VUI
    /// `num_reorder_frames`), pictures are output at once, and the depth grows only when a picture
    /// turns up that should have been shown earlier (that one is not output). Otherwise, as
    /// `with_threads(1)`: on the caller's thread, so that no picture waits for others.
    pub fn low_delay() -> Self {
        let mut d = Self::with_threads(1);
        d.dpb.low_delay = true;
        d
    }

    /// Pictures low-delay mode left out because they came after a later one.
    pub fn late_pictures(&self) -> u64 {
        self.dpb.late
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
        let frame = pic.frame.lock().unwrap_or_else(PoisonError::into_inner).take();
        let mut frame = frame.unwrap_or_else(|| output(pic, [0, 1, 2].map(|pl| pic.data.plane(pl, pic.mb_h))));
        frame.pts = p.pts;
        self.dpb.release(p.pic);
        self.out.push(Frame::Video(frame));
    }

    fn finish_picture(&mut self) {
        let Some(mut c) = self.cur.take() else { return };
        let (mb_w, mb_h, cf, bd, crop) = (c.sps.mb_width, c.sps.mb_height, c.sps.chroma_format_idc as u8, c.sps.bit_depth_luma, c.sps.crop);
        let id = self.dpb.new_id();
        // memory_management_control_operation 5 makes the picture's order count 0 (8.2.1): its
        // frame order count is the lesser of its fields', which is subtracted from both
        let mmco5 = c.hdr.nal_ref_idc != 0 && c.hdr.marking.adaptive && c.hdr.marking.ops.iter().any(|o| o.0 == 5);
        let poc = if mmco5 { 0 } else { c.poc };
        let decoded = |data| DecodedPic { data, frame: Mutex::new(None), mb_w, mb_h, cf, cw: if cf == 3 { 16 } else { 8 }, poc, id, crop, bit_depth: bd };
        let pic = match c.pic.take() {
            Some(mut p) => {
                deblock::deblock_picture(&mut p, bd);
                Arc::new(decoded(Published::new((p.planes, p.mbs), mb_w, mb_h)))
            }
            None => {
                while self.running.len() >= self.threads {
                    self.join_oldest();
                }
                let work = Picture::with_spare(mb_w, mb_h, cf, bd, self.dpb.spare.pop());
                let to = Picture::with_spare(mb_w, mb_h, cf, bd, self.dpb.spare.pop());
                let (pic, out) = Publisher::new(decoded(Published::new((to.planes, to.mbs), mb_w, mb_h)));
                let (sps, pps, slices, poc) = (c.sps.clone(), c.pps.clone(), std::mem::take(&mut c.slices), c.poc);
                match std::thread::Builder::new().name("h264".into()).spawn(move || decode_picture(sps, pps, slices, poc, work, out)) {
                    Ok(h) => self.running.push_back(h),
                    Err(e) => {
                        self.failed.get_or_insert(Error::new(ErrorCode::Internal, format!("could not start an H.264 decoding thread: {e}")));
                    }
                }
                pic
            }
        };
        let ready = self.dpb.finish(&c.sps, &c.hdr, pic, c.pts);
        self.ready.extend(ready);
        self.emit_ready(false);
    }

    /// Waits for the oldest picture decoded on another thread and keeps its error and buffers.
    fn join_oldest(&mut self) {
        let Some(h) = self.running.pop_front() else { return };
        match h.join() {
            Ok((pic, err)) => {
                if let Some(e) = err {
                    self.failed.get_or_insert(e);
                }
                self.dpb.keep((pic.planes, pic.mbs));
            }
            Err(_) => {
                self.failed.get_or_insert(Error::new(ErrorCode::Internal, "an H.264 decoding thread panicked"));
            }
        }
    }

    /// Outputs the pictures due whose decoding has finished, or all of them, waiting for them.
    fn emit_ready(&mut self, wait: bool) {
        while let Some(p) = self.ready.front() {
            if !wait && !p.pic.data.is_complete() {
                break;
            }
            let p = self.ready.pop_front().unwrap();
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
        if sps.chroma_format_idc > 3 || sps.bit_depth_luma != sps.bit_depth_chroma || !(8..=10).contains(&sps.bit_depth_luma) {
            return Err(unsupported("this H.264 decoder handles monochrome, 4:2:0, 4:2:2 and 4:4:4 video with 8 or 10 bits per sample only"));
        }
        let starts_new = match &self.cur {
            Some(c) => Self::new_picture(&c.hdr, &hdr),
            None => true,
        };
        if starts_new {
            self.finish_picture();
            let sps = self.sps[sps_id].as_ref().unwrap();
            let pps = self.pps[hdr.pps_id as usize].as_ref().unwrap();
            let poc = self.dpb.compute_poc(sps, &hdr);
            let pic = (self.threads == 1).then(|| Picture::with_spare(sps.mb_width, sps.mb_height, sps.chroma_format_idc as u8, sps.bit_depth_luma, self.dpb.spare.pop()));
            self.cur = Some(Current { pic, slices: Vec::new(), sps: sps.clone(), pps: pps.clone(), hdr: hdr.clone(), pts, poc, mbs_done: 0 });
        }
        let cur = self.cur.as_mut().unwrap();
        let sps = self.sps[sps_id].as_ref().unwrap();
        let pps = self.pps[hdr.pps_id as usize].as_ref().unwrap();
        let refs: [RefList; 2] = if hdr.slice_type == SliceType::I { [Vec::new(), Vec::new()] } else { self.dpb.ref_lists(sps, &hdr, cur.poc)? };
        let Some(pic) = cur.pic.as_mut() else {
            cur.slices.push(SliceJob { rbsp: rbsp.to_vec(), hdr: hdr.clone(), refs });
            cur.hdr = hdr;
            return Ok(());
        };
        let scale = LevelScale::new(pps);
        pic.slices.push(SliceParams { disable_deblocking: hdr.disable_deblocking, alpha_offset: hdr.alpha_offset, beta_offset: hdr.beta_offset, chroma_qp_offset: pps.chroma_qp_offset });
        let id = pic.slices.len() as u16;
        let mut dec = SliceDec::new(sps, pps, &hdr, pic, &scale, id, rbsp, &refs, cur.poc, None)?;
        let r = dec.decode();
        cur.mbs_done = pic.mbs.iter().filter(|m| m.slice != 0).count();
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
        // a packet holds whole access units, so a complete picture is ready now (decoding on other
        // threads, the picture starts when the next one does)
        if let Some(c) = &self.cur {
            if c.pic.as_ref().is_some_and(|p| c.mbs_done == p.mb_w * p.mb_h) {
                self.finish_picture();
            }
        }
        while self.running.front().is_some_and(|h| h.is_finished()) {
            self.join_oldest();
        }
        self.emit_ready(false);
        if let Some(e) = err.or_else(|| self.failed.take()) {
            return Err(e);
        }
        Ok(std::mem::take(&mut self.out))
    }

    fn flush(&mut self) -> Result<Vec<Frame>> {
        self.finish_picture();
        while !self.running.is_empty() {
            self.join_oldest();
        }
        let ready = self.dpb.drain_all();
        self.ready.extend(ready);
        self.emit_ready(true);
        if let Some(e) = self.failed.take() {
            return Err(e);
        }
        Ok(std::mem::take(&mut self.out))
    }
}
