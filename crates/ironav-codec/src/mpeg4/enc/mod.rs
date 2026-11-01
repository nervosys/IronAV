//! A software MPEG-4 Part 2 video encoder for the Simple profile: I and P pictures of a rectangular,
//! progressive video object layer, with half-sample motion (one vector or four per macroblock),
//! intra DC and AC prediction, H.263 quantisation, and run-level escapes 1 to 3.
//!
//! The bit syntax is the one the decoder in `mpeg4/mb.rs` reads, and the headers and VLC tables are
//! the decoder's (from FFmpeg's `mpeg4data.h` and `h263data.c`; Michael Niedermayer, Fabrice Bellard
//! and the FFmpeg developers, LGPL-2.1-or-later). Every picture is reconstructed with the decoder's
//! own half-sample motion compensation and inverse transform, so the decoder's output is the
//! encoder's reconstruction, and the references the encoder predicts from are exactly those.
//!
//! Not written: B-pictures, quarter-sample motion, MPEG quantisation, interlaced pictures, resync
//! markers and data partitioning. The layer is progressive, so the frames are as the decoder writes
//! them: cropped to the picture size, with the macroblock-aligned padding coded but not shown.

mod mb;

use super::{tables as T, Pic, Vol};
use ironav_core::{Encoder, Error, Frame, Packet, PixelFormat, Result, StreamParams, VideoFrame};
use mb::Coder;
use std::sync::Arc;

/// Frames between key frames.
const KEYINT: u64 = 250;
/// The quantiser when neither a quantiser nor a bit rate is set.
const DEFAULT_QSCALE: i32 = 5;
/// The first quantiser of a rate-controlled stream, before the first frame's size is known.
const START_QSCALE: f64 = 6.0;
/// The motion search's range in half samples: vectors lie within plus or minus this.
pub(super) const SEARCH: i32 = 30;

/// The macroblock-aligned planes of a source frame.
pub(super) struct Source {
    pub y: Vec<u8>,
    pub u: Vec<u8>,
    pub v: Vec<u8>,
}

/// An MSB-first bit writer.
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

    /// Pads with zero bits to a byte boundary and returns the bytes.
    pub fn finish(mut self) -> Vec<u8> {
        while self.n != 0 {
            self.bit(false);
        }
        self.out
    }
}

/// The start codes and the video object layer that describe a stream of `width` by `height` at the
/// frame rate `num / den`: the visual object sequence, the visual object, and the layer header.
/// The bytes are the stream's codec configuration (the `esds` decoder specific info or Matroska's
/// CodecPrivate), and a key picture carries them in front of it.
pub fn config(width: u32, height: u32, num: i64) -> Vec<u8> {
    let res = num.clamp(1, 65535) as u32;
    let mut out = vec![0, 0, 1, 0xB0, 0x01]; // visual object sequence, simple profile at level 1
    out.extend_from_slice(&[0, 0, 1, 0xB5, 0x08]); // visual object: video, no signal type
    out.extend_from_slice(&[0, 0, 1, 0x20]); // video object layer 0
    let mut b = BitW::default();
    b.put(0, 1); // random accessible
    b.put(1, 8); // video object type: simple
    b.put(0, 1); // no layer identifier
    b.put(1, 4); // aspect ratio 1:1
    b.put(0, 1); // no vol control parameters
    b.put(0, 2); // rectangular
    b.put(1, 1);
    b.put(res, 16); // time increment resolution
    b.put(1, 1);
    b.put(0, 1); // no fixed rate
    b.put(1, 1);
    b.put(width, 13);
    b.put(1, 1);
    b.put(height, 13);
    b.put(1, 1);
    b.put(0, 1); // progressive
    b.put(1, 1); // no OBMC
    b.put(0, 1); // no sprites
    b.put(0, 1); // 8-bit quantiser precision
    b.put(0, 1); // H.263 quantisation
    b.put(1, 1); // no complexity estimation
    b.put(1, 1); // no resync markers
    b.put(0, 1); // no data partitioning
    b.put(0, 1); // no scalability
    out.extend(b.finish());
    out
}

pub struct Mpeg4Encoder {
    vol: Vol,
    config: Vec<u8>,
    num: i64,
    den: i64,
    fcode: i32,
    /// Pictures coded so far, and the second of the last one (the time base of the next).
    frames: u64,
    last_sec: i64,
    keyint: u64,
    /// Pictures since the last key picture; 0 before the first.
    since_key: u64,
    /// The quantiser when it is fixed, else the rate control's (fractional) quantiser.
    fixed: Option<i32>,
    qscale: f64,
    /// Bits per picture to average when rate controlled, and the bits over (or under) that lately.
    target: Option<f64>,
    debt: f64,
    /// The last reconstructed picture, which the next one predicts from.
    refp: Option<Arc<Pic>>,
}

