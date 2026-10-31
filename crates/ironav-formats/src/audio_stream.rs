//! Audio formats read as they arrive: Ogg (Opus, Vorbis, FLAC), MP3, and whatever follows an ID3v2
//! tag (MP3 or ADTS). Internet radio sends these: Icecast and SHOUTcast streams are Ogg pages or
//! MP3 frames over HTTP, often chained or tagged.
//!
//! Ogg pages are unwrapped as they come, checksum-checked (a bad page is stepped over, not fatal),
//! and only the first Opus, Vorbis or FLAC stream is read. A chained stream (a new logical stream
//! after the end of the last, as radio does between songs) of the same codec continues it, and its
//! packets carry on the timeline. Its headers are not passed on, except a Vorbis link's that differ
//! from the ones in use (other codebooks, rate or channels): those go in front of its audio, the
//! decoder starts over with them, and its packets are timed in the stream's time base. Timing comes from the
//! packets, as in the file reader, so the packets are the same; the file reader's end trim needs
//! the last page and does not apply to a stream.
//!
//! MP3 frames are found the way the ADTS reader finds its frames: the first must be followed by an
//! agreeing header, after which a header at the same rate is trusted. A leading Xing/Info frame is
//! skipped, and ID3v2 tags between frames are stepped over. The LAME tag's gapless trim is a
//! file's affair and is not used.

use crate::stream::{Pending, StreamDemuxer, StreamPacket, MAX_PENDING};
use crate::{adts, ogg};
use ironav_codec::mp3::{parse_header, Header};
use ironav_core::{CodecId, Error, ErrorCode, Packet, Rational, Result, StreamInfo, StreamParams};
use std::collections::VecDeque;

/// The length of an ID3v2 tag at the start of `d`, when `d` starts with one and holds its header.
/// A tag too big to hold is refused at once, not after its bytes have piled up.
fn id3_len(d: &[u8]) -> Result<Option<usize>> {
    if !d.starts_with(b"ID3") || d.len() < 10 {
        return Ok(None);
    }
    let size = d[6..10].iter().fold(0usize, |a, &b| (a << 7) | (b & 0x7F) as usize);
    let len = 10 + size + if d[5] & 0x10 != 0 { 10 } else { 0 };
    if len > MAX_PENDING {
        return Err(Error::invalid_input(format!("an ID3v2 tag of {} MiB is more than can be held ({} MiB)", len >> 20, MAX_PENDING >> 20)));
    }
    Ok(Some(len))
}

// ---- Ogg --------------------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Opus,
    Vorbis,
    Flac,
}

impl Kind {
    fn of(first: &[u8]) -> Option<Kind> {
        if first.starts_with(b"OpusHead") {
            Some(Kind::Opus)
        } else if first.starts_with(b"\x01vorbis") {
            Some(Kind::Vorbis)
        } else if first.starts_with(b"\x7fFLAC") {
            Some(Kind::Flac)
        } else {
            None
        }
    }
}

/// The largest Ogg packet held while it is assembled across pages, and the most header bytes kept.
const MAX_OGG_PACKET: usize = 16 << 20;
const MAX_OGG_HEADERS: usize = 16 << 20;

#[derive(Default)]
pub struct OggStream {
    pending: Pending,
    streams: Vec<StreamInfo>,
    kind: Option<Kind>,
    /// The logical stream read, and whether its last page has passed.
    serial: Option<u32>,
    stream_ended: bool,
    /// Header packets of the current logical stream, and packets of it seen.
    headers: Vec<Vec<u8>>,
    seen: usize,
    /// The packet being assembled, and whether its start was missed (so it is dropped).
    packet: Vec<u8>,
    broken: bool,
    timer: Option<ironav_codec::vorbis::PacketTimer>,
    max_block: u32,
    out: VecDeque<StreamPacket>,
    pts: i64,
    /// Vorbis: the headers the decoder has now, and the current link's rate, where its first
    /// packet falls, and the samples since, so a link at another rate keeps the stream's clock.
    in_use: Vec<u8>,
    link_rate: u32,
    link_start: i64,
    link_samples: i64,
    unsupported: Option<&'static str>,
    ended: bool,
}

