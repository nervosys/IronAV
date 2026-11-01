//! A software AV1 encoder, stage 1: key frames only (every frame is intra and shown), 8-bit 4:2:0,
//! profile 0, one tile.
//!
//! Each frame is a temporal unit: a temporal delimiter, the sequence header, and a frame OBU with
//! the key frame's header and its tile. Superblocks are 64x64, split by an open-loop search on the
//! source down to 8x8 (never larger than 32x32, so every block is coded with one transform of its
//! size). Blocks take the best of the 13 intra modes with angle deltas, chosen on the reconstruction,
//! and the chroma modes likewise. Coefficients are quantised at the base quantiser index that
//! `qp` maps to (as for the other encoders, 0 to 51). Deblocking, CDEF and loop restoration are off.
//! The encoder predicts and reconstructs with the decoder's own intra predictor and inverse
//! transform, so the pictures it keeps are the decoder's pictures.
//!
//! The range coder (`rc.rs`) is adapted from rav1e's `src/ec.rs`, which is BSD-2-Clause licensed
//! (the rav1e contributors); rav1e is not a dependency.

mod fwd;
mod headers;
mod rc;
mod tile;

pub use headers::config_record;

use ironav_core::{Encoder, Error, ErrorCode, Frame, Packet, PixelFormat, Result, StreamParams, VideoFrame};

/// The quantiser when none is set (the VP9 encoder's default).
const DEFAULT_QP: u8 = 23;

/// The largest picture one tile holds: 4096 wide, and no more than 2304 superblocks, so that the
/// minimum tile counts stay zero.
const MAX_WIDTH: u32 = 4096;
const MAX_SUPERBLOCKS: usize = 2304;

/// The base quantiser index of a quantiser of 0 to 51 (as the VP9 encoder maps it). Index 0 would be
/// lossless, which this encoder does not code.
fn base_q(qp: u8) -> u32 {
    (qp.min(51) as u32 * 255 / 51).max(1)
}

pub struct Av1Encoder {
    width: usize,
    height: usize,
    base_q: u32,
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
        Ok(Self { width: width as usize, height: height as usize, base_q: base_q(qp.unwrap_or(DEFAULT_QP)) })
    }

    /// The temporal unit of one picture.
    pub fn encode_frame(&self, v: &VideoFrame) -> Result<Vec<u8>> {
        let (w, h) = (self.width, self.height);
        if v.pix_fmt != PixelFormat::Yuv420p || v.width as usize != w || v.height as usize != h || v.planes.len() < 3 {
            return Err(Error::invalid_input(format!("the AV1 encoder was set up for {w}x{h} yuv420p frames")));
        }
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        if v.planes[0].len() < w * h || v.planes[1].len() < cw * ch || v.planes[2].len() < cw * ch {
            return Err(Error::invalid_input("a video frame plane is shorter than its dimensions"));
        }
        let (mi_cols, mi_rows) = (2 * w.div_ceil(8), 2 * h.div_ceil(8));
        let fe = tile::FrameEnc::new(mi_cols, mi_rows, self.base_q as usize, &v.planes[0], &v.planes[1], &v.planes[2], (w, h));
        let (tile_bytes, _recon) = fe.encode();
        let mut frame = headers::key_frame_header(w as u32, h as u32, self.base_q);
        frame.extend_from_slice(&tile_bytes);

        let mut out = headers::obu(headers::OBU_TEMPORAL_DELIMITER, &[]);
        out.extend(headers::obu(headers::OBU_SEQUENCE_HEADER, &headers::sequence_header(w as u32, h as u32)));
        out.extend(headers::obu(headers::OBU_FRAME, &frame));
        Ok(out)
    }
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
    fn picture(w: usize, h: usize) -> VideoFrame {
        let mut seed = 1u32;
        let mut noise = move || {
            seed = seed.wrapping_mul(1103515245).wrapping_add(12345);
            ((seed >> 16) % 9) as i32 - 4
        };
        let luma: Vec<u8> = (0..w * h)
            .map(|i| {
                let (x, y) = (i % w, i / w);
                let v = if x + y > (w + h) / 2 { 200 } else { (x * 2 + y) as i32 } + noise();
                v.clamp(0, 255) as u8
            })
            .collect();
        let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
        let chroma = |k: usize| -> Vec<u8> { (0..cw * ch).map(|i| ((i % cw) * 3 + (i / cw) * k) as u8).collect() };
        VideoFrame { width: w as u32, height: h as u32, pix_fmt: PixelFormat::Yuv420p, pts: 0, planes: vec![luma, chroma(1), chroma(2)] }
    }

    fn psnr(a: &[u8], b: &[u8]) -> f64 {
        let mse: f64 = a.iter().zip(b).map(|(&x, &y)| (x as f64 - y as f64).powi(2)).sum::<f64>() / a.len() as f64;
        10.0 * (255.0 * 255.0 / mse.max(1e-9)).log10()
    }

    /// A picture through the encoder and IronAV's own decoder comes back close to the source, at
    /// sizes with and without a partial superblock.
    #[test]
    fn round_trips_through_the_decoder() {
        for &(w, h) in &[(64usize, 48usize), (177, 99)] {
            let src = picture(w, h);
            let frame_rate = ironav_core::Rational { num: 25, den: 1 };
            let params = StreamParams::Video { width: w as u32, height: h as u32, pix_fmt: PixelFormat::Yuv420p, frame_rate };
            let enc = Av1Encoder::new(&params, Some(20)).unwrap();
            let data = enc.encode_frame(&src).unwrap();
            let mut dec = Av1Decoder::new(&[]);
            let mut frames = dec.decode(&Packet { stream: 0, pts: 0, duration: 1, data }).unwrap();
            frames.extend(dec.flush().unwrap());
            let CoreFrame::Video(out) = frames.remove(0) else { panic!("no picture") };
            let p = psnr(&src.planes[0], &out.planes[0]);
            assert!(p > 35.0, "{w}x{h}: luma PSNR {p:.1} dB");
        }
    }
}
