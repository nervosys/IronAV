//! MPEG-4 Part 2 (ISO/IEC 14496-2) visual decoding: the Simple and Advanced Simple profiles of
//! progressive, rectangular video, with I, P and B pictures, quarter-sample motion, four vectors
//! per macroblock, direct mode, resync markers and both quantisers. Interlaced pictures, sprites
//! and global motion, data partitioning, complexity estimation, new prediction and scalable layers
//! are rejected with an error.
//!
//! The header syntax, the macroblock layer and the prediction rules follow FFmpeg's
//! `mpeg4videodec.c`, `mpeg4video.c`, `mpegvideo_unquantize.c` and `h263.c` (Michael Niedermayer,
//! Fabrice Bellard and the FFmpeg developers, LGPL-2.1-or-later), so that the output matches
//! FFmpeg's decoder with `-flags +bitexact -idct simple`.

pub(crate) mod bits;
pub(crate) mod idct;
mod mb;
mod mc;
mod tables;

pub mod enc;
pub use enc::Mpeg4Encoder;

use bits::{Bits, Rl, Vlc};
use ironav_core::{Decoder, Error, ErrorCode, Frame, Packet, PixelFormat, Result, VideoFrame};
use std::sync::Arc;
use tables as T;

/// Macroblock flags: not coded (a skipped macroblock), intra, and four vectors.
pub(crate) const NOT_CODED: u8 = 1;
pub(crate) const INTRA: u8 = 2;
pub(crate) const FOUR_MV: u8 = 4;

fn unsupported(msg: &str) -> Error {
    Error::new(ErrorCode::UnsupportedFormat, format!("MPEG-4: {msg}"))
}

/// The parameters of a video object layer that the pictures depend on.
#[derive(Clone)]
pub(crate) struct Vol {
    pub width: usize,
    pub height: usize,
    pub mbw: usize,
    pub mbh: usize,
    pub quarter: bool,
    pub resync: bool,
    pub quant_precision: u32,
    pub time_bits: u32,
    pub time_res: i64,
    pub mpeg_quant: bool,
    pub intra_matrix: [u8; 64],
    pub inter_matrix: [u8; 64],
}

/// A decoded picture: the planes at macroblock-aligned size, the vectors of each 8x8 block of luma
/// and the flags of each macroblock.
#[derive(Clone)]
pub(crate) struct Pic {
    pub planes: [Vec<u8>; 3],
    pub mv: Vec<[i16; 2]>,
    pub mbf: Vec<u8>,
    pub pts: i64,
}

impl Pic {
    fn new(vol: &Vol, pts: i64) -> Self {
        let (lw, lh) = (vol.mbw * 16, vol.mbh * 16);
        let (cw, ch) = (vol.mbw * 8, vol.mbh * 8);
        Pic {
            planes: [vec![0; lw * lh], vec![0; cw * ch], vec![0; cw * ch]],
            mv: vec![[0; 2]; 4 * vol.mbw * vol.mbh],
            mbf: vec![0; vol.mbw * vol.mbh],
            pts,
        }
    }

    /// The picture cropped to the layer's size, as 8-bit 4:2:0.
    fn frame(&self, vol: &Vol) -> VideoFrame {
        let (w, h) = (vol.width, vol.height);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let crop = |src: &[u8], stride: usize, w: usize, h: usize| -> Vec<u8> {
            let mut out = Vec::with_capacity(w * h);
            for row in src.chunks(stride).take(h) {
                out.extend_from_slice(&row[..w]);
            }
            out
        };
        let (lstride, cstride) = (vol.mbw * 16, vol.mbw * 8);
        VideoFrame {
            width: w as u32,
            height: h as u32,
            pix_fmt: PixelFormat::Yuv420p,
            pts: self.pts,
            planes: vec![
                crop(&self.planes[0], lstride, w, h),
                crop(&self.planes[1], cstride, cw, ch),
                crop(&self.planes[2], cstride, cw, ch),
            ],
        }
    }
}

