//! A software AV1 encoder: key frames every `keyint` frames (250 by default, as the VP9 and HEVC
//! encoders), and inter frames between them. Profile 0, 8-bit 4:2:0, one tile.
//!
//! Each frame is a temporal unit: a temporal delimiter, the sequence header, and a frame OBU with
//! the frame's header and its tile. An inter frame predicts from LAST_FRAME alone, the frame before
//! it (every reference slot holds the last frame, and each frame refreshes slot 0), with the
//! regular 8-tap filter, quarter-sample motion vectors (searched to half samples) and the reference MV stack's candidates. Superblocks
//! are 64x64, split by an open-loop search on the source down to 8x8 (never larger than 32x32, so
//! every block is coded with one transform of its size). Each block is coded as intra or inter by
//! rate-distortion cost, and intra blocks take the best of the 13 intra modes with angle deltas.
//! Coefficients are quantised at the base quantiser index that `qp` maps to (as for the other
//! encoders, 0 to 51). The deblocking filter and CDEF run on each reconstructed frame, with levels
//! and strengths that follow the quantiser, and the reference is the filtered frame.
//!
//! The encoder keeps the decoder's own per-frame state (`av1::block::FrameDec`) and reconstructs
//! through it, so the pictures it keeps as references are the decoder's pictures.
//!
//! The range coder (`rc.rs`) is adapted from rav1e's `src/ec.rs`, which is BSD-2-Clause licensed
//! (the rav1e contributors); rav1e is not a dependency.

mod fwd;
mod headers;
mod motion;
mod rc;
mod tile;

pub use headers::config_record;

use std::sync::Arc;

use ironav_core::{Encoder, Error, ErrorCode, Frame, Packet, PixelFormat, Result, StreamParams, VideoFrame};

use crate::av1::block::{FrameDec, Picture};
use crate::av1::ipred::Plane;
use crate::av1::obu::{parse_frame_header, parse_sequence_header, RefInfo, RefState, SequenceHeader};
use crate::av1::postfilter;
use crate::av1::superres::downscaled_width;
use headers::FrameParams;

/// The quantiser when none is set (the VP9 encoder's default).
const DEFAULT_QP: u8 = 23;

/// The key frame interval when none is set (the VP9 and HEVC encoders' default).
const DEFAULT_KEYINT: u64 = 250;

/// The largest picture one tile holds: 4096 wide, and no more than 2304 superblocks, so that the
/// minimum tile counts stay zero.
const MAX_WIDTH: u32 = 4096;
const MAX_SUPERBLOCKS: usize = 2304;

/// The base quantiser index of a quantiser of 0 to 51 (as the VP9 encoder maps it). Index 0 would be
/// lossless, which this encoder does not code.
fn base_q(qp: u8) -> u32 {
    (qp.min(51) as u32 * 255 / 51).max(1)
}

/// The deblocking level of a base quantiser index: a level that rises with the quantiser, from 0 at
/// the finest to 31 at the coarsest.
fn filter_level(q: u32) -> u8 {
    (q / 8).min(31) as u8
}

/// The CDEF strengths (primary and secondary, luma then chroma) of a base quantiser index.
fn cdef_strengths(q: u32) -> [u8; 4] {
    let pri = (q / 32).min(15) as u8;
    [pri, 1, pri / 2, 0]
}

pub struct Av1Encoder {
    width: usize,
    height: usize,
    base_q: u32,
    keyint: u64,
    /// Frames since the last key frame (1 right after one).
    since_key: u64,
    seq: SequenceHeader,
    /// The last frame as a reference, with what the frame headers read of it.
    last: Option<(Arc<Picture>, RefState)>,
    /// The source luma of the last frame, which the next frame's motion field reads.
    prev_src: Option<Plane>,
    /// The forward transform bases, built on first use and kept across frames.
    bases: fwd::Bases,
    /// The superres denominator, when frames are coded at a downscaled width (see `with_superres`).
    superres: Option<u32>,
    /// Whether frames use loop restoration (see `with_restoration`).
    restoration: bool,
}

