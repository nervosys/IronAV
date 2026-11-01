//! A software MPEG-2 video encoder: the Main profile at 4:2:0, progressive frame pictures, with I and
//! P pictures and, with `with_bframes(n)`, B pictures too (as the MPEG-4 encoder's are, off by
//! default). Each slice is one macroblock row, the quantiser is linear (`qscale = 2 * code`), the
//! intra DC has 8 bits, and the default matrices are used. Motion is half-sample, one vector per
//! macroblock. Each picture is reconstructed with the decoder's motion compensation, dequantisation
//! (with its mismatch control) and inverse transform, so the decoder's output is the encoder's
//! reconstruction, and the references the encoder predicts from are exactly those.
//!
//! The headers and the macroblock syntax are the ones the decoder in `mpeg2/mb.rs` reads. The block
//! coder and the quantiser scale follow FFmpeg's `mpeg12enc.c` and `mpegvideo_enc.c` (Fabrice Bellard,
//! Michael Niedermayer and the FFmpeg developers, LGPL-2.1-or-later); the motion search, the rate
//! control and the decision structure follow the MPEG-4 encoder (`mpeg4/enc`).
//!
//! Not written: field or dual-prime pictures, 4:2:2, quantiser changes within a slice, custom
//! matrices, concealment vectors, and slice position extensions (pictures of more than 175
//! macroblock rows). B macroblocks never skip, since MPEG-2 has no direct mode and a skipped B
//! macroblock would repeat the one before it.

mod mb;

use super::{Frame as Pic, Geo, FRAME_RATES};
use ironav_core::{Encoder, Error, ErrorCode, Frame, Packet, PixelFormat, Result, StreamParams, VideoFrame};
use mb::Coder;
use std::sync::Arc;

/// Frames between key pictures.
const KEYINT: u64 = 250;
/// The largest key picture interval: the temporal reference counts within a group of pictures in ten bits.
const MAX_KEYINT: u64 = 1023;
/// The quantiser code when neither a quantiser nor a bit rate is set.
const DEFAULT_QSCALE: i32 = 5;
/// The first quantiser code of a rate-controlled stream, before the first picture's size is known.
const START_QSCALE: f64 = 6.0;
/// The motion search's range in half samples: vectors lie within plus or minus this.
pub(super) const SEARCH: i32 = 30;
/// The most B pictures between anchor pictures.
const MAX_BFRAMES: usize = 3;
/// The picture coding types, as the `picture_coding_type` codes them.
pub(super) const KIND_I: u8 = 1;
pub(super) const KIND_P: u8 = 2;
pub(super) const KIND_B: u8 = 3;

/// The macroblock-aligned planes of a source frame.
pub(super) struct Source {
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

/// An MSB-first bit writer. Byte-aligned byte strings (start codes) go in with `bytes`.
#[derive(Default)]
pub(super) struct BitW {
    out: Vec<u8>,
    acc: u32,
    n: u32,
}

impl BitW {
    pub fn bit(&mut self, b: bool) {
        self.acc = (self.acc << 1) | b as u32;
        self.n += 1;
        if self.n == 8 {
            self.out.push(self.acc as u8);
            self.acc = 0;
            self.n = 0;
        }
    }

    /// The low `bits` bits of `v`, most significant first.
    pub fn put(&mut self, v: u32, bits: u32) {
        for i in (0..bits).rev() {
            self.bit((v >> i) & 1 != 0);
        }
    }

    /// Pads with zero bits to a byte boundary.
    pub fn align(&mut self) {
        while self.n != 0 {
            self.bit(false);
        }
    }

    /// Whole bytes, at a byte boundary.
    pub fn bytes(&mut self, b: &[u8]) {
        self.align();
        self.out.extend_from_slice(b);
    }

