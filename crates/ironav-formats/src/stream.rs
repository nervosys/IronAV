//! Demuxers fed bytes as they arrive, for input that cannot seek and may never end: a pipe, a
//! socket, a file still being written. They own no reader, so the same parser serves every
//! transport (and a fuzzer): [`StreamDemuxer::feed`] bytes in, [`StreamDemuxer::next_packet`]
//! packets out, `None` meaning "not enough yet".
//!
//! The formats here are the self-delimiting ones: Y4M, WAV, ADTS (AAC), Annex B H.264/HEVC and
//! MPEG-TS ([`crate::ts`]), Matroska/WebM (`mkv::live`), fragmented MP4 (`mp4demux::live`), RTP
//! in RTSP's interleaved framing ([`crate::rtp`]), and Ogg and MP3 ([`crate::audio_stream`]).

use crate::{adts, wav, y4m};
use ironav_core::{CodecId, Error, ErrorCode, Packet, PixelFormat, Rational, Result, StreamInfo, StreamParams};
use std::collections::VecDeque;

/// A packet, and whether a decoder can start from it.
#[derive(Debug, Clone)]
pub struct StreamPacket {
    pub packet: Packet,
    pub key: bool,
}

pub trait StreamDemuxer: Send {
    fn format_name(&self) -> &'static str;
    /// The streams found so far: empty until the header (or, for Annex B, a sequence parameter
    /// set) has arrived.
    fn streams(&self) -> &[StreamInfo];
    /// More bytes of the input.
    fn feed(&mut self, bytes: &[u8]) -> Result<()>;
    /// The next whole packet, or `None` until more bytes are fed (or, after [`end`](Self::end), at the end).
    fn next_packet(&mut self) -> Result<Option<StreamPacket>>;
    /// No more bytes will come; what is buffered and complete still comes out of `next_packet`.
    fn end(&mut self);
}

/// Bytes held back waiting for the rest of a header or packet, at most this many.
pub(crate) const MAX_PENDING: usize = 64 << 20;
/// Audio frames per WAV packet: about 21 ms at 48 kHz, so live audio is not held back long.
const WAV_FRAMES: usize = 1024;

/// Bytes fed but not yet used.
#[derive(Default)]
pub(crate) struct Pending {
    buf: Vec<u8>,
    pos: usize,
}

impl Pending {
    pub(crate) fn push(&mut self, b: &[u8]) -> Result<()> {
        if self.pos > 0 && self.pos * 2 >= self.buf.len() {
            self.buf.drain(..self.pos);
            self.pos = 0;
        }
        if self.buf.len() - self.pos + b.len() > MAX_PENDING {
            return Err(Error::invalid_input(format!("more than {} MiB arrived without a complete packet", MAX_PENDING >> 20)));
        }
        self.buf.extend_from_slice(b);
        Ok(())
    }
    pub(crate) fn data(&self) -> &[u8] {
        &self.buf[self.pos..]
    }
    pub(crate) fn take(&mut self, n: usize) -> Vec<u8> {
        let v = self.data()[..n].to_vec();
        self.pos += n;
        v
    }
    pub(crate) fn skip(&mut self, n: usize) {
        self.pos += n;
    }
}

/// Options that a bare byte stream cannot carry itself.
#[derive(Debug, Clone, Default)]
pub struct StreamOptions {
    /// One of [`STREAM_FORMATS`]; detected from the first bytes when absent.
    pub format: Option<String>,
    /// Pictures per second of an Annex B stream, which carries no timing (25 when absent).
    pub frame_rate: Option<Rational>,
}

/// Formats [`stream_demuxer`] can read.
pub const STREAM_FORMATS: [&str; 12] = ["y4m", "wav", "aac", "mp3", "ogg", "h264", "hevc", "mpegts", "matroska", "webm", "mp4", "rtp"];

/// Bytes [`stream_demuxer`] wants to see before it can tell the format.
/// (A transport stream needs two packets' sync bytes.)
pub const SNIFF_BYTES: usize = 200;

