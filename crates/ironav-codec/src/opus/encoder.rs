//! An Opus encoder at 48 kHz, mono or stereo: CELT mode (the default) with frames of 2.5, 5, 10 or
//! 20 ms at a constant bit rate, or SILK mode for speech, wideband, with frames of 10 or 20 ms at a
//! variable rate around the one asked for (stereo input is coded as its mono mix).
//!
//! The frame coding is in `celt/enc.rs` and `silk_enc.rs`; this file turns a stream of samples into
//! packets: the pre-emphasis filter, or for SILK the resampling to 16 kHz, buffering for the
//! look-ahead, the TOC byte, and the end of the stream. Besides the [`Encoder`] trait, which takes frames of any length, a caller that has its
//! own framing (a voice link, say) codes one frame at a time into its own buffer with
//! [`OpusEncoder::encode_frame_into`], which allocates nothing.

use super::celt::enc::{FRAME, LOOKAHEAD};
use super::celt::prefilter::Prefilter;
use super::celt::Celt;
use super::rc::{Coder, RangeEncoder};
use super::silk_enc::{self, SilkEncoder};
use crate::voice::StreamResampler;
use ironav_core::{Encoder, Error, ErrorCode, Frame, Packet, Result};

const RATE: u32 = 48_000;
/// The high-pass pre-emphasis the decoder undoes with its de-emphasis filter.
const EMPH_COEFF: f32 = 0.850_006_1;
/// The first frame and every this many after it code their band energies without prediction, so a
/// player that starts in the middle recovers.
const INTRA_PERIOD: u64 = 50;
/// Samples the decoder must drop from the start: the first frame's window has nothing to overlap with.
pub const PRE_SKIP: usize = LOOKAHEAD;

/// Samples (at 16 kHz) of zeros before the input in SILK mode, so that each frame of input brings
/// a whole frame with its look-ahead: the look-ahead and the resampler's 1 ms.
const SILK_LEAD: usize = silk_enc::LOOKAHEAD + 16;
/// Samples (at 48 kHz) a decoder must drop from the start of a SILK stream: the lead above, and
/// the decoder's own two samples and its upsampler's eleven (at 16 kHz).
pub const SILK_PRE_SKIP: usize = 3 * (SILK_LEAD + 2 + 11);

/// How much the CELT input of a hybrid frame waits, so that its delay is the SILK side's.
const HYBRID_SHIFT: usize = SILK_PRE_SKIP - PRE_SKIP;
/// The first CELT band of a hybrid frame: 8 kHz, where SILK's wideband ends.
const HYBRID_START: usize = 17;

/// What an [`OpusEncoder`] codes with.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum OpusMode {
    /// CELT, full band: music and anything else, at any bit rate.
    #[default]
    Celt,
    /// SILK, wideband (audio up to 8 kHz): speech, at low bit rates (about 8 to 30 kb/s).
    Silk,
    /// Hybrid, full band: SILK up to 8 kHz and CELT above, at a variable rate; speech with its
    /// high frequencies (about 16 to 40 kb/s).
    Hybrid,
}

/// The SILK side: the resampler to 16 kHz and the signal waiting to be coded.
struct SilkPath {
    enc: SilkEncoder,
    rs: StreamResampler,
    /// 16 kHz samples, from `silk_enc::PAST` before the next frame.
    buf: Vec<f32>,
    mono: Vec<f32>,
}

/// The `OpusHead` that describes a stream from a CELT-mode encoder to a container
/// ([`OpusEncoder::head`] for any mode).
pub fn opus_head(channels: usize) -> Vec<u8> {
    head_with(channels, PRE_SKIP)
}

fn head_with(channels: usize, pre_skip: usize) -> Vec<u8> {
    let mut v = b"OpusHead".to_vec();
    v.push(1);
    v.push(channels as u8);
    v.extend((pre_skip as u16).to_le_bytes());
    v.extend(RATE.to_le_bytes());
    v.extend(0i16.to_le_bytes());
    v.push(0); // channel mapping family 0
    v
}

