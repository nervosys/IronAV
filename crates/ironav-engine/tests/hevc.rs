//! The software HEVC encoder, against FFmpeg and IronAV's own HEVC decoder. The FFmpeg tests skip
//! when `ffmpeg` and `ffprobe` are not on the PATH.

use ironav_codec::hevc::HevcDecoder;
use ironav_codec::new_encoder_with_opts;
use ironav_core::{CodecId, Decoder, Frame, Packet, PixelFormat, Rational, StreamParams, VideoFrame};
use ironav_engine::{run, Job};
use std::path::{Path, PathBuf};
use std::process::Command;

const W: u32 = 178;
const H: u32 = 98;

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ironav-hevc-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn job(input: &Path, output: &Path, qp: u8) -> Job {
    let mut j = Job { input: input.display().to_string(), output: output.display().to_string(), hwaccel: "off".into(), ..Job::default() };
    j.video.codec = Some("hevc".into());
    j.video.qp = Some(qp);
    j
}

/// A job that decodes `input` to `output` with the codecs the output format takes.
fn plain(input: &Path, output: &Path) -> Job {
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

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

/// The pictures of a Y4M file as yuv420p planes, in order.
fn y4m_frames(b: &[u8], w: usize, h: usize) -> Vec<Vec<u8>> {
    let mut rest = &b[b.iter().position(|&c| c == b'\n').unwrap() + 1..];
    let mut out = Vec::new();
    while rest.starts_with(b"FRAME\n") {
        let next = find(&rest[6..], b"FRAME\n").map_or(rest.len(), |j| 6 + j);
        out.push(rest[6..next].to_vec());
        rest = &rest[next..];
    }
    assert!(out.iter().all(|f| f.len() == w * h + 2 * (w / 2) * (h / 2)));
    out
}

fn read(p: &Path) -> Vec<u8> {
    std::fs::read(p).unwrap()
}

/// A test pattern of `frames` pictures of `size` (FFmpeg's moving `testsrc2`) as a Y4M file.
fn pattern(path: &Path, size: &str, frames: u32) {
    let (src, n) = (format!("testsrc2=size={size}:rate=25"), frames.to_string());
    ffmpeg(&["-f", "lavfi", "-i", &src, "-frames:v", &n, "-pix_fmt", "yuv420p", path.to_str().unwrap()]);
}

fn params(w: u32, h: u32) -> StreamParams {
    StreamParams::Video { width: w, height: h, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(25, 1) }
}

/// Encodes the pictures at `qp` with the software encoder and returns the Annex B stream, one packet per picture.
fn encode(frames: &[Vec<u8>], w: u32, h: u32) -> Vec<Vec<u8>> {
    let mut enc = new_encoder_with_opts(CodecId::Hevc, &params(w, h), None, None, None, None).unwrap();
    let mut out = Vec::new();
    for (t, f) in frames.iter().enumerate() {
        let (cw, ch) = (w as usize / 2, h as usize / 2);
        let planes = vec![f[..(w * h) as usize].to_vec(), f[(w * h) as usize..(w * h) as usize + cw * ch].to_vec(), f[(w * h) as usize + cw * ch..].to_vec()];
        let frame = Frame::Video(VideoFrame { width: w, height: h, pix_fmt: PixelFormat::Yuv420p, pts: t as i64, planes });
        for p in enc.encode(&frame).unwrap() {
            out.push(p.data);
        }
    }
    out
}

/// Decodes packets with IronAV's HEVC decoder into yuv420p pictures.
fn decode(packets: &[Vec<u8>]) -> Vec<Vec<u8>> {
    let mut dec = HevcDecoder::new();
    let mut out = Vec::new();
    let mut take = |frames: Vec<Frame>| {
        for f in frames {
            let Frame::Video(v) = f else { panic!("video expected") };
            out.push(v.planes.concat());
        }
    };
    for (i, d) in packets.iter().enumerate() {
        take(dec.decode(&Packet { stream: 0, pts: i as i64, duration: 1, data: d.clone() }).unwrap());
    }
    take(dec.flush().unwrap());
    out
}

/// The PSNR of a yuv420p picture against another, over all planes.
fn psnr(a: &[u8], b: &[u8]) -> f64 {
    let se: f64 = a.iter().zip(b).map(|(&x, &y)| (x as f64 - y as f64).powi(2)).sum();
    if se == 0.0 {
        return 99.0;
    }
    10.0 * (255.0 * 255.0 * a.len() as f64 / se).log10()
}

#[test]
fn hevc_encoder_output_decodes_identically_in_ironav_and_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    let d = scratch("decode");
    let (src, hevc, raw) = (d.join("src.y4m"), d.join("ours.hevc"), d.join("ff.raw"));
    pattern(&src, &format!("{W}x{H}"), 10);
    let frames = y4m_frames(&read(&src), W as usize, H as usize);
    let stream = encode(&frames, W, H);
    std::fs::write(&hevc, stream.concat()).unwrap();

    let ours = decode(&stream);
    assert_eq!(ours.len(), frames.len());
    ffmpeg(&["-f", "hevc", "-i", hevc.to_str().unwrap(), "-f", "rawvideo", "-pix_fmt", "yuv420p", raw.to_str().unwrap()]);
    assert!(ours.concat() == read(&raw), "FFmpeg decodes the stream differently from IronAV");

    // in decode order, against the source: the quality the QP 27 setting asks for
    let mut worst = f64::MAX;
    for (o, s) in ours.iter().zip(&frames) {
        worst = worst.min(psnr(o, s));
    }
    eprintln!("HEVC {W}x{H} at QP 27: lowest picture PSNR {worst:.2} dB");
    assert!(worst > 35.0, "pictures decode at {worst:.1} dB");
}