impl Mpeg4Encoder {
    /// An encoder for `params` (yuv420p video) at a constant quantiser (`qp`, the MPEG-4 quantiser
    /// 1 to 31, larger is smaller) or an average bit rate in bits per second.
    pub fn new(params: &StreamParams, qp: Option<u8>, bitrate: Option<u32>) -> Result<Self> {
        let StreamParams::Video { width, height, pix_fmt, frame_rate } = *params else {
            return Err(Error::invalid_arg("the MPEG-4 encoder takes video"));
        };
        if pix_fmt != PixelFormat::Yuv420p {
            return Err(Error::new(ironav_core::ErrorCode::UnsupportedCodec, format!("the software MPEG-4 encoder takes yuv420p frames, not {}", pix_fmt.name()))
                .hint("add a format filter to convert to yuv420p"));
        }
        if width == 0 || height == 0 || width > 8191 || height > 8191 {
            return Err(Error::invalid_arg(format!("{width}x{height} is not an MPEG-4 picture size (1 to 8191 each way)")));
        }
        let (num, den) = if frame_rate.num > 0 && frame_rate.den > 0 { (frame_rate.num, frame_rate.den) } else { (25, 1) };
        let (w, h) = (width as usize, height as usize);
        let (mbw, mbh) = (w.div_ceil(16), h.div_ceil(16));
        let res = num.clamp(1, 65535);
        let time_bits = (64 - ((res - 1) as u64).leading_zeros()).max(1);
        let vol = Vol {
            width: w,
            height: h,
            mbw,
            mbh,
            quarter: false,
            resync: false,
            quant_precision: 5,
            time_bits,
            time_res: res,
            mpeg_quant: false,
            intra_matrix: T::DEFAULT_INTRA_MATRIX,
            inter_matrix: T::DEFAULT_INTER_MATRIX,
        };
        // the vectors of a macroblock are at most SEARCH half samples, so a difference is at most twice that
        let mut fcode = 1;
        while (32i32 << (fcode - 1)) < 2 * SEARCH {
            fcode += 1;
        }
        let fixed = match (qp, bitrate) {
            (Some(q), _) => Some((q as i32).clamp(1, 31)),
            (None, Some(_)) => None,
            (None, None) => Some(DEFAULT_QSCALE),
        };
        let fps = num as f64 / den as f64;
        Ok(Self {
            config: config(width, height, num),
            vol,
            num,
            den,
            fcode,
            frames: 0,
            last_sec: 0,
            keyint: KEYINT,
            since_key: 0,
            fixed,
            qscale: fixed.map_or(START_QSCALE, f64::from),
            target: bitrate.map(|b| b as f64 / fps),
            debt: 0.0,
            refp: None,
        })
    }

    /// Key pictures every `n` pictures (1 for key pictures only).
    pub fn with_keyint(mut self, n: u64) -> Self {
        self.keyint = n.max(1);
        self
    }

    /// The codec configuration to hand a container (the visual object sequence, object and layer).
    pub fn config(&self) -> &[u8] {
        &self.config
    }