/// A stream demuxer for input starting with `magic` (at least [`SNIFF_BYTES`] of it, unless the
/// input is shorter), or of the format `opts.format` names.
pub fn stream_demuxer(magic: &[u8], opts: &StreamOptions) -> Result<Box<dyn StreamDemuxer>> {
    let fps = opts.frame_rate.unwrap_or(Rational::new(25, 1));
    // made by the HLS source whatever the format named; each part tells its own from its bytes
    if magic.starts_with(crate::renditions::MAGIC) {
        return Ok(Box::new(crate::renditions::Renditions::new(opts)));
    }
    let format = match opts.format.as_deref() {
        Some(f) => f.to_ascii_lowercase(),
        None => sniff(magic)
            .ok_or_else(|| {
                Error::new(ErrorCode::UnsupportedFormat, "the stream's format is not one that can be read as it arrives")
                    .hint("give the format explicitly")
                    .alternatives(STREAM_FORMATS)
            })?
            .to_string(),
    };
    Ok(match format.as_str() {
        "y4m" => Box::new(Y4mStream::default()),
        "wav" => Box::new(WavStream::default()),
        "aac" | "adts" => Box::new(AdtsStream::default()),
        "mp3" => Box::new(crate::audio_stream::Mp3Stream::default()),
        "ogg" | "opus" | "oga" => Box::new(crate::audio_stream::OggStream::default()),
        "id3" => Box::new(crate::audio_stream::Id3Stream::default()),
        "h264" | "avc" => Box::new(AnnexBStream::new(CodecId::H264, fps)),
        "hevc" | "h265" => Box::new(AnnexBStream::new(CodecId::Hevc, fps)),
        "mpegts" | "ts" | "m2ts" => Box::new(crate::ts::TsStream::default()),
        "matroska" | "mkv" | "webm" => Box::new(crate::mkv::live::MkvStream::default()),
        "mp4" | "fmp4" | "cmaf" => Box::new(crate::mp4demux::live::Mp4Stream::default()),
        "rtp" => Box::new(crate::rtp::RtpStream::default()),
        // made by the HLS source for a variant with separate audio, never named by a caller
        "renditions" => Box::new(crate::renditions::Renditions::new(opts)),
        other => {
            return Err(Error::new(ErrorCode::UnsupportedFormat, format!("'{other}' cannot be read as a stream")).alternatives(STREAM_FORMATS));
        }
    })
}

fn sniff(m: &[u8]) -> Option<&'static str> {
    if m.starts_with(crate::renditions::MAGIC) {
        return Some("renditions");
    }
    if m.starts_with(&[b'$', crate::rtp::DESCRIPTION_CHANNEL]) {
        return Some("rtp");
    }
    if crate::ts::packet_size(m).is_some() {
        return Some("mpegts");
    }
    if crate::mkv::looks_like_mkv(m) {
        return Some("matroska");
    }
    if crate::mp4demux::looks_like_mp4(m) {
        return Some("mp4");
    }
    if m.starts_with(b"YUV4MPEG2") {
        return Some("y4m");
    }
    if m.len() >= 12 && &m[..4] == b"RIFF" && &m[8..12] == b"WAVE" {
        return Some("wav");
    }
    if crate::ogg::looks_like_ogg(m) {
        return Some("ogg");
    }
    // what follows the tag says whether it is MP3 or AAC
    if m.starts_with(b"ID3") {
        return Some("id3");
    }
    if adts::looks_like_adts(m) {
        return Some("aac");
    }
    if ironav_codec::mp3::parse_header(m).is_some() {
        return Some("mp3");
    }
    let nal = if m.starts_with(&[0, 0, 0, 1]) {
        m.get(4..)
    } else if m.starts_with(&[0, 0, 1]) {
        m.get(3..)
    } else {
        None
    }?;
    let b0 = *nal.first()?;
    // HEVC's first NAL unit is usually a VPS (0x40 0x01) or an access unit delimiter (0x46 0x01);
    // as H.264 headers those would be type 0 (invalid) or a SEI with nal_ref_idc set (never).
    if nal.get(1) == Some(&1) && matches!(b0, 0x40 | 0x42 | 0x44 | 0x46 | 0x4e) {
        return Some("hevc");
    }
    (b0 & 0x80 == 0 && matches!(b0 & 31, 1 | 5 | 6 | 7 | 8 | 9)).then_some("h264")
}

