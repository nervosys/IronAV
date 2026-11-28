//! Flash Video (`.flv`): a 9-byte header, then tags (type, 24-bit data size, 24-bit timestamp and
//! an extension byte for its top bits, stream id, body) each followed by the size of the tag before
//! it. Audio tags are type 8, video 9, script data 18.
//!
//! Read: H.264 (codec id 7: the AVCDecoderConfigurationRecord of the sequence header packet, NAL
//! units with a composition time offset, end of sequence), HEVC (codec id 12, laid out as H.264 is,
//! and the "enhanced FLV" `hvc1` form), AV1 (enhanced FLV `av01`), AAC (the AudioSpecificConfig
//! packet, then raw frames), MP3 and PCM (format 0, big-endian as FFmpeg reads it, and 3,
//! little-endian; 8-bit unsigned or 16-bit signed). Times are the tags' milliseconds (the extension
//! byte included). `onMetaData` gives the picture size and frame rate when the sequence header does
//! not; it is read defensively (nesting and sizes are capped, and a damaged one is ignored).
//!
//! The file is walked once, tag to tag, reading only the first bytes of each, to build the table of
//! samples (a key frame is a video tag marked so). The table goes to the MP4 reader's machinery
//! (`Mp4Demuxer::from_parts`), which reads, seeks (to the key frame at or before the time) and
//! discards. That one pass also does what the `keyframes` object of the metadata would, and is
//! right for files whose metadata is missing or wrong (live recordings), so the object is not read.
//!
//! Streams of other codecs (Sorenson H.263, screen video, VP6, ADPCM, Nellymoser, Speex, A-law and
//! u-law, other enhanced FourCCs) are reported by `unsupported_streams`, not played. Not handled:
//! encrypted tags (skipped), a second sequence header after the first (a change of picture size
//! mid-stream), multiple audio or video tracks of enhanced FLV, enhanced audio.

use crate::aiff::fourcc_text;
use crate::mp4demux::{av1c_config, parse_avcc, parse_hvcc, Config, Mp4Demuxer, Sample, TrackInfo};
use ironav_codec::av1::obu::{parse_sequence_header, split_obus};
use ironav_codec::mp3::{parse_frame, Header};
use ironav_core::{CodecId, Error, Rational, Result, StreamInfo, StreamParams};
use std::io::{BufReader, Read, Seek, SeekFrom};

/// The most tags indexed.
const MAX_TAGS: usize = 1 << 24;

/// The largest script data tag read.
const MAX_SCRIPT: u64 = 1 << 20;

/// The largest codec configuration read.
const MAX_CONFIG: u64 = 1 << 16;

/// The deepest AMF nesting followed.
const MAX_DEPTH: u32 = 16;

/// Whether `magic` starts an FLV file: the signature and version 1.
pub fn looks_like_flv(magic: &[u8]) -> bool {
    magic.len() >= 9 && &magic[..3] == b"FLV" && magic[3] == 1
}

fn bad(msg: impl std::fmt::Display) -> Error {
    Error::invalid_input(format!("FLV: {msg}"))
}

/// What `onMetaData` says that is of use here.
#[derive(Default, Debug)]
struct Meta {
    width: Option<f64>,
    height: Option<f64>,
    frame_rate: Option<f64>,
}

/// A reader of AMF0 values that never goes past its data, nests no more than `MAX_DEPTH` deep and
/// allocates nothing.
struct Amf<'a> {
    d: &'a [u8],
    at: usize,
}

