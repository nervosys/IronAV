//! MPEG-1 and MPEG-2 video outside a container: elementary streams (`.m2v`) and program streams
//! (`.mpg`, `.vob`). Neither has an index or a timing of its own worth trusting, so an elementary
//! stream's pictures come out in decoding order, one packet per frame (the two fields of an
//! interlaced frame together), numbered in display order from each picture's temporal reference.
//! Its time base is the frame duration, so a packet's time is its display index.
//!
//! A program stream is read through its PES packets, and every stream in it that can be decoded
//! comes out: each video stream (0xE0-0xEF), MPEG audio (0xC0-0xDF, MP2 or MP3 by each frame's
//! layer), and the private stream 1 substreams DVDs use: AC-3 (0x80-0x87) and DVD linear PCM
//! (0xA0-0xA7, 16, 20 or 24 bits, converted to little-endian). DTS (0x88-0x8F), subpictures, DVD-Audio
//! PCM and other private streams are skipped. Times are PES presentation timestamps on the 90 kHz
//! clock; every stream starts at the earliest of them, as FFmpeg's start time does, so audio and
//! video stay aligned. A video stream's pictures are placed from its first timestamp by display
//! index, and a stream that first shows up after the first few megabytes is not read.

use ironav_codec::mp3::parse_frame;
use ironav_core::{CodecId, Demuxer, Error, ErrorCode, Packet, PixelFormat, Rational, Result, StreamInfo, StreamParams};
use std::collections::VecDeque;
use std::io::Read;

/// Bytes read from the file at a time.
const CHUNK: usize = 1 << 20;
/// How much of a file is read, at most, looking for the streams and their first sequence header.
const MAX_HEAD: usize = 16 << 20;
/// The 90 kHz clock of PES timestamps.
const CLOCK: i64 = 90_000;
/// PES timestamps are 33 bits.
const WRAP: i64 = 1 << 33;

/// Whether `m`, the first bytes of a file, open a video elementary stream (a sequence header).
pub fn looks_like_es(m: &[u8]) -> bool {
    m.starts_with(&[0, 0, 1, 0xB3])
}

/// Whether `m` open a program stream (a pack header).
pub fn looks_like_ps(m: &[u8]) -> bool {
    m.starts_with(&[0, 0, 1, 0xBA])
}

/// One frame of an elementary stream.
pub(crate) struct Unit {
    pub data: Vec<u8>,
    /// The frame's place in display order.
    pub index: i64,
}

/// Cuts a video elementary stream into frames. A frame starts at a sequence header, a group of
/// pictures header or a picture header, unless it is the second field of a frame.
#[derive(Default)]
pub(crate) struct Framer {
    buf: Vec<u8>,
    scan: usize,
    has_pic: bool,
    field_open: bool,
    gop_pending: bool,
    saw_gop: bool,
    tr: i64,
    base: i64,
    high: i64,
    pub out: VecDeque<Unit>,
    pub info: Option<ironav_codec::mpeg2::SequenceInfo>,
}

fn find_start(d: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 3 <= d.len() {
        if d[i + 2] > 1 {
            i += 3;
        } else if d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1 {
            return Some(i);
        } else {
            i += 1;
        }
    }
    None
}

