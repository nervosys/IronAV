//! Renditions of one presentation read as one stream: an HLS variant's video and the separate
//! audio playlist it names arrive as one byte stream of tagged pieces (written by the HLS source:
//! [`MAGIC`], then pieces made by [`piece`]; an empty piece says the part has ended, or failed).
//! Each part is read by its own stream demuxer, and the
//! packets come out in time order, the second part's streams numbered after the first's.
//!
//! The streams are settled once every part has found its own (or has ended, or the other has a
//! backlog): a part's streams that turn up later are not added. Packets wait for the other part's
//! next one, so the order holds, unless that part has ended or fallen this far behind
//! ([`MAX_WAITING`] packets, or [`MAX_WAITING_SECONDS`] of media). A second part whose format
//! cannot be read is left out; the first part's errors are the stream's.

use crate::stream::{stream_demuxer, Pending, StreamDemuxer, StreamOptions, StreamPacket, MAX_PENDING, SNIFF_BYTES};
use ironav_core::{Error, Result, StreamInfo};
use std::collections::VecDeque;

/// What a stream of renditions starts with.
pub const MAGIC: &[u8] = b"IRONAV-RENDITIONS\n";

/// Packets one part may queue while the other has none to compare them with.
const MAX_WAITING: usize = 1024;
/// Seconds of media one part may queue while the other has none.
const MAX_WAITING_SECONDS: f64 = 8.0;

/// A piece of part `part`'s bytes: its number, its length (32 bits, little-endian), the bytes.
pub fn piece(part: u8, bytes: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(5 + bytes.len());
    v.push(part);
    v.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
    v.extend_from_slice(bytes);
    v
}

#[derive(Default)]
struct Part {
    /// The first bytes, until there are enough to tell the format.
    head: Vec<u8>,
    demux: Option<Box<dyn StreamDemuxer>>,
    queue: VecDeque<StreamPacket>,
    /// Its streams as settled, and where they start in the whole.
    streams: Vec<StreamInfo>,
    offset: usize,
    /// It has said it has ended (or failed), so nothing is waited for from it.
    ended: bool,
}

impl Part {
    fn open(&mut self, opts: &StreamOptions, ended: bool) -> Result<()> {
        if self.demux.is_some() || self.head.is_empty() || (self.head.len() < SNIFF_BYTES && !ended) {
            return Ok(());
        }
        let mut d = stream_demuxer(&self.head, opts)?;
        d.feed(&std::mem::take(&mut self.head))?;
        if ended {
            d.end();
        }
        self.demux = Some(d);
        Ok(())
    }

    fn found(&self) -> bool {
        self.demux.as_ref().is_some_and(|d| !d.streams().is_empty())
    }

    fn time(&self, p: &StreamPacket) -> f64 {
        self.streams.get(p.packet.stream).map_or(0.0, |s| p.packet.pts as f64 * s.time_base.as_f64())
    }

    /// Whether it has queued so much that the other part is not waited for any longer.
    fn backlog(&self) -> bool {
        if self.queue.len() >= MAX_WAITING {
            return true;
        }
        let Some(d) = self.demux.as_ref() else { return false };
        let at = |p: &StreamPacket| d.streams().get(p.packet.stream).map(|s| p.packet.pts as f64 * s.time_base.as_f64());
        match (self.queue.front().and_then(at), self.queue.back().and_then(at)) {
            (Some(a), Some(b)) => b - a >= MAX_WAITING_SECONDS,
            _ => false,
        }
    }

    /// No more bytes come for it.
    fn finish(&mut self, opts: &StreamOptions, second: bool) -> Result<()> {
        self.ended = true;
        if let Err(e) = self.open(opts, true) {
            if !second {
                return Err(e);
            }
            self.head.clear();
        }
        if let Some(d) = self.demux.as_mut() {
            d.end();
        }
        Ok(())
    }
}

#[derive(Default)]
pub struct Renditions {
    opts: StreamOptions,
    pending: Pending,
    magic: bool,
    parts: [Part; 2],
    streams: Vec<StreamInfo>,
    settled: bool,
    ended: bool,
}

impl Renditions {
    pub fn new(opts: &StreamOptions) -> Self {
        // the parts' formats are their own
        Renditions { opts: StreamOptions { format: None, ..opts.clone() }, ..Renditions::default() }
    }

