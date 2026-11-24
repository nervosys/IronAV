//! Start-up and seek benchmark: no window, no sound device.
//!
//! ```text
//! cargo run --release -p ironav-player --example bench_seek -- [--size 640x360] [--frames 600] [--keyint 60]
//!                                                              [--reps 5] [--long-hours 2]
//! ```
//!
//! It encodes a synthetic H.264 clip in-process (the software encoder, an IDR picture every
//! `--keyint` frames), writes it as an MP4 and as a Matroska file, and prints one JSON object:
//!
//! * `open_to_first_frame_ms`: `Player::open` until the first picture is available, per container;
//! * `seek_ms`: the time from asking for a seek until the picture is available, in both modes
//!   (`accurate`: the picture at the target; `keyframe`: the keyframe at or before it), at several
//!   positions inside a GOP, with the timestamp of the picture that came up (the median of `--reps`);
//! * `mkv_open_ms`: opening a long synthetic Matroska file (the clip's packets repeated for
//!   `--long-hours` of media): `open_demuxer` (indexes every frame), and `open_demuxer_lazy` (what the
//!   player uses) with its Cues, and with the Cues made unreadable so it falls back to scanning the whole file.

use ironav_core::{CodecId, Frame, Packet, PixelFormat, Rational, StreamInfo, StreamParams, VideoFrame};
use ironav_formats::{create_muxer, open_demuxer, open_demuxer_lazy};
use ironav_player::{AudioMode, OpenOptions, Player, SeekMode};
use serde_json::{json, Value};
use std::path::Path;
use std::time::{Duration, Instant};

