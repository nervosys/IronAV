//! Software codecs and filters, plus the registries that describe them.
//!
//! The registries ([`implemented`], [`FILTERS`]) are the single source of truth for
//! "what can IronAV actually do"; the ontology and the planner both read them.

pub mod aac;
pub mod av1;
pub mod alac;
pub mod change;
pub mod flac;
pub mod h264;
pub mod hevc;
pub mod vp9;
pub mod jpeg;
mod filters;
pub mod mp3;
pub mod mpeg4;
pub mod opus;
pub mod order;
pub mod prores;
pub mod vorbis;
pub mod vp8;
mod pcm;
mod pixconv;
mod rawvideo;
mod resample;
mod scale;
pub mod voice;

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
    c == CodecId::Hevc || c == CodecId::Av1 || c == CodecId::H264 && !matches!(params, ironav_core::StreamParams::Video { pix_fmt: ironav_core::PixelFormat::Yuv420p10 | ironav_core::PixelFormat::Yuv422p10 | ironav_core::PixelFormat::Yuv444p10, .. })
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
            if c == CodecId::Mp2 {
                return Some(CodecImpl { codec: c, decode: true, encode: true, implementation: "ironav-mp2" });
            }
            if c == CodecId::Alac {
                return Some(CodecImpl { codec: c, decode: true, encode: true, implementation: "ironav-alac" });
            }
            if c == CodecId::Mpeg4 {
                return Some(CodecImpl { codec: c, decode: true, encode: true, implementation: "ironav-mpeg4" });
            }
            if c == CodecId::ProRes {
                return Some(CodecImpl { codec: c, decode: true, encode: true, implementation: "ironav-prores" });
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
                return Some(CodecImpl { codec: c, decode: true, encode: true, implementation: "ironav-hevc" });
            }
            if c == CodecId::Av1 {
                return Some(CodecImpl { codec: c, decode: true, encode: true, implementation: "ironav-av1" });
            }
            if c == CodecId::Vp8 {
                return Some(CodecImpl { codec: c, decode: true, encode: true, implementation: "ironav-vp8" });
            }
            if c == CodecId::Vp9 {
                return Some(CodecImpl { codec: c, decode: true, encode: true, implementation: "ironav-vp9" });
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

/// The frame rate a H.264 or HEVC sequence parameter set states in its VUI timing, reduced; `nal`
/// is the whole NAL unit, header included. `None` when the VUI says nothing about timing. H.264
/// counts ticks per field, so a frame is two ticks; HEVC counts ticks per picture.
pub fn sps_frame_rate(codec: CodecId, nal: &[u8]) -> Option<ironav_core::Rational> {
    let (units, scale, ticks) = match codec {
        CodecId::H264 if nal.len() > 1 && nal[0] & 31 == 7 => {
            let (units, scale) = h264::ps::parse_sps(&h264::bits::unescape(&nal[1..])).ok()?.timing?;
            (units, scale, 2)
        }
        CodecId::Hevc if nal.len() > 2 && (nal[0] >> 1) & 63 == 33 => {
            let (units, scale) = hevc::ps::parse_sps(&h264::bits::unescape(&nal[2..])).ok()?.timing?;
            (units, scale, 1)
        }
        _ => return None,
    };
    let (num, den) = (scale as i64, units as i64 * ticks);
    if num == 0 || den == 0 {
        return None;
    }
    let g = gcd(num, den);
    Some(ironav_core::Rational::new(num / g, den / g))
}

/// The pixel format an H.264 or HEVC sequence parameter set describes, from its chroma format and bit
/// depth: monochrome 8-bit is `gray`, deeper monochrome comes out as 4:2:0 (as the decoders give it).
/// `nal` is the whole NAL unit, header included.
pub fn sps_pixel_format(codec: CodecId, nal: &[u8]) -> Option<ironav_core::PixelFormat> {
    let (chroma, depth) = match codec {
        CodecId::H264 if nal.len() > 1 && nal[0] & 31 == 7 => {
            let s = h264::ps::parse_sps(&h264::bits::unescape(&nal[1..])).ok()?;
            (s.chroma_format_idc, s.bit_depth_luma.max(s.bit_depth_chroma))
        }
        CodecId::Hevc if nal.len() > 2 && (nal[0] >> 1) & 63 == 33 => {
            let s = hevc::ps::parse_sps(&h264::bits::unescape(&nal[2..])).ok()?;
            (s.chroma_format_idc, s.bit_depth_luma.max(s.bit_depth_chroma))
        }
        _ => return None,
    };
    use ironav_core::PixelFormat as P;
    let bits = if depth <= 8 { 8 } else if depth <= 10 { 10 } else { 12 };
    Some(match (chroma, bits) {
        (0, 8) => P::Gray8,
        (1, 8) => P::Yuv420p,
        (0 | 1, 10) => P::Yuv420p10,
        (0 | 1, _) => P::Yuv420p12,
        (2, 8) => P::Yuv422p,
        (2, 10) => P::Yuv422p10,
        (2, _) => P::Yuv422p12,
        (_, 8) => P::Yuv444p,
        (_, 10) => P::Yuv444p10,
        _ => P::Yuv444p12,
    })
}

fn gcd(a: i64, b: i64) -> i64 {
    if b == 0 {
        a
    } else {
        gcd(b, a % b)
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
            Ok(kept_span(s, d))
        }
        CodecId::Mp3 => {
            let d: Box<dyn Decoder> = Box::new(mp3::Mp3Decoder::new(&s.params)?);
            // extradata from the MP3 reader: where the LAME tag says the audio starts and ends
            Ok(match s.extradata[..] {
                [a, b, c, d2, e, f, g, h] => Box::new(Gapless { inner: d, start: u32::from_le_bytes([a, b, c, d2]) as u64, end: u32::from_le_bytes([e, f, g, h]) as u64, pos: 0 }),
                _ => kept_span(s, d),
            })
        }
        CodecId::Mp2 => Ok(kept_span(s, Box::new(mp3::Mp2Decoder::new(&s.params)?))),
        CodecId::Alac => Ok(Box::new(alac::AlacDecoder::new(&s.extradata, &s.params)?)),
        CodecId::Opus => Ok(trimmed(s, opus::new_decoder(&s.extradata, &s.params)?)),
        CodecId::Vorbis => Ok(trimmed(s, Box::new(vorbis::VorbisDecoder::new(&s.extradata, &s.params)?))),
        CodecId::Vp8 => Ok(Box::new(vp8::Vp8Decoder::new())),
        CodecId::H264 => Ok(Box::new(h264::H264Decoder::new())),
        CodecId::Hevc => Ok(Box::new(hevc::HevcDecoder::new())),
        CodecId::Vp9 => Ok(Box::new(vp9::Vp9Decoder::new())),
        CodecId::Mjpeg => Ok(Box::new(jpeg::JpegDecoder::new())),
        CodecId::ProRes => Ok(Box::new(prores::ProResDecoder::new())),
        CodecId::Mpeg4 => Ok(Box::new(mpeg4::Mpeg4Decoder::new(&s.extradata))),
        CodecId::Av1 => Ok(Box::new(av1::Av1Decoder::new(&s.extradata))),
        _ => new_decoder(s.codec, &s.params, s.bits),
    }
}