impl Framer {
    pub fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
        self.run(false);
    }

    /// No more input: the frame in progress is complete.
    pub fn end(&mut self) {
        self.run(true);
        self.emit(self.buf.len());
    }

    fn emit(&mut self, upto: usize) {
        if upto == 0 || !self.has_pic {
            return;
        }
        let data: Vec<u8> = self.buf.drain(..upto).collect();
        if self.info.is_none() {
            self.info = ironav_codec::mpeg2::sequence_info(&data);
        }
        let index = self.base + self.tr;
        self.high = self.high.max(index + 1);
        self.out.push_back(Unit { data, index });
        self.has_pic = false;
        self.field_open = false;
        self.scan = 0;
    }

    fn run(&mut self, last: bool) {
        while let Some(i) = find_start(&self.buf, self.scan) {
            if i + 4 > self.buf.len() {
                break;
            }
            let code = self.buf[i + 3];
            let boundary = match code {
                0xB3 | 0xB8 => self.has_pic,
                0x00 => self.has_pic && !self.field_open,
                _ => false,
            };
            if boundary {
                self.emit(i);
                continue;
            }
            match code {
                0xB8 => {
                    self.gop_pending = true;
                    self.saw_gop = true;
                }
                0x00 => {
                    if i + 6 > self.buf.len() && !last {
                        break;
                    }
                    let (b4, b5) = (*self.buf.get(i + 4).unwrap_or(&0) as i64, *self.buf.get(i + 5).unwrap_or(&0) as i64);
                    let (tr, ty) = ((b4 << 2) | (b5 >> 6), (b5 >> 3) & 7);
                    if !self.has_pic {
                        // the group of pictures header starts a new run of temporal references
                        if self.gop_pending || (!self.saw_gop && ty == 1 && self.base + tr < self.high) {
                            self.base = self.high;
                        }
                        self.gop_pending = false;
                        self.tr = tr;
                    }
                    self.has_pic = true;
                }
                0xB5 => {
                    if i + 7 > self.buf.len() && !last {
                        break;
                    }
                    // the picture coding extension: a field picture opens or closes a frame
                    if self.buf.get(i + 4).is_some_and(|b| b >> 4 == 8) {
                        let structure = self.buf.get(i + 6).map_or(3, |b| b & 3);
                        self.field_open = if structure == 3 || structure == 0 { false } else { !self.field_open };
                    }
                }
                _ => {}
            }
            self.scan = i + 4;
        }
        // keep the scan position for the next push, a little behind in case a start code is split
        self.scan = self.scan.min(self.buf.len().saturating_sub(3));
    }
}

/// The chroma format an MPEG-2 video track's codec configuration declares (the sequence header and
/// its extension, in the extradata of an MP4 or Matroska track): 2 for 4:2:2, otherwise 1. MPEG-1 and
/// MPEG-4 are 4:2:0 here.
pub(crate) fn chroma_of(codec: CodecId, extradata: &[u8]) -> u8 {
    if codec != CodecId::Mpeg2Video {
        return 1;
    }
    match ironav_codec::mpeg2::sequence_info(extradata) {
        Some(s) if s.chroma_format == 2 => 2,
        _ => 1,
    }
}

/// The stream a framer has found, once it has found its first sequence header.
fn stream_of(info: &ironav_codec::mpeg2::SequenceInfo) -> StreamInfo {
    let (n, d) = info.frame_rate;
    StreamInfo {
        index: 0,
        codec: if info.mpeg2 { CodecId::Mpeg2Video } else { CodecId::Mpeg1Video },
        time_base: Rational::new(d as i64, n as i64),
        duration: None,
        bits: None,
        extradata: Vec::new(),
        lead_in: 0,
        params: StreamParams::Video {
            width: info.width,
            height: info.height,
            pix_fmt: if info.chroma_format == 2 { PixelFormat::Yuv422p } else { PixelFormat::Yuv420p },
            frame_rate: Rational::new(n as i64, d as i64),
        },
    }
}

/// Ticks of the 90 kHz clock for `index` frames at `rate` (frames per second as numerator and
/// denominator), rounded.
fn ticks(index: i64, rate: (u32, u32)) -> i64 {
    let (n, d) = (rate.0.max(1) as i64, rate.1.max(1) as i64);
    (index * d * CLOCK + n / 2) / n
}

/// A timestamp of 33 bits, as a PES header carries it (the marker bits are ignored).
fn read_ts(b: &[u8]) -> i64 {
    (((b[0] as i64 >> 1) & 7) << 30) | ((b[1] as i64) << 22) | ((b[2] as i64 >> 1) << 15) | ((b[3] as i64) << 7) | (b[4] as i64 >> 1)
}