/// The VLCs and run-level codes of the macroblock layer, built once.
pub(crate) struct Tables {
    pub dc_lum: Vlc,
    pub dc_chrom: Vlc,
    pub intra_mcbpc: Vlc,
    pub inter_mcbpc: Vlc,
    pub cbpy: Vlc,
    pub mb_type_b: Vlc,
    pub mv: Vlc,
    pub rl_intra: Rl,
    pub rl_inter: Rl,
}

/// The first symbol of the intra and inter run-level codes that carries "last" (FFmpeg's `last`).
const INTRA_LAST: usize = 67;
const INTER_LAST: usize = 58;

impl Tables {
    fn new() -> Self {
        let pairs = |codes: &[u8], bits: &[u8]| -> Vlc {
            Vlc::new(codes.iter().zip(bits).map(|(&c, &b)| (c as u32, b as u32)))
        };
        Tables {
            dc_lum: Vlc::new(T::DC_LUM.chunks(2).map(|p| (p[0] as u32, p[1] as u32))),
            dc_chrom: Vlc::new(T::DC_CHROM.chunks(2).map(|p| (p[0] as u32, p[1] as u32))),
            intra_mcbpc: pairs(&T::INTRA_MCBPC_CODE, &T::INTRA_MCBPC_BITS),
            inter_mcbpc: pairs(&T::INTER_MCBPC_CODE, &T::INTER_MCBPC_BITS),
            cbpy: pairs(&T::CBPY_CODE, &T::CBPY_BITS),
            // a B-VOP macroblock type: direct, bidirectional, backward, forward
            mb_type_b: Vlc::new([(1, 1), (1, 2), (1, 3), (1, 4)].into_iter()),
            mv: Vlc::new(T::MV_CODE.iter().zip(&T::MV_BITS).map(|(&c, &b)| (c as u32, b as u32))),
            rl_intra: Rl::new(&T::INTRA_VLC, &T::INTRA_RUN, &T::INTRA_LEVEL, INTRA_LAST),
            rl_inter: Rl::new(&T::INTER_VLC, &T::INTER_RUN, &T::INTER_LEVEL, INTER_LAST),
        }
    }
}

/// Each start-code unit of `d`: its code and the payload's byte range.
pub(crate) fn units(d: &[u8]) -> Vec<(u8, usize, usize)> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 < d.len() {
        if d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1 {
            starts.push((d[i + 3], i + 4));
            i += 4;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &(code, start)) in starts.iter().enumerate() {
        let end = starts.get(k + 1).map_or(d.len(), |&(_, s)| s - 4);
        out.push((code, start, end));
    }
    out
}