    /// Hands out the pieces fed so far.
    fn split(&mut self) -> Result<()> {
        if !self.magic {
            let d = self.pending.data();
            if d.len() < MAGIC.len() {
                return Ok(());
            }
            if !d.starts_with(MAGIC) {
                return Err(Error::invalid_input("not a stream of renditions"));
            }
            self.pending.skip(MAGIC.len());
            self.magic = true;
        }
        loop {
            let d = self.pending.data();
            if d.len() < 5 {
                return Ok(());
            }
            let part = d[0] as usize;
            let len = u32::from_le_bytes(d[1..5].try_into().unwrap()) as usize;
            if part >= self.parts.len() || len > MAX_PENDING {
                return Err(Error::invalid_input("a piece of a stream of renditions is malformed"));
            }
            if d.len() < 5 + len {
                return Ok(());
            }
            self.pending.skip(5);
            let bytes = self.pending.take(len);
            let p = &mut self.parts[part];
            if p.ended {
                continue;
            }
            if bytes.is_empty() {
                p.finish(&self.opts, part > 0)?;
                continue;
            }
            match p.demux.as_mut() {
                Some(dm) => dm.feed(&bytes)?,
                None => {
                    p.head.extend_from_slice(&bytes);
                    if let Err(e) = p.open(&self.opts, false) {
                        if part == 0 {
                            return Err(e);
                        }
                        // the audio cannot be read: the video goes on without it
                        p.head.clear();
                        p.ended = true;
                    }
                }
            }
        }
    }

    /// Takes in the parts' packets, and settles the streams once it can.
    fn pull(&mut self) -> Result<()> {
        for p in &mut self.parts {
            if let Some(d) = p.demux.as_mut() {
                while let Some(sp) = d.next_packet()? {
                    p.queue.push_back(sp);
                }
            }
        }
        if !self.settled {
            let ready = self.ended || self.parts.iter().all(|p| p.found() || p.ended) || self.parts.iter().any(Part::backlog);
            if ready {
                for p in &mut self.parts {
                    p.offset = self.streams.len();
                    p.streams = p.demux.as_ref().map_or(Vec::new(), |d| d.streams().to_vec());
                    self.streams.extend(p.streams.iter().cloned().enumerate().map(|(i, mut s)| {
                        s.index = p.offset + i;
                        s
                    }));
                }
                self.settled = true;
            }
        }
        Ok(())
    }
}