/// The PTS of a PES packet `p` (its start code included), if it has one, and where its payload starts.
fn pes_header(p: &[u8]) -> Option<(Option<i64>, usize)> {
    if p.len() < 7 {
        return None;
    }
    if p[6] >> 6 == 2 {
        // MPEG-2 PES: flags, then the length of the optional header that holds the timestamps
        if p.len() < 9 {
            return None;
        }
        let pts = if p[7] & 0x80 != 0 { p.get(9..14).map(read_ts) } else { None };
        return Some((pts, 9 + p[8] as usize));
    }
    // MPEG-1 PES: stuffing, an optional buffer size, then the timestamps
    let mut k = 6;
    while p.get(k) == Some(&0xFF) {
        k += 1;
    }
    if p.get(k).is_some_and(|b| b >> 6 == 1) {
        k += 2;
    }
    Some(match p.get(k) {
        Some(b) if b >> 4 == 2 => (p.get(k..k + 5).map(read_ts), k + 5),
        Some(b) if b >> 4 == 3 => (p.get(k..k + 5).map(read_ts), k + 10),
        _ => (None, k + 1),
    })
}

/// The kinds of stream a program stream's PES packets carry, as far as they are read.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Video,
    /// MPEG audio, Layer II or III by each frame.
    Mpeg,
    Ac3,
    /// DVD linear PCM.
    Lpcm,
}

/// One stream of the program stream: a PES stream id, or private stream 1 with its substream.
struct Track {
    kind: Kind,
    key: u16,
    /// Video: the frames.
    framer: Framer,
    /// Video: the first PTS seen, and the PTS of display index 0 (set with the first frame).
    start: Option<i64>,
    origin: Option<i64>,
    /// The last PTS seen, unwrapped, for the wrap at 2^33.
    last: Option<i64>,
    /// Audio: the stream's parameters, once its first frame is read.
    info: Option<StreamInfo>,
    /// Audio: bytes not yet framed, the time of the next frame and the fraction of a tick it owes.
    carry: Vec<u8>,
    next: Option<i64>,
    rem: i64,
    /// LPCM: the bits, rate and channels the carried bytes are in.
    lpcm: Option<(u8, u32, usize)>,
    /// The index of the stream in the demuxer's streams, once they are published.
    out: Option<usize>,
}

impl Track {
    fn new(key: u16, kind: Kind) -> Self {
        Track { kind, key, framer: Framer::default(), start: None, origin: None, last: None, info: None, carry: Vec::new(), next: None, rem: 0, lpcm: None, out: None }
    }

    fn has_info(&self) -> bool {
        if self.kind == Kind::Video {
            self.framer.info.is_some()
        } else {
            self.info.is_some()
        }
    }

    fn stream_info(&self, ps: bool) -> Option<StreamInfo> {
        if self.kind == Kind::Video {
            let mut s = stream_of(self.framer.info.as_ref()?);
            if ps {
                s.time_base = Rational::new(1, CLOCK);
            }
            Some(s)
        } else {
            self.info.clone()
        }
    }
}

/// A unit made from the streams, before the streams are published: its track, its time (a PTS on
/// the 90 kHz clock for a program stream), its duration (90 kHz ticks for video, samples for audio)
/// and its data.
struct Out {
    track: usize,
    pts: i64,
    dur: i64,
    data: Vec<u8>,
}

/// A demuxer over a byte source: an elementary stream directly, or a program stream through its
/// PES packets.
pub struct MpegVideoDemuxer<R> {
    r: R,
    ps: bool,
    tracks: Vec<Track>,
    streams: Vec<StreamInfo>,
    /// Program stream bytes not yet parsed.
    raw: Vec<u8>,
    /// Units made but not yet handed out (until the streams are published, all of them).
    pending: Vec<Out>,
    published: bool,
    /// The time every stream starts from (program streams only).
    base: i64,
    ready: VecDeque<Packet>,
    eof: bool,
}

/// Whether a PES stream id is one this demuxer reads: video, MPEG audio, or private stream 1.
fn wanted(id: u8) -> bool {
    id == 0xBD || (0xC0..=0xEF).contains(&id)
}

