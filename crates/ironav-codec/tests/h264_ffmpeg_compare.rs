//! The software H.264 decoder against FFmpeg: streams made by libx264 with different coding tools
//! (CAVLC and CABAC, the 8x8 transform, B pictures, explicit and implicit weighted prediction,
//! MBAFF, 4:2:2 and 4:4:4, 10 bits) are decoded by both, and the pictures must match byte for byte,
//! whether the decoder works on one thread or on several.
//!
//! The test needs `ffmpeg` (with libx264) on the PATH and does nothing without it; a stream the
//! local build cannot encode (for instance 10-bit) is skipped. Files go to a temporary directory.

use ironav_codec::h264::H264Decoder;
use ironav_core::{Decoder, Frame, Packet};
use std::path::Path;
use std::process::Command;

fn ffmpeg(args: &[&str]) -> bool {
    Command::new("ffmpeg").args(["-v", "error", "-y", "-nostdin"]).args(args).output().map(|o| o.status.success()).unwrap_or(false)
}

/// Our decode of an Annex B file, all planes of all pictures in output order.
fn decode(data: &[u8], threads: usize) -> Vec<u8> {
    let mut d = H264Decoder::with_threads(threads);
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

fn first_difference(a: &[u8], b: &[u8]) -> Option<usize> {
    a.iter().zip(b).position(|(x, y)| x != y)
}

fn run_case(dir: &Path, name: &str, source: &str, codec_args: &[&str]) {
    let stream = dir.join(format!("{name}.h264"));
    let reference = dir.join(format!("{name}.yuv"));
    let (stream_s, reference_s) = (stream.to_str().unwrap(), reference.to_str().unwrap());
    let mut args = vec!["-f", "lavfi", "-i", source, "-frames:v", "30", "-c:v", "libx264"];
    args.extend_from_slice(codec_args);
    args.extend_from_slice(&["-f", "h264", stream_s]);
    if !ffmpeg(&args) {
        eprintln!("{name}: this FFmpeg cannot make the stream, skipped");
        return;
    }
    assert!(ffmpeg(&["-i", stream_s, "-f", "rawvideo", reference_s]), "{name}: FFmpeg cannot decode its own stream");
    let want = std::fs::read(&reference).unwrap();
    let data = std::fs::read(&stream).unwrap();
    for threads in [1, 4] {
        let got = decode(&data, threads);
        assert_eq!(got.len(), want.len(), "{name}, {threads} threads: the output is not the reference's size");
        if let Some(at) = first_difference(&got, &want) {
            panic!("{name}, {threads} threads: the output differs from FFmpeg's at byte {at}");
        }
    }
    eprintln!("{name}: identical to FFmpeg ({} bytes of pictures)", want.len());
}

#[test]
fn matches_ffmpeg_on_streams_with_different_tools() {
    if Command::new("ffmpeg").arg("-version").output().is_err() {
        eprintln!("no ffmpeg on the PATH, skipped");
        return;
    }
    let dir = std::env::temp_dir().join(format!("ironav-h264-compare-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let plain = "testsrc2=size=320x192:rate=30";
    // a fade makes weighted prediction worth using
    let fading = "testsrc2=size=320x192:rate=30,fade=t=in:st=0:d=0.5,fade=t=out:st=0.6:d=0.4";
    let cases: &[(&str, &str, &[&str])] = &[
        ("baseline_cavlc", plain, &["-profile:v", "baseline"]),
        ("main_cavlc_bframes", plain, &["-profile:v", "main", "-coder", "0", "-bf", "2"]),
        ("main_cabac_bframes", plain, &["-profile:v", "main", "-bf", "3"]),
        ("high_8x8", plain, &["-profile:v", "high", "-bf", "3", "-refs", "4", "-x264-params", "8x8dct=1"]),
        ("weighted", fading, &["-profile:v", "main", "-bf", "2", "-weightp", "2", "-x264-params", "weightb=1"]),
        ("mbaff", plain, &["-profile:v", "high", "-bf", "2", "-x264-params", "interlaced=1:tff=1"]),
        ("high422", plain, &["-pix_fmt", "yuv422p", "-profile:v", "high422", "-bf", "2"]),
        ("high444", plain, &["-pix_fmt", "yuv444p", "-profile:v", "high444", "-bf", "2"]),
        ("high10", plain, &["-pix_fmt", "yuv420p10le", "-profile:v", "high10", "-bf", "2"]),
    ];
    for (name, source, args) in cases {
        run_case(&dir, name, source, args);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
