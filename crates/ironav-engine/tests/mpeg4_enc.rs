//! MPEG-4 Part 2 video written by IronAV's software encoder. FFmpeg decodes the MP4 and Matroska files
//! to exactly what IronAV's decoder decodes, the pictures are close to the source, and the size and
//! the rate behave as the quantiser and the bit rate ask. The tests skip when `ffmpeg` is not on the
//! PATH. Scratch files go under `IRONAV_TEST_SCRATCH` when it is set, else the temporary directory.
//!
//! `cargo test -p ironav-engine --test mpeg4_enc -- --ignored --nocapture` prints the 720p table.

use ironav_engine::{run, Job};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

fn scratch(name: &str) -> PathBuf {
    let root = std::env::var_os("IRONAV_TEST_SCRATCH").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let d = root.join(format!("ironav-m4enc-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn job(input: &Path, output: &Path) -> Job {
    Job { input: input.display().to_string(), output: output.display().to_string(), hwaccel: "off".into(), overwrite: true, ..Job::default() }
}

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn ffmpeg(args: &[&str]) -> bool {
    Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).status().unwrap().success()
}

/// A y4m file's frame size and the (width, height) in its header.
fn y4m_dims(d: &[u8]) -> (usize, usize) {
    let head = String::from_utf8_lossy(&d[..d.iter().position(|&b| b == b'\n').unwrap()]).to_string();
    let field = |c: char| head.split(' ').find_map(|t| t.strip_prefix(c)?.parse::<usize>().ok()).unwrap();
    (field('W'), field('H'))
}

/// The raw planar frames of a y4m file.
fn y4m_frames(path: &Path) -> Vec<Vec<u8>> {
    let d = std::fs::read(path).unwrap();
    let (w, h) = y4m_dims(&d);
    let fsize = w * h + 2 * w.div_ceil(2) * h.div_ceil(2);
    let mut i = d.iter().position(|&b| b == b'\n').unwrap() + 1;
    let mut out = Vec::new();
    while i < d.len() {
        let nl = i + d[i..].iter().position(|&b| b == b'\n').unwrap();
        out.push(d[nl + 1..nl + 1 + fsize].to_vec());
        i = nl + 1 + fsize;
    }
    out
}

fn luma_psnr(a: &[u8], b: &[u8], n: usize) -> f64 {
    let mse = a[..n].iter().zip(&b[..n]).map(|(&p, &q)| (p as f64 - q as f64).powi(2)).sum::<f64>() / n as f64;
    10.0 * (255.0f64 * 255.0 / mse.max(1e-9)).log10()
}

/// A y4m source of `frames` pictures of testsrc2, made by FFmpeg.
fn source(dir: &Path, w: usize, h: usize, frames: usize, fps: u32) -> PathBuf {
    let src = dir.join("src.y4m");
    let size = format!("testsrc2=size={w}x{h}:rate={fps}");
    let n = frames.to_string();
    assert!(ffmpeg(&["-f", "lavfi", "-i", &size, "-frames:v", &n, "-pix_fmt", "yuv420p", src.to_str().unwrap()]), "ffmpeg could not make the source");
    src
}

/// Encodes `src` to `out` with IronAV's MPEG-4 encoder and returns the file's size.
fn encode(src: &Path, out: &Path, qp: Option<u8>, bitrate_kbps: Option<u32>, keyint: Option<u32>) -> u64 {
    let mut j = job(src, out);
    j.video.codec = Some("mpeg4".into());
    j.video.qp = qp;
    j.video.bitrate_kbps = bitrate_kbps;
    j.video.keyint = keyint;
    run(&j, &mut |_| {}).unwrap();
    std::fs::metadata(out).unwrap().len()
}

/// IronAV's decode of `file`, as y4m frames in display order.
fn decode_ironav(file: &Path, dir: &Path) -> Vec<Vec<u8>> {
    let y4m = dir.join("ours.y4m");
    run(&job(file, &y4m), &mut |_| {}).unwrap();
    y4m_frames(&y4m)
}

/// FFmpeg's decode of `file` as raw 4:2:0 planar frames.
fn decode_ffmpeg(file: &Path, dir: &Path) -> Vec<u8> {
    let raw = dir.join("ffmpeg.yuv");
    assert!(ffmpeg(&["-i", file.to_str().unwrap(), "-f", "rawvideo", "-pix_fmt", "yuv420p", raw.to_str().unwrap()]), "ffmpeg could not decode {}", file.display());
    std::fs::read(raw).unwrap()
}

fn mean_psnr(decoded: &[Vec<u8>], src: &[Vec<u8>], n: usize) -> f64 {
    decoded.iter().zip(src).map(|(d, s)| luma_psnr(d, s, n)).sum::<f64>() / decoded.len() as f64
}

/// Written by IronAV in MP4 and in Matroska, FFmpeg decodes each file to what IronAV decodes, and the
/// pictures are close to the source, at odd sizes too.
#[test]
fn encoded_files_decode_to_what_ffmpeg_decodes() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    for (w, h) in [(176, 144), (178, 100)] {
        let dir = scratch(&format!("dec-{w}x{h}"));
        let src = source(&dir, w, h, 10, 25);
        let frames = y4m_frames(&src);
        for ext in ["mp4", "mkv"] {
            let enc = dir.join(format!("out.{ext}"));
            encode(&src, &enc, Some(5), None, None);
            let ours = decode_ironav(&enc, &dir);
            let theirs = decode_ffmpeg(&enc, &dir);
            assert_eq!(ours.len(), frames.len(), "{w}x{h} {ext}: picture count");
            assert!(ours.concat() == theirs, "{w}x{h} {ext}: IronAV's decode differs from FFmpeg's");
            let p = mean_psnr(&ours, &frames, w * h);
            assert!(p > 32.0, "{w}x{h} {ext}: PSNR {p:.1} dB at qscale 5");
        }
    }
}

