//! A software HEVC (H.265) decoder: Main, Main 10 and the range extension profiles' chroma formats
//! (monochrome, 4:2:0, 4:2:2 and 4:4:4, 8 to 12 bits), with tiles, wavefronts, multiple slices, sample
//! adaptive offset, the deblocking filter and inter prediction (merge and AMVP, temporal candidates,
//! weighted prediction). The range extension coding tools (RDPCM, cross-component prediction and the
//! rest) are not implemented; a stream that turns one on is refused, naming it.

#![allow(dead_code)]

mod cabac;
mod filter;
mod frame;
mod inter;
mod intra;
pub(crate) mod ps;
mod slice;
mod tables;
mod transform;

use crate::h264::bits::{split_annexb, unescape};
use frame::{CurPic, Frame, Geometry, SliceInfo};
use ironav_core::{Decoder, Error, ErrorCode, Frame as OutFrame, Packet, PixelFormat, Result, VideoFrame};
use ps::{parse_pps, parse_slice_header, parse_sps, Pps, SliceHeader, SliceType, Sps};
use slice::{active_scaling_list, Carry, Dec, SliceCtx};
use std::sync::Arc;
use transform::ScalingFactors;

fn unsupported(msg: &str) -> Error {
    Error::new(ErrorCode::UnsupportedCodec, msg.to_string())
}

/// A reference picture of a slice's list.
#[derive(Clone)]
pub struct RefPic {
    pub frame: Arc<Frame>,
    pub poc: i32,
    pub long_term: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Marking {
    Unused,
    Short,
    Long,
}

struct DpbEntry {
    frame: Arc<Frame>,
    marking: Marking,
    needed_for_output: bool,
}

struct Cur {
    pic: CurPic,
    sps: Arc<Sps>,
    pps: Arc<Pps>,
    geo: Arc<Geometry>,
    scaling: Option<Arc<ScalingFactors>>,
    poc: i32,
    output: bool,
    slice_addr_rs: i32,
    slice_idx: u16,
    refs: [Vec<RefPic>; 2],
    st_before: Vec<i32>,
    st_after: Vec<i32>,
    lt_curr: Vec<i32>,
}

pub struct HevcDecoder {
    sps: Vec<Option<Arc<Sps>>>,
    pps: Vec<Option<Arc<Pps>>>,
    cur: Option<Cur>,
    dpb: Vec<DpbEntry>,
    out: Vec<OutFrame>,
    prev_tid0_poc: i32,
    first_picture: bool,
    after_eos: bool,
    /// NoRaslOutputFlag of the last IRAP picture.
    no_rasl_output: bool,
    skipping_picture: bool,
    last_hdr: Option<SliceHeader>,
    carry: Carry,
    geo_cache: Option<(usize, usize, Arc<Geometry>)>,
}

impl Default for HevcDecoder {
    fn default() -> Self {
        Self::new()
    }
}

impl HevcDecoder {
    pub fn new() -> Self {
        HevcDecoder {
            sps: vec![None; 16],
            pps: vec![None; 64],
            cur: None,
            dpb: Vec::new(),
            out: Vec::new(),
            prev_tid0_poc: 0,
            first_picture: true,
            after_eos: false,
            no_rasl_output: true,
            skipping_picture: false,
            last_hdr: None,
            carry: Carry::default(),
            geo_cache: None,
        }
    }

    // ------------------------------------------------------------ output

    fn emit(&mut self, f: &Frame) {
        let [cl, cr, ct, cb] = f.crop;
        let (w, h) = (f.width - cl - cr, f.height - ct - cb);
        let wide = f.bit_depth > 8;
        let put = |v: &mut Vec<u8>, src: &[u16]| {
            if wide {
                for s in src {
                    v.extend_from_slice(&s.to_le_bytes());
                }
            } else {
                v.extend(src.iter().map(|&s| s as u8));
            }
        };
        let (sx, sy) = match f.chroma_format {
            1 => (1, 1),
            2 => (1, 0),
            _ => (0, 0),
        };
        let gray = f.chroma_format == 0 && !wide;
        let mut planes = Vec::with_capacity(3);
        let mut y = Vec::with_capacity(w * h);
        for r in 0..h {
            put(&mut y, &f.planes[0][(ct + r) * f.width + cl..(ct + r) * f.width + cl + w]);
        }
        planes.push(y);
        if f.chroma_format == 0 {
            if !gray {
                // monochrome deeper than 8 bits goes out as 4:2:0 with flat chroma
                let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
                let flat = 1u16 << (f.bit_depth - 1);
                for _ in 0..2 {
                    let mut v = Vec::new();
                    put(&mut v, &vec![flat; cw * ch]);
                    planes.push(v);
                }
            }
        } else {
            let (cw, ch) = (w >> sx, h >> sy);
            for pl in 1..3 {
                let mut v = Vec::with_capacity(cw * ch);
                for r in 0..ch {
                    let o = ((ct >> sy) + r) * f.cwidth + (cl >> sx);
                    put(&mut v, &f.planes[pl][o..o + cw]);
                }
                planes.push(v);
            }
        }
        let layout = match f.chroma_format {
            0 | 1 => PixelFormat::Yuv420p,
            2 => PixelFormat::Yuv422p,
            _ => PixelFormat::Yuv444p,
        };
        let pix_fmt = if gray { PixelFormat::Gray8 } else { layout.with_depth(f.bit_depth as u8).expect("8, 10 or 12 bits") };
        self.out.push(OutFrame::Video(VideoFrame { width: w as u32, height: h as u32, pix_fmt, pts: f.pts, planes }));
    }

