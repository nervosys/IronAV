//! Throughput of the software HEVC decoder against FFmpeg's, on 1080p clips that libx265 encodes when
//! the test runs (Main and Main 10, with B pictures; a few MB each, under the OS temp directory or
//! `IRONAV_HEVC_SCRATCH`). The decoded pictures must match FFmpeg's byte for byte. Skips itself without
//! an `ffmpeg` that has libx265. `IRONAV_HEVC_FRAMES` sets the clip length (default 48);
//! `IRONAV_HEVC_NO_FFMPEG_TIMING` skips timing FFmpeg (it still decodes once for the comparison).
//! Run with `cargo test --release -p ironav-codec --test hevc_perf -- --ignored --nocapture`.

use ironav_codec::hevc::HevcDecoder;
use ironav_core::{Decoder, Frame, Packet};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Instant;

fn have_x265() -> bool {
    Command::new("ffmpeg").args(["-hide_banner", "-encoders"]).output().is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains("libx265"))
}

fn scratch() -> PathBuf {
    let d = std::env::var_os("IRONAV_HEVC_SCRATCH").map_or_else(|| std::env::temp_dir().join("ironav-hevc-perf"), PathBuf::from);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run(args: &[&str]) {
    let st = Command::new("ffmpeg").args(["-hide_banner", "-loglevel", "error", "-y"]).args(args).stdin(Stdio::null()).status().unwrap();
    assert!(st.success(), "ffmpeg {args:?} failed");
}

fn ffmpeg_secs(input: &Path, threads: &str, pix_fmt: &str) -> f64 {
    let t = Instant::now();
    run(&["-threads", threads, "-i", input.to_str().unwrap(), "-pix_fmt", pix_fmt, "-f", "null", "-"]);
    t.elapsed().as_secs_f64()
}

fn ironav_decode(data: &[u8], threads: Option<usize>) -> (Vec<Frame>, f64) {
    let mut d = threads.map_or_else(HevcDecoder::new, HevcDecoder::with_threads);
    let t = Instant::now();
    let mut frames = d.decode(&Packet { stream: 0, pts: 0, duration: 0, data: data.to_vec() }).unwrap();
    frames.extend(d.flush().unwrap());
    (frames, t.elapsed().as_secs_f64())
}

fn median(mut v: Vec<f64>) -> f64 {
    if v.is_empty() {
        return f64::INFINITY;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn best(v: &[f64]) -> f64 {
    v.iter().copied().fold(f64::INFINITY, f64::min)
}

fn clip(profile: &str, pix_fmt: &str, n: usize) {
    let dir = scratch();
    let hevc = dir.join(format!("{profile}.hevc"));
    let yuv = dir.join(format!("{profile}.yuv"));
    let frames = n.to_string();
    // testsrc2 with noise gives both flat areas and detail; x265 picks inter, intra, merge, skip
    let vf = format!("noise=alls=6:allf=t,format={pix_fmt}");
    run(&[
        "-f", "lavfi", "-i", "testsrc2=size=1920x1080:rate=30", "-frames:v", &frames, "-vf", &vf, "-c:v", "libx265", "-preset", "fast", "-crf", "24", "-x265-params",
        "keyint=24:bframes=3:log-level=none:frame-threads=2", "-f", "hevc", hevc.to_str().unwrap(),
    ]);
    run(&["-i", hevc.to_str().unwrap(), "-pix_fmt", pix_fmt, "-f", "rawvideo", yuv.to_str().unwrap()]);
    let data = std::fs::read(&hevc).unwrap();
    let want = std::fs::read(&yuv).unwrap();
    let _ = std::fs::remove_file(&yuv);

    let ffmpeg_timing = std::env::var_os("IRONAV_HEVC_NO_FFMPEG_TIMING").is_none();
    let mut one = Vec::new();
    let mut many = Vec::new();
    let mut f1 = Vec::new();
    let mut fn_ = Vec::new();
    let mut got = Vec::new();
    let mut got_many = Vec::new();
    for _ in 0..3 {
        let (fr, s) = ironav_decode(&data, Some(1));
        one.push(s);
        got = fr;
        let (fm, sm) = ironav_decode(&data, None);
        many.push(sm);
        got_many = fm;
        if ffmpeg_timing {
            f1.push(ffmpeg_secs(&hevc, "1", pix_fmt));
            fn_.push(ffmpeg_secs(&hevc, "0", pix_fmt));
        }
    }
    let n = got.len() as f64;
    eprintln!("{profile}: best of 3: IronAV 1 thread {:.1} fps, default threads {:.1} fps", n / best(&one), n / best(&many));
    eprintln!(
        "{profile}: {} pictures ({} KB); IronAV 1 thread {:.1} fps, default threads {:.1} fps; FFmpeg -threads 1 {:.1} fps, default {:.1} fps (median of 3, wall clock; FFmpeg's includes process start)",
        got.len(),
        data.len() / 1024,
        n / median(one),
        n / median(many),
        n / median(f1),
        n / median(fn_),
    );
    for (label, frames) in [("one thread", &got), ("default threads", &got_many)] {
        let mut out = Vec::with_capacity(want.len());
        for f in frames {
            if let Frame::Video(v) = f {
                for p in &v.planes {
                    out.extend_from_slice(p);
                }
            }
        }
        assert_eq!(out.len(), want.len(), "{profile}, {label}: the output is not FFmpeg's size");
        let first = out.iter().zip(&want).position(|(a, b)| a != b);
        assert!(first.is_none(), "{profile}, {label}: the output differs from FFmpeg's at byte {}", first.unwrap_or(0));
    }
    eprintln!("{profile}: identical to FFmpeg with one thread and with the default threads");
    let _ = std::fs::remove_file(&hevc);
}

#[test]
#[ignore]
fn hevc_decode_throughput() {
    if !have_x265() {
        eprintln!("no ffmpeg with libx265: skipping");
        return;
    }
    let n = std::env::var("IRONAV_HEVC_FRAMES").ok().and_then(|v| v.parse().ok()).unwrap_or(48);
    clip("main", "yuv420p", n);
    clip("main10", "yuv420p10le", n);
}