/// Keeps the samples a container's audio edit keeps: from the stream's `lead_in` (the decoder's
/// priming, in samples of the stream's rate) to `lead_in` plus its duration, which the container
/// set (an MP4 edit's segment, or a Matroska codec delay less its discard padding). A stream with no
/// lead-in, or whose time base is not its sample rate, is left whole.
fn kept_span(s: &ironav_core::StreamInfo, d: Box<dyn Decoder>) -> Box<dyn Decoder> {
    let in_samples = matches!(s.params, StreamParams::Audio { sample_rate, .. } if s.time_base.num == 1 && s.time_base.den == sample_rate as i64);
    if s.lead_in <= 0 || !in_samples {
        return d;
    }
    let end = match s.duration {
        Some(n) if n > 0 => (s.lead_in + n) as u64,
        _ => u64::MAX,
    };
    Box::new(Gapless { inner: d, start: s.lead_in as u64, end, pos: 0 })
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
        CodecId::Mp2 => Some((32, 384)),
        _ => None,
    }
}

/// Like `new_encoder`, with a target bit rate for the codecs that take one (`audio_bitrate_range`).
pub fn new_encoder_with(codec: CodecId, params: &StreamParams, bits: Option<u8>, bitrate_kbps: Option<u32>) -> Result<Box<dyn Encoder>> {
    new_encoder_with_opts(codec, params, bits, bitrate_kbps, None, None, None)
}

