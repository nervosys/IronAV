//! Matroska and WebM reader.
//!
//! Only the structure is read here: the elements are walked once, seeking over every frame's data,
//! to find where each frame lies and when it plays. The list of samples that results goes to the
//! MP4 reader's machinery (`Mp4Demuxer::from_parts`), which reads the packets, interleaves the
//! tracks, seeks, and rewrites H.264 and HEVC from length-prefixed NAL units to Annex B.
//!
//! Matroska stores blocks in decode order with a presentation time and no decode time. Where the
//! video is reordered, decode times are taken to be the sorted presentation times, which is what
//! a player's dts/pts model needs: the frame decoded `i`-th is the `i`-th to be shown, give or
//! take the reorder delay. Audio is given a sample-rate time base when the frame length is known
//! (so its times are exact despite the file's millisecond ticks).
//!
//! Content encodings: header stripping and zlib are undone frame by frame (the live reader refuses encoded
//! tracks); encryption, bzip2, lzo and encodings of the CodecPrivate alone are refused.
//! Not handled:
//! tags, which are skipped; and a track's start offset, since a copy re-bases each track to its own
//! first timestamp. The Cues are not used by [`open`], which reads every frame's place and so builds
//! an exact index; [`lazy`] uses them to open a long file without walking it.

pub(crate) mod lazy;
pub(crate) mod live;

use crate::mp4demux::{av1c_config, gcd, parse_avcc, parse_hvcc, unwrap_frame, Config, Mp4Demuxer, Sample, TrackInfo, Unwrap};
use ironav_core::{Attachment, Chapter, CodecId, Error, ErrorCode, Rational, Result, StreamInfo, StreamParams};
use std::io::{BufReader, Read, Seek, SeekFrom};

const EBML: u32 = 0x1A45_DFA3;
const DOC_TYPE: u32 = 0x4282;
const SEGMENT: u32 = 0x1853_8067;
const SEEK_HEAD: u32 = 0x114D_9B74;
const INFO: u32 = 0x1549_A966;
const TIMECODE_SCALE: u32 = 0x2A_D7B1;
const TRACKS: u32 = 0x1654_AE6B;
const CLUSTER: u32 = 0x1F43_B675;
const CUES: u32 = 0x1C53_BB6B;
const TAGS: u32 = 0x1254_C367;
const SEEK: u32 = 0x4DBB;
const SEEK_ID: u32 = 0x53AB;
const SEEK_POSITION: u32 = 0x53AC;
const DURATION: u32 = 0x4489;
const CHAPTERS: u32 = 0x1043_A770;
const ATTACHMENTS: u32 = 0x1941_A469;

const TRACK_ENTRY: u32 = 0xAE;
const TRACK_NUMBER: u32 = 0xD7;
const TRACK_TYPE: u32 = 0x83;
const CODEC_ID: u32 = 0x86;
const CODEC_PRIVATE: u32 = 0x63A2;
const DEFAULT_DURATION: u32 = 0x23_E383;
const VIDEO: u32 = 0xE0;
const PIXEL_WIDTH: u32 = 0xB0;
const PIXEL_HEIGHT: u32 = 0xBA;
const AUDIO: u32 = 0xE1;
const SAMPLING_FREQUENCY: u32 = 0xB5;
const CHANNELS: u32 = 0x9F;
const BIT_DEPTH: u32 = 0x6264;
const CONTENT_ENCODINGS: u32 = 0x6D80;
const CONTENT_ENCODING: u32 = 0x6240;
const CONTENT_ENCODING_ORDER: u32 = 0x5031;
const CONTENT_ENCODING_SCOPE: u32 = 0x5032;
const CONTENT_ENCODING_TYPE: u32 = 0x5033;
const CONTENT_COMPRESSION: u32 = 0x5034;
const CONTENT_COMP_ALGO: u32 = 0x4254;
const CONTENT_COMP_SETTINGS: u32 = 0x4255;

const CLUSTER_TIMECODE: u32 = 0xE7;
const SIMPLE_BLOCK: u32 = 0xA3;
const BLOCK_GROUP: u32 = 0xA0;
const BLOCK: u32 = 0xA1;
const BLOCK_DURATION: u32 = 0x9B;
const DISCARD_PADDING: u32 = 0x75A2;
const CODEC_DELAY: u32 = 0x56AA;
const REFERENCE_BLOCK: u32 = 0xFB;

const TRACK_SUBTITLE: u64 = 17;

const EDITION_ENTRY: u32 = 0x45B9;
const CHAPTER_ATOM: u32 = 0xB6;
const CHAPTER_TIME_START: u32 = 0x91;
const CHAPTER_TIME_END: u32 = 0x92;
const CHAPTER_FLAG_HIDDEN: u32 = 0x98;
const CHAPTER_FLAG_ENABLED: u32 = 0x4598;
const CHAPTER_DISPLAY: u32 = 0x80;
const CHAP_STRING: u32 = 0x85;
const CHAP_LANGUAGE: u32 = 0x437C;
const CHAP_LANGUAGE_IETF: u32 = 0x437D;
const ATTACHED_FILE: u32 = 0x61A7;
const FILE_NAME: u32 = 0x466E;
const FILE_MEDIA_TYPE: u32 = 0x4660;
const FILE_DATA: u32 = 0x465C;

/// The Chapters element is read whole up to this size.
const MAX_CHAPTERS: u64 = 16 << 20;
/// The most chapters and attachments listed, the deepest chapter nesting followed, and the longest name or title kept.
const MAX_LISTED: usize = 10_000;
const MAX_CHAPTER_DEPTH: u32 = 8;
const MAX_NAME: usize = 512;

/// Sanity cap on the Tracks element we are willing to hold in memory.
const MAX_TRACKS: u64 = 64 << 20;
/// The SeekHead is read whole up to this size.
const MAX_SEEK_HEAD: u64 = 1 << 20;

fn bad(msg: impl std::fmt::Display) -> Error {
    Error::invalid_input(format!("Matroska: {msg}"))
}

/// Whether `magic` starts with an EBML header.
pub fn looks_like_mkv(magic: &[u8]) -> bool {
    magic.starts_with(&EBML.to_be_bytes())
}

/// Top-level elements; an unknown-size cluster ends where one of these begins.
fn is_level1(id: u32) -> bool {
    matches!(id, CLUSTER | CUES | TAGS | SEEK_HEAD | INFO | TRACKS | CHAPTERS | ATTACHMENTS)
}

/// A frame found in a block: where it is and when it plays, in ticks of the file's time scale.
struct Frame {
    track: u64,
    tc: i64,
    key: bool,
    offset: u64,
    size: u32,
    /// Index within a laced block.
    lace: u32,
    duration: Option<i64>,
    /// Nanoseconds at the end of the frame that the decoder's output should not include.
    discard: i64,
    /// The first bytes of the frame (Opus gives its length in the first one).
    head: [u8; 2],
}

struct Scan<R: Read + Seek> {
    r: BufReader<R>,
    pos: u64,
    len: u64,
    /// An element id read ahead (the one that ended an unknown-size cluster).
    pending: Option<u32>,
    truncated: bool,
    frames: Vec<Frame>,
    /// Nanoseconds per tick.
    scale: u64,
    tracks: Option<Vec<u8>>,
    chapters: Option<Vec<u8>>,
    attachments: Vec<Attachment>,
    doc_type: String,
    /// Where the Segment's data starts: SeekHead and Cues positions are counted from here.
    segment_data: u64,
    /// The SeekHead's entries: an element id and its position from `segment_data`.
    seek_entries: Vec<(u32, u64)>,
    /// Stop at the first Cluster instead of walking the file (and note where it is).
    stop_at_cluster: bool,
    first_cluster: Option<u64>,
    /// The Info element's Duration, in ticks.
    duration: Option<f64>,
    /// The first and last block times seen in `frames`, in ticks.
    min_tc: i64,
    max_tc: i64,
}

impl<R: Read + Seek> Scan<R> {
    fn byte(&mut self) -> Result<Option<u8>> {
        let mut b = [0u8; 1];
        if self.r.read(&mut b)? == 0 {
            return Ok(None);
        }
        self.pos += 1;
        Ok(Some(b[0]))
    }

    fn need(&mut self) -> Result<u8> {
        self.byte()?.ok_or_else(|| bad("the file ends inside an element header"))
    }

    /// An element id, with its length marker kept (as the ids are written in the specification).
    fn id(&mut self) -> Result<Option<u32>> {
        if let Some(id) = self.pending.take() {
            return Ok(Some(id));
        }
        let Some(first) = self.byte()? else { return Ok(None) };
        let n = first.leading_zeros() + 1;
        if n > 4 {
            return Err(bad("invalid element id"));
        }
        let mut id = first as u32;
        for _ in 1..n {
            id = id << 8 | self.need()? as u32;
        }
        Ok(Some(id))
    }

    /// A variable-length integer without its length marker, and its length in bytes.
    fn vint(&mut self) -> Result<(u64, u32)> {
        let first = self.need()?;
        let n = first.leading_zeros() + 1;
        if n > 8 {
            return Err(bad("invalid variable-length integer"));
        }
        let mut v = first as u64 & ((1u64 << (8 - n)) - 1);
        for _ in 1..n {
            v = v << 8 | self.need()? as u64;
        }
        Ok((v, n))
    }

