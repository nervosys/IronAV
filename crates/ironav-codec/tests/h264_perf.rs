//! Throughput of the software H.264 decoder alone, on an Annex B stream named by `IRONAV_H264`
//! (none is checked in: make one with FFmpeg, e.g. `ffmpeg -f lavfi -i testsrc2=size=1280x720:rate=30
//! -frames:v 240 -c:v libx264 -bf 3 -bsf:v h264_mp4toannexb bf.h264`). If `IRONAV_H264_REF` names
//! FFmpeg's decode of it as raw planar video (`ffmpeg -i bf.h264 -f rawvideo bf.yuv`), the output
//! must match it byte for byte.
//! Run with `cargo test --release -p ironav-codec --test h264_perf -- --ignored --nocapture`.

use ironav_codec::h264::H264Decoder;
use ironav_core::{Decoder, Frame, Packet};
use std::time::Instant;

#[test]
#[ignore]
fn h264_decode_throughput() {
    let Ok(path) = std::env::var("IRONAV_H264") else {
        eprintln!("set IRONAV_H264 to an Annex B H.264 file");
        return;
    };
    let data = std::fs::read(path).unwrap();
    let mut best = f64::MAX;
    let mut frames = Vec::new();
    for _ in 0..3 {
        let mut d = H264Decoder::new();
        let t = Instant::now();
        // the whole stream as one packet: the decoder finds the pictures in it
        frames = d.decode(&Packet { stream: 0, pts: 0, duration: 0, data: data.clone() }).unwrap();
        frames.extend(d.flush().unwrap());
        best = best.min(t.elapsed().as_secs_f64());
    }
    eprintln!("{} pictures in {best:.3} s: {:.1} pictures/s", frames.len(), frames.len() as f64 / best);
    if let Ok(r) = std::env::var("IRONAV_H264_REF") {
        let want = std::fs::read(r).unwrap();
        let mut got = Vec::with_capacity(want.len());
        for f in &frames {
            if let Frame::Video(v) = f {
                for p in &v.planes {
                    got.extend_from_slice(p);
                }
            }
        }
        assert_eq!(got.len(), want.len(), "the output is not the reference's size");
        let first = got.iter().zip(&want).position(|(a, b)| a != b);
        assert!(first.is_none(), "the output differs from the reference at byte {}", first.unwrap_or(0));
        eprintln!("identical to the reference");
    }
}