impl Av1Encoder {
    /// An encoder for `params` (yuv420p video) at a constant quantiser (`qp`, 0 to 51). A bit rate is
    /// not followed: the quantiser stays where `qp` puts it.
    pub fn new(params: &StreamParams, qp: Option<u8>) -> Result<Self> {
        let StreamParams::Video { width, height, pix_fmt, .. } = *params else {
            return Err(Error::invalid_arg("the AV1 encoder takes video"));
        };
        if pix_fmt != PixelFormat::Yuv420p {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("the software AV1 encoder takes yuv420p frames, not {}", pix_fmt.name()))
                .hint("add a format filter to convert to yuv420p"));
        }
        let superblocks = width.div_ceil(64) as usize * height.div_ceil(64) as usize;
        if width == 0 || height == 0 || width > MAX_WIDTH || superblocks > MAX_SUPERBLOCKS {
            return Err(Error::invalid_arg(format!("{width}x{height} is too large for the software AV1 encoder (one tile, up to 4096 wide)")));
        }
        let seq = parse_sequence_header(&headers::sequence_header(width, height))?;
        Ok(Self {
            width: width as usize,
            height: height as usize,
            base_q: base_q(qp.unwrap_or(DEFAULT_QP)),
            keyint: DEFAULT_KEYINT,
            since_key: 0,
            seq,
            last: None,
            prev_src: None,
            bases: fwd::Bases::default(),
            superres: None,
            restoration: false,
        })
    }

    /// Sets the key frame interval: a key frame every `n` frames (at least 1).
    pub fn with_keyint(mut self, n: u64) -> Self {
        self.keyint = n.max(1);
        self
    }

    /// Codes every frame with superres at `denom` (9 to 16, the denominator of 8ths of the width):
    /// the frame is coded at the downscaled width (the source is resampled horizontally), and its
    /// reconstruction is upscaled before it becomes a reference, as the decoder does. Used by the
    /// tests, which compare the decoder's output with libaom's.
    pub fn with_superres(mut self, denom: u32) -> Self {
        assert!((9..=16).contains(&denom), "superres denominator {denom} is not 9 to 16");
        self.superres = Some(denom);
        self.set_sequence_header();
        self
    }

    /// Codes every frame with loop restoration: the Wiener filter on each plane, in units of 64
    /// luma samples (32 chroma) of which every other one is filtered and the rest are left alone.
    /// The filter's coefficients are the midpoints, so the filter is the same in every unit that
    /// uses it. Used by the tests, with superres.
    pub fn with_restoration(mut self) -> Self {
        self.restoration = true;
        self.set_sequence_header();
        self
    }

    /// Parses the sequence header of the options set so far, for the encoder's own frame headers.
    fn set_sequence_header(&mut self) {
        let hdr = headers::sequence_header_with(self.width as u32, self.height as u32, self.superres.is_some(), self.restoration);
        self.seq = parse_sequence_header(&hdr).expect("the sequence header of the options parses");
    }

    /// The temporal unit of one picture.
    pub fn encode_frame(&mut self, v: &VideoFrame) -> Result<Vec<u8>> {
        let (w, h) = (self.width, self.height);
        if v.pix_fmt != PixelFormat::Yuv420p || v.width as usize != w || v.height as usize != h || v.planes.len() < 3 {
            return Err(Error::invalid_input(format!("the AV1 encoder was set up for {w}x{h} yuv420p frames")));
        }
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        if v.planes[0].len() < w * h || v.planes[1].len() < cw * ch || v.planes[2].len() < cw * ch {
            return Err(Error::invalid_input("a video frame plane is shorter than its dimensions"));
        }
        let key = self.last.is_none() || self.since_key >= self.keyint;
        let params = FrameParams {
            key,
            width: w as u32,
            height: h as u32,
            base_q_idx: self.base_q,
            lf_level: filter_level(self.base_q),
            cdef: cdef_strengths(self.base_q),
            superres: self.superres,
            restoration: self.restoration,
        };
        let hdr = headers::frame_header(&params);

        // the frame's header as the decoder reads it, with every slot holding the last frame's state
        let refs = RefInfo { slots: std::array::from_fn(|_| self.last.as_ref().map(|(_, s)| s.clone())) };
        let fh = parse_frame_header(&hdr, &self.seq, &refs, 0, 0)?;
        // with superres the frame is coded at its downscaled width, from the resampled source
        let coded_w = self.superres.map_or(w, |d| downscaled_width(w as u32, d) as usize);
        let resampled = self.superres.map(|_| resample_planes(v, w, h, coded_w));
        let planes = match &resampled {
            Some(p) => [&p[0][..], &p[1][..], &p[2][..]],
            None => [&v.planes[0][..], &v.planes[1][..], &v.planes[2][..]],
        };
        let mut f = FrameDec::new(self.seq.clone(), fh);
        if let (false, Some((pic, _))) = (key, &self.last) {
            for slot in f.refs.iter_mut().skip(1) {
                *slot = Some(pic.clone());
            }
        }
        let enc = tile::FrameEnc::new(f, planes[0], planes[1], planes[2], (coded_w, h), self.prev_src.clone(), std::mem::take(&mut self.bases));
        let (tile_bytes, mut f, src_luma, bases) = enc.encode();
        self.bases = bases;

        // the post filters, as the decoder runs them on the frame
        postfilter(&mut f);
        let state = RefState {
            frame_type: f.h.frame_type,
            order_hint: f.h.order_hint,
            upscaled_width: f.h.upscaled_width,
            height: f.h.height,
            render_width: f.h.render_width,
            render_height: f.h.render_height,
            lf_ref_deltas: f.h.lf_ref_deltas,
            lf_mode_deltas: f.h.lf_mode_deltas,
            feature_enabled: f.h.feature_enabled,
            feature_data: f.h.feature_data,
            gm_params: f.h.gm_params,
            grain: None,
        };
        let pic = Picture {
            width: w,
            height: h,
            bd: 8,
            ss: [1, 1],
            mono: false,
            rgb: false,
            frame_type: f.h.frame_type,
            order_hint: 0,
            saved_hints: [0; 8],
            mi_rows: f.h.mi_rows,
            mi_cols: f.h.mi_cols,
            mvs: std::mem::take(&mut f.saved_mvs),
            seg_ids: Vec::new(),
            planes: std::mem::replace(&mut f.planes, [Plane::new(0, 0), Plane::new(0, 0), Plane::new(0, 0)]),
        };
        self.last = Some((Arc::new(pic), state));
        self.prev_src = Some(src_luma);
        self.since_key = if key { 1 } else { self.since_key + 1 };

        let mut frame = hdr;
        frame.extend_from_slice(&tile_bytes);
        let mut out = headers::obu(headers::OBU_TEMPORAL_DELIMITER, &[]);
        out.extend(headers::obu(headers::OBU_SEQUENCE_HEADER, &headers::sequence_header_with(w as u32, h as u32, self.superres.is_some(), self.restoration)));
        out.extend(headers::obu(headers::OBU_FRAME, &frame));
        Ok(out)
    }
}

