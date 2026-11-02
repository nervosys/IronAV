//! Recording a live session as it arrives, without re-encoding: to one file, or (for a `.m3u8`
//! path) to MPEG-TS segments cut at keyframes, listed in an HLS playlist (RFC 8216) that is
//! replaced whole after every segment, so a player or another agent can follow the recording while
//! it grows.
//!
//! A transport stream is readable up to its last whole packet, so a recording cut off by a crash or
//! a pulled cable still plays; other containers (MP4) are complete only when the session closes.
//! With `keep_segments` the playlist is a sliding window, and a segment is deleted once the
//! playlist no longer lists it; without it the playlist keeps every segment (an EVENT playlist).
//! Either way it ends with `#EXT-X-ENDLIST` when the session does, even after an error. A jump
//! backwards in time (a restarted source) starts a new segment marked as a discontinuity.
//!
//! The recorder belongs to the session's thread, so slow disk writes never hold up the readers of
//! the session; it reports through [`Recorder::status`].

use ironav_core::{Error, ErrorCode, MediaType, Muxer, Packet, Result, StreamInfo};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct RecordOptions {
    pub path: String,
    /// Segment length for an `.m3u8` recording (6 s when absent).
    pub segment_seconds: Option<f64>,
    /// Segments kept on disk and in the playlist; all of them when absent.
    pub keep_segments: Option<usize>,
    pub overwrite: bool,
}

impl RecordOptions {
    fn hls(&self) -> bool {
        Path::new(&self.path).extension().is_some_and(|e| e.eq_ignore_ascii_case("m3u8"))
    }

    /// Checks what can be checked before any media arrives.
    pub fn check(&self) -> Result<()> {
        crate::policy::check_write(&self.path)?;
        let partial = format!("{}.partial", self.path);
        if std::fs::symlink_metadata(&partial).is_ok() {
            return Err(Error::new(ErrorCode::OutputExists, format!("{partial} exists"))
                .hint("it is the playlist's temporary name, left by an earlier run that did not finish (or put there by someone else); remove it and try again"));
        }
        let path = Path::new(&self.path);
        if path.file_stem().is_none_or(|s| s.is_empty()) {
            return Err(Error::invalid_arg(format!("'{}' is not a file path", self.path)));
        }
        let mut outputs = vec![path.to_path_buf()];
        if self.hls() {
            if let Some(s) = self.segment_seconds {
                if !(1.0..=600.0).contains(&s) {
                    return Err(Error::invalid_arg("segment_seconds must be from 1 to 600"));
                }
            }
            if self.keep_segments == Some(0) {
                return Err(Error::invalid_arg("keep_segments must be at least 1"));
            }
            outputs.push(segment_path(&self.path, 0).1);
        } else {
            if self.segment_seconds.is_some() || self.keep_segments.is_some() {
                return Err(Error::invalid_arg("segment_seconds and keep_segments need an .m3u8 recording path"));
            }
            let fmt = ironav_formats::format_for_path(path)?;
            if !fmt.mux {
                return Err(Error::new(ErrorCode::UnsupportedFormat, format!("{} cannot be written", fmt.name)));
            }
        }
        if let Some(p) = outputs.iter().find(|p| p.exists()).filter(|_| !self.overwrite) {
            return Err(Error::new(ErrorCode::OutputExists, format!("{} exists", p.display())).hint("pass overwrite: true to replace the recording"));
        }
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            if !dir.is_dir() {
                return Err(Error::new(ErrorCode::NotFound, format!("the folder {} does not exist", dir.display())));
            }
        }
        Ok(())
    }
}

/// The name and path of segment `index` of the playlist at `playlist`.
fn segment_path(playlist: &str, index: u64) -> (String, PathBuf) {
    let p = Path::new(playlist);
    let stem = p.file_stem().map_or("live".into(), |s| s.to_string_lossy().into_owned());
    let name = format!("{stem}_{index:05}.ts");
    (name.clone(), p.with_file_name(name))
}

struct Segment {
    name: String,
    seconds: f64,
    discontinuity: bool,
}

/// The segment being written.
struct Open {
    start: f64,
    /// Where its media ends so far.
    end: f64,
    discontinuity: bool,
}