// ---- Y4M ------------------------------------------------------------------------------------------

#[derive(Default)]
pub struct Y4mStream {
    pending: Pending,
    streams: Vec<StreamInfo>,
    frame_bytes: usize,
    index: i64,
}

impl StreamDemuxer for Y4mStream {
    fn format_name(&self) -> &'static str {
        "y4m"
    }
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }
    fn feed(&mut self, bytes: &[u8]) -> Result<()> {
        self.pending.push(bytes)
    }
    fn end(&mut self) {}
    fn next_packet(&mut self) -> Result<Option<StreamPacket>> {
        let line_end = |d: &[u8], max: usize| -> Result<Option<usize>> {
            match d.iter().take(max).position(|&b| b == b'\n') {
                Some(i) => Ok(Some(i + 1)),
                None if d.len() >= max => Err(Error::invalid_input("Y4M: a header line is too long")),
                None => Ok(None),
            }
        };
        if self.streams.is_empty() {
            let Some(n) = line_end(self.pending.data(), 4096)? else { return Ok(None) };
            let (w, h, fps, pix) = y4m::parse_header(&self.pending.data()[..n])?;
            self.pending.skip(n);
            self.frame_bytes = pix.frame_bytes(w as usize, h as usize);
            self.streams.push(StreamInfo {
                index: 0,
                codec: CodecId::RawVideo,
                time_base: fps.invert(),
                duration: None,
                bits: None,
                extradata: Vec::new(),
                lead_in: 0,
                params: StreamParams::Video { width: w, height: h, pix_fmt: pix, frame_rate: fps },
            });
        }
        let d = self.pending.data();
        if d.len() < 5 {
            return Ok(None);
        }
        if !d.starts_with(b"FRAME") {
            return Err(Error::invalid_input("Y4M: expected a FRAME line"));
        }
        let Some(n) = line_end(d, 1024)? else { return Ok(None) };
        if d.len() < n + self.frame_bytes {
            return Ok(None);
        }
        self.pending.skip(n);
        let data = self.pending.take(self.frame_bytes);
        let pts = self.index;
        self.index += 1;
        Ok(Some(StreamPacket { packet: Packet { stream: 0, pts, duration: 1, data }, key: true }))
    }
}

// ---- WAV ------------------------------------------------------------------------------------------

#[derive(Default)]
pub struct WavStream {
    pending: Pending,
    streams: Vec<StreamInfo>,
    riff: bool,
    fmt: Option<(u16, u16, u32, u16, u16)>,
    align: usize,
    /// Bytes of audio left, when the header says (a streaming writer leaves 0 or 0xFFFFFFFF).
    remaining: Option<u64>,
    frames: i64,
    ended: bool,
}

