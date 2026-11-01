//! MPEG-1 and MPEG-2 video outside a container: elementary streams (`.m2v`) and program streams
//! (`.mpg`, `.vob`). Neither has an index or timestamps worth trusting, so pictures come out in
//! decoding order, one packet per frame (the two fields of an interlaced frame together), numbered
//! in display order from each picture's temporal reference. The time base is the frame duration, so
//! a packet's time is its display index. A program stream's video is reassembled from its PES
//! packets; its audio and other streams are not read.

use ironav_core::{CodecId, Demuxer, Error, ErrorCode, Packet, PixelFormat, Rational, Result, StreamInfo, StreamParams};
use std::collections::VecDeque;
use std::io::Read;

/// Bytes read from the file at a time.
const CHUNK: usize = 1 << 20;
/// How much of a file is read, at most, looking for its first sequence header.
const MAX_HEAD: usize = 16 << 20;

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

/// A demuxer over a byte source: an elementary stream directly, or a program stream through its
/// PES packets.
pub struct MpegVideoDemuxer<R> {
    r: R,
    ps: bool,
    framer: Framer,
    streams: Vec<StreamInfo>,
    /// Program stream bytes not yet parsed, and the stream id of the video that is read.
    raw: Vec<u8>,
    video_id: Option<u8>,
    eof: bool,
}

impl<R: Read + Send> MpegVideoDemuxer<R> {
    /// Opens an elementary stream (`ps` false) or a program stream, reading until the first sequence
    /// header has been seen.
    pub fn open(r: R, ps: bool) -> Result<Self> {
        let mut d = MpegVideoDemuxer { r, ps, framer: Framer::default(), streams: Vec::new(), raw: Vec::new(), video_id: None, eof: false };
        let mut read = 0usize;
        while d.framer.info.is_none() {
            if read > MAX_HEAD {
                return Err(Error::invalid_input(format!("no sequence header in the first {} MiB", MAX_HEAD >> 20)));
            }
            if d.eof {
                break;
            }
            read += d.pump()?;
        }
        let Some(info) = d.framer.info.clone() else {
            return Err(Error::new(ErrorCode::InvalidInput, "the stream holds no MPEG video sequence header"));
        };
        d.streams.push(stream_of(&info));
        Ok(d)
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
            self.framer.end();
            return Ok(0);
        }
        if self.ps {
            self.raw.extend_from_slice(&buf[..n]);
            self.parse_ps(false);
        } else {
            self.framer.push(&buf[..n]);
        }
        Ok(n)
    }

    /// Parses the program stream packets in `raw`, feeding the video payloads to the framer.
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
                0xBB..=0xBF | 0xC0..=0xEF | 0xF0..=0xFF => {
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
                    if (0xE0..=0xEF).contains(&id) {
                        self.pes(&d[s..]);
                    }
                    i = d.len();
                } else {
                    i = s;
                }
                break;
            }
            if (0xE0..=0xEF).contains(&id) {
                self.pes(&d[s..s + n]);
            }
            i = s + n;
        }
        if i < d.len() && !last {
            self.raw = d[i..].to_vec();
        }
    }

    /// The video payload of one PES packet (`p` is the whole packet, start code included).
    fn pes(&mut self, p: &[u8]) {
        let id = p[3];
        if *self.video_id.get_or_insert(id) != id {
            return;
        }
        if p.len() < 7 {
            return;
        }
        let start = if p[6] >> 6 == 2 {
            // MPEG-2 PES header: flags, then the header's length
            if p.len() < 9 {
                return;
            }
            9 + p[8] as usize
        } else {
            // MPEG-1 PES header: stuffing, an optional buffer size, then timestamps
            let mut k = 6;
            while p.get(k) == Some(&0xFF) {
                k += 1;
            }
            if p.get(k).is_some_and(|b| b >> 6 == 1) {
                k += 2;
            }
            match p.get(k) {
                Some(b) if b >> 4 == 2 => k + 5,
                Some(b) if b >> 4 == 3 => k + 10,
                _ => k + 1,
            }
        };
        if let Some(payload) = p.get(start..) {
            self.framer.push(payload);
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
            if let Some(u) = self.framer.out.pop_front() {
                return Ok(Some(Packet { stream: 0, pts: u.index, duration: 1, data: u.data }));
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
}
