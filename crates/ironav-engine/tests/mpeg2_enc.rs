//! MPEG-2 video written by IronAV's software encoder (Main profile, 4:2:0). FFmpeg decodes the raw
//! elementary stream, MPEG-TS, Matroska and MP4 files to exactly what IronAV's decoder decodes (with
//! `-flags +bitexact -idct simple -cpuflags 0`), the pictures are close to the source, and the size and
//! the rate behave as the quantiser and the bit rate ask. The tests skip when `ffmpeg` is not on the
//! PATH. Scratch files go under `IRONAV_TEST_SCRATCH` when it is set, else the temporary directory.
//!
//! `cargo test -p ironav-engine --test mpeg2_enc -- --ignored --nocapture` prints the table: 352x288
//! and 720x576, testsrc2 and mandelbrot, 60 frames, at qscale 3, 5 and 10, without and with two B pictures
//! between anchors, against FFmpeg's `-c:v mpeg2video -qscale:v N` (`-bf 2` for the B pictures).

use ironav_engine::{run, Job};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

fn scratch(name: &str) -> PathBuf {
    let root = std::env::var_os("IRONAV_TEST_SCRATCH").map(PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let d = root.join(format!("ironav-m2enc-{}-{name}", std::process::id()));
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

/// A y4m source of `frames` pictures of an FFmpeg lavfi source (`filter`: testsrc2, mandelbrot) at `w` by `h`.
fn source(dir: &Path, filter: &str, w: usize, h: usize, frames: usize, fps: u32) -> PathBuf {
    let src = dir.join(format!("{filter}-{w}x{h}.y4m"));
    let size = format!("{filter}=size={w}x{h}:rate={fps}");
    let n = frames.to_string();
    assert!(ffmpeg(&["-f", "lavfi", "-i", &size, "-frames:v", &n, "-pix_fmt", "yuv420p", src.to_str().unwrap()]), "ffmpeg could not make the source");
    src
}

/// Encodes `src` to `out` (its extension picks the container: .m2v, .ts, .mkv or .mp4) with IronAV's
/// MPEG-2 encoder, and returns the file's size.
fn encode(src: &Path, out: &Path, qp: Option<u8>, bitrate_kbps: Option<u32>, keyint: Option<u32>, bframes: Option<u32>) -> u64 {
    let mut j = job(src, out);
    j.video.codec = Some("mpeg2video".into());
    j.video.qp = qp;
    j.video.bitrate_kbps = bitrate_kbps;
    j.video.keyint = keyint;
    j.video.bframes = bframes;
    run(&j, &mut |_| {}).unwrap();
    std::fs::metadata(out).unwrap().len()
}

/// IronAV's decode of `file`, as y4m frames in display order.
fn decode_ironav(file: &Path, dir: &Path) -> Vec<Vec<u8>> {
    let y4m = dir.join("ours.y4m");
    run(&job(file, &y4m), &mut |_| {}).unwrap();
    y4m_frames(&y4m)
}

/// FFmpeg's decode of `file` as raw 4:2:0 planar frames, bit-exact with the IDCT of the reference.
fn decode_ffmpeg(file: &Path, dir: &Path) -> Vec<u8> {
    let raw = dir.join("ffmpeg.yuv");
    let f = file.to_str().unwrap();
    let r = raw.to_str().unwrap();
    assert!(ffmpeg(&["-flags", "+bitexact", "-idct", "simple", "-cpuflags", "0", "-threads", "1", "-i", f, "-fps_mode", "passthrough", "-f", "rawvideo", "-pix_fmt", "yuv420p", r]), "ffmpeg could not decode {}", file.display());
    std::fs::read(raw).unwrap()
}

fn mean_psnr(decoded: &[Vec<u8>], src: &[Vec<u8>], n: usize) -> f64 {
    decoded.iter().zip(src).map(|(d, s)| luma_psnr(d, s, n)).sum::<f64>() / decoded.len() as f64
}

/// Written by IronAV in each container, FFmpeg decodes each file to what IronAV decodes, and the
/// pictures are close to the source, at odd sizes too.
#[test]
fn encoded_files_decode_to_what_ffmpeg_decodes() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    for (w, h) in [(176, 144), (178, 100)] {
        let dir = scratch(&format!("dec-{w}x{h}"));
        let src = source(&dir, "testsrc2", w, h, 10, 25);
        let frames = y4m_frames(&src);
        for ext in ["m2v", "ts", "mkv", "mp4"] {
            let enc = dir.join(format!("out.{ext}"));
            encode(&src, &enc, Some(5), None, None, None);
            let ours = decode_ironav(&enc, &dir);
            let theirs = decode_ffmpeg(&enc, &dir);
            assert_eq!(ours.len(), frames.len(), "{w}x{h} {ext}: picture count");
            assert!(ours.concat() == theirs, "{w}x{h} {ext}: IronAV's decode differs from FFmpeg's");
            let p = mean_psnr(&ours, &frames, w * h);
            assert!(p > 30.0, "{w}x{h} {ext}: PSNR {p:.1} dB at qscale 5");
        }
    }
}

/// B pictures: the stream with two B pictures between anchors decodes the same in FFmpeg and in IronAV,
/// and the pictures come out in display order.
#[test]
fn b_pictures_decode_to_what_ffmpeg_decodes() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch("bframes");
    let src = source(&dir, "mandelbrot", 178, 100, 12, 25);
    let frames = y4m_frames(&src);
    for ext in ["ts", "mkv"] {
        let enc = dir.join(format!("b.{ext}"));
        encode(&src, &enc, Some(6), None, None, Some(2));
        let ours = decode_ironav(&enc, &dir);
        let theirs = decode_ffmpeg(&enc, &dir);
        assert_eq!(ours.len(), frames.len(), "{ext}: picture count");
        assert!(ours.concat() == theirs, "{ext}: IronAV's decode differs from FFmpeg's");
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
    let src = source(&dir, "testsrc2", 176, 144, 10, 25);
    let frames = y4m_frames(&src);
    let mut prev: Option<(u64, f64)> = None;
    for qp in [2u8, 5, 10, 20] {
        let enc = dir.join(format!("q{qp}.ts"));
        let size = encode(&src, &enc, Some(qp), None, None, None);
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
    let src = source(&dir, "testsrc2", 176, 144, 10, 25);
    let predicted = encode(&src, &dir.join("p.m2v"), Some(5), None, None, None);
    let intra = encode(&src, &dir.join("i.m2v"), Some(5), None, Some(1), None);
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
    let src = source(&dir, "testsrc2", 176, 144, 50, 25);
    let size = encode(&src, &dir.join("r.ts"), None, Some(500), None, None);
    let kbps = size as f64 * 8.0 / 2.0 / 1000.0;
    assert!((250.0..1000.0).contains(&kbps), "{kbps:.0} kbit/s for a 500 kbit/s target");
}

/// MPEG-2 takes a quantiser code of 1 to 31; the plan refuses anything else by name before any output is written.
#[test]
fn qp_outside_the_mpeg2_range_is_refused_by_name() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch("qp-range");
    let src = source(&dir, "testsrc2", 176, 144, 2, 25);
    for qp in [0u8, 32, 51] {
        let out = dir.join(format!("bad{qp}.m2v"));
        let mut j = job(&src, &out);
        j.video.codec = Some("mpeg2video".into());
        j.video.qp = Some(qp);
        let msg = run(&j, &mut |_| {}).expect_err("qp outside 1-31 is refused").to_string();
        assert!(msg.contains("mpeg2video") && msg.contains("1-31"), "qp {qp}: {msg}");
        assert!(!out.exists(), "qp {qp} wrote an output");
    }
    for qp in [1u8, 31] {
        let out = dir.join(format!("ok{qp}.m2v"));
        encode(&src, &out, Some(qp), None, None, None);
    }
}

/// The table: IronAV at qscale 3, 5 and 10 for 60 frames of testsrc2 and mandelbrot at 352x288 and 720x576,
/// without B pictures and with two, each with the file's size, the luma PSNR of IronAV's decode (by picture
/// index, against the source) and the encode speed, next to FFmpeg's `-c:v mpeg2video -qscale:v N` for the
/// same source (`-bf 2` with B pictures). Both are raw .m2v files. Not a pass/fail test.
#[test]
#[ignore]
fn report_table() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch("report");
    let n = 60;
    println!("| source | size | qscale | B pictures | IronAV bytes | IronAV luma PSNR dB | IronAV fps | FFmpeg bytes | FFmpeg luma PSNR dB |");
    println!("|---|---|---|---|---|---|---|---|---|");
    for (filter, w, h) in [("testsrc2", 352, 288), ("mandelbrot", 352, 288), ("testsrc2", 720, 576), ("mandelbrot", 720, 576)] {
        let src = source(&dir, filter, w, h, n, 25);
        let frames = y4m_frames(&src);
        for bframes in [0u32, 2] {
            for qp in [3u8, 5, 10] {
                let enc = dir.join(format!("ours-{filter}-{w}-{bframes}-{qp}.m2v"));
                let t = Instant::now();
                let size = encode(&src, &enc, Some(qp), None, None, Some(bframes));
                let fps = n as f64 / t.elapsed().as_secs_f64();
                let p = mean_psnr(&decode_ironav(&enc, &dir), &frames, w * h);

                let theirs = dir.join(format!("ff-{filter}-{w}-{bframes}-{qp}.m2v"));
                let q = qp.to_string();
                let b = bframes.to_string();
                let fsize = if ffmpeg(&["-i", src.to_str().unwrap(), "-c:v", "mpeg2video", "-qscale:v", &q, "-bf", &b, "-pix_fmt", "yuv420p", "-f", "mpeg2video", theirs.to_str().unwrap()]) {
                    std::fs::metadata(&theirs).unwrap().len()
                } else {
                    0
                };
                let fraw = decode_ffmpeg(&theirs, &dir);
                let fframes: Vec<Vec<u8>> = fraw.chunks(w * h * 3 / 2).map(|c| c.to_vec()).collect();
                let fp = mean_psnr(&fframes, &frames, w * h);
                println!("| {filter} | {w}x{h} | {qp} | {bframes} | {size} | {p:.2} | {fps:.1} | {fsize} | {fp:.2} |");
            }
        }
    }
}
