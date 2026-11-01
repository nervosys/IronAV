//! Matroska and WebM read as they arrive: what a browser's MediaRecorder, OBS or `ffmpeg -f
//! matroska -` sends down a socket or a pipe. Such a stream has a Segment and Clusters of unknown
//! size, no Cues and often no DefaultDuration, so nothing here seeks or looks ahead: elements are
//! taken as their bytes come, an unknown-size Cluster ends where the next top-level element begins,
//! and large elements that are not wanted (Cues, Tags, attachments) are stepped over without being
//! held.
//!
//! Tracks, codec configuration and audio timing follow the file reader (the parent module), so the
//! two give the same packets. Packets are held until every track has shown a frame, and video until
//! a few pictures give its frame rate (when DefaultDuration does not); video starts at a keyframe.

use super::*;
use crate::stream::{StreamDemuxer, StreamPacket};
use ironav_core::Packet;
use std::collections::VecDeque;

/// The largest element held whole (a block, the track list).
const MAX_ELEMENT: u64 = 64 << 20;
/// Video pictures held to measure the frame rate when the track does not give it.
const RATE_UNITS: usize = 4;
/// Bytes held waiting for every track to show itself; past this the tracks seen so far are used.
const MAX_HELD: usize = 16 << 20;
/// What a held unit costs beyond its bytes, so empty blocks also count toward [`MAX_HELD`].
const UNIT_COST: usize = 64;
/// The largest TimestampScale taken (10 s per tick; files use 1 ms).
const MAX_SCALE: u64 = 10_000_000_000;
/// The largest Cluster timestamp taken, in ticks: 35 years at 1 ms. Keeps the time arithmetic in range.
const MAX_TC: i64 = 1 << 40;

#[derive(Clone, Copy, PartialEq, Debug)]
enum Level {
    Top,
    Segment,
    Cluster,
}

struct Track {
    t: MkTrack,
    codec: CodecId,
    video: bool,
    config: Option<Config>,
    convert: bool,
    /// Index among the published streams.
    out: Option<usize>,
    info: Option<StreamInfo>,
    started: bool,
    /// Whether the parameter sets have gone out in front of a picture yet.
    primed: bool,
    /// Audio: samples per frame (Opus: per packet, read from it), and the running clock.
    fixed: Option<u32>,
    rate: u32,
    next: Option<i64>,
    default_ticks: i64,
    /// Video frame duration in ticks.
    frame: i64,
}

/// A frame of a block, before it becomes a packet.
struct Unit {
    tc: i64,
    lace: usize,
    key: bool,
    data: Vec<u8>,
    duration: Option<i64>,
    discard: i64,
}

pub struct MkvStream {
    buf: Vec<u8>,
    pos: usize,
    /// Absolute offset of `buf[0]` in the stream.
    abs: u64,
    level: Level,
    segment_end: Option<u64>,
    cluster_end: Option<u64>,
    cluster_tc: i64,
    skip: u64,
    scale: u64,
    webm: bool,
    tracks: Vec<Track>,
    streams: Vec<StreamInfo>,
    ready: bool,
    held: Vec<(usize, Unit)>,
    held_bytes: usize,
    /// Units held per track.
    held_count: Vec<usize>,
    out: VecDeque<StreamPacket>,
    ended: bool,
}

impl Default for MkvStream {
    fn default() -> Self {
        MkvStream {
            buf: Vec::new(),
            pos: 0,
            abs: 0,
            level: Level::Top,
            segment_end: None,
            cluster_end: None,
            cluster_tc: 0,
            skip: 0,
            scale: 1_000_000,
            webm: false,
            tracks: Vec::new(),
            streams: Vec::new(),
            ready: false,
            held: Vec::new(),
            held_bytes: 0,
            held_count: Vec::new(),
            out: VecDeque::new(),
            ended: false,
        }
    }
}

/// An element id at the start of `d` (marker bits kept), and its length; `None` until enough bytes.
fn read_id(d: &[u8]) -> Result<Option<(u32, usize)>> {
    let Some(&b) = d.first() else { return Ok(None) };
    let n = b.leading_zeros() as usize + 1;
    if n > 4 {
        return Err(bad("an element id is malformed"));
    }
    Ok(d.get(..n).map(|v| (v.iter().fold(0u32, |a, &b| a << 8 | b as u32), n)))
}