impl OggStream {
    /// One page: `None` until it has all arrived. Bytes before a page, and pages that fail their
    /// checksum, are stepped over.
    fn page(&mut self) -> Option<(u8, u32, Vec<u8>, Vec<u8>)> {
        loop {
            let d = self.pending.data();
            match d.windows(4).position(|w| w == b"OggS") {
                Some(0) => {}
                Some(k) => {
                    self.pending.skip(k);
                    continue;
                }
                None => {
                    // keep a possible start of "OggS"
                    self.pending.skip(d.len().saturating_sub(3));
                    return None;
                }
            }
            if d.len() < 27 {
                return None;
            }
            let nseg = d[26] as usize;
            if d.len() < 27 + nseg {
                return None;
            }
            let body: usize = d[27..27 + nseg].iter().map(|&s| s as usize).sum();
            let len = 27 + nseg + body;
            if d.len() < len {
                return None;
            }
            let mut page = d[..len].to_vec();
            let want = u32::from_le_bytes(page[22..26].try_into().unwrap());
            page[22..26].fill(0);
            if d[4] != 0 || ogg::crc(&page) != want {
                self.pending.skip(1);
                continue;
            }
            self.pending.skip(len);
            let flags = page[5];
            let serial = u32::from_le_bytes(page[14..18].try_into().unwrap());
            let table = page[27..27 + nseg].to_vec();
            let body = page[27 + nseg..].to_vec();
            return Some((flags, serial, table, body));
        }
    }

    fn take_page(&mut self, flags: u8, serial: u32, table: &[u8], body: &[u8]) -> Result<()> {
        if flags & 0x02 != 0 {
            let kind = Kind::of(body);
            let adopt = match (self.serial, kind) {
                (None, Some(_)) => true,
                // a chained stream of the same codec carries on (after the last one's end, or once its
                // audio has begun: a multiplexed stream's beginnings all come before any audio, and a
                // radio stream may lose the page that marks the end)
                (Some(_), Some(k)) => (self.stream_ended || self.pts > 0) && self.kind == Some(k),
                (None, None) => {
                    if self.unsupported.is_none() {
                        self.unsupported = Some(if body.starts_with(b"Speex") {
                            "Speex"
                        } else if body.starts_with(b"\x80theora") {
                            "Theora"
                        } else {
                            "an unrecognised codec"
                        });
                    }
                    false
                }
                _ => false,
            };
            if adopt {
                self.serial = Some(serial);
                self.kind = kind;
                self.stream_ended = false;
                self.headers.clear();
                self.seen = 0;
                self.packet.clear();
                self.broken = false;
            }
        }
        if self.serial != Some(serial) {
            if self.serial.is_none() && flags & 0x02 == 0 {
                // past the first pages with nothing readable
                if let Some(name) = self.unsupported {
                    return Err(Error::new(ErrorCode::UnsupportedCodec, format!("Ogg: the stream holds {name}; only Opus, Vorbis and FLAC in Ogg are read")));
                }
            }
            return Ok(());
        }
        if flags & 0x01 == 0 && !self.packet.is_empty() {
            // the packet lost its tail
            self.packet.clear();
        }
        if flags & 0x01 != 0 && self.packet.is_empty() {
            // joined in the middle of a packet
            self.broken = true;
        }
        let mut p = 0;
        for &s in table {
            if self.packet.len() + s as usize > MAX_OGG_PACKET {
                self.packet.clear();
                self.broken = true;
            }
            self.packet.extend_from_slice(&body[p..p + s as usize]);
            p += s as usize;
            if s < 255 {
                let pkt = std::mem::take(&mut self.packet);
                if !std::mem::take(&mut self.broken) {
                    self.take_packet(pkt)?;
                }
            }
        }
        if flags & 0x04 != 0 {
            self.stream_ended = true;
        }
        Ok(())
    }

