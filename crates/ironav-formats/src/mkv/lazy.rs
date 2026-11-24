//! Opening a Matroska file without walking all of it.
//!
//! [`open`](super::open) reads where every frame lies, which for a long file means touching every
//! block of every cluster. Here the Cues do that work: the file is read up to its first cluster (Info,
//! Tracks, SeekHead), the Cues are found through the SeekHead, and frames are indexed a window of clusters
//! at a time, from the start when playing and from the cue at or before the target when seeking.
//! A window is turned into samples and read by the same machinery as a whole file (`Mp4Demuxer`).
//!
//! This is used only when the file has what it needs: a SeekHead that names the Cues, Cues that look
//! right (cluster positions inside the segment, in order, and clusters where the first, the last and some
//! between them are said to be), the Info's Duration, and clusters of known size. Otherwise [`open_indexed`]
//! falls back to [`open`](super::open), which finds every frame itself. A stream's duration here is the
//! Info's, not the sum of its frames, and a video track's decode order inside a window is its presentation
//! order, as it is in a file read whole.

use super::*;
use ironav_core::{Demuxer, Packet};
use std::sync::{Arc, Mutex};

const CUE_POINT: u32 = 0xBB;
const CUE_TIME: u32 = 0xB3;
const CUE_TRACK_POSITIONS: u32 = 0xB7;
const CUE_TRACK: u32 = 0xF7;
const CUE_CLUSTER_POSITION: u32 = 0xF1;

/// The Cues are read whole up to this size, and hold this many positions at most.
const MAX_CUES_BYTES: u64 = 256 << 20;
const MAX_CUES: usize = 4 << 20;
/// How much media a window covers when playing on (a window is whole clusters, so it may be more).
const WINDOW_SECS: f64 = 10.0;
/// How many of the cues are checked to point at a cluster.
const SPOT_CHECKS: usize = 9;

/// A place to start reading: the time (in ticks) of a cue and where its cluster is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Cue {
    pub time: i64,
    pub track: u64,
    /// From the start of the file.
    pub at: u64,
}

/// The cue points of a Cues element's body. `segment_data` is where the Segment's data starts, which
/// the positions are counted from. A malformed element is an error, not a partial list.
pub(crate) fn parse_cues(body: &[u8], segment_data: u64) -> Result<Vec<Cue>> {
    let mut out = Vec::new();
    for e in Elems(body) {
        let (id, point) = e?;
        if id != CUE_POINT {
            continue;
        }
        let (mut time, mut places) = (None, Vec::new());
        for e in Elems(point) {
            let (id, v) = e?;
            match id {
                CUE_TIME => time = Some(be_uint(v)?),
                CUE_TRACK_POSITIONS => {
                    let (mut track, mut at) = (None, None);
                    for e in Elems(v) {
                        let (id, v) = e?;
                        match id {
                            CUE_TRACK => track = Some(be_uint(v)?),
                            CUE_CLUSTER_POSITION => at = Some(be_uint(v)?),
                            _ => {}
                        }
                    }
                    if let (Some(track), Some(at)) = (track, at) {
                        places.push((track, at));
                    }
                }
                _ => {}
            }
        }
        let time = i64::try_from(time.ok_or_else(|| bad("a cue point has no time"))?).map_err(|_| bad("a cue time is out of range"))?;
        for (track, at) in places {
            let at = segment_data.checked_add(at).ok_or_else(|| bad("a cue position is out of range"))?;
            out.push(Cue { time, track, at });
        }
        if out.len() > MAX_CUES {
            return Err(bad("the Cues list more places than IronAV uses"));
        }
    }
    Ok(out)
}

