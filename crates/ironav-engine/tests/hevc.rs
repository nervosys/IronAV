//! The software HEVC encoder, against FFmpeg and IronAV's own HEVC decoder. The FFmpeg tests skip
//! when `ffmpeg` and `ffprobe` are not on the PATH.

use ironav_codec::hevc::{HevcDecoder, HevcEncoder};
use ironav_codec::new_encoder_with_opts;
use ironav_core::{CodecId, Decoder, Encoder, Frame, Packet, PixelFormat, Rational, StreamInfo, StreamParams, VideoFrame};
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

/// Encodes the pictures with the software encoder and returns the Annex B stream, one packet per
/// picture, in decoding order.
fn encode(frames: &[Vec<u8>], w: u32, h: u32) -> Vec<Vec<u8>> {
    let mut enc = new_encoder_with_opts(CodecId::Hevc, &params(w, h), None, None, None, None).unwrap();
    encode_with(enc.as_mut(), frames, w, h)
}

/// Feeds the pictures to `enc` and flushes it; the packets' data in decoding order.
fn encode_with(enc: &mut dyn Encoder, frames: &[Vec<u8>], w: u32, h: u32) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for (t, f) in frames.iter().enumerate() {
        let (cw, ch) = (w as usize / 2, h as usize / 2);
        let planes = vec![f[..(w * h) as usize].to_vec(), f[(w * h) as usize..(w * h) as usize + cw * ch].to_vec(), f[(w * h) as usize + cw * ch..].to_vec()];
        let frame = Frame::Video(VideoFrame { width: w, height: h, pix_fmt: PixelFormat::Yuv420p, pts: t as i64, planes });
        for p in enc.encode(&frame).unwrap() {
            out.push(p.data);
        }
    }
    for p in enc.flush().unwrap() {
        out.push(p.data);
    }
    out
}

/// Decodes packets with IronAV's HEVC decoder into yuv420p pictures, in display order.
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

    // in display order, against the source: the quality the QP 27 setting asks for
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

/// A picture of `f` (planar yuv420p) as a video frame with time stamp `pts`.
fn video_frame(f: &[u8], w: u32, h: u32, pts: i64) -> Frame {
    let (cw, ch) = (w as usize / 2, h as usize / 2);
    let planes = vec![f[..(w * h) as usize].to_vec(), f[(w * h) as usize..(w * h) as usize + cw * ch].to_vec(), f[(w * h) as usize + cw * ch..].to_vec()];
    Frame::Video(VideoFrame { width: w, height: h, pix_fmt: PixelFormat::Yuv420p, pts, planes })
}

/// B pictures (three between anchors) decode to the same pictures in FFmpeg and in IronAV, in display
/// order. They are off by default (see the report).
#[test]
fn hevc_b_pictures_decode_identically_in_ironav_and_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    let d = scratch("b-decode");
    let (src, hevc, raw) = (d.join("src.y4m"), d.join("b.hevc"), d.join("b.raw"));
    pattern(&src, &format!("{W}x{H}"), 13);
    let frames = y4m_frames(&read(&src), W as usize, H as usize);
    let mut enc = HevcEncoder::new(&params(W, H), Some(27), None).unwrap().with_bframes(3);
    let stream = encode_with(&mut enc, &frames, W, H);
    let ours = decode(&stream);
    assert_eq!(ours.len(), frames.len());
    std::fs::write(&hevc, stream.concat()).unwrap();
    ffmpeg(&["-f", "hevc", "-i", hevc.to_str().unwrap(), "-f", "rawvideo", "-pix_fmt", "yuv420p", raw.to_str().unwrap()]);
    assert!(ours.concat() == read(&raw), "FFmpeg decodes the B-picture stream differently from IronAV");
    let worst = ours.iter().zip(&frames).map(|(o, s)| psnr(o, s)).fold(f64::MAX, f64::min);
    assert!(worst > 30.0, "B pictures decode at {worst:.1} dB");
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