    fn take_packet(&mut self, pkt: Vec<u8>) -> Result<()> {
        let kind = self.kind.unwrap();
        let header = match kind {
            Kind::Opus => self.seen < 2,
            Kind::Vorbis => self.seen < 3,
            // FLAC's metadata packets never start with a frame sync code
            Kind::Flac => self.seen == 0 || pkt.len() < 2 || pkt[0] != 0xFF || pkt[1] & 0xFE != 0xF8,
        };
        self.seen += 1;
        if header {
            if !self.streams.is_empty() && kind == Kind::Flac {
                // metadata after the stream is known (or junk) is not needed
                return Ok(());
            }
            if self.headers.len() >= 64 || self.headers.iter().map(Vec::len).sum::<usize>() + pkt.len() > MAX_OGG_HEADERS {
                return Err(Error::invalid_input("Ogg: the stream's headers are larger than 16 MiB"));
            }
            self.headers.push(pkt);
            if kind == Kind::Vorbis && self.headers.len() == 3 {
                let extradata = ironav_codec::vorbis::join_headers([&self.headers[0][..], &self.headers[1], &self.headers[2]]);
                self.timer = Some(ironav_codec::vorbis::PacketTimer::new(&extradata)?);
                let (channels, rate) = ironav_codec::vorbis::PacketTimer::channels_and_rate(&extradata)?;
                (self.link_rate, self.link_start, self.link_samples) = (rate, self.pts, 0);
                if self.streams.is_empty() {
                    self.streams.push(info(CodecId::Vorbis, rate, channels, None, extradata.clone()));
                } else if extradata != self.in_use {
                    // a link set up differently (other codebooks, rate or channels): its headers go
                    // in front of its audio, and the decoder starts over with them
                    for h in std::mem::take(&mut self.headers) {
                        self.out.push_back(StreamPacket { packet: Packet { stream: 0, pts: self.pts, duration: 0, data: h }, key: true });
                    }
                }
                self.in_use = extradata;
            }
            if kind == Kind::Opus && self.headers.len() == 2 && self.streams.is_empty() {
                let hd = &self.headers[0];
                if hd.len() < 19 || hd[8] >> 4 != 0 {
                    return Err(Error::invalid_input("Ogg: OpusHead is too short or has an unknown version"));
                }
                self.streams.push(info(CodecId::Opus, 48_000, hd[9] as u16, None, hd.clone()));
            }
            return Ok(());
        }
        if kind == Kind::Flac && self.streams.is_empty() {
            let hd = &self.headers[0];
            if hd.len() < 51 || &hd[9..13] != b"fLaC" || hd[5] != 1 {
                return Err(Error::invalid_input("Ogg: the FLAC mapping header is too short or has an unknown version"));
            }
            let b = &hd[17..51];
            let packed = u64::from_be_bytes(b[10..18].try_into().unwrap());
            let (rate, channels, bits) = ((packed >> 44) as u32, ((packed >> 41) & 7) as u16 + 1, ((packed >> 36) & 31) as u8 + 1);
            if rate == 0 {
                return Err(Error::invalid_input("Ogg: STREAMINFO has a zero sample rate"));
            }
            self.max_block = u16::from_be_bytes([b[2], b[3]]) as u32;
            self.streams.push(info(CodecId::Flac, rate, channels, Some(bits), Vec::new()));
        }
        if self.streams.is_empty() {
            // audio before the headers it needs
            return Ok(());
        }
        let len = match kind {
            Kind::Opus => {
                if pkt.is_empty() {
                    960
                } else {
                    ironav_codec::opus::packet_samples(&pkt[..pkt.len().min(2)]).filter(|&n| n > 0).unwrap_or(960) as u32
                }
            }
            Kind::Vorbis => match self.timer.as_mut() {
                Some(t) => t.next(&pkt),
                None => return Ok(()),
            },
            Kind::Flac => ironav_codec::flac::header::parse_header(&pkt).map_or(self.max_block, |h| h.block_size),
        };
        let pts = self.pts;
        let rate = match &self.streams[0].params {
            StreamParams::Audio { sample_rate, .. } => *sample_rate,
            _ => 0,
        };
        if kind == Kind::Vorbis && self.link_rate != rate && self.link_rate > 0 {
            // counted in the link's samples, timed in the stream's
            self.link_samples += len as i64;
            self.pts = self.link_start + (self.link_samples as i128 * rate as i128 / self.link_rate as i128) as i64;
        } else {
            self.pts += len as i64;
        }
        self.out.push_back(StreamPacket { packet: Packet { stream: 0, pts, duration: self.pts - pts, data: pkt }, key: true });
        Ok(())
    }
}