/// Whether cues are in a shape that can be trusted: each in the file's clusters, and in time order with
/// the places in order too (for each track).
pub(crate) fn plausible(cues: &[Cue], first_cluster: u64, end: u64) -> bool {
    if cues.is_empty() {
        return false;
    }
    let mut last: Vec<(u64, i64, u64)> = Vec::new(); // per track: the previous time and place
    for c in cues {
        if c.at < first_cluster || c.at >= end || c.time < 0 {
            return false;
        }
        match last.iter_mut().find(|l| l.0 == c.track) {
            Some(l) => {
                if c.time < l.1 || c.at < l.2 {
                    return false;
                }
                (l.1, l.2) = (c.time, c.at);
            }
            None => last.push((c.track, c.time, c.at)),
        }
    }
    true
}

/// One reader that the scan and the demuxer of each window take turns with.
struct Shared<R>(Arc<Mutex<R>>);

impl<R> Clone for Shared<R> {
    fn clone(&self) -> Self {
        Shared(self.0.clone())
    }
}

impl<R: Read> Read for Shared<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).read(buf)
    }
}

impl<R: Seek> Seek for Shared<R> {
    fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
        self.0.lock().unwrap_or_else(std::sync::PoisonError::into_inner).seek(pos)
    }
}

impl<R: Read + Seek> Scan<R> {
    /// Take the Chapters or Attachments the SeekHead places past the first cluster.
    fn level1_at(&mut self, at: u64) -> Result<()> {
        self.r.seek(SeekFrom::Start(at))?;
        self.pos = at;
        self.pending = None;
        let (Some(id), Some(size)) = (self.id()?, self.size()?) else { return Ok(()) };
        if matches!(id, CHAPTERS | ATTACHMENTS) {
            if let Some(end) = self.end_of(size) {
                self.level1(id, size, end)?;
            }
        }
        Ok(())
    }

    /// The Cues element at `at`.
    fn read_cues(&mut self, at: u64) -> Option<Vec<Cue>> {
        self.r.seek(SeekFrom::Start(at)).ok()?;
        self.pos = at;
        self.pending = None;
        if self.id().ok()?? != CUES {
            return None;
        }
        let size = self.size().ok()??;
        if size > MAX_CUES_BYTES || self.pos.checked_add(size)? > self.len {
            return None;
        }
        let body = self.body(size).ok()?;
        parse_cues(&body, self.segment_data).ok()
    }

    /// Whether a cluster starts at each of `places`.
    fn clusters_at(&mut self, places: &[u64]) -> bool {
        places.iter().all(|&at| {
            let mut id = [0u8; 4];
            self.r.seek(SeekFrom::Start(at)).is_ok() && self.r.read_exact(&mut id).is_ok() && id == CLUSTER.to_be_bytes()
        })
    }
}

/// Open a Matroska file for reading, without indexing all of it when the Cues allow. What comes back
/// plays the same as what [`open`](super::open) gives, to within what the module comment says.
pub fn open_indexed<R: Read + Seek + Send + 'static>(mut r: R) -> Result<Box<dyn Demuxer>> {
    let len = r.seek(SeekFrom::End(0))?;
    r.seek(SeekFrom::Start(0))?;
    let shared = Shared(Arc::new(Mutex::new(r)));
    match Lazy::new(shared.clone(), len) {
        Some(l) => Ok(Box::new(l)),
        None => Ok(Box::new(open(shared)?)),
    }
}

struct Lazy<R: Read + Seek + Send> {
    scan: Scan<R>,
    reader: R,
    name: &'static str,
    mk: Vec<MkTrack>,
    streams: Vec<StreamInfo>,
    chapters: Vec<ironav_core::Chapter>,
    attachments: Vec<ironav_core::Attachment>,
    unsupported: Vec<String>,
    /// The cues of the track seeks go by, in time order.
    cues: Vec<Cue>,
    first_cluster: u64,
    /// The window being read, and where the one after it starts.
    inner: Option<Mp4Demuxer<BufReader<R>>>,
    next: u64,
    exhausted: bool,
    discarded: Vec<bool>,
    vp9: Option<(u8, u8)>,
    /// A window covers this many ticks at least.
    span: i64,
    scale: u64,
}

