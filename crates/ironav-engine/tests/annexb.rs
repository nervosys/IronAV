//! Raw Annex B H.264 and HEVC files read as input. FFmpeg encodes them; IronAV probes, reads and
//! decodes them, and FFmpeg decodes the same files to the same pictures. A stream copy into MP4 is
//! read by ffprobe and decodes alike too. The tests skip when `ffmpeg` and `ffprobe` are not on the
//! PATH.

use ironav_core::PixelFormat;
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

/// The pictures of a file in `pix` (FFmpeg's pixel format name), as FFmpeg decodes it.
fn ffmpeg_decode(path: &Path, raw: &Path, pix: &str) -> Vec<u8> {
    ffmpeg(&["-i", path.to_str().unwrap(), "-f", "rawvideo", "-pix_fmt", pix, raw.to_str().unwrap()]);
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
    round_trip_as(name, codec, ext, rate, expected, bframes, "yuv420p", extra);
}

/// [`round_trip`] for pictures of the pixel format `pix` (FFmpeg's name, `yuv420p` or `yuv420p10le`).
#[allow(clippy::too_many_arguments)]
fn round_trip_as(name: &str, codec: &str, ext: &str, rate: &str, expected: (i64, i64), bframes: u32, pix: &str, extra: &[&str]) {
    let d = scratch(name);
    let src = d.join("src.y4m");
    let size = format!("testsrc2=size={W}x{H}:rate={rate}");
    let n = FRAMES.to_string();
    ffmpeg(&["-f", "lavfi", "-i", &size, "-frames:v", &n, "-pix_fmt", "yuv420p", src.to_str().unwrap()]);
    let stream = d.join(format!("clip.{ext}"));
    let bf = bframes.to_string();
    let mut enc = vec!["-i", src.to_str().unwrap(), "-c:v", codec, "-pix_fmt", pix, "-bf", &bf, "-g", "4"];
    enc.extend_from_slice(extra);
    let fmt = if codec == "libx265" { "hevc" } else { "h264" };
    enc.extend_from_slice(&["-f", fmt, stream.to_str().unwrap()]);
    ffmpeg(&enc);

    // the frame rate the SPS states is the one IronAV's reader gives the stream, and the pixel format
    // the SPS describes
    let demux = ironav_formats::open_demuxer(&stream).unwrap();
    let info = demux.streams()[0].clone();
    let want_fmt = match pix {
        "yuv420p10le" => PixelFormat::Yuv420p10,
        _ => PixelFormat::Yuv420p,
    };
    match info.params {
        ironav_core::StreamParams::Video { frame_rate, width, height, pix_fmt } => {
            assert_eq!((width, height), (W, H), "{name}: picture size");
            assert_eq!((frame_rate.num, frame_rate.den), expected, "{name}: frame rate");
            assert_eq!(pix_fmt, want_fmt, "{name}: pixel format");
        }
        _ => panic!("{name}: not a video stream"),
    }

    // decoded by IronAV and by FFmpeg, the same pictures
    let bytes_per_picture = (W * H * 3 / 2) as usize * if pix == "yuv420p10le" { 2 } else { 1 };
    let want_raw = ffmpeg_decode(&stream, &d.join("ffmpeg.raw"), pix);
    assert_eq!(want_raw.len(), FRAMES * bytes_per_picture, "{name}: FFmpeg's picture count");
    let back = d.join("ironav.y4m");
    run(&job(&stream, &back), &mut |_| {}).unwrap();
    assert_eq!(y4m_bytes(&std::fs::read(&back).unwrap()), want_raw, "{name}: IronAV and FFmpeg decode differently");

    // a stream copy into MP4 and into Matroska is a file ffprobe reads at the same rate and size, and
    // FFmpeg decodes it to the same pictures in the same order (B pictures included)
    for ext in ["mp4", "mkv"] {
        let out = d.join(format!("copy.{ext}"));
        run(&job(&stream, &out), &mut |_| {}).unwrap();
        let probe = Command::new("ffprobe")
            .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=codec_name,width,height", "-of", "csv=p=0"])
            .arg(&out)
            .output()
            .unwrap();
        let text = String::from_utf8_lossy(&probe.stdout).trim().to_string();
        assert_eq!(text, format!("{fmt},{W},{H}"), "{name}: ffprobe of the {ext} copy");
        if ext == "mp4" {
            assert_eq!(ffprobe_avg_rate(&out), expected, "{name}: frame rate of the copy");
        }
        let raw = ffmpeg_decode(&out, &d.join(format!("copy-{ext}.raw")), pix);
        assert!(raw == want_raw, "{name}: the {ext} copy decodes to other pictures or times ({} bytes, want {})", raw.len(), want_raw.len());
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

#[test]
fn hevc_10_bit_annex_b_reads_its_pixel_format_and_decodes_like_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    round_trip_as("hevc-10bit", "libx265", "hevc", "25", (25, 1), 2, "yuv420p10le", &["-x265-params", "log-level=error"]);
}

#[test]
fn h264_10_bit_annex_b_reads_its_pixel_format_and_decodes_like_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    round_trip_as("h264-10bit", "libx264", "h264", "25", (25, 1), 2, "yuv420p10le", &["-preset", "ultrafast", "-profile:v", "high10"]);
}

#[test]
fn h264_annex_b_joined_mid_gop_is_read_from_its_first_keyframe() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    let d = scratch("h264-midgop");
    let src = d.join("src.y4m");
    let size = format!("testsrc2=size={W}x{H}:rate=25");
    let n = FRAMES.to_string();
    ffmpeg(&["-f", "lavfi", "-i", &size, "-frames:v", &n, "-pix_fmt", "yuv420p", src.to_str().unwrap()]);
    // an IDR picture every four frames and no B pictures, so FFmpeg's output is in decoding order
    let stream = d.join("full.h264");
    ffmpeg(&["-i", src.to_str().unwrap(), "-c:v", "libx264", "-bf", "0", "-g", "4", "-preset", "ultrafast", "-f", "h264", stream.to_str().unwrap()]);
    let full = std::fs::read(&stream).unwrap();
    // the stream joined mid-way: the sequence parameter set from the start, then the P pictures after
    // the first IDR picture (their picture parameter set is left out), then the second IDR picture with
    // its parameter sets
    let starts: Vec<usize> = (0..full.len() - 3).filter(|&i| full[i..i + 3] == [0, 0, 1]).collect();
    assert_eq!(full[starts[0] + 3] & 31, 7, "the stream starts with its sequence parameter set");
    let first_idr = starts.iter().position(|&p| full[p + 3] & 31 == 5).unwrap();
    let mut joined = full[starts[0]..starts[1]].to_vec();
    joined.extend_from_slice(&full[starts[first_idr + 1]..]);
    let cut = d.join("joined.h264");
    std::fs::write(&cut, &joined).unwrap();
    // the pictures FFmpeg decodes from the second IDR picture on (the fifth of the eight)
    let want = ffmpeg_decode(&stream, &d.join("ffmpeg.raw"), "yuv420p");
    let picture = (W * H * 3 / 2) as usize;
    let out = d.join("joined.y4m");
    run(&job(&cut, &out), &mut |_| {}).unwrap();
    assert_eq!(y4m_bytes(&std::fs::read(&out).unwrap()), want[4 * picture..], "a joined stream decodes from its first keyframe");
    let _ = std::fs::remove_dir_all(&d);
}
