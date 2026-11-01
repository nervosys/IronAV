//! MPEG transport streams (`.ts`, `.m2ts`): the container of broadcast, IPTV, HLS and most cameras.
//!
//! Reading: one push parser (`TsParser`) serves both the file reader and the live
//! [`StreamDemuxer`], so a socket and a file give the same packets.
//! 188-byte packets are read, and 192-byte ones (M2TS, with a 4-byte arrival stamp in front). The
//! first program of the PAT is followed; its H.264, HEVC, AAC (ADTS), MP3 and Opus streams come out, and
//! other stream types are skipped. A stream's parameters come from its first units (the SPS, the
//! ADTS or MP3 header), so packets are held until every stream is known, and video starts at a
//! keyframe. Timestamps (90 kHz, 33 bits) are unwrapped and start from the earliest one; video keeps
//! the 90 kHz time base, audio is counted in samples. A continuity-counter jump drops the unit it
//! breaks.
//!
//! Writing: one program; video gets PID 0x100 and carries the PCR, the others follow. The PAT and
//! PMT go before every video keyframe and at least twice a second. H.264 and HEVC access units are
//! written as Annex B behind an access unit delimiter; AAC gets an ADTS header per frame. Packets
//! carry presentation times only, in decode order, so a decode time is made for video from the
//! presentation times seen so far: the one `REORDER` places back in their sorted order, which is
//! never after a presentation time when pictures are reordered by at most that many places.
//!
//! Opus follows ETSI TS 102 366 as FFmpeg writes it: stream type 0x06 with the `Opus` registration
//! descriptor and the 0x7F extension descriptor (tag 0x80) naming the channel configuration, and
//! each packet behind a control header (0x7FE0, its size, and optional start and end trims). The
//! start trims of the first packets are the stream's pre-skip, which goes in the OpusHead the reader
//! makes; the end trim is the part of a packet that is not shown. FFmpeg's decoder does not apply
//! the trims, so its output of such a stream keeps the pre-skip samples.
//!
//! AC-3 is written as stream type 0x81 with the `AC-3` registration descriptor, the way FFmpeg writes
//! it by default (the DVB descriptor 0x6A form is read, not written), in private_stream_1 PES packets.
//!
//! Not handled: MPEG-2 video (skipped when reading, refused when writing), more than one program, and
//! PMT changes after the first.

use crate::stream::{StreamDemuxer, StreamPacket};
use crate::{adts, WriteSeek};
use ironav_core::{CodecId, Demuxer, Error, ErrorCode, Muxer, Packet, PixelFormat, Rational, Result, StreamInfo, StreamParams};
use std::collections::VecDeque;
use std::io::{Read, Seek, SeekFrom};

const TS: usize = 188;
const CLOCK: i64 = 90_000;
const WRAP: i64 = 1 << 33;
/// Bytes held while waiting for every stream to show its parameters; past this the streams known
/// so far are used and the rest dropped.
const MAX_HELD: usize = 16 << 20;
/// Video units held to measure the frame rate before the streams are published.
const RATE_UNITS: usize = 4;
/// The longest PSI section (13818-1 caps PAT and PMT sections at 1021 bytes after the length).
const MAX_SECTION: usize = 1021;
/// The largest PES gathered; a bigger one is dropped as broken.
const MAX_PES: usize = 32 << 20;

/// The packet size (188 or 192) if `m` starts a transport stream: sync bytes where the next
/// packets begin, as far as `m` reaches (at least one more is needed).
pub fn packet_size(m: &[u8]) -> Option<usize> {
    [TS, TS + 4].into_iter().find(|&size| {
        let at = size - TS;
        let syncs: Vec<usize> = (0..3).map(|k| at + k * size).filter(|&i| i < m.len()).collect();
        syncs.len() >= 2 && syncs.iter().all(|&i| m[i] == 0x47)
    })
}

/// CRC-32/MPEG-2 over a PSI section.
fn crc32(b: &[u8]) -> u32 {
    let mut crc = 0xFFFF_FFFFu32;
    for &x in b {
        crc ^= (x as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 { (crc << 1) ^ 0x04C1_1DB7 } else { crc << 1 };
        }
    }
    crc
}

fn read_ts(b: &[u8]) -> i64 {
    (((b[0] as i64 >> 1) & 7) << 30) | ((b[1] as i64) << 22) | ((b[2] as i64 >> 1) << 15) | ((b[3] as i64) << 7) | (b[4] as i64 >> 1)
}

/// The PTS of a PES header at the start of `b`, if it has one.
fn pes_pts(b: &[u8]) -> Option<i64> {
    (b.len() >= 14 && b[..3] == [0, 0, 1] && b[7] & 0x80 != 0).then(|| read_ts(&b[9..14]))
}

fn codec_of(stream_type: u8) -> Option<CodecId> {
    match stream_type {
        0x1B => Some(CodecId::H264),
        0x24 => Some(CodecId::Hevc),
        0x01 => Some(CodecId::Mpeg1Video),
        0x02 => Some(CodecId::Mpeg2Video),
        0x0F => Some(CodecId::Aac),
        0x03 | 0x04 => Some(CodecId::Mp3),
        // AC-3 as the ATSC and Blu-ray tables give it (0x06 with a descriptor is the DVB way)
        0x81 => Some(CodecId::Ac3),
        _ => None,
    }
}

/// Opus is always 48 kHz in a transport stream, and its PES has stream_id private_stream_1.
const OPUS_RATE: i64 = 48_000;
const PRIVATE_STREAM_1: u8 = 0xBD;
/// The registration descriptor (tag 0x05) that marks an AC-3 stream of stream type 0x81.
const AC3_REGISTRATION: [u8; 6] = [0x05, 4, b'A', b'C', b'-', b'3'];
/// The number of streams and coupled streams, and the channel mapping table, of the channel
/// configuration codes 1 to 8 (indexed by the code), as the descriptor names them.
const OPUS_STREAMS: [u8; 9] = [1, 1, 1, 2, 2, 3, 4, 4, 5];
const OPUS_COUPLED: [u8; 9] = [1, 0, 1, 1, 2, 2, 2, 3, 3];
const OPUS_MAP: [&[u8]; 8] = [&[0], &[0, 1], &[0, 2, 1], &[0, 1, 2, 3], &[0, 4, 1, 2, 3], &[0, 4, 1, 2, 3, 5], &[0, 4, 1, 2, 3, 5, 6], &[0, 6, 1, 2, 3, 4, 5, 7]];

/// The OpusHead (with no pre-skip yet) that channel configuration `code` of the Opus descriptor
/// stands for: 1 and 2 are mono and stereo, 3 to 8 are that many channels in channel-mapping family
/// 1, and 0 is dual mono, which FFmpeg reads as one coupled stream in family 255.
fn opus_head_for(code: u8) -> Option<Vec<u8>> {
    let (channels, family) = match code {
        0 => (2, 255),
        1 | 2 => (code, 0),
        3..=8 => (code, 1),
        _ => return None,
    };
    let mut h = b"OpusHead".to_vec();
    h.push(1);
    h.push(channels);
    h.extend_from_slice(&[0, 0]);
    h.extend_from_slice(&(OPUS_RATE as u32).to_le_bytes());
    h.extend_from_slice(&[0, 0, family]);
    if family != 0 {
        let c = code as usize;
        let (streams, coupled, map) = if family == 255 { (1, 1, OPUS_MAP[1]) } else { (OPUS_STREAMS[c], OPUS_COUPLED[c], OPUS_MAP[c - 1]) };
        h.extend_from_slice(&[streams, coupled]);
        h.extend_from_slice(map);
    }
    Some(h)
}

/// The channel configuration code of an OpusHead that the descriptor can name (the inverse of
/// `opus_head_for`): mono and stereo in family 0, and family 1 with 3 to 8 channels in the RFC 7845
/// layout.
fn opus_code(head: &[u8]) -> Option<u8> {
    if head.len() < 19 || &head[..8] != b"OpusHead" {
        return None;
    }
    let ch = head[9] as usize;
    match head[18] {
        0 if (1..=2).contains(&ch) => Some(ch as u8),
        1 if (3..=8).contains(&ch) && head.len() >= 21 + ch && head[19] == OPUS_STREAMS[ch] && head[20] == OPUS_COUPLED[ch] && head[21..21 + ch] == *OPUS_MAP[ch - 1] => Some(ch as u8),
        _ => None,
    }
}