fn info(codec: CodecId, rate: u32, channels: u16, bits: Option<u8>, extradata: Vec<u8>) -> StreamInfo {
    StreamInfo {
        index: 0,
        codec,
        time_base: Rational::new(1, rate as i64),
        duration: None,
        bits,
        extradata,
        lead_in: 0,
        params: StreamParams::Audio { sample_rate: rate, channels },
    }
}

impl StreamDemuxer for OggStream {
    fn format_name(&self) -> &'static str {
        "ogg"
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
            if let Some(p) = self.out.pop_front() {
                return Ok(Some(p));
            }
            let Some((flags, serial, table, body)) = self.page() else { return Ok(None) };
            self.take_page(flags, serial, &table, &body)?;
        }
    }
}

// ---- MP3 --------------------------------------------------------------------------------------------

#[derive(Default)]
pub struct Mp3Stream {
    pending: Pending,
    streams: Vec<StreamInfo>,
    /// The first frame's header, once the stream is found.
    locked: Option<Header>,
    pts: i64,
    ended: bool,
}

impl StreamDemuxer for Mp3Stream {
    fn format_name(&self) -> &'static str {
        "mp3"
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
            let d = self.pending.data();
            if d.len() < 10 && !self.ended || d.len() < 4 {
                return Ok(None);
            }
            if let Some(tag) = id3_len(d)? {
                if d.len() < tag {
                    return Ok(None);
                }
                self.pending.skip(tag);
                continue;
            }
            let Some(h) = parse_header(&d[..4]) else {
                self.pending.skip(1);
                continue;
            };
            if d.len() < h.frame_len {
                // a frame cut off at the end of the input is dropped
                return Ok(None);
            }
            match self.locked {
                Some(f) if f.sample_rate == h.sample_rate && f.lsf == h.lsf => {}
                Some(_) => {
                    self.pending.skip(1);
                    continue;
                }
                None => {
                    if d.len() < h.frame_len + 4 && !self.ended {
                        return Ok(None);
                    }
                    let follows = d.get(h.frame_len..h.frame_len + 4).and_then(parse_header).is_some_and(|n| n.sample_rate == h.sample_rate && n.lsf == h.lsf);
                    if !follows && !(self.ended && d.len() == h.frame_len) {
                        self.pending.skip(1);
                        continue;
                    }
                    self.locked = Some(h);
                    if crate::mp3::is_info_frame(d, 0, &h) {
                        self.pending.skip(h.frame_len);
                        continue;
                    }
                }
            }
            if self.streams.is_empty() {
                self.streams.push(info(CodecId::Mp3, h.sample_rate, h.channels(), None, Vec::new()));
            }
            let data = self.pending.take(h.frame_len);
            let pts = self.pts;
            self.pts += h.samples() as i64;
            return Ok(Some(StreamPacket { packet: Packet { stream: 0, pts, duration: h.samples() as i64, data }, key: true }));
        }
    }
}

// ---- after an ID3v2 tag ---------------------------------------------------------------------------

/// A stream that starts with an ID3v2 tag: what follows the tag (MP3 or ADTS) decides the reader.
#[derive(Default)]
pub struct Id3Stream {
    pending: Pending,
    inner: Option<Box<dyn StreamDemuxer>>,
    ended: bool,
}