impl StreamDemuxer for WavStream {
    fn format_name(&self) -> &'static str {
        "wav"
    }
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }
    fn feed(&mut self, bytes: &[u8]) -> Result<()> {
        self.pending.push(bytes)
    }
    fn end(&mut self) {
        self.ended = true;
    }
    fn next_packet(&mut self) -> Result<Option<StreamPacket>> {
        if !self.riff {
            let d = self.pending.data();
            if d.len() < 12 {
                return Ok(None);
            }
            if &d[..4] != b"RIFF" || &d[8..12] != b"WAVE" {
                return Err(Error::invalid_input("not a WAV stream (no RIFF/WAVE header)"));
            }
            self.pending.skip(12);
            self.riff = true;
        }
        while self.streams.is_empty() {
            let d = self.pending.data();
            if d.len() < 8 {
                return Ok(None);
            }
            let size = u32::from_le_bytes([d[4], d[5], d[6], d[7]]);
            match &d[..4] {
                b"data" => {
                    let (tag, channels, rate, align, bits) = self.fmt.ok_or_else(|| Error::invalid_input("WAV 'data' chunk precedes 'fmt ' chunk"))?;
                    let codec = wav::pcm_codec(tag, bits, channels, rate, align)?;
                    self.pending.skip(8);
                    self.align = align as usize;
                    self.remaining = (size != 0 && size != u32::MAX).then_some(size as u64);
                    self.streams.push(StreamInfo {
                        index: 0,
                        codec,
                        time_base: Rational::new(1, rate as i64),
                        duration: None,
                        bits: Some(bits as u8),
                        extradata: Vec::new(),
                        lead_in: 0,
                        params: StreamParams::Audio { sample_rate: rate, channels },
                    });
                }
                id => {
                    let body = size as usize + (size as usize & 1);
                    if d.len() < 8 + body {
                        if body > MAX_PENDING {
                            return Err(Error::invalid_input("WAV: a header chunk is too large"));
                        }
                        return Ok(None);
                    }
                    if id == b"fmt " {
                        self.fmt = Some(wav::fmt_fields(&d[8..8 + size as usize])?);
                    }
                    self.pending.skip(8 + body);
                }
            }
        }
        let have = self.pending.data().len() / self.align * self.align;
        let mut want = WAV_FRAMES * self.align;
        if let Some(r) = self.remaining {
            want = want.min(r as usize);
        }
        if want == 0 || (have < want && !(self.ended && have > 0)) {
            return Ok(None);
        }
        let n = want.min(have);
        let data = self.pending.take(n);
        if let Some(r) = &mut self.remaining {
            *r -= n as u64;
        }
        let frames = (n / self.align) as i64;
        let pts = self.frames;
        self.frames += frames;
        Ok(Some(StreamPacket { packet: Packet { stream: 0, pts, duration: frames, data }, key: true }))
    }
}

// ---- ADTS -----------------------------------------------------------------------------------------

#[derive(Default)]
pub struct AdtsStream {
    pending: Pending,
    streams: Vec<StreamInfo>,
    /// The rate index and channel configuration of the stream, once found.
    locked: Option<(u8, u8)>,
    id3_checked: bool,
    pts: i64,
    ended: bool,
    /// Where the first frame falls on a 90 kHz clock, from the ID3 tag in front (HLS packed audio).
    start_90k: Option<i64>,
}

/// The time an HLS packed-audio segment's ID3 tag gives its first frame, on a 90 kHz clock: the
/// PRIV frame `com.apple.streaming.transportStreamTimestamp` (RFC 8216 3.4), 33 bits.
pub(crate) fn apple_timestamp(tag: &[u8]) -> Option<i64> {
    const OWNER: &[u8] = b"com.apple.streaming.transportStreamTimestamp\0";
    let at = tag.windows(OWNER.len()).position(|w| w == OWNER)? + OWNER.len();
    let b: [u8; 8] = tag.get(at..at + 8)?.try_into().ok()?;
    Some((u64::from_be_bytes(b) & 0x1_FFFF_FFFF) as i64)
}

impl AdtsStream {
    /// An ADTS reader whose clock starts at `start_90k` (90 kHz), when given.
    pub(crate) fn starting_at(start_90k: Option<i64>) -> Self {
        AdtsStream { start_90k, ..AdtsStream::default() }
    }
}