/// The bits, rate and channels of a DVD LPCM frame header: emphasis, quantization and frequency
/// byte, and the channel count.
fn lpcm_format(h: &[u8]) -> Option<(u8, u32, usize)> {
    let bits = match h[1] >> 6 & 3 {
        0 => 16,
        1 => 20,
        2 => 24,
        _ => return None,
    };
    let rate = [48000, 96000, 44100, 32000][(h[1] >> 4 & 3) as usize];
    Some((bits, rate, (h[1] & 7) as usize + 1))
}

/// Bytes and frames in one block of DVD LPCM: the smallest unit the samples are packed in. A
/// block holds four samples per channel group (one channel: two pairs, 2 or 4 channels: one group,
/// 8 channels: two groups, others one group per channel), and 16-bit samples are not packed.
fn lpcm_layout(bits: u8, ch: usize) -> (usize, usize) {
    if bits == 16 {
        return (2 * ch, 1);
    }
    let values = match ch {
        1 | 2 | 4 => 4,
        8 => 8,
        n => 4 * n,
    };
    (values * bits as usize / 8, values / ch)
}

/// The samples of one DVD LPCM block, interleaved by channel, as 32-bit values with each sample
/// left-justified (so a 16-bit sample is in the top 16 bits).
fn lpcm_block(b: &[u8], bits: u8, ch: usize, out: &mut Vec<i32>) {
    let be = |i: usize| (u16::from_be_bytes([b[i], b[i + 1]]) as i32) << 16;
    if bits == 16 {
        out.extend((0..ch).map(|k| be(2 * k)));
        return;
    }
    if ch == 1 {
        // two pairs of samples, each pair followed by its low bits
        for g in 0..2 {
            let p = g * if bits == 20 { 5 } else { 6 };
            let (a, c) = (be(p), be(p + 2));
            if bits == 20 {
                let t = b[p + 4] as i32;
                out.extend([a + ((t & 0xF0) << 8), c + ((t & 0x0F) << 12)]);
            } else {
                out.extend([a + ((b[p + 4] as i32) << 8), c + ((b[p + 5] as i32) << 8)]);
            }
        }
        return;
    }
    // groups of four samples, each followed by their low bits
    let values = match ch {
        2 | 4 => 4,
        8 => 8,
        n => 4 * n,
    };
    let low = if bits == 20 { 2 } else { 4 };
    for g in 0..values / 4 {
        let p = g * (8 + low);
        let mut v = [be(p), be(p + 2), be(p + 4), be(p + 6)];
        if bits == 20 {
            let (t0, t1) = (b[p + 8] as i32, b[p + 9] as i32);
            v[0] += (t0 & 0xF0) << 8;
            v[1] += (t0 & 0x0F) << 12;
            v[2] += (t1 & 0xF0) << 8;
            v[3] += (t1 & 0x0F) << 12;
        } else {
            for (k, x) in v.iter_mut().enumerate() {
                *x += (b[p + 8 + k] as i32) << 8;
            }
        }
        out.extend(v);
    }
}

/// Frames of an MPEG audio or AC-3 stream found in `data`, with the PTS of the first one that
/// starts at or after `pts.0` (the offset of the PES's own bytes in `data`). Bytes that do not
/// begin a frame are skipped; a frame cut short is kept for the next PES.
fn audio_frames(t: usize, tr: &mut Track, data: Vec<u8>, pts: Option<(usize, i64)>) -> Vec<Out> {
    let mut out = Vec::new();
    let mut i = 0;
    let mut synced = false;
    while i < data.len() {
        let frame = if tr.kind == Kind::Ac3 {
            match ironav_codec::ac3::frame_info(&data[i..]) {
                Ok(f) if f.offset == 0 && f.sample_rate > 0 => Some((f.frame_size, f.sample_rate, f.channels, 1536usize, CodecId::Ac3)),
                _ => None,
            }
        } else {
            parse_frame(&data[i..]).filter(|h| h.sample_rate > 0).map(|h| (h.frame_len, h.sample_rate, h.channels(), h.samples(), h.codec()))
        };
        let Some((len, rate, channels, samples, codec)) = frame else {
            if data.len() - i < 16 {
                break;
            }
            i += 1;
            continue;
        };
        if i + len > data.len() {
            break;
        }
        if let Some((at, p)) = pts {
            if i >= at && !synced {
                synced = true;
                tr.next = Some(p);
                tr.rem = 0;
            }
        }
        let rate_i = rate as i64;
        if tr.info.is_none() {
            tr.info = Some(StreamInfo {
                index: 0,
                codec,
                time_base: Rational::new(1, rate_i),
                duration: None,
                bits: None,
                extradata: Vec::new(),
                lead_in: 0,
                params: StreamParams::Audio { sample_rate: rate, channels },
            });
        }
        let Some(start) = tr.next else {
            i += len;
            continue;
        };
        let ticks = samples as i64 * CLOCK + tr.rem;
        tr.next = Some(start + ticks / rate_i);
        tr.rem = ticks % rate_i;
        out.push(Out { track: t, pts: start, dur: samples as i64, data: data[i..i + len].to_vec() });
        i += len;
    }
    if i < data.len() {
        tr.carry = data[i..].to_vec();
    }
    out
}

