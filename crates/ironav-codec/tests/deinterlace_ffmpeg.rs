//! The `deinterlace` filter against FFmpeg's `bwdif` on an interlaced clip FFmpeg makes of `testsrc2`
//! (`-vf interlace`), and the field order the MPEG-2 and H.264 decoders report for streams FFmpeg
//! encodes as top or bottom field first. Skipped without `ffmpeg` on PATH.
//!
//! FFmpeg's filter handles the picture edges and a few rounding steps its own way, so the two are
//! close but not identical: the test asserts a loose PSNR floor and prints the value.

use ironav_codec::h264::H264Decoder;
use ironav_codec::mpeg2::Mpeg2Decoder;
use ironav_codec::{DeintMethod, DeintMode, DeintOptions, DeintParity, Deinterlacer};
use ironav_core::{Decoder, FieldOrder, Packet, PixelFormat, VideoFrame};
use std::path::{Path, PathBuf};
use std::process::Command;

const W: usize = 352;
const H: usize = 288;

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn ffmpeg(args: &[&str]) {
    let st = Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).status().unwrap();
    assert!(st.success(), "ffmpeg {args:?} failed");
}

fn scratch() -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("deint-ffmpeg-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn frames_of(raw: &[u8]) -> Vec<VideoFrame> {
    let n = PixelFormat::Yuv420p.frame_bytes(W, H);
    raw.chunks_exact(n)
        .enumerate()
        .map(|(i, c)| {
            let (y, uv) = c.split_at(W * H);
            let (u, v) = uv.split_at(uv.len() / 2);
            VideoFrame { width: W as u32, height: H as u32, pix_fmt: PixelFormat::Yuv420p, pts: i as i64, planes: vec![y.to_vec(), u.to_vec(), v.to_vec()] }
        })
        .collect()
}

fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let mse = a.iter().zip(b).map(|(&x, &y)| (x as f64 - y as f64).powi(2)).sum::<f64>() / a.len() as f64;
    if mse == 0.0 { 99.0 } else { 10.0 * (255.0f64 * 255.0 / mse).log10() }
}

/// Mean luma PSNR of `ours` against `theirs` over the pictures from `skip` to `len - skip`.
fn mean_psnr(ours: &[VideoFrame], theirs: &[VideoFrame], skip: usize) -> f64 {
    assert_eq!(ours.len(), theirs.len(), "same number of pictures");
    let range = skip..ours.len() - skip;
    range.clone().map(|i| psnr(&ours[i].planes[0], &theirs[i].planes[0])).sum::<f64>() / range.len() as f64
}

fn deinterlace(input: &[VideoFrame], opts: DeintOptions) -> Vec<VideoFrame> {
    let mut d = Deinterlacer::new(opts);
    let mut out = Vec::new();
    for f in input {
        out.extend(d.push(f.clone(), FieldOrder::Unknown).unwrap());
    }
    out.extend(d.flush().unwrap());
    out
}

fn raw_in(dir: &Path, name: &str) -> String {
    dir.join(name).to_str().unwrap().to_string()
}

#[test]
fn bwdif_is_close_to_ffmpegs() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch();
    let (inter, fr, fi) = (raw_in(&dir, "interlaced.yuv"), raw_in(&dir, "bwdif_frame.yuv"), raw_in(&dir, "bwdif_field.yuv"));
    // 50 progressive pictures a second, woven two by two into 25 interlaced ones, top field first
    ffmpeg(&["-f", "lavfi", "-i", &format!("testsrc2=s={W}x{H}:r=50:d=2"), "-vf", "interlace=scan=tff,format=yuv420p", "-f", "rawvideo", &inter]);
    let size = format!("{W}x{H}");
    for (mode, out) in [("send_frame", &fr), ("send_field", &fi)] {
        ffmpeg(&["-f", "rawvideo", "-pix_fmt", "yuv420p", "-s", &size, "-r", "25", "-i", &inter, "-vf", &format!("bwdif=mode={mode}:parity=tff:deint=all"), "-f", "rawvideo", out]);
    }
    let input = frames_of(&std::fs::read(&inter).unwrap());
    assert!(input.len() >= 40, "{} pictures", input.len());

    let ours = deinterlace(&input, DeintOptions { parity: DeintParity::TopFirst, ..Default::default() });
    let theirs = frames_of(&std::fs::read(&fr).unwrap());
    let p_frame = mean_psnr(&ours, &theirs, 2);
    // the weave itself, for scale: what leaving the combing in gives against FFmpeg
    let p_weave = mean_psnr(&input, &theirs, 2);
    eprintln!("deinterlace (frame mode) against ffmpeg bwdif: {p_frame:.2} dB luma (the interlaced input itself: {p_weave:.2} dB)");
    assert!(p_frame > 30.0 && p_frame > p_weave + 3.0, "frame mode: {p_frame} dB (weave {p_weave})");

    let ours = deinterlace(&input, DeintOptions { parity: DeintParity::TopFirst, mode: DeintMode::Field, ..Default::default() });
    let theirs = frames_of(&std::fs::read(&fi).unwrap());
    assert_eq!(ours.len(), input.len() * 2);
    let p_field = mean_psnr(&ours, &theirs, 4);
    eprintln!("deinterlace (field mode) against ffmpeg bwdif: {p_field:.2} dB luma");
    assert!(p_field > 30.0, "field mode: {p_field} dB");

    // the linear method is cruder, but still not combed: it is nearer to FFmpeg's than the input is
    let linear = deinterlace(&input, DeintOptions { method: DeintMethod::Linear, parity: DeintParity::TopFirst, ..Default::default() });
    let theirs = frames_of(&std::fs::read(&fr).unwrap());
    let p_linear = mean_psnr(&linear, &theirs, 2);
    eprintln!("deinterlace (linear) against ffmpeg bwdif: {p_linear:.2} dB luma");
    assert!(p_linear > p_weave, "linear: {p_linear} dB (weave {p_weave})");
}