pub struct OpusEncoder {
    celt: Celt,
    rc: RangeEncoder,
    pre: Prefilter,
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
    silk: Option<Box<SilkPath>>,
    mode: OpusMode,
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
            pre: Prefilter::default(),
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
            silk: None,
            mode: OpusMode::Celt,
        })
    }

    /// Codes in `mode`. Only before any audio; SILK takes frames of 10 or 20 ms.
    pub fn with_mode(mut self, mode: OpusMode) -> Result<Self> {
        if self.input > 0 {
            return Err(Error::invalid_arg("the mode is set before any audio"));
        }
        if mode != OpusMode::Celt && self.size < 2 {
            return Err(Error::invalid_arg("SILK and hybrid frames are 10 or 20 ms").alternatives(["10", "20"]));
        }
        self.silk = match mode {
            OpusMode::Celt => None,
            OpusMode::Silk | OpusMode::Hybrid => {
                let mut buf = Vec::with_capacity(silk_enc::PAST + 2 * 320 + SILK_LEAD);
                buf.resize(silk_enc::PAST + SILK_LEAD, 0.0);
                Some(Box::new(SilkPath { enc: SilkEncoder::new(), rs: StreamResampler::new(RATE, 16_000, 1, FRAME)?, buf, mono: Vec::with_capacity(FRAME) }))
            }
        };
        // a hybrid frame's CELT side is mono (the TOC's stereo flag would ask for stereo SILK too)
        let (cch, lead) = match mode {
            OpusMode::Hybrid => (1, LOOKAHEAD + HYBRID_SHIFT),
            _ => (self.channels, LOOKAHEAD),
        };
        self.celt = Celt::new(cch, true);
        self.sig = vec![vec![0.0; lead]; cch];
        self.mode = mode;
        self.bytes = self.max_bytes();
        Ok(self)
    }

    /// The mode the encoder codes with.
    pub fn mode(&self) -> OpusMode {
        self.mode
    }

    /// The most bytes a packet takes: CELT's are all this size, SILK's and hybrid's twice the bit
    /// rate's share at most.
    fn max_bytes(&self) -> usize {
        match self.mode {
            OpusMode::Celt => Self::frame_bytes(self.bitrate, self.frame, self.channels),
            OpusMode::Silk | OpusMode::Hybrid => (2 * self.bitrate as usize * self.frame / RATE as usize / 8 + 1).clamp(80, 1275),
        }
    }

    /// Samples (at 48 kHz) a decoder drops from the start of the stream.
    pub fn pre_skip(&self) -> usize {
        if self.silk.is_some() { SILK_PRE_SKIP } else { PRE_SKIP }
    }

    /// The `OpusHead` that describes this encoder's stream to a container.
    pub fn head(&self) -> Vec<u8> {
        head_with(self.channels, self.pre_skip())
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
        if self.silk.is_some() && size < 2 {
            return Err(Error::invalid_arg(format!("SILK and hybrid frames are 10 or 20 ms, not {ms}")).alternatives(["10", "20"]));
        }
        self.size = size;
        self.frame = FRAME >> (3 - size);
        self.bytes = self.max_bytes();
        Ok(self)
    }

    /// Samples per channel in a frame (at 48 kHz).
    pub fn frame_samples(&self) -> usize {
        self.frame
    }

    /// Bytes in every packet but those of digital silence (3), TOC included; in SILK mode, the
    /// most a packet takes.
    pub fn packet_bytes(&self) -> usize {
        self.bytes
    }

    /// The delay the encoder adds, in samples at 48 kHz: a frame of buffering and the window's
    /// look-ahead (the decoder drops the look-ahead as the stream's pre-skip). A frame passed
    /// to [`encode_frame_into`](Self::encode_frame_into) comes out of a decoder this much later.
    pub fn delay_samples(&self) -> usize {
        self.frame + self.pre_skip()
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
        self.encode_with(pcm.len(), |i| pcm[i], out)
    }

    /// [`encode_frame_into`](Self::encode_frame_into) for 16-bit samples.
    pub fn encode_frame_into_i16(&mut self, pcm: &[i16], out: &mut [u8]) -> Result<usize> {
        self.encode_with(pcm.len(), |i| pcm[i] as f32 / 32768.0, out)
    }

    fn encode_with(&mut self, len: usize, get: impl Fn(usize) -> f32, out: &mut [u8]) -> Result<usize> {
        if len != self.frame * self.channels {
            return Err(Error::invalid_arg(format!("a frame is {} samples ({} per channel), not {len}", self.frame * self.channels, self.frame)));
        }
        if out.len() < self.bytes {
            return Err(Error::invalid_arg(format!("the packet needs {} bytes, the buffer has {}", self.bytes, out.len())));
        }
        if let Some(s) = &mut self.silk {
            if !self.input.is_multiple_of(self.frame as u64) {
                return Err(Error::invalid_arg("audio from encode() is waiting for the rest of its frame"));
            }
            s.mono.clear();
            for i in 0..self.frame {
                s.mono.push((0..self.channels).map(|c| get(i * self.channels + c)).sum::<f32>() / self.channels as f32);
            }
            s.rs.push(&s.mono)?;
            Self::pull(s);
            if self.mode == OpusMode::Hybrid {
                for &m in &s.mono {
                    self.sig[0].push(m - EMPH_COEFF * self.prev[0]);
                    self.prev[0] = m;
                }
            }
            self.input += self.frame as u64;
            return if self.mode == OpusMode::Hybrid { self.code_hybrid(out) } else { self.code_silk(out) };
        }
        if self.sig[0].len() != LOOKAHEAD {
            return Err(Error::invalid_arg("audio from encode() is waiting for the rest of its frame"));
        }
        for i in 0..self.frame {
            for c in 0..self.channels {
                self.push(c, get(i * self.channels + c));
            }
        }
        self.input += self.frame as u64;
        self.code(out)
    }

    /// The bit rate used when none is asked for: 64 kb/s per channel.
    pub fn default_bitrate(channels: usize) -> u32 {
        64_000 * channels as u32
    }

    /// Moves what the resampler has ready to the SILK buffer, within full scale: the decoder's
    /// output stops there, and an input it cannot follow would make the coder chase it.
    fn pull(s: &mut SilkPath) {
        let have = s.buf.len();
        let n = s.rs.ready();
        s.buf.resize(have + n, 0.0);
        s.rs.pull(&mut s.buf[have..]);
        for v in &mut s.buf[have..] {
            *v = v.clamp(-1.0, 1.0);
        }
    }

    /// Codes the SILK frame at the front of the buffer into `out`, and returns the packet's length.
    fn code_silk(&mut self, out: &mut [u8]) -> Result<usize> {
        let s = self.silk.as_mut().expect("SILK mode");
        let fl = self.frame / 3;
        let need = silk_enc::PAST + fl + silk_enc::LOOKAHEAD;
        if s.buf.len() < need {
            return Err(Error::new(ErrorCode::Internal, "Opus: the SILK frame is not all here"));
        }
        let size = self.bytes;
        self.rc.reset(size - 1);
        let target = self.bitrate as f64 * self.frame as f64 / RATE as f64 - 8.0;
        let used = s.enc.encode(&mut self.rc, &s.buf[..need], fl / 80, target, ((size - 1) * 8) as i32);
        self.rc.finish_in_place().map_err(|_| Error::new(ErrorCode::Internal, "Opus: a frame did not fit its packet"))?;
        // the frame ends where the coder's bits do: any more would read as a redundancy frame
        let len = (used as usize).div_ceil(8);
        debug_assert!(self.rc.bytes()[len..].iter().all(|&b| b == 0));
        // configurations 8 and 9: SILK, wideband, 10 or 20 ms; one mono frame per packet
        out[0] = (8 + self.size as u8 - 2) << 3;
        out[1..1 + len].copy_from_slice(&self.rc.bytes()[..len]);
        s.buf.drain(..fl);
        self.frames += 1;
        Ok(1 + len)
    }

    /// Codes a hybrid frame (SILK, then CELT from 8 kHz up, in one range coder) into `out`, and
    /// returns the packet's length.
    fn code_hybrid(&mut self, out: &mut [u8]) -> Result<usize> {
        let s = self.silk.as_mut().expect("hybrid mode");
        let fl = self.frame / 3;
        let need = silk_enc::PAST + fl + silk_enc::LOOKAHEAD;
        let n = self.frame + LOOKAHEAD;
        if s.buf.len() < need || self.sig[0].len() < n {
            return Err(Error::new(ErrorCode::Internal, "Opus: the hybrid frame is not all here"));
        }
        // the high band takes a steady share, its energies and some folded detail; SILK takes the
        // rest, as much as it needs, and the frame ends after both
        let cap = self.bytes;
        self.rc.reset(cap - 1);
        let celt_bps = 3000.0 + 0.25 * (self.bitrate as f64 - 16_000.0).max(0.0);
        let per_frame = |bps: f64| bps * self.frame as f64 / RATE as f64;
        let celt_bits = per_frame(celt_bps) as i32;
        let target = per_frame(self.bitrate as f64 - celt_bps) - 8.0;
        let used = s.enc.encode(&mut self.rc, &s.buf[..need], fl / 80, target, ((cap - 1) * 8) as i32 - celt_bits - 12);
        let size = (1 + (used + 12 + celt_bits) as usize).div_ceil(8).clamp(3, cap);
        self.rc.shrink(size - 1);
        let total = ((size - 1) * 8) as i32;
        // no redundant CELT frame
        if used + 37 <= total {
            self.rc.log(12, 0);
        }
        let intra = self.frames.is_multiple_of(INTRA_PERIOD);
        self.celt.encode_frame(&mut self.rc, &[&self.sig[0][..n]], self.size, false, intra, None, HYBRID_START);
        self.rc.finish_in_place().map_err(|_| Error::new(ErrorCode::Internal, "Opus: a frame did not fit its packet"))?;
        // configurations 14 and 15: hybrid, full band, 10 or 20 ms; one mono frame per packet
        out[0] = (14 + self.size as u8 - 2) << 3;
        out[1..size].copy_from_slice(self.rc.bytes());
        s.buf.drain(..fl);
        self.sig[0].drain(..self.frame);
        self.frames += 1;
        self.transients += self.celt.was_transient() as u64;
        Ok(size)
    }

    /// Whether a whole frame is waiting to be coded.
    fn frame_ready(&self) -> bool {
        match &self.silk {
            Some(s) => s.buf.len() >= silk_enc::PAST + self.frame / 3 + silk_enc::LOOKAHEAD,
            None => self.sig[0].len() >= self.frame + LOOKAHEAD,
        }
    }

    /// Takes `n` frames of interleaved input from `get`.
    fn take(&mut self, n: usize, get: impl Fn(usize) -> f32) -> Result<()> {
        match &mut self.silk {
            Some(s) => {
                for start in (0..n).step_by(FRAME) {
                    s.mono.clear();
                    for i in start..n.min(start + FRAME) {
                        s.mono.push((0..self.channels).map(|c| get(i * self.channels + c)).sum::<f32>() / self.channels as f32);
                    }
                    s.rs.push(&s.mono)?;
                    Self::pull(s);
                    if self.mode == OpusMode::Hybrid {
                        for &m in &s.mono {
                            self.sig[0].push(m - EMPH_COEFF * self.prev[0]);
                            self.prev[0] = m;
                        }
                    }
                }
            }
            None => {
                for i in 0..n {
                    for c in 0..self.channels {
                        self.push(c, get(i * self.channels + c));
                    }
                }
            }
        }
        Ok(())
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
        // the pitch pre-filter needs room for its parameters; off, it fades out
        let enabled = !silent && size - 1 > 12 * self.channels;
        let pitch = match &mut self.sig[..] {
            [a] => self.pre.run(&mut [&mut a[..n]], self.frame, enabled, size - 1),
            [a, b] => self.pre.run(&mut [&mut a[..n], &mut b[..n]], self.frame, enabled, size - 1),
            _ => unreachable!("one or two channels"),
        };
        let (a, b) = (&self.sig[0][..n], self.sig.get(1).map_or(&[][..], |s| &s[..n]));
        let both = [a, b];
        let refs = &both[..self.channels];
        self.celt.encode_frame(&mut self.rc, refs, self.size, silent, self.frames % INTRA_PERIOD == 0, pitch, 0);
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
        let len = match self.mode {
            OpusMode::Celt => self.code(&mut data)?,
            OpusMode::Silk => self.code_silk(&mut data)?,
            OpusMode::Hybrid => self.code_hybrid(&mut data)?,
        };
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
        let mut packets = Vec::new();
        // a frame at a time, so the SILK buffer never holds more than one and its look-ahead
        for start in (0..n).step_by(self.frame) {
            let m = (n - start).min(self.frame);
            let base = start * self.channels;
            self.take(m, |i| a.samples[base + i] as f32)?;
            while self.frame_ready() {
                self.emit(&mut packets, self.frame as i64)?;
            }
        }
        self.input += n as u64;
        Ok(packets)
    }

    fn flush(&mut self) -> Result<Vec<Packet>> {
        let mut packets = Vec::new();
        if self.input == 0 {
            return Ok(packets);
        }
        // frames until every input sample has come out of the decoder, which is the pre-skip late
        let frame = self.frame;
        let skip = self.pre_skip();
        let wanted = (self.input as usize + skip).div_ceil(frame) as u64;
        while self.frames < wanted {
            while !self.frame_ready() {
                self.take(frame, |_| 0.0)?;
            }
            let last = self.frames + 1 == wanted;
            // the final packet decodes to more than the stream holds; say how much to keep
            let duration = if last { (self.input as usize + skip - (wanted as usize - 1) * frame) as i64 } else { frame as i64 };
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

    /// Speech through SILK mode and our decoder: the SNR at the best alignment, that alignment
    /// (0 when the pre-skip is right), and the bit rate.
    fn silk_round_trip(channels: usize, ms: f64, bitrate: u32) -> (f64, i64, f64) {
        let (snr, lag, bps, _) = mode_round_trip(OpusMode::Silk, channels, ms, bitrate, false);
        (snr, lag, bps)
    }

    /// Speech (with hiss above 8 kHz if `hiss`) through `mode` and our decoder: the SNR at the best alignment,
    /// that alignment, the bit rate, and the share of the input's energy above 8 kHz that the
    /// output has.
    fn mode_round_trip(mode: OpusMode, channels: usize, ms: f64, bitrate: u32, hiss: bool) -> (f64, i64, f64, f64) {
        let mut mono = super::super::silk_enc::tests::speechlike(96_000, 48_000.0);
        if hiss {
            // sibilance: noise from 10 to 14 kHz, in bursts
            let mut s = 99u32;
            let mut prev = 0f32;
            for (i, v) in mono.iter_mut().enumerate() {
                s = s.wrapping_mul(1664525).wrapping_add(1013904223);
                let w = (s >> 9) as f32 / (1 << 23) as f32 - 0.5;
                let hp = w - prev;
                prev = w;
                let burst = if (i / 4800) % 3 == 1 { 0.08 } else { 0.0 };
                *v += burst * hp * (2.0 * std::f32::consts::PI * 12_000.0 * i as f32 / 48_000.0).cos();
            }
        }
        let input: Vec<f64> = mono.iter().flat_map(|&v| std::iter::repeat_n(v as f64, channels)).collect();
        let mut enc = OpusEncoder::new(48_000, channels, bitrate).unwrap().with_mode(mode).unwrap().with_frame_ms(ms).unwrap();
        let mut packets = Vec::new();
        for chunk in input.chunks(1000 * channels) {
            let f = Frame::Audio(AudioFrame { sample_rate: 48_000, channels: channels as u16, pts: 0, samples: chunk.to_vec() });
            packets.extend(enc.encode(&f).unwrap());
        }
        packets.extend(enc.flush().unwrap());
        let config = if mode == OpusMode::Silk { 8 } else { 14 } + (ms == 20.0) as u8;
        assert!(packets.iter().all(|p| p.data.len() <= enc.packet_bytes() && p.data[0] >> 3 == config));
        let params = StreamParams::Audio { sample_rate: 48_000, channels: channels as u16 };
        let mut dec = super::super::OpusDecoder::new(&enc.head(), &params).unwrap();
        let mut out = Vec::new();
        for p in &packets {
            for f in dec.decode(p).unwrap() {
                let Frame::Audio(a) = f else { panic!() };
                out.extend(a.samples.iter().step_by(channels).copied());
            }
        }
        assert!(out.len() >= mono.len(), "decoded {} samples for {}", out.len(), mono.len());
        let snr_at = |lag: i64| {
            let (mut sig, mut err) = (0f64, 0f64);
            for n in 100..mono.len() - 100 {
                let (a, b) = (mono[n] as f64, out[(n as i64 + lag) as usize]);
                sig += a * a;
                err += (a - b) * (a - b);
            }
            10.0 * (sig / err.max(1e-30)).log10()
        };
        let lag = (-30..=30).max_by(|&a, &b| snr_at(a).partial_cmp(&snr_at(b)).unwrap()).unwrap();
        let bytes: usize = packets.iter().map(|p| p.data.len()).sum();
        // energy above 9 kHz, in and out: a windowed-sinc high-pass
        let taps: Vec<f64> = (-64i32..=64)
            .map(|k| {
                let t = k as f64;
                let lp = if k == 0 { 2.0 * 9_000.0 / 48_000.0 } else { (2.0 * std::f64::consts::PI * 9_000.0 / 48_000.0 * t).sin() / (std::f64::consts::PI * t) };
                let w = 0.5 + 0.5 * (std::f64::consts::PI * t / 65.0).cos();
                (if k == 0 { 1.0 } else { 0.0 }) - lp * w
            })
            .collect();
        let high = |x: &dyn Fn(usize) -> f64| (200..mono.len() - 200).map(|n| taps.iter().enumerate().map(|(k, c)| c * x(n + k - 64)).sum::<f64>().powi(2)).sum::<f64>();
        let hf = high(&|n| out[(n as i64 + lag) as usize]) / high(&|n| mono[n] as f64);
        (snr_at(lag), lag, bytes as f64 * 8.0 / 2.0, hf)
    }

    #[test]
    fn hybrid_mode_round_trips_with_the_high_band() {
        for (ms, rate) in [(20.0, 24_000), (10.0, 32_000)] {
            let (snr, lag, bps, hf) = mode_round_trip(OpusMode::Hybrid, 1, ms, rate, true);
            let (silk_snr, _, _, silk_hf) = mode_round_trip(OpusMode::Silk, 1, ms, rate, true);
            eprintln!("hybrid {ms} ms at {rate}: {snr:.1} dB at lag {lag}, {:.1} kb/s, high band {hf:.2} (SILK alone {silk_snr:.1} dB, {silk_hf:.2})", bps / 1000.0);
            assert_eq!(lag, 0, "the two layers are out of step, or the pre-skip is off");
            assert!(snr > 8.0, "{snr}");
            assert!((bps / rate as f64 - 1.0).abs() < 0.2, "{bps} for {rate}");
            assert!(hf > 0.3 && hf > 3.0 * silk_hf, "{hf} vs {silk_hf}");
        }
        // a stereo input is coded as its mix, the TOC mono
        let (snr, lag, _, _) = mode_round_trip(OpusMode::Hybrid, 2, 20.0, 24_000, true);
        assert!(snr > 8.0 && lag == 0, "{snr} {lag}");
    }

    #[test]
    fn silk_mode_round_trips_through_our_decoder() {
        for (channels, ms, rate) in [(1, 20.0, 12_000), (1, 20.0, 20_000), (1, 10.0, 16_000), (2, 20.0, 16_000)] {
            let (snr, lag, bps) = silk_round_trip(channels, ms, rate);
            eprintln!("SILK {channels} ch {ms} ms at {rate}: {snr:.1} dB at lag {lag}, {:.1} kb/s", bps / 1000.0);
            assert_eq!(lag, 0, "the pre-skip is off");
            assert!(snr > 8.0, "{snr}");
            assert!((bps / rate as f64 - 1.0).abs() < 0.3, "{bps} for {rate}");
        }
    }

    #[test]
    fn silk_mode_keeps_its_rate_on_clipped_input() {
        let mut enc = OpusEncoder::new(48_000, 1, 16_000).unwrap().with_mode(OpusMode::Silk).unwrap();
        let x: Vec<f32> = super::super::silk_enc::tests::speechlike(96_000, 48_000.0).iter().map(|v| v * 4.0).collect();
        let mut buf = vec![0u8; enc.packet_bytes()];
        let mut bytes = 0;
        for f in x.chunks_exact(enc.frame_samples()) {
            bytes += enc.encode_frame_into(f, &mut buf).unwrap();
        }
        let bps = bytes as f64 * 8.0 / 2.0;
        eprintln!("clipped: {:.1} kb/s", bps / 1000.0);
        assert!(bps < 16_000.0 * 1.4, "{bps}");
    }

    #[test]
    fn silk_mode_frame_at_a_time() {
        let mut enc = OpusEncoder::new(48_000, 1, 16_000).unwrap().with_mode(OpusMode::Silk).unwrap();
        assert!(OpusEncoder::new(48_000, 1, 16_000).unwrap().with_frame_ms(5.0).unwrap().with_mode(OpusMode::Silk).is_err());
        assert_eq!(enc.mode(), OpusMode::Silk);
        let x = super::super::silk_enc::tests::speechlike(48_000, 48_000.0);
        let mut buf = vec![0u8; enc.packet_bytes()];
        let mut sizes = Vec::new();
        for f in x.chunks_exact(enc.frame_samples()) {
            sizes.push(enc.encode_frame_into(f, &mut buf).unwrap());
        }
        assert_eq!(sizes.len(), 50);
        assert!(sizes.iter().all(|&n| n > 2 && n <= enc.packet_bytes()), "{sizes:?}");
    }
}
