use crate::{Error, ErrorCode, Rational, Result};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MediaType {
    Audio,
    Video,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SampleFormat {
    U8,
    S16,
    S24,
    S32,
    F32,
    F64,
}

impl SampleFormat {
    pub const ALL: [SampleFormat; 6] = [Self::U8, Self::S16, Self::S24, Self::S32, Self::F32, Self::F64];
    pub fn name(self) -> &'static str {
        match self {
            Self::U8 => "u8",
            Self::S16 => "s16",
            Self::S24 => "s24",
            Self::S32 => "s32",
            Self::F32 => "f32",
            Self::F64 => "f64",
        }
    }
    pub fn bytes(self) -> usize {
        match self {
            Self::U8 => 1,
            Self::S16 => 2,
            Self::S24 => 3,
            Self::S32 | Self::F32 => 4,
            Self::F64 => 8,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PixelFormat {
    Gray8,
    Yuv420p,
    Yuv422p,
    Yuv444p,
    Rgb24,
    /// 4:2:0 with 10-bit samples stored as little-endian 16-bit words (value in the low 10 bits).
    #[serde(rename = "yuv420p10le")]
    Yuv420p10,
}

impl PixelFormat {
    pub const ALL: [PixelFormat; 6] = [Self::Gray8, Self::Yuv420p, Self::Yuv422p, Self::Yuv444p, Self::Rgb24, Self::Yuv420p10];
    pub fn name(self) -> &'static str {
        match self {
            Self::Gray8 => "gray8",
            Self::Yuv420p => "yuv420p",
            Self::Yuv422p => "yuv422p",
            Self::Yuv444p => "yuv444p",
            Self::Rgb24 => "rgb24",
            Self::Yuv420p10 => "yuv420p10le",
        }
    }
    /// Bits per sample.
    pub fn bit_depth(self) -> u8 {
        if self == Self::Yuv420p10 { 10 } else { 8 }
    }
    /// `(width, height, bytes_per_pixel)` of each plane for a frame of the given size.
    pub fn planes(self, w: usize, h: usize) -> Vec<(usize, usize, usize)> {
        let (cw, ch) = ((w + 1) / 2, (h + 1) / 2);
        match self {
            Self::Gray8 => vec![(w, h, 1)],
            Self::Yuv420p => vec![(w, h, 1), (cw, ch, 1), (cw, ch, 1)],
            Self::Yuv422p => vec![(w, h, 1), (cw, h, 1), (cw, h, 1)],
            Self::Yuv444p => vec![(w, h, 1); 3],
            Self::Rgb24 => vec![(w, h, 3)],
            Self::Yuv420p10 => vec![(w, h, 2), (cw, ch, 2), (cw, ch, 2)],
        }
    }
    pub fn frame_bytes(self, w: usize, h: usize) -> usize {
        self.planes(w, h).iter().map(|&(pw, ph, bpp)| pw * ph * bpp).sum()
    }
}

macro_rules! codecs {
    ($( $variant:ident => $name:literal, $media:ident; )*) => {
        /// Every codec the ontology knows about. Whether it can actually be decoded or
        /// encoded is reported by `ironav-codec`, not implied by membership here.
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub enum CodecId { $($variant),* }
        impl CodecId {
            pub const ALL: &'static [CodecId] = &[$(CodecId::$variant),*];
            pub fn name(self) -> &'static str { match self { $(CodecId::$variant => $name),* } }
            pub fn media_type(self) -> MediaType { match self { $(CodecId::$variant => MediaType::$media),* } }
        }
    };
}

codecs! {
    PcmU8 => "pcm_u8", Audio;
    PcmS16Le => "pcm_s16le", Audio;
    PcmS24Le => "pcm_s24le", Audio;
    PcmS32Le => "pcm_s32le", Audio;
    PcmF32Le => "pcm_f32le", Audio;
    PcmF64Le => "pcm_f64le", Audio;
    Aac => "aac", Audio;
    Mp3 => "mp3", Audio;
    Opus => "opus", Audio;
    Vorbis => "vorbis", Audio;
    Flac => "flac", Audio;
    Alac => "alac", Audio;
    RawVideo => "rawvideo", Video;
    H264 => "h264", Video;
    Hevc => "hevc", Video;
    Av1 => "av1", Video;
    Vp8 => "vp8", Video;
    Vp9 => "vp9", Video;
    Mpeg4 => "mpeg4", Video;
    ProRes => "prores", Video;
    Mjpeg => "mjpeg", Video;
}

impl CodecId {
    pub fn parse(s: &str) -> Result<CodecId> {
        let key = s.to_ascii_lowercase();
        let key = match key.as_str() {
            "h265" => "hevc",
            "avc" => "h264",
            "pcm" => "pcm_s16le",
            k => k,
        };
        Self::ALL.iter().copied().find(|c| c.name() == key).ok_or_else(|| {
            Error::new(ErrorCode::UnsupportedCodec, format!("unknown codec '{s}'"))
                .hint("query the ontology with `ironav ontology search <text> --kind codec`")
                .alternatives(Self::ALL.iter().map(|c| c.name()))
        })
    }
    /// For PCM codecs, the sample format they carry.
    pub fn sample_format(self) -> Option<SampleFormat> {
        Some(match self {
            Self::PcmU8 => SampleFormat::U8,
            Self::PcmS16Le => SampleFormat::S16,
            Self::PcmS24Le => SampleFormat::S24,
            Self::PcmS32Le => SampleFormat::S32,
            Self::PcmF32Le => SampleFormat::F32,
            Self::PcmF64Le => SampleFormat::F64,
            _ => return None,
        })
    }
    pub fn pcm_for(fmt: SampleFormat) -> CodecId {
        match fmt {
            SampleFormat::U8 => Self::PcmU8,
            SampleFormat::S16 => Self::PcmS16Le,
            SampleFormat::S24 => Self::PcmS24Le,
            SampleFormat::S32 => Self::PcmS32Le,
            SampleFormat::F32 => Self::PcmF32Le,
            SampleFormat::F64 => Self::PcmF64Le,
        }
    }
}

impl Serialize for CodecId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> std::result::Result<S::Ok, S::Error> {
        s.serialize_str(self.name())
    }
}
impl<'de> Deserialize<'de> for CodecId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        let s = String::deserialize(d)?;
        CodecId::parse(&s).map_err(|e| serde::de::Error::custom(e.message))
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "type", rename_all = "kebab-case")]
pub enum StreamParams {
    Audio { sample_rate: u32, channels: u16 },
    Video { width: u32, height: u32, pix_fmt: PixelFormat, frame_rate: Rational },
}