pub struct Recorder {
    opts: RecordOptions,
    streams: Vec<StreamInfo>,
    video: Option<usize>,
    mux: Option<Box<dyn Muxer>>,
    /// HLS: the next segment's index, the one being written, and the finished ones listed.
    index: u64,
    open: Option<Open>,
    listed: VecDeque<Segment>,
    first_seq: u64,
    /// The playlist's target duration never shrinks (RFC 8216 4.3.3.1).
    target: u64,
    discontinuity: bool,
    packets: u64,
    bytes: u64,
    error: Option<Error>,
    finished: bool,
    /// Started on a session already running: a single file waits for a video keyframe, and its
    /// timestamps start at zero there (`base`, in seconds, once it has started).
    late: bool,
    base: Option<f64>,
}

impl Recorder {
    pub fn new(opts: RecordOptions) -> Self {
        Recorder {
            opts,
            streams: Vec::new(),
            video: None,
            mux: None,
            index: 0,
            open: None,
            listed: VecDeque::new(),
            first_seq: 0,
            target: 1,
            discontinuity: false,
            packets: 0,
            bytes: 0,
            error: None,
            finished: false,
            late: false,
            base: None,
        }
    }

    /// [`start`](Self::start) on a session already running. A single file begins at the next
    /// video keyframe (any packet, without video) with its timestamps from zero; HLS segments
    /// always begin at a keyframe and keep the session's timestamps.
    pub fn start_late(&mut self, streams: &[StreamInfo]) {
        self.late = true;
        self.start(streams);
    }

    /// The streams are known: checks they fit, and opens the output (HLS waits for a keyframe).
    pub fn start(&mut self, streams: &[StreamInfo]) {
        let fmt = if self.opts.hls() { ironav_formats::find_format("mpegts") } else { ironav_formats::format_for_path(Path::new(&self.opts.path)) };
        let r = fmt.and_then(|fmt| match streams.iter().find(|s| !fmt.codecs.contains(&s.codec)) {
            Some(s) => Err(Error::new(ErrorCode::UnsupportedFormat, format!("a {} recording cannot hold {}", fmt.name, s.codec.name()))
                .hint("record to a container that holds the stream as it arrives (an .m3u8 or .ts recording holds H.264, HEVC, AAC and MP3)")),
            None => Ok(()),
        });
        self.streams = streams.to_vec();
        self.video = streams.iter().position(|s| s.params.media_type() == MediaType::Video);
        if let Err(e) = r.and_then(|_| if self.opts.hls() { Ok(()) } else { self.create(PathBuf::from(&self.opts.path)) }) {
            self.error = Some(e);
        }
    }

    fn create(&mut self, path: PathBuf) -> Result<()> {
        let fmt = if self.opts.hls() { "mpegts" } else { ironav_formats::format_for_path(&path)?.name };
        let mut m = ironav_formats::create_muxer(&path, fmt)?;
        if let Err(e) = m.write_header(&self.streams) {
            drop(m);
            let _ = std::fs::remove_file(&path);
            return Err(e);
        }
        self.mux = Some(m);
        Ok(())
    }

    /// One packet at `t` seconds; `key` says whether a decoder can start from it.
    pub fn packet(&mut self, p: &Packet, t: f64, key: bool) {
        if self.error.is_some() || self.finished || self.streams.is_empty() {
            return;
        }
        if let Err(e) = self.write(p, t, key) {
            self.fail(e);
        }
    }

    /// Stops recording after an error; the segment being written is not listed, so it goes.
    fn fail(&mut self, e: Error) {
        self.mux = None;
        if self.opts.hls() && self.open.take().is_some() {
            let _ = std::fs::remove_file(segment_path(&self.opts.path, self.index).1);
        }
        self.error = Some(e);
    }

    fn write(&mut self, p: &Packet, t: f64, key: bool) -> Result<()> {
        if self.opts.hls() {
            // a segment starts at a video keyframe (or any packet, without video)
            let cut = key && self.video.is_none_or(|v| v == p.stream);
            let len = self.opts.segment_seconds.unwrap_or(6.0);
            let next = match &self.open {
                None => cut,
                Some(o) if cut && t + 1e-6 < o.start => {
                    // time went backwards: what follows does not continue what came before
                    self.discontinuity = true;
                    true
                }
                Some(o) => cut && t - o.start >= len - 1e-6,
            };
            if next {
                if let Some(o) = &self.open {
                    let end = if t >= o.start { t } else { o.end };
                    self.close_segment(end)?;
                }
                self.create(segment_path(&self.opts.path, self.index).1)?;
                self.open = Some(Open { start: t, end: t, discontinuity: std::mem::take(&mut self.discontinuity) });
            } else if self.open.is_none() {
                return Ok(());
            }
        }
        let rebased;
        let p = if self.late && !self.opts.hls() {
            let base = match self.base {
                Some(b) if t + 1e-6 < b => return Ok(()),
                Some(b) => b,
                // without video, any packet can start it
                None if self.video.is_none_or(|v| v == p.stream && key) => *self.base.insert(t),
                None => return Ok(()),
            };
            let tb = self.streams[p.stream].time_base.as_f64();
            rebased = Packet { pts: p.pts - (base / tb).round() as i64, ..p.clone() };
            &rebased
        } else {
            p
        };
        if let Some(m) = self.mux.as_mut() {
            m.write_packet(p)?;
            self.packets += 1;
            self.bytes += p.data.len() as u64;
            let end = t + p.duration as f64 * self.streams[p.stream].time_base.as_f64();
            if let Some(o) = self.open.as_mut() {
                o.end = o.end.max(end);
            }
        }
        Ok(())
    }

