//! The `format` video filter: convert between the supported pixel formats (gray8, yuv420p,
//! yuv422p, yuv444p, rgb24, and the same YUV layouts at 10 and 12 bits).
//!
//! Everything goes through full-resolution Y/U/V planes. Chroma is resampled with the same
//! triangle filter as `scale`, and RGB conversion uses limited-range ("TV") BT.601 or BT.709
//! in 8-bit fixed point. Gray8 means luma only: converting it to a colour format gives neutral
//! chroma, and converting colour to gray8 keeps the luma.

use crate::scale::resize_plane;
use ironav_core::{Filter, Frame, PixelFormat, Result, StreamParams, VideoFrame};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Matrix {
    Bt601,
    Bt709,
}

pub struct Format {
    pub(crate) to: PixelFormat,
    pub(crate) matrix: Matrix,
}

impl Filter for Format {
    fn output_params(&self, input: &StreamParams) -> StreamParams {
        match input {
            StreamParams::Video { width, height, frame_rate, .. } => {
                StreamParams::Video { width: *width, height: *height, pix_fmt: self.to, frame_rate: *frame_rate }
            }
            other => other.clone(),
        }
    }

    fn process(&mut self, frame: Frame) -> Result<Vec<Frame>> {
        let Frame::Video(v) = frame else { return Ok(vec![frame]) };
        if v.pix_fmt == self.to {
            return Ok(vec![Frame::Video(v)]);
        }
        Ok(vec![Frame::Video(convert(v, self.to, self.matrix))])
    }
}

fn clip(v: i32) -> u8 {
    v.clamp(0, 255) as u8
}

/// Full-resolution luma and chroma planes of any supported frame.
fn to_yuv444(v: &VideoFrame, m: Matrix) -> [Vec<u8>; 3] {
    let (w, h) = (v.width as usize, v.height as usize);
    let layout = v.pix_fmt.planes(w, h);
    match v.pix_fmt {
        PixelFormat::Gray8 => [v.planes[0].clone(), vec![128; w * h], vec![128; w * h]],
        PixelFormat::Yuv444p => [v.planes[0].clone(), v.planes[1].clone(), v.planes[2].clone()],
        PixelFormat::Yuv420p10 | PixelFormat::Yuv422p10 | PixelFormat::Yuv444p10 | PixelFormat::Yuv420p12 | PixelFormat::Yuv422p12 | PixelFormat::Yuv444p12 => {
            unreachable!("deep frames are narrowed before conversion")
        }
        PixelFormat::Yuv420p | PixelFormat::Yuv422p => {
            let (cw, ch, _) = layout[1];
            [v.planes[0].clone(), resize_plane(&v.planes[1], cw, ch, w, h, 1), resize_plane(&v.planes[2], cw, ch, w, h, 1)]
        }
        PixelFormat::Rgb24 => {
            let (mut y, mut u, mut vv) = (vec![0u8; w * h], vec![0u8; w * h], vec![0u8; w * h]);
            for (i, p) in v.planes[0].as_chunks::<3>().0.iter().enumerate() {
                let (r, g, b) = (p[0] as i32, p[1] as i32, p[2] as i32);
                let (yy, uu, vv_) = match m {
                    Matrix::Bt601 => (66 * r + 129 * g + 25 * b, -38 * r - 74 * g + 112 * b, 112 * r - 94 * g - 18 * b),
                    Matrix::Bt709 => (47 * r + 157 * g + 16 * b, -26 * r - 87 * g + 112 * b, 112 * r - 102 * g - 10 * b),
                };
                y[i] = clip(16 + ((yy + 128) >> 8));
                u[i] = clip(128 + ((uu + 128) >> 8));
                vv[i] = clip(128 + ((vv_ + 128) >> 8));
            }
            [y, u, vv]
        }
    }
}

