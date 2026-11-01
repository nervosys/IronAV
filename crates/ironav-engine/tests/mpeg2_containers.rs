//! MPEG-2 video in MP4 and Matroska: the pixel format a container declares follows the stream's
//! chroma format, read from the sequence header and extension that FFmpeg writes into the track's
//! codec configuration (esds for MP4, CodecPrivate for Matroska). A 4:2:2 stream is declared 4:2:2
//! and decodes to FFmpeg's frames bit for bit; a 4:2:0 stream stays 4:2:0. Skipped without `ffmpeg`
//! on the PATH.

use ironav_core::{PixelFormat, StreamParams};
use ironav_engine::{run, Job};
use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch(tag: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ironav-mpeg2-containers-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn ffmpeg(args: &[&str]) {
    let status = Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).status().expect("running ffmpeg");
    assert!(status.success(), "ffmpeg {args:?} failed");
}

/// Twenty frames of 176x144 MPEG-2 video in `container`, in `pix` (4:2:2 uses the 4:2:2 profile).
fn encode(dir: &Path, container: &str, pix: &str) -> PathBuf {
    let out = dir.join(format!("{pix}.{container}"));
    let mut args = vec!["-f", "lavfi", "-i", "testsrc2=size=176x144:rate=25", "-frames:v", "20", "-c:v", "mpeg2video", "-g", "12", "-bf", "2", "-pix_fmt", pix];
    if pix == "yuv422p" {
        args.extend_from_slice(&["-profile:v", "0"]);
    }
    args.push(out.to_str().unwrap());
    ffmpeg(&args);
    out
}

/// The pixel format the container declares for its first video stream.
fn declared(path: &Path) -> PixelFormat {
    let d = ironav_formats::open_demuxer(path).unwrap();
    match d.streams().iter().find_map(|s| match s.params {
        StreamParams::Video { pix_fmt, .. } => Some(pix_fmt),
        _ => None,
    }) {
        Some(p) => p,
        None => panic!("{}: no video stream", path.display()),
    }
}

/// The picture data of a Y4M file: the frames without the stream header or the FRAME lines.
fn y4m_payload(b: &[u8]) -> Vec<u8> {
    let find = |h: &[u8], n: &[u8]| h.windows(n.len()).position(|w| w == n);
    let mut out = Vec::new();
    let mut rest = &b[b.iter().position(|&c| c == b'\n').unwrap() + 1..];
    while rest.starts_with(b"FRAME\n") {
        let next = find(&rest[6..], b"FRAME\n").map_or(rest.len(), |j| 6 + j);
        out.extend_from_slice(&rest[6..next]);
        rest = &rest[next..];
    }
    out
}

#[test]
fn mpeg2_containers_declare_the_chroma_format_they_hold() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    let d = scratch("declared");
    for container in ["mp4", "mkv"] {
        assert_eq!(declared(&encode(&d, container, "yuv420p")), PixelFormat::Yuv420p, "{container} 4:2:0");
        assert_eq!(declared(&encode(&d, container, "yuv422p")), PixelFormat::Yuv422p, "{container} 4:2:2");
    }
}

#[test]
fn mpeg2_422_in_mp4_and_mkv_decodes_like_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    let d = scratch("decode");
    for container in ["mp4", "mkv"] {
        let src = encode(&d, container, "yuv422p");
        let raw = d.join(format!("ref-{container}.yuv"));
        ffmpeg(&["-flags", "+bitexact", "-idct", "simple", "-threads", "1", "-i", src.to_str().unwrap(), "-fps_mode", "passthrough", "-f", "rawvideo", "-pix_fmt", "yuv422p", raw.to_str().unwrap()]);
        let y4m = d.join(format!("out-{container}.y4m"));
        let job = Job { input: src.display().to_string(), output: y4m.display().to_string(), hwaccel: "off".into(), ..Job::default() };
        run(&job, &mut |_| {}).unwrap();
        let got = y4m_payload(&std::fs::read(&y4m).unwrap());
        let want = std::fs::read(&raw).unwrap();
        assert!(got == want, "{container}: IronAV decodes the 4:2:2 stream differently from FFmpeg ({} bytes, FFmpeg {} bytes)", got.len(), want.len());
    }
}
