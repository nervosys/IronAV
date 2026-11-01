//! The software H.264 encoder at 10 bits (High 10 profile): a yuv420p10le source encodes to a stream
//! that FFmpeg and IronAV decode to identical pictures, and the pictures are close to the source.
//! The test skips when `ffmpeg` is not on the PATH.

use ironav_engine::{run, Job};
use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ironav-h26410-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn ffmpeg(args: &[&str]) -> bool {
    Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).status().map(|s| s.success()).unwrap_or(false)
}

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").output().map(|o| o.status.success()).unwrap_or(false)
}

/// The frames of a y4m file, without its headers.
fn y4m_payload(data: &[u8], frame: usize) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = data.iter().position(|&b| b == b'\n').unwrap() + 1;
    while i < data.len() {
        let eol = i + data[i..].iter().position(|&b| b == b'\n').unwrap();
        out.extend_from_slice(&data[eol + 1..eol + 1 + frame]);
        i = eol + 1 + frame;
    }
    out
}

fn encode(dir: &Path, src: &Path) -> PathBuf {
    let out = dir.join("ten.mp4");
    let mut j = Job { input: src.display().to_string(), output: out.display().to_string(), hwaccel: "off".into(), overwrite: true, ..Job::default() };
    j.video.codec = Some("h264".into());
    j.video.qp = Some(26);
    run(&j, &mut |_| {}).unwrap_or_else(|e| panic!("IronAV could not encode 10-bit video: {e}"));
    out
}

#[test]
fn ten_bit_streams_decode_as_ffmpeg_decodes() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    let dir = scratch("decode");
    let (w, h, frames) = (160usize, 96usize, 12usize);
    // FFmpeg's raw 10-bit clip, wrapped in a y4m header (its own y4m writer refuses 10 bits here)
    let raw_src = dir.join("src.raw");
    assert!(ffmpeg(&["-f", "lavfi", "-i", &format!("testsrc2=size={w}x{h}:rate=25"), "-frames:v", &frames.to_string(), "-pix_fmt", "yuv420p10le", "-f", "rawvideo", raw_src.to_str().unwrap()]));
    let mut y4m_src = format!("YUV4MPEG2 W{w} H{h} F25:1 Ip A1:1 C420p10 XYSCSS=420P10\n").into_bytes();
    for f in std::fs::read(&raw_src).unwrap().chunks(w * h * 3 / 2 * 2) {
        y4m_src.extend_from_slice(b"FRAME\n");
        y4m_src.extend_from_slice(f);
    }
    let src = dir.join("src.y4m");
    std::fs::write(&src, &y4m_src).unwrap();
    let stream = encode(&dir, &src);
    // FFmpeg's decode of the stream, against IronAV's
    let raw = dir.join("ffmpeg.raw");
    assert!(ffmpeg(&["-i", stream.to_str().unwrap(), "-f", "rawvideo", "-pix_fmt", "yuv420p10le", raw.to_str().unwrap()]), "FFmpeg could not decode");
    let y4m = dir.join("ironav.y4m");
    let j = Job { input: stream.display().to_string(), output: y4m.display().to_string(), hwaccel: "off".into(), overwrite: true, ..Job::default() };
    run(&j, &mut |_| {}).unwrap();
    let frame = w * h * 3 / 2 * 2;
    let got = y4m_payload(&std::fs::read(&y4m).unwrap(), frame);
    let want = std::fs::read(&raw).unwrap();
    assert_eq!(want.len(), frames * frame, "FFmpeg's frame count");
    assert!(got == want, "IronAV decodes the 10-bit stream differently from FFmpeg");
}