impl StreamDemuxer for AdtsStream {
    fn format_name(&self) -> &'static str {
        "aac"
    }
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }
    fn feed(&mut self, bytes: &[u8]) -> Result<()> {
        self.pending.push(bytes)
    }
    fn end(&mut self) {
        self.ended = true;
    }
    fn next_packet(&mut self) -> Result<Option<StreamPacket>> {
        if !self.id3_checked {
            let d = self.pending.data();
            if d.len() < 10 && !self.ended {
                return Ok(None);
            }
            if d.starts_with(b"ID3") && d.len() >= 10 {
                let size = d[6..10].iter().fold(0usize, |a, &b| (a << 7) | (b & 0x7F) as usize);
                let tag = 10 + size + if d[5] & 0x10 != 0 { 10 } else { 0 };
                if d.len() < tag {
                    return Ok(None);
                }
                self.start_90k = self.start_90k.or(apple_timestamp(&d[..tag]));
                self.pending.skip(tag);
            }
            self.id3_checked = true;
        }
        loop {
            let d = self.pending.data();
            if d.len() < 7 {
                return Ok(None);
            }
            // a tag between frames: HLS packed audio has one at each segment, with its time
            if d.starts_with(b"ID3") && self.locked.is_some() {
                if d.len() < 10 {
                    return Ok(None);
                }
                let size = d[6..10].iter().fold(0usize, |a, &b| (a << 7) | (b & 0x7F) as usize);
                let tag = 10 + size + if d[5] & 0x10 != 0 { 10 } else { 0 };
                if d.len() < tag {
                    return Ok(None);
                }
                if let (Some(t), Some(s)) = (apple_timestamp(&d[..tag]), self.streams.first()) {
                    self.pts = (t as i128 * s.time_base.den as i128 / 90_000) as i64;
                }
                self.pending.skip(tag);
                continue;
            }
            let Some(h) = adts::parse(&d[..7]) else {
                self.pending.skip(1);
                continue;
            };
            if d.len() < h.frame {
                return Ok(None);
            }
            match self.locked {
                // once the stream is known, a matching header is trusted without looking ahead
                Some(l) if l == (h.sfi, h.channels) => {}
                Some(_) => {
                    self.pending.skip(1);
                    continue;
                }
                // before that, the next frame must follow where this one says it ends
                None => {
                    if d.len() < h.frame + 7 && !self.ended {
                        return Ok(None);
                    }
                    let follows = d.get(h.frame..h.frame + 7).and_then(adts::parse).is_some_and(|n| (n.sfi, n.channels) == (h.sfi, h.channels));
                    if !follows && !(self.ended && d.len() == h.frame) {
                        self.pending.skip(1);
                        continue;
                    }
                    self.locked = Some((h.sfi, h.channels));
                    let rate = adts::RATES[h.sfi as usize];
                    if let Some(t) = self.start_90k {
                        self.pts = (t as i128 * rate as i128 / 90_000) as i64;
                    }
                    let asc = ((h.profile as u16 + 1) << 11) | ((h.sfi as u16) << 7) | ((h.channels as u16) << 3);
                    self.streams.push(StreamInfo {
                        index: 0,
                        codec: CodecId::Aac,
                        time_base: Rational::new(1, rate as i64),
                        duration: None,
                        bits: None,
                        extradata: asc.to_be_bytes().to_vec(),
                        lead_in: 0,
                        params: StreamParams::Audio { sample_rate: rate, channels: if h.channels == 7 { 8 } else { h.channels as u16 } },
                    });
                }
            }
            self.pending.skip(h.len);
            let data = self.pending.take(h.frame - h.len);
            let pts = self.pts;
            self.pts += 1024;
            return Ok(Some(StreamPacket { packet: Packet { stream: 0, pts, duration: 1024, data }, key: true }));
        }
    }
}

// ---- Annex B --------------------------------------------------------------------------------------

/// H.264 or HEVC NAL units with start codes, grouped into access units (one picture each).
/// Nothing comes out before a sequence parameter set and a keyframe, since a decoder could not
/// start earlier. The stream has no timestamps, so pictures are numbered in decoding order at the
/// given frame rate.
pub struct AnnexBStream {
    codec: CodecId,
    fps: Rational,
    pending: Pending,
    streams: Vec<StreamInfo>,
    /// The access unit being gathered: its bytes, whether it has a picture's slices yet, and
    /// whether they are a keyframe's.
    au: Vec<u8>,
    au_vcl: bool,
    au_key: bool,
    started: bool,
    count: i64,
    ready: VecDeque<StreamPacket>,
    ended: bool,
}

