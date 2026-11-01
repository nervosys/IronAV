//! Size and speed of the software H.264 encoder on a raw yuv420p clip, for several reference counts
//! and quantisers. Ignored: it takes its input from the environment and writes an Annex B stream
//! per setting, which FFmpeg decodes to measure PSNR.
//!
//! `IRONAV_RAW` names the raw clip, `IRONAV_SIZE` its `WxH`, `IRONAV_FRAMES` its frame count,
//! `IRONAV_QPS` a comma-separated list of quantisers, `IRONAV_REFS` a comma-separated list of
//! reference counts, and `IRONAV_OUT` a directory for the streams (`refs{R}_qp{Q}.h264`).
//! Run with `cargo test --release -p ironav-codec --test h264_enc_refs -- --ignored --nocapture`.

use ironav_codec::h264::H264Encoder;
use ironav_core::{Encoder, Frame, Packet, PixelFormat, Rational, StreamParams, VideoFrame};
use std::time::Instant;

fn list(name: &str, default: &str) -> Vec<usize> {
    std::env::var(name).unwrap_or_else(|_| default.to_string()).split(',').map(|v| v.trim().parse().unwrap()).collect()
}

#[test]
#[ignore]
fn encoder_size_and_speed_by_references() {
    let Ok(raw) = std::env::var("IRONAV_RAW") else {
        eprintln!("set IRONAV_RAW to a raw yuv420p clip");
        return;
    };
    let size = std::env::var("IRONAV_SIZE").unwrap_or_else(|_| "1280x720".into());
    let (w, h): (usize, usize) = {
        let (a, b) = size.split_once('x').unwrap();
        (a.parse().unwrap(), b.parse().unwrap())
    };
    let frames: usize = std::env::var("IRONAV_FRAMES").ok().and_then(|v| v.parse().ok()).unwrap_or(60);
    let out = std::env::var("IRONAV_OUT").unwrap_or_else(|_| ".".into());
    let data = std::fs::read(&raw).unwrap();
    // IRONAV_BITS 10 reads the clip as yuv420p10le (two bytes a sample)
    let bytes = if std::env::var("IRONAV_BITS").is_ok_and(|v| v == "10") { 2 } else { 1 };
    let pix_fmt = if bytes == 2 { PixelFormat::Yuv420p10 } else { PixelFormat::Yuv420p };
    let fsize = w * h * 3 / 2 * bytes;
    assert!(data.len() >= fsize * frames, "the clip holds fewer than {frames} frames");
    let clip: Vec<VideoFrame> = (0..frames)
        .map(|t| {
            let f = &data[t * fsize..(t + 1) * fsize];
            let (y, c) = f.split_at(w * h * bytes);
            let (u, v) = c.split_at(w * h * bytes / 4);
            VideoFrame { width: w as u32, height: h as u32, pix_fmt, pts: t as i64, planes: vec![y.to_vec(), u.to_vec(), v.to_vec()] }
        })
        .collect();
    let params = StreamParams::Video { width: w as u32, height: h as u32, pix_fmt, frame_rate: Rational::new(25, 1) };
    for refs in list("IRONAV_REFS", "1,3") {
        for qp in list("IRONAV_QPS", "20,26,32,38") {
            let mut enc = H264Encoder::new(&params, Some(qp as u8), None).unwrap().with_refs(refs);
            let t = Instant::now();
            let mut pkts: Vec<Packet> = Vec::new();
            for f in &clip {
                pkts.extend(enc.encode(&Frame::Video(f.clone())).unwrap());
            }
            pkts.extend(enc.flush().unwrap());
            let secs = t.elapsed().as_secs_f64();
            let bytes: Vec<u8> = pkts.iter().flat_map(|p| p.data.iter().copied()).collect();
            std::fs::write(format!("{out}/refs{refs}_qp{qp}.h264"), &bytes).unwrap();
            eprintln!("refs {refs} qp {qp}: {} bytes, {:.1} fps ({:.2} s for {frames} frames)", bytes.len(), frames as f64 / secs, secs);
        }
    }
}
