//! A software VP8 encoder (RFC 6386): key frames and inter frames that predict from the last frame,
//! intra 16x16 and 4x4 modes, whole-macroblock motion with quarter-pixel vectors, and one token
//! partition. The encoder reconstructs every macroblock with the decoder's own prediction, inverse
//! transforms and loop filter (through a `Vp8Decoder` that holds its reference frames), so what it
//! predicts from is exactly what a decoder will have. The boolean coder is RFC 6386 section 7.3; the
//! forward transforms follow the libvpx encoder's DCT and Walsh-Hadamard (`vp8/encoder/dct.c`).

mod boolenc;
mod mb;
mod syntax;

use std::sync::Arc;

use super::{quant_factors, FilterStrength, Picture, Vp8Decoder};
use boolenc::BoolEncoder;
use ironav_core::{Encoder, Error, ErrorCode, Frame, Packet, PixelFormat, Result, StreamParams, VideoFrame};
use syntax::FrameProbs;

/// Frames between key frames.
const KEYINT: u64 = 250;
/// The quantiser when neither a quantiser nor a bit rate is set (the H.264 encoder's default, mapped).
const DEFAULT_QP: u8 = 23;

/// The padded planes of a frame, a whole number of macroblocks each.
struct Source {
    y: Vec<u8>,
    u: Vec<u8>,
    v: Vec<u8>,
}

pub struct Vp8Encoder {
    width: usize,
    height: usize,
    mbw: usize,
    mbh: usize,
    /// The decoder's state: reference frames and the neighbouring macroblocks.
    rec: Vp8Decoder,
    /// The quantiser index for the next frame, and the bits per frame to average when rate controlled,
    /// with the bits over (or under) that target lately.
    qindex: f64,
    target: Option<f64>,
    debt: f64,
    keyint: u64,
    /// Frames since the last key frame; 0 before the first.
    since_key: u64,
}