#[test]
fn hevc_p_pictures_are_smaller_than_intra_pictures() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    let d = scratch("p-size");
    let src = d.join("src.y4m");
    pattern(&src, &format!("{W}x{H}"), 10);
    let frames = y4m_frames(&read(&src), W as usize, H as usize);
    let one: usize = encode(&frames[..1], W, H).iter().map(Vec::len).sum();
    let ten: usize = encode(&frames, W, H).iter().map(Vec::len).sum();
    eprintln!("HEVC 10 pictures: {ten} bytes; one intra picture: {one} bytes; ten intra pictures would be about {}", 10 * one);
    assert!(ten < 10 * one * 9 / 10, "the P pictures take {ten} bytes against {} for ten intra pictures", 10 * one);
}

#[test]
fn hevc_in_mp4_and_matroska_is_hevc_at_the_picture_size() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    let d = scratch("containers");
    let src = d.join("src.y4m");
    pattern(&src, &format!("{W}x{H}"), 6);
    for ext in ["mp4", "mkv"] {
        let out = d.join(format!("ours.{ext}"));
        let r = run(&job(&src, &out, 27), &mut |_| {}).unwrap();
        assert_eq!(r.format, if ext == "mp4" { "mp4" } else { "mkv" });
        let probe = Command::new("ffprobe").args(["-v", "error", "-select_streams", "v:0", "-show_entries", "stream=codec_name,width,height", "-of", "csv=p=0"]).arg(&out).output().unwrap();
        let text = String::from_utf8_lossy(&probe.stdout).trim().to_string();
        assert_eq!(text, format!("hevc,{W},{H}"), "{ext}: ffprobe says {text}");
        // and FFmpeg decodes the container's pictures to the same bytes as IronAV does
        let raw = d.join(format!("{ext}.raw"));
        ffmpeg(&["-i", out.to_str().unwrap(), "-f", "rawvideo", "-pix_fmt", "yuv420p", raw.to_str().unwrap()]);
        let back = d.join(format!("{ext}-back.y4m"));
        run(&plain(&out, &back), &mut |_| {}).unwrap();
        assert_eq!(y4m_frames(&read(&back), W as usize, H as usize).concat(), read(&raw), "{ext}: decoders disagree");
    }
}

/// Speed, size and quality of the HEVC and H.264 software encoders at 720p, through the engine.
/// Run with `cargo test --release -p ironav-engine --test hevc -- --ignored --nocapture`.
#[test]
#[ignore]
fn report_720p_hevc_against_h264() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    let d = scratch("report");
    let src = d.join("src.y4m");
    pattern(&src, "1280x720", 30);
    let frames = y4m_frames(&read(&src), 1280, 720);
    for qp in [22u8, 27, 32, 37] {
        for codec in ["hevc", "h264"] {
            let out = d.join(format!("{codec}-{qp}.mp4"));
            let mut j = job(&src, &out, qp);
            j.video.codec = Some(codec.into());
            let t = std::time::Instant::now();
            run(&j, &mut |_| {}).unwrap();
            let secs = t.elapsed().as_secs_f64();
            let back = d.join(format!("{codec}-{qp}.y4m"));
            run(&plain(&out, &back), &mut |_| {}).unwrap();
            let dec = y4m_frames(&read(&back), 1280, 720);
            let psnrs: Vec<f64> = dec.iter().zip(&frames).map(|(a, b)| psnr(a, b)).collect();
            let mean = psnrs.iter().sum::<f64>() / psnrs.len() as f64;
            let bytes = std::fs::metadata(&out).unwrap().len();
            eprintln!("{codec} QP {qp}: {bytes} bytes, {:.1} kbit/s at 25 fps, mean PSNR {mean:.2} dB, encode {:.1} frames/s", bytes as f64 * 8.0 / 1200.0,frames.len() as f64 / secs);
        }
    }
}