    /// Finishes the segment being written (it ends at `end` seconds), lists it, rewrites the
    /// playlist, and then deletes what fell out of the window.
    fn close_segment(&mut self, end: f64) -> Result<()> {
        let (Some(mut m), Some(o)) = (self.mux.take(), self.open.take()) else { return Ok(()) };
        m.finish()?;
        drop(m);
        let (name, _) = segment_path(&self.opts.path, self.index);
        let seconds = (end - o.start).max(0.001);
        self.target = self.target.max(seconds.round() as u64);
        self.listed.push_back(Segment { name, seconds, discontinuity: o.discontinuity });
        self.index += 1;
        let mut gone = Vec::new();
        if let Some(keep) = self.opts.keep_segments {
            while self.listed.len() > keep {
                gone.push(self.listed.pop_front().unwrap().name);
                self.first_seq += 1;
            }
        }
        self.write_playlist(false)?;
        for name in gone {
            let _ = std::fs::remove_file(Path::new(&self.opts.path).with_file_name(name));
        }
        Ok(())
    }

    fn write_playlist(&self, ended: bool) -> Result<()> {
        let mut m = format!("#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:{}\n#EXT-X-MEDIA-SEQUENCE:{}\n", self.target, self.first_seq);
        if self.opts.keep_segments.is_none() {
            m.push_str("#EXT-X-PLAYLIST-TYPE:EVENT\n");
        }
        for s in &self.listed {
            if s.discontinuity {
                m.push_str("#EXT-X-DISCONTINUITY\n");
            }
            m.push_str(&format!("#EXTINF:{:.3},\n{}\n", s.seconds, s.name));
        }
        if ended {
            m.push_str("#EXT-X-ENDLIST\n");
        }
        // replaced whole, so a reader never sees half a playlist
        let tmp = format!("{}.partial", self.opts.path);
        // exclusive: a name that is taken is refused, and not deleted
        let mut f = ironav_core::create_new(Path::new(&tmp))?;
        let r = (|| {
            std::io::Write::write_all(&mut f, m.as_bytes())?;
            drop(f);
            ironav_core::replace_file(Path::new(&tmp), Path::new(&self.opts.path))
        })();
        if r.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        Ok(r?)
    }

    /// The session is over; `end` is how far it got, in seconds (used only if nothing was written).
    pub fn finish(&mut self, end: f64) {
        if self.finished {
            return;
        }
        self.finished = true;
        if self.error.is_some() {
            // what was listed before the error still gets a playlist that says it is complete
            if self.opts.hls() && !self.listed.is_empty() {
                let _ = self.write_playlist(true);
            }
            return;
        }
        if self.streams.is_empty() {
            self.error = Some(Error::new(ErrorCode::NotFound, "nothing was recorded: no media arrived"));
            return;
        }
        let r = if self.opts.hls() {
            let end = self.open.as_ref().map_or(end, |o| o.end);
            self.close_segment(end).and_then(|_| {
                if self.listed.is_empty() {
                    Err(Error::new(ErrorCode::NotFound, "nothing was recorded: no keyframe arrived to start a segment"))
                } else {
                    self.write_playlist(true)
                }
            })
        } else if self.late && self.packets == 0 {
            // only a file this recorder made is removed
            if self.mux.take().is_some() {
                let _ = std::fs::remove_file(&self.opts.path);
            }
            Err(Error::new(ErrorCode::NotFound, "nothing was recorded: no keyframe arrived to start the file"))
        } else {
            self.mux.take().map_or(Ok(()), |mut m| m.finish())
        };
        if let Err(e) = r {
            self.error = Some(e);
        }
    }

