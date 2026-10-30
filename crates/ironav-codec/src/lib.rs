//! Software codecs and filters, plus the registries that describe them.
//!
//! The registries ([`implemented`], [`FILTERS`]) are the single source of truth for
//! "what can IronAV actually do"; the ontology and the planner both read them.

pub mod aac;
pub mod change;
pub mod flac;
pub mod h264;
pub mod hevc;
pub mod vp9;
pub mod jpeg;
mod filters;
pub mod mp3;
pub mod opus;
pub mod vorbis;
pub mod vp8;
mod pcm;
mod pixconv;
mod rawvideo;
mod resample;
mod scale;

pub use filters::{build_filter, FilterSpec, ParamSpec, FILTERS};
pub use pixconv::{convert as convert_pixel_format, Matrix};
pub use scale::resize_frame;

use ironav_core::{CodecId, Decoder, Encoder, Error, ErrorCode, MediaType, Result, StreamParams};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct CodecImpl {
    pub codec: CodecId,
    pub decode: bool,
    pub encode: bool,
    pub implementation: &'static str,
}

/// Whether a GPU decoder, when one is usable, should be chosen over the software decoder.
/// NVDEC cannot decode 10-bit H.264, so those streams go to the software decoder.
pub fn prefers_hardware_decode(c: CodecId, params: &ironav_core::StreamParams) -> bool {
    c == CodecId::Hevc || c == CodecId::H264 && !matches!(params, ironav_core::StreamParams::Video { pix_fmt: ironav_core::PixelFormat::Yuv420p10 | ironav_core::PixelFormat::Yuv422p10 | ironav_core::PixelFormat::Yuv444p10, .. })
}

pub fn implemented() -> Vec<CodecImpl> {
    CodecId::ALL
        .iter()
        .filter_map(|&c| {
            if c == CodecId::Aac {
                return Some(CodecImpl { codec: c, decode: true, encode: true, implementation: "ironav-aac" });
            }
            if c == CodecId::Mp3 {
                return Some(CodecImpl { codec: c, decode: true, encode: true, implementation: "ironav-mp3" });
            }
            if c == CodecId::Opus {
                return Some(CodecImpl { codec: c, decode: true, encode: true, implementation: "ironav-opus" });
            }
            if c == CodecId::Vorbis {
                return Some(CodecImpl { codec: c, decode: true, encode: true, implementation: "ironav-vorbis" });
            }
            if c == CodecId::Mjpeg {
                return Some(CodecImpl { codec: c, decode: true, encode: true, implementation: "ironav-jpeg" });
            }
            if c == CodecId::H264 {
                return Some(CodecImpl { codec: c, decode: true, encode: true, implementation: "ironav-h264" });
            }
            if c == CodecId::Hevc {
                return Some(CodecImpl { codec: c, decode: true, encode: false, implementation: "ironav-hevc" });
            }
            if c == CodecId::Vp8 {
                return Some(CodecImpl { codec: c, decode: true, encode: false, implementation: "ironav-vp8" });
            }
            if c == CodecId::Vp9 {
                return Some(CodecImpl { codec: c, decode: true, encode: false, implementation: "ironav-vp9" });
            }
            let implementation = if c.sample_format().is_some() {
                "ironav-pcm"
            } else if c == CodecId::RawVideo {
                "ironav-rawvideo"
            } else if c == CodecId::Flac {
                "ironav-flac"
            } else {
                return None;
            };
            Some(CodecImpl { codec: c, decode: true, encode: true, implementation })
        })
        .collect()
}

pub fn is_implemented(codec: CodecId, encode: bool) -> bool {
    implemented().iter().any(|i| i.codec == codec && if encode { i.encode } else { i.decode })
}

fn unsupported(codec: CodecId, what: &str) -> Error {
    let alts: Vec<_> = implemented()
        .into_iter()
        .filter(|i| i.codec.media_type() == codec.media_type())
        .map(|i| i.codec.name())
        .collect();
    Error::new(ErrorCode::UnsupportedCodec, format!("no software {what} for {}", codec.name()))
        .hint("this codec is in the ontology but not implemented yet; use one of the alternatives, or check `ironav capabilities`")
        .alternatives(alts)
}

