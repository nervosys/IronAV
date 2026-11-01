//! The software AV1 encoder: its streams decode with FFmpeg's libaom and with IronAV's own decoder to
//! identical pictures, at a useful quality, and get smaller as the quantiser rises. The source is
//! FFmpeg's testsrc2; odd sizes are cropped from an even testsrc2 (FFmpeg rounds odd sizes down). The
//! tests skip when `ffmpeg` is not on the PATH.

use ironav_engine::{run, Job};
use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ironav-av1enc-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn ffmpeg(args: &[&str]) -> bool {
    Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).status().map(|s| s.success()).unwrap_or(false)
}

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").output().map(|o| o.status.success()).unwrap_or(false)
}

macro_rules! need_ffmpeg {
    () => {
        if !ffmpeg_available() {
            eprintln!("ffmpeg not on PATH; skipping");
            return;
        }
    };
}

/// A 4:2:0 clip: each frame is its Y, U and V planes, back to back.
struct Clip {
    w: usize,
    h: usize,
    frames: Vec<Vec<u8>>,
}

impl Clip {
    fn chroma(&self) -> (usize, usize) {
        (self.w.div_ceil(2), self.h.div_ceil(2))
    }

    fn y4m(&self) -> Vec<u8> {
        let mut out = format!("YUV4MPEG2 W{} H{} F25:1 Ip A1:1 C420jpeg\n", self.w, self.h).into_bytes();
        for f in &self.frames {
            out.extend_from_slice(b"FRAME\n");
            out.extend_from_slice(f);
        }
        out
    }
}

/// testsrc2 at `w` x `h` for `frames` frames, as 4:2:0 with each chroma sample the mean of the 2x2
/// 4:4:4 samples it covers (the chroma of an odd picture covers the edge's samples once).
fn source(dir: &Path, w: usize, h: usize, frames: usize) -> Clip {
    let raw = dir.join("src444.raw");
    let n = frames.to_string();
    // testsrc2 is 4:2:0, so a crop of an odd picture in 4:2:0 comes out rounded down (177x99 as 176x98),
    // and the frames would be read at the wrong stride; the crop is taken in 4:4:4, where it is exact
    let size = format!("testsrc2=size={}x{}:rate=25", w.next_multiple_of(2), h.next_multiple_of(2));
    let crop = format!("format=yuv444p,crop={w}:{h}:0:0");
    assert!(ffmpeg(&["-f", "lavfi", "-i", &size, "-vf", &crop, "-frames:v", &n, "-f", "rawvideo", "-pix_fmt", "yuv444p", raw.to_str().unwrap()]), "ffmpeg could not make testsrc2");
    let data = std::fs::read(&raw).unwrap();
    let (cw, ch) = (w.div_ceil(2), h.div_ceil(2));
    let mut out = Vec::new();
    for f in data.chunks_exact(3 * w * h) {
        let mut frame = f[..w * h].to_vec();
        for plane in 1..3 {
            let p = &f[plane * w * h..(plane + 1) * w * h];
            for cy in 0..ch {
                for cx in 0..cw {
                    let (mut sum, mut n) = (0u32, 0u32);
                    for y in cy * 2..(cy * 2 + 2).min(h) {
                        for x in cx * 2..(cx * 2 + 2).min(w) {
                            sum += p[y * w + x] as u32;
                            n += 1;
                        }
                    }
                    frame.push(((sum + n / 2) / n) as u8);
                }
            }
        }
        out.push(frame);
    }
    Clip { w, h, frames: out }
}

/// The picture data of a Y4M file: the frames without the stream header or the FRAME lines.
fn y4m_payload(b: &[u8]) -> Vec<u8> {
    let find = |h: &[u8], n: &[u8]| h.windows(n.len()).position(|w| w == n);
    let mut out = Vec::new();
    let mut rest = &b[b.iter().position(|&c| c == b'\n').unwrap() + 1..];
    while rest.starts_with(b"FRAME\n") {
        let next = find(&rest[6..], b"FRAME\n").map_or(rest.len(), |j| 6 + j);
        out.extend_from_slice(&rest[6..next]);
        rest = &rest[next..];
    }
    out
}

/// Encodes `src` at `qp` into `dir`'s `name` WebM and returns its path.
fn encode(dir: &Path, name: &str, src: &Path, qp: u8, keyint: Option<u32>) -> PathBuf {
    let out = dir.join(format!("{name}.webm"));
    let mut j = Job { input: src.display().to_string(), output: out.display().to_string(), hwaccel: "off".into(), overwrite: true, ..Job::default() };
    j.video.codec = Some("av1".into());
    j.video.qp = Some(qp);
    j.video.keyint = keyint;
    run(&j, &mut |_| {}).unwrap_or_else(|e| panic!("{name}: IronAV could not encode: {e}"));
    out
}

