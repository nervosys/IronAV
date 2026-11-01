//! VP9 at 10 and 12 bits, in 4:4:0, and at odd sizes: the software decoder's pictures are bit-exact
//! with FFmpeg's, and come out in the matching pixel format. The decoder is driven directly, since
//! the y4m writer has no 4:4:0 layout to write them to.
//!
//! The streams were made with FFmpeg's libvpx-vp9 encoder, e.g.
//! `ffmpeg -i src.y4m -pix_fmt yuv440p12le -c:v libvpx-vp9 -profile:v 3 -b:v 200k -auto-alt-ref 0 vp9_h12_440.webm`,
//! and their references with `ffmpeg -i vp9_h12_440.webm -f rawvideo -pix_fmt yuv440p12le vp9_h12_440.ffmpeg.yuv`.

use ironav_core::{Frame, PixelFormat};
use std::path::Path;

/// Every video frame the decoder makes from `file`, as its planes concatenated, and the pixel
/// formats seen.
fn decode(file: &str) -> (Vec<u8>, Vec<(u32, u32, PixelFormat)>) {
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let mut d = ironav_formats::open_demuxer(&data.join(format!("{file}.webm"))).unwrap();
    let info = d.streams()[0].clone();
    let mut dec = ironav_codec::new_decoder_for(&info).unwrap();
    let (mut out, mut shapes) = (Vec::new(), Vec::new());
    let mut take = |frames: Vec<Frame>| {
        for f in frames {
            if let Frame::Video(v) = f {
                shapes.push((v.width, v.height, v.pix_fmt));
                for p in &v.planes {
                    out.extend_from_slice(p);
                }
            }
        }
    };
    while let Some(p) = d.read_packet().unwrap() {
        take(dec.decode(&p).unwrap());
    }
    take(dec.flush().unwrap());
    (out, shapes)
}

/// Checks that `file` decodes to FFmpeg's pictures, at `w` x `h` in `pix_fmt`, for every frame.
fn matches_ffmpeg(file: &str, w: u32, h: u32, pix_fmt: PixelFormat) {
    let (got, shapes) = decode(file);
    assert!(!shapes.is_empty(), "{file}: no frames");
    assert!(shapes.iter().all(|&s| s == (w, h, pix_fmt)), "{file}: {shapes:?}");
    let data = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data");
    let want = std::fs::read(data.join(format!("{file}.ffmpeg.yuv"))).unwrap();
    assert_eq!(got.len(), want.len(), "{file}: size");
    let first = got.iter().zip(&want).position(|(a, b)| a != b);
    assert!(first.is_none(), "{file}: pixels differ from FFmpeg's, first at byte {first:?}");
}

#[test]
fn vp9_10_bit_420_matches_ffmpeg() {
    matches_ffmpeg("vp9_h10", 176, 144, PixelFormat::Yuv420p10);
}

#[test]
fn vp9_12_bit_420_matches_ffmpeg() {
    matches_ffmpeg("vp9_h12", 176, 144, PixelFormat::Yuv420p12);
}

#[test]
fn vp9_12_bit_444_matches_ffmpeg() {
    matches_ffmpeg("vp9_h12_444", 176, 144, PixelFormat::Yuv444p12);
}

#[test]
fn vp9_8_bit_440_matches_ffmpeg() {
    matches_ffmpeg("vp9_440", 176, 144, PixelFormat::Yuv440p);
}

#[test]
fn vp9_10_bit_440_matches_ffmpeg() {
    matches_ffmpeg("vp9_h10_440", 176, 144, PixelFormat::Yuv440p10);
}

#[test]
fn vp9_12_bit_440_matches_ffmpeg() {
    matches_ffmpeg("vp9_h12_440", 176, 144, PixelFormat::Yuv440p12);
}

#[test]
fn vp9_odd_size_12_bit_420_matches_ffmpeg() {
    matches_ffmpeg("vp9_odd_h12", 173, 141, PixelFormat::Yuv420p12);
}

#[test]
fn vp9_odd_size_8_bit_440_matches_ffmpeg() {
    matches_ffmpeg("vp9_odd_440", 173, 141, PixelFormat::Yuv440p);
}

#[test]
fn vp9_odd_size_12_bit_440_matches_ffmpeg() {
    matches_ffmpeg("vp9_odd_h12_440", 173, 141, PixelFormat::Yuv440p12);
}
