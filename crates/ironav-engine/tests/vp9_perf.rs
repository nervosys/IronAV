//! Decode throughput of the software VP9 decoder alone: no output file, no conversion.
//! `IRONAV_PERF_WEBM=clip.webm cargo test --release -p ironav-engine --test vp9_perf -- --ignored --nocapture`
//! `IRONAV_VP9_THREADS=1` decodes the tile columns one after another, for comparison.

use ironav_core::{MediaType, Packet};
use ironav_formats::open_demuxer;
use std::time::Instant;

#[test]
#[ignore]
fn vp9_decode_only() {
    let Ok(path) = std::env::var("IRONAV_PERF_WEBM") else { return };
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
    for _ in 0..7 {
        let t = Instant::now();
        let mut dec = ironav_codec::new_decoder_for(&info).unwrap();
        frames = 0;
        for p in &packets {
            frames += dec.decode(p).unwrap().len();
        }
        frames += dec.flush().unwrap().len();
        let dt = t.elapsed().as_secs_f64();
        println!("  run: {:.0} ms", dt * 1e3);
        best = best.min(dt);
    }
    println!("vp9: {frames} frames, {:.0} ms best of 7 ({:.0} fps)", best * 1e3, frames as f64 / best);
}
