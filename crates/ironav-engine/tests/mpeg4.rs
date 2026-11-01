//! MPEG-4 Part 2 video, decoded by IronAV and compared with FFmpeg: `ffmpeg -c:v mpeg4` writes
//! each stream (I-only, P, B, quarter-sample, MPEG quantiser, four vectors) into MP4 or Matroska,
//! and both decoders must agree byte for byte with `-flags +bitexact -idct simple`. The tests
//! skip when `ffmpeg` is not on the PATH.

use ironav_engine::{run, Job};
use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ironav-mpeg4-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn job(input: &Path, output: &Path) -> Job {
    Job { input: input.display().to_string(), output: output.display().to_string(), hwaccel: "auto".into(), ..Job::default() }
}

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn ffmpeg(args: &[&str]) -> bool {
    Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).status().unwrap().success()
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

/// The picture data of a Y4M file: the frames without the stream header or the FRAME lines.
fn y4m_payload(b: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut rest = &b[b.iter().position(|&c| c == b'\n').unwrap() + 1..];
    while rest.starts_with(b"FRAME\n") {
        let next = find(&rest[6..], b"FRAME\n").map_or(rest.len(), |j| 6 + j);
        out.extend_from_slice(&rest[6..next]);
        rest = &rest[next..];
    }
    out
}

/// Encodes `frames` pictures of 178x98 with `args`, then checks that IronAV's decode of the file
/// is FFmpeg's bit for bit. Returns the number of frames compared.
fn check(name: &str, container: &str, args: &[&str], frames: u32) -> usize {
    check_sized(name, container, args, frames, 178, 98)
}

fn check_sized(name: &str, container: &str, args: &[&str], frames: u32, w: u32, h: u32) -> usize {
    let d = scratch(name);
    let src = d.join(format!("src.{container}"));
    let raw = d.join("ref.raw");
    let y4m = d.join("out.y4m");
    let n = frames.to_string();
    let size = format!("testsrc2=size={w}x{h}:rate=25");
    let mut enc = vec!["-f", "lavfi", "-i", &size, "-frames:v", &n, "-c:v", "mpeg4", "-pix_fmt", "yuv420p"];
    enc.extend_from_slice(args);
    enc.push(src.to_str().unwrap());
    assert!(ffmpeg(&enc), "{name}: ffmpeg could not encode the stream");
    assert!(
        ffmpeg(&["-flags", "+bitexact", "-idct", "simple", "-i", src.to_str().unwrap(), "-f", "rawvideo", "-pix_fmt", "yuv420p", raw.to_str().unwrap()]),
        "{name}: ffmpeg could not decode the stream"
    );
    let r = run(&job(&src, &y4m), &mut |_| {}).unwrap();
    assert_eq!(r.format, "y4m", "{name}");
    let (got, want) = (y4m_payload(&std::fs::read(&y4m).unwrap()), std::fs::read(&raw).unwrap());
    assert!(got == want, "{name}: IronAV decodes FFmpeg's MPEG-4 differently ({} bytes, FFmpeg {} bytes)", got.len(), want.len());
    want.len() / (w as usize * h as usize * 3 / 2)
}

#[test]
fn intra_pictures_decode_bit_exactly() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    assert_eq!(check("intra-mp4", "mp4", &["-g", "1"], 3), 3);
    assert_eq!(check("intra-mkv", "mkv", &["-g", "1"], 3), 3);
}

#[test]
fn predicted_pictures_decode_bit_exactly() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    assert_eq!(check("p-mp4", "mp4", &["-g", "10", "-bf", "0"], 12), 12);
    assert_eq!(check("p-4mv-mp4", "mp4", &["-g", "10", "-bf", "0", "-flags", "+mv4"], 12), 12);
    assert_eq!(check("p-qpel-mp4", "mp4", &["-g", "10", "-bf", "0", "-flags", "+qpel"], 12), 12);
    assert_eq!(check("p-mpeg-quant-mp4", "mp4", &["-g", "10", "-bf", "0", "-mpeg_quant", "1"], 12), 12);
}

#[test]
fn bidirectional_pictures_decode_bit_exactly() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    assert_eq!(check("b-mp4", "mp4", &["-g", "10", "-bf", "2"], 12), 12);
    assert_eq!(check("b-mkv", "mkv", &["-g", "10", "-bf", "2"], 12), 12);
    assert_eq!(check("b-qpel-4mv-mp4", "mp4", &["-g", "10", "-bf", "2", "-flags", "+qpel+mv4"], 12), 12);
}

#[test]
fn data_partitioned_streams_are_rejected_with_an_error() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    let d = scratch("data-partitioning");
    let src = d.join("dp.mp4");
    assert!(ffmpeg(&["-f", "lavfi", "-i", "testsrc2=size=178x98:rate=25", "-frames:v", "3", "-c:v", "mpeg4", "-data_partitioning", "1", "-pix_fmt", "yuv420p", src.to_str().unwrap()]));
    let err = run(&job(&src, &d.join("out.y4m")), &mut |_| {}).expect_err("data partitioning is not decoded");
    assert!(err.to_string().contains("data partitioning"), "unexpected error: {err}");
}

#[test]
fn four_vector_and_rate_distorted_streams_at_cif_decode_bit_exactly() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    for q in ["3", "12"] {
        let name = format!("cif-mv4-rd-q{q}");
        assert_eq!(check_sized(&name, "mp4", &["-g", "30", "-bf", "0", "-flags", "+mv4", "-mbd", "rd", "-qscale:v", q], 30, 352, 288), 30);
    }
}