fn unwrap_pts(t: &mut Track, raw: i64) -> i64 {
    let v = match t.last {
        None => raw,
        Some(last) => {
            let d = (raw - last).rem_euclid(WRAP);
            last + if d >= WRAP / 2 { d - WRAP } else { d }
        }
    };
    t.last = Some(v);
    v
}

impl<R: Read + Send> MpegVideoDemuxer<R> {
    /// Opens an elementary stream (`ps` false) or a program stream, reading until its streams have
    /// shown their parameters (and at least one video stream its first sequence header).
    pub fn open(r: R, ps: bool) -> Result<Self> {
        let mut d = MpegVideoDemuxer {
            r,
            ps,
            tracks: Vec::new(),
            streams: Vec::new(),
            raw: Vec::new(),
            pending: Vec::new(),
            published: false,
            base: 0,
            ready: VecDeque::new(),
            eof: false,
        };
        if !ps {
            d.track(0, Kind::Video);
        }
        let mut read = 0usize;
        while !d.is_ready() && !d.eof && read <= MAX_HEAD {
            read += d.pump()?;
        }
        d.publish();
        if !d.tracks.iter().any(|t| t.kind == Kind::Video && t.framer.info.is_some()) {
            return Err(if read > MAX_HEAD {
                Error::invalid_input(format!("no sequence header in the first {} MiB", MAX_HEAD >> 20))
            } else {
                Error::new(ErrorCode::InvalidInput, "the stream holds no MPEG video sequence header")
            });
        }
        Ok(d)
    }

    /// Whether a video stream and every stream found so far have shown their parameters.
    fn is_ready(&self) -> bool {
        self.tracks.iter().any(|t| t.kind == Kind::Video && t.framer.info.is_some()) && self.tracks.iter().all(Track::has_info)
    }

    fn track(&mut self, key: u16, kind: Kind) -> usize {
        if let Some(i) = self.tracks.iter().position(|t| t.key == key) {
            return i;
        }
        self.tracks.push(Track::new(key, kind));
        self.tracks.len() - 1
    }

    /// Reads one chunk and parses what is whole. Returns the bytes read.
    fn pump(&mut self) -> Result<usize> {
        let mut buf = vec![0u8; CHUNK];
        let n = self.r.read(&mut buf)?;
        if n == 0 {
            self.eof = true;
            if self.ps {
                self.parse_ps(true);
            }
            for t in self.tracks.iter_mut().filter(|t| t.kind == Kind::Video) {
                t.framer.end();
            }
            self.collect();
            self.settle();
            return Ok(0);
        }
        if self.ps {
            self.raw.extend_from_slice(&buf[..n]);
            self.parse_ps(false);
        } else {
            self.tracks[0].framer.push(&buf[..n]);
        }
        self.collect();
        self.settle();
        Ok(n)
    }

