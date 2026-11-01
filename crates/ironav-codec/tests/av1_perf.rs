//! Throughput of the software AV1 decoder alone, on an IVF file named by `IRONAV_AV1` (none is
//! checked in: make one with FFmpeg and libaom, e.g. `ffmpeg -f lavfi -i testsrc2=size=1280x720:rate=30
//! -frames:v 60 -c:v libaom-av1 -cpu-used 6 -crf 30 -aom-params enable-restoration=1:enable-cdef=1
//! av1.ivf`). If `IRONAV_AV1_REF` names FFmpeg's decode of it as raw planar video
//! (`ffmpeg -i av1.ivf -f rawvideo av1.yuv`), the output must match it byte for byte. For RGB
//! streams (matrix_coefficients 0, 4:4:4) the output is packed `rgb24`, so the reference is
//! `ffmpeg -i av1.ivf -f rawvideo -pix_fmt rgb24 av1.rgb`. `IRONAV_AV1_THREADS` sets the threads
//! of the post filters (the decoder's default otherwise; 1 runs them on the calling thread).
//! Run with `cargo test --release -p ironav-codec --test av1_perf -- --ignored --nocapture`.

use ironav_codec::av1::Av1Decoder;
use ironav_core::{Decoder, Frame, Packet};
use std::time::Instant;

/// The frames of an IVF file: a 32-byte file header, then each frame with a 12-byte header.
fn ivf_frames(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let hdr = u16::from_le_bytes([data[6], data[7]]) as usize;
    let mut at = hdr;
    while at + 12 <= data.len() {
        let n = u32::from_le_bytes([data[at], data[at + 1], data[at + 2], data[at + 3]]) as usize;
        at += 12;
        out.push(&data[at..(at + n).min(data.len())]);
        at += n;
    }
    out
}

#[test]
#[ignore]
fn av1_decode_throughput() {
    let Ok(path) = std::env::var("IRONAV_AV1") else {
        eprintln!("set IRONAV_AV1 to an IVF file of AV1 frames");
        return;
    };
    let data = std::fs::read(path).unwrap();
    let packets = ivf_frames(&data);
    let threads: Option<usize> = std::env::var("IRONAV_AV1_THREADS").ok().and_then(|v| v.parse().ok());
    let mut best = f64::MAX;
    let mut frames = Vec::new();
    for _ in 0..3 {
        let mut d = threads.map_or_else(|| Av1Decoder::new(&[]), |n| Av1Decoder::with_threads(&[], n));
        let t = Instant::now();
        frames.clear();
        for (i, p) in packets.iter().enumerate() {
            frames.extend(d.decode(&Packet { stream: 0, pts: i as i64, duration: 1, data: p.to_vec() }).unwrap());
        }
        best = best.min(t.elapsed().as_secs_f64());
    }
    eprintln!("{} pictures in {best:.3} s: {:.1} pictures/s", frames.len(), frames.len() as f64 / best);
    if let Ok(r) = std::env::var("IRONAV_AV1_REF") {
        let want = std::fs::read(r).unwrap();
        let mut got = Vec::with_capacity(want.len());
        for f in &frames {
            if let Frame::Video(v) = f {
                for p in &v.planes {
                    got.extend_from_slice(p);
                }
            }
        }
        if let Ok(o) = std::env::var("IRONAV_AV1_OUT") {
            std::fs::write(o, &got).unwrap();
        }
        assert_eq!(got.len(), want.len(), "the output is not the reference's size");
        let first = got.iter().zip(&want).position(|(a, b)| a != b);
        assert!(first.is_none(), "the output differs from the reference at byte {}", first.unwrap_or(0));
        eprintln!("identical to the reference");
    }
}