/// Decodes `stream` with FFmpeg's libaom and with IronAV, requires equal pictures, and returns them.
fn decode_both(dir: &Path, stream: &Path, name: &str) -> Vec<u8> {
    let raw = dir.join(format!("{name}.libaom.raw"));
    assert!(ffmpeg(&["-c:v", "libaom-av1", "-i", stream.to_str().unwrap(), "-f", "rawvideo", "-pix_fmt", "yuv420p", raw.to_str().unwrap()]), "{name}: libaom could not decode");
    let y4m = dir.join(format!("{name}.ironav.y4m"));
    let j = Job { input: stream.display().to_string(), output: y4m.display().to_string(), hwaccel: "off".into(), overwrite: true, ..Job::default() };
    let r = run(&j, &mut |_| {}).unwrap_or_else(|e| panic!("{name}: IronAV could not decode: {e}"));
    assert_eq!(r.format, "y4m", "{name}");
    let (got, want) = (y4m_payload(&std::fs::read(&y4m).unwrap()), std::fs::read(&raw).unwrap());
    if got != want {
        let i = got.iter().zip(&want).position(|(a, b)| a != b).unwrap_or(got.len().min(want.len()));
        panic!("{name}: IronAV decodes the stream differently from libaom ({} bytes, libaom {}; first difference at byte {i})", got.len(), want.len());
    }
    want
}

/// The luma and chroma PSNR of decoded pictures against the source, in dB.
fn psnr(clip: &Clip, decoded: &[u8]) -> (f64, f64) {
    let (cw, ch) = clip.chroma();
    let frame = clip.w * clip.h + 2 * cw * ch;
    let (mut luma, mut chroma) = ((0f64, 0usize), (0f64, 0usize));
    for (f, src) in clip.frames.iter().enumerate() {
        let got = &decoded[f * frame..(f + 1) * frame];
        for (i, (&a, &b)) in src.iter().zip(got).enumerate() {
            let e = (a as f64 - b as f64).powi(2);
            let acc = if i < clip.w * clip.h { &mut luma } else { &mut chroma };
            acc.0 += e;
            acc.1 += 1;
        }
    }
    let db = |(sse, n): (f64, usize)| 10.0 * (255.0 * 255.0 / (sse / n as f64).max(1e-9)).log10();
    (db(luma), db(chroma))
}

/// Encodes testsrc2 at `w` x `h` and checks the decode and the quality; returns the size of the
/// stream and its luma PSNR.
fn round_trip(name: &str, w: usize, h: usize, frames: usize, qp: u8) -> (u64, f64) {
    round_trip_keyint(name, w, h, frames, qp, None)
}

/// `round_trip` with a key frame interval (the encoder's default when `keyint` is None).
fn round_trip_keyint(name: &str, w: usize, h: usize, frames: usize, qp: u8, keyint: Option<u32>) -> (u64, f64) {
    let dir = scratch(name);
    let clip = source(&dir, w, h, frames);
    let src = dir.join("src.y4m");
    std::fs::write(&src, clip.y4m()).unwrap();
    let stream = encode(&dir, name, &src, qp, keyint);
    let decoded = decode_both(&dir, &stream, name);
    assert_eq!(decoded.len(), clip.frames.len() * clip.frames[0].len(), "{name}: picture count");
    let (y, c) = psnr(&clip, &decoded);
    let size = std::fs::metadata(&stream).unwrap().len();
    eprintln!("{name}: {w}x{h} qp {qp}: {size} bytes, luma {y:.2} dB, chroma {c:.2} dB");
    let _ = std::fs::remove_dir_all(&dir);
    (size, y)
}

#[test]
fn pictures_decode_identically_with_libaom_and_ironav() {
    need_ffmpeg!();
    let (_, y) = round_trip("cif", 352, 288, 3, 30);
    assert!(y > 30.0, "352x288 qp 30: luma PSNR {y:.2} dB");
    // an odd size: the picture and its chroma are cut from a partial superblock and a partial 8x8
    let (_, y) = round_trip("odd", 177, 99, 3, 30);
    assert!(y > 30.0, "177x99 qp 30: luma PSNR {y:.2} dB");
}

#[test]
fn quality_holds_at_a_mid_quantiser() {
    need_ffmpeg!();
    let (_, y) = round_trip("mid", 352, 288, 2, 20);
    assert!(y > 35.0, "352x288 qp 20: luma PSNR {y:.2} dB");
}

#[test]
fn the_file_gets_smaller_as_the_quantiser_rises() {
    need_ffmpeg!();
    let sizes: Vec<u64> = [20u8, 30, 40].iter().map(|&qp| round_trip(&format!("size{qp}"), 352, 288, 2, qp).0).collect();
    assert!(sizes[0] > sizes[1] && sizes[1] > sizes[2], "sizes at qp 20, 30, 40: {sizes:?}");
}

/// Inter frames: a key frame every 10 of 12 moving pictures (one run of nine inter frames), decoded
/// identically by libaom and IronAV, at a useful quality and smaller than the same clip with every
/// frame a key frame. The odd size checks the partial superblocks and 8x8s of inter prediction, and
/// its inter frames must save as much as the even size's do (about 30% at qp 30).
#[test]
fn inter_frames_decode_identically_and_are_smaller_than_key_frames() {
    need_ffmpeg!();
    for (name, w, h) in [("inter", 352usize, 288usize), ("inter_odd", 177, 99)] {
        let (inter, y) = round_trip_keyint(name, w, h, 12, 30, Some(10));
        assert!(y > 30.0, "{name}: luma PSNR {y:.2} dB");
        let (key_only, _) = round_trip_keyint(&format!("{name}_keys"), w, h, 12, 30, Some(1));
        assert!(inter * 100 < key_only * 85, "{name}: inter frames {inter} bytes, key frames only {key_only} bytes");
    }
}