    /// Pads with zero bits to a byte boundary and returns the bytes.
    pub fn finish(mut self) -> Vec<u8> {
        self.align();
        self.out
    }
}

/// The start codes and headers that describe a stream of `width` by `height` at the frame rate
/// `num / den`: the sequence header and its extension (MPEG-2, Main profile at main level, 4:2:0,
/// progressive). The bytes are the stream's codec configuration (Matroska's CodecPrivate, the MP4
/// decoder specific info), and a key picture carries them in front of it.
pub fn config(width: u32, height: u32, num: i64, den: i64) -> Vec<u8> {
    let (code, en, ed) = frame_rate_code(num, den);
    let mut out = vec![0, 0, 1, 0xB3];
    let mut b = BitW::default();
    b.put(width & 0xFFF, 12);
    b.put(height & 0xFFF, 12);
    b.put(1, 4); // square pixels
    b.put(code, 4);
    b.put(0x3FFFF, 18); // variable bit rate
    b.bit(true); // marker
    b.put(112, 10); // VBV buffer size
    b.bit(false); // not constrained parameters
    b.bit(false); // default intra matrix
    b.bit(false); // default non-intra matrix
    out.extend(b.finish());
    out.extend_from_slice(&[0, 0, 1, 0xB5]);
    let mut e = BitW::default();
    e.put(1, 4); // sequence extension
    e.put(0x48, 8); // Main profile at main level
    e.bit(true); // progressive
    e.put(1, 2); // 4:2:0
    e.put((width >> 12) & 3, 2);
    e.put((height >> 12) & 3, 2);
    e.put(0, 12); // bit rate extension
    e.bit(true); // marker
    e.put(0, 8); // VBV buffer size extension
    e.bit(false); // not low delay
    e.put(en, 2);
    e.put(ed, 5);
    out.extend(e.finish());
    out
}

/// The frame rate code and the extension that give `num / den` frames a second: a standard rate
/// times a small ratio, the closest there is.
fn frame_rate_code(num: i64, den: i64) -> (u32, u32, u32) {
    let target = num as f64 / den as f64;
    let mut best = (f64::MAX, 3u32, 0u32, 0u32);
    for (code, &(bn, bd)) in FRAME_RATES.iter().enumerate().skip(1) {
        let base = bn as f64 / bd as f64;
        for n in 1..=4u32 {
            for d in 1..=32u32 {
                let err = (base * n as f64 / d as f64 - target).abs() / target;
                if err < best.0 - 1e-12 {
                    best = (err, code as u32, n - 1, d - 1);
                }
            }
        }
    }
    (best.1, best.2, best.3)
}

/// A group of pictures header: a zero time code, and a closed group, since no B picture refers to a
/// picture before its key picture.
fn gop_header() -> Vec<u8> {
    let mut out = vec![0, 0, 1, 0xB8];
    let mut b = BitW::default();
    b.put(0, 25); // time code
    b.bit(true); // closed group
    b.bit(false); // no broken link
    out.extend(b.finish());
    out
}

/// The quantiser of a B picture from the anchors': FFmpeg's default B quantiser factor and offset (1.25
/// each), applied in both rate modes. B pictures are never referred to.
fn b_quant(q: i32) -> i32 {
    ((q as f64 * 1.25 + 1.25 + 0.5) as i32).clamp(1, 31)
}

pub struct Mpeg2Encoder {
    geo: Geo,
    config: Vec<u8>,
    fcode: i32,
    /// Frames received: the display index of the next one.
    frames: u64,
    /// B pictures between anchor pictures (0 for none).
    bframes: usize,
    /// Frames waiting for the anchor picture they are coded with: source, time stamp and display index.
    pending: Vec<(Source, i64, u64)>,
    /// The display index of the last key picture, and the pictures between key pictures.
    key_at: u64,
    keyint: u64,
    /// The quantiser code when it is fixed, else the rate control's (fractional) code.
    fixed: Option<i32>,
    qscale: f64,
    /// Bits per picture to average when rate controlled, and the bits over (or under) that lately.
    target: Option<f64>,
    debt: f64,
    /// The last anchor picture as the decoder reconstructs it: the next anchor predicts from it.
    refp: Option<Arc<Pic>>,
    /// Every picture's reconstruction with its time stamp, in coding order.
    #[cfg(test)]
    recon: Vec<(i64, Arc<Pic>)>,
}

impl Mpeg2Encoder {
    /// An encoder for `params` (yuv420p video) at a constant quantiser (`qp`, the quantiser code 1 to 31:
    /// the quantiser scale is twice it, as the linear scale codes it) or an average bit rate in bits per
    /// second.
    pub fn new(params: &StreamParams, qp: Option<u8>, bitrate: Option<u32>) -> Result<Self> {
        let StreamParams::Video { width, height, pix_fmt, frame_rate } = *params else {
            return Err(Error::invalid_arg("the MPEG-2 encoder takes video"));
        };
        if pix_fmt != PixelFormat::Yuv420p {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("the software MPEG-2 encoder takes yuv420p frames, not {}", pix_fmt.name()))
                .hint("add a format filter to convert to yuv420p"));
        }
        if width == 0 || height == 0 || width > 16383 || height > 2800 {
            return Err(Error::invalid_arg(format!("{width}x{height} is not a picture size this MPEG-2 encoder writes (1 to 16383 wide, 1 to 2800 high)")));
        }
        let (num, den) = if frame_rate.num > 0 && frame_rate.den > 0 { (frame_rate.num, frame_rate.den) } else { (25, 1) };
        let (w, h) = (width as usize, height as usize);
        let geo = Geo { width: w, height: h, mbw: w.div_ceil(16), mbh: h.div_ceil(16), fmt: 1 };
        // the vectors of a macroblock are at most SEARCH half samples, so a difference is at most twice that
        let mut fcode = 1;
        while (16i32 << (fcode - 1)) <= 2 * SEARCH {
            fcode += 1;
        }
        let fixed = match (qp, bitrate) {
            (Some(q), _) => Some((q as i32).clamp(1, 31)),
            (None, Some(_)) => None,
            (None, None) => Some(DEFAULT_QSCALE),
        };
        let fps = num as f64 / den as f64;
        Ok(Self {
            config: config(width, height, num, den),
            geo,
            fcode,
            frames: 0,
            bframes: 0,
            pending: Vec::new(),
            key_at: 0,
            keyint: KEYINT,
            fixed,
            qscale: fixed.map_or(START_QSCALE, f64::from),
            target: bitrate.map(|b| b as f64 / fps),
            debt: 0.0,
            refp: None,
            #[cfg(test)]
            recon: Vec::new(),
        })
    }

    /// Key pictures every `n` pictures (1 for key pictures only; at most 1023).
    pub fn with_keyint(mut self, n: u64) -> Self {
        self.keyint = n.clamp(1, MAX_KEYINT);
        self
    }

    /// `n` B pictures between anchor pictures (0 for none, up to 3). Each B picture refers to the
    /// anchors on either side of it, and the anchors are coded ahead of the pictures between them, so
    /// the packets come out in decoding order, each with its display time.
    pub fn with_bframes(mut self, n: usize) -> Self {
        self.bframes = n.min(MAX_BFRAMES);
        self
    }

    /// The codec configuration to hand a container (the sequence header and its extension).
    pub fn config(&self) -> &[u8] {
        &self.config
    }

    fn source(&self, v: &VideoFrame) -> Result<Source> {
        let (w, h) = (self.geo.width, self.geo.height);
        if v.pix_fmt != PixelFormat::Yuv420p || v.width as usize != w || v.height as usize != h || v.planes.len() < 3 {
            return Err(Error::invalid_input(format!("the MPEG-2 encoder was set up for {w}x{h} yuv420p frames")));
        }
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        if v.planes[0].len() < w * h || v.planes[1].len() < cw * ch || v.planes[2].len() < cw * ch {
            return Err(Error::invalid_input("a video frame plane is shorter than its dimensions"));
        }
        // edge pixels repeat into the padding
        let pad = |p: &[u8], pw: usize, ph: usize, bw: usize, bh: usize| {
            let mut out = vec![0u8; bw * bh];
            for y in 0..bh {
                let row = &p[y.min(ph - 1) * pw..][..pw];
                for x in 0..bw {
                    out[y * bw + x] = row[x.min(pw - 1)];
                }
            }
            out
        };
        let (mbw, mbh) = (self.geo.mbw, self.geo.mbh);
        Ok(Source {
            y: pad(&v.planes[0], w, h, mbw * 16, mbh * 16),
            u: pad(&v.planes[1], cw, ch, mbw * 8, mbh * 8),
            v: pad(&v.planes[2], cw, ch, mbw * 8, mbh * 8),
        })
    }

    /// The quantiser code of the next picture.
    fn quant(&self) -> i32 {
        self.fixed.unwrap_or(self.qscale.round() as i32).clamp(1, 31)
    }

    /// The temporal reference of display index `d`: its place in the group of pictures that starts at
    /// the last key picture.
    fn tref(&self, d: u64) -> u32 {
        ((d - self.key_at) % 1024) as u32
    }

    /// Codes one picture: its header and extension, then its slices, each a macroblock row. `refs` are
    /// its forward and backward references. Returns the picture's bytes (start codes included) and its
    /// reconstruction.
    fn code_picture(&self, src: &Source, kind: u8, tr: u32, code: i32, refs: [Option<Arc<Pic>>; 2], pts: i64) -> (Vec<u8>, Pic) {
        let fc = self.fcode as u32;
        let mut bw = BitW::default();
        bw.bytes(&[0, 0, 1, 0x00]);
        bw.put(tr, 10);
        bw.put(kind as u32, 3);
        bw.put(0xFFFF, 16); // vbv_delay: not specified
        if kind != KIND_I {
            bw.bit(false); // full pel forward vector: no
            bw.put(fc, 3);
        }
        if kind == KIND_B {
            bw.bit(false);
            bw.put(fc, 3);
        }
        bw.bit(false); // no extra picture information
        bw.bytes(&[0, 0, 1, 0xB5]);
        bw.put(8, 4); // picture coding extension
        let (f_fwd, f_bwd) = match kind {
            KIND_I => (1, 1),
            KIND_P => (fc, 1),
            _ => (fc, fc),
        };
        bw.put(f_fwd, 4);
        bw.put(f_fwd, 4);
        bw.put(f_bwd, 4);
        bw.put(f_bwd, 4);
        bw.put(0, 2); // intra DC precision: 8 bits
        bw.put(3, 2); // a frame picture
        bw.bit(false); // top field first: no matter for frames
        bw.bit(true); // frame prediction and frame DCT
        bw.bit(false); // no concealment vectors
        bw.bit(false); // linear quantiser scale
        bw.bit(false); // the first VLC table of intra blocks
        bw.bit(false); // the zig-zag scan
        bw.bit(false); // no repeated first field
        bw.bit(true); // 4:2:0 chroma
        bw.bit(true); // progressive
        bw.bit(false); // no composite display information
        let mut c = Coder::new(self.geo, src, refs, code, self.fcode, bw, pts);
        c.slices(kind);
        c.finish()
    }

    /// Rate control: the quantiser follows how far each picture's size was from its share of the bit
    /// rate, and how far the recent pictures have drifted from it in total. Key pictures only add to
    /// the drift: they are much larger, and the quantiser has to hold for the pictures after them.
    fn adapt(&mut self, bits: f64, key: bool) {
        let Some(target) = self.target else { return };
        self.debt = self.debt * 0.8 + bits - target;
        if !key {
            let step = 1.5 * (bits / target).log2().clamp(-2.0, 2.0) + (self.debt / (target * 4.0)).clamp(-1.0, 1.0);
            self.qscale = (self.qscale + step).clamp(1.0, 31.0);
        }
    }

    /// Codes an anchor picture at display index `d`: a key picture, or a P picture from the last anchor.
    /// It becomes the last anchor. A key picture carries the configuration and a group of pictures
    /// header in front of it.
    fn code_anchor(&mut self, src: &Source, pts: i64, d: u64, key: bool) -> Packet {
        let kind = if key { KIND_I } else { KIND_P };
        let refs = [if key { None } else { self.refp.clone() }, None];
        let tr = self.tref(d);
        let (body, pic) = self.code_picture(src, kind, tr, self.quant(), refs, pts);
        self.adapt(body.len() as f64 * 8.0, key);
        let mut data = Vec::with_capacity(body.len() + 64);
        if key {
            data.extend_from_slice(&self.config);
            data.extend(gop_header());
        }
        data.extend_from_slice(&body);
        let pic = Arc::new(pic);
        #[cfg(test)]
        self.recon.push((pts, pic.clone()));
        self.refp = Some(pic);
        Packet { stream: 0, pts, duration: 1, data }
    }

    /// Codes a B picture at display index `d` between the last two anchors: `fwd` is the past one and
    /// `bwd` the future one.
    fn code_b(&mut self, src: &Source, pts: i64, d: u64, fwd: Arc<Pic>, bwd: Arc<Pic>) -> Packet {
        let tr = self.tref(d);
        #[cfg_attr(not(test), allow(unused_variables))]
        let (body, pic) = self.code_picture(src, KIND_B, tr, b_quant(self.quant()), [Some(fwd), Some(bwd)], pts);
        self.adapt(body.len() as f64 * 8.0, false);
        #[cfg(test)]
        self.recon.push((pts, Arc::new(pic)));
        Packet { stream: 0, pts, duration: 1, data: body }
    }

    /// Codes the frames waiting for an anchor as P pictures, each from the one before. This is for the
    /// frames before a key picture, and for the end of the stream.
    fn code_pending(&mut self) -> Vec<Packet> {
        let waiting = std::mem::take(&mut self.pending);
        waiting.into_iter().map(|(src, pts, d)| self.code_anchor(&src, pts, d, false)).collect()
    }

    /// Codes the frames waiting with the newest one as an anchor: a P picture from the last anchor,
    /// then the others as B pictures between the two anchors, in display order.
    fn code_gop(&mut self) -> Vec<Packet> {
        let mut waiting = std::mem::take(&mut self.pending);
        let Some((src, pts, d)) = waiting.pop() else {
            return Vec::new();
        };
        let fwd = self.refp.clone().expect("a key picture precedes the anchors");
        let mut out = vec![self.code_anchor(&src, pts, d, false)];
        let bwd = self.refp.clone().expect("the anchor was just coded");
        for (src, pts, d) in waiting {
            out.push(self.code_b(&src, pts, d, fwd.clone(), bwd.clone()));
        }
        out
    }
}

