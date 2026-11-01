//! MPEG-1 and MPEG-2 video, decoded by IronAV and compared with FFmpeg byte for byte.
//!
//! Two kinds of test. The fixture tests decode the streams in `tests/data/mpeg2_*.m2v` (random but
//! valid syntax from `tools/gen_mpeg2_synth.py`: field pictures, 16x8 and dual prime motion,
//! concealment vectors, escape codes, custom matrices, 4:2:2, MPEG-1) and compare with the
//! `.ffmpeg.yuv` files FFmpeg wrote for them; they need no `ffmpeg`. The live tests have
//! `ffmpeg -c:v mpeg2video` or `mpeg1video` write streams (progressive, interlaced, with B pictures,
//! 4:2:2) into an elementary stream, MPEG-TS, MPEG-PS, VOB, MP4 and Matroska and compare with
//! `-flags +bitexact -idct simple`; they skip when `ffmpeg` is not on the PATH.

use ironav_engine::{run, Job};
use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ironav-mpeg2-{}-{name}", std::process::id()));
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

fn decode(input: &Path, dir: &Path) -> Vec<u8> {
    let y4m = dir.join("out.y4m");
    let r = run(&job(input, &y4m), &mut |_| {}).unwrap();
    assert_eq!(r.format, "y4m");
    y4m_payload(&std::fs::read(&y4m).unwrap())
}

fn data(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests").join("data").join(name)
}

fn fixture(name: &str) {
    let d = scratch(name);
    let got = decode(&data(&format!("mpeg2_{name}.m2v")), &d);
    let want = std::fs::read(data(&format!("mpeg2_{name}.ffmpeg.yuv"))).unwrap();
    assert!(got == want, "{name}: IronAV decodes the stream differently ({} bytes, FFmpeg {} bytes)", got.len(), want.len());
}

#[test]
fn mpeg1_fixtures_match_ffmpeg() {
    fixture("m1");
    fixture("m1_crop");
}

#[test]
fn frame_picture_fixtures_match_ffmpeg() {
    fixture("p_frame");
    fixture("p_frame_ivlc");
    fixture("i_frame_il");
    fixture("i_frame_il_422");
}

#[test]
fn field_picture_fixtures_match_ffmpeg() {
    fixture("i_field");
    fixture("i_field_bff");
    fixture("i_field_422");
    fixture("i_mixed_alt");
    fixture("i_mixed_422");
}

#[test]
fn concealment_vectors_and_matrices_match_ffmpeg() {
    fixture("i_mixed_conceal");
    fixture("i_mixed_matrix");
}

const V2: &[&str] = &["-c:v", "mpeg2video", "-g", "12", "-bf", "2"];
const V1: &[&str] = &["-c:v", "mpeg1video", "-g", "12", "-bf", "2"];
const INTERLACED: &[&str] = &["-flags", "+ildct+ilme", "-vf", "setfield=tff"];

/// Encodes `frames` pictures of 176x144 with `args` into a `container` file, then checks that
/// IronAV's decode is FFmpeg's bit for bit. Returns the number of frames compared.
fn check(name: &str, container: &str, pix: &str, args: &[&str], frames: u32) -> usize {
    let d = scratch(name);
    let src = d.join(format!("src.{container}"));
    let raw = d.join("ref.raw");
    let n = frames.to_string();
    let mut enc = vec!["-f", "lavfi", "-i", "testsrc2=size=176x144:rate=25", "-frames:v", &n, "-pix_fmt", pix];
    enc.extend_from_slice(args);
    enc.push(src.to_str().unwrap());
    assert!(ffmpeg(&enc), "{name}: ffmpeg could not encode the stream");
    assert!(
        ffmpeg(&["-flags", "+bitexact", "-idct", "simple", "-threads", "1", "-i", src.to_str().unwrap(), "-fps_mode", "passthrough", "-f", "rawvideo", "-pix_fmt", pix, raw.to_str().unwrap()]),
        "{name}: ffmpeg could not decode the stream"
    );
    let (got, want) = (decode(&src, &d), std::fs::read(&raw).unwrap());
    assert!(got == want, "{name}: IronAV decodes FFmpeg's MPEG differently ({} bytes, FFmpeg {} bytes)", got.len(), want.len());
    let frame = if pix == "yuv422p" { 176 * 144 * 2 } else { 176 * 144 * 3 / 2 };
    want.len() / frame
}

fn cat(parts: &[&[&str]]) -> Vec<String> {
    parts.iter().flat_map(|p| p.iter().map(|s| s.to_string())).collect()
}

fn with(name: &str, container: &str, pix: &str, parts: &[&[&str]], frames: u32) -> usize {
    let args = cat(parts);
    let refs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    check(name, container, pix, &refs, frames)
}

#[test]
fn mpeg2_progressive_streams_decode_bit_exactly() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    assert_eq!(with("es-p", "m2v", "yuv420p", &[V2, &["-bf", "0"]], 20), 20);
    assert_eq!(with("es-b", "m2v", "yuv420p", &[V2], 20), 20);
    assert_eq!(with("ts-b", "ts", "yuv420p", &[V2], 20), 20);
    assert_eq!(with("mp4-b", "mp4", "yuv420p", &[V2], 20), 20);
    assert_eq!(with("mkv-b", "mkv", "yuv420p", &[V2], 20), 20);
}

#[test]
fn mpeg2_interlaced_streams_decode_bit_exactly() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    assert_eq!(with("es-il", "m2v", "yuv420p", &[V2, INTERLACED], 20), 20);
    assert_eq!(with("ts-il", "ts", "yuv420p", &[V2, INTERLACED], 20), 20);
    assert_eq!(with("ps-il", "mpg", "yuv420p", &[V2, INTERLACED, &["-f", "mpeg"]], 20), 20);
    assert_eq!(with("vob-il", "vob", "yuv420p", &[V2, INTERLACED, &["-f", "vob"]], 20), 20);
    assert_eq!(with("mp4-il", "mp4", "yuv420p", &[V2, INTERLACED], 20), 20);
}

#[test]
fn mpeg2_422_streams_decode_bit_exactly() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    assert_eq!(with("es-422", "m2v", "yuv422p", &[V2, INTERLACED, &["-profile:v", "0"]], 20), 20);
    assert_eq!(with("mkv-422", "mkv", "yuv422p", &[V2, INTERLACED, &["-profile:v", "0"]], 20), 20);
}

#[test]
fn mpeg1_streams_decode_bit_exactly() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    assert_eq!(with("m1-es", "m1v", "yuv420p", &[V1], 20), 20);
    assert_eq!(with("m1-ts", "ts", "yuv420p", &[V1], 20), 20);
    assert_eq!(with("m1-ps", "mpg", "yuv420p", &[V1, &["-f", "mpeg"]], 20), 20);
}