fn main() {
    if let Err(e) = run() {
        eprintln!("bench_seek: {e}");
        std::process::exit(1);
    }
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

const FPS: i64 = 30;

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (w, h) = arg(&args, "--size").and_then(|s| s.split_once('x').and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))).unwrap_or((640u32, 360u32));
    let frames: usize = arg(&args, "--frames").and_then(|s| s.parse().ok()).unwrap_or(600);
    let keyint: u64 = arg(&args, "--keyint").and_then(|s| s.parse().ok()).unwrap_or(60);
    let reps: usize = arg(&args, "--reps").and_then(|s| s.parse().ok()).unwrap_or(5);
    let hours: f64 = arg(&args, "--long-hours").and_then(|s| s.parse().ok()).unwrap_or(2.0);

    let dir = std::env::temp_dir().join(format!("ironav-bench-seek-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let t = Instant::now();
    let (info, packets) = encode(w, h, frames, keyint).map_err(|e| format!("encode: {}", e.message))?;
    let encode_secs = t.elapsed().as_secs_f64();
    let duration = frames as f64 / FPS as f64;

    let (mut first, mut seeks) = (serde_json::Map::new(), serde_json::Map::new());
    for name in ["mp4", "mkv"] {
        let path = dir.join(format!("clip.{name}"));
        write_file(&path, name, &info, packets.iter().cloned()).map_err(|e| e.message)?;
        let (ms, seek) = measure(&path, duration, keyint as f64 / FPS as f64, reps)?;
        first.insert(name.into(), json!(ms));
        seeks.insert(name.into(), seek);
    }
    let long = long_mkv(&dir, &info, &packets, hours)?;

    let out = json!({
        "codec": "h264", "size": [w, h], "fps": FPS, "frames": frames, "keyint_frames": keyint, "duration_secs": duration,
        "encode_secs": encode_secs, "reps": reps,
        "open_to_first_frame_ms": first,
        "seek_ms": seeks,
        "mkv_open_ms": long,
        "profile": if cfg!(debug_assertions) { "debug (numbers are not meaningful, use --release)" } else { "release" },
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
    let _ = std::fs::remove_dir_all(&dir);
    Ok(())
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

/// Wait for a picture to be available; returns its timestamp.
fn wait_frame(p: &Player) -> Result<f64, String> {
    let t = Instant::now();
    loop {
        if let Some(f) = p.frame() {
            return Ok(f.time);
        }
        if let Some(e) = p.error() {
            return Err(e.message);
        }
        if t.elapsed() > Duration::from_secs(30) {
            return Err("timed out waiting for a picture".into());
        }
        std::thread::sleep(Duration::from_micros(50));
    }
}

fn opts() -> OpenOptions {
    OpenOptions { audio: AudioMode::Off, hwaccel: "off".into(), autoplay: false, ..Default::default() }
}

const MODES: &[&str] = &["accurate", "keyframe"];

fn seek(p: &Player, t: f64, mode: &str) {
    p.seek_mode(t, if mode == "keyframe" { SeekMode::Keyframe } else { SeekMode::Accurate });
}

fn measure(path: &Path, duration: f64, gop: f64, reps: usize) -> Result<(f64, Value), String> {
    let p = path.display().to_string();
    let mut opens = Vec::new();
    for _ in 0..reps {
        let t = Instant::now();
        let player = Player::open(&p, &opts()).map_err(|e| e.message)?;
        wait_frame(&player)?;
        opens.push(t.elapsed().as_secs_f64() * 1000.0);
    }
    let first = median(&mut opens);

    let player = Player::open(&p, &opts()).map_err(|e| e.message)?;
    wait_frame(&player)?;
    // targets 0.9 of the way into a GOP, early, in the middle and late in the file
    let targets: Vec<f64> = [0.2, 0.5, 0.8].iter().map(|f| ((duration * f / gop).floor() + 0.9) * gop).collect();
    let mut rows = Vec::new();
    for &target in &targets {
        let mut modes = serde_json::Map::new();
        for mode in MODES {
            let (mut ms, mut shown) = (Vec::new(), 0.0);
            for _ in 0..reps {
                // go somewhere else first so each seek is a real jump
                seek(&player, 0.0, "accurate");
                wait_frame(&player)?;
                let t = Instant::now();
                seek(&player, target, mode);
                shown = wait_frame(&player)?;
                ms.push(t.elapsed().as_secs_f64() * 1000.0);
            }
            modes.insert((*mode).into(), json!({ "median_ms": median(&mut ms), "frame_time": shown }));
        }
        rows.push(json!({ "target": target, "modes": modes }));
    }
    Ok((first, json!(rows)))
}

fn long_mkv(dir: &Path, info: &StreamInfo, packets: &[Packet], hours: f64) -> Result<Value, String> {
    let per_loop = packets.len() as i64;
    let loops = ((hours * 3600.0 * FPS as f64) / per_loop as f64).ceil() as i64;
    let path = dir.join("long.mkv");
    let mut long = info.clone();
    long.duration = Some(loops * per_loop);
    write_file(&path, "mkv", &long, (0..loops).flat_map(|l| packets.iter().map(move |p| Packet { pts: p.pts + l * per_loop, ..p.clone() }))).map_err(|e| e.message)?;
    let size_mb = std::fs::metadata(&path).map(|m| m.len() as f64 / 1e6).unwrap_or(0.0);
    let open = |p: &Path, lazy: bool| -> Result<f64, String> {
        let mut v = Vec::new();
        for _ in 0..3 {
            let t = Instant::now();
            let d = if lazy { open_demuxer_lazy(p) } else { open_demuxer(p) }.map_err(|e| e.message)?;
            std::hint::black_box(d.streams().len());
            v.push(t.elapsed().as_secs_f64() * 1000.0);
        }
        Ok(median(&mut v))
    };
    let scan_all = open(&path, false)?;
    let with = open(&path, true)?;
    // make the Cues unreadable: the SeekHead entry that names them gets another id, so a reader
    // that looks for the Cues through it finds nothing there
    let broken = dir.join("long-nocues.mkv");
    let mut bytes = std::fs::read(&path).map_err(|e| e.to_string())?;
    let cues_id = [0x1C, 0x53, 0xBB, 0x6B];
    let head = bytes.len().min(4096);
    let mut hidden = false;
    if let Some(at) = bytes[..head].windows(4).position(|w| w == cues_id) {
        bytes[at..at + 4].copy_from_slice(&[0x12, 0x54, 0xC3, 0x67]); // the Tags id
        hidden = true;
    }
    std::fs::write(&broken, &bytes).map_err(|e| e.to_string())?;
    let without = open(&broken, true)?;
    Ok(json!({ "media_hours": hours, "video_frames": loops * per_loop, "file_mb": size_mb, "open_demuxer_scans_all": scan_all, "open_demuxer_lazy_with_cues": with, "open_demuxer_lazy_without_cues": without, "seekhead_found": hidden }))
}

/// A moving gradient with some texture, so the encoder has real work and the picture is not flat.
fn picture(w: u32, h: u32, n: usize) -> VideoFrame {
    let (wu, hu) = (w as usize, h as usize);
    let (cw, ch) = (wu.div_ceil(2), hu.div_ceil(2));
    let mut y = vec![0u8; wu * hu];
    for (j, row) in y.chunks_exact_mut(wu).enumerate() {
        for (i, p) in row.iter_mut().enumerate() {
            let v = ((i + n * 4) / 8 + j / 6) as u32 * 3 + ((i ^ j) & 7) as u32;
            *p = (16 + v % 220) as u8;
        }
    }
    let mut u = vec![128u8; cw * ch];
    let mut v = vec![128u8; cw * ch];
    for (j, (ur, vr)) in u.chunks_exact_mut(cw).zip(v.chunks_exact_mut(cw)).enumerate() {
        for (i, (a, b)) in ur.iter_mut().zip(vr).enumerate() {
            *a = (64 + (i + n * 2) * 128 / cw.max(1)) as u8;
            *b = (64 + j * 128 / ch.max(1)) as u8;
        }
    }
    VideoFrame { width: w, height: h, pix_fmt: PixelFormat::Yuv420p, pts: n as i64, planes: vec![y, u, v] }
}

fn encode(w: u32, h: u32, frames: usize, keyint: u64) -> ironav_core::Result<(StreamInfo, Vec<Packet>)> {
    let params = StreamParams::Video { width: w, height: h, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(FPS, 1) };
    let mut enc = ironav_codec::encoders::H264Encoder::new(&params, None, None)?.with_keyint(keyint);
    let info = StreamInfo {
        index: 0,
        codec: CodecId::H264,
        time_base: Rational::new(1, FPS),
        duration: Some(frames as i64),
        bits: None,
        extradata: ironav_codec::encoder_extradata(CodecId::H264, &params),
        lead_in: ironav_codec::encoder_lead_in(CodecId::H264),
        params,
    };
    let mut packets = Vec::new();
    for n in 0..frames {
        packets.extend(ironav_core::Encoder::encode(&mut enc, &Frame::Video(picture(w, h, n)))?);
    }
    packets.extend(ironav_core::Encoder::flush(&mut enc)?);
    Ok((info, packets))
}

fn write_file(path: &Path, format: &str, info: &StreamInfo, packets: impl Iterator<Item = Packet>) -> ironav_core::Result<()> {
    let mut mux = create_muxer(path, format)?;
    mux.write_header(std::slice::from_ref(info))?;
    for p in packets {
        mux.write_packet(&p)?;
    }
    mux.finish()
}
