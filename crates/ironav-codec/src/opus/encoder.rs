//! An Opus encoder: CELT mode, 48 kHz, 20 ms frames, constant bit rate, mono or stereo.
//!
//! The frame coding is in `celt/enc.rs`; this file turns a stream of samples into packets: the
//! pre-emphasis filter, buffering for the window's look-ahead, the TOC byte, and the end of the
//! stream.

use super::celt::enc::{FRAME, LOOKAHEAD};
use super::celt::Celt;
use super::rc::RangeEncoder;
use ironav_core::{Encoder, Error, ErrorCode, Frame, Packet, Result};

const RATE: u32 = 48_000;
/// The high-pass pre-emphasis the decoder undoes with its de-emphasis filter.
const EMPH_COEFF: f32 = 0.850_006_1;
/// The first frame and every this many after it code their band energies without prediction, so a
/// player that starts in the middle recovers.
const INTRA_PERIOD: u64 = 50;
/// Samples the decoder must drop from the start: the first frame's window has nothing to overlap with.
pub const PRE_SKIP: usize = LOOKAHEAD;

/// The `OpusHead` that describes a stream from this encoder to a container.
pub fn opus_head(channels: usize) -> Vec<u8> {
    let mut v = b"OpusHead".to_vec();
    v.push(1);
    v.push(channels as u8);
    v.extend((PRE_SKIP as u16).to_le_bytes());
    v.extend(RATE.to_le_bytes());
    v.extend(0i16.to_le_bytes());
    v.push(0); // channel mapping family 0
    v
}

pub struct OpusEncoder {
    celt: Celt,
    channels: usize,
    /// Bytes of a full packet, TOC included.
    bytes: usize,
    /// The previous input sample of each channel, for the pre-emphasis.
    prev: [f32; 2],
    /// Pre-emphasised samples not yet fully coded, from the start of the next frame.
    sig: Vec<Vec<f32>>,
    frames: u64,
    /// Samples per channel received.
    input: u64,
}

impl OpusEncoder {
    /// An encoder for `channels` (1 or 2) at about `bitrate` bits per second.
    pub fn new(sample_rate: u32, channels: usize, bitrate: u32) -> Result<Self> {
        if sample_rate != RATE {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("the Opus encoder takes 48000 Hz audio, not {sample_rate} Hz"))
                .hint("resample the audio to 48000 Hz first"));
        }
        if !(1..=2).contains(&channels) {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("the Opus encoder takes mono or stereo audio, not {channels} channels")));
        }
        // 20 ms of the bit rate, but never less than a few bytes or more than a frame can hold
        let bytes = (bitrate as usize * FRAME / RATE as usize / 8).clamp(8 * channels, 1275);
        Ok(Self { celt: Celt::new(channels, true), channels, bytes, prev: [0.0; 2], sig: vec![vec![0.0; LOOKAHEAD]; channels], frames: 0, input: 0 })
    }

    /// The bit rate used when none is asked for: 64 kb/s per channel.
    pub fn default_bitrate(channels: usize) -> u32 {
        64_000 * channels as u32
    }

    fn push(&mut self, c: usize, x: f32) {
        self.sig[c].push(x - EMPH_COEFF * self.prev[c]);
        self.prev[c] = x;
    }

    fn emit(&mut self, packets: &mut Vec<Packet>, duration: i64) -> Result<()> {
        let n = FRAME + LOOKAHEAD;
        let silent = self.sig.iter().all(|s| s[..n].iter().all(|&v| v == 0.0));
        // digital silence needs only the flag
        let size = if silent { 3 } else { self.bytes };
        let mut rc = RangeEncoder::new(size - 1);
        let refs: Vec<&[f32]> = self.sig.iter().map(|s| &s[..n]).collect();
        self.celt.encode_frame(&mut rc, &refs, silent, self.frames % INTRA_PERIOD == 0);
        let body = rc.finish().map_err(|_| Error::new(ErrorCode::Internal, "Opus: a frame did not fit its packet"))?;
        let mut data = Vec::with_capacity(size);
        // configuration 31 (CELT, full band, 20 ms), one frame per packet
        data.push(0xF8 | (((self.channels == 2) as u8) << 2));
        data.extend(body);
        packets.push(Packet { stream: 0, pts: self.frames as i64 * FRAME as i64, duration, data });
        self.frames += 1;
        for s in self.sig.iter_mut() {
            s.drain(..FRAME);
        }
        Ok(())
    }
}

impl Encoder for OpusEncoder {
    fn encode(&mut self, frame: &Frame) -> Result<Vec<Packet>> {
        let Frame::Audio(a) = frame else {
            return Err(Error::invalid_input("the Opus encoder received a video frame"));
        };
        if a.sample_rate != RATE || a.channels as usize != self.channels {
            return Err(Error::invalid_input("the audio changed sample rate or channel count mid-stream"));
        }
        let n = a.samples.len() / self.channels;
        for i in 0..n {
            for c in 0..self.channels {
                self.push(c, a.samples[i * self.channels + c] as f32);
            }
        }
        self.input += n as u64;
        let mut packets = Vec::new();
        while self.sig[0].len() >= FRAME + LOOKAHEAD {
            self.emit(&mut packets, FRAME as i64)?;
        }
        Ok(packets)
    }