/// Whether a private (0x06) stream's PMT descriptors say it is AC-3: the DVB AC-3 descriptor (tag
/// 0x6A) or the `AC-3` registration descriptor (tag 0x05).
fn has_ac3_descriptor(desc: &[u8]) -> bool {
    let mut i = 0;
    while i + 2 <= desc.len() {
        let (tag, len) = (desc[i], desc[i + 1] as usize);
        let Some(body) = desc.get(i + 2..i + 2 + len) else { return false };
        if tag == 0x6A || (tag == 0x05 && body.starts_with(b"AC-3")) {
            return true;
        }
        i += 2 + len;
    }
    false
}

/// The OpusHead of an Opus stream from its PMT descriptors: the `Opus` registration descriptor
/// (tag 0x05) and the user-defined extension descriptor 0x7F whose first byte is 0x80, followed by
/// the channel configuration code. `None` when either is missing or the code is not one of 0 to 8.
fn opus_head_from_descriptors(desc: &[u8]) -> Option<Vec<u8>> {
    let (mut opus, mut code) = (false, None);
    let mut i = 0;
    while i + 2 <= desc.len() {
        let (tag, len) = (desc[i], desc[i + 1] as usize);
        let body = desc.get(i + 2..i + 2 + len)?;
        match tag {
            0x05 if body.starts_with(b"Opus") => opus = true,
            0x7F if body.len() >= 2 && body[0] == 0x80 => code = Some(body[1]),
            _ => {}
        }
        i += 2 + len;
    }
    if !opus {
        return None;
    }
    opus_head_for(code?)
}

/// The number of 48 kHz samples in an Opus packet, from its TOC byte and frame count.
fn opus_samples(packet: &[u8]) -> Option<i64> {
    ironav_codec::opus::packet_samples(packet).filter(|&n| n > 0).map(|n| n as i64)
}

fn be16(p: &[u8], i: usize) -> Option<usize> {
    Some(u16::from_be_bytes([*p.get(i)?, *p.get(i + 1)?]) as usize)
}

/// One packet of an Opus PES from the control header at the start of `p`: the length of the header
/// and packet together, the start and end trims, and the packet. `None` at a bad header.
fn opus_control(p: &[u8]) -> Option<(usize, usize, usize, &[u8])> {
    if be16(p, 0)? & 0xFFE0 != 0x7FE0 {
        return None;
    }
    let flags = p[1];
    let mut i = 2;
    let mut size = 0usize;
    loop {
        let b = *p.get(i)?;
        i += 1;
        size += b as usize;
        if b != 0xFF {
            break;
        }
    }
    let start = if flags & 0x10 != 0 {
        i += 2;
        be16(p, i - 2)?
    } else {
        0
    };
    let end = if flags & 0x08 != 0 {
        i += 2;
        be16(p, i - 2)?
    } else {
        0
    };
    if flags & 0x04 != 0 {
        i += 1 + *p.get(i)? as usize;
    }
    let data = p.get(i..i.checked_add(size)?)?;
    Some((i + size, start, end, data))
}