/// Six pictures coded with B pictures (so the packets are out of display order) are muxed into MP4 and
/// Matroska with their time stamps: FFmpeg reads the packets' presentation times as 0 to 5 pictures, each
/// a picture long, and the containers decode to the pictures IronAV decodes.
#[test]
fn hevc_b_pictures_have_display_time_stamps_in_mp4_and_matroska() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    let d = scratch("b-times");
    let src = d.join("src.y4m");
    pattern(&src, &format!("{W}x{H}"), 6);
    let frames = y4m_frames(&read(&src), W as usize, H as usize);
    let mut enc = HevcEncoder::new(&params(W, H), Some(27), None).unwrap().with_bframes(3);
    let mut pkts: Vec<Packet> = Vec::new();
    for (t, f) in frames.iter().enumerate() {
        pkts.extend(enc.encode(&video_frame(f, W, H, t as i64)).unwrap());
    }
    pkts.extend(enc.flush().unwrap());
    let decode_order: Vec<i64> = pkts.iter().map(|p| p.pts).collect();
    assert_eq!(decode_order, [0, 4, 1, 2, 3, 5]);
    for ext in ["mp4", "mkv"] {
        let out = d.join(format!("ours.{ext}"));
        let mut m = ironav_formats::create_muxer(&out, ext).unwrap();
        let info = StreamInfo { index: 0, codec: CodecId::Hevc, time_base: Rational::new(1, 25), duration: None, bits: None, extradata: Vec::new(), lead_in: 0, params: params(W, H) };
        m.write_header(&[info]).unwrap();
        for p in &pkts {
            m.write_packet(p).unwrap();
        }
        m.finish().unwrap();
        // the packets in the file's order, each with its presentation time and duration
        let probe = Command::new("ffprobe").args(["-v", "error", "-select_streams", "v:0", "-show_entries", "packet=pts,duration", "-of", "csv=p=0"]).arg(&out).output().unwrap();
        let text = String::from_utf8_lossy(&probe.stdout).to_string();
        let mut rows: Vec<(i64, i64)> = text
            .lines()
            .map(|l| {
                let f: Vec<i64> = l.split(',').filter_map(|v| v.trim().parse().ok()).collect();
                assert_eq!(f.len(), 2, "{ext}: a packet without time stamps: {text}");
                (f[0], f[1])
            })
            .collect();
        assert_eq!(rows.len(), 6, "{ext}: {text}");
        rows.sort();
        // pictures are a whole number of ticks apart: the step is the picture duration
        let (step, dur) = (rows[1].0 - rows[0].0, rows[0].1);
        assert!(step > 0 && dur > 0, "{ext}: {text}");
        let pts: Vec<i64> = rows.iter().map(|r| r.0).collect();
        assert_eq!(pts, (0..6).map(|k| k * step).collect::<Vec<_>>(), "{ext}: presentation times {text}");
        assert!(rows.iter().all(|r| r.1 == dur), "{ext}: durations differ {text}");
        let raw = d.join(format!("{ext}.raw"));
        ffmpeg(&["-i", out.to_str().unwrap(), "-f", "rawvideo", "-pix_fmt", "yuv420p", raw.to_str().unwrap()]);
        let back = d.join(format!("{ext}-back.y4m"));
        run(&plain(&out, &back), &mut |_| {}).unwrap();
        assert_eq!(y4m_frames(&read(&back), W as usize, H as usize).concat(), read(&raw), "{ext}: decoders disagree");
    }
}

/// The report behind the HEVC B-picture numbers: 720p, 60 frames, four quantisers, P pictures only and
/// with three B pictures between anchors (the QP cascade of `QP_OFFSET_BFRAMES`), through the encoder
/// directly, on FFmpeg's testsrc2 and mandelbrot. Bytes, mean Y PSNR over the pictures in display order
/// (IronAV's decoder, which FFmpeg matches), and encode speed. Run with
/// `cargo test --release -p ironav-engine --test hevc -- --ignored --nocapture`.
#[test]
#[ignore]
fn report_720p_hevc_p_only_against_b_pictures() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    let d = scratch("report");
    for (name, src_spec) in [
        ("testsrc2", "testsrc2=size=1280x720:rate=25".to_string()),
        ("mandelbrot", "mandelbrot=s=1280x720:r=25".to_string()),
    ] {
        let src = d.join(format!("{name}.y4m"));
        ffmpeg(&["-f", "lavfi", "-i", &src_spec, "-frames:v", "60", "-pix_fmt", "yuv420p", src.to_str().unwrap()]);
        let frames = y4m_frames(&read(&src), 1280, 720);
        for qp in [22u8, 27, 32, 37] {
            for (label, b) in [("P only", 0usize), ("B pictures", 3)] {
                let mut enc = HevcEncoder::new(&params(1280, 720), Some(qp), None).unwrap().with_bframes(b);
                let t = std::time::Instant::now();
                let stream = encode_with(&mut enc, &frames, 1280, 720);
                let secs = t.elapsed().as_secs_f64();
                let bytes: usize = stream.iter().map(Vec::len).sum();
                let ours = decode(&stream);
                let luma = ours.iter().zip(&frames).map(|(o, s)| psnr(&o[..1280 * 720], &s[..1280 * 720])).sum::<f64>() / frames.len() as f64;
                eprintln!("{name} QP {qp} {label}: {bytes} bytes, Y PSNR {luma:.2} dB, encode {:.2} frames/s", frames.len() as f64 / secs);
            }
        }
    }
}
