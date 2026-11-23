//! Headless playback benchmark: no window, no sound device.
//!
//! ```text
//! cargo run --release -p ironav-player --example bench_decode -- [FILE] [--size 1920x1080] [--frames 60]
//!                                                                   [--hwaccel off|auto] [--convert-reps 10]
//! ```
//!
//! With no FILE it encodes a synthetic clip in-process (the software H.264 encoder, into a temp
//! MP4), then decodes it the way the player does. It prints one JSON object:
//!
//! * `first_frame_ms`: from opening the file to the first decoded picture;
//! * `decode_fps`: decoded pictures per second over the whole file;
//! * `convert_ms_per_frame`: the YUV to RGB conversion of one decoded picture on the CPU, for the
//!   old scalar converter (`reference`) and the current one (`fast`), with the speed-up.
//!
//! The GPU path does this conversion in a shader, so the CPU cost it removes is `fast` (or
//! `reference` before this change) plus the RGB to RGBA pass and the texture upload.

use ironav_core::{CodecId, Frame, Packet, PixelFormat, Rational, StreamInfo, StreamParams, VideoFrame};
use ironav_formats::{create_muxer, open_demuxer};
use ironav_player::{open_video_decoder, rgb_frame, rgb_frame_reference};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::time::Instant;

fn main() {
    if let Err(e) = run() {
        eprintln!("bench_decode: {e}");
        std::process::exit(1);
    }
}

fn arg(args: &[String], name: &str) -> Option<String> {
    args.iter().position(|a| a == name).and_then(|i| args.get(i + 1)).cloned()
}

fn run() -> Result<(), String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let file = args.first().filter(|a| !a.starts_with("--")).cloned();
    let (w, h) = arg(&args, "--size").and_then(|s| s.split_once('x').and_then(|(a, b)| Some((a.parse().ok()?, b.parse().ok()?)))).unwrap_or((1920u32, 1080u32));
    let frames: usize = arg(&args, "--frames").and_then(|s| s.parse().ok()).unwrap_or(60);
    let hwaccel = arg(&args, "--hwaccel").unwrap_or_else(|| "off".into());
    let reps: usize = arg(&args, "--convert-reps").and_then(|s| s.parse().ok()).unwrap_or(10);

    let mut synthetic = None;
    let (path, encode_secs) = match file {
        Some(f) => (PathBuf::from(f), None),
        None => {
            let p = std::env::temp_dir().join(format!("ironav-bench-{}-{w}x{h}.mp4", std::process::id()));
            let t = Instant::now();
            make_clip(&p, w, h, frames).map_err(|e| format!("could not make the synthetic clip: {e}"))?;
            synthetic = Some(p.clone());
            (p, Some(t.elapsed().as_secs_f64()))
        }
    };

    // Open and decode, like the player's video thread.
    let t0 = Instant::now();
    let mut demux = open_demuxer(&path).map_err(|e| e.message)?;
    let stream = demux.streams().iter().find(|s| matches!(s.params, StreamParams::Video { .. })).cloned().ok_or("no video stream")?;
    for s in demux.streams().to_vec() {
        if s.index != stream.index {
            demux.discard(s.index);
        }
    }
    let (mut dec, decoder) = open_video_decoder(&stream, &hwaccel).map_err(|e| e.message)?;
    let (mut first, mut count) = (None, 0usize);
    let mut last: Option<VideoFrame> = None;
    let mut decode_secs = 0.0;
    let mut take = |frames: Vec<Frame>, count: &mut usize, first: &mut Option<f64>| {
        for f in frames {
            if let Frame::Video(v) = f {
                *count += 1;
                first.get_or_insert_with(|| t0.elapsed().as_secs_f64() * 1000.0);
                last = Some(v);
            }
        }
    };
    let t_decode = Instant::now();
    while let Some(p) = demux.read_packet().map_err(|e| e.message)? {
        if p.stream != stream.index {
            continue;
        }
        let out = dec.decode(&p).map_err(|e| e.message)?;
        take(out, &mut count, &mut first);
    }
    take(dec.flush().map_err(|e| e.message)?, &mut count, &mut first);
    decode_secs += t_decode.elapsed().as_secs_f64();
    let last = last.ok_or("the file decoded no pictures")?;

    // The CPU conversion of one picture, old against new. Full size, then fitted to 1280x720 (what
    // the window's CPU fallback does for a 1080p source on a small window).
    let time = |f: &dyn Fn() -> (u32, u32, Vec<u8>)| -> f64 {
        let _ = f(); // warm up
        let t = Instant::now();
        for _ in 0..reps {
            std::hint::black_box(f());
        }
        t.elapsed().as_secs_f64() * 1000.0 / reps as f64
    };
    let ref_full = time(&|| rgb_frame_reference(&last, None, None));
    let fast_full = time(&|| rgb_frame(&last, None, None));
    let ref_fit = time(&|| rgb_frame_reference(&last, Some(1280), Some(720)));
    let fast_fit = time(&|| rgb_frame(&last, Some(1280), Some(720)));

    let out = json!({
        "source": if synthetic.is_some() { "synthetic".to_string() } else { path.display().to_string() },
        "codec": stream.codec.name(),
        "decoder": decoder,
        "size": [last.width, last.height],
        "pixel_format": format!("{:?}", last.pix_fmt),
        "frames_decoded": count,
        "encode_secs": encode_secs,
        "first_frame_ms": first,
        "decode_fps": count as f64 / decode_secs,
        "convert_ms_per_frame": {
            "full_size": { "reference": ref_full, "fast": fast_full, "speedup": ref_full / fast_full },
            "fit_1280x720": { "reference": ref_fit, "fast": fast_fit, "speedup": ref_fit / fast_fit },
        },
        "convert_reps": reps,
        "profile": if cfg!(debug_assertions) { "debug (numbers are not meaningful, use --release)" } else { "release" },
    });
    println!("{}", serde_json::to_string_pretty(&out).unwrap());
    if let Some(p) = synthetic {
        let _ = std::fs::remove_file(p);
    }
    Ok(())
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

fn make_clip(path: &Path, w: u32, h: u32, frames: usize) -> ironav_core::Result<()> {
    let params = StreamParams::Video { width: w, height: h, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(30, 1) };
    let mut enc = ironav_codec::new_encoder_with(CodecId::H264, &params, None, None)?;
    let info = StreamInfo {
        index: 0,
        codec: CodecId::H264,
        time_base: Rational::new(1, 30),
        duration: Some(frames as i64),
        bits: None,
        extradata: ironav_codec::encoder_extradata(CodecId::H264, &params),
        lead_in: ironav_codec::encoder_lead_in(CodecId::H264),
        params,
    };
    let mut mux = create_muxer(path, "mp4")?;
    mux.write_header(std::slice::from_ref(&info))?;
    let mut write = |ps: Vec<Packet>| -> ironav_core::Result<()> {
        for p in ps {
            mux.write_packet(&p)?;
        }
        Ok(())
    };
    for n in 0..frames {
        write(enc.encode(&Frame::Video(picture(w, h, n)))?)?;
    }
    write(enc.flush()?)?;
    mux.finish()
}