/// NAL units of an Annex B access unit, without their start codes.
pub(crate) fn nals(d: &[u8]) -> impl Iterator<Item = &[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= d.len() {
        if d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let ends: Vec<usize> = starts.iter().skip(1).map(|&s| s - 3).chain(std::iter::once(d.len())).collect();
    starts.into_iter().zip(ends).map(move |(s, e)| {
        // a four-byte start code leaves its leading zero on the unit before
        let mut e = e;
        while e > s && d[e - 1] == 0 {
            e -= 1;
        }
        &d[s..e]
    })
}

/// MPEG-1 and MPEG-2 video are not made of NAL units: their pictures are found by start codes.
fn is_mpeg_video(codec: CodecId) -> bool {
    matches!(codec, CodecId::Mpeg1Video | CodecId::Mpeg2Video)
}

/// The stream an MPEG-1 or MPEG-2 video access unit with a sequence header describes.
fn mpeg_video_info(codec: CodecId, au: &[u8]) -> Option<StreamInfo> {
    let s = ironav_codec::mpeg2::sequence_info(au)?;
    Some(StreamInfo {
        index: 0,
        codec,
        time_base: Rational::new(1, CLOCK),
        duration: None,
        bits: None,
        extradata: Vec::new(),
        lead_in: 0,
        params: StreamParams::Video {
            width: s.width,
            height: s.height,
            pix_fmt: if s.chroma_format == 2 { PixelFormat::Yuv422p } else { PixelFormat::Yuv420p },
            frame_rate: Rational::new(25, 1),
        },
    })
}

fn nal_type(codec: CodecId, nal: &[u8]) -> u8 {
    if codec == CodecId::Hevc { (nal[0] >> 1) & 63 } else { nal[0] & 31 }
}

fn is_keyframe(codec: CodecId, au: &[u8]) -> bool {
    nals(au).filter(|n| !n.is_empty()).any(|n| {
        let t = nal_type(codec, n);
        if codec == CodecId::Hevc { (16..=23).contains(&t) } else { t == 5 }
    })
}

/// A unit read from a PES: a video access unit or one audio frame, at a 90 kHz time.
struct Unit {
    pts: i64,
    data: Vec<u8>,
    key: bool,
    /// The samples the unit plays for, when it is not the codec's usual frame (an Opus packet
    /// with an end trim).
    dur: Option<i64>,
}

/// One elementary stream of the program.
struct Es {
    pid: u16,
    codec: CodecId,
    /// The PES being gathered, from its start code on, and whether a packet of it was lost.
    pes: Vec<u8>,
    broken: bool,
    cc: Option<u8>,
    random_access: bool,
    /// The last unwrapped timestamp, to unwrap the next one against.
    last: Option<i64>,
    /// Audio bytes left over from the previous PES (a frame split across two), and the clock of
    /// the next frame.
    carry: Vec<u8>,
    next: Option<i64>,
    /// What the 90 kHz clock is short of the true time, in 1/rate ticks, so it does not drift.
    rem: i64,
    info: Option<StreamInfo>,
    /// Index among the published streams; `None` until then, or for good if it never showed.
    out: Option<usize>,
    /// Video: no unit is passed on before a keyframe.
    started: bool,
    /// Video frame duration (90 kHz), measured from the held units.
    frame: i64,
    /// Opus: the OpusHead without its pre-skip, the pre-skip so far (the start trims of the leading
    /// packets), and whether those leading packets are still coming.
    opus_head: Vec<u8>,
    opus_pre: u32,
    opus_lead: bool,
}

#[derive(Default)]
pub(crate) struct TsParser {
    size: usize,
    buf: Vec<u8>,
    pmt_pid: Option<u16>,
    pat: Vec<u8>,
    pmt: Vec<u8>,
    es: Vec<Es>,
    pub(crate) streams: Vec<StreamInfo>,
    ready: bool,
    held: Vec<(usize, Unit)>,
    held_bytes: usize,
    base: i64,
    out: VecDeque<StreamPacket>,
    ended: bool,
    /// Units dropped because a packet of them was lost.
    pub(crate) losses: u64,
}

impl TsParser {
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Result<()> {
        self.buf.extend_from_slice(bytes);
        let mut at = 0;
        loop {
            if self.size == 0 {
                match packet_size(&self.buf[at..]) {
                    Some(s) => self.size = s,
                    None if self.buf.len() - at >= 3 * (TS + 4) => {
                        at += 1;
                        continue;
                    }
                    None => break,
                }
            }
            let (size, skip) = (self.size, self.size - TS);
            if self.buf.len() - at < size {
                break;
            }
            if self.buf[at + skip] != 0x47 {
                // lost sync: look for it again
                self.size = 0;
                at += 1;
                continue;
            }
            let p: [u8; TS] = self.buf[at + skip..at + size].try_into().unwrap();
            self.packet(&p)?;
            at += size;
        }
        self.buf.drain(..at);
        Ok(())
    }

    pub(crate) fn end(&mut self) {
        if self.ended {
            return;
        }
        self.ended = true;
        for i in 0..self.es.len() {
            // with `ended` set, an audio PES gives up its last frames instead of carrying them
            self.finish_pes(i);
        }
        for i in 0..self.es.len() {
            if self.es[i].opus_lead {
                self.opus_finish(i);
            }
        }
        self.publish();
    }

    pub(crate) fn next(&mut self) -> Option<StreamPacket> {
        self.out.pop_front()
    }

    /// Forget partial units, for a seek: what comes next is parsed afresh, video from a keyframe.
    fn reset(&mut self) {
        self.buf.clear();
        self.out.clear();
        self.size = 0;
        self.ended = false;
        for e in &mut self.es {
            e.pes.clear();
            e.carry.clear();
            e.cc = None;
            e.next = None;
            e.rem = 0;
            e.started = false;
        }
    }

    fn packet(&mut self, p: &[u8; TS]) -> Result<()> {
        if p[1] & 0x80 != 0 {
            return Ok(()); // transport error
        }
        let pusi = p[1] & 0x40 != 0;
        let pid = (((p[1] & 0x1F) as u16) << 8) | p[2] as u16;
        let (afc, cc) = ((p[3] >> 4) & 3, p[3] & 15);
        let mut i = 4;
        let mut rai = false;
        if afc & 2 != 0 {
            let len = p[4] as usize;
            rai = len > 0 && p[5] & 0x40 != 0;
            i = 5 + len;
        }
        if afc & 1 == 0 || i >= TS {
            return Ok(());
        }
        let payload = &p[i..];
        if pid == 0 {
            self.psi(true, pusi, payload);
        } else if Some(pid) == self.pmt_pid {
            self.psi(false, pusi, payload);
        } else if let Some(k) = self.es.iter().position(|e| e.pid == pid) {
            let e = &mut self.es[k];
            match e.cc {
                Some(last) if last == cc => return Ok(()), // a repeated packet
                Some(last) if (last + 1) & 15 != cc => e.broken = true,
                _ => {}
            }
            e.cc = Some(cc);
            if pusi {
                self.finish_pes(k);
                let e = &mut self.es[k];
                e.broken = false;
                e.random_access = rai;
                e.pes.extend_from_slice(payload);
            } else if !e.pes.is_empty() {
                if e.pes.len() + payload.len() > MAX_PES {
                    e.broken = true;
                } else {
                    e.pes.extend_from_slice(payload);
                }
            }
            // a PES of known length is done when its last byte arrives
            let e = &self.es[k];
            if e.pes.len() >= 6 {
                let len = u16::from_be_bytes([e.pes[4], e.pes[5]]) as usize;
                if len != 0 && e.pes.len() >= 6 + len {
                    self.finish_pes(k);
                }
            }
        }
        Ok(())
    }

    /// Gathers a PAT (`pat`) or PMT section and reads it once whole.
    fn psi(&mut self, pat: bool, pusi: bool, payload: &[u8]) {
        let sec = if pat { &mut self.pat } else { &mut self.pmt };
        if pusi {
            let ptr = payload[0] as usize;
            sec.clear();
            if let Some(s) = payload.get(1 + ptr..) {
                sec.extend_from_slice(s);
            }
        } else if !sec.is_empty() {
            sec.extend_from_slice(payload);
        }
        if sec.len() < 3 {
            return;
        }
        let len = (((sec[1] & 0x0F) as usize) << 8) | sec[2] as usize;
        if !(9..=MAX_SECTION).contains(&len) {
            sec.clear();
            return;
        }
        if sec.len() < 3 + len {
            return;
        }
        let s = std::mem::take(sec);
        let s = &s[..3 + len];
        if crc32(s) != 0 {
            return;
        }
        let body = &s[8..s.len() - 4];
        if pat && s[0] == 0x00 {
            if self.pmt_pid.is_none() {
                self.pmt_pid = body.as_chunks::<4>().0.iter().find(|e| u16::from_be_bytes([e[0], e[1]]) != 0).map(|e| (((e[2] & 0x1F) as u16) << 8) | e[3] as u16);
            }
        } else if !pat && s[0] == 0x02 && self.es.is_empty() && body.len() >= 4 {
            let info_len = (((body[2] & 0x0F) as usize) << 8) | body[3] as usize;
            let mut i = 4 + info_len;
            while i + 5 <= body.len() {
                let (ty, pid) = (body[i], (((body[i + 1] & 0x1F) as u16) << 8) | body[i + 2] as u16);
                let es_len = (((body[i + 3] & 0x0F) as usize) << 8) | body[i + 4] as usize;
                // private PES (0x06) carries Opus when its descriptors say so
                let opus = if ty == 0x06 { body.get(i + 5..i + 5 + es_len).and_then(opus_head_from_descriptors) } else { None };
                let ac3 = ty == 0x06 && body.get(i + 5..i + 5 + es_len).is_some_and(has_ac3_descriptor);
                if let Some(codec) = if opus.is_some() { Some(CodecId::Opus) } else if ac3 { Some(CodecId::Ac3) } else { codec_of(ty) } {
                    self.es.push(Es {
                        pid,
                        codec,
                        pes: Vec::new(),
                        broken: false,
                        cc: None,
                        random_access: false,
                        last: None,
                        carry: Vec::new(),
                        next: None,
                        rem: 0,
                        info: None,
                        out: None,
                        started: false,
                        frame: 0,
                        opus_lead: opus.is_some(),
                        opus_head: opus.unwrap_or_default(),
                        opus_pre: 0,
                    });
                }
                i += 5 + es_len;
            }
        }
    }

    fn unwrap(&mut self, k: usize, raw: i64) -> i64 {
        let e = &mut self.es[k];
        let t = match e.last {
            None => raw,
            Some(last) => {
                let d = (raw - last).rem_euclid(WRAP);
                last + if d >= WRAP / 2 { d - WRAP } else { d }
            }
        };
        e.last = Some(t);
        t
    }

    fn finish_pes(&mut self, k: usize) {
        let pes = std::mem::take(&mut self.es[k].pes);
        if self.es[k].broken {
            self.es[k].broken = false;
            if !pes.is_empty() {
                self.losses += 1;
                // the audio clock is lost with it
                self.es[k].carry.clear();
                self.es[k].next = None;
            }
            return;
        }
        if pes.len() < 9 || pes[..3] != [0, 0, 1] {
            return;
        }
        let hdr = 9 + pes[8] as usize;
        let declared = u16::from_be_bytes([pes[4], pes[5]]) as usize;
        let end = if declared == 0 { pes.len() } else { (6 + declared).min(pes.len()) };
        if hdr > end {
            return;
        }
        let pts = pes_pts(&pes).map(|raw| self.unwrap(k, raw));
        let payload = pes[hdr..end].to_vec();
        let codec = self.es[k].codec;
        if codec.media_type() == ironav_core::MediaType::Video {
            let Some(pts) = pts.or_else(|| self.es[k].next) else { return };
            let key = if is_mpeg_video(codec) { crate::keyframe::is_keyframe(codec, &payload) == Some(true) } else { is_keyframe(codec, &payload) } || self.es[k].random_access;
            if self.es[k].info.is_none() && is_mpeg_video(codec) {
                self.es[k].info = mpeg_video_info(codec, &payload);
            } else if self.es[k].info.is_none() {
                if let Some((w, h)) = nals(&payload).find(|n| !n.is_empty() && nal_type(codec, n) == if codec == CodecId::Hevc { 33 } else { 7 }).and_then(|n| ironav_codec::sps_picture_size(codec, n)) {
                    self.es[k].info = Some(StreamInfo {
                        index: 0,
                        codec,
                        time_base: Rational::new(1, CLOCK),
                        duration: None,
                        bits: None,
                        extradata: Vec::new(),
                        lead_in: 0,
                        params: StreamParams::Video { width: w, height: h, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(25, 1) },
                    });
                }
            }
            if !self.es[k].started {
                if !key || self.es[k].info.is_none() {
                    return;
                }
                self.es[k].started = true;
            }
            self.unit(k, Unit { pts, data: payload, key, dur: None });
        } else if codec == CodecId::Opus {
            self.opus_packets(k, &payload, pts);
        } else {
            let mut data = std::mem::take(&mut self.es[k].carry);
            let carried = data.len();
            data.extend_from_slice(&payload);
            self.audio_frames(k, data, pts.map(|p| (carried, p)), self.ended);
        }
    }

    /// Splits audio bytes into frames. `pts` is the time of the frame starting at that offset (the
    /// first byte of this PES); frames before it continue the running clock.
    fn audio_frames(&mut self, k: usize, data: Vec<u8>, pts: Option<(usize, i64)>, last: bool) {
        let codec = self.es[k].codec;
        let mut i = 0;
        // the PES's time belongs to the first frame that starts in it
        let mut synced = false;
        while i < data.len() {
            let (len, hdr, rate, channels, samples, asc, frame_codec) = if codec == CodecId::Aac {
                match adts::parse(&data[i..]) {
                    Some(h) => {
                        let asc = ((h.profile as u16 + 1) << 11) | ((h.sfi as u16) << 7) | ((h.channels as u16) << 3);
                        (h.frame, h.len, adts::RATES[h.sfi as usize], h.channels as u16, 1024, asc.to_be_bytes().to_vec(), CodecId::Aac)
                    }
                    None if data.len() - i < 9 && !last => break,
                    None => {
                        i += 1;
                        continue;
                    }
                }
            } else if codec == CodecId::Ac3 {
                match ironav_codec::ac3::frame_info(&data[i..]) {
                    Ok(f) if f.offset == 0 => (f.frame_size, 0, f.sample_rate, f.channels, 1536, Vec::new(), CodecId::Ac3),
                    _ if data.len() - i < 16 && !last => break,
                    _ => {
                        i += 1;
                        continue;
                    }
                }
            } else {
                // MPEG audio is labelled MP3, but a Layer II (MP2) frame is MP2: the frame says which
                match ironav_codec::mp3::parse_frame(&data[i..]) {
                    Some(h) => (h.frame_len, 0, h.sample_rate, h.channels(), h.samples(), Vec::new(), h.codec()),
                    None if data.len() - i < 4 && !last => break,
                    None => {
                        i += 1;
                        continue;
                    }
                }
            };
            if i + len > data.len() {
                // the rest of the frame is in the next PES (or, at the end, never comes)
                break;
            }
            if let Some((at, p)) = pts {
                if i >= at && !synced {
                    synced = true;
                    self.es[k].next = Some(p);
                    self.es[k].rem = 0;
                }
            }
            let Some(t) = self.es[k].next else {
                i += len;
                continue;
            };
            let e = &mut self.es[k];
            if e.info.is_none() {
                e.info = Some(StreamInfo {
                    index: 0,
                    codec: frame_codec,
                    time_base: Rational::new(1, rate as i64),
                    duration: None,
                    bits: None,
                    extradata: asc,
                    lead_in: 0,
                    params: StreamParams::Audio { sample_rate: rate, channels },
                });
            }
            let ticks = samples as i64 * CLOCK + e.rem;
            e.next = Some(t + ticks / rate as i64);
            e.rem = ticks % rate as i64;
            let frame = data[i + hdr..i + len].to_vec();
            self.unit(k, Unit { pts: t, data: frame, key: true, dur: None });
            i += len;
        }
        if !last {
            self.es[k].carry = data[i..].to_vec();
        }
    }

    /// Opus packets of a PES, each behind its control header. The first has the PES time; the
    /// others follow on from the packet before, as FFmpeg bundles several into one PES.
    fn opus_packets(&mut self, k: usize, payload: &[u8], pts: Option<i64>) {
        if let Some(p) = pts {
            self.es[k].next = Some(p);
            self.es[k].rem = 0;
        }
        let mut at = 0;
        while let Some((used, start, end, data)) = opus_control(&payload[at..]) {
            at += used;
            let Some(samples) = opus_samples(data) else { break };
            // the pre-skip is the start trims of the packets at the front; the first packet
            // without one ends it
            if self.es[k].opus_lead {
                if start == 0 {
                    self.opus_finish(k);
                } else {
                    self.es[k].opus_pre = self.es[k].opus_pre.saturating_add(start as u32);
                }
            }
            let Some(t) = self.es[k].next else { continue };
            let e = &mut self.es[k];
            let ticks = samples * CLOCK + e.rem;
            e.next = Some(t + ticks / OPUS_RATE);
            e.rem = ticks % OPUS_RATE;
            let dur = (samples - end as i64).max(0);
            self.unit(k, Unit { pts: t, data: data.to_vec(), key: true, dur: Some(dur) });
        }
    }

    /// Fixes an Opus stream's OpusHead, with the pre-skip its leading packets trimmed.
    fn opus_finish(&mut self, k: usize) {
        let e = &mut self.es[k];
        e.opus_lead = false;
        let mut head = e.opus_head.clone();
        head[10..12].copy_from_slice(&(e.opus_pre.min(u16::MAX as u32) as u16).to_le_bytes());
        let channels = head[9] as u16;
        e.info = Some(StreamInfo {
            index: 0,
            codec: CodecId::Opus,
            time_base: Rational::new(1, OPUS_RATE),
            duration: None,
            bits: None,
            extradata: head,
            lead_in: 0,
            params: StreamParams::Audio { sample_rate: OPUS_RATE as u32, channels },
        });
    }

    fn unit(&mut self, k: usize, u: Unit) {
        if self.ready {
            self.emit(k, u);
            return;
        }
        self.held_bytes += u.data.len();
        self.held.push((k, u));
        let known = self.es.iter().enumerate().all(|(i, e)| {
            e.info.is_some() && (e.codec.media_type() != ironav_core::MediaType::Video || self.held.iter().filter(|(j, _)| *j == i).count() >= RATE_UNITS)
        });
        if known || self.held_bytes > MAX_HELD {
            self.publish();
        }
    }

    /// Fixes the streams from what is held, then passes the held units on.
    fn publish(&mut self) {
        if self.ready {
            return;
        }
        let held = std::mem::take(&mut self.held);
        if held.is_empty() && !self.ended {
            return;
        }
        self.ready = true;
        for k in 0..self.es.len() {
            let Some(mut info) = self.es[k].info.clone() else { continue };
            let mut pts: Vec<i64> = held.iter().filter(|(j, _)| *j == k).map(|(_, u)| u.pts).collect();
            if pts.is_empty() {
                continue;
            }
            if let StreamParams::Video { frame_rate, .. } = &mut info.params {
                pts.sort_unstable();
                let d = pts.windows(2).map(|w| w[1] - w[0]).filter(|&d| d > 0).min().unwrap_or(CLOCK / 25);
                self.es[k].frame = d;
                *frame_rate = if d == 3003 { Rational::new(30000, 1001) } else if d == 1501 || d == 1502 { Rational::new(60000, 1001) } else if d == 3754 { Rational::new(24000, 1001) } else { reduce(CLOCK, d) };
            }
            info.index = self.streams.len();
            self.es[k].out = Some(info.index);
            self.streams.push(info);
        }
        self.base = held.iter().filter(|(k, _)| self.es[*k].out.is_some()).map(|(_, u)| u.pts).min().unwrap_or(0);
        for (k, u) in held {
            self.emit(k, u);
        }
    }

    fn emit(&mut self, k: usize, u: Unit) {
        let e = &self.es[k];
        let Some(index) = e.out else { return };
        let t = u.pts - self.base;
        let (pts, duration) = match e.info.as_ref().map(|i| &i.params) {
            Some(StreamParams::Audio { sample_rate, .. }) => {
                let r = *sample_rate as i64;
                let samples = u.dur.unwrap_or_else(|| if e.codec == CodecId::Aac { 1024 } else if e.codec == CodecId::Ac3 { 1536 } else { ironav_codec::mp3::parse_header(&u.data).map_or(1152, |h| h.samples() as i64) });
                ((t * r + CLOCK / 2).div_euclid(CLOCK), samples)
            }
            _ => (t, e.frame.max(1)),
        };
        // an MP3 frame keeps its header; an AAC one is raw
        self.out.push_back(StreamPacket { packet: Packet { stream: index, pts, duration, data: u.data }, key: u.key });
    }
}

fn reduce(n: i64, d: i64) -> Rational {
    let (mut a, mut b) = (n, d);
    while b != 0 {
        (a, b) = (b, a % b);
    }
    Rational::new(n / a, d / a)
}

/// The live reader.
#[derive(Default)]
pub struct TsStream {
    p: TsParser,
}

impl StreamDemuxer for TsStream {
    fn format_name(&self) -> &'static str {
        "mpegts"
    }
    fn streams(&self) -> &[StreamInfo] {
        &self.p.streams
    }
    fn feed(&mut self, bytes: &[u8]) -> Result<()> {
        self.p.feed(bytes)
    }
    fn next_packet(&mut self) -> Result<Option<StreamPacket>> {
        Ok(self.p.next())
    }
    fn end(&mut self) {
        self.p.end();
    }
}