/// Parses a video object layer header (the payload after its start code).
fn parse_vol(d: &[u8]) -> Result<Vol> {
    let mut br = Bits::new(d);
    br.skip(1);
    br.skip(8);
    let verid = if br.bit() {
        let v = br.read(4);
        br.skip(3);
        v
    } else {
        1
    };
    if br.read(4) == 15 {
        br.skip(16);
    }
    if br.bit() {
        br.skip(2);
        br.skip(1);
        if br.bit() {
            br.skip(15 + 1 + 15 + 1 + 15 + 1 + 3 + 11 + 1 + 15 + 1);
        }
    }
    if br.read(2) != 0 {
        return Err(unsupported("only rectangular pictures are supported").hint("binary and grayscale shapes are not decoded"));
    }
    br.skip(1);
    let res = br.read(16) as i64;
    if res == 0 {
        return Err(Error::new(ErrorCode::InvalidInput, "MPEG-4: zero time increment resolution"));
    }
    br.skip(1);
    let time_bits = (64 - ((res - 1) as u64).leading_zeros()).max(1);
    if br.bit() {
        br.skip(time_bits as usize);
    }
    br.skip(1);
    let width = br.read(13) as usize;
    br.skip(1);
    let height = br.read(13) as usize;
    br.skip(1);
    if br.bit() {
        return Err(unsupported("interlaced pictures are not supported"));
    }
    br.skip(1);
    let sprite = if verid == 1 { br.read(1) } else { br.read(2) };
    if sprite != 0 {
        return Err(unsupported("sprites and global motion compensation are not supported"));
    }
    let quant_precision = if br.bit() {
        let p = br.read(4);
        br.skip(4);
        if !(3..=9).contains(&p) {
            return Err(Error::new(ErrorCode::InvalidInput, "MPEG-4: bad quantiser precision"));
        }
        p
    } else {
        5
    };
    let mpeg_quant = br.bit();
    let mut intra_matrix = T::DEFAULT_INTRA_MATRIX;
    let mut inter_matrix = T::DEFAULT_INTER_MATRIX;
    if mpeg_quant {
        if br.bit() {
            load_matrix(&mut br, &mut intra_matrix);
        }
        if br.bit() {
            load_matrix(&mut br, &mut inter_matrix);
        }
    }
    let quarter = if verid != 1 { br.bit() } else { false };
    if !br.bit() {
        return Err(unsupported("complexity estimation headers are not supported"));
    }
    let resync = !br.bit();
    if br.bit() {
        return Err(unsupported("data partitioning is not supported"));
    }
    if verid != 1 && br.bit() {
        return Err(unsupported("new prediction is not supported"));
    }
    if verid != 1 {
        br.skip(1);
    }
    if br.bit() {
        return Err(unsupported("scalable layers are not supported"));
    }
    if width == 0 || height == 0 {
        return Err(Error::new(ErrorCode::InvalidInput, "MPEG-4: zero picture size"));
    }
    Ok(Vol {
        width,
        height,
        mbw: width.div_ceil(16),
        mbh: height.div_ceil(16),
        quarter,
        resync,
        quant_precision,
        time_bits,
        time_res: res,
        mpeg_quant,
        intra_matrix,
        inter_matrix,
    })
}

/// A custom quantiser matrix in zigzag order, its last value repeated to the end.
fn load_matrix(br: &mut Bits, m: &mut [u8; 64]) {
    let mut last = 0u8;
    let mut i = 0;
    while i < 64 {
        let v = br.read(8) as u8;
        if v == 0 {
            break;
        }
        last = v;
        m[T::ZIGZAG[i] as usize] = v;
        i += 1;
    }
    for j in i..64 {
        m[T::ZIGZAG[j] as usize] = last;
    }
}

/// The MPEG-4 decoder: one packet is one picture (or a stream of start-code units).
pub struct Mpeg4Decoder {
    t: Tables,
    vol: Option<Vol>,
    /// Why the layer header in the container's extradata did not parse, if it did not.
    vol_err: Option<Error>,
    time_base: i64,
    last_time_base: i64,
    last_non_b: i64,
    pp_time: i64,
    pb_time: i64,
    /// The most recent anchor and the one before it: the references of the next B-VOP.
    last: Option<Arc<Pic>>,
    prev: Option<Arc<Pic>>,
    /// The anchor waiting to be emitted: anchors come out when the next anchor arrives, so
    /// B-VOPs (which lie between them in display order) come out in between.
    held: Option<Arc<Pic>>,
}

impl Mpeg4Decoder {
    /// Creates a decoder whose layer header is in `extradata` (the codec private data of the
    /// container, if any); a layer header in the stream replaces it.
    pub fn new(extradata: &[u8]) -> Self {
        let mut vol_err = None;
        let vol = units(extradata).into_iter().find_map(|(code, s, e)| match code {
            0x20..=0x2F => parse_vol(&extradata[s..e]).map_err(|x| vol_err = Some(x)).ok(),
            _ => None,
        });
        Mpeg4Decoder {
            t: Tables::new(),
            vol,
            vol_err,
            time_base: 0,
            last_time_base: 0,
            last_non_b: 0,
            pp_time: 1,
            pb_time: 0,
            last: None,
            prev: None,
            held: None,
        }
    }

    fn frame(&self, pic: &Pic) -> Result<Frame> {
        let vol = self.vol.as_ref().ok_or_else(|| Error::new(ErrorCode::InvalidInput, "MPEG-4: no layer header"))?;
        Ok(Frame::Video(pic.frame(vol)))
    }