/// A decoder for a stream, including the codec configuration (AAC's AudioSpecificConfig) it carries.
/// The displayed picture size an H.264 or HEVC sequence parameter set describes; `nal` is the
/// whole NAL unit, header included. `None` for other NAL units or codecs, or a damaged one.
pub fn sps_picture_size(codec: CodecId, nal: &[u8]) -> Option<(u32, u32)> {
    match codec {
        CodecId::H264 if nal.len() > 1 && nal[0] & 31 == 7 => {
            let s = h264::ps::parse_sps(&h264::bits::unescape(&nal[1..])).ok()?;
            let [l, r, t, b] = s.crop;
            Some(((s.mb_width * 16).checked_sub(l + r)? as u32, (s.mb_height * 16).checked_sub(t + b)? as u32))
        }
        CodecId::Hevc if nal.len() > 2 && (nal[0] >> 1) & 63 == 33 => {
            let s = hevc::ps::parse_sps(&h264::bits::unescape(&nal[2..])).ok()?;
            let [l, r, t, b] = s.crop;
            Some((s.width.checked_sub(l + r)? as u32, s.height.checked_sub(t + b)? as u32))
        }
        _ => None,
    }
}

/// How a decoder from [`new_decoder_with`] behaves.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct DecoderOptions {
    /// Output each picture as soon as nothing earlier can still arrive, for live sources. H.264
    /// that does not declare its reordering depth then starts with none and learns it (see
    /// [`h264::H264Decoder::low_delay`]); HEVC always declares it, and other codecs do not reorder.
    pub low_delay: bool,
}

/// [`new_decoder_for`] with options.
pub fn new_decoder_with(s: &ironav_core::StreamInfo, o: &DecoderOptions) -> Result<Box<dyn Decoder>> {
    if o.low_delay && s.codec == CodecId::H264 {
        return Ok(Box::new(h264::H264Decoder::low_delay()));
    }
    new_decoder_for(s)
}

pub fn new_decoder_for(s: &ironav_core::StreamInfo) -> Result<Box<dyn Decoder>> {
    match s.codec {
        CodecId::Aac => {
            let d: Box<dyn Decoder> = Box::new(aac::AacDecoder::new(&s.extradata, &s.params)?);
            // an MP4 edit list that starts after the encoder's priming samples says where the audio begins
            let in_samples = matches!(s.params, StreamParams::Audio { sample_rate, .. } if s.time_base.num == 1 && s.time_base.den == sample_rate as i64);
            Ok(if s.lead_in > 0 && in_samples { Box::new(Gapless { inner: d, start: s.lead_in as u64, end: u64::MAX, pos: 0 }) } else { d })
        }
        CodecId::Mp3 => {
            let d: Box<dyn Decoder> = Box::new(mp3::Mp3Decoder::new(&s.params)?);
            // extradata from the MP3 reader: where the LAME tag says the audio starts and ends
            Ok(match s.extradata[..] {
                [a, b, c, d2, e, f, g, h] => Box::new(Gapless { inner: d, start: u32::from_le_bytes([a, b, c, d2]) as u64, end: u32::from_le_bytes([e, f, g, h]) as u64, pos: 0 }),
                _ => d,
            })
        }
        CodecId::Opus => Ok(trimmed(s, Box::new(opus::OpusDecoder::new(&s.extradata, &s.params)?))),
        CodecId::Vorbis => Ok(trimmed(s, Box::new(vorbis::VorbisDecoder::new(&s.extradata, &s.params)?))),
        CodecId::Vp8 => Ok(Box::new(vp8::Vp8Decoder::new())),
        CodecId::H264 => Ok(Box::new(h264::H264Decoder::new())),
        CodecId::Hevc => Ok(Box::new(hevc::HevcDecoder::new())),
        CodecId::Vp9 => Ok(Box::new(vp9::Vp9Decoder::new())),
        CodecId::Mjpeg => Ok(Box::new(jpeg::JpegDecoder::new())),
        _ => new_decoder(s.codec, &s.params, s.bits),
    }
}

/// Cuts a packet's audio to the packet's duration. The Ogg reader shortens the last packet's
/// duration to what the final page's granule position allows, which drops the codec's padding.
/// The second field is the stream's rate: frames at another rate (a chained Ogg link's, whose
/// packets are timed in the stream's rate) are not cut, since a duration in the stream's rate cannot
/// say their length exactly; the padding being cut is a file's last packet's.
struct EndTrim(Box<dyn Decoder>, i64);