/// The file reader.
pub struct TsDemuxer<R> {
    r: R,
    p: TsParser,
    len: u64,
    eof: bool,
}

const CHUNK: usize = 256 << 10;
/// How much of the end of a file is read for its duration.
const TAIL: u64 = 4 << 20;

impl<R: Read + Seek + Send> TsDemuxer<R> {
    pub fn open(mut r: R) -> Result<Self> {
        let len = r.seek(SeekFrom::End(0))?;
        r.seek(SeekFrom::Start(0))?;
        let mut d = TsDemuxer { r, p: TsParser::default(), len, eof: false };
        while !d.p.ready {
            if !d.fill()? {
                break;
            }
        }
        if d.p.streams.is_empty() {
            return Err(Error::new(ErrorCode::UnsupportedFormat, "the transport stream has no H.264, HEVC, AAC or MP3 stream that could be read"));
        }
        d.durations()?;
        Ok(d)
    }

    /// Reads another chunk into the parser; false at the end of the file.
    fn fill(&mut self) -> Result<bool> {
        if self.eof {
            return Ok(false);
        }
        let mut buf = vec![0u8; CHUNK];
        let n = self.r.read(&mut buf)?;
        if n == 0 {
            self.eof = true;
            self.p.end();
            return Ok(false);
        }
        self.p.feed(&buf[..n])?;
        Ok(true)
    }

