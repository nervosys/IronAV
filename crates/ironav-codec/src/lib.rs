//! Software codecs and filters, plus the registries that describe them.
//!
//! The registries ([`implemented`], [`FILTERS`]) are the single source of truth for
//! "what can IronAV actually do"; the ontology and the planner both read them.

pub mod flac;
mod filters;
mod pcm;
mod rawvideo;

pub use filters::{build_filter, FilterSpec, ParamSpec, FILTERS};

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