    /// Outputs the picture with the smallest POC among those waiting.
    fn bump(&mut self) -> bool {
        let Some(i) = self.dpb.iter().enumerate().filter(|(_, e)| e.needed_for_output).min_by_key(|(_, e)| e.frame.poc).map(|(i, _)| i) else {
            return false;
        };
        self.dpb[i].needed_for_output = false;
        let f = self.dpb[i].frame.clone();
        self.emit(&f);
        true
    }

    fn waiting(&self) -> usize {
        self.dpb.iter().filter(|e| e.needed_for_output).count()
    }

    // ------------------------------------------------------------ picture start and end

    fn finish_picture(&mut self) {
        let Some(mut c) = self.cur.take() else { return };
        filter::run(&mut c.pic, &c.sps, &c.pps, &c.geo);
        let frame = Arc::new(c.pic.frame);
        self.dpb.push(DpbEntry { frame, marking: Marking::Short, needed_for_output: c.output });
        let max_reorder = c.sps.max_num_reorder as usize;
        while self.waiting() > max_reorder {
            self.bump();
        }
    }

    fn missing_frame(sps: &Sps, poc: i32) -> Frame {
        CurPic::new(sps, 0, poc).frame
    }

    /// Reference picture set derivation and marking (8.3.2). Returns the POCs of the pictures the
    /// current one may refer to: before, after and long-term.
    fn apply_rps(&mut self, sps: &Sps, hdr: &SliceHeader, poc: i32) -> (Vec<i32>, Vec<i32>, Vec<i32>) {
        let max_lsb = 1i32 << sps.log2_max_poc_lsb;
        let (mut before, mut after, mut lt_curr) = (Vec::new(), Vec::new(), Vec::new());
        let mut keep_short = Vec::new();
        let mut keep_long: Vec<(i32, bool)> = Vec::new();
        if !hdr.idr() {
            for (i, &d) in hdr.st_rps.delta_poc_s0.iter().enumerate() {
                keep_short.push(poc + d);
                if hdr.st_rps.used_s0[i] {
                    before.push(poc + d);
                }
            }
            for (i, &d) in hdr.st_rps.delta_poc_s1.iter().enumerate() {
                keep_short.push(poc + d);
                if hdr.st_rps.used_s1[i] {
                    after.push(poc + d);
                }
            }
            for &(v, msb, used) in &hdr.lt {
                let full = if msb { v + (poc & !(max_lsb - 1)) } else { v };
                keep_long.push((full, msb));
                if used {
                    lt_curr.push((full, msb));
                }
            }
        }
        let mut long_pocs = Vec::new();
        for &(v, msb) in &keep_long {
            if let Some(e) = self.dpb.iter().find(|e| e.marking != Marking::Unused && if msb { e.frame.poc == v } else { (e.frame.poc & (max_lsb - 1)) == v }) {
                long_pocs.push(e.frame.poc);
            }
        }
        for e in self.dpb.iter_mut() {
            let p = e.frame.poc;
            if long_pocs.contains(&p) {
                e.marking = Marking::Long;
            } else if e.marking == Marking::Short && keep_short.contains(&p) {
            } else {
                e.marking = Marking::Unused;
            }
        }
        let lt_full: Vec<i32> = lt_curr
            .iter()
            .map(|&(v, msb)| self.dpb.iter().find(|e| e.marking == Marking::Long && if msb { e.frame.poc == v } else { (e.frame.poc & (max_lsb - 1)) == v }).map_or(v, |e| e.frame.poc))
            .collect();
        (before, after, lt_full)
    }

    fn find_ref(&mut self, sps: &Sps, poc: i32, long: bool) -> RefPic {
        if let Some(e) = self.dpb.iter().find(|e| e.frame.poc == poc && e.marking != Marking::Unused) {
            return RefPic { frame: e.frame.clone(), poc, long_term: long };
        }
        // a reference the stream does not have (after random access, or damage): substitute grey
        let frame = Arc::new(Self::missing_frame(sps, poc));
        self.dpb.push(DpbEntry { frame: frame.clone(), marking: if long { Marking::Long } else { Marking::Short }, needed_for_output: false });
        RefPic { frame, poc, long_term: long }
    }

