//! Fragmented MP4 (CMAF) read as it arrives: an initialization segment (`ftyp`, `moov` with
//! `mvex`) and then `moof`+`mdat` pairs, as LL-HLS and DASH packagers, Safari's MediaRecorder and
//! `ffmpeg -movflags frag_keyframe+empty_moov` write it. Each fragment is parsed by the file
//! reader's own `parse_trak` and `parse_moof` (the parent module), and its samples are cut from the
//! `mdat` that follows, so the two readers give the same packets.
//!
//! The streams are published after the first fragment, whose sample durations give the video frame
//! rate (the index of a fragmented file is empty), and the rate is not revised later. Video starts at
//! a sync sample. Packets come out in fragment order, as they arrive, like the other live readers.
//! Boxes that are not needed (`styp`, `sidx`, `emsg`, `free`, ...) are stepped over without being
//! held.
//!
//! A stream must start with its initialization segment: a fragment before it is an error rather
//! than silence, since joining a CMAF stream mid-way leaves nothing to decode. A repeated
//! initialization segment (a packager reconnecting) is accepted if it is identical, and refused if
//! it changes the tracks.

use super::*;
use crate::stream::{StreamDemuxer, StreamPacket};
use std::collections::VecDeque;

/// The largest box held whole (`moov`, `moof`, `mdat`).
const MAX_BOX: u64 = 64 << 20;
/// Samples waiting for their `mdat`, at most.
const MAX_PENDING: usize = 1 << 16;

#[derive(Default)]
pub struct Mp4Stream {
    buf: Vec<u8>,
    pos: usize,
    /// Absolute offset of `buf[0]` in the stream.
    abs: u64,
    skip: u64,
    /// The initialization segment's `moov`, to recognise a repeat of it.
    init: Vec<u8>,
    tracks: Vec<TrackInfo>,
    ids: Vec<u32>,
    defaults: Vec<[u32; 3]>,
    next_dts: Vec<i64>,
    streams: Vec<StreamInfo>,
    ready: bool,
    /// Samples of the last `moof`, waiting for the `mdat` that holds them.
    pending: VecDeque<Sample>,
    seen: Vec<bool>,
    started: Vec<bool>,
    out: VecDeque<StreamPacket>,
}

impl Mp4Stream {
    fn here(&self) -> u64 {
        self.abs + self.pos as u64
    }

    fn step(&mut self) -> Result<bool> {
        if self.skip > 0 {
            let n = (self.buf.len() - self.pos).min(self.skip.min(usize::MAX as u64) as usize);
            self.pos += n;
            self.skip -= n as u64;
            return Ok(n > 0);
        }
        let d = &self.buf[self.pos..];
        if d.len() < 8 {
            return Ok(false);
        }
        let kind: [u8; 4] = d[4..8].try_into().unwrap();
        let (total, hdr) = match be32(d, 0)? {
            0 => return Err(corrupt(format!("a '{}' box runs to the end of the stream, which a live stream never reaches", String::from_utf8_lossy(&kind)))),
            1 if d.len() < 16 => return Ok(false),
            1 => (be64(d, 8)?, 16usize),
            n => (n as u64, 8usize),
        };
        if total < hdr as u64 {
            return Err(corrupt("a box is smaller than its header"));
        }
        if !matches!(&kind, b"moov" | b"moof" | b"mdat") {
            self.pos += hdr;
            self.skip = total - hdr as u64;
            return Ok(true);
        }
        if total > MAX_BOX {
            return Err(corrupt(format!("a '{}' box of {total} bytes is larger than a live stream's should be", String::from_utf8_lossy(&kind))));
        }
        if (d.len() as u64) < total {
            return Ok(false);
        }
        let start = self.here();
        let body = d[hdr..total as usize].to_vec();
        self.pos += total as usize;
        match &kind {
            b"moov" if self.init.is_empty() => {
                self.moov(&body)?;
                self.init = body;
            }
            b"moov" if body != self.init => {
                return Err(Error::new(ErrorCode::UnsupportedFormat, "the stream sent a new initialization segment that changes its tracks").hint("open the stream again to read the new tracks"));
            }
            b"moof" if self.init.is_empty() => return Err(corrupt("a fragment came before the initialization segment ('moov'); a live MP4 must start with it")),
            b"moof" => self.moof(&body, start)?,
            b"mdat" => self.mdat(&body, start + hdr as u64)?,
            _ => {}
        }
        Ok(true)
    }

