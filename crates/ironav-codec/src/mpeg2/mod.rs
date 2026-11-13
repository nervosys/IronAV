//! MPEG-2 video (ITU-T H.262 / ISO/IEC 13818-2) and MPEG-1 video (ISO/IEC 11172-2) decoding: I, P
//! and B pictures, frame and field pictures, field and frame DCT, frame, field, 16x8 and dual prime
//! motion, both scans, both intra VLC tables, non-linear quantisation, custom matrices, and 4:2:0
//! and 4:2:2 chroma. Interlaced pictures come out as frames with their fields woven, as FFmpeg
//! outputs them. 4:4:4 and the scalable profiles are rejected with an error; concealment motion
//! vectors are parsed and ignored.
//!
//! The headers, the macroblock layer, the reconstruction and the picture reordering follow
//! FFmpeg's `mpeg12dec.c`, `mpeg12.c`, `mpeg12data.c`, `mpegvideo_motion.c` and `mpegvideo_dec.c`
//! (Fabrice Bellard, Michael Niedermayer and the FFmpeg developers, LGPL-2.1-or-later), so that
//! the output matches FFmpeg's decoder with `-idct simple`. The bit reader, the VLC lookup and the
//! 8x8 inverse DCT are those of the MPEG-4 decoder.

mod mb;
mod mc;
pub mod enc;
pub use enc::Mpeg2Encoder;
mod tables;

use crate::mpeg4::bits::{Bits, Vlc};
use ironav_core::{Decoder, Error, ErrorCode, Frame as OutFrame, Packet, PixelFormat, Result, VideoFrame};
use mc::Plane;
use std::sync::Arc;
use tables as T;

fn unsupported(msg: &str) -> Error {
    Error::new(ErrorCode::UnsupportedFormat, format!("MPEG-2: {msg}"))
}

/// The macroblock-aligned geometry of the pictures of a sequence.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) struct Geo {
    pub width: usize,
    pub height: usize,
    pub mbw: usize,
    /// Macroblock rows of a frame: even when the sequence can be interlaced.
    pub mbh: usize,
    /// 1 for 4:2:0, 2 for 4:2:2.
    pub fmt: u8,
}

impl Geo {
    fn chroma_rows(&self) -> usize {
        if self.fmt == 1 {
            self.mbh * 8
        } else {
            self.mbh * 16
        }
    }
}

/// A decoded frame: the planes at the macroblock-aligned size, and the time of its packet.
#[derive(Clone)]
pub(crate) struct Frame {
    pub p: [Plane; 3],
    pub pts: i64,
}

impl Frame {
    fn new(g: &Geo, pts: i64, fill: u8) -> Self {
        Frame {
            p: [Plane::new(g.mbw * 16, g.mbh * 16, fill), Plane::new(g.mbw * 8, g.chroma_rows(), fill), Plane::new(g.mbw * 8, g.chroma_rows(), fill)],
            pts,
        }
    }

    /// The frame cropped to the picture's size.
    fn video(&self, g: &Geo) -> VideoFrame {
        let (w, h) = (g.width, g.height);
        let cw = w.div_ceil(2);
        let ch = if g.fmt == 1 { h.div_ceil(2) } else { h };
        let crop = |p: &Plane, w: usize, h: usize| -> Vec<u8> {
            let mut out = Vec::with_capacity(w * h);
            for row in p.d.chunks(p.stride).take(h) {
                out.extend_from_slice(&row[..w]);
            }
            out
        };
        VideoFrame {
            width: w as u32,
            height: h as u32,
            pix_fmt: if g.fmt == 1 { PixelFormat::Yuv420p } else { PixelFormat::Yuv422p },
            pts: self.pts,
            planes: vec![crop(&self.p[0], w, h), crop(&self.p[1], cw, ch), crop(&self.p[2], cw, ch)],
        }
    }
}

/// The quantiser matrices, in raster order.
#[derive(Clone)]
pub(crate) struct Matrices {
    pub intra: [u8; 64],
    pub chroma_intra: [u8; 64],
    pub inter: [u8; 64],
    pub chroma_inter: [u8; 64],
}

