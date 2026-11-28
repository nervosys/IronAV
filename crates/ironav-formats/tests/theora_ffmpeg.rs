//! Theora streams made with FFmpeg's libtheora when the test runs decode through the Ogg reader (and the
//! Matroska reader, for the same stream copied) and IronAV's decoder to the same bytes as FFmpeg's own
//! decode: 4:2:0, 4:2:2 and 4:4:4, a size that is not a whole number of macroblocks (so the picture
//! region matters), several qualities and key frame intervals, with and without motion. Also seeking in
//! Ogg (it lands on a key frame, and the packets after it are the ones a full read has) and damaged
//! streams (errors, never panics). Skipped without `ffmpeg` (and its libtheora) on PATH.

use ironav_core::{Frame, Packet, StreamParams};
use std::path::{Path, PathBuf};
use std::process::Command;

fn ffmpeg(args: &[&str]) -> bool {
    Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).status().map(|s| s.success()).unwrap_or(false)
}

/// Whether FFmpeg is there and has the libtheora encoder.
fn have_ffmpeg() -> bool {
    Command::new("ffmpeg").args(["-hide_banner", "-encoders"]).output().is_ok_and(|o| o.status.success() && String::from_utf8_lossy(&o.stdout).contains("libtheora"))
}

fn scratch(tag: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("theora-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Case {
    name: &'static str,
    size: (u32, u32),
    pix_fmt: &'static str,
    quality: u32,
    gop: u32,
    source: &'static str,
    frames: u32,
}

const CASES: &[Case] = &[
    Case { name: "cif_q6_g12", size: (176, 144), pix_fmt: "yuv420p", quality: 6, gop: 12, source: "testsrc2", frames: 30 },
    Case { name: "cif_q2_g250", size: (176, 144), pix_fmt: "yuv420p", quality: 2, gop: 250, source: "testsrc2", frames: 30 },
    Case { name: "cif_q10_g1", size: (176, 144), pix_fmt: "yuv420p", quality: 10, gop: 1, source: "testsrc2", frames: 12 },
    Case { name: "odd_q6_g12", size: (330, 186), pix_fmt: "yuv420p", quality: 6, gop: 12, source: "testsrc2", frames: 30 },
    Case { name: "odd_q8_g250", size: (330, 186), pix_fmt: "yuv420p", quality: 8, gop: 250, source: "mandelbrot", frames: 24 },
    Case { name: "odd_q3_g6", size: (330, 186), pix_fmt: "yuv420p", quality: 3, gop: 6, source: "mandelbrot", frames: 24 },
    Case { name: "p444_q6_g12", size: (176, 144), pix_fmt: "yuv444p", quality: 6, gop: 12, source: "testsrc2", frames: 24 },
    Case { name: "p444_odd_q9", size: (330, 186), pix_fmt: "yuv444p", quality: 9, gop: 250, source: "testsrc2", frames: 24 },
    Case { name: "p422_q6_g12", size: (176, 144), pix_fmt: "yuv422p", quality: 6, gop: 12, source: "testsrc2", frames: 24 },
    Case { name: "p422_odd_q4", size: (330, 186), pix_fmt: "yuv422p", quality: 4, gop: 250, source: "mandelbrot", frames: 24 },
    Case { name: "tiny_q6", size: (64, 48), pix_fmt: "yuv420p", quality: 6, gop: 8, source: "testsrc2", frames: 20 },
];

/// Encode a case to `.ogv`, and decode it with FFmpeg to raw frames in the stream's own format.
fn make(dir: &Path, c: &Case) -> Option<(PathBuf, Vec<u8>)> {
    let ogv = dir.join(format!("{}.ogv", c.name));
    let src = format!("{}=size={}x{}:rate=25", c.source, c.size.0, c.size.1);
    let (q, g, n) = (c.quality.to_string(), c.gop.to_string(), c.frames.to_string());
    if !ffmpeg(&["-f", "lavfi", "-i", &src, "-frames:v", &n, "-c:v", "libtheora", "-pix_fmt", c.pix_fmt, "-q:v", &q, "-g", &g, ogv.to_str().unwrap()]) {
        return None;
    }
    let raw = reference(dir, c.name, &ogv, c.pix_fmt);
    Some((ogv, raw))
}

fn reference(dir: &Path, name: &str, file: &Path, pix_fmt: &str) -> Vec<u8> {
    let out = dir.join(format!("{name}-{}.yuv", file.extension().unwrap().to_str().unwrap()));
    assert!(ffmpeg(&["-i", file.to_str().unwrap(), "-fps_mode", "passthrough", "-f", "rawvideo", "-pix_fmt", pix_fmt, out.to_str().unwrap()]), "decoding {name} with FFmpeg");
    std::fs::read(&out).unwrap()
}

/// Decode the first video stream of `file` through the reader and the Theora decoder; every frame's
/// planes, one frame after another.
fn decode(file: &Path) -> (Vec<Vec<u8>>, u32, u32) {
    let mut demux = ironav_formats::open_demuxer(file).unwrap();
    let (i, stream) = demux.streams().iter().enumerate().find(|(_, s)| matches!(s.params, StreamParams::Video { .. })).map(|(i, s)| (i, s.clone())).unwrap();
    assert_eq!(stream.codec, ironav_core::CodecId::Theora);
    let StreamParams::Video { width, height, .. } = stream.params else { unreachable!() };
    let mut dec = ironav_codec::new_decoder_for(&stream).unwrap();
    let mut frames = Vec::new();
    let mut take = |out: Vec<Frame>| {
        for f in out {
            let Frame::Video(v) = f else { panic!("video expected") };
            assert_eq!((v.width, v.height), (width, height));
            frames.push(v.planes.concat());
        }
    };
    while let Some(p) = demux.read_packet().unwrap() {
        if p.stream == i {
            take(dec.decode(&p).unwrap());
        }
    }
    take(dec.flush().unwrap());
    (frames, width, height)
}

/// Compare IronAV's frames with FFmpeg's raw ones; the first difference, if any.
fn compare(got: &[Vec<u8>], want: &[u8]) -> Result<(), String> {
    let frame = got.first().map_or(1, |f| f.len());
    if want.len() != frame * got.len() {
        return Err(format!("{} frames of {frame} bytes against {} bytes from FFmpeg", got.len(), want.len()));
    }
    for (i, f) in got.iter().enumerate() {
        let w = &want[i * frame..(i + 1) * frame];
        if let Some(at) = f.iter().zip(w).position(|(a, b)| a != b) {
            let n = f.iter().zip(w).filter(|(a, b)| a != b).count();
            return Err(format!("frame {i} differs at byte {at} ({n} bytes differ)"));
        }
    }
    Ok(())
}

#[test]
fn theora_in_ogg_matches_ffmpeg() {
    if !have_ffmpeg() {
        eprintln!("skipped: no ffmpeg with libtheora");
        return;
    }
    let dir = scratch("ogg");
    let mut failures = Vec::new();
    for c in CASES {
        let Some((ogv, raw)) = make(&dir, c) else { panic!("encoding {} with FFmpeg", c.name) };
        let (frames, w, h) = decode(&ogv);
        assert_eq!((w, h), c.size, "{}", c.name);
        match compare(&frames, &raw) {
            Ok(()) => println!("{}: {} frames, byte-identical", c.name, frames.len()),
            Err(e) => {
                println!("{}: {e}", c.name);
                failures.push(format!("{}: {e}", c.name));
            }
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(failures.is_empty(), "{failures:#?}");
}

#[test]
fn theora_in_matroska_matches_ffmpeg() {
    if !have_ffmpeg() {
        eprintln!("skipped: no ffmpeg with libtheora");
        return;
    }
    let dir = scratch("mkv");
    let mut failures = Vec::new();
    for c in CASES.iter().filter(|c| matches!(c.name, "cif_q6_g12" | "odd_q6_g12" | "p444_odd_q9" | "p422_q6_g12")) {
        let Some((ogv, raw)) = make(&dir, c) else { panic!("encoding {} with FFmpeg", c.name) };
        let mkv = dir.join(format!("{}.mkv", c.name));
        assert!(ffmpeg(&["-i", ogv.to_str().unwrap(), "-c:v", "copy", mkv.to_str().unwrap()]), "copying {} into Matroska", c.name);
        let (frames, w, h) = decode(&mkv);
        assert_eq!((w, h), c.size, "{}", c.name);
        match compare(&frames, &raw) {
            Ok(()) => println!("{} (mkv): {} frames, byte-identical", c.name, frames.len()),
            Err(e) => failures.push(format!("{}: {e}", c.name)),
        }
        // the demuxer says the same about the stream in both containers
        let a = ironav_formats::open_demuxer(&ogv).unwrap().streams()[0].clone();
        let b = ironav_formats::open_demuxer(&mkv).unwrap().streams()[0].clone();
        assert_eq!((a.codec, &a.params), (b.codec, &b.params), "{}", c.name);
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(failures.is_empty(), "{failures:#?}");
}

fn read_all(file: &Path) -> Vec<Packet> {
    let mut d = ironav_formats::open_demuxer(file).unwrap();
    let mut v = Vec::new();
    while let Some(p) = d.read_packet().unwrap() {
        v.push(p);
    }
    v
}

#[test]
fn seeking_in_ogg_lands_on_a_key_frame() {
    if !have_ffmpeg() {
        eprintln!("skipped: no ffmpeg with libtheora");
        return;
    }
    let dir = scratch("seek");
    let c = Case { name: "seek", size: (176, 144), pix_fmt: "yuv420p", quality: 6, gop: 10, source: "testsrc2", frames: 60 };
    let (ogv, raw) = make(&dir, &c).unwrap();
    let all = read_all(&ogv);
    assert_eq!(all.len(), 60);
    let keys: Vec<usize> = (0..all.len()).filter(|&i| ironav_codec::theora::is_key_frame(&all[i].data)).collect();
    assert!(keys.len() >= 3 && keys[0] == 0, "key frames at {keys:?}");
    let frame_len = raw.len() / 60;
    for target in [0.0, 0.5, 1.2, 1.3, 2.0, 2.38, 9.0] {
        let mut d = ironav_formats::open_demuxer(&ogv).unwrap();
        let landed = d.seek(target).unwrap();
        let want_index = keys.iter().rev().find(|&&k| k as f64 / 25.0 <= target + 1e-6).copied().unwrap();
        assert!((landed - want_index as f64 / 25.0).abs() < 1e-6, "seek {target} landed on {landed}, key frame {want_index} expected");
        // the packets after the seek are the ones of a full read from there on, and decode to the same pictures
        let stream = d.streams()[0].clone();
        let mut dec = ironav_codec::new_decoder_for(&stream).unwrap();
        let mut n = want_index;
        while let Some(p) = d.read_packet().unwrap() {
            assert_eq!((p.pts, &p.data), (all[n].pts, &all[n].data), "seek {target}, packet {n}");
            for f in dec.decode(&p).unwrap() {
                let Frame::Video(v) = f else { panic!() };
                assert_eq!(v.planes.concat(), raw[n * frame_len..(n + 1) * frame_len], "seek {target}, frame {n}");
            }
            n += 1;
        }
        assert_eq!(n, 60);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn damaged_theora_is_an_error_not_a_panic() {
    if !have_ffmpeg() {
        eprintln!("skipped: no ffmpeg with libtheora");
        return;
    }
    let dir = scratch("damage");
    let mut failures = 0;
    for c in CASES.iter().filter(|c| matches!(c.name, "odd_q6_g12" | "p444_q6_g12" | "p422_q6_g12")) {
        let (ogv, _) = make(&dir, c).unwrap();
        let mut d = ironav_formats::open_demuxer(&ogv).unwrap();
        let stream = d.streams()[0].clone();
        let mut packets = Vec::new();
        while let Some(p) = d.read_packet().unwrap() {
            packets.push(p);
        }

        // truncated codec data: every length gives an error or a decoder, never a panic
        for cut in (0..stream.extradata.len()).step_by(37) {
            let mut s = stream.clone();
            s.extradata.truncate(cut);
            assert!(ironav_codec::new_decoder_for(&s).is_err(), "{} bytes of headers", cut);
        }
        // flipped bits in the headers
        let mut x = 0x1234_5678u32;
        let mut rng = move || {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x
        };
        for _ in 0..300 {
            let mut s = stream.clone();
            for _ in 0..1 + rng() % 4 {
                let at = rng() as usize % s.extradata.len();
                s.extradata[at] ^= 1 << (rng() % 8);
            }
            let _ = ironav_codec::new_decoder_for(&s).map(|mut dec| dec.decode(&packets[0]));
        }
        // truncated packets and flipped bits in them, after a good key frame
        let mut dec = ironav_codec::new_decoder_for(&stream).unwrap();
        for _ in 0..packets.len() {
            let _ = dec.decode(&packets[0]);
        }
        for i in 0..packets.len() {
            let p = &packets[i];
            for cut in [0, 1, 2, 3, 5, p.data.len() / 4, p.data.len() / 2, p.data.len().saturating_sub(1)] {
                let mut q = p.clone();
                q.data.truncate(cut);
                let _ = dec.decode(&packets[0]);
                if dec.decode(&q).is_err() {
                    failures += 1;
                }
            }
            for _ in 0..20 {
                let mut q = p.clone();
                for _ in 0..1 + rng() % 8 {
                    if q.data.is_empty() {
                        break;
                    }
                    let at = rng() as usize % q.data.len();
                    q.data[at] ^= 1 << (rng() % 8);
                }
                let _ = dec.decode(&packets[0]);
                let _ = dec.decode(&q);
                let _ = dec.decode(p);
            }
        }
        // random bytes as packets
        for len in [1usize, 7, 100, 3000] {
            let q = Packet { stream: 0, pts: 0, duration: 1, data: (0..len).map(|_| (rng() >> 11) as u8 & 0x3F).collect() };
            let _ = dec.decode(&q);
        }
        // a file cut off inside its pages
        let bytes = std::fs::read(&ogv).unwrap();
        for cut in (0..bytes.len()).step_by(bytes.len() / 40 + 1) {
            let f = dir.join("cut.ogv");
            std::fs::write(&f, &bytes[..cut]).unwrap();
            let _ = ironav_formats::open_demuxer(&f).map(|mut d| while let Ok(Some(_)) = d.read_packet() {});
        }
    }
    println!("{failures} damaged packets were refused");
    let _ = std::fs::remove_dir_all(&dir);
}