/// Opus encoder settings beyond the bit rate. The defaults are the library's: CELT mode, 20 ms
/// packets, no FEC, no DTX.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct OpusOptions {
    /// CELT, SILK, hybrid or automatic.
    pub mode: opus::OpusMode,
    /// What the input is, for automatic mode.
    pub signal: opus::OpusSignal,
    /// Packet length in milliseconds: 2.5, 5, 10, 20, 40 or 60.
    pub frame_ms: Option<f64>,
    /// Expected packet loss in percent, for in-band FEC; 0 is off.
    pub fec_loss: u32,
    /// Discontinuous transmission: a pause sends only TOC bytes.
    pub dtx: bool,
}

impl OpusOptions {
    fn apply(&self, enc: opus::OpusEncoder) -> Result<opus::OpusEncoder> {
        // the frame length first: the SILK and hybrid modes check it
        let enc = match self.frame_ms {
            Some(ms) => enc.with_frame_ms(ms)?,
            None => enc,
        };
        Ok(enc.with_mode(self.mode)?.with_signal(self.signal).with_fec(self.fec_loss).with_dtx(self.dtx))
    }
}

/// Video encoder settings beyond the rate and the key frame interval, for the encoders that take them
/// (the others ignore them). The defaults are the library's: no B pictures, the encoder's own tiles,
/// half-sample motion search only, and no hidden ALTREF frames.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct VideoOptions {
    /// B pictures between anchor pictures, for HEVC and MPEG-4 (0 to 3).
    pub bframes: Option<usize>,
    /// Tile columns and rows as log2 counts, for AV1 (`with_tiles`).
    pub tiles: Option<(u32, u32)>,
    /// Quarter-sample motion refinement, for AV1 (`with_quarter_mv`).
    pub quarter_mv: bool,
    /// Hidden ALTREF frames every this many frames, for AV1 (`with_altref`, 2 to 16).
    pub altref: Option<usize>,
}

impl VideoOptions {
    /// The settings for a HEVC encoder.
    pub fn apply_hevc(&self, enc: hevc::HevcEncoder) -> hevc::HevcEncoder {
        match self.bframes {
            Some(n) => enc.with_bframes(n),
            None => enc,
        }
    }

    /// The settings for an MPEG-4 encoder.
    pub fn apply_mpeg4(&self, enc: mpeg4::enc::Mpeg4Encoder) -> mpeg4::enc::Mpeg4Encoder {
        match self.bframes {
            Some(n) => enc.with_bframes(n),
            None => enc,
        }
    }

    /// The settings for an AV1 encoder.
    pub fn apply_av1(&self, mut enc: av1::enc::Av1Encoder) -> av1::enc::Av1Encoder {
        if let Some((cols_log2, rows_log2)) = self.tiles {
            enc = enc.with_tiles(cols_log2, rows_log2);
        }
        if self.quarter_mv {
            enc = enc.with_quarter_mv(true);
        }
        if let Some(lag) = self.altref {
            enc = enc.with_altref(lag);
        }
        enc
    }
}