impl Matrices {
    fn default_matrices() -> Self {
        Matrices { intra: T::INTRA_MATRIX, chroma_intra: T::INTRA_MATRIX, inter: T::INTER_MATRIX, chroma_inter: T::INTER_MATRIX }
    }
}

/// What a picture header and its coding extension say.
#[derive(Clone)]
pub(crate) struct PicHdr {
    /// 1 for I, 2 for P, 3 for B.
    pub ptype: u8,
    pub full_pel: [bool; 2],
    pub fcode: [[i32; 2]; 2],
    pub intra_dc_precision: u32,
    /// 1 for a top field, 2 for a bottom field, 3 for a frame.
    pub structure: u8,
    pub tff: bool,
    pub frame_pred_frame_dct: bool,
    pub concealment: bool,
    pub q_scale_type: bool,
    pub intra_vlc_format: bool,
    pub alternate_scan: bool,
    pub progressive_sequence: bool,
}

impl PicHdr {
    /// The fields an MPEG-1 picture implies.
    fn mpeg1(ptype: u8) -> Self {
        PicHdr {
            ptype,
            full_pel: [false; 2],
            fcode: [[1; 2]; 2],
            intra_dc_precision: 0,
            structure: 3,
            tff: false,
            frame_pred_frame_dct: true,
            concealment: false,
            q_scale_type: false,
            intra_vlc_format: false,
            alternate_scan: false,
            progressive_sequence: true,
        }
    }
}

/// The VLCs, built once.
pub(crate) struct Tables {
    pub dc_lum: Vlc,
    pub dc_chroma: Vlc,
    pub mbincr: Vlc,
    pub mbpat: Vlc,
    pub mv: Vlc,
    pub ptype: Vlc,
    pub btype: Vlc,
    /// The run-level codes: the MPEG-1 table, and the second intra table of MPEG-2.
    pub rl1: Vlc,
    pub rl2: Vlc,
}

impl Tables {
    fn new() -> Self {
        let pairs8 = |t: &[u8]| Vlc::new(t.chunks(2).map(|p| (p[0] as u32, p[1] as u32)));
        let rl = |t: &[u16]| Vlc::new(t.chunks(2).map(|p| (p[0] as u32, p[1] as u32)));
        Tables {
            dc_lum: Vlc::new(T::DC_LUM_CODE.iter().zip(&T::DC_LUM_BITS).map(|(&c, &b)| (c as u32, b as u32))),
            dc_chroma: Vlc::new(T::DC_CHROMA_CODE.iter().zip(&T::DC_CHROMA_BITS).map(|(&c, &b)| (c as u32, b as u32))),
            mbincr: pairs8(&T::MBINCR),
            mbpat: pairs8(&T::MBPAT),
            mv: pairs8(&T::MV),
            ptype: Vlc::new(T::PTYPE_CODE.iter().copied()),
            btype: Vlc::new(T::BTYPE_CODE.iter().copied()),
            rl1: rl(&T::MPEG1_VLC),
            rl2: rl(&T::MPEG2_VLC),
        }
    }
}

/// Everything a slice needs to know besides its own bits.
pub(crate) struct Env<'a> {
    pub t: &'a Tables,
    pub g: &'a Geo,
    pub m: &'a Matrices,
    pub h: &'a PicHdr,
    pub mpeg2: bool,
    /// The picture is a frame, or the first field of one.
    pub first_field: bool,
    /// The forward and backward references (stand-ins when the stream has not given them).
    pub fwd: &'a Frame,
    pub bwd: &'a Frame,
    /// The frame being decoded as it was when its second field began, for a P field to predict from
    /// the first field.
    pub same: Option<&'a Frame>,
}

/// What a sequence header (and its extension) says, for a demuxer.
#[derive(Debug, Clone, PartialEq)]
pub struct SequenceInfo {
    pub width: u32,
    pub height: u32,
    /// The frame rate, as numerator and denominator.
    pub frame_rate: (u32, u32),
    /// Whether the sequence has the MPEG-2 extension.
    pub mpeg2: bool,
    /// 1 for 4:2:0, 2 for 4:2:2, 3 for 4:4:4.
    pub chroma_format: u8,
    pub progressive: bool,
}

