//! Raw Annex B H.264 and HEVC files read as input. FFmpeg encodes them; IronAV probes, reads and
//! decodes them, and FFmpeg decodes the same files to the same pictures. A stream copy into MP4 is
//! read by ffprobe and decodes alike too. The tests skip when `ffmpeg` and `ffprobe` are not on the
//! PATH.

use ironav_engine::{run, Job};
use std::path::{Path, PathBuf};
use std::process::Command;

const W: u32 = 178;
const H: u32 = 98;
const FRAMES: usize = 8;

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ironav-annexb-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn job(input: &Path, output: &Path) -> Job {
    Job { input: input.display().to_string(), output: output.display().to_string(), hwaccel: "off".into(), ..Job::default() }
}

fn ffmpeg_available() -> bool {
    let ok = |t: &str| Command::new(t).arg("-version").output().map(|o| o.status.success()).unwrap_or(false);
    ok("ffmpeg") && ok("ffprobe")
}

fn ffmpeg(args: &[&str]) {
    let st = Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).status().unwrap();
    assert!(st.success(), "ffmpeg {args:?} failed");
}

/// The picture-by-picture yuv420p bytes of a file, as FFmpeg decodes it.
fn ffmpeg_decode(path: &Path, raw: &Path) -> Vec<u8> {
    ffmpeg(&["-i", path.to_str().unwrap(), "-f", "rawvideo", "-pix_fmt", "yuv420p", raw.to_str().unwrap()]);
    std::fs::read(raw).unwrap()
}

/// The pictures of a Y4M file, one plane-set after another.
fn y4m_bytes(b: &[u8]) -> Vec<u8> {
    let find = |h: &[u8], n: &[u8]| h.windows(n.len()).position(|w| w == n);
    let mut rest = &b[b.iter().position(|&c| c == b'\n').unwrap() + 1..];
    let mut out = Vec::new();
    while rest.starts_with(b"FRAME\n") {
        let next = find(&rest[6..], b"FRAME\n").map_or(rest.len(), |j| 6 + j);
        out.extend_from_slice(&rest[6..next]);
        rest = &rest[next..];
    }
    out
}

/// The frame rate of a video file as ffprobe's `avg_frame_rate` gives it, as (numerator, denominator).
fn ffprobe_avg_rate(path: &Path) -> (i64, i64) {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=avg_frame_rate", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let (n, d) = text.split_once('/').unwrap();
    (n.parse().unwrap(), d.parse().unwrap())
}

/// Encodes FFmpeg's test pattern at `rate` (frames a second, as `25` or `30000/1001`) with `codec`
/// into a raw stream of `ext`, with `bframes` B pictures between references, then checks IronAV's
/// reader against FFmpeg's decoder. `expected` is the stream's frame rate: the SPS timing says it, and
/// ffprobe reads a raw H.264 stream's rate as twice that (its ticks count fields).
fn round_trip(name: &str, codec: &str, ext: &str, rate: &str, expected: (i64, i64), bframes: u32, extra: &[&str]) {
    let d = scratch(name);
    let src = d.join("src.y4m");
    let size = format!("testsrc2=size={W}x{H}:rate={rate}");
    let n = FRAMES.to_string();
    ffmpeg(&["-f", "lavfi", "-i", &size, "-frames:v", &n, "-pix_fmt", "yuv420p", src.to_str().unwrap()]);
    let stream = d.join(format!("clip.{ext}"));
    let bf = bframes.to_string();
    let mut enc = vec!["-i", src.to_str().unwrap(), "-c:v", codec, "-bf", &bf, "-g", "4"];
    enc.extend_from_slice(extra);
    let fmt = if codec == "libx265" { "hevc" } else { "h264" };
    enc.extend_from_slice(&["-f", fmt, stream.to_str().unwrap()]);
    ffmpeg(&enc);

    // the frame rate the SPS states is the one IronAV's reader gives the stream
    let demux = ironav_formats::open_demuxer(&stream).unwrap();
    let info = demux.streams()[0].clone();
    match info.params {
        ironav_core::StreamParams::Video { frame_rate, width, height, .. } => {
            assert_eq!((width, height), (W, H), "{name}: picture size");
            assert_eq!((frame_rate.num, frame_rate.den), expected, "{name}: frame rate");
        }
        _ => panic!("{name}: not a video stream"),
    }

    // decoded by IronAV and by FFmpeg, the same pictures
    let want_raw = ffmpeg_decode(&stream, &d.join("ffmpeg.raw"));
    assert_eq!(want_raw.len(), FRAMES * (W * H * 3 / 2) as usize, "{name}: FFmpeg's picture count");
    let back = d.join("ironav.y4m");
    run(&job(&stream, &back), &mut |_| {}).unwrap();
    assert_eq!(y4m_bytes(&std::fs::read(&back).unwrap()), want_raw, "{name}: IronAV and FFmpeg decode differently");

    // a stream copy into MP4 is a file ffprobe reads at the same rate and size
    let mp4 = d.join("copy.mp4");
    run(&job(&stream, &mp4), &mut |_| {}).unwrap();
    let probe = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=codec_name,width,height", "-of", "csv=p=0"])
        .arg(&mp4)
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&probe.stdout).trim().to_string();
    assert_eq!(text, format!("{fmt},{W},{H}"), "{name}: ffprobe of the copy");
    assert_eq!(ffprobe_avg_rate(&mp4), expected, "{name}: frame rate of the copy");
    // Pictures are numbered in decoding order and the MP4 takes those numbers as presentation times,
    // so a stream with B pictures is out of order in the copy: FFmpeg then repeats pictures to fill
    // the gaps. Only a stream without B pictures decodes alike from the copy.
    if bframes == 0 {
        assert_eq!(ffmpeg_decode(&mp4, &d.join("copy.raw")), want_raw, "{name}: the copy decodes differently");
    }
    let _ = std::fs::remove_dir_all(&d);
}

#[test]
fn h264_annex_b_reads_and_decodes_like_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    round_trip("h264", "libx264", "h264", "25", (25, 1), 0, &["-preset", "ultrafast"]);
    round_trip("h264-b", "libx264", "h264", "25", (25, 1), 2, &["-preset", "ultrafast"]);
}

#[test]
fn h264_annex_b_at_ntsc_rate_takes_the_rate_from_the_sps() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    round_trip("h264-ntsc", "libx264", "264", "30000/1001", (30000, 1001), 0, &["-preset", "ultrafast"]);
}

#[test]
fn hevc_annex_b_reads_and_decodes_like_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    round_trip("hevc", "libx265", "hevc", "25", (25, 1), 0, &["-x265-params", "log-level=error"]);
    round_trip("hevc-b", "libx265", "hevc", "25", (25, 1), 2, &["-x265-params", "log-level=error"]);
}
