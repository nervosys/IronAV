//! FLAC (Free Lossless Audio Codec), implemented from RFC 9639.
//!
//! * [`decoder`] handles all subframe types, stereo modes, wasted bits and 4-32 bit depths.
//! * [`encoder`] writes 8/12/16/20/24-bit streams with fixed and LPC prediction.
//! * [`header`] and [`crc`] are shared with the `.flac` container in `ironav-formats`.
//!
//! Each packet is exactly one FLAC frame.

pub mod bits;
pub mod crc;
pub mod decoder;
pub mod encoder;
pub mod header;

pub use decoder::{decode_frame, FlacDecoder};
pub use encoder::FlacEncoder;
pub use header::{parse_header, FrameHeader, HeaderError};

#[cfg(test)]
mod tests {
    use super::*;
    use ironav_core::{AudioFrame, Decoder, Encoder, Frame};

    fn signal(n: usize, ch: usize, bits: u32, seed: u64) -> Vec<f64> {
        let scale = (1i64 << (bits - 1)) as f64;
        let mut x = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        let mut out = Vec::with_capacity(n * ch);
        for i in 0..n {
            for c in 0..ch {
                x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
                let noise = ((x >> 40) as f64 / (1u64 << 24) as f64 - 0.5) * 0.02;
                let tone = (i as f64 * (0.03 + 0.011 * c as f64)).sin() * 0.6 + (i as f64 * 0.31).sin() * 0.1;
                let v = ((tone + noise) * scale).round().clamp(-scale, scale - 1.0);
                out.push(v / scale);
            }
        }
        out
    }

    fn roundtrip(n: usize, ch: u16, bits: u8, seed: u64) -> (usize, usize) {
        let samples = signal(n, ch as usize, bits as u32, seed);
        let mut enc = FlacEncoder::new(44_100, ch, Some(bits)).unwrap();
        let mut pkts = enc.encode(&Frame::Audio(AudioFrame { sample_rate: 44_100, channels: ch, pts: 0, samples: samples.clone() })).unwrap();
        pkts.extend(enc.flush().unwrap());
        let mut dec = FlacDecoder::new(44_100, ch, Some(bits));
        let mut got = Vec::new();
        let mut bytes = 0;
        for p in &pkts {
            bytes += p.data.len();
            let Frame::Audio(a) = dec.decode(p).unwrap().remove(0) else { panic!() };
            got.extend(a.samples);
        }
        assert_eq!(got.len(), samples.len(), "sample count");
        assert!(got == samples, "lossless mismatch for n={n} ch={ch} bits={bits}");
        (bytes, n * ch as usize * bits as usize / 8)
    }

    #[test]
    fn lossless_across_layouts_depths_and_lengths() {
        for &(n, ch, bits) in &[(10_000, 1, 16), (10_000, 2, 16), (9_999, 2, 24), (4096, 2, 8), (5000, 3, 16), (4097, 6, 24), (100, 2, 16), (1, 1, 16), (37, 2, 24)] {
            let (comp, raw) = roundtrip(n, ch, bits, n as u64 + ch as u64);
            if n >= 4096 && bits >= 16 {
                assert!(comp < raw, "no compression for n={n} ch={ch} bits={bits}: {comp} >= {raw}");
            }
        }
    }

    #[test]
    fn edge_signals_are_lossless() {
        for sig in [vec![0.0; 5000], vec![-1.0; 5000], (0..5000).map(|i| if i % 2 == 0 { 0.999969482421875 } else { -1.0 }).collect::<Vec<f64>>()] {
            let mut enc = FlacEncoder::new(48_000, 1, Some(16)).unwrap();
            let mut pk = enc.encode(&Frame::Audio(AudioFrame { sample_rate: 48_000, channels: 1, pts: 0, samples: sig.clone() })).unwrap();
            pk.extend(enc.flush().unwrap());
            let mut dec = FlacDecoder::new(48_000, 1, Some(16));
            let got: Vec<f64> = pk.iter().flat_map(|p| match dec.decode(p).unwrap().remove(0) { Frame::Audio(a) => a.samples, _ => unreachable!() }).collect();
            assert!(got == sig);
        }
    }

    #[test]
    fn corruption_is_detected_not_decoded() {
        let samples = signal(4096, 1, 16, 7);
        let mut enc = FlacEncoder::new(44_100, 1, Some(16)).unwrap();
        let mut pk = enc.encode(&Frame::Audio(AudioFrame { sample_rate: 44_100, channels: 1, pts: 0, samples })).unwrap();
        pk.extend(enc.flush().unwrap());
        let mut bad = pk[0].clone();
        let mid = bad.data.len() / 2;
        bad.data[mid] ^= 0x10;
        let e = FlacDecoder::new(44_100, 1, Some(16)).decode(&bad).expect_err("must fail");
        assert!(e.message.contains("CRC"), "{}", e.message);
    }
}