    /// An element's data size; `None` when it is the reserved "unknown" value.
    fn size(&mut self) -> Result<Option<u64>> {
        let (v, n) = self.vint()?;
        Ok(if v == (1u64 << (7 * n)) - 1 { None } else { Some(v) })
    }

    fn uint(&mut self, n: u64) -> Result<u64> {
        if n > 8 {
            return Err(bad("an integer is wider than 8 bytes"));
        }
        let mut v = 0u64;
        for _ in 0..n {
            v = v << 8 | self.need()? as u64;
        }
        Ok(v)
    }

    fn skip_to(&mut self, end: u64) -> Result<()> {
        if self.pos > end {
            return Err(bad("an element is larger than its parent"));
        }
        self.r.seek_relative((end - self.pos) as i64)?;
        self.pos = end;
        Ok(())
    }

    fn body(&mut self, size: u64) -> Result<Vec<u8>> {
        let mut v = vec![0u8; size as usize];
        self.r.read_exact(&mut v)?;
        self.pos += size;
        Ok(v)
    }

    /// The end of an element whose body starts here, or `None` (and the scan is marked truncated)
    /// when the file ends before it does.
    fn end_of(&mut self, size: u64) -> Option<u64> {
        let end = self.pos.saturating_add(size);
        if end > self.len {
            self.truncated = true;
            return None;
        }
        Some(end)
    }

    fn file(&mut self) -> Result<()> {
        if self.id()? != Some(EBML) {
            return Err(bad("the file does not start with an EBML header"));
        }
        let size = self.size()?.ok_or_else(|| bad("the EBML header has no size"))?;
        let end = self.end_of(size).ok_or_else(|| bad("the EBML header is cut short"))?;
        if size > 4096 {
            return Err(bad("the EBML header is implausibly large"));
        }
        let header = self.body(size)?;
        for e in Elems(&header) {
            let (id, v) = e?;
            if id == DOC_TYPE {
                self.doc_type = String::from_utf8_lossy(v).trim_end_matches('\0').to_string();
            }
        }
        debug_assert_eq!(self.pos, end);
        if !matches!(self.doc_type.as_str(), "matroska" | "webm") {
            return Err(Error::new(ErrorCode::UnsupportedFormat, format!("EBML document type '{}' is not Matroska or WebM", self.doc_type)));
        }
        loop {
            let Some(id) = self.id()? else { return Err(bad("the file has no segment")) };
            let size = self.size()?;
            if id == SEGMENT {
                let end = size.map_or(self.len, |s| self.pos.saturating_add(s).min(self.len));
                self.segment_data = self.pos;
                return self.segment(end);
            }
            let size = size.ok_or_else(|| bad("an element of unknown size before the segment"))?;
            let Some(end) = self.end_of(size) else { return Err(bad("the file has no segment")) };
            self.skip_to(end)?;
        }
    }

    fn segment(&mut self, end: u64) -> Result<()> {
        while self.pos < end && !self.truncated {
            let at = self.pos;
            let Some(id) = self.id()? else { break };
            let size = self.size()?;
            if id == CLUSTER {
                if self.stop_at_cluster {
                    self.first_cluster = Some(at);
                    return Ok(());
                }
                let cluster_end = size.map(|s| self.pos.saturating_add(s).min(self.len));
                if size.is_some_and(|s| self.pos.saturating_add(s) > self.len) {
                    self.truncated = true;
                }
                self.cluster(cluster_end)?;
                if let Some(e) = cluster_end {
                    if !self.truncated {
                        self.skip_to(e)?;
                    }
                }
                continue;
            }
            let size = size.ok_or_else(|| bad("an element of unknown size that is not a cluster"))?;
            let Some(body_end) = self.end_of(size) else { break };
            self.level1(id, size, body_end)?;
        }
        Ok(())
    }

    /// A top-level element other than a cluster, whose header has been read: take what it holds, and
    /// leave the position at its end.
    fn level1(&mut self, id: u32, size: u64, body_end: u64) -> Result<()> {
        match id {
            INFO => {
                let info = self.body(size)?;
                for e in Elems(&info) {
                    let (id, v) = e?;
                    match id {
                        TIMECODE_SCALE => self.scale = be_uint(v)?.max(1),
                        DURATION => self.duration = be_float(v).ok().filter(|d| d.is_finite() && *d > 0.0),
                        _ => {}
                    }
                }
            }
            SEEK_HEAD if size <= MAX_SEEK_HEAD => {
                let head = self.body(size)?;
                for e in Elems(&head) {
                    let Ok((SEEK, entry)) = e else { continue };
                    let (mut what, mut at) = (None, None);
                    for e in Elems(entry) {
                        let Ok((id, v)) = e else { break };
                        match id {
                            SEEK_ID if v.len() <= 4 => what = be_uint(v).ok().map(|n| n as u32),
                            SEEK_POSITION => at = be_uint(v).ok(),
                            _ => {}
                        }
                    }
                    if let (Some(what), Some(at)) = (what, at) {
                        if self.seek_entries.len() < 64 {
                            self.seek_entries.push((what, at));
                        }
                    }
                }
            }
            CHAPTERS if size <= MAX_CHAPTERS => self.chapters = Some(self.body(size)?),
            ATTACHMENTS => {
                // a damaged list of attachments is left out; it is not a reason to refuse the file
                if self.read_attachments(body_end).is_err() {
                    self.attachments.clear();
                    self.r.seek(SeekFrom::Start(body_end))?;
                    self.pos = body_end;
                }
            }
            TRACKS => {
                if size > MAX_TRACKS {
                    return Err(bad("the track list is implausibly large"));
                }
                let t = self.body(size)?;
                self.tracks = Some(t);
            }
            _ => self.skip_to(body_end)?,
        }
        Ok(())
    }

    /// Read the frames of the clusters from `from` on, until they span at least `span` ticks and, when
    /// `until` is given, reach that time, or until `end`. Leaves `pos` where the next window starts.
    /// Clusters of unknown size are refused: their end is where the next element begins, which
    /// a window cannot restart from.
    fn window(&mut self, from: u64, end: u64, span: i64, until: Option<i64>) -> Result<()> {
        self.frames.clear();
        (self.min_tc, self.max_tc) = (i64::MAX, i64::MIN);
        self.pending = None;
        self.truncated = false;
        self.r.seek(SeekFrom::Start(from))?;
        self.pos = from;
        while self.pos < end && !self.truncated {
            let Some(id) = self.id()? else { break };
            let size = self.size()?;
            if id == CLUSTER {
                let size = size.ok_or_else(|| bad("a cluster of unknown size cannot be indexed a window at a time"))?;
                let cluster_end = self.pos.saturating_add(size).min(self.len);
                if self.pos.saturating_add(size) > self.len {
                    self.truncated = true;
                }
                self.cluster(Some(cluster_end))?;
                if !self.truncated {
                    self.skip_to(cluster_end)?;
                }
                if self.max_tc != i64::MIN && self.max_tc - self.min_tc >= span && until.is_none_or(|u| self.max_tc >= u) {
                    break;
                }
                continue;
            }
            let size = size.ok_or_else(|| bad("an element of unknown size that is not a cluster"))?;
            let Some(body_end) = self.end_of(size) else { break };
            self.skip_to(body_end)?;
        }
        Ok(())
    }

    /// The Attachments element: each file's name and type and the size of its data, which is not read.
    fn read_attachments(&mut self, end: u64) -> Result<()> {
        while self.pos < end && self.attachments.len() < MAX_LISTED {
            let Some(id) = self.id()? else { break };
            let size = self.size()?.ok_or_else(|| bad("an attachment element has unknown size"))?;
            let file_end = self.pos.saturating_add(size);
            if file_end > end {
                return Err(bad("an attachment is larger than the attachments element"));
            }
            if id != ATTACHED_FILE {
                self.skip_to(file_end)?;
                continue;
            }
            let mut a = Attachment { name: String::new(), mime_type: String::new(), size: 0 };
            while self.pos < file_end {
                let Some(id) = self.id()? else { break };
                let size = self.size()?.ok_or_else(|| bad("an attachment field has unknown size"))?;
                let field_end = self.pos.saturating_add(size);
                if field_end > file_end {
                    return Err(bad("an attachment field is larger than its attachment"));
                }
                match id {
                    FILE_NAME | FILE_MEDIA_TYPE if size <= MAX_NAME as u64 * 4 => {
                        let v = self.body(size)?;
                        let s = String::from_utf8_lossy(&v).trim_end_matches('\0').chars().take(MAX_NAME).collect::<String>();
                        if id == FILE_NAME { a.name = s } else { a.mime_type = s }
                    }
                    FILE_DATA => {
                        a.size = size;
                        self.skip_to(field_end)?;
                    }
                    _ => self.skip_to(field_end)?,
                }
            }
            self.skip_to(file_end)?;
            self.attachments.push(a);
        }
        self.skip_to(end)
    }