impl Vp8Encoder {
    /// An encoder for `params` (yuv420p video) at a constant quantiser (`qp`, 0-51 as for H.264) or an
    /// average bit rate in bits per second.
    pub fn new(params: &StreamParams, qp: Option<u8>, bitrate: Option<u32>) -> Result<Self> {
        let StreamParams::Video { width, height, pix_fmt, frame_rate } = *params else {
            return Err(Error::invalid_arg("the VP8 encoder takes video"));
        };
        if pix_fmt != PixelFormat::Yuv420p {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("the software VP8 encoder takes yuv420p frames, not {}", pix_fmt.name()))
                .hint("add a format filter to convert to yuv420p"));
        }
        if width == 0 || height == 0 || width > 16383 || height > 16383 {
            return Err(Error::invalid_arg(format!("{width}x{height} is not a VP8 picture size")));
        }
        let fps = if frame_rate.num > 0 && frame_rate.den > 0 { frame_rate.num as f64 / frame_rate.den as f64 } else { 25.0 };
        let (w, h) = (width as usize, height as usize);
        let (mbw, mbh) = (w.div_ceil(16), h.div_ceil(16));
        let mut rec = Vp8Decoder::new();
        rec.resize(w, h);
        // a first guess from the bits per pixel the rate allows, about 60 at a tenth of a bit per pixel
        let qindex = match (bitrate, qp) {
            (Some(b), _) => (60.0 + 6.0 * (0.1 / (b as f64 / (fps * w as f64 * h as f64))).log2()).clamp(4.0, 120.0),
            (None, q) => q.unwrap_or(DEFAULT_QP).min(51) as f64 * 127.0 / 51.0,
        };
        Ok(Self { width: w, height: h, mbw, mbh, rec, qindex, target: bitrate.map(|b| b as f64 / fps), debt: 0.0, keyint: KEYINT, since_key: 0 })
    }

    /// Key frames every `n` frames (1 for key frames only).
    pub fn with_keyint(mut self, n: u64) -> Self {
        self.keyint = n.max(1);
        self
    }

    fn source(&self, v: &VideoFrame) -> Result<Source> {
        let (w, h) = (self.width, self.height);
        if v.pix_fmt != PixelFormat::Yuv420p || v.width as usize != w || v.height as usize != h || v.planes.len() < 3 {
            return Err(Error::invalid_input(format!("the VP8 encoder was set up for {w}x{h} yuv420p frames")));
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
        Ok(Source {
            y: pad(&v.planes[0], w, h, self.mbw * 16, self.mbh * 16),
            u: pad(&v.planes[1], cw, ch, self.mbw * 8, self.mbh * 8),
            v: pad(&v.planes[2], cw, ch, self.mbw * 8, self.mbh * 8),
        })
    }

    /// Codes one frame: every macroblock is decided and reconstructed, the loop filter runs, and the
    /// frame is written with its header and the two partitions.
    fn code_frame(&mut self, src: &Source, key: bool, q: i32) -> Result<Vec<u8>> {
        let (ys, cs) = (self.mbw * 16, self.mbw * 8);
        let quant = quant_factors(q, [0; 5]);
        let level = (q * 3 / 8).min(63);
        let mut cur = Picture { y: vec![0; ys * self.mbh * 16], u: vec![0; cs * self.mbh * 8], v: vec![0; cs * self.mbh * 8] };
        let stride = self.mbw + 1;
        let mut strengths = vec![FilterStrength::default(); self.mbw * self.mbh];
        let mut coded = Vec::with_capacity(self.mbw * self.mbh);
        for mb_y in 0..self.mbh {
            for mb_x in 0..self.mbw {
                let (mb, c) = self.code_mb(&mut cur, src, key, mb_x, mb_y, &quant)?;
                strengths[mb_y * self.mbw + mb_x] = self.rec.filter_strength(&mb, level, 0);
                self.rec.mbs[(mb_y + 1) * stride + mb_x + 1] = mb;
                coded.push((mb, c));
            }
        }
        if level != 0 {
            self.rec.loop_filter(&mut cur, &strengths, false, key);
        }
        let cur = Arc::new(cur);
        if key {
            self.rec.refs = [None, Some(cur.clone()), Some(cur.clone()), Some(cur)];
        } else {
            self.rec.refs[1] = Some(cur);
        }

        let n = coded.len() as f64;
        let prob = |count: f64| ((count / n * 256.0).round() as i32).clamp(1, 255) as u8;
        let skipped = coded.iter().filter(|(m, _)| m.skip).count() as f64;
        let intra = coded.iter().filter(|(m, _)| m.ref_frame == 0).count() as f64;
        let probs = FrameProbs { skip: prob(n - skipped), intra: prob(intra), last: 255 };

        let mut first = BoolEncoder::default();
        write_header(&mut first, key, q, level, probs);
        syntax::write_modes(&mut first, &self.rec, key, probs);
        let mut tokens = BoolEncoder::default();
        syntax::write_tokens(&mut tokens, &self.rec, &coded);
        let first = first.finish();
        let tokens = tokens.finish();

        let tag = u32::from(!key) | (1 << 4) | ((first.len() as u32) << 5);
        let mut out = Vec::with_capacity(10 + first.len() + tokens.len());
        out.extend_from_slice(&tag.to_le_bytes()[..3]);
        if key {
            out.extend_from_slice(&[0x9d, 0x01, 0x2a]);
            out.extend_from_slice(&(self.width as u16).to_le_bytes());
            out.extend_from_slice(&(self.height as u16).to_le_bytes());
        }
        out.extend_from_slice(&first);
        out.extend_from_slice(&tokens);
        Ok(out)
    }

    /// Rate control: the quantiser follows how far each frame's size was from its share of the bit rate,
    /// and how far the recent frames have drifted from it in total. Key frames only add to the drift:
    /// they are much larger, and the quantiser has to hold for the frames after them.
    fn adapt(&mut self, bits: f64, key: bool) {
        let Some(target) = self.target else { return };
        self.debt = self.debt * 0.8 + bits - target;
        if !key {
            let step = 6.0 * (bits / target).log2().clamp(-1.5, 1.5) + 4.0 * (self.debt / (target * 4.0)).clamp(-1.0, 1.0);
            self.qindex = (self.qindex + step).clamp(0.0, 127.0);
        }
    }
}

/// The frame header at the start of the first partition: no segments or loop filter deltas, normal
/// loop filter, one token partition, and the default probabilities with no updates to them.
fn write_header(e: &mut BoolEncoder, key: bool, q: i32, level: i32, p: FrameProbs) {
    if key {
        e.literal(0, 2); // colour space and clamping type
    }
    e.literal(0, 1); // no segmentation
    e.literal(0, 1); // normal loop filter
    e.literal(level as u32, 6);
    e.literal(0, 3); // sharpness
    e.literal(0, 1); // no loop filter deltas
    e.literal(0, 2); // one token partition
    e.literal(q as u32, 7);
    e.literal(0, 5); // no quantiser deltas
    if key {
        e.literal(1, 1); // the probabilities this frame updates persist
    } else {
        e.literal(0, 2); // golden and altref are not refreshed
        e.literal(0, 4); // nor copied
        e.literal(0, 2); // sign biases
        e.literal(1, 1); // the probabilities persist
        e.literal(1, 1); // the last frame is refreshed
    }
    for plane in &crate::vp8::tables::TOKEN_UPDATE_PROBS {
        for band in plane {
            for ctx in band {
                for &prob in ctx {
                    e.put(prob, false);
                }
            }
        }
    }
    e.put(128, true); // macroblocks carry a skip flag
    e.literal(p.skip as u32, 8);
    if !key {
        e.literal(p.intra as u32, 8);
        e.literal(p.last as u32, 8);
        e.literal(128, 8); // the golden probability, unused
        e.put(128, false); // no new 16x16 mode probabilities
        e.put(128, false); // no new chroma mode probabilities
        for comp in &crate::vp8::tables::MV_UPDATE_PROB {
            for &prob in comp {
                e.put(prob, false);
            }
        }
    }
}