/// The quantiser trades size for quality: a coarser quantiser is smaller and less faithful.
#[test]
fn larger_quantiser_is_smaller_and_less_faithful() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch("qp");
    let src = source(&dir, 176, 144, 10, 25);
    let frames = y4m_frames(&src);
    let mut prev: Option<(u64, f64)> = None;
    for qp in [2u8, 5, 10, 20] {
        let enc = dir.join(format!("q{qp}.mp4"));
        let size = encode(&src, &enc, Some(qp), None, None);
        let p = mean_psnr(&decode_ironav(&enc, &dir), &frames, 176 * 144);
        if let Some((s0, p0)) = prev {
            assert!(size < s0, "qscale {qp}: {size} bytes, not below {s0}");
            assert!(p < p0, "qscale {qp}: {p:.1} dB, not below {p0:.1}");
        }
        assert!(p > 20.0, "qscale {qp}: PSNR {p:.1} dB");
        prev = Some((size, p));
    }
}

/// Inter pictures are what make the stream small: the same pictures, all of them key pictures, take
/// more bytes than the stream does.
#[test]
fn inter_pictures_make_the_stream_smaller_than_intra_only() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch("inter");
    let src = source(&dir, 176, 144, 10, 25);
    let predicted = encode(&src, &dir.join("p.mp4"), Some(5), None, None);
    let intra = encode(&src, &dir.join("i.mp4"), Some(5), None, Some(1));
    assert!(predicted < intra, "{predicted} bytes with inter pictures, {intra} intra only");
}

/// A bit rate target is roughly met: two seconds of video at 500 kbit/s come out near that size.
#[test]
fn bit_rate_target_is_roughly_met() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch("rate");
    let src = source(&dir, 176, 144, 50, 25);
    let size = encode(&src, &dir.join("r.mp4"), None, Some(500), None);
    let kbps = size as f64 * 8.0 / 2.0 / 1000.0;
    assert!((250.0..1000.0).contains(&kbps), "{kbps:.0} kbit/s for a 500 kbit/s target");
}

/// The 720p table: IronAV at qscale 2, 5, 10 and 20 for 60 frames of testsrc2, with each file's size,
/// PSNR (decoded by IronAV, in decode order, against the source) and encode speed, and FFmpeg's
/// `-c:v mpeg4 -qscale:v N` for the same quantisers. Run it in release mode; it is not a pass/fail test.
#[test]
#[ignore]
fn report_720p_table() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch("report");
    let (w, h, n) = (1280, 720, 60);
    let src = source(&dir, w, h, n, 25);
    let frames = y4m_frames(&src);
    println!("| qscale | IronAV bytes | IronAV PSNR dB | IronAV fps | FFmpeg bytes | FFmpeg PSNR dB |");
    println!("|---|---|---|---|---|---|");
    for qp in [2u8, 5, 10, 20] {
        let enc = dir.join(format!("ours{qp}.mp4"));
        let t = Instant::now();
        let size = encode(&src, &enc, Some(qp), None, None);
        let fps = n as f64 / t.elapsed().as_secs_f64();
        let ours = decode_ironav(&enc, &dir);
        let p = mean_psnr(&ours, &frames, w * h);

        let theirs = dir.join(format!("ff{qp}.mp4"));
        let q = qp.to_string();
        let gs = if ffmpeg(&["-i", src.to_str().unwrap(), "-c:v", "mpeg4", "-qscale:v", &q, "-pix_fmt", "yuv420p", theirs.to_str().unwrap()]) {
            std::fs::metadata(&theirs).unwrap().len()
        } else {
            0
        };
        let fraw = decode_ffmpeg(&theirs, &dir);
        let fframes: Vec<Vec<u8>> = fraw.chunks(w * h * 3 / 2).map(|c| c.to_vec()).collect();
        let fp = mean_psnr(&fframes, &frames, w * h);
        println!("| {qp} | {size} | {p:.2} | {fps:.1} | {gs} | {fp:.2} |");
    }
}

/// MPEG-4 takes a quantiser scale of 1 to 31; the plan refuses anything else by name before any output is written.
#[test]
fn qp_outside_the_mpeg4_range_is_refused_by_name() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch("qp-range");
    let src = source(&dir, 176, 144, 2, 25);
    for qp in [0u8, 32, 51] {
        let out = dir.join(format!("bad{qp}.mp4"));
        let mut j = job(&src, &out);
        j.video.codec = Some("mpeg4".into());
        j.video.qp = Some(qp);
        let msg = run(&j, &mut |_| {}).expect_err("qp outside 1-31 is refused").to_string();
        assert!(msg.contains("mpeg4") && msg.contains("1-31"), "qp {qp}: {msg}");
        assert!(!out.exists(), "qp {qp} wrote an output");
    }
    for qp in [1u8, 31] {
        let out = dir.join(format!("ok{qp}.mp4"));
        encode(&src, &out, Some(qp), None, None);
    }
}