impl StreamDemuxer for Renditions {
    fn format_name(&self) -> &'static str {
        self.parts[0].demux.as_ref().map_or("renditions", |d| d.format_name())
    }
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }
    fn feed(&mut self, bytes: &[u8]) -> Result<()> {
        self.pending.push(bytes)?;
        self.split()
    }
    fn end(&mut self) {
        self.ended = true;
        for p in self.parts.iter_mut().filter(|p| !p.ended) {
            // a short part is read as what it is; one that cannot be is left out
            let _ = p.finish(&self.opts, true);
        }
    }
    fn next_packet(&mut self) -> Result<Option<StreamPacket>> {
        self.pull()?;
        if !self.settled {
            return Ok(None);
        }
        loop {
            // the earliest packet, once every part that may still give one has one to compare
            let behind = !self.ended && self.parts.iter().any(|p| p.queue.is_empty() && !p.ended);
            let backlog = self.parts.iter().any(Part::backlog);
            if behind && !backlog {
                return Ok(None);
            }
            let Some(k) = (0..self.parts.len()).filter(|&k| !self.parts[k].queue.is_empty()).min_by(|&a, &b| {
                let (pa, pb) = (&self.parts[a], &self.parts[b]);
                pa.time(&pa.queue[0]).total_cmp(&pb.time(&pb.queue[0]))
            }) else {
                return Ok(None);
            };
            let p = &mut self.parts[k];
            let mut sp = p.queue.pop_front().unwrap();
            if sp.packet.stream >= p.streams.len() {
                // a stream found after the streams were settled
                continue;
            }
            sp.packet.stream += p.offset;
            return Ok(Some(sp));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironav_core::MediaType;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(name)).unwrap()
    }

    #[test]
    fn two_renditions_come_out_as_one_stream_in_time_order() {
        // the video of a transport stream, and AAC as a separate rendition
        let ts = fixture("h264_aac.ts");
        let aac = std::fs::read(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../ironav-engine/tests/data/aac_stereo_tools.aac")).unwrap();
        let mut bytes = MAGIC.to_vec();
        // in pieces, interleaved, as the HLS source fetches segments in turn
        let (mut a, mut b) = (ts.chunks(3000), aac.chunks(700));
        loop {
            let (x, y) = (a.next(), b.next());
            if x.is_none() && y.is_none() {
                break;
            }
            x.into_iter().for_each(|c| bytes.extend(piece(0, c)));
            y.into_iter().for_each(|c| bytes.extend(piece(1, c)));
        }
        let mut d = Renditions::new(&StreamOptions::default());
        let mut got = Vec::new();
        for c in bytes.chunks(1000) {
            d.feed(c).unwrap();
            while let Some(p) = d.next_packet().unwrap() {
                got.push(p);
            }
        }
        d.end();
        while let Some(p) = d.next_packet().unwrap() {
            got.push(p);
        }
        let s = d.streams();
        // the transport stream's video and audio, then the rendition's audio
        assert_eq!(s.len(), 3, "{s:?}");
        assert_eq!(s[2].params.media_type(), MediaType::Audio);
        assert!(s.iter().enumerate().all(|(i, x)| x.index == i));
        let count = |k: usize| got.iter().filter(|p| p.packet.stream == k).count();
        assert!(count(0) > 0 && count(1) > 0 && count(2) > 0, "{} {} {}", count(0), count(1), count(2));
        // where the output turns from one part to the other, it goes forward in time (within a part,
        // the order is the part's own: decoding order, its streams interleaved as they were)
        let t: Vec<(bool, f64)> = got.iter().map(|p| (p.packet.stream == 2, p.packet.pts as f64 * s[p.packet.stream].time_base.as_f64())).collect();
        assert!(t.windows(2).filter(|w| w[0].0 != w[1].0).all(|w| w[1].1 >= w[0].1 - 1e-9), "{t:?}");
        let switches = t.windows(2).filter(|w| w[0].0 != w[1].0).count();
        assert!(switches >= 10, "interleaved: {switches} switches, {t:?}");
        // the rendition's packets are all there
        let mut alone = crate::stream::AdtsStream::default();
        alone.feed(&aac).unwrap();
        alone.end();
        let mut n = 0;
        while alone.next_packet().unwrap().is_some() {
            n += 1;
        }
        assert_eq!(count(2), n);
        assert_eq!(format!("{}", d.format_name()), "mpegts");
    }

    #[test]
    fn a_part_that_ends_or_cannot_be_read_holds_nothing_up() {
        let ts = fixture("h264_aac.ts");
        let mut bytes = MAGIC.to_vec();
        // the second part is not media, and then says it has ended; the first never ends
        bytes.extend(piece(1, &[0x55; 300]));
        bytes.extend(piece(1, &[]));
        bytes.extend(piece(0, &ts));
        let mut d = Renditions::new(&StreamOptions::default());
        d.feed(&bytes).unwrap();
        let mut n = 0;
        while d.next_packet().unwrap().is_some() {
            n += 1;
        }
        // all but what the transport stream reader keeps until it knows more
        assert_eq!(d.streams().len(), 2);
        assert!(n > 100, "{n}");
    }
}

#[cfg(test)]
mod packed_audio {
    use crate::stream::{stream_demuxer, StreamOptions};

    #[test]
    fn packed_audio_starts_where_its_id3_tag_says() {
        let aac = std::fs::read(std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../ironav-engine/tests/data/aac_stereo_tools.aac")).unwrap();
        // a segment: an ID3v2.4 tag with Apple's PRIV timestamp (90 kHz clock), then the frames
        let segment = |t90k: u64| {
            let mut frame = b"com.apple.streaming.transportStreamTimestamp\0".to_vec();
            frame.extend_from_slice(&t90k.to_be_bytes());
            let mut tag = b"ID3\x04\x00\x00".to_vec();
            let size = 10 + frame.len();
            tag.extend((0..4).rev().map(|i| (size >> (7 * i) & 0x7F) as u8));
            tag.extend_from_slice(b"PRIV");
            tag.extend((0..4).rev().map(|i| (frame.len() >> (7 * i) & 0x7F) as u8));
            tag.extend_from_slice(&[0, 0]);
            tag.extend_from_slice(&frame);
            tag.extend_from_slice(&aac);
            tag
        };
        // at 10 s, then at 20 s (as after a gap): each segment's tag sets the clock
        let mut bytes = segment(900_000);
        bytes.extend(segment(1_800_000));
        let mut d = stream_demuxer(&bytes[..200], &StreamOptions::default()).unwrap();
        d.feed(&bytes).unwrap();
        d.end();
        let mut t = Vec::new();
        while let Some(p) = d.next_packet().unwrap() {
            t.push(p.packet.pts as f64 * d.streams()[0].time_base.as_f64());
        }
        assert!((t[0] - 10.0).abs() < 0.001, "{t:?}");
        assert!((t[t.len() / 2] - 20.0).abs() < 0.001, "{t:?}");
    }
}