    /// Decodes one picture: its header, then its macroblocks into a new picture.
    fn vop(&mut self, d: &[u8], pts: i64, out: &mut Vec<Frame>) -> Result<()> {
        let vol = match (&self.vol, &self.vol_err) {
            (Some(v), _) => v.clone(),
            (None, Some(e)) => return Err(e.clone()),
            (None, None) => return Err(Error::new(ErrorCode::InvalidInput, "MPEG-4: a picture before its layer header")),
        };
        let mut br = Bits::new(d);
        let pict = br.read(2) as u8;
        if pict == 3 {
            return Err(unsupported("sprite pictures are not supported"));
        }
        let mut modulo = 0i64;
        while br.bit() {
            modulo += 1;
        }
        br.skip(1);
        let inc = br.read(vol.time_bits) as i64;
        br.skip(1);
        let res = self.res();
        if pict != 2 {
            self.last_time_base = self.time_base;
            self.time_base += modulo;
            let time = self.time_base * res + inc;
            self.pp_time = time - self.last_non_b;
            self.last_non_b = time;
        } else {
            let time = (self.last_time_base + modulo) * res + inc;
            let pb = self.pp_time - (self.last_non_b - time);
            // the timing FFmpeg rejects (after a seek, or a mis-ordered stream)
            if self.pp_time <= pb || self.pp_time <= self.pp_time - pb || self.pp_time <= 0 {
                return Ok(());
            }
            self.pb_time = pb;
        }
        if !br.bit() {
            return Ok(());
        }
        let mut nr = false;
        if pict == 1 {
            nr = br.bit();
        }
        let dc_thr = T::DC_THRESHOLD[br.read(3) as usize] as i32;
        let qscale = br.read(vol.quant_precision) as i32;
        let mut fcode = [1, 1];
        if pict != 0 {
            fcode[0] = br.read(3) as i32;
        }
        if pict == 2 {
            fcode[1] = br.read(3) as i32;
        }
        let (fwd_ref, bwd_ref) = match pict {
            0 => (None, None),
            1 => (self.last.clone(), None),
            _ => match (self.prev.clone(), self.last.clone()) {
                (Some(p), Some(l)) => (Some(p), Some(l)),
                _ => return Err(Error::new(ErrorCode::InvalidInput, "MPEG-4: a B-picture without its references")),
            },
        };
        if pict == 1 && fwd_ref.is_none() {
            return Err(Error::new(ErrorCode::InvalidInput, "MPEG-4: a P-picture without a reference"));
        }
        let mut v = mb::Vop::new(&self.t, &vol, br, Pic::new(&vol, pts));
        v.pict = pict;
        v.qscale = qscale.clamp(1, 31);
        v.fcode = fcode;
        v.nr = nr;
        v.dc_thr = dc_thr;
        v.fwd_ref = fwd_ref;
        v.bwd_ref = bwd_ref;
        v.pp_time = self.pp_time;
        v.pb_time = self.pb_time;
        v.run()?;
        let pic = v.cur;
        if pict == 2 {
            out.push(self.frame(&pic)?);
            return Ok(());
        }
        let pic = Arc::new(pic);
        if let Some(h) = self.held.take() {
            out.push(self.frame(&h)?);
        }
        self.prev = self.last.take();
        self.last = Some(pic.clone());
        self.held = Some(pic);
        Ok(())
    }

    /// The time resolution of the layer: the number of ticks in a second.
    fn res(&self) -> i64 {
        self.vol.as_ref().map_or(1, |v| v.time_res)
    }
}

impl Decoder for Mpeg4Decoder {
    fn decode(&mut self, packet: &Packet) -> Result<Vec<Frame>> {
        let mut out = Vec::new();
        for (code, s, e) in units(&packet.data) {
            match code {
                0x20..=0x2F => self.vol = Some(parse_vol(&packet.data[s..e])?),
                0xB6 => self.vop(&packet.data[s..e], packet.pts, &mut out)?,
                _ => {}
            }
        }
        Ok(out)
    }

    fn flush(&mut self) -> Result<Vec<Frame>> {
        match self.held.take() {
            Some(h) => Ok(vec![self.frame(&h)?]),
            None => Ok(Vec::new()),
        }
    }
}