impl Encoder for Mpeg2Encoder {
    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>> {
        let Frame::Video(v) = frame else {
            return Err(Error::invalid_input("the MPEG-2 encoder received an audio frame"));
        };
        let src = self.source(v)?;
        let d = self.frames;
        self.frames += 1;
        if d == 0 || d - self.key_at >= self.keyint {
            // a key picture: the frames waiting before it are coded first, as P pictures of the group before
            let mut out = self.code_pending();
            self.key_at = d;
            out.push(self.code_anchor(&src, v.pts, d, true));
            return Ok(out);
        }
        self.pending.push((src, v.pts, d));
        if self.pending.len() > self.bframes {
            return Ok(self.code_gop());
        }
        Ok(Vec::new())
    }

    fn flush(&mut self) -> Result<Vec<Packet>> {
        Ok(self.code_pending())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpeg2::Mpeg2Decoder;
    use ironav_core::{Decoder, Rational};

    /// Moving content: a gradient, a bright bar sliding right and a dark block sliding left, with noise.
    fn scene(w: usize, h: usize, t: usize) -> VideoFrame {
        let mut seed = 7u32 + t as u32 * 1000;
        let mut noise = move || {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            ((seed >> 16) % 7) as i32 - 3
        };
        let mut y = vec![0u8; w * h];
        for r in 0..h {
            for c in 0..w {
                let mut v = (c * 160 / w + r * 60 / h) as i32 + 20;
                if (c + 3 * t) % w < w / 5 && r > h / 5 && r < h / 2 {
                    v = 220 - ((c + r) % 7) as i32 * 10;
                }
                if (c + w - (2 * t) % w) % w > w / 2 && (c + w - (2 * t) % w) % w < w / 2 + w / 6 && r > h / 2 {
                    v = 30 + ((r * 5) % 40) as i32;
                }
                y[r * w + c] = (v + noise()).clamp(0, 255) as u8;
            }
        }
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let u = (0..cw * ch).map(|i| (100 + (i % cw) * 50 / cw + t) as u8).collect();
        let v = (0..cw * ch).map(|i| (150 - (i / cw) * 40 / ch) as u8).collect();
        VideoFrame { width: w as u32, height: h as u32, pix_fmt: PixelFormat::Yuv420p, pts: t as i64, planes: vec![y, u, v] }
    }

    fn psnr(a: &[u8], b: &[u8]) -> f64 {
        let se: f64 = a.iter().zip(b).map(|(x, y)| (*x as f64 - *y as f64).powi(2)).sum::<f64>() / a.len() as f64;
        10.0 * (255.0f64 * 255.0 / se.max(1e-9)).log10()
    }

    /// Crops a macroblock-aligned plane to `w` by `h`.
    fn crop(p: &[u8], stride: usize, w: usize, h: usize) -> Vec<u8> {
        (0..h).flat_map(|r| p[r * stride..r * stride + w].to_vec()).collect()
    }

    fn params(w: u32, h: u32, fps: i64) -> StreamParams {
        StreamParams::Video { width: w, height: h, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(fps, 1) }
    }

    /// Encodes `n` pictures with `bframes` B pictures between anchors and decodes them with the IronAV
    /// decoder. Checks that the decoder's pictures, in display order, are the encoder's reconstructions
    /// bit for bit. Returns the packet sizes in coding order, the mean luma PSNR against the source, and
    /// the time stamps in coding order.
    fn round_trip_b(w: usize, h: usize, n: usize, qp: Option<u8>, bitrate: Option<u32>, keyint: u64, bframes: usize) -> (Vec<usize>, f64, Vec<i64>) {
        let mut enc = Mpeg2Encoder::new(&params(w as u32, h as u32, 25), qp, bitrate).unwrap().with_keyint(keyint).with_bframes(bframes);
        let mut dec = Mpeg2Decoder::new(&[]);
        let mut srcs = Vec::new();
        let mut pkts = Vec::new();
        for t in 0..n {
            let src = scene(w, h, t);
            pkts.extend(enc.encode(&Frame::Video(src.clone())).unwrap());
            srcs.push(src);
        }
        pkts.extend(enc.flush().unwrap());
        assert_eq!(pkts.len(), n, "every frame is coded once");
        let sizes = pkts.iter().map(|p| p.data.len()).collect();
        let order = pkts.iter().map(|p| p.pts).collect();
        let mut out = Vec::new();
        for p in pkts {
            out.extend(dec.decode(&p).unwrap());
        }
        out.extend(dec.flush().unwrap());
        assert_eq!(out.len(), n, "the decoder returns every picture");
        assert_eq!(dec.slice_errors(), 0, "every slice decodes");
        enc.recon.sort_by_key(|r| r.0);
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let mut p = 0.0;
        for (t, f) in out.iter().enumerate() {
            let Frame::Video(v) = f else { unreachable!() };
            assert_eq!(v.pts, t as i64, "picture {t} is shown in display order");
            let rec = &enc.recon[t].1;
            let (lw, cs) = (enc.geo.mbw * 16, enc.geo.mbw * 8);
            let want = [crop(&rec.p[0].d, lw, w, h), crop(&rec.p[1].d, cs, cw, ch), crop(&rec.p[2].d, cs, cw, ch)];
            assert!(v.planes == want, "picture {t}: the decoder's picture differs from the encoder's reconstruction");
            p += psnr(&v.planes[0], &srcs[t].planes[0]);
        }
        (sizes, p / n as f64, order)
    }

    /// Encodes `n` pictures and decodes them with the IronAV decoder (no B pictures). Returns the packet
    /// sizes and the mean luma PSNR against the source.
    fn round_trip(w: usize, h: usize, n: usize, qp: Option<u8>, bitrate: Option<u32>, keyint: u64) -> (Vec<usize>, f64) {
        let (sizes, p, _) = round_trip_b(w, h, n, qp, bitrate, keyint, 0);
        (sizes, p)
    }

    #[test]
    fn configuration_describes_the_stream() {
        let info = crate::mpeg2::sequence_info(&config(352, 288, 30000, 1001)).unwrap();
        assert_eq!((info.width, info.height, info.frame_rate, info.mpeg2, info.progressive), (352, 288, (30000, 1001), true, true));
        let info = crate::mpeg2::sequence_info(&config(720, 576, 25, 1)).unwrap();
        assert_eq!((info.width, info.height, info.frame_rate), (720, 576, (25, 1)));
    }

    #[test]
    fn pictures_decode_to_the_encoder_reconstruction() {
        let (sizes, p) = round_trip(64, 48, 6, Some(4), None, 250);
        assert!(p > 30.0, "PSNR {p:.1}");
        assert!(sizes[1] < sizes[0], "inter pictures are smaller: {sizes:?}");
    }

    #[test]
    fn odd_sizes_round_trip() {
        round_trip(177, 99, 4, Some(8), None, 250);
        round_trip(178, 100, 4, Some(8), None, 250);
    }

    #[test]
    fn quantiser_trades_size_for_quality() {
        let mut last = (usize::MAX, f64::MAX);
        for q in [2u8, 6, 14, 28] {
            let (sizes, p) = round_trip(96, 80, 3, Some(q), None, 250);
            let total: usize = sizes.iter().sum();
            assert!(total < last.0 && p < last.1, "qscale {q}: {total} bytes, {p:.1} dB");
            last = (total, p);
        }
    }

    #[test]
    fn key_pictures_only_at_keyint_one() {
        let (sizes, _) = round_trip(64, 48, 3, Some(8), None, 1);
        assert!(sizes[1] >= sizes[0] / 2, "every picture is a key picture: {sizes:?}");
    }

    #[test]
    fn static_pictures_skip_macroblocks() {
        // the same picture four times: the first P picture codes the key picture's quantisation error, and
        // the later ones, predicted from a stable reconstruction, are nearly all skipped macroblocks
        let mut enc = Mpeg2Encoder::new(&params(96, 80, 25), Some(8), None).unwrap();
        let still = scene(96, 80, 0);
        let mut pkts = Vec::new();
        for _ in 0..4 {
            pkts.extend(enc.encode(&Frame::Video(still.clone())).unwrap());
        }
        let sizes: Vec<usize> = pkts.iter().map(|p| p.data.len()).collect();
        assert!(sizes[3] * 4 < sizes[0], "a still picture is much smaller: {sizes:?}");
    }

    #[test]
    fn rate_control_tracks_the_bit_rate() {
        let (w, h, n) = (176, 144, 30);
        let mut enc = Mpeg2Encoder::new(&params(w as u32, h as u32, 30), None, Some(300_000)).unwrap();
        let mut bits = 0usize;
        for t in 0..n {
            bits += enc.encode(&Frame::Video(scene(w, h, t))).unwrap()[0].data.len() * 8;
        }
        let kbps = bits as f64 * 30.0 / n as f64 / 1000.0;
        assert!((150.0..450.0).contains(&kbps), "{kbps:.0} kbit/s for a 300 kbit/s target");
    }

    /// B pictures: the anchors are coded ahead of the pictures between them, so the packets come out
    /// in decoding order, each with its display time, and the decoder shows them in display order, bit
    /// for bit as the encoder reconstructed them.
    #[test]
    fn b_pictures_decode_to_the_encoder_reconstruction() {
        let (sizes, p, order) = round_trip_b(64, 48, 9, Some(4), None, 250, 2);
        assert_eq!(order, vec![0, 3, 1, 2, 6, 4, 5, 7, 8], "anchors first, then the pictures between them");
        assert!(p > 28.0, "PSNR {p:.1}");
        assert_eq!(sizes.len(), 9);
    }

    #[test]
    fn one_b_picture_between_anchors_at_odd_sizes() {
        round_trip_b(177, 99, 7, Some(8), None, 250, 1);
        round_trip_b(178, 100, 7, Some(8), None, 250, 1);
    }

    /// A key picture with B pictures waiting before it: those are coded as P pictures ahead of it.
    #[test]
    fn key_pictures_with_frames_waiting_for_an_anchor() {
        let (_, _, order) = round_trip_b(64, 48, 12, Some(6), None, 5, 2);
        assert_eq!(order.len(), 12);
        let mut sorted = order.clone();
        sorted.sort();
        assert_eq!(sorted, (0..12).collect::<Vec<i64>>(), "every frame is coded once");
    }

    #[test]
    fn b_pictures_under_rate_control() {
        let (w, h, n) = (176, 144, 20);
        let mut enc = Mpeg2Encoder::new(&params(w as u32, h as u32, 25), None, Some(400_000)).unwrap().with_bframes(2);
        let mut pkts = Vec::new();
        for t in 0..n {
            pkts.extend(enc.encode(&Frame::Video(scene(w, h, t))).unwrap());
        }
        pkts.extend(enc.flush().unwrap());
        assert_eq!(pkts.len(), n);
    }
}