    /// Each stream's duration, from the last presentation time near the end of the file.
    fn durations(&mut self) -> Result<()> {
        let pos = self.r.stream_position()?;
        self.r.seek(SeekFrom::Start(self.len.saturating_sub(TAIL)))?;
        let mut tail = Vec::new();
        self.r.by_ref().take(TAIL).read_to_end(&mut tail)?;
        self.r.seek(SeekFrom::Start(pos))?;
        let size = self.p.size.max(TS);
        let Some(start) = (0..tail.len().min(size * 2)).find(|&i| packet_size(&tail[i..]) == Some(size)) else { return Ok(()) };
        let mut last = vec![None::<i64>; self.p.es.len()];
        for p in tail[start..].chunks_exact(size) {
            let p = &p[size - TS..];
            if p[0] != 0x47 || p[1] & 0x40 == 0 {
                continue;
            }
            let pid = (((p[1] & 0x1F) as u16) << 8) | p[2] as u16;
            let Some(k) = self.p.es.iter().position(|e| e.pid == pid) else { continue };
            let mut i = 4;
            if p[3] & 0x20 != 0 {
                i = 5 + p[4] as usize;
            }
            if let Some(raw) = p.get(i..).and_then(pes_pts) {
                // relative to the stream's last unwrapped time, which is near the start
                let near = self.p.es[k].last.unwrap_or(raw);
                let d = (raw - near).rem_euclid(WRAP);
                let t = near + if d >= WRAP / 2 { d - WRAP } else { d };
                last[k] = Some(last[k].map_or(t, |l: i64| l.max(t)));
            }
        }
        for (k, e) in self.p.es.iter().enumerate() {
            let (Some(i), Some(t)) = (e.out, last[k]) else { continue };
            let s = &mut self.p.streams[i];
            let span = t - self.p.base;
            s.duration = Some(match s.params {
                // an Opus frame is 20 ms; the last one's length is not read from the tail
                StreamParams::Audio { sample_rate, .. } => span * sample_rate as i64 / CLOCK + match e.codec {
                    CodecId::Aac => 1024,
                    CodecId::Ac3 => 1536,
                    CodecId::Opus => 960,
                    _ => 1152,
                },
                _ => span + e.frame,
            });
        }
        Ok(())
    }

    fn seconds_of(&self, p: &Packet) -> f64 {
        p.pts as f64 * self.p.streams[p.stream].time_base.as_f64()
    }

    /// The first video keyframe (or, without video, the first packet) from byte `at` on.
    fn land(&mut self, at: u64) -> Result<Option<f64>> {
        self.r.seek(SeekFrom::Start(at))?;
        self.eof = false;
        self.p.reset();
        let video = self.p.streams.iter().position(|s| s.params.media_type() == ironav_core::MediaType::Video);
        loop {
            while let Some(sp) = self.p.out.front() {
                if video.is_none_or(|v| sp.packet.stream == v && sp.key) {
                    return Ok(Some(self.seconds_of(&sp.packet)));
                }
                self.p.out.pop_front();
            }
            if !self.fill()? {
                return Ok(None);
            }
        }
    }
}

impl<R: Read + Seek + Send> Demuxer for TsDemuxer<R> {
    fn format_name(&self) -> &'static str {
        "mpegts"
    }
    fn streams(&self) -> &[StreamInfo] {
        &self.p.streams
    }
    fn read_packet(&mut self) -> Result<Option<Packet>> {
        loop {
            if let Some(sp) = self.p.next() {
                return Ok(Some(sp.packet));
            }
            if !self.fill()? && self.p.out.is_empty() {
                return Ok(None);
            }
        }
    }

    fn seek(&mut self, seconds: f64) -> Result<f64> {
        let total = self.p.streams.iter().filter_map(|s| s.duration.map(|d| d as f64 * s.time_base.as_f64())).fold(0.0, f64::max);
        if seconds <= 0.0 || total <= 0.0 {
            return self.land(0).map(|t| t.unwrap_or(0.0));
        }
        let seconds = seconds.min(total);
        let size = self.p.size.max(TS) as u64;
        let rate = self.len as f64 / total;
        // aim a little early, then step back until the keyframe landed on is at or before the request
        let mut at = ((seconds - 1.0).max(0.0) * rate) as u64;
        for _ in 0..8 {
            at -= at % size;
            match self.land(at)? {
                Some(t) if t <= seconds + 1e-6 || at == 0 => return Ok(t),
                Some(t) => at = at.saturating_sub(((t - seconds + 1.0) * rate) as u64 + size),
                None => at = at.saturating_sub((2.0 * rate) as u64 + size),
            }
        }
        self.land(0).map(|t| t.unwrap_or(0.0))
    }
}

/// How many places video pictures may be moved between decode and presentation order (the most
/// H.264 and HEVC allow).
const REORDER: usize = 16;
/// Added to every time written, so decode times and the PCR before them stay positive.
const OFFSET: i64 = CLOCK * 14 / 10;
/// The PCR runs this far behind the decode times.
const PCR_LEAD: i64 = CLOCK * 7 / 10;

struct OutStream {
    pid: u16,
    codec: CodecId,
    tb: Rational,
    cc: u8,
    /// ADTS header bytes 1-3, for AAC.
    adts: Option<[u8; 3]>,
    /// Video presentation times so far, sorted, and how many pictures have been written.
    seen: Vec<i64>,
    count: usize,
    last_dts: i64,
    /// Opus: the channel configuration code, and the pre-skip still to trim from the first packets.
    opus: Option<(u8, u32)>,
}

pub struct TsMuxer {
    out: Box<dyn WriteSeek>,
    streams: Vec<OutStream>,
    pcr_pid: u16,
    pat_cc: u8,
    pmt_cc: u8,
    last_psi: Option<i64>,
}

const PMT_PID: u16 = 0x1000;

impl TsMuxer {
    pub fn new(out: Box<dyn WriteSeek>) -> Self {
        TsMuxer { out, streams: Vec::new(), pcr_pid: 0x100, pat_cc: 0, pmt_cc: 0, last_psi: None }
    }

    fn write_ts(&mut self, pid: u16, pusi: bool, cc: u8, adapt: Option<(Option<i64>, bool)>, payload: &[u8]) -> Result<usize> {
        let mut af = Vec::new();
        if let Some((pcr, rai)) = adapt {
            af.push(if pcr.is_some() { 0x10 } else { 0 } | if rai { 0x40 } else { 0 });
            if let Some(pcr) = pcr {
                let base = pcr.rem_euclid(WRAP) as u64;
                af.extend_from_slice(&[(base >> 25) as u8, (base >> 17) as u8, (base >> 9) as u8, (base >> 1) as u8, ((base & 1) << 7) as u8 | 0x7E, 0]);
            }
        }
        let room = TS - 4 - if adapt.is_some() { 1 + af.len() } else { 0 };
        let take = payload.len().min(room);
        let stuffing = room - take;
        if stuffing > 0 {
            if adapt.is_some() {
                af.extend(std::iter::repeat_n(0xFF, stuffing));
            } else if stuffing == 1 {
                // an adaptation field of length 0 is the one byte
            } else {
                af.push(0);
                af.extend(std::iter::repeat_n(0xFF, stuffing - 2));
            }
        }
        let has_af = adapt.is_some() || stuffing > 0;
        let mut p = Vec::with_capacity(TS);
        p.extend_from_slice(&[0x47, (if pusi { 0x40 } else { 0 }) | (pid >> 8) as u8, pid as u8, (if has_af { 0x30 } else { 0x10 }) | cc]);
        if has_af {
            p.push(af.len() as u8);
            p.extend_from_slice(&af);
        }
        p.extend_from_slice(&payload[..take]);
        debug_assert_eq!(p.len(), TS);
        self.out.write_all(&p)?;
        Ok(take)
    }