/// A variable-length integer at the start of `d`, its length, and whether it is all ones (an
/// unknown size); `None` until enough bytes.
fn read_vint(d: &[u8]) -> Result<Option<(u64, usize, bool)>> {
    let Some(&b) = d.first() else { return Ok(None) };
    let n = b.leading_zeros() as usize + 1;
    if n > 8 {
        return Err(bad("an element size is malformed"));
    }
    let Some(v) = d.get(..n) else { return Ok(None) };
    let mut x = v[0] as u64 & ((1u64 << (8 - n)) - 1);
    for &b in &v[1..] {
        x = x << 8 | b as u64;
    }
    Ok(Some((x, n, x == (1u64 << (7 * n)) - 1)))
}

/// Length-prefixed NAL units as Annex B.
fn annexb(d: &[u8], len: usize, prefix: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(d.len() + prefix.len() + 16);
    out.extend_from_slice(prefix);
    let mut i = 0;
    while len > 0 && i + len <= d.len() {
        let n = d[i..i + len].iter().fold(0usize, |a, &b| a << 8 | b as usize);
        i += len;
        let Some(nal) = d.get(i..i + n) else { break };
        out.extend_from_slice(&[0, 0, 0, 1]);
        out.extend_from_slice(nal);
        i += n;
    }
    out
}

impl MkvStream {
    fn here(&self) -> u64 {
        self.abs + self.pos as u64
    }

    /// One step of the walk; false when more bytes are needed.
    fn step(&mut self) -> Result<bool> {
        if self.skip > 0 {
            let n = (self.buf.len() - self.pos).min(self.skip as usize);
            self.pos += n;
            self.skip -= n as u64;
            return Ok(n > 0);
        }
        if self.level == Level::Cluster && self.cluster_end.is_some_and(|e| self.here() >= e) {
            self.level = Level::Segment;
        }
        if self.level == Level::Segment && self.segment_end.is_some_and(|e| self.here() >= e) {
            self.level = Level::Top;
        }
        let d = &self.buf[self.pos..];
        let Some((id, n)) = read_id(d)? else { return Ok(false) };
        let Some((size, m, unknown)) = read_vint(&d[n..])? else { return Ok(false) };
        let size = (!unknown).then_some(size);
        if self.level == Level::Cluster && self.cluster_end.is_none() && (is_level1(id) || id == EBML) {
            // an unknown-size cluster ends where the next top-level element begins
            self.level = Level::Segment;
            return Ok(true);
        }
        let start = self.here() + (n + m) as u64;
        match (self.level, id) {
            (Level::Top, SEGMENT) => {
                self.pos += n + m;
                self.level = Level::Segment;
                self.segment_end = size.map(|s| start + s);
                return Ok(true);
            }
            (Level::Segment, CLUSTER) => {
                self.pos += n + m;
                self.level = Level::Cluster;
                self.cluster_end = size.map(|s| start + s);
                self.cluster_tc = 0;
                return Ok(true);
            }
            (Level::Segment, EBML) => {
                // a second stream chained after the first
                self.level = Level::Top;
                return Ok(true);
            }
            (Level::Top, EBML) | (Level::Segment, TRACKS | INFO) | (Level::Cluster, CLUSTER_TIMECODE | SIMPLE_BLOCK | BLOCK_GROUP) => {}
            _ => {
                let Some(size) = size else { return Err(bad(format!("element {id:#x} has unknown size"))) };
                self.pos += n + m;
                self.skip = size;
                return Ok(true);
            }
        }
        let Some(size) = size else { return Err(bad(format!("element {id:#x} has unknown size"))) };
        if size > MAX_ELEMENT {
            return Err(bad(format!("element {id:#x} is implausibly large ({size} bytes)")));
        }
        let end = n + m + size as usize;
        if d.len() < end {
            return Ok(false);
        }
        let body = d[n + m..end].to_vec();
        self.pos += end;
        self.element(id, &body)?;
        Ok(true)
    }