    fn source(&self, v: &VideoFrame) -> Result<Source> {
        let (w, h) = (self.vol.width, self.vol.height);
        if v.pix_fmt != PixelFormat::Yuv420p || v.width as usize != w || v.height as usize != h || v.planes.len() < 3 {
            return Err(Error::invalid_input(format!("the MPEG-4 encoder was set up for {w}x{h} yuv420p frames")));
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
        let (mbw, mbh) = (self.vol.mbw, self.vol.mbh);
        Ok(Source {
            y: pad(&v.planes[0], w, h, mbw * 16, mbh * 16),
            u: pad(&v.planes[1], cw, ch, mbw * 8, mbh * 8),
            v: pad(&v.planes[2], cw, ch, mbw * 8, mbh * 8),
        })
    }

    /// Codes one picture: its header, then every macroblock, which are reconstructed as they go.
    /// Returns the picture's bytes (start code included) and its reconstruction.
    fn code_picture(&self, src: &Source, key: bool, q: i32, modulo: i64, inc: i64) -> (Vec<u8>, Pic) {
        let mut bw = BitW::default();
        bw.put(if key { 0 } else { 1 }, 2); // vop_coding_type
        for _ in 0..modulo.max(0) {
            bw.put(1, 1); // whole seconds since the last picture
        }
        bw.put(0, 1);
        bw.put(1, 1);
        bw.put(inc as u32, self.vol.time_bits);
        bw.put(1, 1);
        bw.put(1, 1); // vop_coded
        if !key {
            bw.put(0, 1); // rounding type
        }
        bw.put(0, 3); // intra_dc_vlc_thr 0: the DC is always coded with its VLC
        bw.put(q as u32, 5);
        if !key {
            bw.put(self.fcode as u32, 3);
        }
        let refp = if key { None } else { self.refp.clone() };
        let mut c = Coder::new(&self.vol, src, refp, q, self.fcode, bw);
        for mby in 0..self.vol.mbh {
            for mbx in 0..self.vol.mbw {
                if key {
                    c.intra_mb(mbx, mby, false);
                } else {
                    c.p_mb(mbx, mby);
                }
            }
        }
        let (bits, pic) = c.finish();
        let mut out = vec![0, 0, 1, 0xB6];
        out.extend(bits);
        (out, pic)
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
}

impl Encoder for Mpeg4Encoder {
    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>> {
        let Frame::Video(v) = frame else {
            return Err(Error::invalid_input("the MPEG-4 encoder received an audio frame"));
        };
        let src = self.source(v)?;
        let key = self.since_key == 0 || self.since_key >= self.keyint;
        let q = self.fixed.unwrap_or(self.qscale.round() as i32).clamp(1, 31);
        // the picture's time in ticks of the layer's resolution: frame n is n * den / num seconds
        let res = self.vol.time_res;
        let ticks = (self.frames as i128 * self.den as i128 * res as i128 / self.num as i128) as i64;
        let (sec, inc) = (ticks / res, ticks % res);
        let (body, pic) = self.code_picture(&src, key, q, sec - self.last_sec, inc);
        self.adapt(body.len() as f64 * 8.0, key);
        let mut data = if key { self.config.clone() } else { Vec::new() };
        data.extend_from_slice(&body);
        self.refp = Some(Arc::new(pic));
        self.last_sec = sec;
        self.frames += 1;
        self.since_key = if key { 1 } else { self.since_key + 1 };
        Ok(vec![Packet { stream: 0, pts: v.pts, duration: 1, data }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mpeg4::Mpeg4Decoder;
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

    /// Encodes `n` pictures and decodes them with the IronAV decoder. Checks that the decoder's
    /// pictures are the encoder's reconstructions bit for bit, and returns the packet sizes and the
    /// mean luma PSNR against the source.
    fn round_trip(w: usize, h: usize, n: usize, qp: Option<u8>, bitrate: Option<u32>, keyint: u64) -> (Vec<usize>, f64) {
        let mut enc = Mpeg4Encoder::new(&params(w as u32, h as u32, 25), qp, bitrate).unwrap().with_keyint(keyint);
        let mut dec = Mpeg4Decoder::new(&[]);
        let (mut sizes, mut recs, mut srcs) = (Vec::new(), Vec::new(), Vec::new());
        let mut out = Vec::new();
        for t in 0..n {
            let src = scene(w, h, t);
            let pkt = enc.encode(&Frame::Video(src.clone())).unwrap().remove(0);
            sizes.push(pkt.data.len());
            out.extend(dec.decode(&Packet { stream: 0, pts: t as i64, duration: 1, data: pkt.data }).unwrap());
            recs.push(enc.refp.clone().unwrap());
            srcs.push(src);
        }
        out.extend(dec.flush().unwrap());
        assert_eq!(out.len(), n, "the decoder returns every picture");
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let mut p = 0.0;
        for (t, f) in out.iter().enumerate() {
            let Frame::Video(v) = f else { unreachable!() };
            let rec = &recs[t];
            let (lw, cs) = (enc.vol.mbw * 16, enc.vol.mbw * 8);
            let want = [crop(&rec.planes[0], lw, w, h), crop(&rec.planes[1], cs, cw, ch), crop(&rec.planes[2], cs, cw, ch)];
            assert!(v.planes == want, "picture {t}: the decoder's picture differs from the encoder's reconstruction");
            p += psnr(&v.planes[0], &srcs[t].planes[0]);
        }
        (sizes, p / n as f64)
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
    fn rate_control_tracks_the_bit_rate() {
        let (w, h, n) = (176, 144, 30);
        let mut enc = Mpeg4Encoder::new(&params(w as u32, h as u32, 30), None, Some(300_000)).unwrap();
        let mut bits = 0usize;
        for t in 0..n {
            bits += enc.encode(&Frame::Video(scene(w, h, t))).unwrap()[0].data.len() * 8;
        }
        let kbps = bits as f64 * 30.0 / n as f64 / 1000.0;
        assert!((150.0..450.0).contains(&kbps), "{kbps:.0} kbit/s for a 300 kbit/s target");
    }
}