const FRAME_RATES: [(u32, u32); 9] = [(0, 0), (24000, 1001), (24, 1), (25, 1), (30000, 1001), (30, 1), (50, 1), (60000, 1001), (60, 1)];

/// Each start-code unit of `d`: its code and the payload's byte range.
fn units(d: &[u8]) -> Vec<(u8, usize, usize)> {
    crate::mpeg4::units(d)
}

/// The sequence header, with its extension if one follows, in `d` (a stream of start-code units).
pub fn sequence_info(d: &[u8]) -> Option<SequenceInfo> {
    let u = units(d);
    let k = u.iter().position(|&(c, _, _)| c == 0xB3)?;
    let (_, s, e) = u[k];
    let h = &d[s..e];
    if h.len() < 8 {
        return None;
    }
    let mut br = Bits::new(h);
    let (mut width, mut height) = (br.read(12), br.read(12));
    br.skip(4);
    let rate = br.read(4) as usize;
    let (mut fr, mut chroma_format, mut progressive, mut mpeg2) = (FRAME_RATES.get(rate).copied().unwrap_or((0, 0)), 1u8, true, false);
    if let Some(&(0xB5, s2, e2)) = u.get(k + 1) {
        let x = &d[s2..e2];
        if x.len() >= 6 && x[0] >> 4 == 1 {
            let mut b = Bits::new(x);
            b.skip(4 + 1 + 3 + 4);
            progressive = b.bit();
            chroma_format = b.read(2).max(1) as u8;
            width |= b.read(2) << 12;
            height |= b.read(2) << 12;
            b.skip(12 + 1 + 8 + 1);
            let (n, dd) = (b.read(2) + 1, b.read(5) + 1);
            fr = (fr.0 * n, fr.1 * dd);
            mpeg2 = true;
        }
    }
    if width == 0 || height == 0 || fr.0 == 0 {
        return None;
    }
    Some(SequenceInfo { width, height, frame_rate: fr, mpeg2, chroma_format, progressive })
}

/// The picture in progress between its header and the end of its slices.
struct PicState {
    h: PicHdr,
    pts: i64,
    first_slice: bool,
    skip: bool,
}

/// The MPEG-1 and MPEG-2 video decoder. A packet holds whole pictures (or a frame's two fields).
pub struct Mpeg2Decoder {
    t: Tables,
    width: usize,
    height: usize,
    have_seq: bool,
    mpeg2: bool,
    progressive_sequence: bool,
    chroma_format: u8,
    low_delay: bool,
    m: Matrices,
    closed_gop: bool,
    sync: bool,
    /// The previous picture's header: the extension fields persist until the next extension.
    hdr: PicHdr,
    pic: Option<PicState>,
    geo: Option<Geo>,
    /// The frame being decoded, and whether the next field is the first of its frame.
    cur: Option<Frame>,
    first_field: bool,
    same: Option<Arc<Frame>>,
    /// The references: the anchor before the latest, and the latest, in decoding order.
    last: Option<Arc<Frame>>,
    next: Option<Arc<Frame>>,
    gray: Option<Arc<Frame>>,
    /// Slices that did not decode, for the debug trace.
    slice_errors: u64,
}

impl Mpeg2Decoder {
    /// Creates a decoder; a sequence header in `extradata` (the codec private data of a container)
    /// is read now, and a header in the stream replaces it.
    pub fn new(extradata: &[u8]) -> Self {
        let mut d = Mpeg2Decoder {
            t: Tables::new(),
            width: 0,
            height: 0,
            have_seq: false,
            mpeg2: false,
            progressive_sequence: true,
            chroma_format: 1,
            low_delay: false,
            m: Matrices::default_matrices(),
            closed_gop: false,
            sync: false,
            hdr: PicHdr::mpeg1(1),
            pic: None,
            geo: None,
            cur: None,
            first_field: false,
            same: None,
            last: None,
            next: None,
            gray: None,
            slice_errors: 0,
        };
        let mut sink = Vec::new();
        let _ = d.feed(extradata, 0, &mut sink);
        d.pic = None;
        d.sync = false;
        d
    }