impl<'a> Amf<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let s = self.d.get(self.at..self.at.checked_add(n)?)?;
        self.at += n;
        Some(s)
    }
    fn u16(&mut self) -> Option<usize> {
        self.take(2).map(|b| u16::from_be_bytes([b[0], b[1]]) as usize)
    }
    fn u32(&mut self) -> Option<usize> {
        self.take(4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]) as usize)
    }
    fn string(&mut self) -> Option<&'a [u8]> {
        let n = self.u16()?;
        self.take(n)
    }

    /// Skips one value, and returns it when it is a number. `top` collects the numbers among the
    /// properties of the object or array this value is, if it is one.
    fn value(&mut self, depth: u32, top: Option<&mut Meta>) -> Option<Option<f64>> {
        if depth > MAX_DEPTH {
            return None;
        }
        let marker = self.take(1)?[0];
        Some(match marker {
            0 => Some(f64::from_be_bytes(self.take(8)?.try_into().ok()?)),
            1 => {
                self.take(1)?;
                None
            }
            2 => {
                self.string()?;
                None
            }
            3 | 8 | 16 => {
                if marker == 16 {
                    self.string()?; // the class name
                }
                if marker == 8 {
                    self.take(4)?; // the count, which is not trusted
                }
                self.properties(depth, top)?;
                None
            }
            5 | 6 => None,
            7 => {
                self.take(2)?;
                None
            }
            10 => {
                let n = self.u32()?;
                if n > self.d.len() - self.at {
                    return None; // every value is at least a byte
                }
                for _ in 0..n {
                    self.value(depth + 1, None)?;
                }
                None
            }
            11 => {
                self.take(10)?;
                None
            }
            12 | 15 => {
                let n = self.u32()?;
                self.take(n)?;
                None
            }
            _ => return None,
        })
    }

    /// The name and value pairs of an object, up to its end marker.
    fn properties(&mut self, depth: u32, mut top: Option<&mut Meta>) -> Option<()> {
        loop {
            let key = self.string()?;
            if key.is_empty() && self.d.get(self.at) == Some(&9) {
                self.at += 1;
                return Some(());
            }
            let v = self.value(depth + 1, None)?;
            if let (Some(m), Some(x)) = (top.as_deref_mut(), v) {
                match key {
                    b"width" => m.width = Some(x),
                    b"height" => m.height = Some(x),
                    b"framerate" => m.frame_rate = Some(x),
                    _ => {}
                }
            }
        }
    }
}

/// The metadata of a script data tag's body, if it is `onMetaData`. Whatever was read before a
/// damaged place is kept.
fn parse_meta(body: &[u8]) -> Meta {
    let mut meta = Meta::default();
    let mut a = Amf { d: body, at: 0 };
    if a.take(1) == Some(&[2]) && a.string() == Some(b"onMetaData") {
        let _ = a.value(0, Some(&mut meta));
    }
    meta
}