    /// Parses the program stream packets in `raw`, passing the PES packets it reads on.
    fn parse_ps(&mut self, last: bool) {
        let mut i = 0;
        let d = std::mem::take(&mut self.raw);
        while let Some(s) = find_start(&d, i) {
            if s + 4 > d.len() {
                i = s;
                break;
            }
            let id = d[s + 3];
            let rest = &d[s..];
            let take = match id {
                0xBA => {
                    // MPEG-2 packs are 14 bytes and a stuffing count, MPEG-1 packs 12
                    if rest.len() < 14 {
                        None
                    } else if rest[4] >> 6 == 1 {
                        Some(14 + (rest[13] & 7) as usize)
                    } else {
                        Some(12)
                    }
                }
                0xB9 => Some(4),
                0xBB..=0xFF => {
                    if rest.len() < 6 {
                        None
                    } else {
                        Some(6 + u16::from_be_bytes([rest[4], rest[5]]) as usize)
                    }
                }
                _ => Some(4),
            };
            let Some(n) = take else {
                i = s;
                break;
            };
            if s + n > d.len() {
                if last {
                    // a cut-off last packet: use what there is
                    if wanted(id) {
                        self.pes(&d[s..]);
                    }
                    i = d.len();
                } else {
                    i = s;
                }
                break;
            }
            if wanted(id) {
                self.pes(&d[s..s + n]);
            }
            i = s + n;
        }
        if i < d.len() && !last {
            self.raw = d[i..].to_vec();
        }
    }