/// The same layout at `depth` bits (8, 10 or 12): fewer bits by rounding, more by replicating the
/// top bits, so that the largest value stays the largest (255 becomes 1023 or 4095).
fn redepth(v: VideoFrame, depth: u8) -> VideoFrame {
    let from = v.pix_fmt.bit_depth() as u32;
    let to = depth as u32;
    let pix_fmt = v.pix_fmt.with_depth(depth).expect("a planar YUV layout at 8, 10 or 12 bits");
    let get: fn(&[u8]) -> Vec<u32> = if from == 8 { |p| p.iter().map(|&b| b as u32).collect() } else { |p| p.as_chunks::<2>().0.iter().map(|w| u16::from_le_bytes([w[0], w[1]]) as u32).collect() };
    let max = (1u32 << to) - 1;
    let planes = v
        .planes
        .iter()
        .map(|p| {
            let samples = get(p).into_iter().map(|x| if to > from { (x << (to - from)) | (x >> (2 * from - to)) } else { ((x + (1 << (from - to) >> 1)) >> (from - to)).min(max) });
            if to == 8 { samples.map(|x| x as u8).collect() } else { samples.flat_map(|x| (x as u16).to_le_bytes()).collect() }
        })
        .collect();
    VideoFrame { pix_fmt, planes, ..v }
}

/// Convert a frame to another pixel format. A change of depth alone keeps the layout and loses no
/// more than the bits it drops; anything else goes through 8 bits, so a deep frame loses its extra
/// precision.
pub fn convert(v: VideoFrame, to: PixelFormat, m: Matrix) -> VideoFrame {
    if v.pix_fmt == to {
        return v;
    }
    if v.pix_fmt.with_depth(to.bit_depth()) == Some(to) {
        return redepth(v, to.bit_depth());
    }
    if v.pix_fmt.bit_depth() > 8 {
        return convert(redepth(v, 8), to, m);
    }
    if to.bit_depth() > 8 {
        let eight = to.with_depth(8).expect("deep formats are planar YUV");
        return redepth(convert8(v, eight, m), to.bit_depth());
    }
    convert8(v, to, m)
}