    pub fn status(&self) -> Value {
        json!({
            "path": self.opts.path,
            "packets": self.packets,
            "bytes": self.bytes,
            "segments": if self.opts.hls() { json!({ "written": self.index, "listed": self.listed.len(), "first_seq": self.first_seq }) } else { Value::Null },
            "finished": self.finished,
            "started_at": self.base,
            "error": self.error,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironav_core::{CodecId, PixelFormat, Rational, StreamParams};

    fn video() -> StreamInfo {
        StreamInfo {
            index: 0,
            codec: CodecId::H264,
            time_base: Rational::new(1, 10),
            duration: None,
            bits: None,
            extradata: Vec::new(),
            lead_in: 0,
            params: StreamParams::Video { width: 16, height: 16, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(10, 1) },
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("ironav-record-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn hls(dir: &Path, keep: Option<usize>) -> Recorder {
        let opts = RecordOptions { path: dir.join("r.m3u8").to_str().unwrap().into(), segment_seconds: Some(2.0), keep_segments: keep, overwrite: false };
        opts.check().unwrap();
        let mut r = Recorder::new(opts);
        r.start(&[video()]);
        r
    }

    /// Ten pictures a second from `from` for `n` pictures, a keyframe every second.
    fn feed(r: &mut Recorder, from: i64, n: i64) {
        for i in from..from + n {
            let key = i % 10 == 0;
            let data = vec![0, 0, 0, 1, if key { 0x65 } else { 0x41 }, 0x88];
            r.packet(&Packet { stream: 0, pts: i, duration: 1, data }, i as f64 / 10.0, key);
        }
    }

    #[test]
    fn a_restarted_source_starts_a_segment_marked_as_a_discontinuity() {
        let dir = scratch("disc");
        let mut r = hls(&dir, None);
        feed(&mut r, 0, 30);
        feed(&mut r, 0, 30); // the source restarted at zero
        r.finish(0.0);
        assert_eq!(r.error.as_ref().map(|e| e.code), None);
        let m = std::fs::read_to_string(dir.join("r.m3u8")).unwrap();
        let extinf: Vec<&str> = m.lines().filter(|l| l.starts_with("#EXTINF")).collect();
        assert_eq!(extinf, ["#EXTINF:2.000,", "#EXTINF:1.000,", "#EXTINF:2.000,", "#EXTINF:1.000,"], "{m}");
        assert_eq!(m.matches("#EXT-X-DISCONTINUITY").count(), 1);
        assert!(m.contains("#EXT-X-TARGETDURATION:2") && m.ends_with("#EXT-X-ENDLIST\n"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_window_keeps_its_target_duration_and_deletes_only_unlisted_segments() {
        let dir = scratch("window");
        let mut r = hls(&dir, Some(2));
        // one long segment first (no keyframe for 4 s), then short ones
        for i in 0..40 {
            r.packet(&Packet { stream: 0, pts: i, duration: 1, data: vec![0, 0, 0, 1, if i == 0 { 0x65 } else { 0x41 }] }, i as f64 / 10.0, i == 0);
        }
        feed(&mut r, 40, 60);
        r.finish(0.0);
        let m = std::fs::read_to_string(dir.join("r.m3u8")).unwrap();
        assert!(m.contains("#EXT-X-TARGETDURATION:4"), "{m}");
        assert!(m.contains("#EXT-X-MEDIA-SEQUENCE:2"), "{m}");
        let listed: Vec<&str> = m.lines().filter(|l| l.ends_with(".ts")).collect();
        assert_eq!(listed, ["r_00002.ts", "r_00003.ts"]);
        let mut on_disk: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).filter(|n| n.ends_with(".ts")).collect();
        on_disk.sort();
        assert_eq!(on_disk, listed);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nothing_recorded_and_existing_segments_are_errors() {
        let dir = scratch("none");
        let mut r = hls(&dir, None);
        feed(&mut r, 1, 5); // no keyframe
        r.finish(0.0);
        assert_eq!(r.error.as_ref().map(|e| e.code), Some(ErrorCode::NotFound));
        assert!(!dir.join("r.m3u8").exists());
        std::fs::write(dir.join("r_00000.ts"), b"old").unwrap();
        let opts = RecordOptions { path: dir.join("r.m3u8").to_str().unwrap().into(), segment_seconds: None, keep_segments: None, overwrite: false };
        assert_eq!(opts.check().err().map(|e| e.code), Some(ErrorCode::OutputExists));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