impl StreamParams {
    pub fn media_type(&self) -> MediaType {
        match self {
            Self::Audio { .. } => MediaType::Audio,
            Self::Video { .. } => MediaType::Video,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct StreamInfo {
    pub index: usize,
    pub codec: CodecId,
    pub time_base: Rational,
    /// Duration in `time_base` units, when known.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub duration: Option<i64>,
    /// Bits per sample of the coded audio, when meaningful (PCM, FLAC).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bits: Option<u8>,
    /// Codec configuration a container needs to describe the stream when it is copied (for AAC, the
    /// AudioSpecificConfig). Empty when the codec has none.
    #[serde(skip)]
    pub extradata: Vec<u8>,
    /// Audio only: time (in `time_base` units) at the start of the stream that a container's edit list
    /// skips, such as encoder delay. Copying the stream keeps it so audio stays in sync with video.
    #[serde(skip)]
    pub lead_in: i64,
    #[serde(flatten)]
    pub params: StreamParams,
}

impl StreamInfo {
    pub fn duration_secs(&self) -> Option<f64> {
        self.duration.map(|d| d as f64 * self.time_base.as_f64())
    }
}

/// A compressed (or raw-bitstream) chunk. Moved, never cloned, between pipeline stages.
#[derive(Debug, Clone)]
pub struct Packet {
    pub stream: usize,
    pub pts: i64,
    pub duration: i64,
    pub data: Vec<u8>,
}

/// Interleaved audio, normalised to f64 in [-1, 1] so every PCM conversion is lossless
/// up to the destination's precision.
#[derive(Debug, Clone)]
pub struct AudioFrame {
    pub sample_rate: u32,
    pub channels: u16,
    pub pts: i64,
    pub samples: Vec<f64>,
}

impl AudioFrame {
    pub fn frames(&self) -> usize {
        self.samples.len() / self.channels.max(1) as usize
    }
}

#[derive(Debug, Clone)]
pub struct VideoFrame {
    pub width: u32,
    pub height: u32,
    pub pix_fmt: PixelFormat,
    pub pts: i64,
    pub planes: Vec<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub enum Frame {
    Audio(AudioFrame),
    Video(VideoFrame),
}