    fn cluster(&mut self, end: Option<u64>) -> Result<()> {
        let mut tc = 0i64;
        loop {
            if end.is_some_and(|e| self.pos >= e) || self.truncated {
                return Ok(());
            }
            let Some(id) = self.id()? else { return Ok(()) };
            if end.is_none() && is_level1(id) {
                self.pending = Some(id);
                return Ok(());
            }
            let size = self.size()?.ok_or_else(|| bad("an element inside a cluster has unknown size"))?;
            let Some(body_end) = self.end_of(size) else { return Ok(()) };
            match id {
                CLUSTER_TIMECODE => tc = self.uint(size)? as i64,
                SIMPLE_BLOCK => {
                    self.block(size, true, tc)?;
                }
                BLOCK_GROUP => {
                    let mut range = 0..0;
                    let mut duration = None;
                    let mut discard = 0i64;
                    let mut referenced = false;
                    while self.pos < body_end {
                        let Some(gid) = self.id()? else { break };
                        let gsize = self.size()?.ok_or_else(|| bad("an element inside a block group has unknown size"))?;
                        let gend = self.pos.saturating_add(gsize);
                        if gend > body_end {
                            return Err(bad("an element is larger than its block group"));
                        }
                        match gid {
                            BLOCK => range = self.block(gsize, false, tc)?,
                            BLOCK_DURATION => duration = Some(self.uint(gsize)? as i64),
                            DISCARD_PADDING => {
                                let v = self.uint(gsize)?;
                                // signed; a negative value trims the start, which the pre-skip covers
                                if gsize > 0 && gsize <= 8 && v < 1u64 << (8 * gsize - 1) {
                                    discard = v as i64;
                                }
                            }
                            REFERENCE_BLOCK => {
                                referenced = true;
                                self.skip_to(gend)?;
                            }
                            _ => self.skip_to(gend)?,
                        }
                    }
                    let single = range.len() == 1;
                    for f in &mut self.frames[range] {
                        f.key = !referenced;
                        if single {
                            f.duration = duration;
                            f.discard = discard;
                        }
                    }
                }
                _ => {}
            }
            self.skip_to(body_end)?;
        }
    }

    /// Read a block's header (and lace sizes), add its frames, and leave the position at its end.
    /// Returns the range of `frames` that were added.
    fn block(&mut self, size: u64, simple: bool, cluster_tc: i64) -> Result<std::ops::Range<usize>> {
        let end = self.pos + size;
        let (track, _) = self.vint()?;
        let rel = i16::from_be_bytes([self.need()?, self.need()?]) as i64;
        let flags = self.need()?;
        let key = simple && flags & 0x80 != 0;
        let first = self.frames.len();
        let tc = cluster_tc + rel;
        let mut sizes: Vec<u64> = Vec::new();
        let lacing = (flags >> 1) & 3;
        if lacing != 0 {
            let n = self.need()? as usize + 1;
            match lacing {
                1 => {
                    for _ in 1..n {
                        let mut s = 0u64;
                        loop {
                            let b = self.need()?;
                            s += b as u64;
                            if b != 255 {
                                break;
                            }
                        }
                        sizes.push(s);
                    }
                }
                3 if n > 1 => {
                    let (mut prev, _) = self.vint()?;
                    sizes.push(prev);
                    for _ in 2..n {
                        let (v, len) = self.vint()?;
                        let bias = (1i64 << (7 * len - 1)) - 1;
                        let next = prev as i64 + v as i64 - bias;
                        if next < 0 {
                            return Err(bad("a lace size is negative"));
                        }
                        prev = next as u64;
                        sizes.push(prev);
                    }
                }
                _ => {}
            }
            let data = end.checked_sub(self.pos).ok_or_else(|| bad("a block header runs past its block"))?;
            if lacing == 2 {
                if data % n as u64 != 0 {
                    return Err(bad("a fixed-size lace does not divide its block"));
                }
                sizes = vec![data / n as u64; n];
            } else {
                let used: u64 = sizes.iter().sum();
                sizes.push(data.checked_sub(used).ok_or_else(|| bad("lace sizes add up to more than the block"))?);
            }
        } else {
            sizes.push(end.checked_sub(self.pos).ok_or_else(|| bad("a block header runs past its block"))?);
        }
        let mut offset = self.pos;
        for (lace, s) in sizes.into_iter().enumerate() {
            let size = u32::try_from(s).map_err(|_| bad("a frame is larger than 4 GiB"))?;
            let mut head = [0u8; 2];
            if size > 0 {
                self.skip_to(offset)?;
                for h in head.iter_mut().take(size.min(2) as usize) {
                    *h = self.need()?;
                }
            }
            self.min_tc = self.min_tc.min(tc);
            self.max_tc = self.max_tc.max(tc);
            self.frames.push(Frame { track, tc, key, offset, size, lace: lace as u32, duration: None, discard: 0, head });
            offset += s;
        }
        self.skip_to(end)?;
        Ok(first..self.frames.len())
    }
}

fn be_uint(v: &[u8]) -> Result<u64> {
    if v.len() > 8 {
        return Err(bad("an integer is wider than 8 bytes"));
    }
    Ok(v.iter().fold(0u64, |a, &b| a << 8 | b as u64))
}

fn be_float(v: &[u8]) -> Result<f64> {
    match v.len() {
        0 => Ok(0.0),
        4 => Ok(f32::from_be_bytes(v.try_into().unwrap()) as f64),
        8 => Ok(f64::from_be_bytes(v.try_into().unwrap())),
        _ => Err(bad("a float is neither 4 nor 8 bytes")),
    }
}

/// The elements of an in-memory master element.
struct Elems<'a>(&'a [u8]);

