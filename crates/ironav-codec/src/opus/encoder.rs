//! An Opus encoder: CELT mode, 48 kHz, frames of 2.5, 5, 10 or 20 ms, constant bit rate, mono or
//! stereo.
//!
//! The frame coding is in `celt/enc.rs`; this file turns a stream of samples into packets: the
//! pre-emphasis filter, buffering for the window's look-ahead, the TOC byte, and the end of the
//! stream. Besides the [`Encoder`] trait, which takes frames of any length, a caller that has its
//! own framing (a voice link, say) codes one frame at a time into its own buffer with
//! [`OpusEncoder::encode_frame_into`], which allocates nothing.

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
    rc: RangeEncoder,
    channels: usize,
    bitrate: u32,
    /// The frame's size code (0 to 3: 2.5 to 20 ms) and samples.
    size: i32,
    frame: usize,
    /// Bytes of a full packet, TOC included.
    bytes: usize,
    /// The previous input sample of each channel, for the pre-emphasis.
    prev: [f32; 2],
    /// Pre-emphasised samples not yet fully coded, from the start of the next frame.
    sig: Vec<Vec<f32>>,
    frames: u64,
    /// Frames coded with short blocks.
    pub transients: u64,
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
        let mut sig = vec![Vec::with_capacity(2 * FRAME + LOOKAHEAD); channels];
        for s in sig.iter_mut() {
            s.resize(LOOKAHEAD, 0.0);
        }
        Ok(Self {
            celt: Celt::new(channels, true),
            rc: RangeEncoder::new(1275),
            channels,
            bitrate,
            size: 3,
            frame: FRAME,
            bytes: Self::frame_bytes(bitrate, FRAME, channels),
            prev: [0.0; 2],
            sig,
            frames: 0,
            transients: 0,
            input: 0,
        })
    }

    /// A frame's worth of the bit rate, but never less than a few bytes or more than a frame can hold.
    fn frame_bytes(bitrate: u32, frame: usize, channels: usize) -> usize {
        (bitrate as usize * frame / RATE as usize / 8).clamp(8 * channels, 1275)
    }

    /// Frames of `ms` milliseconds: 2.5, 5, 10 or 20 (the default). Shorter frames lower the delay
    /// and cost quality at the same bit rate. Only before any audio.
    pub fn with_frame_ms(mut self, ms: f64) -> Result<Self> {
        let size = match ms {
            2.5 => 0,
            5.0 => 1,
            10.0 => 2,
            20.0 => 3,
            _ => return Err(Error::invalid_arg(format!("Opus frames are 2.5, 5, 10 or 20 ms, not {ms}")).alternatives(["2.5", "5", "10", "20"])),
        };
        if self.input > 0 {
            return Err(Error::invalid_arg("the frame length is set before any audio"));
        }
        self.size = size;
        self.frame = FRAME >> (3 - size);
        self.bytes = Self::frame_bytes(self.bitrate, self.frame, self.channels);
        Ok(self)
    }

    /// Samples per channel in a frame (at 48 kHz).
    pub fn frame_samples(&self) -> usize {
        self.frame
    }

    /// Bytes in every packet but those of digital silence (3), TOC included.
    pub fn packet_bytes(&self) -> usize {
        self.bytes
    }

    /// The delay the encoder adds, in samples at 48 kHz: a frame of buffering and the window's
    /// look-ahead (the decoder drops the look-ahead as the stream's pre-skip). A frame passed
    /// to [`encode_frame_into`](Self::encode_frame_into) comes out of a decoder this much later.
    pub fn delay_samples(&self) -> usize {
        self.frame + LOOKAHEAD
    }

    /// [`delay_samples`](Self::delay_samples) in milliseconds.
    pub fn delay_ms(&self) -> f64 {
        self.delay_samples() as f64 * 1000.0 / RATE as f64
    }

    /// Codes one frame: `pcm` holds exactly [`frame_samples`](Self::frame_samples) interleaved
    /// samples per channel (-1 to 1), and the packet, TOC byte first, is written to the start of
    /// `out` (at least [`packet_bytes`](Self::packet_bytes) long). Returns the packet's length.
    /// Allocates nothing. It cannot follow audio passed to [`Encoder::encode`] that has not made a
    /// whole frame.
    pub fn encode_frame_into(&mut self, pcm: &[f32], out: &mut [u8]) -> Result<usize> {
        if pcm.len() != self.frame * self.channels {
            return Err(Error::invalid_arg(format!("a frame is {} samples ({} per channel), not {}", self.frame * self.channels, self.frame, pcm.len())));
        }
        if out.len() < self.bytes {
            return Err(Error::invalid_arg(format!("the packet needs {} bytes, the buffer has {}", self.bytes, out.len())));
        }
        if self.sig[0].len() != LOOKAHEAD {
            return Err(Error::invalid_arg("audio from encode() is waiting for the rest of its frame"));
        }
        for i in 0..self.frame {
            for c in 0..self.channels {
                self.push(c, pcm[i * self.channels + c]);
            }
        }
        self.input += self.frame as u64;
        self.code(out)
    }

    /// The bit rate used when none is asked for: 64 kb/s per channel.
    pub fn default_bitrate(channels: usize) -> u32 {
        64_000 * channels as u32
    }

    fn push(&mut self, c: usize, x: f32) {
        self.sig[c].push(x - EMPH_COEFF * self.prev[c]);
        self.prev[c] = x;
    }

    /// Codes the frame at the front of the buffered signal into `out`, and returns the packet's length.
    fn code(&mut self, out: &mut [u8]) -> Result<usize> {
        let n = self.frame + LOOKAHEAD;
        let silent = self.sig.iter().all(|s| s[..n].iter().all(|&v| v == 0.0));
        // digital silence needs only the flag
        let size = if silent { 3 } else { self.bytes };
        self.rc.reset(size - 1);
        let (a, b) = (&self.sig[0][..n], self.sig.get(1).map_or(&[][..], |s| &s[..n]));
        let both = [a, b];
        let refs = &both[..self.channels];
        self.celt.encode_frame(&mut self.rc, refs, self.size, silent, self.frames % INTRA_PERIOD == 0);
        self.rc.finish_in_place().map_err(|_| Error::new(ErrorCode::Internal, "Opus: a frame did not fit its packet"))?;
        // configurations 28 to 31: CELT, full band, 2.5 to 20 ms; one frame per packet
        out[0] = ((28 + self.size as u8) << 3) | (((self.channels == 2) as u8) << 2);
        out[1..size].copy_from_slice(self.rc.bytes());
        self.frames += 1;
        self.transients += self.celt.was_transient() as u64;
        for s in self.sig.iter_mut() {
            s.drain(..self.frame);
        }
        Ok(size)
    }

    fn emit(&mut self, packets: &mut Vec<Packet>, duration: i64) -> Result<()> {
        let pts = self.frames as i64 * self.frame as i64;
        let mut data = vec![0u8; self.bytes];
        let len = self.code(&mut data)?;
        data.truncate(len);
        packets.push(Packet { stream: 0, pts, duration, data });
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
        while self.sig[0].len() >= self.frame + LOOKAHEAD {
            self.emit(&mut packets, self.frame as i64)?;
        }
        Ok(packets)
    }

    fn flush(&mut self) -> Result<Vec<Packet>> {
        let mut packets = Vec::new();
        if self.input == 0 {
            return Ok(packets);
        }
        // frames until every input sample has come out of the decoder, which is the pre-skip late
        let frame = self.frame;
        let wanted = (self.input as usize + PRE_SKIP).div_ceil(frame) as u64;
        while self.frames < wanted {
            while self.sig[0].len() < frame + LOOKAHEAD {
                for c in 0..self.channels {
                    self.push(c, 0.0);
                }
            }
            let last = self.frames + 1 == wanted;
            // the final packet decodes to more than the stream holds; say how much to keep
            let duration = if last { (self.input as usize + PRE_SKIP - (wanted as usize - 1) * frame) as i64 } else { frame as i64 };
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

#[cfg(test)]
mod stress {
    use super::*;
    use ironav_core::{AudioFrame, Decoder, StreamParams};

    fn encode_all(channels: usize, bitrate: u32, input: &[f64]) -> Vec<Packet> {
        let mut enc = OpusEncoder::new(48_000, channels, bitrate).unwrap();
        let mut packets = Vec::new();
        for chunk in input.chunks(777 * channels) {
            let f = Frame::Audio(AudioFrame { sample_rate: 48_000, channels: channels as u16, pts: 0, samples: chunk.to_vec() });
            packets.extend(enc.encode(&f).unwrap());
        }
        packets.extend(enc.flush().unwrap());
        packets
    }

    fn decode_all(channels: usize, packets: &[Packet]) -> usize {
        let params = StreamParams::Audio { sample_rate: 48_000, channels: channels as u16 };
        let mut dec = super::super::OpusDecoder::new(&opus_head(channels), &params).unwrap();
        let mut n = 0;
        for p in packets {
            for f in dec.decode(p).unwrap() {
                let Frame::Audio(a) = f else { panic!() };
                assert!(a.samples.iter().all(|v| v.is_finite() && v.abs() <= 1.0 + 1e-6), "samples out of range");
                n += a.samples.len() / channels;
            }
        }
        n
    }

    #[test]
    fn extreme_signals_and_rates_never_overflow_a_packet() {
        let mut s = 99u32;
        let mut rnd = || {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            (s >> 8) as f64 / (1 << 23) as f64 - 1.0
        };
        let n = 48_000 / 2;
        for channels in [1usize, 2] {
            let noise: Vec<f64> = (0..n * channels).map(|_| rnd()).collect();
            let square: Vec<f64> = (0..n * channels).map(|i| if (i / channels / 50) % 2 == 0 { 1.0 } else { -1.0 }).collect();
            let clicks: Vec<f64> = (0..n * channels).map(|i| if (i / channels) % 4801 == 0 { 1.0 } else { 0.0 }).collect();
            let quiet: Vec<f64> = (0..n * channels).map(|_| rnd() * 1e-6).collect();
            let silence = vec![0.0; n * channels];
            for (name, sig) in [("noise", &noise), ("square", &square), ("clicks", &clicks), ("quiet", &quiet), ("silence", &silence)] {
                for bitrate in [6_000u32, 8_000, 16_000, 64_000, 256_000, 510_000] {
                    let packets = encode_all(channels, bitrate, sig);
                    let got = decode_all(channels, &packets);
                    assert!(got >= n, "{name} {channels}ch {bitrate}: {got} samples");
                    if name == "silence" {
                        assert!(packets.iter().all(|p| p.data.len() == 3), "silence is three bytes a frame");
                    }
                }
            }
        }
    }

    #[test]
    fn tiny_inputs() {
        for len in [0usize, 1, 2, 119, 120, 121, 959, 960, 961] {
            let sig: Vec<f64> = (0..len).map(|i| (i as f64 * 0.3).sin() * 0.5).collect();
            let packets = encode_all(1, 64_000, &sig);
            if len == 0 {
                assert!(packets.is_empty());
                continue;
            }
            let got = decode_all(1, &packets);
            assert!(got >= len && got < len + 960, "{len}: {got}");
        }
    }
}

#[cfg(test)]
mod attacks {
    use super::*;
    use ironav_core::{AudioFrame, Decoder, StreamParams};

    /// A quiet tone with a hard-hit decaying note every 0.3 s: the kind of signal that short blocks are for.
    fn hits(n: usize) -> Vec<f64> {
        (0..n)
            .map(|i| {
                let t = i as f64 / 48_000.0;
                let since = t % 0.3;
                let hit = if since < 0.15 && t >= 0.3 { 0.8 * (-since * 40.0).exp() * (2.0 * std::f64::consts::PI * 1800.0 * since).sin() } else { 0.0 };
                0.003 * (2.0 * std::f64::consts::PI * 200.0 * t).sin() + hit
            })
            .collect()
    }

    #[test]
    fn attacks_use_short_blocks_and_decode_well() {
        let input = hits(48_000 * 2);
        let mut enc = OpusEncoder::new(48_000, 1, 64_000).unwrap();
        let mut packets = Vec::new();
        for chunk in input.chunks(960) {
            packets.extend(enc.encode(&Frame::Audio(AudioFrame { sample_rate: 48_000, channels: 1, pts: 0, samples: chunk.to_vec() })).unwrap());
        }
        packets.extend(enc.flush().unwrap());
        assert!(enc.transients >= 5 && enc.transients <= 12, "{} transient frames for 6 hits", enc.transients);
        let params = StreamParams::Audio { sample_rate: 48_000, channels: 1 };
        let mut dec = super::super::OpusDecoder::new(&opus_head(1), &params).unwrap();
        let mut out = Vec::new();
        for p in &packets {
            for f in dec.decode(p).unwrap() {
                let Frame::Audio(a) = f else { panic!() };
                out.extend(a.samples);
            }
        }
        let (mut sig, mut err) = (0f64, 0f64);
        for (a, b) in input.iter().zip(&out) {
            sig += a * a;
            err += (a - b) * (a - b);
        }
        let snr = 10.0 * (sig / err).log10();
        eprintln!("hits: {snr:.1} dB, {} short-block frames", enc.transients);
        assert!(snr > 15.0, "{snr}");
    }
}