    /// The number of slices that could not be decoded so far.
    pub fn slice_errors(&self) -> u64 {
        self.slice_errors
    }

    fn feed(&mut self, data: &[u8], pts: i64, out: &mut Vec<OutFrame>) -> Result<()> {
        for (code, s, e) in units(data) {
            let d = &data[s..e];
            match code {
                0xB3 => {
                    self.end_picture(out);
                    self.sequence_header(d);
                }
                0xB5 => self.extension(d),
                0xB8 => {
                    self.end_picture(out);
                    self.closed_gop = d.len() >= 4 && d[3] & 0x40 != 0;
                    self.first_field = false;
                    self.sync = true;
                }
                0xB7 => self.end_picture(out),
                0x00 => {
                    self.end_picture(out);
                    self.picture_header(d, pts);
                }
                0x01..=0xAF => self.slice(code, d, out)?,
                _ => {}
            }
        }
        Ok(())
    }

    fn sequence_header(&mut self, d: &[u8]) {
        if d.len() < 8 {
            return;
        }
        let mut br = Bits::new(d);
        let (w, h) = (br.read(12) as usize, br.read(12) as usize);
        br.skip(4 + 4 + 18 + 1 + 10 + 1);
        let mut m = Matrices::default_matrices();
        if br.bit() {
            load_matrix(&mut br, &mut m.intra, Some(&mut m.chroma_intra), true);
        }
        if br.bit() {
            load_matrix(&mut br, &mut m.inter, Some(&mut m.chroma_inter), false);
        }
        self.m = m;
        self.width = w;
        self.height = h;
        self.have_seq = true;
        // an MPEG-1 sequence until an extension follows
        self.mpeg2 = false;
        self.progressive_sequence = true;
        self.chroma_format = 1;
        self.first_field = false;
    }

    fn extension(&mut self, d: &[u8]) {
        if d.is_empty() {
            return;
        }
        let mut br = Bits::new(d);
        match br.read(4) {
            1 if self.have_seq && d.len() >= 6 => {
                br.skip(1 + 3 + 4);
                self.progressive_sequence = br.bit();
                let cf = br.read(2) as u8;
                self.chroma_format = if cf == 0 { 1 } else { cf };
                self.width |= (br.read(2) as usize) << 12;
                self.height |= (br.read(2) as usize) << 12;
                br.skip(12 + 1 + 8);
                self.low_delay = br.bit();
                self.mpeg2 = true;
            }
            3 => {
                let mut m = self.m.clone();
                if br.bit() {
                    let (a, b) = (&mut m.intra, &mut m.chroma_intra);
                    load_matrix(&mut br, a, Some(b), true);
                }
                if br.bit() {
                    let (a, b) = (&mut m.inter, &mut m.chroma_inter);
                    load_matrix(&mut br, a, Some(b), false);
                }
                if br.bit() {
                    load_matrix(&mut br, &mut m.chroma_intra, None, true);
                }
                if br.bit() {
                    load_matrix(&mut br, &mut m.chroma_inter, None, false);
                }
                self.m = m;
            }
            8 if self.pic.as_ref().is_some_and(|p| p.first_slice) && d.len() >= 5 => {
                let mut h = self.pic.as_ref().map(|p| p.h.clone()).unwrap();
                h.full_pel = [false; 2];
                for s in 0..2 {
                    for t in 0..2 {
                        let f = br.read(4) as i32;
                        h.fcode[s][t] = f + (f == 0) as i32;
                    }
                }
                h.intra_dc_precision = br.read(2);
                h.structure = br.read(2) as u8;
                h.tff = br.bit();
                h.frame_pred_frame_dct = br.bit();
                h.concealment = br.bit();
                h.q_scale_type = br.bit();
                h.intra_vlc_format = br.bit();
                h.alternate_scan = br.bit();
                let _rff = br.bit();
                let _chroma_420_type = br.bit();
                let progressive_frame = br.bit();
                // the stream may not contradict itself
                if h.structure == 0 || (progressive_frame && h.structure != 3) {
                    h.structure = 3;
                }
                if let Some(p) = self.pic.as_mut() {
                    p.h = h;
                }
            }
            _ => {}
        }
    }