    fn build_ref_lists(&mut self, sps: &Sps, hdr: &SliceHeader) -> [Vec<RefPic>; 2] {
        let c = self.cur.as_ref().unwrap();
        let (before, after, lt) = (c.st_before.clone(), c.st_after.clone(), c.lt_curr.clone());
        let mut lists: [Vec<RefPic>; 2] = [Vec::new(), Vec::new()];
        if hdr.slice_type == SliceType::I {
            return lists;
        }
        for l in 0..if hdr.slice_type == SliceType::B { 2 } else { 1 } {
            let (first, second) = if l == 0 { (&before, &after) } else { (&after, &before) };
            let total = first.len() + second.len() + lt.len();
            let n = hdr.num_ref_idx[l].max(total);
            let mut temp: Vec<(i32, bool)> = Vec::new();
            if total > 0 {
                while temp.len() < n {
                    for &p in first.iter() {
                        if temp.len() < n {
                            temp.push((p, false));
                        }
                    }
                    for &p in second.iter() {
                        if temp.len() < n {
                            temp.push((p, false));
                        }
                    }
                    for &p in lt.iter() {
                        if temp.len() < n {
                            temp.push((p, true));
                        }
                    }
                }
            }
            for i in 0..hdr.num_ref_idx[l] {
                let idx = match &hdr.list_mod[l] {
                    Some(m) => m[i] as usize,
                    None => i,
                };
                if let Some(&(poc, long)) = temp.get(idx) {
                    let r = self.find_ref(sps, poc, long);
                    lists[l].push(r);
                }
            }
        }
        lists
    }

    fn start_picture(&mut self, nal_type: u8, tid: u8, hdr: &SliceHeader, pts: i64) -> Result<()> {
        let pps = self.pps[hdr.pps_id].clone().ok_or_else(|| unsupported("missing PPS"))?;
        let sps = self.sps[pps.sps_id].clone().ok_or_else(|| unsupported("missing SPS"))?;
        if sps.bit_depth_luma != sps.bit_depth_chroma || !(8..=12).contains(&sps.bit_depth_luma) {
            return Err(unsupported("this HEVC decoder handles equal luma and chroma bit depths of 8 to 12 only"));
        }
        if let Some(tool) = sps.range_tool.or(pps.range_tool) {
            return Err(unsupported(&format!("this HEVC decoder does not implement the range extension tool \"{tool}\"")));
        }
        let irap = hdr.irap();
        if irap {
            self.no_rasl_output = matches!(nal_type, 16..=20) || self.first_picture || self.after_eos;
        }
        // RASL pictures of a random access point that starts the stream cannot be decoded
        if matches!(nal_type, 8 | 9) && self.no_rasl_output {
            self.skipping_picture = true;
            return Ok(());
        }
        self.skipping_picture = false;
        // picture order count (8.3.1)
        let max_lsb = 1i32 << sps.log2_max_poc_lsb;
        let lsb = hdr.poc_lsb as i32;
        let msb = if irap && self.no_rasl_output {
            0
        } else {
            let prev_lsb = self.prev_tid0_poc & (max_lsb - 1);
            let prev_msb = self.prev_tid0_poc - prev_lsb;
            if lsb < prev_lsb && prev_lsb - lsb >= max_lsb / 2 {
                prev_msb + max_lsb
            } else if lsb > prev_lsb && lsb - prev_lsb > max_lsb / 2 {
                prev_msb - max_lsb
            } else {
                prev_msb
            }
        };
        let poc = msb + lsb;
        if tid == 0 && !matches!(nal_type, 6..=9) && !(nal_type <= 14 && nal_type % 2 == 0) {
            self.prev_tid0_poc = poc;
        }
        if irap && self.no_rasl_output && !self.first_picture {
            // a new coded video sequence (C.5.2.2): what is still waiting goes out first, unless
            // the picture says to drop it (no_output_of_prior_pics_flag, taken as set for a CRA
            // after an end of sequence, as the HM and FFmpeg take it)
            let drop = hdr.no_output_of_prior_pics || (nal_type == 21 && self.after_eos);
            if !drop {
                while self.bump() {}
            }
            self.dpb.clear();
        }
        self.first_picture = false;
        self.after_eos = false;
        let (before, after, lt) = self.apply_rps(&sps, hdr, poc);
        // free what is neither referenced nor waiting for output, then make room by outputting
        self.dpb.retain(|e| e.marking != Marking::Unused || e.needed_for_output);
        while self.waiting() > sps.max_num_reorder as usize || (self.dpb.len() >= sps.max_dec_pic_buffering as usize && self.waiting() > 0) {
            if !self.bump() {
                break;
            }
            self.dpb.retain(|e| e.marking != Marking::Unused || e.needed_for_output);
        }
        let geo = match &self.geo_cache {
            Some((s, p, g)) if *s == sps.id && *p == pps.id => g.clone(),
            _ => {
                let g = Arc::new(Geometry::new(&sps, &pps));
                self.geo_cache = Some((sps.id, pps.id, g.clone()));
                g
            }
        };
        let scaling = active_scaling_list(&sps, &pps).map(|l| Arc::new(ScalingFactors::new(&l)));
        let output = hdr.pic_output;
        self.cur = Some(Cur {
            pic: CurPic::new(&sps, pts, poc),
            sps,
            pps,
            geo,
            scaling,
            poc,
            output,
            slice_addr_rs: 0,
            slice_idx: 0,
            refs: [Vec::new(), Vec::new()],
            st_before: before,
            st_after: after,
            lt_curr: lt,
        });
        self.carry = Carry::default();
        Ok(())
    }