    fn element(&mut self, id: u32, body: &[u8]) -> Result<()> {
        match id {
            EBML => {
                for e in Elems(body) {
                    let (id, v) = e?;
                    if id == DOC_TYPE {
                        self.webm = String::from_utf8_lossy(v).trim_end_matches('\0') == "webm";
                    }
                }
            }
            INFO => {
                for e in Elems(body) {
                    let (id, v) = e?;
                    if id == TIMECODE_SCALE {
                        self.scale = be_uint(v)?.max(1);
                        if self.scale > MAX_SCALE {
                            return Err(bad(format!("a TimestampScale of {} ns is implausible", self.scale)));
                        }
                    }
                }
            }
            TRACKS if self.tracks.is_empty() => self.set_tracks(body)?,
            CLUSTER_TIMECODE => {
                let tc = be_uint(body)?;
                if tc > MAX_TC as u64 {
                    return Err(bad(format!("a cluster timestamp of {tc} ticks is implausible")));
                }
                self.cluster_tc = tc as i64;
            }
            SIMPLE_BLOCK => self.block(body, None, None, 0)?,
            BLOCK_GROUP => {
                let (mut block, mut duration, mut referenced, mut discard) = (None, None, false, 0i64);
                for e in Elems(body) {
                    let (id, v) = e?;
                    match id {
                        BLOCK => block = Some(v),
                        BLOCK_DURATION => duration = Some(be_uint(v)? as i64),
                        REFERENCE_BLOCK => referenced = true,
                        DISCARD_PADDING if !v.is_empty() && v.len() <= 8 => {
                            // a signed integer
                            let u = be_uint(v)?;
                            let shift = 64 - 8 * v.len() as u32;
                            discard = ((u << shift) as i64) >> shift;
                        }
                        _ => {}
                    }
                }
                if let Some(b) = block {
                    self.block(b, Some(!referenced), duration, discard)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn set_tracks(&mut self, body: &[u8]) -> Result<()> {
        for t in parse_tracks(body)? {
            let video = match t.kind {
                1 => true,
                2 => false,
                _ => continue,
            };
            let Some(codec) = (if video { video_codec(&t.codec) } else { audio_codec(&t.codec) }) else { continue };
            if t.encoded {
                return Err(Error::new(ErrorCode::UnsupportedFormat, format!("track {} uses a Matroska content encoding (header stripping, compression or encryption)", t.number)));
            }
            let config = if video {
                match codec {
                    CodecId::H264 => Some(parse_avcc(&t.private)?),
                    CodecId::Hevc => Some(parse_hvcc(&t.private)?),
                    CodecId::Vp9 => None, // from the first keyframe
                    CodecId::Av1 if !t.private.is_empty() => Some(av1c_config(&t.private)?),
                    _ => Some(Config { prefix: Vec::new(), nal_length: 0, bits: 8, chroma: 1 }),
                }
            } else {
                Some(Config { prefix: Vec::new(), nal_length: 0, bits: 8, chroma: 1 })
            };
            let opus = codec == CodecId::Opus;
            let rate = if opus { 48_000 } else { t.rate.round() as u32 };
            let fixed = if video { None } else { audio_frame_len(&t, codec).filter(|_| rate > 0).or(if opus { Some(0) } else { None }) };
            let default_ticks = (t.default_ns / self.scale).min(MAX_TC as u64) as i64;
            self.tracks.push(Track {
                codec,
                video,
                config,
                convert: matches!(codec, CodecId::H264 | CodecId::Hevc),
                out: None,
                info: None,
                started: false,
                primed: false,
                fixed,
                rate,
                next: None,
                default_ticks,
                frame: default_ticks,
                t,
            });
        }
        if self.tracks.is_empty() {
            return Err(Error::new(ErrorCode::UnsupportedCodec, "the stream has no audio or video track IronAV recognises"));
        }
        self.held_count = vec![0; self.tracks.len()];
        Ok(())
    }

    /// A Block or SimpleBlock; `key` is `None` for a SimpleBlock (its flags say).
    fn block(&mut self, b: &[u8], key: Option<bool>, duration: Option<i64>, discard: i64) -> Result<()> {
        let Some((number, n, _)) = read_vint(b)? else { return Err(bad("a block is cut short")) };
        if b.len() < n + 3 {
            return Err(bad("a block is cut short"));
        }
        let Some(k) = self.tracks.iter().position(|t| t.t.number == number) else { return Ok(()) };
        let rel = i16::from_be_bytes([b[n], b[n + 1]]) as i64;
        let flags = b[n + 2];
        let key = key.unwrap_or(flags & 0x80 != 0);
        let tc = self.cluster_tc.saturating_add(rel);
        let mut d = &b[n + 3..];
        let frames: Vec<&[u8]> = match (flags >> 1) & 3 {
            0 => vec![d],
            lacing => {
                let count = *d.first().ok_or_else(|| bad("a laced block is cut short"))? as usize + 1;
                d = &d[1..];
                let mut sizes = Vec::with_capacity(count);
                match lacing {
                    1 => {
                        for _ in 0..count - 1 {
                            let mut s = 0usize;
                            loop {
                                let (&x, rest) = d.split_first().ok_or_else(|| bad("a Xiph lace is cut short"))?;
                                d = rest;
                                s += x as usize;
                                if x != 255 {
                                    break;
                                }
                            }
                            sizes.push(s);
                        }
                    }
                    3 => {
                        let mut prev = 0i64;
                        for i in 0..count - 1 {
                            let (v, len, _) = read_vint(d)?.ok_or_else(|| bad("an EBML lace is cut short"))?;
                            d = &d[len..];
                            prev = if i == 0 { v as i64 } else { prev + v as i64 - ((1i64 << (7 * len - 1)) - 1) };
                            sizes.push(usize::try_from(prev).map_err(|_| bad("an EBML lace size is negative"))?);
                        }
                    }
                    _ => {
                        if !d.len().is_multiple_of(count) {
                            return Err(bad("a fixed lace does not divide its block"));
                        }
                        sizes = vec![d.len() / count; count - 1];
                    }
                }
                let mut out = Vec::with_capacity(count);
                for s in sizes {
                    let (f, rest) = (d.get(..s).ok_or_else(|| bad("a lace runs past its block"))?, &d[s..]);
                    out.push(f);
                    d = rest;
                }
                out.push(d);
                out
            }
        };
        let single = frames.len() == 1;
        for (lace, f) in frames.into_iter().enumerate() {
            self.unit(k, Unit { tc, lace, key, data: f.to_vec(), duration: duration.filter(|_| single), discard });
        }
        Ok(())
    }

    fn unit(&mut self, k: usize, u: Unit) {
        let t = &mut self.tracks[k];
        if t.video && !t.started {
            if !u.key {
                return;
            }
            t.started = true;
            if t.config.is_none() {
                let (bits, chroma) = vp9_format(&u.data[..u.data.len().min(16)]).unwrap_or((8, 1));
                t.config = Some(Config { prefix: Vec::new(), nal_length: 0, bits, chroma });
            }
        }
        if self.ready {
            self.emit(k, u);
            return;
        }
        self.held_bytes += u.data.len() + UNIT_COST;
        self.held_count[k] += 1;
        self.held.push((k, u));
        let all = self.tracks.iter().zip(&self.held_count).all(|(t, &n)| if t.video && t.default_ticks == 0 { n >= RATE_UNITS } else { n >= 1 });
        if all || self.held_bytes > MAX_HELD {
            self.publish();
        }
    }

    fn publish(&mut self) {
        if self.ready {
            return;
        }
        self.ready = true;
        let held = std::mem::take(&mut self.held);
        self.held_bytes = 0;
        let g = gcd(self.scale as i64, 1_000_000_000);
        let tick = Rational::new(self.scale as i64 / g, 1_000_000_000 / g);
        for (k, t) in self.tracks.iter_mut().enumerate() {
            let tcs: Vec<i64> = held.iter().filter(|(j, _)| *j == k).map(|(_, u)| u.tc).collect();
            if tcs.is_empty() {
                continue;
            }
            let config = t.config.as_ref().unwrap();
            let (time_base, params, extradata, bits) = if t.video {
                let mut s = tcs.clone();
                s.sort_unstable();
                let measured = s.windows(2).map(|w| w[1] - w[0]).filter(|&d| d > 0).min().unwrap_or(0);
                if t.frame <= 0 {
                    t.frame = measured.max(1);
                }
                let fps = if t.t.default_ns > 0 {
                    1e9 / t.t.default_ns as f64
                } else if measured > 0 {
                    1.0 / (measured as f64 * tick.as_f64())
                } else {
                    0.0
                };
                let extradata = if t.codec == CodecId::Av1 { t.t.private.clone() } else { Vec::new() };
                (tick, StreamParams::Video { width: t.t.width, height: t.t.height, pix_fmt: config.pix_fmt(), frame_rate: frame_rate(fps) }, extradata, None)
            } else {
                let keep = matches!(t.codec, CodecId::Aac | CodecId::Opus | CodecId::Mp3 | CodecId::Mp2 | CodecId::Vorbis);
                let tb = if t.fixed.is_some() { Rational::new(1, t.rate as i64) } else { tick };
                (tb, StreamParams::Audio { sample_rate: t.rate, channels: t.t.channels }, if keep { t.t.private.clone() } else { Vec::new() }, if keep { None } else { t.t.bits })
            };
            let info = StreamInfo { index: self.streams.len(), codec: t.codec, time_base, duration: None, bits, extradata, lead_in: 0, params };
            t.out = Some(info.index);
            t.info = Some(info.clone());
            self.streams.push(info);
        }
        for (k, u) in held {
            self.emit(k, u);
        }
    }

    fn emit(&mut self, k: usize, u: Unit) {
        let scale = self.scale;
        let t = &mut self.tracks[k];
        let Some(index) = t.out else { return };
        let (pts, duration, data) = if t.video {
            let data = if t.convert {
                let c = t.config.as_ref().unwrap();
                let prefix = if u.key || !t.primed { &c.prefix[..] } else { &[][..] };
                t.primed = true;
                annexb(&u.data, c.nal_length, prefix)
            } else {
                u.data
            };
            (u.tc, t.frame, data)
        } else if let Some(fixed) = t.fixed {
            let len = if t.codec == CodecId::Opus {
                let n = if u.data.is_empty() { 960 } else { ironav_codec::opus::packet_samples(&u.data[..u.data.len().min(2)]).unwrap_or(960) as u32 };
                // negative padding means nothing (as in the file reader)
                n.saturating_sub((u.discard.max(0) as i128 * 48_000 / 1_000_000_000).min(u32::MAX as i128) as u32)
            } else {
                fixed
            };
            let rate = t.rate as i128;
            let at = ((u.tc as i128 * scale as i128 * rate + 500_000_000) / 1_000_000_000) as i64 + u.lace as i64 * len as i64;
            let tolerance = t.rate as i64 / 500 + 1;
            let pts = match t.next {
                Some(n) if (n - at).abs() <= tolerance => n,
                _ => at,
            };
            t.next = Some(pts + len as i64);
            (pts, len as i64, u.data)
        } else {
            (u.tc.saturating_add(u.lace as i64 * t.default_ticks), u.duration.filter(|&d| d > 0).unwrap_or(t.default_ticks), u.data)
        };
        self.out.push_back(StreamPacket { packet: Packet { stream: index, pts, duration, data }, key: u.key || !t.video });
    }
}

impl StreamDemuxer for MkvStream {
    fn format_name(&self) -> &'static str {
        if self.webm { "webm" } else { "matroska" }
    }
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }
    fn feed(&mut self, bytes: &[u8]) -> Result<()> {
        self.buf.extend_from_slice(bytes);
        while self.step()? {}
        // keep what is not used yet
        self.buf.drain(..self.pos);
        self.abs += self.pos as u64;
        self.pos = 0;
        Ok(())
    }
    fn next_packet(&mut self) -> Result<Option<StreamPacket>> {
        Ok(self.out.pop_front())
    }
    fn end(&mut self) {
        if !self.ended {
            self.ended = true;
            self.publish();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironav_core::Demuxer;
    use std::collections::BTreeMap;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    /// Per stream: each packet's bytes and its time from the stream's first, in order.
    fn by_stream(packets: impl IntoIterator<Item = Packet>) -> BTreeMap<usize, Vec<(i64, Vec<u8>)>> {
        let mut m: BTreeMap<usize, Vec<(i64, Vec<u8>)>> = BTreeMap::new();
        let mut first = BTreeMap::new();
        for p in packets {
            let f = *first.entry(p.stream).or_insert(p.pts);
            m.entry(p.stream).or_default().push((p.pts - f, p.data));
        }
        m
    }

    fn live(data: &[u8], piece: usize) -> (MkvStream, Vec<Packet>) {
        let mut s = MkvStream::default();
        let mut got = Vec::new();
        for c in data.chunks(piece) {
            s.feed(c).unwrap();
            while let Some(p) = s.next_packet().unwrap() {
                got.push(p.packet);
            }
        }
        s.end();
        while let Some(p) = s.next_packet().unwrap() {
            got.push(p.packet);
        }
        (s, got)
    }

    #[test]
    fn streamed_matroska_gives_the_file_readers_packets() {
        // FFmpeg's matroska muxer in live mode (unknown-size segment, no cues): H.264 with B-frames
        // and AAC; and a WebM rewritten with unknown-size clusters too, as MediaRecorder writes it
        for name in ["h264_aac_live.mkv", "vp9_opus_unsized.webm"] {
            let data = fixture(name);
            let mut file = super::super::open(std::io::Cursor::new(data.clone())).unwrap();
            let want_streams = file.streams().to_vec();
            let want = by_stream(std::iter::from_fn(|| file.read_packet().unwrap()));
            for piece in [1, 97, 4096, data.len()] {
                let (s, got) = live(&data, piece);
                assert_eq!(s.streams().len(), want_streams.len(), "{name}");
                for (a, b) in s.streams().iter().zip(&want_streams) {
                    assert_eq!((a.codec, a.time_base, &a.extradata), (b.codec, b.time_base, &b.extradata), "{name}");
                    assert_eq!(serde_json::to_value(&a.params).unwrap(), serde_json::to_value(&b.params).unwrap(), "{name}");
                }
                let got = by_stream(got);
                for (k, w) in &want {
                    assert_eq!(got[k].len(), w.len(), "{name} stream {k} pieces of {piece}");
                    assert!(got[k] == *w, "{name} stream {k} pieces of {piece}");
                }
            }
            assert_eq!(live(&data, 4096).0.format_name(), if name.ends_with("webm") { "webm" } else { "matroska" });
        }
    }

    /// An element with an eight-byte size.
    fn el(id: &[u8], body: &[u8]) -> Vec<u8> {
        let mut v = id.to_vec();
        v.push(0x01);
        v.extend_from_slice(&(body.len() as u64).to_be_bytes()[1..]);
        v.extend_from_slice(body);
        v
    }

    /// A header, a VP8 track and an Opus track, and the start of an unknown-size cluster.
    fn two_tracks() -> Vec<u8> {
        let video = [el(&[0xD7], &[1]), el(&[0x83], &[1]), el(&[0x86], b"V_VP8"), el(&[0xE0], &[el(&[0xB0], &[16]), el(&[0xBA], &[16])].concat())].concat();
        let audio = [el(&[0xD7], &[2]), el(&[0x83], &[2]), el(&[0x86], b"A_OPUS"), el(&[0xE1], &[el(&[0x9F], &[2])].concat())].concat();
        let tracks = el(&[0x16, 0x54, 0xAE, 0x6B], &[el(&[0xAE], &video), el(&[0xAE], &audio)].concat());
        let mut d = el(&[0x1A, 0x45, 0xDF, 0xA3], &el(&[0x42, 0x82], b"webm"));
        d.extend_from_slice(&[0x18, 0x53, 0x80, 0x67, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        d.extend(tracks);
        d.extend_from_slice(&[0x1F, 0x43, 0xB6, 0x75, 0x01, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF]);
        d
    }

    #[test]
    fn hostile_timestamps_and_floods_of_empty_blocks_are_survived() {
        // a cluster timestamp that would overflow the time arithmetic is an error, not a panic
        let mut d = two_tracks();
        d.extend(el(&[0xE7], &0x7FFF_FFFF_FFFF_FFFFu64.to_be_bytes()));
        d.extend(el(&[0xA3], &[0x81, 0x7F, 0xFF, 0x80]));
        assert!(MkvStream::default().feed(&d).is_err());

        // empty keyframes for the video while the audio never comes: the streams are published once
        // the held units reach the cap, rather than holding everything forever
        let mut d = two_tracks();
        d.extend(el(&[0xE7], &[0]));
        let block = el(&[0xA3], &[0x81, 0, 0, 0x80]);
        let n = MAX_HELD / UNIT_COST + 10;
        let t0 = std::time::Instant::now();
        let mut s = MkvStream::default();
        s.feed(&d).unwrap();
        let mut got = 0;
        for _ in 0..n / 1000 + 1 {
            s.feed(&block.repeat(1000)).unwrap();
            while s.next_packet().unwrap().is_some() {
                got += 1;
            }
        }
        assert_eq!(s.streams().len(), 1);
        assert!(got > n - 2000, "{got}");
        assert!(t0.elapsed() < std::time::Duration::from_secs(20), "{:?}", t0.elapsed());
    }

    #[test]
    fn damaged_input_never_panics() {
        let good = fixture("vp9_opus_unsized.webm");
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut rand = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for round in 0..200 {
            let mut d = good.clone();
            for _ in 0..1 + round % 30 {
                let i = rand() as usize % d.len();
                d[i] = rand() as u8;
            }
            let mut s = MkvStream::default();
            for c in d.chunks(1 + rand() as usize % 2000) {
                if s.feed(c).is_err() {
                    break;
                }
                while s.next_packet().unwrap().is_some() {}
            }
            s.end();
            while s.next_packet().unwrap().is_some() {}
        }
    }
}