    fn flush(&mut self) -> Result<Vec<Packet>> {
        let mut packets = Vec::new();
        if self.input == 0 {
            return Ok(packets);
        }
        // frames until every input sample has come out of the decoder, which is the pre-skip late
        let wanted = (self.input as usize + PRE_SKIP).div_ceil(FRAME) as u64;
        while self.frames < wanted {
            while self.sig[0].len() < FRAME + LOOKAHEAD {
                for c in 0..self.channels {
                    self.push(c, 0.0);
                }
            }
            let last = self.frames + 1 == wanted;
            // the final packet decodes to more than the stream holds; say how much to keep
            let duration = if last { (self.input as usize + PRE_SKIP - (wanted as usize - 1) * FRAME) as i64 } else { FRAME as i64 };
            self.emit(&mut packets, duration)?;
        }
        Ok(packets)
    }
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;
    use ironav_core::{AudioFrame, Decoder, StreamParams};

    fn music(n: usize, channels: usize) -> Vec<f64> {
        let mut s = 12345u32;
        let mut out = Vec::with_capacity(n * channels);
        for i in 0..n {
            let t = i as f64 / 48_000.0;
            let mut noise = |amp: f64| {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                ((s >> 8) as f64 / (1 << 23) as f64 - 1.0) * amp
            };
            let base = 0.3 * (2.0 * std::f64::consts::PI * 440.0 * t).sin() + 0.15 * (2.0 * std::f64::consts::PI * 1330.0 * t).sin() + 0.08 * (2.0 * std::f64::consts::PI * 5200.0 * t).sin();
            let l = base + noise(0.01);
            out.push(l);
            if channels == 2 {
                out.push(0.7 * base + 0.2 * (2.0 * std::f64::consts::PI * 880.0 * t).sin() + noise(0.01));
            }
        }
        out
    }

    pub fn round_trip(channels: usize, bitrate: u32, seconds: f64) -> (f64, usize) {
        let n = (48_000.0 * seconds) as usize;
        let input = music(n, channels);
        let mut enc = OpusEncoder::new(48_000, channels, bitrate).unwrap();
        let mut packets = Vec::new();
        // in awkward chunks, to exercise the buffering
        for chunk in input.chunks(1000 * channels) {
            let f = Frame::Audio(AudioFrame { sample_rate: 48_000, channels: channels as u16, pts: 0, samples: chunk.to_vec() });
            packets.extend(enc.encode(&f).unwrap());
        }
        packets.extend(enc.flush().unwrap());
        let params = StreamParams::Audio { sample_rate: 48_000, channels: channels as u16 };
        let mut dec = super::super::OpusDecoder::new(&opus_head(channels), &params).unwrap();
        let mut out = Vec::new();
        for p in &packets {
            for f in dec.decode(p).unwrap() {
                let Frame::Audio(a) = f else { panic!() };
                out.extend(a.samples);
            }
        }
        assert!(out.len() >= input.len(), "decoded {} samples for {}", out.len(), input.len());
        let (mut sig, mut err) = (0f64, 0f64);
        for (a, b) in input.iter().zip(&out) {
            sig += a * a;
            err += (a - b) * (a - b);
        }
        (10.0 * (sig / err.max(1e-30)).log10(), packets.iter().map(|p| p.data.len()).sum())
    }

    #[test]
    fn mono_round_trips_through_our_decoder() {
        let (snr, bytes) = round_trip(1, 64_000, 1.0);
        eprintln!("mono: {snr:.1} dB, {bytes} bytes");
        assert!(snr > 15.0, "{snr}");
    }

    #[test]
    fn stereo_round_trips_through_our_decoder() {
        let (snr, bytes) = round_trip(2, 128_000, 1.0);
        eprintln!("stereo: {snr:.1} dB, {bytes} bytes");
        assert!(snr > 15.0, "{snr}");
    }
}

#[cfg(test)]
mod sweep {
    use super::tests::round_trip as round_trip_pub;
    #[test]
    #[ignore]
    fn bitrate_sweep() {
        for br in [16_000, 32_000, 64_000, 96_000, 128_000, 192_000, 256_000, 320_000, 510_000] {
            let (snr, bytes) = round_trip_pub(2, br, 1.0);
            eprintln!("stereo {br}: {snr:.1} dB {bytes} bytes");
        }
        for br in [16_000, 32_000, 64_000, 128_000, 256_000] {
            let (snr, bytes) = round_trip_pub(1, br, 1.0);
            eprintln!("mono {br}: {snr:.1} dB {bytes} bytes");
        }
    }
}
