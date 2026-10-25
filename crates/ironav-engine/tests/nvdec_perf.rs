//! Decode throughput of the NVDEC path alone: no output file, no conversion.
//! `IRONAV_PERF_MP4=clip.mp4 cargo test --release -p ironav-engine --test nvdec_perf -- --ignored --nocapture`

use ironav_core::{MediaType, Packet};
use ironav_formats::open_demuxer;
use ironav_hwaccel::{new_video_decoder, Backend};
use std::time::Instant;

#[test]
#[ignore]
fn nvdec_decode_only() {
    let Ok(path) = std::env::var("IRONAV_PERF_MP4") else { return };
    let mut d = open_demuxer(std::path::Path::new(&path)).unwrap();
    let info = d.streams().iter().find(|s| s.params.media_type() == MediaType::Video).unwrap().clone();
    let mut packets: Vec<Packet> = Vec::new();
    while let Some(p) = d.read_packet().unwrap() {
        if p.stream == info.index {
            packets.push(p);
        }
    }
    let mut best = f64::MAX;
    let mut frames = 0;
    for _ in 0..5 {
        let t = Instant::now();
        let mut dec = new_video_decoder(Backend::Cuda, info.codec, &info.params).unwrap();
        frames = 0;
        println!("  decoder created after {:.0} ms", t.elapsed().as_secs_f64() * 1e3);
        let mut first = None;
        for p in &packets {
            frames += dec.decode(p).unwrap().len();
            if first.is_none() && frames > 0 {
                first = Some(t.elapsed().as_secs_f64());
            }
        }
        frames += dec.flush().unwrap().len();
        let dt = t.elapsed().as_secs_f64();
        println!("  run: {:.0} ms (first frame after {:.0} ms)", dt * 1e3, first.unwrap_or(0.0) * 1e3);
        best = best.min(dt);
    }
    println!("nvdec: {frames} frames, {:.0} ms best of 5 ({:.0} fps)", best * 1e3, frames as f64 / best);
}