impl EndTrim {
    fn cut(frames: &mut [ironav_core::Frame], duration: i64, rate: i64) {
        for f in frames {
            if let ironav_core::Frame::Audio(a) = f {
                if a.sample_rate as i64 != rate {
                    continue;
                }
                let keep = duration.max(0) as usize * a.channels.max(1) as usize;
                if a.samples.len() > keep {
                    a.samples.truncate(keep);
                }
            }
        }
    }
}

impl Decoder for EndTrim {
    fn decode(&mut self, packet: &ironav_core::Packet) -> Result<Vec<ironav_core::Frame>> {
        let mut frames = self.0.decode(packet)?;
        // a duration of 0 means unknown
        if packet.duration > 0 {
            Self::cut(&mut frames, packet.duration, self.1);
        }
        Ok(frames)
    }
    fn flush(&mut self) -> Result<Vec<ironav_core::Frame>> {
        self.0.flush()
    }
}

/// Drops decoded samples before `start` and from `end` on (positions in the decoder's own output),
/// and renumbers the frames' pts from the first kept sample.
struct Gapless {
    inner: Box<dyn Decoder>,
    start: u64,
    end: u64,
    pos: u64,
}

impl Gapless {
    fn keep(&mut self, frames: Vec<ironav_core::Frame>) -> Vec<ironav_core::Frame> {
        let mut out = Vec::with_capacity(frames.len());
        for f in frames {
            let ironav_core::Frame::Audio(mut a) = f else {
                out.push(f);
                continue;
            };
            let ch = a.channels.max(1) as usize;
            let (from, to) = (self.pos, self.pos + a.frames() as u64);
            self.pos = to;
            let (lo, hi) = (from.max(self.start), to.min(self.end));
            if lo >= hi {
                continue;
            }
            a.samples = a.samples[(lo - from) as usize * ch..(hi - from) as usize * ch].to_vec();
            a.pts = (lo - self.start) as i64;
            out.push(ironav_core::Frame::Audio(a));
        }
        out
    }
}

impl Decoder for Gapless {
    fn decode(&mut self, packet: &ironav_core::Packet) -> Result<Vec<ironav_core::Frame>> {
        let frames = self.inner.decode(packet)?;
        Ok(self.keep(frames))
    }
    fn flush(&mut self) -> Result<Vec<ironav_core::Frame>> {
        let frames = self.inner.flush()?;
        Ok(self.keep(frames))
    }
}

/// Wrap a decoder in [`EndTrim`] when the stream's packet durations are in samples (time base
/// 1/rate), which is how the Ogg reader gives them; Matroska's millisecond durations are left alone.
fn trimmed(s: &ironav_core::StreamInfo, d: Box<dyn Decoder>) -> Box<dyn Decoder> {
    let rate = match s.params {
        StreamParams::Audio { sample_rate, .. } => sample_rate as i64,
        _ => 0,
    };
    if s.time_base.num == 1 && s.time_base.den == rate {
        Box::new(EndTrim(d, rate))
    } else {
        d
    }
}

/// `bits` is the stream's bits per sample where that is meaningful (PCM, FLAC).
pub fn new_decoder(codec: CodecId, params: &StreamParams, bits: Option<u8>) -> Result<Box<dyn Decoder>> {
    match (codec, params) {
        (c, StreamParams::Audio { sample_rate, channels }) if c.sample_format().is_some() => {
            Ok(Box::new(pcm::PcmDecoder::new(c.sample_format().unwrap(), *sample_rate, *channels)))
        }
        (CodecId::Flac, StreamParams::Audio { sample_rate, channels }) => Ok(Box::new(flac::FlacDecoder::new(*sample_rate, *channels, bits))),
        (CodecId::RawVideo, StreamParams::Video { width, height, pix_fmt, .. }) => {
            Ok(Box::new(rawvideo::RawVideoDecoder::new(*width, *height, *pix_fmt)))
        }
        (c, _) => Err(unsupported(c, "decoder")),
    }
}

pub fn new_encoder(codec: CodecId, params: &StreamParams, bits: Option<u8>) -> Result<Box<dyn Encoder>> {
    new_encoder_with(codec, params, bits, None)
}