    fn slice(&mut self, nal_type: u8, tid: u8, rbsp: &[u8], pts: i64) -> Result<()> {
        let hdr = parse_slice_header(rbsp, nal_type, tid, &self.sps, &self.pps, self.last_hdr.as_ref())?;
        if hdr.first_slice_segment {
            self.finish_picture();
            self.start_picture(nal_type, tid, &hdr, pts)?;
        }
        if self.skipping_picture || self.cur.is_none() {
            return Ok(());
        }
        self.last_hdr = Some(hdr.clone());
        if !hdr.dependent {
            let sps = self.cur.as_ref().unwrap().sps.clone();
            let refs = self.build_ref_lists(&sps, &hdr);
            let c = self.cur.as_mut().unwrap();
            c.refs = refs;
            c.slice_idx = c.pic.slices.len() as u16;
            c.slice_addr_rs = hdr.segment_address as i32;
            c.pic.slices.push(SliceInfo {
                deblocking_disabled: hdr.deblocking_disabled,
                beta_offset_div2: hdr.beta_offset_div2,
                tc_offset_div2: hdr.tc_offset_div2,
                loop_filter_across_slices: hdr.loop_filter_across_slices,
                cb_qp_offset: hdr.cb_qp_offset,
                cr_qp_offset: hdr.cr_qp_offset,
            });
        }
        let c = self.cur.as_mut().unwrap();
        let ctx = SliceCtx { sps: &c.sps, pps: &c.pps, hdr: &hdr, geo: &c.geo, scaling: c.scaling.as_deref(), slice_idx: c.slice_idx, slice_addr_rs: c.slice_addr_rs, refs: &c.refs, poc: c.poc };
        let mut dec = Dec::new(&ctx, &mut c.pic, rbsp, &self.carry)?;
        dec.run(&mut self.carry)
    }
}

impl Decoder for HevcDecoder {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<OutFrame>> {
        let mut err = None;
        for nal in split_annexb(&packet.data) {
            // an end of sequence or of bitstream is the 2-byte header alone
            if nal.len() < 2 {
                continue;
            }
            let ty = (nal[0] >> 1) & 63;
            let layer = ((nal[0] & 1) << 5) | (nal[1] >> 3);
            let tid = (nal[1] & 7).saturating_sub(1);
            if layer != 0 {
                continue;
            }
            let rbsp = unescape(&nal[2..]);
            let r = match ty {
                33 => parse_sps(&rbsp).map(|s| {
                    let id = s.id;
                    self.sps[id] = Some(Arc::new(s));
                    self.geo_cache = None;
                }),
                34 => parse_pps(&rbsp).map(|p| {
                    let id = p.id;
                    self.pps[id] = Some(Arc::new(p));
                    self.geo_cache = None;
                }),
                36 | 37 => {
                    self.finish_picture();
                    self.after_eos = true;
                    Ok(())
                }
                0..=9 | 16..=21 => self.slice(ty, tid, &rbsp, packet.pts),
                _ => Ok(()),
            };
            if let Err(e) = r {
                err = Some(e);
                break;
            }
        }
        if let Some(c) = &self.cur {
            if c.pic.ctbs_done == c.sps.ctb_w * c.sps.ctb_h {
                self.finish_picture();
            }
        }
        if let Some(e) = err {
            return Err(e);
        }
        Ok(std::mem::take(&mut self.out))
    }

    fn flush(&mut self) -> Result<Vec<OutFrame>> {
        self.finish_picture();
        while self.bump() {}
        Ok(std::mem::take(&mut self.out))
    }
}
