//! `refs` in a job: the software H.264 encoder's P pictures predict from that many reference pictures.
//! The stream decodes with IronAV and with FFmpeg to identical pictures, the setting changes the
//! stream, and values outside 1 to 16 are refused. The tests skip when `ffmpeg` is not on the PATH.

use ironav_engine::{run, Job};
use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ironav-h264refs-{}-{name}", std::process::id()));
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

/// Encodes `src` to H.264 in an MP4 with `refs` reference pictures and returns the path.
fn encode(dir: &Path, src: &Path, refs: Option<u32>) -> PathBuf {
    let out = dir.join(format!("refs{}.mp4", refs.unwrap_or(0)));
    let mut j = Job { input: src.display().to_string(), output: out.display().to_string(), hwaccel: "off".into(), overwrite: true, ..Job::default() };
    j.video.codec = Some("h264".into());
    j.video.qp = Some(26);
    j.video.refs = refs;
    run(&j, &mut |_| {}).unwrap_or_else(|e| panic!("refs {refs:?}: IronAV could not encode: {e}"));
    out
}

#[test]
fn several_references_decode_as_ffmpeg_decodes() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    let dir = scratch("decode");
    let (w, h, frames) = (160usize, 96usize, 12usize);
    let src = dir.join("src.y4m");
    assert!(ffmpeg(&["-f", "lavfi", "-i", &format!("testsrc2=size={w}x{h}:rate=25"), "-frames:v", &frames.to_string(), "-pix_fmt", "yuv420p", src.to_str().unwrap()]));
    let mut sizes = Vec::new();
    for refs in [Some(1), Some(3)] {
        let stream = encode(&dir, &src, refs);
        sizes.push(std::fs::metadata(&stream).unwrap().len());
        // FFmpeg's decode of the stream, against IronAV's
        let raw = dir.join("ffmpeg.raw");
        assert!(ffmpeg(&["-i", stream.to_str().unwrap(), "-f", "rawvideo", "-pix_fmt", "yuv420p", raw.to_str().unwrap()]), "FFmpeg could not decode");
        let y4m = dir.join("ironav.y4m");
        let j = Job { input: stream.display().to_string(), output: y4m.display().to_string(), hwaccel: "off".into(), overwrite: true, ..Job::default() };
        run(&j, &mut |_| {}).unwrap();
        let frame = w * h * 3 / 2;
        let got = y4m_payload(&std::fs::read(&y4m).unwrap(), frame);
        let want = std::fs::read(&raw).unwrap();
        assert_eq!(got.len(), frames * frame, "the frame count");
        assert!(got == want, "IronAV decodes the stream differently from FFmpeg (refs {refs:?})");
    }
    assert_ne!(sizes[0], sizes[1], "three references change the stream");
}

#[test]
fn refs_outside_one_to_sixteen_are_refused() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    let dir = scratch("refused");
    let src = dir.join("src.y4m");
    assert!(ffmpeg(&["-f", "lavfi", "-i", "testsrc2=size=64x48:rate=25", "-frames:v", "2", "-pix_fmt", "yuv420p", src.to_str().unwrap()]));
    for refs in [0, 17] {
        let mut j = Job { input: src.display().to_string(), output: dir.join("x.mp4").display().to_string(), hwaccel: "off".into(), overwrite: true, ..Job::default() };
        j.video.codec = Some("h264".into());
        j.video.refs = Some(refs);
        let err = run(&j, &mut |_| {}).expect_err("refused");
        assert!(err.to_string().contains("refs"), "refs {refs} is refused by name: {err}");
    }
}