impl AnnexBStream {
    pub fn new(codec: CodecId, fps: Rational) -> Self {
        AnnexBStream {
            codec,
            fps,
            pending: Pending::default(),
            streams: Vec::new(),
            au: Vec::new(),
            au_vcl: false,
            au_key: false,
            started: false,
            count: 0,
            ready: VecDeque::new(),
            ended: false,
        }
    }

    fn hevc(&self) -> bool {
        self.codec == CodecId::Hevc
    }

    /// One whole NAL unit (header first, no start code).
    fn nal(&mut self, nal: &[u8]) {
        if nal.is_empty() {
            return;
        }
        let (vcl, first_slice, key, sps, starts_au) = if self.hevc() {
            let ty = (nal[0] >> 1) & 63;
            let vcl = ty < 32;
            (vcl, vcl && nal.get(2).is_some_and(|b| b & 0x80 != 0), (16..=23).contains(&ty), ty == 33, matches!(ty, 32..=35 | 39 | 41..=44 | 48..=55))
        } else {
            let ty = nal[0] & 31;
            let vcl = ty == 1 || ty == 5;
            // first_mb_in_slice is ue(v), so 0 is a single 1 bit
            (vcl, vcl && nal.get(1).is_some_and(|b| b & 0x80 != 0), ty == 5, ty == 7, matches!(ty, 6..=9 | 14..=18))
        };
        if self.au_vcl && (starts_au || first_slice) {
            self.finish_au();
        }
        if sps && self.streams.is_empty() {
            if let Some((w, h)) = ironav_codec::sps_picture_size(self.codec, nal) {
                self.streams.push(StreamInfo {
                    index: 0,
                    codec: self.codec,
                    time_base: self.fps.invert(),
                    duration: None,
                    bits: None,
                    extradata: Vec::new(),
                    lead_in: 0,
                    params: StreamParams::Video { width: w, height: h, pix_fmt: PixelFormat::Yuv420p, frame_rate: self.fps },
                });
            }
        }
        self.au.extend_from_slice(&[0, 0, 0, 1]);
        self.au.extend_from_slice(nal);
        self.au_vcl |= vcl;
        self.au_key |= key;
    }

    fn finish_au(&mut self) {
        let data = std::mem::take(&mut self.au);
        let (vcl, key) = (self.au_vcl, self.au_key);
        (self.au_vcl, self.au_key) = (false, false);
        if !vcl || self.streams.is_empty() || !(self.started || key) {
            return;
        }
        self.started = true;
        let pts = self.count;
        self.count += 1;
        self.ready.push_back(StreamPacket { packet: Packet { stream: 0, pts, duration: 1, data }, key });
    }
}

/// Where the next three-byte start code begins in `d`.
fn start_code(d: &[u8]) -> Option<usize> {
    d.windows(3).position(|w| w == [0, 0, 1])
}