impl Id3Stream {
    fn decide(&mut self) -> Result<()> {
        if self.inner.is_some() {
            return Ok(());
        }
        let d = self.pending.data();
        let Some(tag) = id3_len(d)? else {
            if d.len() >= 10 || self.ended {
                return Err(Error::invalid_input("the stream's ID3v2 tag is cut short"));
            }
            return Ok(());
        };
        // enough to tell an ADTS header (7 bytes) from an MP3 one
        if d.len() < tag + 7 && !self.ended {
            return Ok(());
        }
        let rest = &d[tag.min(d.len())..];
        let mut inner: Box<dyn StreamDemuxer> = if adts::looks_like_adts(rest) {
            // HLS packed audio says in the tag where it starts
            Box::new(crate::stream::AdtsStream::starting_at(crate::stream::apple_timestamp(&d[..tag.min(d.len())])))
        } else if rest.len() >= 4 && parse_header(rest).is_some() {
            Box::new(Mp3Stream::default())
        } else if self.ended {
            return Err(Error::new(ErrorCode::UnsupportedFormat, "neither MP3 nor ADTS follows the stream's ID3v2 tag"));
        } else {
            // garbage after the tag: let the MP3 reader search for frames
            Box::new(Mp3Stream::default())
        };
        inner.feed(rest)?;
        if self.ended {
            inner.end();
        }
        self.pending = Pending::default();
        self.inner = Some(inner);
        Ok(())
    }
}