/// The bit rates, in kbit/s, a lossy audio encoder here accepts; `None` for codecs that have no bit rate.
pub fn audio_bitrate_range(codec: CodecId) -> Option<(u32, u32)> {
    match codec {
        CodecId::Opus => Some((6, 510)),
        CodecId::Vorbis => Some((16, 500)),
        CodecId::Aac => Some((8, 512)),
        CodecId::Mp3 => Some((32, 320)),
        _ => None,
    }
}

/// Like `new_encoder`, with a target bit rate for the codecs that take one (`audio_bitrate_range`).
pub fn new_encoder_with(codec: CodecId, params: &StreamParams, bits: Option<u8>, bitrate_kbps: Option<u32>) -> Result<Box<dyn Encoder>> {
    match (codec, params) {
        (c, StreamParams::Audio { .. }) if c.sample_format().is_some() => Ok(Box::new(pcm::PcmEncoder::new(c.sample_format().unwrap()))),
        (CodecId::Flac, StreamParams::Audio { sample_rate, channels }) => Ok(Box::new(flac::FlacEncoder::new(*sample_rate, *channels, bits)?)),
        (CodecId::Opus, StreamParams::Audio { sample_rate, channels }) => {
            let bitrate = bitrate_kbps.map_or(opus::OpusEncoder::default_bitrate(*channels as usize), |k| k * 1000);
            Ok(Box::new(opus::OpusEncoder::new(*sample_rate, *channels as usize, bitrate)?))
        }
        (CodecId::Vorbis, StreamParams::Audio { sample_rate, channels }) => {
            let bitrate = bitrate_kbps.map_or(vorbis::VorbisEncoder::default_bitrate(*channels as usize), |k| k * 1000);
            Ok(Box::new(vorbis::VorbisEncoder::new(*sample_rate, *channels as usize, bitrate)?))
        }
        (CodecId::Aac, StreamParams::Audio { sample_rate, channels }) => {
            let bitrate = bitrate_kbps.map_or(aac::AacEncoder::default_bitrate(*channels as usize), |k| k * 1000);
            Ok(Box::new(aac::AacEncoder::new(*sample_rate, *channels as usize, bitrate)?))
        }
        (CodecId::Mp3, StreamParams::Audio { sample_rate, channels }) => {
            let bitrate = bitrate_kbps.map_or(mp3::Mp3Encoder::default_bitrate(*channels as usize), |k| k * 1000);
            Ok(Box::new(mp3::Mp3Encoder::new(*sample_rate, *channels as usize, bitrate)?))
        }
        (CodecId::RawVideo, StreamParams::Video { .. }) => Ok(Box::new(rawvideo::RawVideoEncoder)),
        (CodecId::Mjpeg, StreamParams::Video { .. }) => Ok(Box::new(jpeg::JpegEncoder::new(jpeg::JpegEncoder::default_quality()))),
        (CodecId::H264, StreamParams::Video { .. }) => Ok(Box::new(h264::H264Encoder::new(params, None, bitrate_kbps.map(|k| k.saturating_mul(1000)))?)),
        (c, p) if c.media_type() != p.media_type() => Err(Error::invalid_arg(format!(
            "codec {} is {:?} but the stream is {:?}",
            c.name(),
            c.media_type(),
            p.media_type()
        ))),
        (c, _) => Err(unsupported(c, "encoder")),
    }
}

/// The codec configuration a container needs for a stream this crate's encoder will produce
/// (empty when the codec needs none).
pub fn encoder_extradata(codec: CodecId, params: &StreamParams) -> Vec<u8> {
    match (codec, params) {
        (CodecId::Opus, StreamParams::Audio { channels, .. }) => opus::opus_head(*channels as usize),
        (CodecId::Vorbis, StreamParams::Audio { sample_rate, channels }) => vorbis::encoder_headers(*sample_rate, *channels as usize),
        (CodecId::Aac, StreamParams::Audio { sample_rate, channels }) => aac::audio_specific_config(*sample_rate, *channels as usize),
        _ => Vec::new(),
    }
}

/// Samples a decoder outputs ahead of the first input sample for a stream from this crate's encoder
/// (containers record it so players can skip it).
pub fn encoder_lead_in(codec: CodecId) -> i64 {
    match codec {
        CodecId::Aac => aac::PRIMING,
        CodecId::Mp3 => mp3::LEAD_IN,
        _ => 0,
    }
}

pub fn media_type_of(params: &StreamParams) -> MediaType {
    params.media_type()
}