    fn section(&mut self, pid: u16, table: u8, id: u16, body: &[u8]) -> Result<()> {
        let mut s = vec![table, 0xB0 | ((body.len() + 9) >> 8) as u8, (body.len() + 9) as u8, (id >> 8) as u8, id as u8, 0xC1, 0, 0];
        s.extend_from_slice(body);
        let crc = crc32(&s);
        s.extend_from_slice(&crc.to_be_bytes());
        if s.len() > TS - 5 {
            return Err(Error::new(ErrorCode::UnsupportedFormat, "too many streams for one transport stream packet's PMT"));
        }
        let mut payload = vec![0u8];
        payload.extend_from_slice(&s);
        payload.resize(TS - 4, 0xFF);
        let cc = if pid == 0 { &mut self.pat_cc } else { &mut self.pmt_cc };
        let c = *cc;
        *cc = (c + 1) & 15;
        self.write_ts(pid, true, c, None, &payload)?;
        Ok(())
    }

    fn psi(&mut self) -> Result<()> {
        self.section(0, 0x00, 1, &[0, 1, 0xE0 | (PMT_PID >> 8) as u8, PMT_PID as u8])?;
        let mut body = vec![0xE0 | (self.pcr_pid >> 8) as u8, self.pcr_pid as u8, 0xF0, 0];
        for s in &self.streams {
            let ty = match s.codec {
                CodecId::H264 => 0x1B,
                CodecId::Hevc => 0x24,
                CodecId::Aac => 0x0F,
                CodecId::Opus => 0x06,
                CodecId::Ac3 => 0x81,
                _ => 0x03,
            };
            // Opus: the registration descriptor 'Opus' and the extension descriptor naming the configuration;
            // AC-3: the registration descriptor 'AC-3'
            let desc = if s.codec == CodecId::Ac3 {
                AC3_REGISTRATION.to_vec()
            } else {
                s.opus.map(|(code, _)| vec![0x05, 4, b'O', b'p', b'u', b's', 0x7F, 2, 0x80, code]).unwrap_or_default()
            };
            body.extend_from_slice(&[ty, 0xE0 | (s.pid >> 8) as u8, s.pid as u8, 0xF0 | (desc.len() >> 8) as u8, desc.len() as u8]);
            body.extend_from_slice(&desc);
        }
        self.section(PMT_PID, 0x02, 1, &body)
    }
}

fn write_ts_time(out: &mut Vec<u8>, marker: u8, t: i64) {
    let t = t.rem_euclid(WRAP);
    out.extend_from_slice(&[(marker << 4) | (((t >> 30) & 7) << 1) as u8 | 1, (t >> 22) as u8, (((t >> 15) & 0x7F) << 1) as u8 | 1, (t >> 7) as u8, ((t & 0x7F) << 1) as u8 | 1]);
}

impl Muxer for TsMuxer {
    fn write_header(&mut self, streams: &[StreamInfo]) -> Result<()> {
        if streams.is_empty() {
            return Err(Error::invalid_arg("a transport stream needs at least one stream"));
        }
        let video = streams.iter().position(|s| s.params.media_type() == ironav_core::MediaType::Video);
        for (i, s) in streams.iter().enumerate() {
            let (adts, opus) = match s.codec {
                CodecId::H264 | CodecId::Hevc | CodecId::Mp3 | CodecId::Mp2 | CodecId::Ac3 => (None, None),
                CodecId::Aac => {
                    let [a, b, ..] = s.extradata[..] else {
                        return Err(Error::new(ErrorCode::UnsupportedFormat, "AAC in a transport stream needs the AudioSpecificConfig").hint("encode to aac, or copy from a file that carries it"));
                    };
                    let asc = u16::from_be_bytes([a, b]);
                    let (aot, sfi, ch) = (asc >> 11, (asc >> 7) & 15, (asc >> 3) & 15);
                    if !(1..=4).contains(&aot) || sfi >= 13 || !(1..=7).contains(&ch) {
                        return Err(Error::new(ErrorCode::UnsupportedFormat, "ADTS cannot describe this AudioSpecificConfig").hint("write it to mp4 or mkv"));
                    }
                    (Some([0xF1, (((aot - 1) << 6) | (sfi << 2) | (ch >> 2)) as u8, ((ch & 3) << 6) as u8]), None)
                }
                CodecId::Opus => {
                    let Some(code) = opus_code(&s.extradata) else {
                        return Err(Error::new(ErrorCode::UnsupportedFormat, "a transport stream holds Opus in mono, stereo, or 3 to 8 channels in channel-mapping family 1").hint("encode to opus with 1 to 8 channels, or write it to ogg or matroska"));
                    };
                    (None, Some((code, u16::from_le_bytes([s.extradata[10], s.extradata[11]]) as u32)))
                }
                c => {
                    return Err(Error::new(ErrorCode::UnsupportedCodec, format!("a transport stream cannot hold {} here", c.name())).alternatives(["h264", "hevc", "aac", "mp3", "opus", "mp2", "ac3"]));
                }
            };
            // video first, so it gets 0x100 and carries the PCR
            let order = match video {
                Some(v) if v == i => 0,
                Some(v) if i < v => i + 1,
                _ => i,
            };
            self.streams.push(OutStream { pid: 0x100 + order as u16, codec: s.codec, tb: s.time_base, cc: 0, adts, seen: Vec::new(), count: 0, last_dts: i64::MIN, opus });
        }
        self.pcr_pid = self.streams[video.unwrap_or(0)].pid;
        Ok(())
    }