impl<R: Read + Seek + Send + Clone> Lazy<R> {
    /// `None` when the file lacks what this needs, or has it wrong; the caller reads the whole file then.
    fn new(reader: R, len: u64) -> Option<Self> {
        let mut scan = Scan::new(reader.clone(), len);
        scan.stop_at_cluster = true;
        scan.file().ok()?;
        let first_cluster = scan.first_cluster.filter(|_| !scan.truncated)?;
        let duration = scan.duration?;
        let mk = parse_tracks(&scan.tracks.take()?).ok()?;

        let seg = scan.segment_data;
        let cues_at = scan.seek_entries.iter().find(|e| e.0 == CUES).and_then(|e| seg.checked_add(e.1))?;
        let mut cues = scan.read_cues(cues_at)?;
        if !plausible(&cues, first_cluster, len) {
            return None;
        }
        // the cues of the track a seek goes by: the first video track, or else the first track
        let by = mk.iter().find(|t| t.kind == 1).or(mk.first())?.number;
        if cues.iter().any(|c| c.track == by) {
            cues.retain(|c| c.track == by);
        }
        cues.sort_by_key(|c| c.time);
        let mut spots: Vec<u64> = (0..SPOT_CHECKS.min(cues.len())).map(|i| cues[i * (cues.len() - 1) / (SPOT_CHECKS - 1).max(1)].at).collect();
        spots.push(first_cluster);
        if !scan.clusters_at(&spots) {
            return None;
        }
        // Chapters and attachments that the SeekHead places after the clusters (they are not read otherwise)
        let later: Vec<u64> = scan.seek_entries.iter().filter(|e| matches!(e.0, CHAPTERS | ATTACHMENTS)).filter_map(|e| seg.checked_add(e.1)).filter(|&a| a > first_cluster && a < len).collect();
        for at in later {
            let _ = scan.level1_at(at); // damaged ones are left out, as when the file is read whole
        }

        let scale = scan.scale;
        let span = (WINDOW_SECS * 1e9 / scale as f64) as i64;
        let mut l = Lazy {
            name: if scan.doc_type == "webm" { "webm" } else { "matroska" },
            chapters: scan.chapters.as_deref().and_then(|c| parse_chapters(c).ok()).unwrap_or_default(),
            attachments: std::mem::take(&mut scan.attachments),
            scan,
            reader,
            mk,
            streams: Vec::new(),
            unsupported: Vec::new(),
            cues,
            first_cluster,
            inner: None,
            next: first_cluster,
            exhausted: false,
            discarded: Vec::new(),
            vp9: None,
            span,
            scale,
        };
        // the first window gives the streams
        l.load(first_cluster, None).ok()?;
        let inner = l.inner.as_ref()?;
        if inner.streams().is_empty() {
            return None;
        }
        let secs = duration * scale as f64 / 1e9;
        l.streams = inner.streams().iter().cloned().map(|mut s| {
            s.duration = Some((secs / s.time_base.as_f64()).round() as i64);
            s
        }).collect();
        l.discarded = vec![false; l.streams.len()];
        Some(l)
    }

    /// Index the clusters from `from` on, and make them the window being read.
    fn load(&mut self, from: u64, until: Option<i64>) -> Result<()> {
        self.inner = None; // its reader's buffer is stale once the scan has moved the file
        self.scan.window(from, self.scan.len, self.span, until)?;
        // a truncated last cluster ends the file, wherever the scan stopped in it
        self.exhausted = self.scan.truncated || self.scan.pos <= from || self.scan.pos >= self.scan.len;
        self.next = self.scan.pos;
        let (tracks, samples, unsupported, vp9) = build_tracks(&mut self.scan, &self.mk, self.vp9)?;
        self.vp9 = vp9;
        self.unsupported = unsupported;
        let mut d = Mp4Demuxer::from_parts(BufReader::with_capacity(64 << 10, self.reader.clone()), self.name, tracks, samples);
        for (i, _) in self.discarded.iter().enumerate().filter(|(_, &x)| x) {
            d.discard(i);
        }
        self.inner = Some(d);
        Ok(())
    }
}