/// The planes of a yuv420p picture of `w` x `h` resampled horizontally to `dw` luma columns (and
/// (dw + 1) / 2 chroma columns), tightly packed. Each output sample is the average of the source
/// samples its span covers, weighted by the overlap.
fn resample_planes(v: &VideoFrame, w: usize, h: usize, dw: usize) -> [Vec<u8>; 3] {
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let resample = |src: &[u8], sw: usize, rows: usize, ow: usize| -> Vec<u8> {
        let mut out = Vec::with_capacity(ow * rows);
        let scale = sw as f64 / ow as f64;
        for y in 0..rows {
            let s = &src[y * sw..(y + 1) * sw];
            for x in 0..ow {
                let (a, b) = (x as f64 * scale, (x + 1) as f64 * scale);
                let (mut acc, mut total) = (0.0, 0.0);
                for (i, &p) in s.iter().enumerate().take((b.ceil() as usize).min(sw)).skip(a.floor() as usize) {
                    let overlap = (i as f64 + 1.0).min(b) - (i as f64).max(a);
                    if overlap > 0.0 {
                        acc += overlap * p as f64;
                        total += overlap;
                    }
                }
                out.push((acc / total).round() as u8);
            }
        }
        out
    };
    let dcw = dw.div_ceil(2);
    [resample(&v.planes[0], w, h, dw), resample(&v.planes[1][..cw * ch], cw, ch, dcw), resample(&v.planes[2][..cw * ch], cw, ch, dcw)]
}