    fn moov(&mut self, moov: &[u8]) -> Result<()> {
        let movie_ts = super::movie_timescale(moov)?;
        let mut samples = Vec::new();
        for b in boxes(moov) {
            let (kind, body) = b?;
            if &kind != b"trak" {
                continue;
            }
            // an initialization segment lists no samples; refuse one that does before expanding it
            let stsz = child(body, b"mdia")?.map(|m| child(m, b"minf")).transpose()?.flatten().map(|m| child(m, b"stbl")).transpose()?.flatten().map(|s| child(s, b"stsz")).transpose()?.flatten();
            if stsz.is_some_and(|z| be32(z, 8).unwrap_or(0) != 0) {
                return Err(Error::new(ErrorCode::UnsupportedFormat, "the MP4's 'moov' lists samples, so it is not a fragmented stream's initialization segment").hint("read it as a file"));
            }
            if let Some(t) = parse_trak(body, self.tracks.len(), &mut samples, movie_ts)? {
                let tkhd = need(body, b"tkhd", "trak")?;
                self.ids.push(if tkhd.first() == Some(&1) { be32(tkhd, 20)? } else { be32(tkhd, 12)? });
                self.tracks.push(t);
            }
        }
        if self.tracks.is_empty() {
            return Err(Error::new(ErrorCode::UnsupportedCodec, "the MP4 has no audio or video track IronAV recognises"));
        }
        let n = self.tracks.len();
        self.defaults = vec![[0; 3]; n];
        if let Some(mvex) = child(moov, b"mvex")? {
            for b in boxes(mvex) {
                let (kind, trex) = b?;
                if &kind == b"trex" {
                    if let Some(i) = self.ids.iter().position(|&id| id == be32(trex, 4).unwrap_or(0)) {
                        self.defaults[i] = [be32(trex, 12)?, be32(trex, 16)?, be32(trex, 20)?];
                    }
                }
            }
        } else {
            return Err(Error::new(ErrorCode::UnsupportedFormat, "the MP4 is not fragmented (no 'mvex'), so it cannot be read as it arrives").hint("read it as a file, or write it with fragments (CMAF)"));
        }
        self.next_dts = vec![0; n];
        self.seen = vec![false; n];
        self.started = vec![false; n];
        Ok(())
    }

    fn moof(&mut self, moof: &[u8], start: u64) -> Result<()> {
        let mut samples = Vec::new();
        parse_moof(moof, start, &self.ids, &self.defaults, &mut self.next_dts, &mut samples, MAX_PENDING - self.pending.len())?;
        if !self.ready {
            // the frame rate, from this fragment as the file reader takes it from all of them
            for (i, t) in self.tracks.iter_mut().enumerate() {
                let (n, total) = samples.iter().filter(|s| s.stream as usize == i).fold((0i64, 0i64), |(n, d), s| (n + 1, d + s.duration as i64));
                if let StreamParams::Video { frame_rate, .. } = &mut t.stream.params {
                    if total > 0 {
                        let (num, den) = (n * t.stream.time_base.den, total);
                        let g = gcd(num, den);
                        *frame_rate = Rational::new(num / g, den / g);
                    }
                }
            }
            self.streams = self.tracks.iter().map(|t| StreamInfo { duration: None, ..t.stream.clone() }).collect();
            self.ready = true;
        }
        self.pending.extend(samples);
        Ok(())
    }

    /// Cuts the waiting samples that lie in this `mdat` (whose data starts at `data_at`).
    fn mdat(&mut self, body: &[u8], data_at: u64) -> Result<()> {
        let end = data_at + body.len() as u64;
        let mut later = VecDeque::new();
        while let Some(s) = self.pending.pop_front() {
            if s.offset < data_at {
                return Err(corrupt("a fragment's sample lies in data that has already gone by"));
            }
            if s.offset.saturating_add(s.size as u64) > end {
                later.push_back(s);
                continue;
            }
            let at = (s.offset - data_at) as usize;
            self.sample(s, &body[at..at + s.size as usize])?;
        }
        self.pending = later;
        Ok(())
    }

    fn sample(&mut self, s: Sample, raw: &[u8]) -> Result<()> {
        let st = s.stream as usize;
        let t = &self.tracks[st];
        let video = matches!(t.stream.params, StreamParams::Video { .. });
        if video && !self.started[st] {
            if !s.sync {
                return Ok(());
            }
            self.started[st] = true;
        }
        let data = if t.convert {
            let n = t.config.nal_length;
            let mut out = Vec::with_capacity(raw.len() + t.config.prefix.len() + 16);
            if s.sync || !self.seen[st] {
                out.extend_from_slice(&t.config.prefix);
            }
            let mut at = 0;
            while at < raw.len() {
                let len = raw.get(at..at + n).ok_or_else(|| corrupt("video sample ends inside a NAL length"))?.iter().fold(0usize, |a, &b| (a << 8) | b as usize);
                at += n;
                push_start_coded(&mut out, raw.get(at..at + len).ok_or_else(|| corrupt("video sample ends inside a NAL unit"))?);
                at += len;
            }
            out
        } else {
            raw.to_vec()
        };
        self.seen[st] = true;
        let packet = Packet { stream: st, pts: s.dts.saturating_add(s.cts_offset as i64), duration: s.duration as i64, data };
        self.out.push_back(StreamPacket { packet, key: s.sync || !video });
        Ok(())
    }
}