impl<'a> Iterator for Elems<'a> {
    type Item = Result<(u32, &'a [u8])>;

    fn next(&mut self) -> Option<Self::Item> {
        let d = self.0;
        if d.is_empty() {
            return None;
        }
        let parse = || -> Result<(u32, &'a [u8], usize)> {
            let n = d[0].leading_zeros() as usize + 1;
            if n > 4 || d.len() < n + 1 {
                return Err(bad("an element id is malformed"));
            }
            let id = d[..n].iter().fold(0u32, |a, &b| a << 8 | b as u32);
            let m = d[n].leading_zeros() as usize + 1;
            if m > 8 || d.len() < n + m {
                return Err(bad("an element size is malformed"));
            }
            let mut size = d[n] as u64 & ((1u64 << (8 - m)) - 1);
            for &b in &d[n + 1..n + m] {
                size = size << 8 | b as u64;
            }
            if size == (1u64 << (7 * m)) - 1 {
                return Err(bad("an element inside a master element has unknown size"));
            }
            let body = d.get(n + m..).and_then(|b| b.get(..size as usize)).ok_or_else(|| bad("an element runs past its parent"))?;
            Ok((id, body, n + m + size as usize))
        };
        Some(match parse() {
            Ok((id, body, used)) => {
                self.0 = &d[used..];
                Ok((id, body))
            }
            Err(e) => {
                self.0 = &[];
                Err(e)
            }
        })
    }
}

struct MkTrack {
    number: u64,
    kind: u64,
    codec: String,
    private: Vec<u8>,
    default_ns: u64,
    /// The CodecDelay in nanoseconds: an AAC track's priming, which the decoder drops.
    delay_ns: u64,
    width: u32,
    height: u32,
    rate: f64,
    channels: u16,
    bits: Option<u8>,
    /// Whether the track has a ContentEncodings element (the live reader refuses such tracks).
    encoded: bool,
    /// The steps that undo its content encodings, first step first (empty when it has none).
    encodings: Vec<Unwrap>,
}

/// The content encodings of a track's frames, as the steps that undo them in the order a reader applies
/// them (the highest ContentEncodingOrder first). Header stripping and zlib are undone; encryption, other
/// compression algorithms, and encodings that cover only the CodecPrivate are refused.
fn content_encodings(body: &[u8]) -> Result<Vec<Unwrap>> {
    let mut steps: Vec<(u64, Unwrap)> = Vec::new();
    for e in Elems(body) {
        let (id, enc) = e?;
        if id != CONTENT_ENCODING {
            continue;
        }
        let (mut order, mut scope, mut kind) = (0u64, 1u64, 0u64);
        let mut compression = None;
        for e in Elems(enc) {
            let (id, v) = e?;
            match id {
                CONTENT_ENCODING_ORDER => order = be_uint(v)?,
                CONTENT_ENCODING_SCOPE => scope = be_uint(v)?,
                CONTENT_ENCODING_TYPE => kind = be_uint(v)?,
                CONTENT_COMPRESSION => compression = Some(content_compression(v)?),
                _ => {}
            }
        }
        if kind != 0 {
            return Err(Error::new(ErrorCode::UnsupportedFormat, "Matroska content encryption is not supported"));
        }
        if scope & 1 == 0 {
            return Err(Error::new(ErrorCode::UnsupportedFormat, "a Matroska content encoding of the CodecPrivate alone is not supported"));
        }
        let (algo, settings) = compression.ok_or_else(|| bad("a content encoding has no compression"))?;
        let step = match algo {
            0 => Unwrap::Inflate,
            3 => Unwrap::Prepend(settings),
            other => {
                return Err(Error::new(ErrorCode::UnsupportedFormat, format!("Matroska content compression algorithm {other} is not supported (0, zlib, and 3, header stripping, are)")));
            }
        };
        steps.push((order, step));
    }
    steps.sort_by_key(|s| std::cmp::Reverse(s.0));
    Ok(steps.into_iter().map(|s| s.1).collect())
}

/// ContentCompression: the algorithm, and its settings (for header stripping, the bytes taken off each frame).
fn content_compression(body: &[u8]) -> Result<(u64, Vec<u8>)> {
    let (mut algo, mut settings) = (0u64, Vec::new());
    for e in Elems(body) {
        let (id, v) = e?;
        match id {
            CONTENT_COMP_ALGO => algo = be_uint(v)?,
            CONTENT_COMP_SETTINGS => settings = v.to_vec(),
            _ => {}
        }
    }
    Ok((algo, settings))
}

fn parse_tracks(body: &[u8]) -> Result<Vec<MkTrack>> {
    let mut out = Vec::new();
    for e in Elems(body) {
        let (id, entry) = e?;
        if id != TRACK_ENTRY {
            continue;
        }
        let mut t = MkTrack { number: 0, kind: 0, codec: String::new(), private: Vec::new(), default_ns: 0, delay_ns: 0, width: 0, height: 0, rate: 8000.0, channels: 1, bits: None, encoded: false, encodings: Vec::new() };
        for e in Elems(entry) {
            let (id, v) = e?;
            match id {
                TRACK_NUMBER => t.number = be_uint(v)?,
                TRACK_TYPE => t.kind = be_uint(v)?,
                CODEC_ID => t.codec = String::from_utf8_lossy(v).trim_end_matches('\0').to_string(),
                CODEC_PRIVATE => t.private = v.to_vec(),
                DEFAULT_DURATION => t.default_ns = be_uint(v)?,
                CODEC_DELAY => t.delay_ns = be_uint(v)?,
                CONTENT_ENCODINGS => {
                    t.encoded = true;
                    t.encodings = content_encodings(v)?;
                }
                VIDEO => {
                    for e in Elems(v) {
                        let (id, v) = e?;
                        match id {
                            PIXEL_WIDTH => t.width = be_uint(v)? as u32,
                            PIXEL_HEIGHT => t.height = be_uint(v)? as u32,
                            _ => {}
                        }
                    }
                }
                AUDIO => {
                    for e in Elems(v) {
                        let (id, v) = e?;
                        match id {
                            SAMPLING_FREQUENCY => t.rate = be_float(v)?,
                            CHANNELS => t.channels = be_uint(v)? as u16,
                            BIT_DEPTH => t.bits = Some(be_uint(v)? as u8).filter(|&b| b > 0),
                            _ => {}
                        }
                    }
                }
                _ => {}
            }
        }
        out.push(t);
    }
    Ok(out)
}

/// The chapters of a Chapters element, depth first, in the order the file gives them. Hidden and disabled
/// chapters are left out. A chapter's title is its first display string, the one in English or an
/// undetermined language when there are several. Lists are capped; nesting past [`MAX_CHAPTER_DEPTH`] is cut.
fn parse_chapters(body: &[u8]) -> Result<Vec<Chapter>> {
    let mut out = Vec::new();
    for e in Elems(body) {
        let (id, edition) = e?;
        if id == EDITION_ENTRY {
            chapter_atoms(edition, 0, &mut out)?;
        }
    }
    Ok(out)
}

fn chapter_atoms(parent: &[u8], depth: u32, out: &mut Vec<Chapter>) -> Result<()> {
    for e in Elems(parent) {
        let (id, atom) = e?;
        if id == CHAPTER_ATOM {
            chapter_atom(atom, depth, out)?;
        }
    }
    Ok(())
}

fn chapter_atom(atom: &[u8], depth: u32, out: &mut Vec<Chapter>) -> Result<()> {
    if out.len() >= MAX_LISTED {
        return Ok(());
    }
    let (mut start, mut end, mut hidden, mut enabled) = (0u64, None, false, true);
    let mut titles: Vec<(String, Option<String>)> = Vec::new();
    let text = |s: &[u8]| String::from_utf8_lossy(s).trim_end_matches('\0').chars().take(MAX_NAME).collect::<String>();
    for e in Elems(atom) {
        let (id, v) = e?;
        match id {
            CHAPTER_TIME_START => start = be_uint(v)?,
            CHAPTER_TIME_END => end = Some(be_uint(v)?),
            CHAPTER_FLAG_HIDDEN => hidden = be_uint(v)? != 0,
            CHAPTER_FLAG_ENABLED => enabled = be_uint(v)? != 0,
            CHAPTER_DISPLAY => {
                let (mut title, mut lang, mut ietf) = (String::new(), None, None);
                for e in Elems(v) {
                    let (id, s) = e?;
                    match id {
                        CHAP_STRING => title = text(s),
                        CHAP_LANGUAGE => lang = Some(text(s)),
                        CHAP_LANGUAGE_IETF => ietf = Some(text(s)),
                        _ => {}
                    }
                }
                titles.push((title, ietf.or(lang)));
            }
            _ => {}
        }
    }
    if !hidden && enabled {
        let pick = titles.iter().find(|(_, l)| l.as_deref().is_some_and(|l| l.starts_with("en") || l == "und")).or(titles.first());
        let (title, language) = pick.cloned().unwrap_or_default();
        let secs = |ns: u64| ns as f64 / 1e9;
        out.push(Chapter { start: secs(start), end: end.map(secs), title, language, depth });
    }
    if depth < MAX_CHAPTER_DEPTH {
        for e in Elems(atom) {
            let (id, child) = e?;
            if id == CHAPTER_ATOM {
                chapter_atom(child, depth + 1, out)?;
            }
        }
    }
    Ok(())
}

fn subtitle_codec(id: &str) -> Option<CodecId> {
    Some(match id {
        "S_TEXT/UTF8" | "S_TEXT/ASCII" => CodecId::SubRip,
        "S_TEXT/ASS" | "S_TEXT/SSA" | "S_ASS" | "S_SSA" => CodecId::Ass,
        // WebM's form of WebVTT (what FFmpeg writes) holds the cue payload in the block; the settings are in a block addition
        "S_TEXT/WEBVTT" | "D_WEBVTT/SUBTITLES" | "D_WEBVTT/CAPTIONS" => CodecId::WebVtt,
        _ => return None,
    })
}

/// A subtitle track's cues as samples: a block's time and its BlockDuration (or the track's default
/// duration). Blocks of no size, or longer than a cue may be, are left out, and the track holds
/// [`crate::subtitle::MAX_CUES`] cues and [`crate::subtitle::MAX_TOTAL_BYTES`] bytes at most.
fn build_subtitle(t: &MkTrack, codec: CodecId, index: usize, frames: &[&Frame], scale: u64, samples: &mut Vec<Sample>) -> TrackInfo {
    let g = gcd(scale as i64, 1_000_000_000);
    let tick = Rational::new(scale as i64 / g, 1_000_000_000 / g);
    let default_ticks = (t.default_ns / scale.max(1)) as i64;
    let (mut end, mut bytes, mut cues) = (0i64, 0usize, 0usize);
    for f in frames {
        let size = f.size as usize;
        if size == 0 || size > crate::subtitle::MAX_CUE_BYTES * 2 || cues >= crate::subtitle::MAX_CUES || bytes + size > crate::subtitle::MAX_TOTAL_BYTES {
            continue;
        }
        let duration = f.duration.filter(|&d| d > 0).unwrap_or(default_ticks).max(0);
        end = end.max(f.tc + duration);
        bytes += size;
        cues += 1;
        samples.push(Sample { stream: index as u16, sync: true, offset: f.offset, size: f.size, dts: f.tc, cts_offset: 0, duration: u32::try_from(duration).unwrap_or(u32::MAX) });
    }
    let extradata = t.private[..t.private.len().min(crate::subtitle::MAX_CUE_BYTES * 32)].to_vec();
    let stream = StreamInfo { index, codec, time_base: tick, duration: Some(end), bits: None, extradata, lead_in: 0, params: StreamParams::Subtitle };
    TrackInfo { stream, config: Config { prefix: Vec::new(), nal_length: 0, bits: 8, chroma: 1 }, convert: false, swap: 0, unwrap: t.encodings.clone() }
}

fn video_codec(id: &str) -> Option<CodecId> {
    Some(match id {
        "V_MPEG4/ISO/AVC" => CodecId::H264,
        "V_MPEGH/ISO/HEVC" => CodecId::Hevc,
        "V_AV1" => CodecId::Av1,
        "V_VP9" => CodecId::Vp9,
        "V_VP8" => CodecId::Vp8,
        "V_MPEG4/ISO/ASP" | "V_MPEG4/ISO/SP" | "V_MPEG4/ISO/AP" => CodecId::Mpeg4,
        "V_MJPEG" => CodecId::Mjpeg,
        "V_MPEG2" => CodecId::Mpeg2Video,
        "V_MPEG1" => CodecId::Mpeg1Video,
        _ => return None,
    })
}

fn audio_codec(id: &str) -> Option<CodecId> {
    Some(match id {
        "A_OPUS" => CodecId::Opus,
        "A_VORBIS" => CodecId::Vorbis,
        "A_FLAC" => CodecId::Flac,
        "A_MPEG/L3" => CodecId::Mp3,
        "A_MPEG/L2" => CodecId::Mp2,
        "A_AC3" => CodecId::Ac3,
        "A_EAC3" => CodecId::Eac3,
        "A_ALAC" => CodecId::Alac,
        s if s == "A_AAC" || s.starts_with("A_AAC/") => CodecId::Aac,
        _ => return None,
    })
}

/// The nominal frame rate closest to `fps` (to 0.02 %), or `fps` as a short fraction.
fn frame_rate(fps: f64) -> Rational {
    if !fps.is_finite() || fps <= 0.0 {
        return Rational::new(0, 1);
    }
    const NOMINAL: [(i64, i64); 12] =
        [(24000, 1001), (24, 1), (25, 1), (30000, 1001), (30, 1), (48, 1), (50, 1), (60000, 1001), (60, 1), (100, 1), (120000, 1001), (120, 1)];
    for (n, d) in NOMINAL {
        if (fps - n as f64 / d as f64).abs() <= fps * 2e-4 {
            return Rational::new(n, d);
        }
    }
    let num = (fps * 1000.0).round() as i64;
    let g = gcd(num, 1000);
    Rational::new(num / g, 1000 / g)
}

/// Samples per frame of codecs whose frames all have the same length, when it can be told.
fn audio_frame_len(t: &MkTrack, codec: CodecId) -> Option<u32> {
    if codec == CodecId::Vorbis {
        return None; // block sizes vary: the container's timestamps are all there is
    }
    if t.default_ns > 0 {
        let n = (t.default_ns as f64 * t.rate / 1e9).round();
        if (1.0..1e6).contains(&n) {
            return Some(n as u32);
        }
    }
    match codec {
        CodecId::Aac => Some(1024),
        CodecId::Mp3 => Some(if t.rate >= 32000.0 { 1152 } else { 576 }),
        CodecId::Mp2 => Some(1152),
        CodecId::Ac3 | CodecId::Eac3 => Some(1536),
        _ => None,
    }
}

/// Bit depth and chroma format (1 = 4:2:0, 2 = 4:2:2, 3 = 4:4:4) from the header of a VP9 key frame.
fn vp9_format(d: &[u8]) -> Option<(u8, u8)> {
    let k = crate::vpxav1::vp9_key(d)?;
    Some((k.bits, match (k.ssx, k.ssy) { (true, true) => 1, (true, false) => 2, (false, false) => 3, _ => return None }))
}

/// A Matroska PCM track's format: its codec, its sample width in bytes, and whether its samples are big-endian.
struct MkPcm {
    codec: CodecId,
    width: usize,
    big: bool,
}

/// The PCM format of an audio track by its CodecID (`None` for other codecs). Integer samples of 8 bits are
/// unsigned, as WAV has them; the bit depth defaults to 16 for integers and 32 for floats.
fn pcm_format(id: &str, bits: Option<u8>) -> Result<Option<MkPcm>> {
    let (float, big) = match id {
        "A_PCM/INT/LIT" => (false, false),
        "A_PCM/INT/BIG" => (false, true),
        "A_PCM/FLOAT/IEEE" => (true, false),
        _ => return Ok(None),
    };
    let depth = bits.unwrap_or(if float { 32 } else { 16 });
    let (codec, width) = match (float, depth) {
        (false, 8) => (CodecId::PcmU8, 1),
        (false, 16) => (CodecId::PcmS16Le, 2),
        (false, 24) => (CodecId::PcmS24Le, 3),
        (false, 32) => (CodecId::PcmS32Le, 4),
        (true, 32) => (CodecId::PcmF32Le, 4),
        (true, 64) => (CodecId::PcmF64Le, 8),
        _ => return Err(Error::new(ErrorCode::UnsupportedCodec, format!("Matroska {id} at {depth} bits is not supported"))),
    };
    Ok(Some(MkPcm { codec, width, big }))
}

fn build_track(t: &MkTrack, index: usize, frames: &[&Frame], scale: u64, samples: &mut Vec<Sample>, vp9: Option<(u8, u8)>) -> Result<Option<TrackInfo>> {
    if t.kind == TRACK_SUBTITLE {
        return Ok(subtitle_codec(&t.codec).map(|codec| build_subtitle(t, codec, index, frames, scale, samples)));
    }
    let video = match t.kind {
        1 => true,
        2 => false,
        _ => return Ok(None),
    };
    let pcm = if video { None } else { pcm_format(&t.codec, t.bits)? };
    let codec = if video { video_codec(&t.codec) } else { pcm.as_ref().map(|p| p.codec).or_else(|| audio_codec(&t.codec)) };
    // big-endian samples are turned little-endian by the reader, a sample at a time
    let swap = pcm.as_ref().map_or(0, |p| if p.big { p.width } else { 0 });
    let Some(codec) = codec else { return Ok(None) };
    let g = gcd(scale as i64, 1_000_000_000);
    let tick = Rational::new(scale as i64 / g, 1_000_000_000 / g);
    let default_ticks = (t.default_ns / scale.max(1)) as i64;
    let first = samples.len();
    let stream_no = index as u16;
    let mut extradata = Vec::new();
    let mut bits = None;
    // AAC's codec delay (priming) and the samples its discard padding and delay take off the end
    let (mut lead_in, mut trim) = (0i64, 0i64);
    let (time_base, params, config, convert);

    if video {
        config = match codec {
            CodecId::H264 => parse_avcc(&t.private)?,
            CodecId::Hevc => parse_hvcc(&t.private)?,
            CodecId::Vp9 => {
                let (bits, chroma) = vp9.unwrap_or((8, 1));
                Config { prefix: Vec::new(), nal_length: 0, bits, chroma }
            }
            CodecId::Mpeg4 | CodecId::Mpeg2Video | CodecId::Mpeg1Video => {
                extradata = t.private.clone();
                Config { prefix: Vec::new(), nal_length: 0, bits: 8, chroma: crate::mpegvideo::chroma_of(codec, &t.private) }
            }
            CodecId::Av1 if !t.private.is_empty() => {
                extradata = t.private.clone();
                av1c_config(&t.private)?
            }
            _ => Config { prefix: Vec::new(), nal_length: 0, bits: 8, chroma: 1 },
        };
        convert = matches!(codec, CodecId::H264 | CodecId::Hevc);
        time_base = tick;
        let n = frames.len();
        let mut sorted: Vec<i64> = frames.iter().map(|f| f.tc).collect();
        sorted.sort_unstable();
        for (i, f) in frames.iter().enumerate() {
            let dts = sorted[i];
            let duration = match sorted.get(i + 1) {
                Some(next) => next - dts,
                None if default_ticks > 0 => default_ticks,
                None => i.checked_sub(1).map_or(1, |p| sorted[i] - sorted[p]),
            };
            let cts_offset = i32::try_from(f.tc - dts).map_err(|_| bad("a frame is shown implausibly long after it is decoded"))?;
            samples.push(Sample { stream: stream_no, sync: f.key, offset: f.offset, size: f.size, dts, cts_offset, duration: u32::try_from(duration.max(0)).unwrap_or(u32::MAX) });
        }
        let fps = if t.default_ns > 0 {
            1e9 / t.default_ns as f64
        } else if n > 1 && sorted[n - 1] > sorted[0] {
            (n - 1) as f64 / ((sorted[n - 1] - sorted[0]) as f64 * tick.as_f64())
        } else {
            0.0
        };
        let pix_fmt = config.pix_fmt();
        params = StreamParams::Video { width: t.width, height: t.height, pix_fmt, frame_rate: frame_rate(fps) };
    } else {
        config = Config { prefix: Vec::new(), nal_length: 0, bits: 8, chroma: 1 };
        convert = false;
        let rate = t.rate.round() as u32;
        bits = t.bits;
        if matches!(codec, CodecId::Aac | CodecId::Opus | CodecId::Mp3 | CodecId::Mp2 | CodecId::Vorbis) {
            extradata = t.private.clone();
            bits = None;
        }
        if let Some(p) = &pcm {
            bits = Some(p.width as u8 * 8);
        }
        // Opus always counts in 48 kHz samples, whatever rate the file says it came from.
        let opus = codec == CodecId::Opus;
        let rate = if opus { 48_000 } else { rate };
        // PCM blocks are whole sample frames of whatever length they hold, so each one's length comes from its size
        let fixed = if pcm.is_some() { Some(0) } else { audio_frame_len(t, codec) };
        match fixed.filter(|_| rate > 0).or(if opus { Some(0) } else { None }) {
            Some(len) => {
                time_base = Rational::new(1, rate as i64);
                // AAC's codec delay is its priming, and its discard padding (the last block's) the end
                // padding: the stream is the samples between them, as FFmpeg reads it
                // (MPEG audio's codec delay is its encoder's priming, which FFmpeg drops too)
                if matches!(codec, CodecId::Aac | CodecId::Mp2 | CodecId::Mp3 | CodecId::Ac3 | CodecId::Eac3) {
                    let to_samples = |ns: i64| ((ns.max(0) as i128 * rate as i128 + 500_000_000) / 1_000_000_000) as i64;
                    let delay = to_samples(t.delay_ns.min(i64::MAX as u64) as i64);
                    if delay > 0 {
                        let discard: i64 = frames.iter().map(|f| to_samples(f.discard)).sum();
                        lead_in = delay;
                        trim = delay + discard;
                    }
                }
                let tolerance = rate as i64 / 500 + 1; // the file's ticks are coarser than a sample
                let mut next: Option<i64> = None;
                for f in frames {
                    let len = if opus {
                        // a block's frames share one TOC byte, and so one length
                        let n = ironav_codec::opus::packet_samples(&f.head[..f.size.min(2) as usize]).unwrap_or(960);
                        let n = if f.size == 0 { 960 } else { n as u32 };
                        n.saturating_sub((f.discard as i128 * 48_000 / 1_000_000_000) as u32)
                    } else if let Some(p) = &pcm {
                        f.size / (p.width * t.channels.max(1) as usize) as u32
                    } else {
                        len
                    };
                    let at = (f.tc as i128 * scale as i128 * rate as i128 + 500_000_000) / 1_000_000_000;
                    let at = at as i64 + f.lace as i64 * len as i64;
                    let pts = match next {
                        Some(n) if (n - at).abs() <= tolerance => n,
                        _ => at,
                    };
                    samples.push(Sample { stream: stream_no, sync: true, offset: f.offset, size: f.size, dts: pts, cts_offset: 0, duration: len });
                    next = Some(pts + len as i64);
                }
            }
            None => {
                time_base = tick;
                let pts: Vec<i64> = frames.iter().map(|f| f.tc + f.lace as i64 * default_ticks).collect();
                for (i, f) in frames.iter().enumerate() {
                    let duration = match pts.get(i + 1) {
                        Some(next) => next - pts[i],
                        None => f.duration.filter(|&d| d > 0).unwrap_or(default_ticks),
                    };
                    samples.push(Sample { stream: stream_no, sync: true, offset: f.offset, size: f.size, dts: pts[i], cts_offset: 0, duration: u32::try_from(duration.max(0)).unwrap_or(u32::MAX) });
                }
            }
        }
        params = StreamParams::Audio { sample_rate: rate, channels: t.channels };
    }
    let duration: i64 = samples[first..].iter().map(|s| s.duration as i64).sum::<i64>() - trim;
    let stream = StreamInfo { index, codec, time_base, duration: Some(duration.max(0)), bits, extradata, lead_in, params };
    Ok(Some(TrackInfo { stream, config, convert, swap, unwrap: t.encodings.clone() }))
}

impl<R: Read + Seek> Scan<R> {
    fn new(r: R, len: u64) -> Self {
        Scan {
            r: BufReader::with_capacity(256 << 10, r),
            pos: 0,
            len,
            pending: None,
            truncated: false,
            frames: Vec::new(),
            scale: 1_000_000,
            tracks: None,
            chapters: None,
            attachments: Vec::new(),
            doc_type: String::new(),
            segment_data: 0,
            seek_entries: Vec::new(),
            stop_at_cluster: false,
            first_cluster: None,
            duration: None,
            min_tc: i64::MAX,
            max_tc: i64::MIN,
        }
    }
}

/// Open a Matroska or WebM file for reading.
pub fn open<R: Read + Seek + Send>(mut r: R) -> Result<Mp4Demuxer<BufReader<R>>> {
    let len = r.seek(SeekFrom::End(0))?;
    r.seek(SeekFrom::Start(0))?;
    let mut scan = Scan::new(r, len);
    scan.file()?;
    let body = scan.tracks.take().ok_or_else(|| bad("the file has no track list"))?;
    let mk = parse_tracks(&body)?;
    let (tracks, samples, unsupported, _) = build_tracks(&mut scan, &mk, None)?;
    if tracks.is_empty() {
        return Err(Error::new(ErrorCode::UnsupportedCodec, "the file has no audio or video track IronAV recognises"));
    }
    let name = if scan.doc_type == "webm" { "webm" } else { "matroska" };
    // chapters are a convenience: damaged ones are left out, not a reason to refuse the file
    let chapters = scan.chapters.as_deref().and_then(|c| parse_chapters(c).ok()).unwrap_or_default();
    let attachments = std::mem::take(&mut scan.attachments);
    Ok(Mp4Demuxer::from_parts(scan.r, name, tracks, samples).with_extras(chapters, attachments, unsupported))
}

/// The tracks and samples of the frames found so far, `mk` being the file's track list. `vp9_hint` is the
/// format of a VP9 track found earlier, for a part of the file that has no key frame. The last
/// item returned is the VP9 format found.
type Built = (Vec<TrackInfo>, Vec<Sample>, Vec<String>, Option<(u8, u8)>);

fn build_tracks<R: Read + Seek>(scan: &mut Scan<R>, mk: &[MkTrack], vp9_hint: Option<(u8, u8)>) -> Result<Built> {
    let mut tracks = Vec::new();
    let mut samples = Vec::new();
    let mut unsupported = Vec::new();
    let mut found = vp9_hint;
    for t in mk {
        let frames: Vec<&Frame> = scan.frames.iter().filter(|f| f.track == t.number).collect();
        let mut vp9 = vp9_hint;
        if video_codec(&t.codec) == Some(CodecId::Vp9) {
            if let Some(f) = frames.iter().find(|f| f.key) {
                // the header is read from the frame as the codec sees it, so an encoded track is decoded first
                let mut raw = vec![0u8; f.size as usize];
                if scan.r.seek(SeekFrom::Start(f.offset)).is_ok() && scan.r.read_exact(&mut raw).is_ok() {
                    if let Ok(frame) = unwrap_frame(&t.encodings, raw) {
                        vp9 = vp9_format(&frame[..frame.len().min(16)]).or(vp9);
                        found = vp9;
                    }
                }
            }
        }
        let built = build_track(t, tracks.len(), &frames, scan.scale, &mut samples, vp9)?;
        if built.is_none() {
            unsupported.push(describe_track(t));
        }
        if let Some(mut info) = built {
            // MPEG-2 whose codec configuration has no sequence header: the first picture has one
            if let (Some(f), Some(CodecId::Mpeg2Video)) = (frames.iter().find(|f| f.key), video_codec(&t.codec)) {
                let mut head = vec![0u8; (f.size as usize).min(1024)];
                if scan.r.seek(SeekFrom::Start(f.offset)).is_ok() {
                    let n = scan.r.read(&mut head).unwrap_or(0);
                    crate::mp4demux::mpeg2_chroma_from_picture(&mut info, &head[..n]);
                }
            }
            tracks.push(info);
        }
    }
    Ok((tracks, samples, unsupported, found))
}

/// A track IronAV does not read, for `unsupported_streams`.
fn describe_track(t: &MkTrack) -> String {
    let kind = match t.kind {
        1 => "video",
        2 => "audio",
        TRACK_SUBTITLE => "subtitle",
        _ => "other",
    };
    let codec: String = t.codec.chars().filter(|c| c.is_ascii_graphic()).take(64).collect();
    format!("{kind} track {}: codec '{codec}'", t.number)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironav_core::Demuxer;
    use std::io::Cursor;

    fn size(n: usize) -> Vec<u8> {
        // Always eight bytes, so the sizes need no care.
        let mut v = vec![0x01];
        v.extend_from_slice(&(n as u64).to_be_bytes()[1..]);
        v
    }

    fn el(id: u32, body: &[u8]) -> Vec<u8> {
        let mut v: Vec<u8> = id.to_be_bytes().iter().copied().skip_while(|&b| b == 0).collect();
        v.extend(size(body.len()));
        v.extend_from_slice(body);
        v
    }

    fn uint(id: u32, n: u64) -> Vec<u8> {
        el(id, &n.to_be_bytes())
    }

    fn simple(track: u8, rel: i16, key: bool, payload: &[u8]) -> Vec<u8> {
        let mut b = vec![0x80 | track];
        b.extend_from_slice(&rel.to_be_bytes());
        b.push(if key { 0x80 } else { 0 });
        b.extend_from_slice(payload);
        el(SIMPLE_BLOCK, &b)
    }

    /// A video track (VP9, which needs no codec private) and an AAC track.
    fn tracks() -> Vec<u8> {
        let video = [uint(TRACK_NUMBER, 1), uint(TRACK_TYPE, 1), el(CODEC_ID, b"V_VP9"), uint(DEFAULT_DURATION, 40_000_000), el(VIDEO, &[uint(PIXEL_WIDTH, 64), uint(PIXEL_HEIGHT, 48)].concat())].concat();
        let audio = [
            uint(TRACK_NUMBER, 2),
            uint(TRACK_TYPE, 2),
            el(CODEC_ID, b"A_AAC"),
            el(CODEC_PRIVATE, &[0x12, 0x10]),
            el(AUDIO, &[el(SAMPLING_FREQUENCY, &44100f64.to_be_bytes()), uint(CHANNELS, 2)].concat()),
        ]
        .concat();
        el(TRACKS, &[el(TRACK_ENTRY, &video), el(TRACK_ENTRY, &audio)].concat())
    }

    fn file(clusters: Vec<u8>, segment_size_known: bool) -> Vec<u8> {
        let mut f = el(EBML, &el(DOC_TYPE, b"webm"));
        let body = [el(INFO, &uint(TIMECODE_SCALE, 1_000_000)), tracks(), clusters].concat();
        f.extend(SEGMENT.to_be_bytes());
        if segment_size_known {
            f.extend(size(body.len()));
        } else {
            f.push(0xFF);
        }
        f.extend(body);
        f
    }

    #[test]
    fn frames_are_found_with_their_times_keyframes_and_laces() {
        // Audio frames 1024 samples at 44.1 kHz are 23.22 ms; the file's millisecond ticks drift from that.
        let mut lace = vec![0x80 | 2, 0, 0, 0b0000_0010, 2]; // track 2, Xiph lacing, 3 frames
        lace.extend([4, 5]); // sizes of the first two
        lace.extend([1; 4]);
        lace.extend([2; 5]);
        lace.extend([3; 6]);
        let cluster_body = [
            uint(CLUSTER_TIMECODE, 1000),
            simple(1, 0, true, &[0xAA; 7]),
            el(BLOCK_GROUP, &[el(BLOCK, &[0x81, 0, 40, 0, 0xBB, 0xBB]), el(REFERENCE_BLOCK, &[0xFF])].concat()),
            el(SIMPLE_BLOCK, &lace),
            simple(1, 80, false, &[0xCC; 3]),
        ]
        .concat();
        let bytes = file(el(CLUSTER, &cluster_body), true);
        let mut d = open(Cursor::new(bytes)).unwrap();
        assert_eq!(d.format_name(), "webm");
        let s = d.streams().to_vec();
        assert_eq!((s[0].codec, s[1].codec), (CodecId::Vp9, CodecId::Aac));
        assert_eq!(s[1].extradata, vec![0x12, 0x10]);
        assert_eq!(s[1].time_base, Rational::new(1, 44100));
        match s[0].params {
            StreamParams::Video { width, height, frame_rate, .. } => assert_eq!((width, height, frame_rate), (64, 48, Rational::new(25, 1))),
            _ => panic!("first stream is not video"),
        }
        let mut video = Vec::new();
        let mut audio = Vec::new();
        while let Some(p) = d.read_packet().unwrap() {
            if p.stream == 0 { video.push((p.pts, p.duration, p.data)) } else { audio.push((p.pts, p.duration, p.data)) }
        }
        assert_eq!(video, vec![(1000, 40, vec![0xAA; 7]), (1040, 40, vec![0xBB; 2]), (1080, 40, vec![0xCC; 3])]);
        // The three laced frames follow each other exactly, 1024 samples apart, from 1 s = 44100 samples.
        assert_eq!(audio.iter().map(|a| (a.0, a.1)).collect::<Vec<_>>(), vec![(44100, 1024), (45124, 1024), (46148, 1024)]);
        assert_eq!(audio.iter().map(|a| a.2.len()).collect::<Vec<_>>(), vec![4, 5, 6]);
        assert_eq!(s[0].duration, Some(120));
    }

    #[test]
    fn ebml_and_fixed_size_laces_split_their_frames() {
        let mut ebml = vec![0x82, 0, 0, 0b0000_0110, 2, 0x83, 0xC1]; // sizes 3, then +2 = 5, then the rest
        ebml.extend([1; 3]);
        ebml.extend([2; 5]);
        ebml.extend([3; 4]);
        let mut fixed = vec![0x82, 0, 100, 0b0000_0100, 1]; // two frames of 4 bytes
        fixed.extend([4; 8]);
        let cluster = [uint(CLUSTER_TIMECODE, 0), el(SIMPLE_BLOCK, &ebml), el(SIMPLE_BLOCK, &fixed)].concat();
        let mut d = open(Cursor::new(file(el(CLUSTER, &cluster), true))).unwrap();
        let mut got = Vec::new();
        while let Some(p) = d.read_packet().unwrap() {
            got.push((p.pts, p.data.len()));
        }
        // The second block starts at 100 ms = 4410 samples; the first three frames end at 3072, so it is a new start.
        assert_eq!(got, vec![(0, 3), (1024, 5), (2048, 4), (4410, 4), (5434, 4)]);
    }

    #[test]
    fn unknown_sizes_and_a_cut_off_file_are_read_as_far_as_they_go() {
        let c1 = [uint(CLUSTER_TIMECODE, 0), simple(1, 0, true, &[1; 4]), simple(1, 40, false, &[2; 4])].concat();
        let c2 = [uint(CLUSTER_TIMECODE, 80), simple(1, 0, false, &[3; 4]), simple(1, 40, false, &[4; 4])].concat();
        let mut clusters = Vec::new();
        for c in [&c1, &c2] {
            clusters.extend(CLUSTER.to_be_bytes());
            clusters.push(0xFF); // unknown size
            clusters.extend(c.iter());
        }
        let mut bytes = file(clusters, false);
        bytes.truncate(bytes.len() - 3); // the last frame loses its tail
        let mut d = open(Cursor::new(bytes)).unwrap();
        let mut pts = Vec::new();
        while let Some(p) = d.read_packet().unwrap() {
            pts.push(p.pts);
        }
        assert_eq!(pts, vec![0, 40, 80], "the cut-off frame is dropped, the rest survive");
    }

    #[test]
    fn reordered_video_gets_decode_times_from_the_sorted_presentation_times() {
        // Decode order I P B B: presentation 0, 120, 40, 80.
        let cluster = [uint(CLUSTER_TIMECODE, 0), simple(1, 0, true, &[1]), simple(1, 120, false, &[2]), simple(1, 40, false, &[3]), simple(1, 80, false, &[4])].concat();
        let mut d = open(Cursor::new(file(el(CLUSTER, &cluster), true))).unwrap();
        let mut got = Vec::new();
        while let Some(p) = d.read_packet().unwrap() {
            got.push((p.pts, p.duration));
        }
        assert_eq!(got, vec![(0, 40), (120, 40), (40, 40), (80, 40)]);
    }

    fn subtitle_entry(number: u64, codec: &str, private: &[u8]) -> Vec<u8> {
        let mut t = vec![uint(TRACK_NUMBER, number), uint(TRACK_TYPE, TRACK_SUBTITLE), el(CODEC_ID, codec.as_bytes())];
        if !private.is_empty() {
            t.push(el(CODEC_PRIVATE, private));
        }
        el(TRACK_ENTRY, &t.concat())
    }

    /// A block group holding one cue: the block, then its duration in ticks.
    fn cue_block(track: u8, rel: i16, text: &[u8], duration: u64) -> Vec<u8> {
        let mut b = vec![0x80 | track];
        b.extend_from_slice(&rel.to_be_bytes());
        b.push(0);
        b.extend_from_slice(text);
        el(BLOCK_GROUP, &[el(BLOCK, &b), uint(BLOCK_DURATION, duration)].concat())
    }

    /// A file with `entries` as its tracks, `extra` (chapters, attachments) after them and `cluster` as its only cluster.
    fn file_with(entries: &[Vec<u8>], extra: Vec<u8>, cluster: Vec<u8>) -> Vec<u8> {
        let body = [el(INFO, &uint(TIMECODE_SCALE, 1_000_000)), el(TRACKS, &entries.concat()), extra, el(CLUSTER, &cluster)].concat();
        let mut f = el(EBML, &el(DOC_TYPE, b"matroska"));
        f.extend(el(SEGMENT, &body));
        f
    }

    #[test]
    fn text_tracks_are_subtitle_streams_with_their_cues() {
        let video = el(TRACK_ENTRY, &[uint(TRACK_NUMBER, 1), uint(TRACK_TYPE, 1), el(CODEC_ID, b"V_VP9"), uint(DEFAULT_DURATION, 40_000_000), el(VIDEO, &[uint(PIXEL_WIDTH, 64), uint(PIXEL_HEIGHT, 48)].concat())].concat());
        let header = b"[Script Info]\nTitle: x\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\n";
        let entries = [video, subtitle_entry(2, "S_TEXT/UTF8", b""), subtitle_entry(3, "S_TEXT/ASS", header), subtitle_entry(4, "S_TEXT/WEBVTT", b""), subtitle_entry(5, "S_VOBSUB", b"")];
        let cluster = [
            uint(CLUSTER_TIMECODE, 1000),
            simple(1, 0, true, &[0xAA; 5]),
            cue_block(2, 100, b"<i>Hello</i>", 1500),
            cue_block(3, 200, b"0,0,Default,,0,0,0,,{\\an8}Top", 700),
            cue_block(4, 300, b"web cue", 400),
            cue_block(5, 400, b"\x00\x01", 400),
            cue_block(2, 2000, b"later", 500),
            simple(1, 40, false, &[0xBB; 5]),
        ]
        .concat();
        let mut d = open(Cursor::new(file_with(&entries, Vec::new(), cluster))).unwrap();
        let s = d.streams().to_vec();
        assert_eq!(s.iter().map(|s| s.codec).collect::<Vec<_>>(), vec![CodecId::Vp9, CodecId::SubRip, CodecId::Ass, CodecId::WebVtt]);
        assert!(s[1..].iter().all(|s| s.params == StreamParams::Subtitle && s.time_base == Rational::new(1, 1000)));
        assert_eq!(s[2].extradata, header.to_vec(), "the ASS header is the track's private data");
        assert_eq!(s[1].duration, Some(3500));
        let mut cues: Vec<Vec<(i64, i64, Vec<u8>)>> = vec![Vec::new(); 4];
        while let Some(p) = d.read_packet().unwrap() {
            cues[p.stream].push((p.pts, p.duration, p.data));
        }
        assert_eq!(cues[1], vec![(1100, 1500, b"<i>Hello</i>".to_vec()), (3000, 500, b"later".to_vec())]);
        assert_eq!(cues[2], vec![(1200, 700, b"0,0,Default,,0,0,0,,{\\an8}Top".to_vec())]);
        assert_eq!(cues[3], vec![(1300, 400, b"web cue".to_vec())]);
        assert_eq!(crate::subtitle::plain_text(CodecId::Ass, &cues[2][0].2), "Top");
        let u = d.unsupported_streams();
        assert_eq!(u.len(), 1);
        assert!(u[0].contains("S_VOBSUB") && u[0].contains("subtitle"), "{u:?}");
    }

    #[test]
    fn a_file_of_subtitles_alone_opens_and_oversize_cues_are_left_out() {
        let big = vec![b'x'; crate::subtitle::MAX_CUE_BYTES * 2 + 1];
        let cluster = [uint(CLUSTER_TIMECODE, 0), cue_block(1, 0, b"kept", 100), cue_block(1, 200, &big, 100), cue_block(1, 400, b"", 100)].concat();
        let mut d = open(Cursor::new(file_with(&[subtitle_entry(1, "S_TEXT/UTF8", b"")], Vec::new(), cluster))).unwrap();
        let mut n = 0;
        while let Some(p) = d.read_packet().unwrap() {
            assert_eq!(p.data, b"kept");
            n += 1;
        }
        assert_eq!(n, 1);
        // seeking in it lands on the cue
        assert_eq!(d.seek(0.0).unwrap(), 0.0);
    }

    fn atom(start_ns: u64, end_ns: Option<u64>, titles: &[(&str, &str)], children: &[Vec<u8>], hidden: bool) -> Vec<u8> {
        let mut a = vec![uint(0x73C4, 7), uint(CHAPTER_TIME_START, start_ns)];
        if let Some(e) = end_ns {
            a.push(uint(CHAPTER_TIME_END, e));
        }
        if hidden {
            a.push(uint(CHAPTER_FLAG_HIDDEN, 1));
        }
        for (lang, title) in titles {
            a.push(el(CHAPTER_DISPLAY, &[el(CHAP_STRING, title.as_bytes()), el(CHAP_LANGUAGE, lang.as_bytes())].concat()));
        }
        a.extend(children.iter().cloned());
        el(CHAPTER_ATOM, &a.concat())
    }

    #[test]
    fn chapters_and_attachments_are_listed() {
        let nested = atom(5_000_000_000, Some(8_000_000_000), &[("eng", "Scene one")], &[], false);
        let chapters = el(CHAPTERS, &el(EDITION_ENTRY, &[
            atom(0, Some(10_000_000_000), &[("fre", "Chapitre"), ("eng", "Intro")], &[nested], false),
            atom(10_000_000_000, None, &[("und", "Hidden")], &[], true),
            atom(20_500_000_000, None, &[("jpn", "Only Japanese")], &[], false),
            atom(30_000_000_000, None, &[], &[], false),
        ].concat()));
        let attached = |name: &str, mime: &str, data: &[u8]| el(ATTACHED_FILE, &[el(FILE_NAME, name.as_bytes()), el(FILE_MEDIA_TYPE, mime.as_bytes()), el(FILE_DATA, data)].concat());
        let attachments = el(ATTACHMENTS, &[attached("font.ttf", "font/ttf", &[7; 1000]), attached("cover.jpg", "image/jpeg", &[1; 10])].concat());
        let cluster = [uint(CLUSTER_TIMECODE, 0), simple(1, 0, true, &[0xAA; 5])].concat();
        let video = el(TRACK_ENTRY, &[uint(TRACK_NUMBER, 1), uint(TRACK_TYPE, 1), el(CODEC_ID, b"V_VP9"), el(VIDEO, &[uint(PIXEL_WIDTH, 64), uint(PIXEL_HEIGHT, 48)].concat())].concat());
        let d = open(Cursor::new(file_with(&[video], [chapters, attachments].concat(), cluster))).unwrap();
        let c = d.chapters();
        let view: Vec<(f64, Option<f64>, &str, u32)> = c.iter().map(|c| (c.start, c.end, c.title.as_str(), c.depth)).collect();
        assert_eq!(
            view,
            vec![(0.0, Some(10.0), "Intro", 0), (5.0, Some(8.0), "Scene one", 1), (20.5, None, "Only Japanese", 0), (30.0, None, "", 0)],
            "the hidden chapter is left out, English is preferred, and nesting is kept"
        );
        assert_eq!(c[0].language.as_deref(), Some("eng"));
        let a = d.attachments();
        assert_eq!(a.iter().map(|a| (a.name.as_str(), a.mime_type.as_str(), a.size)).collect::<Vec<_>>(), vec![("font.ttf", "font/ttf", 1000), ("cover.jpg", "image/jpeg", 10)]);
    }

    #[test]
    fn damaged_chapters_and_attachments_do_not_stop_the_file_opening() {
        // a chapter atom whose size runs past its parent, and an attachment whose field is larger than its file
        let bad_chapters = el(CHAPTERS, &[0x45, 0xB9, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x20, 0xB6, 0x84]);
        let bad_attachments = el(ATTACHMENTS, &el(ATTACHED_FILE, &[el(FILE_NAME, b"a"), vec![0x46, 0x5C, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF]].concat()));
        let cluster = [uint(CLUSTER_TIMECODE, 0), simple(1, 0, true, &[0xAA; 5])].concat();
        let video = el(TRACK_ENTRY, &[uint(TRACK_NUMBER, 1), uint(TRACK_TYPE, 1), el(CODEC_ID, b"V_VP9"), el(VIDEO, &[uint(PIXEL_WIDTH, 64), uint(PIXEL_HEIGHT, 48)].concat())].concat());
        let mut d = open(Cursor::new(file_with(&[video], [bad_chapters, bad_attachments].concat(), cluster))).unwrap();
        assert!(d.chapters().is_empty() && d.attachments().is_empty());
        assert!(d.read_packet().unwrap().is_some(), "the media is still read");
    }

    #[test]
    fn content_encodings_are_refused() {
        // an empty list of encodings changes nothing; encryption and other compression do
        assert!(content_encodings(&[]).unwrap().is_empty());
        let encrypted = el(CONTENT_ENCODING, &uint(CONTENT_ENCODING_TYPE, 1));
        let err = content_encodings(&encrypted).err().unwrap();
        assert!(err.message.contains("encryption"), "{}", err.message);
        let bzip2 = el(CONTENT_ENCODING, &el(CONTENT_COMPRESSION, &uint(CONTENT_COMP_ALGO, 1)));
        let err = content_encodings(&bzip2).err().unwrap();
        assert!(err.message.contains("algorithm 1"), "{}", err.message);
    }

    /// The content encoding element for one step: its order and a compression algorithm, with its settings.
    fn encoding(order: u64, algo: u64, settings: Option<&[u8]>) -> Vec<u8> {
        let mut comp = uint(CONTENT_COMP_ALGO, algo);
        if let Some(s) = settings {
            comp.extend(el(CONTENT_COMP_SETTINGS, s));
        }
        el(CONTENT_ENCODING, &[uint(CONTENT_ENCODING_ORDER, order), el(CONTENT_COMPRESSION, &comp)].concat())
    }

    /// A Matroska file with one PCM track (16-bit mono, 8 kHz) whose frames are stored with `encodings`.
    fn encoded_pcm(encodings: &[u8], frames: &[Vec<u8>]) -> Vec<u8> {
        let audio = [el(0xB5, &8000f64.to_be_bytes()), uint(0x9F, 1), uint(0x6264, 16)].concat();
        let track = [uint(TRACK_NUMBER, 1), uint(TRACK_TYPE, 2), el(CODEC_ID, b"A_PCM/INT/LIT"), el(AUDIO, &audio), el(CONTENT_ENCODINGS, encodings)].concat();
        let blocks: Vec<u8> = frames.iter().flat_map(|f| simple(1, 0, true, f)).collect();
        let body = [el(INFO, &uint(TIMECODE_SCALE, 1_000_000)), el(TRACKS, &el(TRACK_ENTRY, &track)), el(CLUSTER, &[uint(CLUSTER_TIMECODE, 0), blocks].concat())].concat();
        let mut f = el(EBML, &el(DOC_TYPE, b"matroska"));
        f.extend(el(SEGMENT, &body));
        f
    }

    fn packets_of(file: Vec<u8>) -> Vec<Vec<u8>> {
        let mut d = open(Cursor::new(file)).unwrap();
        let mut out = Vec::new();
        while let Some(p) = d.read_packet().unwrap() {
            out.push(p.data);
        }
        out
    }

    #[test]
    fn header_stripped_frames_get_their_header_back() {
        // every frame began with the same two bytes, which the muxer took off
        let prefix = [0xAA, 0xBB];
        let frames: Vec<Vec<u8>> = vec![vec![0xAA, 0xBB, 1, 2, 3, 4], vec![0xAA, 0xBB, 5, 6, 7, 8]];
        let stored: Vec<Vec<u8>> = frames.iter().map(|f| f[prefix.len()..].to_vec()).collect();
        let file = encoded_pcm(&encoding(0, 3, Some(&prefix)), &stored);
        assert_eq!(packets_of(file), frames);
    }

    #[test]
    fn zlib_frames_are_inflated() {
        use flate2::write::ZlibEncoder;
        use flate2::Compression;
        use std::io::Write;
        let frames: Vec<Vec<u8>> = vec![(0..64u8).collect(), vec![9; 40]];
        let stored: Vec<Vec<u8>> = frames
            .iter()
            .map(|f| {
                let mut z = ZlibEncoder::new(Vec::new(), Compression::default());
                z.write_all(f).unwrap();
                z.finish().unwrap()
            })
            .collect();
        let file = encoded_pcm(&encoding(0, 0, None), &stored);
        assert_eq!(packets_of(file), frames);
    }

    #[test]
    fn header_stripping_and_zlib_undo_in_reverse_order() {
        use flate2::write::ZlibEncoder;
        use flate2::Compression;
        use std::io::Write;
        // order 0 (header stripping) was applied to the sample first, and order 1 (zlib) after it
        let prefix = [0x10, 0x20];
        let frame = vec![0x10, 0x20, 7, 7, 7, 7, 7, 7];
        let mut z = ZlibEncoder::new(Vec::new(), Compression::default());
        z.write_all(&frame[prefix.len()..]).unwrap();
        let stored = z.finish().unwrap();
        let encodings = [encoding(0, 3, Some(&prefix)), encoding(1, 0, None)].concat();
        assert_eq!(packets_of(encoded_pcm(&encodings, &[stored])), vec![frame]);
    }
}