impl StreamDemuxer for Id3Stream {
    fn format_name(&self) -> &'static str {
        self.inner.as_ref().map_or("id3", |i| i.format_name())
    }
    fn streams(&self) -> &[StreamInfo] {
        self.inner.as_ref().map_or(&[], |i| i.streams())
    }
    fn feed(&mut self, bytes: &[u8]) -> Result<()> {
        match self.inner.as_mut() {
            Some(i) => i.feed(bytes),
            None => self.pending.push(bytes),
        }
    }
    fn end(&mut self) {
        self.ended = true;
        if let Some(i) = self.inner.as_mut() {
            i.end();
        }
    }
    fn next_packet(&mut self) -> Result<Option<StreamPacket>> {
        self.decide()?;
        match self.inner.as_mut() {
            Some(i) => i.next_packet(),
            None => Ok(None),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn fixture(name: &str) -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../ironav-engine/tests/data").join(name)
    }

    /// Every packet of `bytes` fed to `d` in pieces of `step`.
    fn streamed(mut d: Box<dyn StreamDemuxer>, bytes: &[u8], step: usize) -> (Vec<StreamInfo>, Vec<(i64, Vec<u8>)>) {
        let mut out = Vec::new();
        for c in bytes.chunks(step) {
            d.feed(c).unwrap();
            while let Some(p) = d.next_packet().unwrap() {
                out.push((p.packet.pts, p.packet.data));
            }
        }
        d.end();
        while let Some(p) = d.next_packet().unwrap() {
            out.push((p.packet.pts, p.packet.data));
        }
        (d.streams().to_vec(), out)
    }

    fn from_file(path: &Path) -> (StreamInfo, Vec<(i64, Vec<u8>)>) {
        let mut d = crate::open_demuxer(path).unwrap();
        let s = d.streams()[0].clone();
        let mut out = Vec::new();
        while let Some(p) = d.read_packet().unwrap() {
            out.push((p.pts, p.data));
        }
        (s, out)
    }

    #[test]
    fn streamed_ogg_gives_the_file_readers_packets() {
        for name in ["opus_celt_stereo.opus", "opus_silk_mono.opus", "vorbis_stereo.ogg", "vorbis_5_1.ogg", "flac_in_ogg.ogg"] {
            let path = fixture(name);
            let bytes = std::fs::read(&path).unwrap();
            let (want_info, want) = from_file(&path);
            for step in [1, 97, 4096, bytes.len()] {
                let (info, got) = streamed(crate::stream::stream_demuxer(&bytes[..200], &Default::default()).unwrap(), &bytes, step);
                assert_eq!(info.len(), 1, "{name}");
                assert_eq!((info[0].codec, &info[0].params, &info[0].extradata), (want_info.codec, &want_info.params, &want_info.extradata), "{name}");
                assert_eq!(got.len(), want.len(), "{name} in pieces of {step}");
                assert!(got == want, "{name} in pieces of {step}");
            }
        }
    }

    /// Every audio frame `d` gives for `bytes`, decoded.
    fn decoded(bytes: &[u8]) -> (StreamInfo, Vec<ironav_core::AudioFrame>) {
        use ironav_core::Frame;
        let mut d = OggStream::default();
        d.feed(bytes).unwrap();
        d.end();
        let mut pkts = Vec::new();
        while let Some(p) = d.next_packet().unwrap() {
            pkts.push(p.packet);
        }
        let s = d.streams()[0].clone();
        // as the registry gives it, with the end trim that packet durations drive
        let mut dec = ironav_codec::new_decoder_for(&s).unwrap();
        let mut out = Vec::new();
        for p in &pkts {
            for f in dec.decode(p).unwrap() {
                let Frame::Audio(a) = f else { unreachable!() };
                out.push(a);
            }
        }
        (s, out)
    }

    #[test]
    fn a_chained_vorbis_link_with_other_parameters_is_decoded_as_itself() {
        let stereo = std::fs::read(fixture("vorbis_stereo.ogg")).unwrap();
        let mono = std::fs::read(fixture("vorbis_mono_8k.ogg")).unwrap();
        let (_, a) = decoded(&stereo);
        let (_, b) = decoded(&mono);
        // 44.1 kHz stereo, then 8 kHz mono, as a radio station might chain them
        let mut chained = stereo.clone();
        chained.extend_from_slice(&mono);
        let (info, got) = decoded(&chained);
        assert_eq!(info.params, StreamParams::Audio { sample_rate: 44100, channels: 2 });
        assert_eq!(got.len(), a.len() + b.len());
        for (g, w) in got.iter().zip(a.iter().chain(&b)) {
            assert_eq!((g.sample_rate, g.channels, &g.samples), (w.sample_rate, w.channels, &w.samples));
        }
        // the second link's times follow the first's, in the stream's time base (1/44100)
        let end = a.last().map(|f| f.pts + f.frames() as i64).unwrap();
        let start = got[a.len()].pts;
        assert!((start - end).abs() <= 44100 / 10, "{start} after {end}");
        let b0 = b[0].pts as f64 / 8000.0;
        let b_last = b.last().unwrap().pts as f64 / 8000.0;
        let g_last = got.last().unwrap().pts as f64 / 44100.0;
        assert!((g_last - start as f64 / 44100.0 - (b_last - b0)).abs() < 0.001, "{g_last}");
        // and the other way round: a link at a higher rate than the stream's keeps all its samples
        let mut chained = mono.clone();
        chained.extend_from_slice(&stereo);
        let (_, got) = decoded(&chained);
        assert_eq!(got.len(), a.len() + b.len());
        for (g, w) in got.iter().zip(b.iter().chain(&a)) {
            assert_eq!((g.sample_rate, g.channels, &g.samples), (w.sample_rate, w.channels, &w.samples));
        }
        // the same link again changes nothing: no headers are passed on
        let mut twice = stereo.clone();
        twice.extend_from_slice(&stereo);
        let (_, pk) = streamed(Box::new(OggStream::default()), &twice, 4096);
        assert!(!pk.iter().any(|(_, d)| d.first().is_some_and(|b| b & 1 == 1)), "no header packets");
    }

    #[test]
    fn a_chained_ogg_stream_carries_on_and_a_damaged_page_is_stepped_over() {
        let bytes = std::fs::read(fixture("opus_silk_mono.opus")).unwrap();
        let (_, once) = streamed(Box::new(OggStream::default()), &bytes, 4096);
        // the same stream twice, as a radio station chains songs (the second with another serial)
        let mut second = bytes.clone();
        let mut at = 0;
        while at + 27 <= second.len() {
            let nseg = second[at + 26] as usize;
            let len = 27 + nseg + second[at + 27..at + 27 + nseg].iter().map(|&s| s as usize).sum::<usize>();
            second[at + 14] ^= 0x55;
            second[at + 22..at + 26].fill(0);
            let c = ogg::crc(&second[at..at + len]);
            second[at + 22..at + 26].copy_from_slice(&c.to_le_bytes());
            at += len;
        }
        let mut chained = bytes.clone();
        chained.extend_from_slice(&second);
        let (_, twice) = streamed(Box::new(OggStream::default()), &chained, 1000);
        assert_eq!(twice.len(), once.len() * 2);
        let end = once.last().map(|p| p.0).unwrap() + 960;
        assert!(twice[once.len()].0 >= end - 960 && twice[once.len()].0 <= end + 960, "the timeline carries on");
        // one damaged byte in the middle loses that page, not the stream
        let mut bad = bytes.clone();
        let mid = bad.len() / 2;
        bad[mid] ^= 0xFF;
        let (_, got) = streamed(Box::new(OggStream::default()), &bad, 4096);
        assert!(got.len() < once.len() && got.len() + 60 > once.len(), "{} of {}", got.len(), once.len());
        // a chain whose first link lost its end-of-stream mark still carries on
        let mut no_eos = bytes.clone();
        let mut at = 0;
        let mut last = 0;
        while at + 27 <= no_eos.len() {
            let nseg = no_eos[at + 26] as usize;
            last = at;
            at += 27 + nseg + no_eos[at + 27..at + 27 + nseg].iter().map(|&s| s as usize).sum::<usize>();
        }
        let len = no_eos.len() - last;
        no_eos[last + 5] &= !0x04;
        no_eos[last + 22..last + 26].fill(0);
        let c = ogg::crc(&no_eos[last..last + len]);
        no_eos[last + 22..last + 26].copy_from_slice(&c.to_le_bytes());
        no_eos.extend_from_slice(&second);
        let (_, got) = streamed(Box::new(OggStream::default()), &no_eos, 4096);
        assert_eq!(got.len(), once.len() * 2);
    }

    #[test]
    fn streamed_mp3_gives_the_file_readers_frames_even_behind_a_tag() {
        for name in ["mp3_mpeg1_stereo.mp3", "mp3_mpeg2_mono.mp3", "mp3_mpeg25_mono.mp3"] {
            let path = fixture(name);
            let bytes = std::fs::read(&path).unwrap();
            let (want_info, want) = from_file(&path);
            // the file reader keeps a LAME trim; a stream has none
            let mut tagged = b"ID3\x04\x00\x00\x00\x00\x07\x68".to_vec();
            tagged.extend(std::iter::repeat_n(0xFFu8, 1000));
            tagged.extend_from_slice(&bytes);
            for (input, step) in [(&bytes, 1), (&bytes, 4096), (&tagged, 97), (&tagged, tagged.len())] {
                let d = crate::stream::stream_demuxer(&input[..200], &Default::default()).unwrap();
                let (info, got) = streamed(d, input, step);
                assert_eq!(info[0].codec, CodecId::Mp3, "{name}");
                assert_eq!(info[0].params, want_info.params, "{name}");
                assert!(got == want, "{name} in pieces of {step}: {} frames, want {}", got.len(), want.len());
            }
        }
    }

    #[test]
    fn adts_behind_a_tag_is_still_adts() {
        let bytes = std::fs::read(fixture("aac_stereo_tools.aac")).unwrap();
        let mut tagged = b"ID3\x03\x00\x00\x00\x00\x01\x00".to_vec();
        tagged.extend(std::iter::repeat_n(0u8, 128));
        tagged.extend_from_slice(&bytes);
        let d = crate::stream::stream_demuxer(&tagged[..200], &Default::default()).unwrap();
        let (info, got) = streamed(d, &tagged, 333);
        assert_eq!(info[0].codec, CodecId::Aac);
        assert!(got.len() > 10);
        // told apart even when the bytes after the tag trickle in
        let d = crate::stream::stream_demuxer(&tagged[..200], &Default::default()).unwrap();
        let (info, got2) = streamed(d, &tagged, 1);
        assert_eq!(info[0].codec, CodecId::Aac);
        assert_eq!(got2.len(), got.len());
        // a tag too big to hold is refused as soon as its header arrives
        let mut d = Id3Stream::default();
        d.feed(b"ID3\x04\x00\x00\x7f\x7f\x7f\x7f").unwrap();
        assert!(d.next_packet().is_err());
    }
}
