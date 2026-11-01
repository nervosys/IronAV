//! Interlaced H.264 coded as MBAFF frames, made with FFmpeg's libx264 when the test runs, decodes
//! to exactly what FFmpeg's decoder makes of it: CABAC and CAVLC, spatial and temporal direct,
//! explicit weights, the 8x8 transform and without it, several slices, bottom field first, 4:2:2
//! and 4:4:4, lossless, and 720x480 and 1920x1080 pictures, each on one thread and on the
//! decoder's default threads. Skipped without `ffmpeg` on PATH. Small streams are also checked in,
//! in ironav-engine's tests. (Constrained intra prediction is left out: FFmpeg 9.0.1 reports errors
//! in libx264's MBAFF streams with it and conceals them, so its output is no reference.)

use ironav_codec::h264::H264Decoder;
use ironav_core::{Decoder, Frame, Packet};
use std::path::{Path, PathBuf};
use std::process::Command;

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn ffmpeg(args: &[&str]) {
    let st = Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).status().unwrap();
    assert!(st.success(), "ffmpeg {args:?} failed");
}

fn scratch() -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("h264-mbaff-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// The pictures `d` makes of an Annex B stream, their planes one after another.
fn decode(mut d: H264Decoder, data: &[u8]) -> Vec<u8> {
    let mut frames = d.decode(&Packet { stream: 0, pts: 0, duration: 0, data: data.to_vec() }).unwrap();
    frames.extend(d.flush().unwrap());
    let mut out = Vec::new();
    for f in &frames {
        if let Frame::Video(v) = f {
            for p in &v.planes {
                out.extend_from_slice(p);
            }
        }
    }
    out
}

/// A stream from `src` (a lavfi graph) with libx264 options `x264` after the interlacing ones, and
/// FFmpeg's decode of it as `pix_fmt`; checks both decoder setups against it.
fn check(dir: &Path, name: &str, src: &str, frames: u32, x264: &str, pix_fmt: &str) {
    let es = dir.join(format!("{name}.h264"));
    let yuv = dir.join(format!("{name}.yuv"));
    let order = if x264.contains("bff=1") { "" } else { "tff=1:" };
    let opts = format!("interlaced=1:{order}{x264}");
    let n = frames.to_string();
    ffmpeg(&["-f", "lavfi", "-i", src, "-frames:v", &n, "-pix_fmt", pix_fmt, "-c:v", "libx264", "-flags", "+ildct+ilme", "-x264opts", &opts, es.to_str().unwrap()]);
    ffmpeg(&["-i", es.to_str().unwrap(), "-f", "rawvideo", "-pix_fmt", pix_fmt, yuv.to_str().unwrap()]);
    let data = std::fs::read(&es).unwrap();
    let want = std::fs::read(&yuv).unwrap();
    for (how, d) in [("one thread", H264Decoder::with_threads(1)), ("default threads", H264Decoder::new())] {
        let got = decode(d, &data);
        assert_eq!(got.len(), want.len(), "{name} ({how}): size");
        let first = got.iter().zip(&want).position(|(a, b)| a != b);
        assert!(first.is_none(), "{name} ({how}): differs from FFmpeg's decode at byte {}", first.unwrap_or(0));
    }
}

/// Fields of a moving picture woven into frames, so that libx264 codes many pairs as fields.
fn woven(size: &str) -> String {
    format!("testsrc2=s={size}:r=50,tinterlace=4")
}

#[test]
fn mbaff_streams_decode_to_what_ffmpeg_decodes() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch();
    let cif = woven("352x288");
    let fade = format!("{cif},fade=in:0:10");
    let cases: [(&str, &str, u32, &str, &str); 13] = [
        ("cabac_spatial", &cif, 12, "bframes=2", "yuv420p"),
        ("cavlc_spatial", &cif, 12, "bframes=2:no-cabac=1", "yuv420p"),
        ("cabac_temporal", &cif, 12, "bframes=3:direct=temporal:ref=4", "yuv420p"),
        ("cavlc_temporal", &cif, 12, "bframes=3:direct=temporal:no-cabac=1", "yuv420p"),
        ("weightp", &fade, 12, "bframes=2:weightp=2", "yuv420p"),
        ("weightp_cavlc", &fade, 12, "bframes=2:weightp=2:no-cabac=1", "yuv420p"),
        ("no_8x8", &cif, 10, "bframes=2:no-8x8dct=1", "yuv420p"),
        ("slices", &cif, 10, "bframes=2:slices=4", "yuv420p"),
        ("bff", &cif, 10, "bframes=2:bff=1:direct=temporal", "yuv420p"),
        ("yuv422", &cif, 8, "bframes=2", "yuv422p"),
        ("yuv444_lossless", &cif, 6, "bframes=2:qp=0", "yuv444p"),
        ("ntsc", &woven("720x480"), 6, "bframes=2", "yuv420p"),
        ("hd", &woven("1920x1080"), 3, "bframes=1", "yuv420p"),
    ];
    for (name, src, frames, x264, pix_fmt) in cases {
        check(&dir, name, src, frames, x264, pix_fmt);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// Low-delay decoding (on the caller's thread, pictures out as soon as they can be) of an MBAFF
/// stream without B frames gives FFmpeg's pictures too.
#[test]
fn mbaff_decodes_in_low_delay_mode() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch().join("low-delay");
    std::fs::create_dir_all(&dir).unwrap();
    let es = dir.join("p.h264");
    let yuv = dir.join("p.yuv");
    let src = woven("352x288");
    ffmpeg(&["-f", "lavfi", "-i", &src, "-frames:v", "10", "-c:v", "libx264", "-flags", "+ildct+ilme", "-x264opts", "interlaced=1:tff=1:bframes=0", es.to_str().unwrap()]);
    ffmpeg(&["-i", es.to_str().unwrap(), "-f", "rawvideo", "-pix_fmt", "yuv420p", yuv.to_str().unwrap()]);
    let got = decode(H264Decoder::low_delay(), &std::fs::read(&es).unwrap());
    let want = std::fs::read(&yuv).unwrap();
    assert!(got == want, "low-delay decoding differs from FFmpeg's");
    let _ = std::fs::remove_dir_all(&dir);
}