    fn picture_header(&mut self, d: &[u8], pts: i64) {
        self.pic = None;
        if !self.have_seq || self.width == 0 || self.height == 0 || d.len() < 4 {
            return;
        }
        let mut br = Bits::new(d);
        br.skip(10);
        let t = br.read(3) as u8;
        if t == 0 || t > 3 {
            return;
        }
        br.skip(16);
        // an MPEG-2 picture keeps the extension fields of the one before until its own extension
        let mut h = if self.mpeg2 { self.hdr.clone() } else { PicHdr::mpeg1(t) };
        h.ptype = t;
        h.progressive_sequence = self.progressive_sequence;
        h.fcode = [[1; 2]; 2];
        h.full_pel = [false; 2];
        if t == 2 || t == 3 {
            h.full_pel[0] = br.bit();
            let f = br.read(3) as i32;
            h.fcode[0] = [f + (f == 0) as i32; 2];
        }
        if t == 3 {
            h.full_pel[1] = br.bit();
            let f = br.read(3) as i32;
            h.fcode[1] = [f + (f == 0) as i32; 2];
        }
        if self.mpeg2 && h.structure == 0 {
            h.structure = 3;
        }
        self.pic = Some(PicState { h, pts, first_slice: true, skip: false });
    }

    fn geometry(&self) -> Result<Geo> {
        if self.chroma_format > 2 {
            return Err(unsupported("4:4:4 chroma is not supported"));
        }
        let mbh = if self.mpeg2 && !self.progressive_sequence { self.height.div_ceil(32) * 2 } else { self.height.div_ceil(16) };
        Ok(Geo { width: self.width, height: self.height, mbw: self.width.div_ceil(16), mbh, fmt: self.chroma_format })
    }

    fn slice(&mut self, code: u8, d: &[u8], out: &mut Vec<OutFrame>) -> Result<()> {
        let Some(st) = self.pic.as_mut() else { return Ok(()) };
        if st.skip {
            return Ok(());
        }
        let ptype = st.h.ptype;
        if st.first_slice {
            st.first_slice = false;
            let h = st.h.clone();
            let pts = st.pts;
            self.hdr = h.clone();
            if h.structure == 3 {
                self.first_field = false;
            } else {
                self.first_field = !self.first_field;
            }
            // pictures a decoder cannot start from
            if self.last.is_none() && ptype == 3 && !self.closed_gop {
                self.pic.as_mut().unwrap().skip = true;
                return Ok(());
            }
            if ptype == 1 {
                self.sync = true;
            }
            if self.next.is_none() && ptype == 2 && !self.sync {
                self.pic.as_mut().unwrap().skip = true;
                return Ok(());
            }
            let g = self.geometry()?;
            if self.geo != Some(g) {
                // a new picture size or format: what is held goes out, the references are dropped
                let saved = self.pic.take();
                let ff = self.first_field;
                self.drain(out);
                self.first_field = ff;
                self.pic = saved;
                self.geo = Some(g);
                self.gray = None;
                self.cur = None;
            }
            if self.first_field || h.structure == 3 {
                // a new frame
                if ptype != 3 {
                    self.last = self.next.take();
                }
                self.cur = Some(Frame::new(&g, pts, 0));
                self.same = None;
            } else {
                if self.cur.is_none() {
                    return Err(Error::new(ErrorCode::InvalidInput, "MPEG-2: a second field without its first"));
                }
                self.same = if ptype == 2 { self.cur.as_ref().map(|f| Arc::new(f.clone())) } else { None };
            }
        }
        let g = self.geo.expect("geometry is set by the first slice");
        let h = self.pic.as_ref().unwrap().h.clone();
        let field = h.structure != 3;
        let mut mb_y = code as usize - 1;
        if self.mpeg2 && g.mbh > 2800 / 16 && !d.is_empty() {
            mb_y += ((d[0] & 0xE0) as usize) << 2;
        }
        let rows = if field { g.mbh >> 1 } else { g.mbh };
        if mb_y >= rows || d.len() < 2 {
            return Ok(());
        }
        if self.gray.is_none() {
            self.gray = Some(Arc::new(Frame::new(&g, 0, 128)));
        }
        let gray = self.gray.clone().unwrap();
        let (fwd, bwd) = match h.ptype {
            1 => (&gray, &gray),
            2 => (self.last.as_ref().unwrap_or(&gray), &gray),
            _ => (self.last.as_ref().unwrap_or(&gray), self.next.as_ref().unwrap_or(&gray)),
        };
        let env = Env {
            t: &self.t,
            g: &g,
            m: &self.m,
            h: &h,
            mpeg2: self.mpeg2,
            first_field: self.first_field,
            fwd,
            bwd,
            same: self.same.as_deref(),
        };
        let Some(cur) = self.cur.as_mut() else { return Ok(()) };
        if let Err(e) = mb::decode_slice(&env, cur, d, mb_y) {
            self.slice_errors += 1;
            if std::env::var_os("IRONAV_MPEG2_DEBUG").is_some() {
                eprintln!("mpeg2: slice {} of a picture of type {}: {}", code, h.ptype, e);
            }
        }
        Ok(())
    }