impl<R: Read + Seek + Send + Clone> Demuxer for Lazy<R> {
    fn format_name(&self) -> &'static str {
        self.name
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn discard(&mut self, stream: usize) {
        if let Some(d) = self.discarded.get_mut(stream) {
            *d = true;
        }
        if let Some(i) = &mut self.inner {
            i.discard(stream);
        }
    }

    fn chapters(&self) -> Vec<ironav_core::Chapter> {
        self.chapters.clone()
    }

    fn attachments(&self) -> Vec<ironav_core::Attachment> {
        self.attachments.clone()
    }

    fn unsupported_streams(&self) -> Vec<String> {
        self.unsupported.clone()
    }

    fn read_packet(&mut self) -> Result<Option<Packet>> {
        loop {
            if let Some(d) = &mut self.inner {
                if let Some(p) = d.read_packet()? {
                    return Ok(Some(p));
                }
            }
            if self.exhausted {
                return Ok(None);
            }
            self.load(self.next, None)?;
        }
    }

    fn seek(&mut self, seconds: f64) -> Result<f64> {
        let t = seconds.max(0.0);
        let tc = (t * 1e9 / self.scale as f64).min(i64::MAX as f64 / 2.0) as i64;
        let from = match self.cues.partition_point(|c| c.time <= tc) {
            0 => self.first_cluster,
            n => self.cues[n - 1].at,
        };
        self.load(from, Some(tc))?;
        self.inner.as_mut().expect("a window was just loaded").seek(t)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mkvwrite::MatroskaWriter;
    use ironav_core::{Muxer, PixelFormat, Rational};
    use std::io::Cursor;

    /// A file of `secs` seconds of fake H.264 at 25 fps, a key frame every `gop` frames, with AAC beside it.
    fn file(secs: i64, gop: i64) -> Vec<u8> {
        struct Out(Arc<Mutex<Cursor<Vec<u8>>>>);
        impl std::io::Write for Out {
            fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
                self.0.lock().unwrap().write(b)
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        impl Seek for Out {
            fn seek(&mut self, p: SeekFrom) -> std::io::Result<u64> {
                self.0.lock().unwrap().seek(p)
            }
        }
        let sink = Arc::new(Mutex::new(Cursor::new(Vec::new())));
        let mut m = MatroskaWriter::new(Box::new(Out(sink.clone())));
        let video = StreamInfo {
            index: 0,
            codec: CodecId::H264,
            time_base: Rational::new(1, 25),
            duration: None,
            bits: None,
            extradata: Vec::new(),
            lead_in: 0,
            params: StreamParams::Video { width: 64, height: 48, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(25, 1) },
        };
        let audio = StreamInfo {
            index: 1,
            codec: CodecId::Aac,
            time_base: Rational::new(1, 44100),
            duration: None,
            bits: None,
            extradata: vec![0x12, 0x10],
            lead_in: 0,
            params: StreamParams::Audio { sample_rate: 44100, channels: 2 },
        };
        m.write_header(&[video, audio]).unwrap();
        let sc = [0, 0, 0, 1];
        let idr = [&sc[..], &[0x67, 0x42, 0, 0x1E, 0xAA], &sc, &[0x68, 0xCE], &sc, &[0x65, 0x88, 1, 2, 3]].concat();
        let p = [&sc[..], &[0x41, 0x9A, 4, 5]].concat();
        for i in 0..secs * 25 {
            let data = if i % gop == 0 { idr.clone() } else { p.clone() };
            m.write_packet(&Packet { stream: 0, pts: i, duration: 1, data }).unwrap();
            // the audio that goes with this second of video, as it comes
            if i % 25 == 0 {
                for k in 0..43 {
                    let n = i / 25 * 43 + k;
                    m.write_packet(&Packet { stream: 1, pts: n * 1024, duration: 1024, data: vec![(n % 251) as u8; 12] }).unwrap();
                }
            }
        }
        m.finish().unwrap();
        let bytes = sink.lock().unwrap().get_ref().clone();
        bytes
    }

    fn all(d: &mut dyn Demuxer) -> Vec<(usize, i64, Vec<u8>)> {
        let mut v = Vec::new();
        while let Some(p) = d.read_packet().unwrap() {
            v.push((p.stream, p.pts, p.data));
        }
        v
    }

    #[test]
    fn a_file_with_cues_is_read_a_window_at_a_time_and_gives_the_same_packets() {
        let bytes = file(40, 50);
        let mut whole = open(Cursor::new(bytes.clone())).unwrap();
        let mut lazy = open_indexed(Cursor::new(bytes)).unwrap();
        assert_eq!(lazy.format_name(), "matroska");
        assert_eq!(lazy.streams().len(), 2);
        // the Info's duration (ms rounded) against the frames'
        let (a, b) = (whole.streams()[0].duration.unwrap(), lazy.streams()[0].duration.unwrap());
        assert!((a - b).abs() <= 2, "{a} {b}");
        let (w, l) = (all(&mut whole), all(&mut *lazy));
        assert_eq!(w.len(), l.len());
        // the same packets, as sets: the order within a moment may differ, and the audio is timed by the window
        let key = |v: &[(usize, i64, Vec<u8>)], s: usize| v.iter().filter(|p| p.0 == s).map(|p| (p.1, p.2.clone())).collect::<Vec<_>>();
        assert_eq!(key(&w, 0), key(&l, 0));
        assert_eq!(key(&w, 1).iter().map(|p| &p.1).collect::<Vec<_>>(), key(&l, 1).iter().map(|p| &p.1).collect::<Vec<_>>());
    }

    #[test]
    fn it_does_not_index_the_whole_file_to_open_and_seeks_land_on_the_same_keyframe() {
        let bytes = file(60, 50);
        let mut whole = open(Cursor::new(bytes.clone())).unwrap();
        let mut lazy = open_indexed(Cursor::new(bytes)).unwrap();
        let tb = lazy.streams()[0].time_base.as_f64();
        for t in [0.0, 1.9, 2.0, 2.1, 17.3, 33.99, 59.0, 500.0] {
            let (a, b) = (whole.seek(t).unwrap(), lazy.seek(t).unwrap());
            assert!((a - b).abs() < 1e-6, "seek({t}): whole file {a}, lazy {b}");
            // the first video packet after it is the keyframe
            let first = |d: &mut dyn Demuxer| loop {
                let p = d.read_packet().unwrap().expect("a packet");
                if p.stream == 0 {
                    break (p.pts, p.data.windows(5).any(|w| w == [0x65, 0x88, 1, 2, 3]));
                }
            };
            let (w, l) = (first(&mut whole), first(&mut *lazy));
            assert_eq!(w, l, "seek({t})");
            assert!(l.1 || t > 59.0, "a key frame at {t}");
            assert!(l.0 as f64 * tb <= t.max(0.0) + 1e-9, "the picture is shown after the target");
        }
        // and playing on from a seek crosses into the next window
        lazy.seek(5.0).unwrap();
        let n = all(&mut *lazy).iter().filter(|p| p.0 == 0).count();
        assert_eq!(n, (60 - 4) * 25);
    }

    #[test]
    fn cues_that_lie_are_not_used() {
        let bytes = file(30, 25);
        let expect = all(&mut open(Cursor::new(bytes.clone())).unwrap()).len();
        // a cluster position past the end of the file, one that is not a cluster, and a SeekHead entry with no Cues behind it
        let cues_id = [0x1C, 0x53, 0xBB, 0x6B];
        let cues_at = bytes.windows(4).rposition(|w| w == cues_id).unwrap();

        // the bytes of each cue's CueClusterPosition
        let places: Vec<std::ops::Range<usize>> = {
            let (mut v, at) = (Vec::new(), cues_at + 4);
            let width = bytes[at].leading_zeros() as usize + 1;
            for e in Elems(&bytes[at + width..]) {
                let (id, point) = e.unwrap();
                assert_eq!(id, CUE_POINT);
                for e in Elems(point) {
                    let (id, pos) = e.unwrap();
                    if id == CUE_TRACK_POSITIONS {
                        for e in Elems(pos) {
                            let (id, value) = e.unwrap();
                            if id == CUE_CLUSTER_POSITION {
                                let from = value.as_ptr() as usize - bytes.as_ptr() as usize;
                                v.push(from..from + value.len());
                            }
                        }
                    }
                }
            }
            v
        };
        assert!(places.len() >= 20);
        let mut out_of_range = bytes.clone();
        let last = places.last().unwrap().clone();
        out_of_range[last].fill(0xFF);
        let mut off_a_cluster = bytes.clone();
        let first = places[0].clone();
        off_a_cluster[first.end - 1] = off_a_cluster[first.end - 1].wrapping_add(3); // a few bytes into the cluster
        let mut no_cues = bytes.clone();
        let seek_id = bytes.windows(4).position(|w| w == cues_id).unwrap(); // in the SeekHead
        no_cues[seek_id..seek_id + 4].copy_from_slice(&[0x12, 0x54, 0xC3, 0x67]);
        let mut garbage = bytes.clone();
        for b in &mut garbage[cues_at + 6..cues_at + 40] {
            *b = 0xAA;
        }

        for (what, bad) in [("out of range", out_of_range), ("not a cluster", off_a_cluster), ("none named", no_cues), ("garbage", garbage)] {
            let mut d = open_indexed(Cursor::new(bad)).unwrap_or_else(|e| panic!("{what}: {}", e.message));
            assert_eq!(all(&mut *d).len(), expect, "{what}: every packet is still read");
            // the file was read whole, so its streams carry the duration of their frames
            assert_eq!(d.streams().len(), 2, "{what}");
        }
    }

    #[test]
    fn cues_are_parsed_and_checked() {
        let el = |id: u32, body: &[u8]| {
            let mut v: Vec<u8> = id.to_be_bytes().iter().copied().skip_while(|&b| b == 0).collect();
            v.push(0x80 | body.len() as u8);
            v.extend_from_slice(body);
            v
        };
        let point = |time: u8, track: u8, at: u8| el(CUE_POINT, &[el(CUE_TIME, &[time]), el(CUE_TRACK_POSITIONS, &[el(CUE_TRACK, &[track]), el(CUE_CLUSTER_POSITION, &[at])].concat())].concat());
        let body = [point(0, 1, 10), point(5, 1, 40), point(9, 1, 80)].concat();
        let cues = parse_cues(&body, 100).unwrap();
        assert_eq!(cues, vec![Cue { time: 0, track: 1, at: 110 }, Cue { time: 5, track: 1, at: 140 }, Cue { time: 9, track: 1, at: 180 }]);
        assert!(plausible(&cues, 110, 1000));
        assert!(!plausible(&cues, 120, 1000), "before the first cluster");
        assert!(!plausible(&cues, 110, 180), "past the end");
        assert!(!plausible(&[], 0, 10));
        let swapped = [cues[1], cues[0]];
        assert!(!plausible(&swapped, 0, 1000), "out of time order");
        // no time, a truncated body
        assert!(parse_cues(&el(CUE_POINT, &el(CUE_TRACK_POSITIONS, &[])), 0).is_err());
        assert!(parse_cues(&body[..body.len() - 3], 0).is_err());
    }
}
