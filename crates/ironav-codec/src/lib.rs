//! Software codecs and filters, plus the registries that describe them.
//!
//! The registries ([`implemented`], [`FILTERS`]) are the single source of truth for
//! "what can IronAV actually do"; the ontology and the planner both read them.

pub mod aac;
pub mod flac;
mod filters;
pub mod mp3;
pub mod opus;
pub mod vorbis;
mod pcm;
mod pixconv;
mod rawvideo;
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

pub fn implemented() -> Vec<CodecImpl> {
    CodecId::ALL
        .iter()
        .filter_map(|&c| {
            if c == CodecId::Aac {
                return Some(CodecImpl { codec: c, decode: true, encode: false, implementation: "ironav-aac" });
            }
            if c == CodecId::Mp3 {
                return Some(CodecImpl { codec: c, decode: true, encode: false, implementation: "ironav-mp3" });
            }
            if c == CodecId::Opus {
                return Some(CodecImpl { codec: c, decode: true, encode: false, implementation: "ironav-opus" });
            }
            if c == CodecId::Vorbis {
                return Some(CodecImpl { codec: c, decode: true, encode: false, implementation: "ironav-vorbis" });
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
pub fn new_decoder_for(s: &ironav_core::StreamInfo) -> Result<Box<dyn Decoder>> {
    match s.codec {
        CodecId::Aac => Ok(Box::new(aac::AacDecoder::new(&s.extradata, &s.params)?)),
        CodecId::Mp3 => Ok(Box::new(mp3::Mp3Decoder::new(&s.params)?)),
        CodecId::Opus => Ok(trimmed(s, Box::new(opus::OpusDecoder::new(&s.extradata, &s.params)?))),
        CodecId::Vorbis => Ok(trimmed(s, Box::new(vorbis::VorbisDecoder::new(&s.extradata, &s.params)?))),
        _ => new_decoder(s.codec, &s.params, s.bits),
    }
}

/// Cuts a packet's audio to the packet's duration. The Ogg reader shortens the last packet's
/// duration to what the final page's granule position allows, which drops the codec's padding.
struct EndTrim(Box<dyn Decoder>);

impl EndTrim {
    fn cut(frames: &mut [ironav_core::Frame], duration: i64) {
        for f in frames {
            if let ironav_core::Frame::Audio(a) = f {
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
            Self::cut(&mut frames, packet.duration);
        }
        Ok(frames)
    }
    fn flush(&mut self) -> Result<Vec<ironav_core::Frame>> {
        self.0.flush()
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
        Box::new(EndTrim(d))
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
    match (codec, params) {
        (c, StreamParams::Audio { .. }) if c.sample_format().is_some() => Ok(Box::new(pcm::PcmEncoder::new(c.sample_format().unwrap()))),
        (CodecId::Flac, StreamParams::Audio { sample_rate, channels }) => Ok(Box::new(flac::FlacEncoder::new(*sample_rate, *channels, bits)?)),
        (CodecId::RawVideo, StreamParams::Video { .. }) => Ok(Box::new(rawvideo::RawVideoEncoder)),
        (c, p) if c.media_type() != p.media_type() => Err(Error::invalid_arg(format!(
            "codec {} is {:?} but the stream is {:?}",
            c.name(),
            c.media_type(),
            p.media_type()
        ))),
        (c, _) => Err(unsupported(c, "encoder")),
    }
}

pub fn media_type_of(params: &StreamParams) -> MediaType {
    params.media_type()
}