impl StreamDemuxer for AnnexBStream {
    fn format_name(&self) -> &'static str {
        if self.hevc() { "hevc" } else { "h264" }
    }
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }
    fn feed(&mut self, bytes: &[u8]) -> Result<()> {
        self.pending.push(bytes)
    }
    fn end(&mut self) {
        self.ended = true;
    }
    fn next_packet(&mut self) -> Result<Option<StreamPacket>> {
        loop {
            if let Some(p) = self.ready.pop_front() {
                return Ok(Some(p));
            }
            let d = self.pending.data();
            let Some(s0) = start_code(d) else {
                // keep two bytes: they may be the start of a start code
                let keep = d.len().min(2);
                let n = d.len() - keep;
                self.pending.skip(n);
                if self.ended {
                    self.pending.skip(keep);
                    self.finish_au();
                    return Ok(self.ready.pop_front());
                }
                return Ok(None);
            };
            let body = s0 + 3;
            match start_code(&d[body..]) {
                Some(len) => {
                    let mut nal = &d[body..body + len];
                    // the zero before a four-byte start code is not part of this NAL unit
                    while let [rest @ .., 0] = nal {
                        nal = rest;
                    }
                    let nal = nal.to_vec();
                    self.pending.skip(body + len);
                    self.nal(&nal);
                }
                None if self.ended => {
                    let nal = d[body..].to_vec();
                    let n = d.len();
                    self.pending.skip(n);
                    self.nal(&nal);
                    self.finish_au();
                }
                None => {
                    self.pending.skip(s0);
                    return Ok(None);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironav_core::{Demuxer, Encoder, Frame, VideoFrame};

    /// Feeds `bytes` in chunks of varying size (1 to 97 bytes, then 4 KiB) and collects every packet.
    fn chunked(d: &mut dyn StreamDemuxer, bytes: &[u8]) -> Vec<StreamPacket> {
        let mut out = Vec::new();
        let mut pos = 0;
        let mut k = 0usize;
        while pos < bytes.len() {
            let n = if k < 200 { k % 97 + 1 } else { 4096 }.min(bytes.len() - pos);
            d.feed(&bytes[pos..pos + n]).unwrap();
            pos += n;
            k += 1;
            while let Some(p) = d.next_packet().unwrap() {
                out.push(p);
            }
        }
        d.end();
        while let Some(p) = d.next_packet().unwrap() {
            out.push(p);
        }
        out
    }

    fn y4m_bytes() -> Vec<u8> {
        let mut f = b"YUV4MPEG2 W8 H4 F30:1 Ip A1:1 C420jpeg\n".to_vec();
        for i in 0..5u8 {
            f.extend_from_slice(b"FRAME\n");
            f.extend((0..48).map(|k| k as u8 ^ i));
        }
        f
    }

    #[test]
    fn y4m_in_pieces_gives_the_same_frames_as_the_file_reader() {
        let bytes = y4m_bytes();
        let mut file = crate::y4m::Y4mDemuxer::open(std::io::Cursor::new(bytes.clone())).unwrap();
        let mut want = Vec::new();
        while let Some(p) = file.read_packet().unwrap() {
            want.push((p.pts, p.data));
        }
        let mut s = stream_demuxer(&bytes[..SNIFF_BYTES], &StreamOptions::default()).unwrap();
        let got: Vec<_> = chunked(s.as_mut(), &bytes).into_iter().map(|p| (p.packet.pts, p.packet.data)).collect();
        assert_eq!(got, want);
        assert_eq!(s.streams()[0].params, file.streams()[0].params);
    }

    #[test]
    fn wav_in_pieces_gives_every_sample_once() {
        let samples: Vec<u8> = (0..10_000u32).flat_map(|i| (i as i16).to_le_bytes()).collect();
        let mut f = b"RIFF\xff\xff\xff\xffWAVEfmt ".to_vec();
        f.extend_from_slice(&16u32.to_le_bytes());
        f.extend_from_slice(&[1, 0, 2, 0]);
        f.extend_from_slice(&8000u32.to_le_bytes());
        f.extend_from_slice(&32000u32.to_le_bytes());
        f.extend_from_slice(&[4, 0, 16, 0]);
        f.extend_from_slice(b"LIST\x04\x00\x00\x00abcd");
        // a streaming writer's unknown data size
        f.extend_from_slice(b"data\xff\xff\xff\xff");
        f.extend_from_slice(&samples);
        let mut s = stream_demuxer(&f[..SNIFF_BYTES], &StreamOptions::default()).unwrap();
        let got = chunked(s.as_mut(), &f);
        let joined: Vec<u8> = got.iter().flat_map(|p| p.packet.data.clone()).collect();
        assert_eq!(joined, samples);
        assert_eq!(got[1].packet.pts, 1024);
        assert_eq!(s.streams()[0].params, StreamParams::Audio { sample_rate: 8000, channels: 2 });
    }

    #[test]
    fn adts_in_pieces_gives_the_same_frames_as_the_file_reader() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../ironav-engine/tests/data/aac_stereo_tools.aac");
        let bytes = std::fs::read(path).unwrap();
        let mut file = crate::adts::open(std::io::Cursor::new(bytes.clone())).unwrap();
        let mut want = Vec::new();
        while let Some(p) = file.read_packet().unwrap() {
            want.push(p.data);
        }
        let mut s = stream_demuxer(&bytes[..SNIFF_BYTES], &StreamOptions::default()).unwrap();
        assert_eq!(s.format_name(), "aac");
        let got: Vec<_> = chunked(s.as_mut(), &bytes).into_iter().map(|p| p.packet.data).collect();
        assert_eq!(got.len(), want.len());
        assert!(got == want);
        assert_eq!(s.streams()[0].extradata, file.streams()[0].extradata);
    }

    #[test]
    fn annex_b_in_pieces_gives_one_packet_per_picture() {
        let (w, h) = (48u32, 32u32);
        let params = StreamParams::Video { width: w, height: h, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(25, 1) };
        let mut enc = ironav_codec::h264::H264Encoder::new(&params, Some(30), None).unwrap();
        let mut coded = Vec::new();
        for i in 0..12u32 {
            let y: Vec<u8> = (0..w * h).map(|k| ((k % w + i * 3) % 256) as u8).collect();
            let f = Frame::Video(VideoFrame { width: w, height: h, pix_fmt: PixelFormat::Yuv420p, pts: i as i64, planes: vec![y, vec![128; 384], vec![128; 384]] });
            coded.extend(enc.encode(&f).unwrap());
        }
        coded.extend(enc.flush().unwrap());
        let stream: Vec<u8> = coded.iter().flat_map(|p| p.data.clone()).collect();
        let mut s = stream_demuxer(&stream[..SNIFF_BYTES], &StreamOptions::default()).unwrap();
        assert_eq!(s.format_name(), "h264");
        let got = chunked(s.as_mut(), &stream);
        assert_eq!(got.len(), coded.len());
        for (g, c) in got.iter().zip(&coded) {
            // the same NAL units, start codes aside
            assert_eq!(nal_units(&g.packet.data), nal_units(&c.data));
        }
        assert!(got[0].key && !got[1].key);
        assert_eq!(s.streams()[0].params, params);
    }

    /// The NAL units of an Annex B buffer, without start codes.
    fn nal_units(d: &[u8]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut rest = d;
        while let Some(s) = start_code(rest) {
            rest = &rest[s + 3..];
            let end = start_code(rest).unwrap_or(rest.len());
            let mut nal = &rest[..end];
            while let [r @ .., 0] = nal {
                nal = r;
            }
            out.push(nal.to_vec());
        }
        out
    }

    #[test]
    fn annex_b_starts_at_a_keyframe() {
        // a slice with no parameter sets before it is dropped; nothing comes out
        let mut s = AnnexBStream::new(CodecId::H264, Rational::new(25, 1));
        s.feed(&[0, 0, 0, 1, 0x41, 0x80, 0, 0, 0, 1, 0x41, 0x80]).unwrap();
        s.end();
        assert!(s.next_packet().unwrap().is_none());
    }

    #[test]
    fn formats_are_told_apart_from_their_first_bytes() {
        assert_eq!(sniff(b"YUV4MPEG2 W8"), Some("y4m"));
        assert_eq!(sniff(b"RIFF\0\0\0\0WAVEfmt "), Some("wav"));
        assert_eq!(sniff(&[0, 0, 0, 1, 0x67, 0x64, 0, 0x1f]), Some("h264"));
        assert_eq!(sniff(&[0, 0, 0, 1, 0x40, 0x01, 0x0c, 0x01]), Some("hevc"));
        assert_eq!(sniff(b"\x1aE\xdf\xa3 matroska"), Some("matroska"));
        assert_eq!(sniff(b"OggS\0\x02"), Some("ogg"));
        // native FLAC has no frames that can be found without its header's help
        assert!(stream_demuxer(b"fLaC\0\0\0\x22", &StreamOptions::default()).is_err());
    }
}