impl Encoder for Vp8Encoder {
    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>> {
        let Frame::Video(v) = frame else {
            return Err(Error::invalid_input("the VP8 encoder received an audio frame"));
        };
        let src = self.source(v)?;
        let key = self.since_key == 0 || self.since_key >= self.keyint;
        let q = self.qindex.round().clamp(0.0, 127.0) as i32;
        let data = self.code_frame(&src, key, q)?;
        self.adapt(data.len() as f64 * 8.0, key);
        self.since_key = if key { 1 } else { self.since_key + 1 };
        Ok(vec![Packet { stream: 0, pts: v.pts, duration: 1, data }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironav_core::{Decoder, Frame};

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

    /// Crops a padded picture plane to `w` x `h`.
    fn crop(p: &[u8], stride: usize, w: usize, h: usize) -> Vec<u8> {
        (0..h).flat_map(|r| p[r * stride..r * stride + w].to_vec()).collect()
    }

    fn params(w: u32, h: u32) -> StreamParams {
        StreamParams::Video { width: w, height: h, pix_fmt: PixelFormat::Yuv420p, frame_rate: ironav_core::Rational::new(30, 1) }
    }

    /// Encodes `n` frames and decodes them with the IronAV decoder. Checks that each decoded frame is
    /// the encoder's reconstruction, bit for bit, and returns the packets with the PSNR against the source.
    fn round_trip(w: usize, h: usize, n: usize, qp: Option<u8>, bitrate: Option<u32>, keyint: u64) -> (Vec<usize>, f64) {
        let mut enc = Vp8Encoder::new(&params(w as u32, h as u32), qp, bitrate).unwrap().with_keyint(keyint);
        let mut dec = Vp8Decoder::new();
        let (mut sizes, mut psnrs) = (Vec::new(), Vec::new());
        for t in 0..n {
            let src = scene(w, h, t);
            let pkt = enc.encode(&Frame::Video(src.clone())).unwrap().remove(0);
            sizes.push(pkt.data.len());
            let out = match dec.decode(&Packet { stream: 0, pts: t as i64, duration: 1, data: pkt.data }).unwrap().remove(0) {
                Frame::Video(v) => v,
                _ => unreachable!(),
            };
            let rec = enc.rec.refs[1].as_ref().unwrap();
            let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
            let want = [crop(&rec.y, enc.mbw * 16, w, h), crop(&rec.u, enc.mbw * 8, cw, ch), crop(&rec.v, enc.mbw * 8, cw, ch)];
            assert!(out.planes == want, "frame {t}: the decoder's picture differs from the encoder's reconstruction");
            psnrs.push(psnr(&out.planes[0], &src.planes[0]));
        }
        (sizes, psnrs.iter().sum::<f64>() / n as f64)
    }

    #[test]
    fn reconstruction_matches_the_decoder_on_motion() {
        let (sizes, p) = round_trip(64, 48, 6, Some(30), None, 250);
        assert!(p > 28.0, "PSNR {p:.1}");
        assert!(sizes[1] < sizes[0], "inter frames are smaller: {sizes:?}");
    }

    #[test]
    fn odd_sizes_round_trip() {
        round_trip(177, 99, 4, Some(40), None, 250);
    }

    #[test]
    fn rate_control_tracks_the_bit_rate() {
        let (w, h, n) = (176, 144, 30);
        let mut enc = Vp8Encoder::new(&params(w as u32, h as u32), None, Some(300_000)).unwrap();
        let mut bits = 0usize;
        for t in 0..n {
            bits += enc.encode(&Frame::Video(scene(w, h, t))).unwrap()[0].data.len() * 8;
        }
        let kbps = bits as f64 * 30.0 / n as f64 / 1000.0;
        assert!((150.0..450.0).contains(&kbps), "{kbps:.0} kbit/s for a 300 kbit/s target");
    }
}