    fn write_packet(&mut self, p: &Packet) -> Result<()> {
        let Some(s) = self.streams.get_mut(p.stream) else {
            return Err(Error::new(ErrorCode::Internal, "a packet for a stream the header did not have"));
        };
        let pts = Rational::rescale(p.pts, s.tb, Rational::new(1, CLOCK)) + OFFSET;
        let video = matches!(s.codec, CodecId::H264 | CodecId::Hevc);
        let (dts, key) = if video {
            let at = s.seen.partition_point(|&x| x < pts);
            s.seen.insert(at, pts);
            let frame = Rational::rescale(p.duration.max(1), s.tb, Rational::new(1, CLOCK));
            // the first pictures lead by up to REORDER frames, but never so far that the PCR goes below zero
            let dts = if s.count >= REORDER { s.seen[s.count - REORDER] } else { s.seen[0] - ((REORDER - s.count) as i64 * frame).min(OFFSET - PCR_LEAD) };
            s.count += 1;
            // keep the window small: what is REORDER places behind is final
            if s.seen.len() > 64 {
                let drop = s.seen.len() - 64;
                s.seen.drain(..drop);
                s.count -= drop;
            }
            // never after its presentation time, even if the stream reorders more than allowed
            let dts = dts.max(s.last_dts.saturating_add(1)).min(pts);
            s.last_dts = dts;
            (dts, is_keyframe(s.codec, &p.data))
        } else {
            (pts, true)
        };
        let (pid, codec, adts) = (s.pid, s.codec, s.adts);
        if (video && key) || self.last_psi.is_none_or(|t| dts - t >= CLOCK / 2) {
            self.psi()?;
            self.last_psi = Some(dts);
        }
        let mut es = Vec::with_capacity(p.data.len() + 16);
        match codec {
            CodecId::H264 | CodecId::Hevc => {
                let has_aud = nals(&p.data).next().is_some_and(|n| !n.is_empty() && nal_type(codec, n) == if codec == CodecId::Hevc { 35 } else { 9 });
                if !has_aud {
                    es.extend_from_slice(if codec == CodecId::Hevc { &[0, 0, 0, 1, 0x46, 0x01, 0x50][..] } else { &[0, 0, 0, 1, 0x09, 0xF0][..] });
                }
                es.extend_from_slice(&p.data);
            }
            CodecId::Aac => {
                let f = adts.unwrap();
                let len = p.data.len() + 7;
                if len >= 1 << 13 {
                    return Err(Error::invalid_input("an AAC frame is too large for an ADTS header"));
                }
                es.extend_from_slice(&[0xFF, f[0], f[1], f[2] | (len >> 11) as u8, (len >> 3) as u8, (((len & 7) << 5) | 0x1F) as u8, 0xFC]);
                es.extend_from_slice(&p.data);
            }
            CodecId::Opus => {
                let Some(samples) = opus_samples(&p.data) else {
                    return Err(Error::invalid_input("an Opus packet that its TOC byte does not time"));
                };
                let s = &mut self.streams[p.stream];
                // the pre-skip is trimmed from the first packets, as FFmpeg writes it
                let mut start = 0;
                if let Some((_, pending)) = s.opus.as_mut() {
                    if *pending > 0 {
                        start = (*pending as i64).min(samples);
                        *pending -= start as u32;
                    }
                }
                // what the packet's duration leaves out at its end
                let end = (samples - p.duration).max(0);
                let flags = (if start > 0 { 0x10 } else { 0 }) | (if end > 0 { 0x08 } else { 0 });
                es.extend_from_slice(&[0x7F, 0xE0 | flags]);
                let mut n = p.data.len();
                while n >= 255 {
                    es.push(255);
                    n -= 255;
                }
                es.push(n as u8);
                if start > 0 {
                    es.extend_from_slice(&(start as u16).to_be_bytes());
                }
                if end > 0 {
                    es.extend_from_slice(&(end as u16).to_be_bytes());
                }
                es.extend_from_slice(&p.data);
            }
            _ => es.extend_from_slice(&p.data),
        }
        let stream_id = if video {
            0xE0
        } else if matches!(codec, CodecId::Opus | CodecId::Ac3) {
            PRIVATE_STREAM_1
        } else {
            0xC0
        };
        let mut pes = vec![0, 0, 1, stream_id, 0, 0, 0x84];
        if dts != pts {
            pes.extend_from_slice(&[0xC0, 10]);
            write_ts_time(&mut pes, 3, pts);
            write_ts_time(&mut pes, 1, dts);
        } else {
            pes.extend_from_slice(&[0x80, 5]);
            write_ts_time(&mut pes, 2, pts);
        }
        let len = pes.len() - 6 + es.len();
        if len <= 0xFFFF {
            pes[4..6].copy_from_slice(&(len as u16).to_be_bytes());
        } else if !video {
            return Err(Error::invalid_input("an audio frame is too large for a PES packet"));
        }
        pes.extend_from_slice(&es);
        let pcr = (pid == self.pcr_pid).then_some(dts - PCR_LEAD);
        let mut at = 0;
        let mut first = true;
        while at < pes.len() {
            let s = &mut self.streams[p.stream];
            let cc = s.cc;
            s.cc = (cc + 1) & 15;
            let adapt = if first && (pcr.is_some() || key && video) { Some((pcr, key && video)) } else { None };
            at += self.write_ts(pid, first, cc, adapt, &pes[at..])?;
            first = false;
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        self.out.flush()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/").to_string() + name).unwrap()
    }

    fn read_all(d: &mut dyn Demuxer) -> Vec<Packet> {
        std::iter::from_fn(|| d.read_packet().unwrap()).collect()
    }

    #[test]
    fn crc_matches_the_mpeg2_check_value() {
        assert_eq!(crc32(b"123456789"), 0x0376_E6E7);
    }

    #[test]
    fn h264_and_aac_are_read_with_their_parameters_and_times() {
        // written by FFmpeg (libx264 with B-frames, 160x96 at 25 fps; AAC 48 kHz stereo; 2 s)
        let mut d = TsDemuxer::open(std::io::Cursor::new(fixture("h264_aac.ts"))).unwrap();
        let s = d.streams().to_vec();
        assert_eq!(s.len(), 2);
        assert!(matches!(s[0].params, StreamParams::Video { width: 160, height: 96, frame_rate: Rational { num: 25, den: 1 }, .. }), "{:?}", s[0]);
        assert_eq!((s[0].codec, s[1].codec), (CodecId::H264, CodecId::Aac));
        assert!(matches!(s[1].params, StreamParams::Audio { sample_rate: 48000, channels: 2 }));
        assert_eq!(s[1].extradata, [0x11, 0x90]);
        let secs = |i: usize| s[i].duration.unwrap() as f64 * s[i].time_base.as_f64();
        assert!((secs(0) - 2.0).abs() < 0.1 && (secs(1) - 2.0).abs() < 0.1, "{} {}", secs(0), secs(1));
        let p = read_all(&mut d);
        let video: Vec<&Packet> = p.iter().filter(|p| p.stream == 0).collect();
        assert_eq!(video.len(), 50);
        assert!(is_keyframe(CodecId::H264, &video[0].data));
        // B-frames: presentation order differs from decode order, and every time is distinct
        assert!(video.windows(2).any(|w| w[1].pts < w[0].pts));
        let mut t: Vec<i64> = video.iter().map(|p| p.pts).collect();
        t.sort_unstable();
        assert!(t.windows(2).all(|w| w[1] - w[0] == 3600), "{t:?}");
        let audio: Vec<&Packet> = p.iter().filter(|p| p.stream == 1).collect();
        assert!(audio.windows(2).all(|w| w[1].pts - w[0].pts == 1024));
        assert!(audio.len() >= 93, "{}", audio.len());
    }

    #[test]
    fn hevc_and_mp3_are_read() {
        let mut d = TsDemuxer::open(std::io::Cursor::new(fixture("hevc_mp3.ts"))).unwrap();
        let s = d.streams().to_vec();
        assert_eq!((s[0].codec, s[1].codec), (CodecId::Hevc, CodecId::Mp3));
        assert!(matches!(s[1].params, StreamParams::Audio { sample_rate: 48000, channels: 2 }));
        let p = read_all(&mut d);
        assert_eq!(p.iter().filter(|p| p.stream == 0).count(), 50);
        assert!(p.iter().filter(|p| p.stream == 1).all(|p| ironav_codec::mp3::parse_header(&p.data).is_some()));
    }

    #[test]
    fn the_live_reader_gives_the_file_readers_packets_whatever_the_pieces() {
        let data = fixture("h264_aac.ts");
        let want: Vec<(usize, i64, Vec<u8>)> = read_all(&mut TsDemuxer::open(std::io::Cursor::new(data.clone())).unwrap()).into_iter().map(|p| (p.stream, p.pts, p.data)).collect();
        for piece in [1, 97, 188, 4096] {
            let mut s = TsStream::default();
            let mut got = Vec::new();
            for c in data.chunks(piece) {
                s.feed(c).unwrap();
                while let Some(p) = s.next_packet().unwrap() {
                    got.push((p.packet.stream, p.packet.pts, p.packet.data));
                }
            }
            s.end();
            while let Some(p) = s.next_packet().unwrap() {
                got.push((p.packet.stream, p.packet.pts, p.packet.data));
            }
            assert_eq!(got.len(), want.len(), "pieces of {piece}");
            assert!(got == want, "pieces of {piece}");
        }
    }

    #[test]
    fn a_lost_packet_drops_only_the_unit_it_breaks() {
        let mut data = fixture("h264_aac.ts");
        let n = data.len() / TS;
        let total = read_all(&mut TsDemuxer::open(std::io::Cursor::new(data.clone())).unwrap()).len();
        // cut one packet out of the middle of an audio PES (PID 0x101, no unit start)
        let k = (n / 2..n).find(|&k| data[k * TS + 1] == 0x01 && data[k * TS + 2] == 0x01).unwrap();
        data.drain(k * TS..(k + 1) * TS);
        let mut d = TsDemuxer::open(std::io::Cursor::new(data)).unwrap();
        let got = read_all(&mut d).len();
        // the PES it was in, a few frames, and nothing else
        assert!(got < total && got >= total - 8, "{got} of {total}");
        assert_eq!(d.p.losses, 1);
    }

    #[test]
    fn written_streams_read_back_the_same() {
        let src = fixture("h264_aac.ts");
        let mut d = TsDemuxer::open(std::io::Cursor::new(src)).unwrap();
        let streams = d.streams().to_vec();
        let packets = read_all(&mut d);
        let path = std::env::temp_dir().join(format!("ironav-ts-{}.ts", std::process::id()));
        let mut m = TsMuxer::new(Box::new(std::io::BufWriter::new(std::fs::File::create(&path).unwrap())));
        m.write_header(&streams).unwrap();
        for p in &packets {
            m.write_packet(p).unwrap();
        }
        m.finish().unwrap();
        drop(m);
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        assert_eq!(bytes.len() % TS, 0);
        let mut back = TsDemuxer::open(std::io::Cursor::new(bytes)).unwrap();
        assert_eq!(back.streams().len(), 2);
        assert_eq!(back.streams()[1].extradata, streams[1].extradata);
        let again = read_all(&mut back);
        assert_eq!(again.len(), packets.len());
        // the same units at the same times; video gains an access unit delimiter in front
        let strip = |p: &Packet| if p.stream == 0 { p.data.strip_prefix(&[0, 0, 0, 1, 0x09, 0xF0][..]).unwrap_or(&p.data).to_vec() } else { p.data.clone() };
        let mut a: Vec<_> = packets.iter().map(|p| (p.stream, p.pts, strip(p))).collect();
        let mut b: Vec<_> = again.iter().map(|p| (p.stream, p.pts, strip(p))).collect();
        a.sort();
        b.sort();
        assert!(a == b);
    }

    #[test]
    fn damaged_input_never_panics() {
        let good = fixture("h264_aac.ts");
        let mut seed = 0x2545_F491_4F6C_DD1Du64;
        let mut rand = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for round in 0..200 {
            let mut d = good.clone();
            for _ in 0..1 + round % 40 {
                let i = rand() as usize % d.len();
                d[i] = rand() as u8;
            }
            // and sometimes a short PMT, a long section length, or a cut
            if round % 7 == 0 {
                d.truncate(rand() as usize % d.len());
            }
            let mut s = TsStream::default();
            for c in d.chunks(1 + rand() as usize % 3000) {
                s.feed(c).unwrap();
                while s.next_packet().unwrap().is_some() {}
            }
            s.end();
            while s.next_packet().unwrap().is_some() {}
            let _ = TsDemuxer::open(std::io::Cursor::new(d)).map(|mut d| {
                let _ = d.seek(1.0);
                read_all(&mut d)
            });
        }
        // a PMT whose section is too short to hold its fixed fields, with a good CRC
        let mut pmt = vec![0x02, 0xB0, 9, 0, 1, 0xC1, 0, 0];
        let crc = crc32(&pmt);
        pmt.extend_from_slice(&crc.to_be_bytes());
        let mut p = TsParser { pmt_pid: Some(0x1000), ..Default::default() };
        p.psi(false, true, &[&[0u8][..], &pmt].concat());
        assert!(p.es.is_empty());
    }

    #[test]
    fn decode_times_never_follow_presentation_times() {
        let info = StreamInfo {
            index: 0,
            codec: CodecId::H264,
            time_base: Rational::new(1, 25),
            duration: None,
            bits: None,
            extradata: Vec::new(),
            lead_in: 0,
            params: StreamParams::Video { width: 16, height: 16, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(25, 1) },
        };
        let path = std::env::temp_dir().join(format!("ironav-ts-dts-{}.ts", std::process::id()));
        let mut m = TsMuxer::new(Box::new(std::io::BufWriter::new(std::fs::File::create(&path).unwrap())));
        m.write_header(&[info]).unwrap();
        // reordered far more than any real stream: one picture shown 30 places early
        let order = [0i64, 31, 32, 33, 34, 35, 1, 2, 36, 37];
        for &t in &order {
            m.write_packet(&Packet { stream: 0, pts: t, duration: 1, data: vec![0, 0, 0, 1, 0x65, 0x88] }).unwrap();
        }
        m.finish().unwrap();
        drop(m);
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        for p in bytes.chunks(TS) {
            let pid = (((p[1] & 0x1F) as u16) << 8) | p[2] as u16;
            if pid != 0x100 || p[1] & 0x40 == 0 {
                continue;
            }
            let at = 4 + if p[3] & 0x20 != 0 { 1 + p[4] as usize } else { 0 };
            let h = &p[at..];
            let pts = read_ts(&h[9..14]);
            let dts = if h[7] & 0x40 != 0 { read_ts(&h[14..19]) } else { pts };
            assert!(dts <= pts, "dts {dts} > pts {pts}");
        }
    }

    #[test]
    fn seeking_lands_on_a_keyframe_at_or_before_the_time() {
        let mut d = TsDemuxer::open(std::io::Cursor::new(fixture("h264_aac.ts"))).unwrap();
        let t = d.seek(1.3).unwrap();
        assert!(t <= 1.3 + 1e-6 && t > 0.3, "{t}");
        let p = d.read_packet().unwrap().unwrap();
        assert_eq!(p.stream, 0);
        assert!(is_keyframe(CodecId::H264, &p.data));
        // the audio starts first; the first picture is shown 21 ms in
        assert!(d.seek(0.0).unwrap() < 0.05);
    }

    #[test]
    fn opus_channel_configurations_name_their_heads() {
        for code in 1..=8u8 {
            let head = opus_head_for(code).unwrap();
            assert_eq!(opus_code(&head), Some(code), "code {code}");
        }
        // dual mono is read, but not written: it has no layout the descriptor could name back
        assert_eq!(opus_code(&opus_head_for(0).unwrap()), None);
        assert!(opus_head_for(9).is_none());
        let desc = [0x05, 4, b'O', b'p', b'u', b's', 0x7F, 2, 0x80, 6];
        let head = opus_head_from_descriptors(&desc).unwrap();
        assert_eq!((head[9], head[18], head[19], head[20]), (6, 1, 4, 2));
        assert_eq!(&head[21..], &[0, 4, 1, 2, 3, 5]);
        // without the registration, or without the configuration, the stream is not Opus
        assert!(opus_head_from_descriptors(&desc[6..]).is_none());
        assert!(opus_head_from_descriptors(&desc[..6]).is_none());
    }

    /// An Opus packet of one 20 ms CELT frame (TOC 0xF8) with `n` bytes after the TOC.
    fn opus_packet(n: usize, seed: u8) -> Vec<u8> {
        let mut p = vec![0xF8];
        p.extend((0..n).map(|i| (i as u8).wrapping_mul(7).wrapping_add(seed)));
        p
    }

    #[test]
    fn opus_round_trips_with_its_pre_skip_and_end_trims() {
        let mut head = opus_head_for(2).unwrap();
        head[10..12].copy_from_slice(&312u16.to_le_bytes());
        let info = StreamInfo {
            index: 0,
            codec: CodecId::Opus,
            time_base: Rational::new(1, OPUS_RATE),
            duration: None,
            bits: None,
            extradata: head.clone(),
            lead_in: 0,
            params: StreamParams::Audio { sample_rate: 48_000, channels: 2 },
        };
        // the sizes cross the 255 boundary (a size is a run of 0xFF and a last byte) and one packet is shortened at its end
        let packets = [(0, 960, opus_packet(300, 1)), (960, 960, opus_packet(40, 2)), (1920, 860, opus_packet(1000, 3)), (2880, 960, opus_packet(255, 4))];
        let path = std::env::temp_dir().join(format!("ironav-ts-opus-{}.ts", std::process::id()));
        let mut m = TsMuxer::new(Box::new(std::io::BufWriter::new(std::fs::File::create(&path).unwrap())));
        m.write_header(&[info]).unwrap();
        for (pts, duration, data) in &packets {
            m.write_packet(&Packet { stream: 0, pts: *pts, duration: *duration, data: data.clone() }).unwrap();
        }
        m.finish().unwrap();
        drop(m);
        let bytes = std::fs::read(&path).unwrap();
        let _ = std::fs::remove_file(&path);
        let mut d = TsDemuxer::open(std::io::Cursor::new(bytes)).unwrap();
        let s = d.streams().to_vec();
        assert_eq!((s.len(), s[0].codec), (1, CodecId::Opus));
        assert_eq!(s[0].extradata, head, "the pre-skip is read back from the start trims");
        let got: Vec<(i64, i64, Vec<u8>)> = read_all(&mut d).into_iter().map(|p| (p.pts, p.duration, p.data)).collect();
        let want: Vec<(i64, i64, Vec<u8>)> = packets.iter().map(|(t, dur, data)| (*t, *dur, data.clone())).collect();
        assert_eq!(got, want);
    }
}