impl Encoder for Av1Encoder {
    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>> {
        let Frame::Video(v) = frame else {
            return Err(Error::invalid_input("the AV1 encoder received an audio frame"));
        };
        let data = self.encode_frame(v)?;
        Ok(vec![Packet { stream: 0, pts: v.pts, duration: 1, data }])
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::av1::Av1Decoder;
    use ironav_core::{Decoder, Frame as CoreFrame};

    /// A test picture: gradients, a diagonal edge and a little noise, so that every mode is used.
    /// `shift` moves the picture to the right and down by that many samples, for a moving clip.
    fn picture_shifted(w: usize, h: usize, shift: usize) -> VideoFrame {
        let mut seed = 1u32;
        let mut noise = move || {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            ((seed >> 16) % 9) as i32 - 4
        };
        let luma: Vec<u8> = (0..w * h)
            .map(|i| {
                let (x, y) = ((i % w + w - shift.min(w)) % w, (i / w + h - shift.min(h)) % h);
                let v = if x + y > (w + h) / 2 { 200 } else { (x * 2 + y) as i32 } + noise();
                v.clamp(0, 255) as u8
            })
            .collect();
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let chroma = |k: usize| -> Vec<u8> { (0..cw * ch).map(|i| ((i % cw) * 3 + (i / cw) * k) as u8).collect() };
        VideoFrame { width: w as u32, height: h as u32, pix_fmt: PixelFormat::Yuv420p, pts: 0, planes: vec![luma, chroma(1), chroma(2)] }
    }

    fn picture(w: usize, h: usize) -> VideoFrame {
        picture_shifted(w, h, 0)
    }

    fn psnr(a: &[u8], b: &[u8]) -> f64 {
        let mse: f64 = a.iter().zip(b).map(|(&x, &y)| (x as f64 - y as f64).powi(2)).sum::<f64>() / a.len() as f64;
        10.0 * (255.0 * 255.0 / mse.max(1e-9)).log10()
    }

    fn params(w: usize, h: usize) -> StreamParams {
        StreamParams::Video { width: w as u32, height: h as u32, pix_fmt: PixelFormat::Yuv420p, frame_rate: ironav_core::Rational { num: 25, den: 1 } }
    }

    /// The Y plane of a decoded frame.
    fn luma_of(f: CoreFrame) -> Vec<u8> {
        let CoreFrame::Video(out) = f else { panic!("no picture") };
        out.planes[0][..out.width as usize * out.height as usize].to_vec()
    }

    /// A picture through the encoder and IronAV's own decoder comes back close to the source, at
    /// sizes with and without a partial superblock.
    #[test]
    fn round_trips_through_the_decoder() {
        for &(w, h) in &[(64usize, 48usize), (177, 99)] {
            let src = picture(w, h);
            let mut enc = Av1Encoder::new(&params(w, h), Some(20)).unwrap();
            let data = enc.encode_frame(&src).unwrap();
            let mut dec = Av1Decoder::new(&[]);
            let mut frames = dec.decode(&Packet { stream: 0, pts: 0, duration: 1, data }).unwrap();
            frames.extend(dec.flush().unwrap());
            let p = psnr(&src.planes[0], &luma_of(frames.remove(0)));
            assert!(p > 35.0, "{w}x{h}: luma PSNR {p:.1} dB");
        }
    }

    /// Key and inter frames: each decoded frame equals the encoder's own reconstruction, bit for bit,
    /// and the moving clip is coded with inter frames at a useful quality.
    #[test]
    fn inter_frames_decode_to_the_encoders_reconstruction() {
        for &(w, h) in &[(96usize, 64usize), (177, 99)] {
            let mut enc = Av1Encoder::new(&params(w, h), Some(24)).unwrap().with_keyint(4);
            let mut dec = Av1Decoder::new(&[]);
            for t in 0..6 {
                let src = picture_shifted(w, h, 2 * t);
                let data = enc.encode_frame(&src).unwrap();
                let recon = enc.last.as_ref().unwrap().0.planes[0].clone();
                let mut frames = dec.decode(&Packet { stream: 0, pts: t as i64, duration: 1, data }).unwrap();
                assert_eq!(frames.len(), 1, "frame {t} shows one picture");
                let got = luma_of(frames.remove(0));
                let want: Vec<u8> = (0..w * h).map(|i| recon.at(i % w, i / w) as u8).collect();
                assert!(got == want, "{w}x{h} frame {t}: decoder differs from the encoder's reconstruction");
                if t % 2 == 1 {
                    let p = psnr(&src.planes[0], &got);
                    assert!(p > 30.0, "{w}x{h} frame {t}: luma PSNR {p:.1} dB");
                }
            }
        }
    }

    /// Superres: frames coded at a downscaled width decode to the encoder's reconstruction, all
    /// three planes, for key and inter frames at each denominator and at an odd width.
    #[test]
    fn superres_frames_decode_to_the_encoders_reconstruction() {
        for &(denom, restoration) in &[(9u32, false), (12, false), (16, false), (9, true), (12, true), (16, true)] {
            for &(w, h) in &[(352usize, 288usize), (177, 99)] {
                let mut enc = Av1Encoder::new(&params(w, h), Some(24)).unwrap().with_keyint(3).with_superres(denom);
                if restoration {
                    enc = enc.with_restoration();
                }
                let mut dec = Av1Decoder::new(&[]);
                for t in 0..4 {
                    let src = picture_shifted(w, h, 2 * t);
                    let data = enc.encode_frame(&src).unwrap();
                    let recon = enc.last.as_ref().unwrap().0.clone();
                    let mut frames = dec.decode(&Packet { stream: 0, pts: t as i64, duration: 1, data }).unwrap();
                    assert_eq!(frames.len(), 1, "frame {t} shows one picture");
                    let CoreFrame::Video(out) = frames.remove(0) else { panic!("no picture") };
                    assert_eq!((out.width as usize, out.height as usize), (w, h), "the output is the upscaled size");
                    for pl in 0..3 {
                        let (pw, ph) = if pl == 0 { (w, h) } else { (w.div_ceil(2), h.div_ceil(2)) };
                        let want: Vec<u8> = (0..pw * ph).map(|i| recon.planes[pl].at(i % pw, i / pw) as u8).collect();
                        assert!(out.planes[pl] == want, "1/{denom} {w}x{h} frame {t} plane {pl}: decoder differs from the encoder's reconstruction");
                    }
                    let p = psnr(&src.planes[0], &out.planes[0]);
                    assert!(p > 20.0, "1/{denom} {w}x{h} frame {t}: luma PSNR {p:.1} dB");
                }
            }
        }
    }
}