fn all_orders(d: &mut dyn Decoder, data: &[u8]) -> Vec<FieldOrder> {
    let first = d.decode(&Packet { stream: 0, pts: 0, duration: 0, data: data.to_vec() }).unwrap();
    let mut orders = d.field_orders();
    assert_eq!(first.len(), orders.len(), "one order per frame");
    let rest = d.flush().unwrap();
    let tail = d.field_orders();
    assert_eq!(rest.len(), tail.len(), "one order per flushed frame");
    orders.extend(tail);
    orders
}

#[test]
fn decoders_report_the_field_order_of_the_stream() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch();
    let src = format!("testsrc2=s={W}x{H}:r=50,interlace=scan=tff");
    let es = |name: &str| raw_in(&dir, name);

    // MPEG-2: field flags in the picture coding extension
    for (name, flags, want) in [
        ("tff.m2v", "+ildct+ilme", FieldOrder::TopFirst),
        ("bff.m2v", "+ildct+ilme", FieldOrder::BottomFirst),
        ("prog.m2v", "-ildct-ilme", FieldOrder::Progressive),
    ] {
        let graph = if name == "bff.m2v" { format!("testsrc2=s={W}x{H}:r=50,interlace=scan=bff") } else if name == "prog.m2v" { format!("testsrc2=s={W}x{H}:r=25") } else { src.clone() };
        ffmpeg(&["-f", "lavfi", "-i", &graph, "-frames:v", "10", "-pix_fmt", "yuv420p", "-c:v", "mpeg2video", "-flags", flags, "-bf", "2", "-b:v", "4M", "-f", "mpeg2video", &es(name)]);
        let data = std::fs::read(es(name)).unwrap();
        let orders = all_orders(&mut Mpeg2Decoder::new(&[]), &data);
        assert_eq!(orders.len(), 10, "{name}");
        assert!(orders.iter().all(|&o| o == want), "{name}: {orders:?}");
    }

    // H.264 MBAFF: the order of the fields' picture order counts
    for (name, x264, want) in [("tff.h264", "interlaced=1:tff=1", FieldOrder::TopFirst), ("bff.h264", "interlaced=1:bff=1", FieldOrder::BottomFirst)] {
        let graph = if name == "bff.h264" { format!("testsrc2=s={W}x{H}:r=50,interlace=scan=bff") } else { src.clone() };
        ffmpeg(&["-f", "lavfi", "-i", &graph, "-frames:v", "8", "-pix_fmt", "yuv420p", "-c:v", "libx264", "-flags", "+ildct+ilme", "-x264opts", x264, &es(name)]);
        let data = std::fs::read(es(name)).unwrap();
        let orders = all_orders(&mut H264Decoder::with_threads(1), &data);
        assert_eq!(orders.len(), 8, "{name}");
        assert!(orders.iter().all(|&o| o == want), "{name}: {orders:?}");
    }
    // and progressive H.264 says so
    ffmpeg(&["-f", "lavfi", "-i", &format!("testsrc2=s={W}x{H}:r=25"), "-frames:v", "4", "-pix_fmt", "yuv420p", "-c:v", "libx264", &es("prog.h264")]);
    let orders = all_orders(&mut H264Decoder::with_threads(1), &std::fs::read(es("prog.h264")).unwrap());
    assert_eq!(orders, vec![FieldOrder::Progressive; 4]);
}
