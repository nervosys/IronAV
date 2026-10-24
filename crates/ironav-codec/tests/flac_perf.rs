//! Throughput of the FLAC encoder and decoder alone, with no container or file I/O.
//! Run with `cargo test --release -p ironav-codec --test flac_perf -- --ignored --nocapture`.
//! The checksum shows whether a change altered the bytes produced.

use ironav_codec::flac::{FlacDecoder, FlacEncoder};
use ironav_core::{AudioFrame, Decoder, Encoder, Frame};
use std::time::Instant;

/// A 220 Hz tone with Gaussian-like noise at about -40 dB, which is harder to code than a clean tone.
fn music(seconds: usize, rate: usize) -> Vec<f64> {
    let mut x = 7u64;
    let mut uniform = move || {
        x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (x >> 40) as f64 / (1u64 << 24) as f64 - 0.5
    };
    let mut out = Vec::with_capacity(seconds * rate * 2);
    for i in 0..seconds * rate {
        let noise = (uniform() + uniform() + uniform() + uniform()) * 0.0346;
        let s = (std::f64::consts::TAU * 220.0 * i as f64 / rate as f64).sin() * 0.3 + noise;
        out.push(s);
        out.push(s * 0.8);
    }
    out
}

#[test]
#[ignore]
fn flac_throughput() {
    let seconds = 180;
    let samples = music(seconds, 44_100);
    let mut best = f64::MAX;
    let mut packets = Vec::new();
    for _ in 0..5 {
        let t = Instant::now();
        let mut enc = FlacEncoder::new(44_100, 2, Some(16)).unwrap();
        let mut p = Vec::new();
        for chunk in samples.chunks(4096 * 2) {
            p.extend(enc.encode(&Frame::Audio(AudioFrame { sample_rate: 44_100, channels: 2, pts: 0, samples: chunk.to_vec() })).unwrap());
        }
        p.extend(enc.flush().unwrap());
        let dt = t.elapsed().as_secs_f64();
        println!("  run: {:.0} ms", dt * 1e3);
        best = best.min(dt);
        packets = p;
    }
    let bytes: usize = packets.iter().map(|p| p.data.len()).sum();
    let sum = packets.iter().flat_map(|p| p.data.iter()).fold(0xcbf29ce484222325u64, |h, &b| (h ^ b as u64).wrapping_mul(0x100000001b3));
    println!("encode: {:.0} ms best of 5 ({:.0}x realtime), {} bytes, fnv {:016x}", best * 1e3, seconds as f64 / best, bytes, sum);

    let mut best = f64::MAX;
    for _ in 0..5 {
        let t = Instant::now();
        let mut dec = FlacDecoder::new(44_100, 2, Some(16));
        let mut n = 0;
        for p in &packets {
            for f in dec.decode(p).unwrap() {
                if let Frame::Audio(a) = f {
                    n += a.samples.len();
                }
            }
        }
        assert_eq!(n, samples.len());
        best = best.min(t.elapsed().as_secs_f64());
    }
    println!("decode: {:.0} ms best of 5 ({:.0}x realtime)", best * 1e3, seconds as f64 / best);
}