impl StreamDemuxer for Mp4Stream {
    fn format_name(&self) -> &'static str {
        "mp4"
    }
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }
    fn feed(&mut self, bytes: &[u8]) -> Result<()> {
        self.buf.extend_from_slice(bytes);
        while self.step()? {}
        self.buf.drain(..self.pos);
        self.abs += self.pos as u64;
        self.pos = 0;
        Ok(())
    }
    fn next_packet(&mut self) -> Result<Option<StreamPacket>> {
        Ok(self.out.pop_front())
    }
    fn end(&mut self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("{}/tests/data/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    fn by_stream(packets: impl IntoIterator<Item = Packet>) -> BTreeMap<usize, Vec<(i64, i64, Vec<u8>)>> {
        let mut m: BTreeMap<usize, Vec<_>> = BTreeMap::new();
        for p in packets {
            m.entry(p.stream).or_default().push((p.pts, p.duration, p.data));
        }
        m
    }

    #[test]
    fn streamed_fragments_give_the_file_readers_packets() {
        // written by FFmpeg with frag_keyframe+empty_moov+default_base_moof: H.264 (B-frames) + AAC,
        // and HEVC + Opus
        for name in ["h264_aac_frag.mp4", "hevc_opus_frag.mp4"] {
            let data = fixture(name);
            let mut file = Mp4Demuxer::open(std::io::Cursor::new(data.clone())).unwrap();
            let want_streams: Vec<StreamInfo> = file.streams().to_vec();
            let want = by_stream(std::iter::from_fn(|| file.read_packet().unwrap()));
            for piece in [1, 97, 4096, data.len()] {
                let mut s = Mp4Stream::default();
                let mut got = Vec::new();
                for c in data.chunks(piece) {
                    s.feed(c).unwrap();
                    while let Some(p) = s.next_packet().unwrap() {
                        got.push(p.packet);
                    }
                }
                s.end();
                assert_eq!(s.streams().len(), want_streams.len());
                for (a, b) in s.streams().iter().zip(&want_streams) {
                    assert_eq!((a.codec, a.time_base, &a.extradata, a.lead_in), (b.codec, b.time_base, &b.extradata, b.lead_in), "{name}");
                    assert_eq!(serde_json::to_value(&a.params).unwrap(), serde_json::to_value(&b.params).unwrap(), "{name}");
                }
                let got = by_stream(got);
                for (k, w) in &want {
                    assert_eq!(got[k].len(), w.len(), "{name} stream {k} pieces of {piece}");
                    assert!(got[k] == *w, "{name} stream {k} pieces of {piece}");
                }
            }
        }
    }

    fn mp4_box(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
        let mut b = (8 + body.len() as u32).to_be_bytes().to_vec();
        b.extend_from_slice(kind);
        b.extend_from_slice(body);
        b
    }

    #[test]
    fn the_initialization_segment_comes_first_and_offsets_never_overflow() {
        let good = fixture("h264_aac_frag.mp4");
        let first_moof = good.windows(4).position(|w| w == b"moof").unwrap() - 4;
        let init = &good[..first_moof];
        let moov_at = init.windows(4).position(|w| w == b"moov").unwrap() - 4;
        let moov = &init[moov_at..];

        // a fragment before any moov is refused, not ignored
        assert!(Mp4Stream::default().feed(&good[first_moof..]).is_err());

        // the same moov again is a reconnecting packager; a different one is refused
        let mut s = Mp4Stream::default();
        s.feed(init).unwrap();
        s.feed(moov).unwrap();
        let mut changed = moov.to_vec();
        *changed.last_mut().unwrap() ^= 1;
        assert!(s.feed(&changed).is_err());

        // a base data offset near 2^64 plus sample sizes must be an error, not an overflow
        let mut s = Mp4Stream::default();
        s.feed(init).unwrap();
        let mut tfhd = vec![0, 0, 0, 1];
        tfhd.extend_from_slice(&s.ids[0].to_be_bytes());
        tfhd.extend_from_slice(&0xFFFF_FFFF_FFFF_FFF0u64.to_be_bytes());
        let mut trun = vec![0, 0, 0x02, 0x01, 0, 0, 0, 2, 0, 0, 0, 8];
        trun.extend_from_slice(&[0, 0, 0, 16, 0, 0, 0, 16]);
        let traf = [mp4_box(b"tfhd", &tfhd), mp4_box(b"trun", &trun)].concat();
        let moof = mp4_box(b"moof", &mp4_box(b"traf", &traf));
        assert!(s.feed(&moof).is_err());
    }

    #[test]
    fn an_unfragmented_file_is_refused_and_damage_never_panics() {
        let mut s = Mp4Stream::default();
        // a moov with one empty trak and no mvex is not something to read live
        let mut moov = Vec::new();
        let (kind, body) = (b"mvhd", vec![0u8; 100]);
        moov.extend_from_slice(&(8 + body.len() as u32).to_be_bytes());
        moov.extend_from_slice(kind);
        moov.extend_from_slice(&body);
        let mut d = (8 + moov.len() as u32).to_be_bytes().to_vec();
        d.extend_from_slice(b"moov");
        d.extend_from_slice(&moov);
        assert!(s.feed(&d).is_err());

        let good = fixture("h264_aac_frag.mp4");
        let mut seed = 0xD1B5_4A32_D192_ED03u64;
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
            let mut s = Mp4Stream::default();
            for c in d.chunks(1 + rand() as usize % 3000) {
                if s.feed(c).is_err() {
                    break;
                }
                while s.next_packet().unwrap().is_some() {}
            }
        }
    }
}