    /// Ends the picture in progress; a frame that is now whole goes to the output logic.
    fn end_picture(&mut self, out: &mut Vec<OutFrame>) {
        let Some(st) = self.pic.take() else { return };
        if st.first_slice || st.skip || self.first_field {
            return;
        }
        let Some(cur) = self.cur.take() else { return };
        let Some(g) = self.geo else { return };
        self.same = None;
        let arc = Arc::new(cur);
        if st.h.ptype == 3 || self.low_delay {
            out.push(OutFrame::Video(arc.video(&g)));
            if st.h.ptype != 3 {
                self.next = Some(arc);
            }
        } else {
            // an anchor goes out when the next one has come: the pictures between them follow it
            if let Some(l) = &self.last {
                out.push(OutFrame::Video(l.video(&g)));
            }
            self.next = Some(arc);
        }
    }

    /// Sends out what is held, and forgets the references.
    fn drain(&mut self, out: &mut Vec<OutFrame>) {
        self.end_picture(out);
        if !self.low_delay {
            if let (Some(n), Some(g)) = (self.next.take(), self.geo) {
                out.push(OutFrame::Video(n.video(&g)));
            }
        }
        self.last = None;
        self.next = None;
        self.cur = None;
        self.first_field = false;
    }
}

/// Reads a quantiser matrix, transmitted in zigzag order, into `m` (and `also`, if given). A zero
/// stops the load; the intra DC entry is always 8.
fn load_matrix(br: &mut Bits, m: &mut [u8; 64], mut also: Option<&mut [u8; 64]>, intra: bool) {
    for i in 0..64 {
        let j = T::ZIGZAG[i] as usize;
        let mut v = br.read(8) as u8;
        if v == 0 {
            return;
        }
        if intra && i == 0 && v != 8 {
            v = 8;
        }
        m[j] = v;
        if let Some(a) = also.as_deref_mut() {
            a[j] = v;
        }
    }
}

impl Decoder for Mpeg2Decoder {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<OutFrame>> {
        let mut out = Vec::new();
        self.feed(&packet.data, packet.pts, &mut out)?;
        // a packet ends the picture in progress
        self.end_picture(&mut out);
        Ok(out)
    }

    fn flush(&mut self) -> Result<Vec<OutFrame>> {
        let mut out = Vec::new();
        self.end_picture(&mut out);
        if !self.low_delay {
            if let (Some(n), Some(g)) = (self.next.take(), self.geo) {
                out.push(OutFrame::Video(n.video(&g)));
            }
        }
        Ok(out)
    }
}