fn convert8(v: VideoFrame, to: PixelFormat, m: Matrix) -> VideoFrame {
    let (w, h) = (v.width as usize, v.height as usize);
    let [y, u, vv] = to_yuv444(&v, m);
    let planes = match to {
        PixelFormat::Gray8 => vec![y],
        PixelFormat::Yuv444p => vec![y, u, vv],
        PixelFormat::Yuv420p10 | PixelFormat::Yuv422p10 | PixelFormat::Yuv444p10 | PixelFormat::Yuv420p12 | PixelFormat::Yuv422p12 | PixelFormat::Yuv444p12 => {
            unreachable!("deep output is widened from 8-bit")
        }
        PixelFormat::Yuv420p | PixelFormat::Yuv422p => {
            let (cw, ch, _) = to.planes(w, h)[1];
            vec![y, resize_plane(&u, w, h, cw, ch, 1), resize_plane(&vv, w, h, cw, ch, 1)]
        }
        PixelFormat::Rgb24 => {
            let mut rgb = vec![0u8; w * h * 3];
            for i in 0..w * h {
                let (c, d, e) = (298 * (y[i] as i32 - 16), u[i] as i32 - 128, vv[i] as i32 - 128);
                let (r, g, b) = match m {
                    Matrix::Bt601 => (c + 409 * e, c - 100 * d - 208 * e, c + 516 * d),
                    Matrix::Bt709 => (c + 459 * e, c - 55 * d - 136 * e, c + 541 * d),
                };
                rgb[i * 3] = clip((r + 128) >> 8);
                rgb[i * 3 + 1] = clip((g + 128) >> 8);
                rgb[i * 3 + 2] = clip((b + 128) >> 8);
            }
            vec![rgb]
        }
    };
    VideoFrame { width: v.width, height: v.height, pix_fmt: to, pts: v.pts, planes }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rgb_frame(w: u32, h: u32, f: impl Fn(usize, usize) -> [u8; 3]) -> VideoFrame {
        let mut px = Vec::new();
        for y in 0..h as usize {
            for x in 0..w as usize {
                px.extend_from_slice(&f(x, y));
            }
        }
        let planes = vec![px];
        VideoFrame { width: w, height: h, pix_fmt: PixelFormat::Rgb24, pts: 3, planes }
    }

    #[test]
    fn known_colours_land_on_the_bt601_limited_range_values() {
        // White, black and pure red in studio-range BT.601: (235,128,128), (16,128,128), (82,90,240).
        let f = rgb_frame(3, 1, |x, _| [[255, 255, 255], [0, 0, 0], [255, 0, 0]][x]);
        let o = convert(f, PixelFormat::Yuv444p, Matrix::Bt601);
        let px = |i: usize| (o.planes[0][i] as i32, o.planes[1][i] as i32, o.planes[2][i] as i32);
        assert_eq!(px(0), (235, 128, 128));
        assert_eq!(px(1), (16, 128, 128));
        let (y, u, v) = px(2);
        assert!((y - 82).abs() <= 1 && (u - 90).abs() <= 1 && (v - 240).abs() <= 1, "{:?}", (y, u, v));
    }

    #[test]
    fn rgb_survives_a_round_trip_through_yuv444_within_rounding() {
        let f = rgb_frame(16, 16, |x, y| [(x * 16) as u8, (y * 16) as u8, ((x + y) * 8) as u8]);
        for m in [Matrix::Bt601, Matrix::Bt709] {
            let back = convert(convert(f.clone(), PixelFormat::Yuv444p, m), PixelFormat::Rgb24, m);
            // Studio range has only 220 luma levels, so a couple of codes of error is expected.
            let worst = f.planes[0].iter().zip(&back.planes[0]).map(|(a, b)| (*a as i32 - *b as i32).abs()).max().unwrap();
            assert!(worst <= 4, "{m:?}: worst error {worst}");
        }
    }

    #[test]
    fn subsampling_changes_only_the_chroma_planes() {
        let f = rgb_frame(8, 6, |x, y| [(x * 30) as u8, (y * 40) as u8, 100]);
        let full = convert(f.clone(), PixelFormat::Yuv444p, Matrix::Bt601);
        for to in [PixelFormat::Yuv420p, PixelFormat::Yuv422p] {
            let o = convert(f.clone(), to, Matrix::Bt601);
            assert_eq!(o.planes[0], full.planes[0], "luma is untouched");
            let want: Vec<usize> = to.planes(8, 6).iter().map(|&(w, h, b)| w * h * b).collect();
            assert_eq!(o.planes.iter().map(Vec::len).collect::<Vec<_>>(), want);
        }
    }

    #[test]
    fn flat_chroma_stays_flat_through_420() {
        let f = VideoFrame { width: 10, height: 6, pix_fmt: PixelFormat::Yuv444p, pts: 0, planes: vec![vec![90; 60], vec![100; 60], vec![150; 60]] };
        let o = convert(convert(f, PixelFormat::Yuv420p, Matrix::Bt601), PixelFormat::Yuv444p, Matrix::Bt601);
        assert!(o.planes[1].iter().all(|&x| x == 100) && o.planes[2].iter().all(|&x| x == 150));
    }

    #[test]
    fn gray_gets_neutral_chroma_and_colour_to_gray_keeps_luma() {
        let g = VideoFrame { width: 4, height: 2, pix_fmt: PixelFormat::Gray8, pts: 0, planes: vec![(0..8).map(|i| 20 + i * 10).collect()] };
        let c = convert(g.clone(), PixelFormat::Yuv420p, Matrix::Bt601);
        assert_eq!(c.planes[0], g.planes[0]);
        assert!(c.planes[1].iter().chain(&c.planes[2]).all(|&x| x == 128));
        assert_eq!(convert(c, PixelFormat::Gray8, Matrix::Bt601).planes[0], g.planes[0]);
    }

    #[test]
    fn ten_bit_converts_to_eight_and_back_without_losing_the_top_bits() {
        let w = 4;
        let samples: Vec<u16> = (0..w * 2).map(|i| (i * 146).min(1023) as u16).collect(); // 0..=1022 in steps
        let bytes = |v: &[u16]| v.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>();
        let f = VideoFrame {
            width: w as u32,
            height: 2,
            pix_fmt: PixelFormat::Yuv420p10,
            pts: 5,
            planes: vec![bytes(&samples), bytes(&[512, 512]), bytes(&[512, 512])],
        };
        let eight = convert(f.clone(), PixelFormat::Yuv420p, Matrix::Bt709);
        assert_eq!(eight.pix_fmt, PixelFormat::Yuv420p);
        assert_eq!(eight.planes[0].len(), 8);
        assert_eq!(eight.planes[0][0], 0);
        assert_eq!(eight.planes[0][7], ((samples[7] as u32 + 2) >> 2).min(255) as u8);
        let back = convert(eight, PixelFormat::Yuv420p10, Matrix::Bt709);
        assert_eq!(back.pix_fmt, PixelFormat::Yuv420p10);
        assert_eq!(back.planes[0].len(), 16);
        // Going 10 -> 8 -> 10 keeps every sample within the 8-bit step (4) of the original.
        for (i, orig) in samples.iter().enumerate() {
            let got = u16::from_le_bytes([back.planes[0][i * 2], back.planes[0][i * 2 + 1]]);
            assert!(got.abs_diff(*orig) <= 4, "sample {i}: {orig} -> {got}");
        }
        // Full scale stays full scale.
        let white = VideoFrame { planes: vec![bytes(&[1023; 8]), bytes(&[1023; 2]), bytes(&[1023; 2])], ..f };
        let w8 = convert(white, PixelFormat::Yuv420p, Matrix::Bt709);
        assert_eq!(w8.planes[0], vec![255; 8]);
        let w10 = convert(w8, PixelFormat::Yuv420p10, Matrix::Bt709);
        assert_eq!(&w10.planes[0][..2], &1023u16.to_le_bytes());
    }

    #[test]
    fn twelve_bit_changes_depth_and_keeps_its_layout() {
        let bytes = |v: &[u16]| v.iter().flat_map(|x| x.to_le_bytes()).collect::<Vec<u8>>();
        let words = |b: &[u8]| b.as_chunks::<2>().0.iter().map(|w| u16::from_le_bytes([w[0], w[1]])).collect::<Vec<u16>>();
        let luma = [0u16, 1, 2, 2047, 2048, 4093, 4094, 4095];
        let f = VideoFrame { width: 4, height: 2, pix_fmt: PixelFormat::Yuv422p12, pts: 0, planes: vec![bytes(&luma), bytes(&[2048; 4]), bytes(&[100, 4095, 0, 1])] };
        // to 10 bits in the same layout: rounded, never past the top
        let ten = convert(f.clone(), PixelFormat::Yuv422p10, Matrix::Bt709);
        assert_eq!(ten.pix_fmt, PixelFormat::Yuv422p10);
        assert_eq!(words(&ten.planes[0]), [0, 0, 1, 512, 512, 1023, 1023, 1023]);
        assert_eq!(words(&ten.planes[2]), [25, 1023, 0, 0]);
        // and back: the top bits are replicated, so full scale stays full scale, and a round trip
        // moves a sample by one step at most
        let back = convert(ten.clone(), PixelFormat::Yuv422p12, Matrix::Bt709);
        assert_eq!(words(&back.planes[0])[7], 4095);
        let again = convert(back, PixelFormat::Yuv422p10, Matrix::Bt709);
        for (a, b) in words(&again.planes[0]).iter().zip(words(&ten.planes[0])) {
            assert!(a.abs_diff(b) <= 1, "{a} {b}");
        }
        // 8 bits: 255 is 4095
        let eight = convert(f, PixelFormat::Yuv422p, Matrix::Bt709);
        assert_eq!(eight.planes[0], [0, 0, 0, 128, 128, 255, 255, 255]);
        assert_eq!(words(&convert(eight.clone(), PixelFormat::Yuv422p12, Matrix::Bt709).planes[0])[7], 4095);
        // another layout goes through 8 bits
        let c420 = convert(eight, PixelFormat::Yuv420p12, Matrix::Bt709);
        assert_eq!((c420.pix_fmt, c420.planes[1].len()), (PixelFormat::Yuv420p12, 4));
    }
}