/// A frame rate as a rational: whole, or over 1001 for the NTSC rates, else to a thousandth.
fn rate_of(fr: f64) -> Rational {
    if !fr.is_finite() || fr <= 0.0 || fr > 1000.0 {
        return Rational::new(0, 1);
    }
    for den in [1i64, 1001] {
        let n = (fr * den as f64).round();
        if n >= 1.0 && (n / den as f64 - fr).abs() < 0.0002 && (den == 1 || n % 1000.0 == 0.0) {
            return Rational::new(n as i64, den);
        }
    }
    let (n, d) = ((fr * 1000.0).round() as i64, 1000);
    let g = crate::mp4demux::gcd(n, d);
    Rational::new(n / g, d / g)
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum VideoKind {
    H264,
    Hevc,
    Av1,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum AudioKind {
    Aac,
    Mp3,
    Pcm,
}

/// What a tag's body is, after its codec header.
#[derive(Debug)]
enum Body {
    /// A codec configuration record, from this byte of the body.
    Config(usize),
    /// A frame from this byte of the body.
    Frame { key: bool, cts: i32, skip: usize },
}

fn si24(b: &[u8]) -> i32 {
    ((b[0] as i32) << 24 | (b[1] as i32) << 16 | (b[2] as i32) << 8) >> 8
}

/// The codec and kind of packet a video tag's first bytes say; `Ok(None)` for tags that hold nothing
/// to play (command frames, end of sequence, metadata), `Err` with a description for a codec not read.
fn video_head(b: &[u8]) -> std::result::Result<Option<(VideoKind, Body)>, String> {
    let Some(&b0) = b.first() else { return Ok(None) };
    let frame_type = b0 >> 4 & 7;
    if b0 & 0x80 != 0 {
        // enhanced FLV: the low nibble is the packet type and a FourCC follows
        if b.len() < 5 {
            return Ok(None);
        }
        let kind = match &b[1..5] {
            b"hvc1" => VideoKind::Hevc,
            b"avc1" => VideoKind::H264,
            b"av01" => VideoKind::Av1,
            other => return Err(format!("video: '{}' in enhanced FLV", fourcc_text(other))),
        };
        if frame_type == 5 {
            return Ok(None);
        }
        return Ok(match b0 & 15 {
            0 => Some((kind, Body::Config(5))),
            // coded frames: H.264 and HEVC have a composition time before the data
            1 if kind != VideoKind::Av1 => (b.len() >= 8).then(|| (kind, Body::Frame { key: frame_type == 1, cts: si24(&b[5..8]), skip: 8 })),
            1 | 3 => Some((kind, Body::Frame { key: frame_type == 1, cts: 0, skip: 5 })),
            _ => None,
        });
    }
    let kind = match b0 & 15 {
        7 => VideoKind::H264,
        12 => VideoKind::Hevc,
        2 => return Err("video: Sorenson H.263 (codec id 2)".into()),
        3 => return Err("video: Screen Video (codec id 3)".into()),
        4 => return Err("video: VP6 (codec id 4)".into()),
        5 => return Err("video: VP6 with alpha (codec id 5)".into()),
        6 => return Err("video: Screen Video 2 (codec id 6)".into()),
        n => return Err(format!("video: codec id {n}")),
    };
    if frame_type == 5 || b.len() < 5 {
        return Ok(None);
    }
    Ok(match b[1] {
        0 => Some((kind, Body::Config(5))),
        1 => Some((kind, Body::Frame { key: frame_type == 1, cts: si24(&b[2..5]), skip: 5 })),
        _ => None, // end of sequence
    })
}

/// The codec and kind of packet an audio tag's first bytes say; as `video_head`.
fn audio_head(b: &[u8]) -> std::result::Result<Option<(AudioKind, Body)>, String> {
    let Some(&b0) = b.first() else { return Ok(None) };
    let frame = |skip| Body::Frame { key: true, cts: 0, skip };
    Ok(match b0 >> 4 {
        10 => match b.get(1) {
            Some(0) => Some((AudioKind::Aac, Body::Config(2))),
            Some(1) => Some((AudioKind::Aac, frame(2))),
            _ => None,
        },
        2 | 14 => Some((AudioKind::Mp3, frame(1))),
        0 | 3 => Some((AudioKind::Pcm, frame(1))),
        1 => return Err("audio: ADPCM (sound format 1)".into()),
        4..=6 => return Err("audio: Nellymoser (sound format 4, 5 or 6)".into()),
        7 => return Err("audio: A-law (sound format 7)".into()),
        8 => return Err("audio: u-law (sound format 8)".into()),
        9 => return Err("audio: enhanced FLV audio (sound format 9)".into()),
        11 => return Err("audio: Speex (sound format 11)".into()),
        n => return Err(format!("audio: sound format {n}")),
    })
}

/// The tracks found by the walk of the tags, as far as they go.
struct Scan<K> {
    /// `None` until a tag of this kind is seen; `Some(Err)` when its codec is not read.
    kind: Option<std::result::Result<K, String>>,
    config: Option<Vec<u8>>,
    samples: Vec<Sample>,
    /// The first audio tag's format byte.
    flags: u8,
    mp3: Option<Header>,
}

impl<K> Scan<K> {
    fn new() -> Self {
        Self { kind: None, config: None, samples: Vec::new(), flags: 0, mp3: None }
    }
}

/// Gives each sample the time to the next one (the last, the time to the one before it) and returns
/// the end time.
fn set_durations(samples: &mut [Sample]) -> i64 {
    let n = samples.len();
    for i in 0..n {
        let d = if i + 1 < n { samples[i + 1].dts - samples[i].dts } else if i > 0 { samples[i].dts - samples[i - 1].dts } else { 0 };
        samples[i].duration = u32::try_from(d.max(0)).unwrap_or(u32::MAX);
    }
    samples.last().map_or(0, |s| s.dts + s.duration as i64)
}

/// Reads `n` bytes, counting them in `got`.
fn grab(b: &mut impl Read, got: &mut u64, n: u64) -> Result<Vec<u8>> {
    let mut v = vec![0u8; n as usize];
    b.read_exact(&mut v)?;
    *got += n;
    Ok(v)
}

/// Opens an FLV file for reading.
pub fn open<R: Read + Seek + Send>(mut r: R) -> Result<Mp4Demuxer<R>> {
    let total = r.seek(SeekFrom::End(0))?;
    r.seek(SeekFrom::Start(0))?;
    let mut head = [0u8; 9];
    r.read_exact(&mut head).map_err(|_| bad("the file is shorter than its header"))?;
    if !looks_like_flv(&head) {
        return Err(bad("not an FLV file"));
    }
    let data_offset = u32::from_be_bytes([head[5], head[6], head[7], head[8]]) as u64;
    if data_offset < 9 {
        return Err(bad("the header's data offset is too small"));
    }
    let mut video = Scan::<VideoKind>::new();
    let mut audio = Scan::<AudioKind>::new();
    let mut meta: Option<Meta> = None;
    let mut pos = data_offset + 4; // the size of the tag before the first, which is zero
    {
        let mut b = BufReader::with_capacity(1 << 16, &mut r);
        b.seek(SeekFrom::Start(pos))?;
        let mut tags = 0usize;
        while pos + 11 <= total && tags < MAX_TAGS {
            let mut h = [0u8; 11];
            if b.read_exact(&mut h).is_err() {
                break;
            }
            tags += 1;
            let size = (h[1] as u64) << 16 | (h[2] as u64) << 8 | h[3] as u64;
            let ts = ((h[7] as u32) << 24 | (h[4] as u32) << 16 | (h[5] as u32) << 8 | h[6] as u32) as i64;
            let body = pos + 11;
            if body + size > total {
                break; // the last tag is cut short
            }
            let mut got = 0u64;
            // an encrypted tag (filter bit) is not read
            if h[0] & 0x20 == 0 {
                match h[0] & 0x1F {
                    18 if meta.is_none() && size <= MAX_SCRIPT => {
                        let d = grab(&mut b, &mut got, size)?;
                        meta = Some(parse_meta(&d)).filter(|m| m.width.is_some() || m.height.is_some() || m.frame_rate.is_some());
                    }
                    9 => {
                        let peek = grab(&mut b, &mut got, size.min(8))?;
                        match video_head(&peek) {
                            Err(text) => {
                                video.kind.get_or_insert(Err(text));
                            }
                            Ok(None) => {}
                            // a tag of another codec than the first video tag's is left out
                            Ok(Some((kind, body_kind))) if *video.kind.get_or_insert(Ok(kind)) == Ok(kind) => match body_kind {
                                Body::Config(skip) => {
                                    if video.config.is_none() && size <= MAX_CONFIG && size as usize > skip {
                                        let mut all = peek;
                                        let rest = size - got;
                                        all.extend(grab(&mut b, &mut got, rest)?);
                                        video.config = Some(all[skip..].to_vec());
                                    }
                                }
                                Body::Frame { key, cts, skip } if size as usize > skip => {
                                    video.samples.push(Sample { stream: 0, sync: key, offset: body + skip as u64, size: (size as usize - skip) as u32, dts: ts, cts_offset: cts, duration: 0 });
                                }
                                Body::Frame { .. } => {}
                            },
                            Ok(Some(_)) => {}
                        }
                    }
                    8 => {
                        let peek = grab(&mut b, &mut got, size.min(8))?;
                        match audio_head(&peek) {
                            Err(text) => {
                                audio.kind.get_or_insert(Err(text));
                            }
                            Ok(None) => {}
                            Ok(Some((kind, body_kind))) => {
                                if audio.kind.is_none() {
                                    audio.flags = peek[0];
                                }
                                if *audio.kind.get_or_insert(Ok(kind)) == Ok(kind) {
                                    match body_kind {
                                        Body::Config(skip) => {
                                            if audio.config.is_none() && size <= MAX_CONFIG && size as usize > skip {
                                                let mut all = peek;
                                                let rest = size - got;
                                        all.extend(grab(&mut b, &mut got, rest)?);
                                                audio.config = Some(all[skip..].to_vec());
                                            }
                                        }
                                        Body::Frame { skip, .. } if size as usize > skip => {
                                            if kind == AudioKind::Mp3 && audio.mp3.is_none() {
                                                audio.mp3 = peek.get(skip..skip + 4).and_then(parse_frame);
                                            }
                                            audio.samples.push(Sample { stream: 0, sync: true, offset: body + skip as u64, size: (size as usize - skip) as u32, dts: ts, cts_offset: 0, duration: 0 });
                                        }
                                        Body::Frame { .. } => {}
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            // the rest of the body, and the size of this tag after it
            b.seek_relative((size - got + 4) as i64)?;
            pos = body + size + 4;
        }
    }

    let mut unsupported = Vec::new();
    let mut tracks = Vec::new();
    let mut samples = Vec::new();
    // video first, then audio
    if let Some(kind) = video.kind {
        match kind.and_then(|k| video_track(k, video.config.as_deref(), meta.as_ref(), &mut video.samples)) {
            Ok(t) => {
                for mut s in video.samples {
                    s.stream = tracks.len() as u16;
                    samples.push(s);
                }
                tracks.push(t);
            }
            Err(text) => unsupported.push(text),
        }
    }
    if let Some(kind) = audio.kind {
        match kind.and_then(|k| audio_track(k, audio.flags, audio.config.as_deref(), audio.mp3, &mut audio.samples)) {
            Ok(t) => {
                for mut s in audio.samples {
                    s.stream = tracks.len() as u16;
                    samples.push(s);
                }
                tracks.push(t);
            }
            Err(text) => unsupported.push(text),
        }
    }
    for (i, t) in tracks.iter_mut().enumerate() {
        t.stream.index = i;
    }
    if tracks.is_empty() {
        let what = if unsupported.is_empty() { "the file has no audio or video tags".to_string() } else { unsupported.join("; ") };
        return Err(Error::new(ironav_core::ErrorCode::UnsupportedCodec, format!("FLV: no stream IronAV can play ({what})"))
            .hint("FLV files with H.264, HEVC or AV1 video and AAC, MP3 or PCM audio are read"));
    }
    Ok(Mp4Demuxer::from_parts(r, "flv", tracks, samples).with_extras(Vec::new(), Vec::new(), unsupported))
}

fn video_track(kind: VideoKind, config: Option<&[u8]>, meta: Option<&Meta>, samples: &mut [Sample]) -> std::result::Result<TrackInfo, String> {
    let name = match kind {
        VideoKind::H264 => "H.264",
        VideoKind::Hevc => "HEVC",
        VideoKind::Av1 => "AV1",
    };
    let cfg = config.ok_or_else(|| format!("video: {name} with no sequence header"))?;
    let c = match kind {
        VideoKind::H264 => parse_avcc(cfg),
        VideoKind::Hevc => parse_hvcc(cfg),
        VideoKind::Av1 => av1c_config(cfg),
    }
    .map_err(|e| format!("video: {name}: {}", e.message))?;
    let (codec, mut extradata) = (
        match kind {
            VideoKind::H264 => CodecId::H264,
            VideoKind::Hevc => CodecId::Hevc,
            VideoKind::Av1 => CodecId::Av1,
        },
        Vec::new(),
    );
    // the picture size: from the sequence parameter set, else the metadata
    let size = match kind {
        VideoKind::Av1 => {
            extradata = cfg.to_vec();
            cfg.get(4..).and_then(|o| split_obus(o).ok()).and_then(|obus| obus.into_iter().find(|o| o.kind == 1).and_then(|o| parse_sequence_header(o.payload).ok())).map(|s| (s.max_frame_width, s.max_frame_height))
        }
        _ => crate::ts::nals(&c.prefix).find_map(|n| ironav_codec::sps_picture_size(codec, n)),
    };
    let from_meta = meta.and_then(|m| Some((m.width?, m.height?))).filter(|(w, h)| (1.0..=65535.0).contains(w) && (1.0..=65535.0).contains(h)).map(|(w, h)| (w as u32, h as u32));
    let (width, height) = size.or(from_meta).ok_or_else(|| format!("video: {name} with no picture size"))?;
    let end = set_durations(samples);
    let n = samples.len() as i64;
    let first = samples.first().map_or(0, |s| s.dts);
    let measured = if end > first { n as f64 * 1000.0 / (end - first) as f64 } else { 0.0 };
    let frame_rate = match meta.and_then(|m| m.frame_rate).map(rate_of) {
        Some(r) if r.num > 0 => r,
        _ => rate_of(measured),
    };
    let params = StreamParams::Video { width, height, pix_fmt: c.pix_fmt(), frame_rate };
    let stream = StreamInfo { index: 0, codec, time_base: Rational::new(1, 1000), duration: Some(end), bits: None, extradata, lead_in: 0, params };
    Ok(TrackInfo { stream, convert: matches!(kind, VideoKind::H264 | VideoKind::Hevc), config: c, swap: 0, unwrap: Vec::new(), meta: Default::default() })
}

fn audio_track(kind: AudioKind, flags: u8, config: Option<&[u8]>, mp3: Option<Header>, samples: &mut [Sample]) -> std::result::Result<TrackInfo, String> {
    let mut extradata = Vec::new();
    let mut swap = 0;
    let mut bits = None;
    let (codec, rate, channels) = match kind {
        AudioKind::Aac => {
            let asc = config.ok_or("audio: AAC with no AudioSpecificConfig")?;
            let (rate, channels) = ironav_codec::aac::output_params(asc).ok_or("audio: AAC whose AudioSpecificConfig is not one this decoder takes")?;
            extradata = asc.to_vec();
            (CodecId::Aac, rate, channels)
        }
        AudioKind::Mp3 => {
            let h = mp3.ok_or("audio: MP3 with no readable frame header")?;
            (h.codec(), h.sample_rate, h.channels())
        }
        AudioKind::Pcm => {
            let rate = 44100u32 >> (3 - (flags >> 2 & 3));
            let channels = 1 + (flags & 1) as u16;
            if flags & 2 == 0 {
                bits = Some(8);
                (CodecId::PcmU8, rate, channels)
            } else {
                bits = Some(16);
                // format 0 is read as big-endian, as FFmpeg does; 3 is little-endian
                swap = if flags >> 4 == 0 { 2 } else { 0 };
                (CodecId::PcmS16Le, rate, channels)
            }
        }
    };
    let end = set_durations(samples);
    let stream = StreamInfo { index: 0, codec, time_base: Rational::new(1, 1000), duration: Some(end), bits, extradata, lead_in: 0, params: StreamParams::Audio { sample_rate: rate, channels } };
    Ok(TrackInfo { stream, config: Config { prefix: Vec::new(), nal_length: 0, bits: 8, chroma: 1 }, convert: false, swap, unwrap: Vec::new(), meta: Default::default() })
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironav_core::Demuxer;
    use std::io::Cursor;

    fn tag(kind: u8, ts: u32, body: &[u8]) -> Vec<u8> {
        let mut v = vec![kind];
        v.extend(&(body.len() as u32).to_be_bytes()[1..]);
        v.extend(&ts.to_be_bytes()[1..]);
        v.push((ts >> 24) as u8);
        v.extend([0, 0, 0]);
        v.extend(body);
        v.extend((11 + body.len() as u32).to_be_bytes());
        v
    }

    fn file(tags: &[Vec<u8>]) -> Vec<u8> {
        let mut f = b"FLV\x01\x05\0\0\0\x09\0\0\0\0".to_vec();
        for t in tags {
            f.extend(t);
        }
        f
    }

    fn amf_string(s: &str) -> Vec<u8> {
        let mut v = vec![2];
        v.extend((s.len() as u16).to_be_bytes());
        v.extend(s.as_bytes());
        v
    }

    fn amf_prop(name: &str, n: f64) -> Vec<u8> {
        let mut v = (name.len() as u16).to_be_bytes().to_vec();
        v.extend(name.as_bytes());
        v.push(0);
        v.extend(n.to_be_bytes());
        v
    }

    fn meta_body(props: &[Vec<u8>]) -> Vec<u8> {
        let mut v = amf_string("onMetaData");
        v.push(8);
        v.extend((props.len() as u32).to_be_bytes());
        for p in props {
            v.extend(p);
        }
        v.extend([0, 0, 9]);
        v
    }

    /// A 16-bit mono PCM tag body (format 3, 11 kHz).
    fn pcm(samples: &[i16]) -> Vec<u8> {
        let mut b = vec![3 << 4 | 1 << 2 | 1 << 1]; // little-endian PCM, 11 kHz, 16-bit, mono
        b.extend(samples.iter().flat_map(|s| s.to_le_bytes()));
        b
    }

    #[test]
    fn the_metadata_gives_what_it_gives() {
        let m = parse_meta(&meta_body(&[amf_prop("width", 640.0), amf_prop("height", 360.0), amf_prop("framerate", 29.97), amf_prop("duration", 5.0)]));
        assert_eq!((m.width, m.height, m.frame_rate), (Some(640.0), Some(360.0), Some(29.97)));
        assert_eq!(rate_of(29.97), Rational::new(30000, 1001));
        assert_eq!(rate_of(25.0), Rational::new(25, 1));
        assert_eq!(rate_of(12.5), Rational::new(25, 2));
    }

    #[test]
    fn nesting_and_sizes_are_capped() {
        // objects nested a hundred thousand deep
        let mut bomb = amf_string("onMetaData");
        bomb.extend(std::iter::repeat_n([3u8, 0, 1, b'a'], 100_000).flatten());
        let _ = parse_meta(&bomb);
        // a strict array that claims four billion values, a long string that claims as much
        let mut v = amf_string("onMetaData");
        v.extend([10, 0xFF, 0xFF, 0xFF, 0xFF]);
        let _ = parse_meta(&v);
        let mut v = amf_string("onMetaData");
        v.extend([3, 0, 1, b'a', 12, 0xFF, 0xFF, 0xFF, 0xFF]);
        let _ = parse_meta(&v);
        // numbers read before damage are kept
        let mut v = meta_body(&[amf_prop("width", 16.0)]);
        v.truncate(v.len() - 3);
        v.extend([0, 1, b'x', 9]);
        assert_eq!(parse_meta(&v).width, Some(16.0));
    }

    #[test]
    fn pcm_tags_are_read_with_extended_timestamps() {
        let tags = [tag(8, 0, &pcm(&[1, 2, 3, 4])), tag(8, 100, &pcm(&[5, 6, 7, 8])), tag(8, 0x0100_0000 + 5, &pcm(&[9, 10]))];
        let mut d = open(Cursor::new(file(&tags))).unwrap();
        let s = &d.streams()[0];
        assert_eq!((s.codec, s.time_base, s.bits), (CodecId::PcmS16Le, Rational::new(1, 1000), Some(16)));
        let p = d.read_packet().unwrap().unwrap();
        assert_eq!((p.pts, p.duration, p.data.len()), (0, 100, 8));
        let p = d.read_packet().unwrap().unwrap();
        assert_eq!(p.pts, 100);
        let p = d.read_packet().unwrap().unwrap();
        assert_eq!(p.pts, 0x0100_0005, "the extension byte is the top eight bits");
        assert!(d.read_packet().unwrap().is_none());
    }

    const AVCC: [u8; 18] = [1, 0x42, 0, 0x1E, 0xFF, 0xE1, 0, 4, 0x67, 1, 2, 3, 1, 0, 3, 0x68, 4, 5];

    fn avc_tag(ts: u32, key: bool, cts: i32, nal: &[u8]) -> Vec<u8> {
        let mut b = vec![if key { 0x17 } else { 0x27 }, 1];
        b.extend(&cts.to_be_bytes()[1..]);
        b.extend((nal.len() as u32).to_be_bytes());
        b.extend(nal);
        tag(9, ts, &b)
    }

    fn h264_file(extra: &[Vec<u8>]) -> Vec<u8> {
        let mut seq = vec![0x17, 0, 0, 0, 0];
        seq.extend(AVCC);
        let mut tags = vec![tag(18, 0, &meta_body(&[amf_prop("width", 64.0), amf_prop("height", 48.0), amf_prop("framerate", 25.0)])), tag(9, 0, &seq)];
        for i in 0..6u32 {
            tags.push(avc_tag(i * 40, i % 3 == 0, 80, &[if i % 3 == 0 { 0x65 } else { 0x41 }, i as u8]));
        }
        tags.extend(extra.iter().cloned());
        file(&tags)
    }

    #[test]
    fn h264_is_read_as_annex_b_with_parameter_sets_and_seeks_to_key_frames() {
        let mut d = open(Cursor::new(h264_file(&[]))).unwrap();
        let s = &d.streams()[0];
        assert_eq!(s.codec, CodecId::H264);
        match s.params {
            StreamParams::Video { width, height, frame_rate, .. } => assert_eq!((width, height, frame_rate), (64, 48, Rational::new(25, 1))),
            _ => panic!(),
        }
        let p = d.read_packet().unwrap().unwrap();
        assert_eq!((p.pts, p.duration), (80, 40), "the composition time is added to the decode time");
        assert_eq!(p.data, [&[0, 0, 0, 1, 0x67, 1, 2, 3, 0, 0, 0, 1, 0x68, 4, 5][..], &[0, 0, 0, 1, 0x65, 0]].concat());
        // the key frame decoded at 0.12 s is shown at 0.2 s: a seek to 0.16 s must start before it
        assert_eq!(d.seek(0.16).unwrap(), 0.0);
        assert!((d.seek(0.25).unwrap() - 0.12).abs() < 1e-9);
        let p = d.read_packet().unwrap().unwrap();
        assert_eq!(p.pts, 200);
        assert_eq!(&p.data[..4], [0, 0, 0, 1], "the parameter sets are put back in front of a key frame");
    }

    #[test]
    fn unsupported_codecs_are_listed() {
        let speex = vec![11 << 4 | 0x0E, 1, 2, 3];
        let d = open(Cursor::new(h264_file(&[tag(8, 0, &speex)]))).unwrap();
        assert_eq!(d.streams().len(), 1);
        let u = d.unsupported_streams();
        assert_eq!(u.len(), 1, "{u:?}");
        assert!(u[0].contains("Speex"), "{u:?}");
        let vp6 = vec![0x14, 0, 0, 0, 0];
        let e = open(Cursor::new(file(&[tag(9, 0, &vp6)]))).err().unwrap();
        assert!(e.message.contains("VP6"), "{}", e.message);
    }

    #[test]
    fn damaged_files_are_errors_not_panics() {
        let good = file(&[tag(18, 0, &meta_body(&[amf_prop("width", 16.0)])), tag(8, 0, &pcm(&[1, 2, 3, 4])), tag(8, 50, &pcm(&[5, 6]))]);
        for n in 0..good.len() {
            if let Ok(mut d) = open(Cursor::new(good[..n].to_vec())) {
                while d.read_packet().ok().flatten().is_some() {}
            }
        }
        // a tag whose size runs past the end, and a header whose data offset is silly
        let mut huge = good.clone();
        huge[14..17].copy_from_slice(&[0xFF, 0xFF, 0xFF]);
        let _ = open(Cursor::new(huge));
        let mut offset = good.clone();
        offset[5..9].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(open(Cursor::new(offset)).is_err());
        let mut small = good;
        small[5..9].copy_from_slice(&1u32.to_be_bytes());
        assert!(open(Cursor::new(small)).is_err());
    }
}
