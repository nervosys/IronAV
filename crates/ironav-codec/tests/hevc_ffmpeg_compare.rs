//! The software HEVC decoder against FFmpeg: streams made by libx265 with different coding tools (slices,
//! the loop filters on and off, weighted prediction, lossless coding, transform skip, scaling lists,
//! CTB sizes, 4:2:2 and 4:4:4, 10 bits) are decoded by both, and the pictures must match byte for byte,
//! whether the decoder works on one thread or on several.
//!
//! The test needs `ffmpeg` (with libx265) on the PATH and does nothing without it; a stream the local
//! build cannot encode is skipped. Files go to a temporary directory.

use ironav_codec::hevc::HevcDecoder;
use ironav_core::{Decoder, Frame, Packet};
use std::path::Path;
use std::process::Command;

fn ffmpeg(args: &[&str]) -> bool {
    Command::new("ffmpeg").args(["-v", "error", "-y", "-nostdin"]).args(args).output().map(|o| o.status.success()).unwrap_or(false)
}

/// Our decode of an Annex B file, all planes of all pictures in output order.
fn decode(data: &[u8], threads: usize) -> Vec<u8> {
    let mut d = HevcDecoder::with_threads(threads);
    let mut frames = d.decode(&Packet { stream: 0, pts: 0, duration: 0, data: data.to_vec() }).unwrap();
    frames.extend(d.flush().unwrap());
    let mut out = Vec::new();
    for f in frames {
        if let Frame::Video(v) = f {
            for p in &v.planes {
                out.extend_from_slice(p);
            }
        }
    }
    out
}

fn run_case(dir: &Path, name: &str, source: &str, pix_fmt: &str, x265: &str, frames: usize) {
    let stream = dir.join(format!("{name}.hevc"));
    let reference = dir.join(format!("{name}.yuv"));
    let (stream_s, reference_s) = (stream.to_str().unwrap(), reference.to_str().unwrap());
    let params = format!("log-level=none:{x265}");
    let frames = frames.to_string();
    let args = ["-f", "lavfi", "-i", source, "-frames:v", &frames, "-pix_fmt", pix_fmt, "-c:v", "libx265", "-x265-params", &params, "-f", "hevc", stream_s];
    if !ffmpeg(&args) {
        eprintln!("{name}: this FFmpeg cannot make the stream, skipped");
        return;
    }
    assert!(ffmpeg(&["-i", stream_s, "-pix_fmt", pix_fmt, "-f", "rawvideo", reference_s]), "{name}: FFmpeg cannot decode its own stream");
    let want = std::fs::read(&reference).unwrap();
    let data = std::fs::read(&stream).unwrap();
    for threads in [1, 2, 3, 4, 8] {
        let got = decode(&data, threads);
        assert_eq!(got.len(), want.len(), "{name}, {threads} threads: the output is not the reference's size");
        if let Some(at) = got.iter().zip(&want).position(|(a, b)| a != b) {
            let bad = got.iter().zip(&want).filter(|(a, b)| a != b).count();
            panic!("{name}, {threads} threads: the output differs from FFmpeg's at byte {at} ({bad} bytes differ)");
        }
    }
    eprintln!("{name}: identical to FFmpeg ({} bytes of pictures)", want.len());
}

#[test]
fn matches_ffmpeg_on_streams_with_different_tools() {
    if !Command::new("ffmpeg").args(["-hide_banner", "-encoders"]).output().is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("libx265")) {
        eprintln!("no ffmpeg with libx265 on the PATH, skipped");
        return;
    }
    let dir = std::env::temp_dir().join(format!("ironav-hevc-compare-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    // a size that is not a multiple of the CTB size, tall enough for the loop filters to split into bands
    let plain = "testsrc2=size=328x264:rate=30";
    // noise gives the transform and the loop filters real work
    let noisy = "testsrc2=size=328x264:rate=30,noise=alls=10:allf=t";
    let fading = "testsrc2=size=328x264:rate=30,fade=t=in:st=0:d=0.2,fade=t=out:st=0.2:d=0.2";
    let (p8, p10) = ("yuv420p", "yuv420p10le");
    let cases: &[(&str, &str, &str, &str)] = &[
        ("main_bframes", noisy, p8, "bframes=3:ref=3"),
        ("main_p_only", noisy, p8, "bframes=0:ref=2"),
        ("slices", noisy, p8, "slices=4:bframes=2"),
        ("no_wpp", noisy, p8, "no-wpp=1:bframes=2"),
        ("no_sao", noisy, p8, "no-sao=1:bframes=2"),
        ("no_deblock", noisy, p8, "no-deblock=1:bframes=2"),
        ("no_loop_filters", noisy, p8, "no-deblock=1:no-sao=1:bframes=2"),
        ("sao_band_and_edge", plain, p8, "limit-sao=0:selective-sao=0:bframes=2"),
        ("weighted", fading, p8, "weightp=1:weightb=1:bframes=3"),
        ("lossless", noisy, p8, "lossless=1:bframes=2"),
        ("transform_skip", noisy, p8, "tskip=1:tskip-fast=0:bframes=2"),
        ("scaling_list", noisy, p8, "scaling-list=default:bframes=2"),
        ("ctu32", noisy, p8, "ctu=32:bframes=2"),
        ("rect_and_amp", noisy, p8, "rect=1:amp=1:bframes=2"),
        ("main10", noisy, p10, "bframes=3"),
        ("main10_no_loop_filters", noisy, p10, "no-deblock=1:no-sao=1:bframes=2"),
        ("main422_10", noisy, "yuv422p10le", "bframes=2"),
        ("main444", noisy, "yuv444p", "bframes=2"),
        ("monochrome", noisy, "gray", "bframes=2"),
    ];
    for (name, source, pix_fmt, x265) in cases {
        run_case(&dir, name, source, pix_fmt, x265, 10);
    }
    // what frame threading depends on: a long GOP with B pictures in a pyramid (pictures that refer to
    // pictures decoded just before them, and to ones further back), and vertical motion of 20 to 30 rows
    // a picture (so a block waits for rows far below it in the reference)
    let scrolling = "testsrc2=size=328x264:rate=30,scroll=v=0.1,noise=alls=8:allf=t";
    let long: &[(&str, &str, &str, &str)] = &[
        ("pyramid_bframes8", noisy, p8, "bframes=8:b-pyramid=1:ref=4:keyint=40"),
        ("pyramid_bframes8_main10", noisy, p10, "bframes=8:b-pyramid=1:ref=3:keyint=40"),
        ("large_vertical_motion", scrolling, p8, "bframes=3:ref=3:merange=128:keyint=40"),
        ("large_vertical_motion_p", scrolling, p8, "bframes=0:ref=2:merange=128:keyint=40"),
        ("slices_pyramid", noisy, p8, "slices=3:bframes=5:b-pyramid=1:keyint=40"),
        ("temporal_mvp_off", noisy, p8, "no-temporal-mvp=1:bframes=4:keyint=40"),
    ];
    for (name, source, pix_fmt, x265) in long {
        run_case(&dir, name, source, pix_fmt, x265, 36);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