    /// One PES packet (`p`, its start code included) of a stream that is read.
    fn pes(&mut self, p: &[u8]) {
        let id = p[3];
        let Some((raw, start)) = pes_header(p) else { return };
        let Some(payload) = p.get(start..) else { return };
        match id {
            0xE0..=0xEF => {
                let t = self.track(id as u16, Kind::Video);
                let pts = self.stamp(t, raw);
                let tr = &mut self.tracks[t];
                if tr.start.is_none() {
                    tr.start = pts;
                }
                tr.framer.push(payload);
            }
            0xC0..=0xDF => {
                let t = self.track(id as u16, Kind::Mpeg);
                let pts = self.stamp(t, raw);
                self.audio(t, payload, pts);
            }
            0xBD => {
                // private stream 1: the substream id, then the substream's own header
                let Some((&sub, body)) = payload.split_first() else { return };
                match sub {
                    0x80..=0x87 if body.len() >= 3 => {
                        let t = self.track(0x100 | sub as u16, Kind::Ac3);
                        let pts = self.stamp(t, raw);
                        self.audio(t, &body[3..], pts);
                    }
                    // DVD-Video LPCM has a dynamic range byte of 0x80 (FFmpeg's test too)
                    0xA0..=0xA7 if body.len() >= 6 && body[5] == 0x80 => {
                        let t = self.track(0x100 | sub as u16, Kind::Lpcm);
                        let pts = self.stamp(t, raw);
                        self.lpcm(t, &body[3..6], &body[6..], pts);
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn stamp(&mut self, t: usize, raw: Option<i64>) -> Option<i64> {
        raw.map(|r| unwrap_pts(&mut self.tracks[t], r))
    }

    /// The payload of an MPEG audio or AC-3 PES, with the PTS it carries.
    fn audio(&mut self, t: usize, body: &[u8], pts: Option<i64>) {
        let tr = &mut self.tracks[t];
        let at = tr.carry.len();
        let mut data = std::mem::take(&mut tr.carry);
        data.extend_from_slice(body);
        let outs = audio_frames(t, tr, data, pts.map(|p| (at, p)));
        self.pending.extend(outs);
    }

    /// The payload of a DVD LPCM PES: its frame header, then whole blocks of samples. A block cut
    /// by the end of the PES is kept for the next one.
    fn lpcm(&mut self, t: usize, hdr: &[u8], body: &[u8], pts: Option<i64>) {
        let Some(cfg @ (bits, rate, ch)) = lpcm_format(hdr) else { return };
        let tr = &mut self.tracks[t];
        if tr.lpcm != Some(cfg) {
            tr.carry.clear();
            tr.lpcm = Some(cfg);
        }
        if tr.carry.is_empty() {
            if let Some(p) = pts {
                tr.next = Some(p);
                tr.rem = 0;
            }
        }
        let mut data = std::mem::take(&mut tr.carry);
        data.extend_from_slice(body);
        let (block, frames) = lpcm_layout(bits, ch);
        let whole = data.len() / block * block;
        tr.carry = data[whole..].to_vec();
        if whole == 0 {
            return;
        }
        let codec = match bits {
            16 => CodecId::PcmS16Le,
            20 => CodecId::PcmS32Le,
            _ => CodecId::PcmS24Le,
        };
        if tr.info.is_none() {
            tr.info = Some(StreamInfo {
                index: 0,
                codec,
                time_base: Rational::new(1, rate as i64),
                duration: None,
                bits: Some(bits),
                extradata: Vec::new(),
                lead_in: 0,
                params: StreamParams::Audio { sample_rate: rate, channels: ch as u16 },
            });
        }
        let Some(start) = tr.next else { return };
        let mut vals = Vec::new();
        let mut bytes = Vec::with_capacity(whole / block * frames * ch * 4);
        for b in data[..whole].chunks_exact(block) {
            vals.clear();
            lpcm_block(b, bits, ch, &mut vals);
            for &v in &vals {
                match bits {
                    16 => bytes.extend_from_slice(&((v >> 16) as i16).to_le_bytes()),
                    20 => bytes.extend_from_slice(&v.to_le_bytes()),
                    _ => bytes.extend_from_slice(&(v >> 8).to_le_bytes()[..3]),
                }
            }
        }
        let n = (whole / block * frames) as i64;
        let ticks = n * CLOCK + tr.rem;
        tr.next = Some(start + ticks / rate as i64);
        tr.rem = ticks % rate as i64;
        self.pending.push(Out { track: t, pts: start, dur: n, data: bytes });
    }

    /// Moves the frames the video streams have finished into the pending units.
    fn collect(&mut self) {
        let (ps, tracks, pending) = (self.ps, &mut self.tracks, &mut self.pending);
        for (i, t) in tracks.iter_mut().enumerate() {
            if t.kind != Kind::Video {
                continue;
            }
            // a frame waits until the sequence header that gives its rate has come
            let Some(rate) = t.framer.info.as_ref().map(|s| s.frame_rate) else { continue };
            while let Some(u) = t.framer.out.pop_front() {
                let (pts, dur) = if ps {
                    let origin = match t.origin {
                        Some(o) => o,
                        None => {
                            let o = t.start.unwrap_or(0) - ticks(u.index, rate);
                            t.origin = Some(o);
                            o
                        }
                    };
                    (origin + ticks(u.index, rate), ticks(1, rate))
                } else {
                    (u.index, 1)
                };
                pending.push(Out { track: i, pts, dur, data: u.data });
            }
        }
    }

    /// Fixes the streams: each track with its parameters, in the order the tracks were found. The
    /// earliest time of the units made so far is the start of every stream.
    fn publish(&mut self) {
        if self.published {
            return;
        }
        self.published = true;
        for t in self.tracks.iter_mut() {
            let Some(mut s) = t.stream_info(self.ps) else { continue };
            s.index = self.streams.len();
            t.out = Some(s.index);
            self.streams.push(s);
        }
        self.base = if self.ps { self.pending.iter().filter(|o| self.tracks[o.track].out.is_some()).map(|o| o.pts).min().unwrap_or(0) } else { 0 };
        self.settle();
    }

    /// Hands out the pending units once the streams are published. Units of a track that was not
    /// published are dropped.
    fn settle(&mut self) {
        if !self.published {
            return;
        }
        for o in std::mem::take(&mut self.pending) {
            let Some(stream) = self.tracks[o.track].out else { continue };
            let pkt = if self.ps {
                let t = (o.pts - self.base).max(0);
                match self.streams[stream].params {
                    StreamParams::Audio { sample_rate, .. } => Packet { stream, pts: (t * sample_rate as i64 + CLOCK / 2) / CLOCK, duration: o.dur, data: o.data },
                    _ => Packet { stream, pts: t, duration: o.dur, data: o.data },
                }
            } else {
                Packet { stream, pts: o.pts, duration: 1, data: o.data }
            };
            self.ready.push_back(pkt);
        }
    }
}

impl<R: Read + Send> Demuxer for MpegVideoDemuxer<R> {
    fn format_name(&self) -> &'static str {
        if self.ps {
            "mpegps"
        } else {
            "m2v"
        }
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn read_packet(&mut self) -> Result<Option<Packet>> {
        loop {
            if let Some(p) = self.ready.pop_front() {
                return Ok(Some(p));
            }
            if self.eof {
                return Ok(None);
            }
            self.pump()?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn picture(tr: u16, ty: u8, field: Option<u8>) -> Vec<u8> {
        let mut v = vec![0, 0, 1, 0, (tr >> 2) as u8, ((tr & 3) << 6) as u8 | ty << 3, 0xFF, 0xF8];
        if let Some(s) = field {
            v.extend_from_slice(&[0, 0, 1, 0xB5, 0x8F, 0xFF, 0xF0 | s, 0x00]);
        }
        v.extend_from_slice(&[0, 0, 1, 1, 0xAA, 0xBB]);
        v
    }

    #[test]
    fn frames_are_cut_at_pictures_and_fields_stay_together() {
        let mut f = Framer::default();
        let mut s = vec![0, 0, 1, 0xB3, 0, 0, 0];
        s.extend_from_slice(&[0, 0, 1, 0xB8, 0, 0, 0, 0]);
        s.extend(picture(2, 1, Some(1)));
        s.extend(picture(2, 2, Some(2)));
        s.extend(picture(0, 3, None));
        s.extend(picture(1, 3, None));
        s.extend_from_slice(&[0, 0, 1, 0xB8, 0, 0, 0, 0]);
        s.extend(picture(1, 1, None));
        // in pieces, to meet a start code split across two
        for c in s.chunks(5) {
            f.push(c);
        }
        f.end();
        let idx: Vec<i64> = f.out.iter().map(|u| u.index).collect();
        assert_eq!(idx, vec![2, 0, 1, 4]);
    }

    #[test]
    fn dvd_lpcm_blocks_decode_to_left_justified_samples() {
        // a sample is its big-endian high bits, shifted up, plus its low bits
        let s = |hi: u16, low: i32| ((hi as i32) << 16) + low;
        // 16 bits, stereo: one frame of two big-endian samples
        let mut v = Vec::new();
        lpcm_block(&[0x12, 0x34, 0x80, 0x00], 16, 2, &mut v);
        assert_eq!(v, vec![s(0x1234, 0), s(0x8000, 0)]);
        // 20 bits, mono: pairs of samples, each pair followed by a byte of low nibbles
        let mut v = Vec::new();
        lpcm_block(&[0x12, 0x34, 0x56, 0x78, 0xAB, 0x9C, 0x00, 0x00, 0xF0, 0x12], 20, 1, &mut v);
        assert_eq!(v, vec![s(0x1234, 0xA << 12), s(0x5678, 0xB << 12), s(0x9C00, 0x1 << 12), s(0x00F0, 0x2 << 12)]);
        // 24 bits, stereo: four samples, then their low bytes
        let mut v = Vec::new();
        lpcm_block(&[0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC, 0xDE, 0xF0, 0x01, 0x02, 0x03, 0x04], 24, 2, &mut v);
        assert_eq!(v, vec![s(0x1234, 0x01 << 8), s(0x5678, 0x02 << 8), s(0x9ABC, 0x03 << 8), s(0xDEF0, 0x04 << 8)]);
    }

    #[test]
    fn layouts_match_the_block_sizes_ffmpeg_reads() {
        assert_eq!(lpcm_layout(16, 2), (4, 1));
        assert_eq!(lpcm_layout(20, 1), (10, 4));
        assert_eq!(lpcm_layout(24, 2), (12, 2));
        assert_eq!(lpcm_layout(20, 8), (20, 1));
        assert_eq!(lpcm_layout(24, 6), (72, 4));
    }
}