/// Like `new_encoder_with`, with Opus encoder settings for the Opus codec (other codecs ignore them), and
/// with `video` settings for the HEVC, MPEG-4 and AV1 encoders (other codecs ignore them).
pub fn new_encoder_with_opts(codec: CodecId, params: &StreamParams, bits: Option<u8>, bitrate_kbps: Option<u32>, opus_opts: Option<&OpusOptions>, prores_profile: Option<prores::Profile>, video: Option<&VideoOptions>) -> Result<Box<dyn Encoder>> {
    match (codec, params) {
        (c, StreamParams::Audio { .. }) if c.sample_format().is_some() => Ok(Box::new(pcm::PcmEncoder::new(c.sample_format().unwrap()))),
        (CodecId::Flac, StreamParams::Audio { sample_rate, channels }) => Ok(Box::new(flac::FlacEncoder::new(*sample_rate, *channels, bits)?)),
        (CodecId::Alac, StreamParams::Audio { sample_rate, channels }) => Ok(Box::new(alac::AlacEncoder::new(*sample_rate, *channels, bits)?)),
        (CodecId::Opus, StreamParams::Audio { sample_rate, channels }) => {
            let bitrate = bitrate_kbps.map_or(opus::OpusEncoder::default_bitrate(*channels as usize), |k| k * 1000);
            if *channels > 2 {
                let enc = opus::MultistreamEncoder::new(*sample_rate, *channels as usize, bitrate)?;
                return Ok(Box::new(match opus_opts {
                    Some(o) => enc.map_streams(|e| o.apply(e))?,
                    None => enc,
                }));
            }
            let enc = opus::OpusEncoder::new(*sample_rate, *channels as usize, bitrate)?;
            Ok(Box::new(match opus_opts {
                Some(o) => o.apply(enc)?,
                None => enc,
            }))
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
        (CodecId::Mp2, StreamParams::Audio { sample_rate, channels }) => {
            let bitrate = bitrate_kbps.map_or(mp3::Mp2Encoder::default_bitrate(*channels as usize), |k| k * 1000);
            Ok(Box::new(mp3::Mp2Encoder::new(*sample_rate, *channels as usize, bitrate)?))
        }
        (CodecId::RawVideo, StreamParams::Video { .. }) => Ok(Box::new(rawvideo::RawVideoEncoder)),
        (CodecId::Mjpeg, StreamParams::Video { .. }) => Ok(Box::new(jpeg::JpegEncoder::new(jpeg::JpegEncoder::default_quality()))),
        (CodecId::ProRes, StreamParams::Video { .. }) => Ok(Box::new(prores::ProResEncoder::new(params, prores_profile.unwrap_or(prores::Profile::Hq))?)),
        (CodecId::H264, StreamParams::Video { .. }) => Ok(Box::new(h264::H264Encoder::new(params, None, bitrate_kbps.map(|k| k.saturating_mul(1000)))?)),
        (CodecId::Hevc, StreamParams::Video { .. }) => {
            let enc = hevc::HevcEncoder::new(params, None, bitrate_kbps.map(|k| k.saturating_mul(1000)))?;
            Ok(Box::new(match video {
                Some(v) => v.apply_hevc(enc),
                None => enc,
            }))
        }
        (CodecId::Vp8, StreamParams::Video { .. }) => Ok(Box::new(vp8::Vp8Encoder::new(params, None, bitrate_kbps.map(|k| k.saturating_mul(1000)))?)),
        (CodecId::Vp9, StreamParams::Video { .. }) => Ok(Box::new(vp9::Vp9Encoder::new(params, None)?)),
        (CodecId::Mpeg4, StreamParams::Video { .. }) => {
            let enc = mpeg4::enc::Mpeg4Encoder::new(params, None, bitrate_kbps.map(|k| k.saturating_mul(1000)))?;
            Ok(Box::new(match video {
                Some(v) => v.apply_mpeg4(enc),
                None => enc,
            }))
        }
        (CodecId::Av1, StreamParams::Video { .. }) => {
            let enc = av1::enc::Av1Encoder::new(params, None, bitrate_kbps.map(|k| k.saturating_mul(1000)))?;
            Ok(Box::new(match video {
                Some(v) => v.apply_av1(enc),
                None => enc,
            }))
        }
        (c, p) if c.media_type() != p.media_type() => Err(Error::invalid_arg(format!(
            "codec {} is {:?} but the stream is {:?}",
            c.name(),
            c.media_type(),
            p.media_type()
        ))),
        (c, _) => Err(unsupported(c, "encoder")),
    }
}

/// The codec configuration for a stream from `new_encoder_with_opts` with the same arguments: as
/// `encoder_extradata`, but an Opus head carries the pre-skip of the mode asked for.
pub fn encoder_extradata_with_opts(codec: CodecId, params: &StreamParams, bitrate_kbps: Option<u32>, opus_opts: Option<&OpusOptions>, prores_profile: Option<prores::Profile>, video: Option<&VideoOptions>) -> Result<Vec<u8>> {
    match (codec, params, opus_opts) {
        (CodecId::Opus, StreamParams::Audio { sample_rate, channels }, Some(o)) if *channels > 2 => {
            let bitrate = bitrate_kbps.map_or(opus::OpusEncoder::default_bitrate(*channels as usize), |k| k * 1000);
            Ok(opus::MultistreamEncoder::new(*sample_rate, *channels as usize, bitrate)?.map_streams(|e| o.apply(e))?.head())
        }
        (CodecId::Opus, StreamParams::Audio { sample_rate, channels }, Some(o)) => {
            let bitrate = bitrate_kbps.map_or(opus::OpusEncoder::default_bitrate(*channels as usize), |k| k * 1000);
            Ok(o.apply(opus::OpusEncoder::new(*sample_rate, *channels as usize, bitrate)?)?.head())
        }
        (CodecId::Mpeg4, StreamParams::Video { width, height, frame_rate, .. }, _) if video.and_then(|v| v.bframes).is_some_and(|n| n > 0) => {
            Ok(mpeg4::enc::config_with_bframes(*width, *height, if frame_rate.num > 0 { frame_rate.num } else { 25 }))
        }
        (CodecId::ProRes, StreamParams::Video { .. }, _) => Ok(prores_profile.unwrap_or(prores::Profile::Hq).fourcc().to_vec()),
        _ => Ok(encoder_extradata(codec, params)),
    }
}

/// The codec configuration a container needs for a stream this crate's encoder will produce
/// (empty when the codec needs none).
pub fn encoder_extradata(codec: CodecId, params: &StreamParams) -> Vec<u8> {
    match (codec, params) {
        (CodecId::Opus, StreamParams::Audio { channels, .. }) => opus::opus_head(*channels as usize),
        (CodecId::Vorbis, StreamParams::Audio { sample_rate, channels }) => vorbis::encoder_headers(*sample_rate, *channels as usize),
        (CodecId::Aac, StreamParams::Audio { sample_rate, channels }) => aac::audio_specific_config(*sample_rate, *channels as usize),
        (CodecId::ProRes, _) => prores::Profile::Hq.fourcc().to_vec(),
        (CodecId::Av1, StreamParams::Video { width, height, .. }) => av1::enc::config_record(*width, *height),
        (CodecId::Mpeg4, StreamParams::Video { width, height, frame_rate, .. }) => mpeg4::enc::config(*width, *height, if frame_rate.num > 0 { frame_rate.num } else { 25 }),
        _ => Vec::new(),
    }
}

/// Samples a decoder outputs ahead of the first input sample for a stream from this crate's encoder
/// (containers record it so players can skip it).
pub fn encoder_lead_in(codec: CodecId) -> i64 {
    match codec {
        CodecId::Aac => aac::PRIMING,
        CodecId::Mp3 => mp3::LEAD_IN,
        CodecId::Mp2 => mp3::MP2_LEAD_IN,
        _ => 0,
    }
}

pub fn media_type_of(params: &StreamParams) -> MediaType {
    params.media_type()
}
