//! A live session's transcode sink: the first video stream decoded, scaled if asked, and encoded
//! again as low-latency H.264 (no B pictures, a keyframe a second), written with the recorder to a
//! file or HLS segments, the audio copied beside it.
//!
//! The work runs on its own thread behind a bounded queue, so a slow encoder never holds up the
//! session: when the queue is full, video is dropped up to the next keyframe (a decoder cannot
//! start anywhere else) and counted. The file begins with the first encoded picture.

use crate::record::{RecordOptions, Recorder};
use ironav_codec::h264::H264Encoder;
use ironav_core::{CodecId, Decoder, Encoder, Error, ErrorCode, Filter, Frame, MediaType, Packet, PixelFormat, Result, StreamInfo, StreamParams};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::sync::mpsc::{sync_channel, Receiver, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Packets waiting for the transcoder, at most.
const QUEUE: usize = 256;

/// The least time between two IDR pictures made because a destination asked for a keyframe: a viewer that
/// keeps asking gets one every half second, not a keyframe for every picture.
const IDR_INTERVAL: Duration = Duration::from_millis(500);

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TranscodeOptions {
    /// A file (`.ts`, `.mp4`, `.mkv`) or an `.m3u8` path for HLS, as for recording.
    pub output: String,
    /// Width of the encoded picture (even); the height keeps the aspect ratio. The source's size
    /// when absent.
    #[serde(default)]
    pub width: Option<u32>,
    /// Average video bit rate in kbit/s; a constant quantiser (`qp`, 23 by default) when absent.
    #[serde(default)]
    pub bitrate_kbps: Option<u32>,
    #[serde(default)]
    pub qp: Option<u8>,
    /// `copy` (the default: the audio as it arrives, when the output can hold it) or `none`.
    #[serde(default)]
    pub audio: Option<String>,
    #[serde(default)]
    pub segment_seconds: Option<f64>,
    #[serde(default)]
    pub keep_segments: Option<usize>,
    #[serde(default)]
    pub overwrite: bool,
}

impl TranscodeOptions {
    fn record(&self) -> RecordOptions {
        RecordOptions { path: self.output.clone(), segment_seconds: self.segment_seconds, keep_segments: self.keep_segments, overwrite: self.overwrite }
    }

    /// Checks what can be checked before any media arrives.
    pub fn check(&self) -> Result<()> {
        if self.width.is_some_and(|w| w < 16 || w % 2 != 0 || w > 16384) {
            return Err(Error::invalid_arg("width must be even, from 16 to 16384"));
        }
        if self.bitrate_kbps.is_some() && self.qp.is_some() {
            return Err(Error::invalid_arg("give bitrate_kbps or qp, not both"));
        }
        if self.qp.is_some_and(|q| q > 51) {
            return Err(Error::invalid_arg("qp must be from 0 to 51"));
        }
        if self.bitrate_kbps.is_some_and(|b| b == 0 || b > 1_000_000) {
            return Err(Error::invalid_arg("bitrate_kbps must be from 1 to 1000000"));
        }
        if let Some(a) = &self.audio {
            if !matches!(a.as_str(), "copy" | "none") {
                return Err(Error::invalid_arg(format!("unknown audio choice '{a}'")).alternatives(["copy", "none"]));
            }
        }
        self.record().check()
    }
}

/// What the session thread holds: the queue to the transcoder and what it last said.
pub struct Transcoder {
    opts: TranscodeOptions,
    tx: Option<SyncSender<(Packet, bool)>>,
    thread: Option<JoinHandle<()>>,
    status: Arc<Mutex<Value>>,
    video: Option<usize>,
    /// Video packets are dropped until the next keyframe (the queue was full, or it started on a
    /// running session).
    dropping: bool,
    late: bool,
    dropped: u64,
    /// Where the output is also sent, as MPEG-TS, when the session publishes.
    publisher: Option<crate::publish::Publisher>,
}

impl Transcoder {
    pub fn new(opts: TranscodeOptions) -> Self {
        let status = Arc::new(Mutex::new(json!({ "output": opts.output, "state": "waiting" })));
        Transcoder { opts, tx: None, thread: None, status, video: None, dropping: false, late: false, dropped: 0, publisher: None }
    }

    /// Sends what the transcoder writes to `p` as well: the session's publisher. It is told the
    /// output's streams when the first picture has been encoded, and finished with the output. A keyframe
    /// the destination asks for is made as an IDR picture, at most one per half second.
    pub fn publish_to(&mut self, p: crate::publish::Publisher) {
        self.publisher = Some(p);
    }

    /// [`start`](Self::start) on a session already running: decoding begins at the next video
    /// keyframe, and the output's timestamps start at zero with its first picture.
    pub fn start_late(&mut self, streams: &[StreamInfo]) {
        (self.late, self.dropping) = (true, true);
        self.start(streams);
    }

    /// The streams are known: starts the transcoder's thread.
    pub fn start(&mut self, streams: &[StreamInfo]) {
        if self.thread.is_some() {
            return;
        }
        self.video = streams.iter().position(|s| s.params.media_type() == MediaType::Video);
        let Some(v) = self.video else {
            *lock(&self.status) = json!({ "output": self.opts.output, "state": "failed", "error": Error::new(ErrorCode::NotFound, "the source has no video to transcode") });
            return;
        };
        let (tx, rx) = sync_channel(QUEUE);
        let mut work = Work::new(self.opts.clone(), streams, v, self.status.clone());
        work.late = self.late;
        work.publisher = self.publisher.take();
        if let Some(p) = &work.publisher {
            p.set_transcoded();
        }
        self.tx = Some(tx);
        let status = self.status.clone();
        let output = self.opts.output.clone();
        let spawned = std::thread::Builder::new().name("ironav-transcode".into()).spawn(move || {
            // a panic is reported, not left looking like a transcode still running
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work.run(rx))).is_err() {
                *lock(&status) = json!({ "output": output, "state": "failed", "error": Error::new(ErrorCode::Internal, "the transcoder stopped unexpectedly") });
            }
        });
        match spawned {
            Ok(t) => self.thread = Some(t),
            Err(e) => {
                self.tx = None;
                *lock(&self.status) = json!({ "output": self.opts.output, "state": "failed", "error": Error::new(ErrorCode::Internal, format!("the transcoder could not start: {e}")) });
            }
        }
    }

    /// One packet of the session; `key` says whether a decoder can start from it.
    pub fn packet(&mut self, p: &Packet, key: bool) {
        let Some(tx) = &self.tx else { return };
        let video = Some(p.stream) == self.video;
        if video && self.dropping {
            if !key {
                // waiting for the first keyframe of a late start is not falling behind
                if !self.late {
                    self.dropped += 1;
                }
                return;
            }
            (self.dropping, self.late) = (false, false);
        }
        match tx.try_send((p.clone(), key)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) if video => {
                self.dropping = true;
                self.dropped += 1;
            }
            // audio waits less than video takes to encode; it is dropped too rather than block
            Err(TrySendError::Full(_)) => {}
            Err(TrySendError::Disconnected(_)) => self.tx = None,
        }
    }

    /// What it has done: its own figures and its recording's.
    pub fn status(&self) -> Value {
        let mut v = lock(&self.status).clone();
        v["dropped_packets"] = json!(self.dropped);
        v
    }

    /// The session is over: the encoder is drained and the output finished.
    pub fn finish(&mut self) {
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for Transcoder {
    fn drop(&mut self) {
        self.finish();
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// The transcoder's thread: decoder, filters, encoder and recorder.
struct Work {
    opts: TranscodeOptions,
    streams: Vec<StreamInfo>,
    video: usize,
    /// Input stream to output stream, for what is written (video, and audio copied).
    map: Vec<Option<usize>>,
    decoder: Option<Box<dyn Decoder>>,
    /// To yuv420p, and to the output's size: made when a picture first needs them, since a
    /// source may change its size or format mid-stream while the output keeps the first.
    convert: Option<Box<dyn Filter>>,
    scale: Option<Box<dyn Filter>>,
    encoder: Option<H264Encoder>,
    rec: Recorder,
    started: bool,
    status: Arc<Mutex<Value>>,
    pictures_in: u64,
    pictures_out: u64,
    size: Option<(u32, u32)>,
    late: bool,
    error: Option<Error>,
    publisher: Option<crate::publish::Publisher>,
    idr: IdrClock,
}

/// When the last IDR picture made for a destination's request was made, so the next one waits for
/// [`IDR_INTERVAL`].
#[derive(Default)]
struct IdrClock {
    last: Option<Instant>,
}

impl IdrClock {
    /// Whether an IDR picture may be made at `now`; if so, it is recorded as made then.
    fn allow(&mut self, now: Instant) -> bool {
        match self.last {
            Some(t) if now.saturating_duration_since(t) < IDR_INTERVAL => false,
            _ => {
                self.last = Some(now);
                true
            }
        }
    }
}

impl Work {
    fn new(opts: TranscodeOptions, streams: &[StreamInfo], video: usize, status: Arc<Mutex<Value>>) -> Work {
        let mut map = vec![None; streams.len()];
        let mut n = 0;
        let fmt = if opts.output.to_ascii_lowercase().ends_with(".m3u8") { ironav_formats::find_format("mpegts") } else { ironav_formats::format_for_path(std::path::Path::new(&opts.output)) };
        let holds = |c: CodecId| fmt.as_ref().is_ok_and(|f| f.codecs.contains(&c));
        let copy_audio = opts.audio.as_deref() != Some("none");
        for (i, s) in streams.iter().enumerate() {
            let take = i == video || (copy_audio && s.params.media_type() == MediaType::Audio && holds(s.codec));
            if take {
                map[i] = Some(n);
                n += 1;
            }
        }
        let rec = Recorder::new(opts.record());
        Work { opts, streams: streams.to_vec(), video, map, decoder: None, convert: None, scale: None, encoder: None, rec, started: false, status, pictures_in: 0, pictures_out: 0, size: None, late: false, error: None, publisher: None, idr: IdrClock::default() }
    }

    fn run(mut self, rx: Receiver<(Packet, bool)>) {
        let info = &self.streams[self.video];
        // live: each picture as soon as it can be shown
        match crate::policy::new_decoder_with(info, &ironav_codec::DecoderOptions { low_delay: true }) {
            Ok(d) => self.decoder = Some(d),
            Err(e) => self.error = Some(e),
        }
        self.publish("running");
        for (p, key) in rx {
            if self.error.is_some() {
                continue;
            }
            if let Err(e) = self.take(p, key) {
                self.error = Some(e);
                self.publish("failed");
            }
            if self.pictures_in.is_multiple_of(25) {
                self.publish("running");
            }
        }
        // drained: whatever the decoder and encoder still hold
        if self.error.is_none() {
            let r = (|| -> Result<()> {
                let frames = self.decoder.as_mut().map_or(Ok(Vec::new()), |d| d.flush())?;
                self.encode(frames)?;
                if let Some(e) = self.encoder.as_mut() {
                    let pkts = e.flush()?;
                    self.write_video(pkts);
                }
                Ok(())
            })();
            if let Err(e) = r {
                self.error = Some(e);
            }
        }
        self.rec.finish(0.0);
        if let Some(pb) = self.publisher.as_mut() {
            pb.finish();
        }
        self.publish("finished");
    }

    fn take(&mut self, p: Packet, key: bool) -> Result<()> {
        if p.stream != self.video {
            // audio goes once the file has begun, with the first picture
            if self.started && self.map.get(p.stream).copied().flatten().is_some() {
                let mut q = p;
                let t = q.pts as f64 * self.streams[q.stream].time_base.as_f64();
                q.stream = self.map[q.stream].unwrap();
                self.rec.packet(&q, t, key);
                if let Some(pb) = self.publisher.as_mut() {
                    pb.packet(&q, key);
                }
            }
            return Ok(());
        }
        let Some(dec) = self.decoder.as_mut() else { return Ok(()) };
        // a damaged picture is skipped, as the analysis does
        let frames = dec.decode(&p).unwrap_or_default();
        self.encode(frames)
    }

    fn encode(&mut self, frames: Vec<Frame>) -> Result<()> {
        for f in frames {
            let Frame::Video(v) = f else { continue };
            self.pictures_in += 1;
            if self.encoder.is_none() {
                self.open(v.width, v.height)?;
            }
            let (ow, oh) = self.size.unwrap();
            let mut cur = vec![Frame::Video(v)];
            if matches!(&cur[0], Frame::Video(v) if v.pix_fmt != PixelFormat::Yuv420p) {
                if self.convert.is_none() {
                    let mut a = Map::new();
                    a.insert("pix_fmt".into(), json!("yuv420p"));
                    self.convert = Some(ironav_codec::build_filter("format", &a)?);
                }
                let f = self.convert.as_mut().unwrap();
                cur = cur.into_iter().map(|fr| f.process(fr)).collect::<Result<Vec<_>>>()?.into_iter().flatten().collect();
            }
            if cur.iter().any(|fr| matches!(fr, Frame::Video(v) if (v.width, v.height) != (ow, oh))) {
                if self.scale.is_none() {
                    let mut a = Map::new();
                    a.insert("width".into(), json!(ow));
                    a.insert("height".into(), json!(oh));
                    self.scale = Some(ironav_codec::build_filter("scale", &a)?);
                }
                let f = self.scale.as_mut().unwrap();
                cur = cur.into_iter().map(|fr| f.process(fr)).collect::<Result<Vec<_>>>()?.into_iter().flatten().collect();
            }
            if let Some(Frame::Video(v)) = cur.iter().find(|fr| !matches!(fr, Frame::Video(v) if v.pix_fmt == PixelFormat::Yuv420p)) {
                return Err(Error::new(ErrorCode::UnsupportedFormat, format!("{:?} pictures cannot be encoded as H.264", v.pix_fmt)));
            }
            self.honour_keyframe_request(Instant::now());
            let mut pkts = Vec::new();
            for fr in &cur {
                pkts.extend(self.encoder.as_mut().unwrap().encode(fr)?);
            }
            self.write_video(pkts);
        }
        Ok(())
    }

    /// Opens the encoder for the output size the first picture (`w` by `h`) gives, and the output.
    fn open(&mut self, w: u32, h: u32) -> Result<()> {
        // the encoder takes even sizes; the height follows the width's scale
        let ow = self.opts.width.unwrap_or(w & !1).max(2);
        let oh = if self.opts.width.is_some() { ((h as u64 * ow as u64 / w.max(1) as u64) as u32 + 1) & !1 } else { h & !1 }.max(2);
        let rate = match self.streams[self.video].params {
            StreamParams::Video { frame_rate, .. } if frame_rate.num > 0 && frame_rate.den > 0 => frame_rate,
            _ => ironav_core::Rational::new(25, 1),
        };
        let params = StreamParams::Video { width: ow, height: oh, pix_fmt: PixelFormat::Yuv420p, frame_rate: rate };
        let qp = if self.opts.bitrate_kbps.is_none() { Some(self.opts.qp.unwrap_or(23)) } else { None };
        self.encoder = Some(H264Encoder::new(&params, qp, self.opts.bitrate_kbps.map(|k| k.saturating_mul(1000)))?.low_latency());
        self.size = Some((ow, oh));
        // the output's streams: the H.264 picture, and the audio copied
        let mut out = Vec::new();
        for (i, s) in self.streams.iter().enumerate() {
            let Some(k) = self.map[i] else { continue };
            let mut s = s.clone();
            s.index = k;
            if i == self.video {
                (s.codec, s.params, s.extradata, s.bits, s.lead_in) = (CodecId::H264, params.clone(), Vec::new(), None, 0);
            }
            out.push(s);
        }
        if self.late {
            self.rec.start_late(&out);
        } else {
            self.rec.start(&out);
        }
        if let Some(pb) = self.publisher.as_mut() {
            pb.start(&out);
        }
        Ok(())
    }

    /// Makes the encoder's next picture an IDR picture when the destination asked for a keyframe and the last
    /// IDR picture made for a request is at least [`IDR_INTERVAL`] back. A request that has to wait stays
    /// pending until it can be made.
    fn honour_keyframe_request(&mut self, now: Instant) {
        let Some(p) = self.publisher.as_ref() else { return };
        if !p.keyframe_wanted() || !self.idr.allow(now) {
            return;
        }
        if let Some(e) = self.encoder.as_mut() {
            e.force_idr();
        }
        p.keyframe_made();
    }

    fn write_video(&mut self, pkts: Vec<Packet>) {
        let tb = self.streams[self.video].time_base.as_f64();
        let out = self.map[self.video].unwrap();
        // a picture lasts a frame interval, in the stream's time base
        let rate = match self.streams[self.video].params {
            StreamParams::Video { frame_rate, .. } if frame_rate.num > 0 && frame_rate.den > 0 => frame_rate.as_f64(),
            _ => 25.0,
        };
        let duration = ((1.0 / rate) / tb).round().max(1.0) as i64;
        for mut p in pkts {
            p.duration = duration;
            self.pictures_out += 1;
            self.started = true;
            let key = ironav_formats::is_keyframe(CodecId::H264, &p.data).unwrap_or(false);
            let t = p.pts as f64 * tb;
            p.stream = out;
            self.rec.packet(&p, t, key);
            if let Some(pb) = self.publisher.as_mut() {
                pb.packet(&p, key);
            }
        }
    }

    fn publish(&self, state: &str) {
        let rec = self.rec.status();
        let err = match &self.error {
            Some(e) => json!(e),
            None => rec["error"].clone(),
        };
        *lock(&self.status) = json!({
            "output": self.opts.output,
            "state": if err.is_null() { state } else { "failed" },
            "video": { "codec": "h264", "encoder": "software", "width": self.size.map(|s| s.0), "height": self.size.map(|s| s.1) },
            "pictures_in": self.pictures_in,
            "pictures_out": self.pictures_out,
            "record": rec,
            "error": err,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::publish::{check, Publisher};
    use ironav_core::{Rational, VideoFrame};

    #[test]
    fn idr_pictures_for_requests_come_at_most_once_per_half_second() {
        let mut clock = IdrClock::default();
        let t0 = Instant::now();
        assert!(clock.allow(t0), "the first request is made at once");
        assert!(!clock.allow(t0 + Duration::from_millis(1)));
        assert!(!clock.allow(t0 + Duration::from_millis(499)));
        assert!(clock.allow(t0 + Duration::from_millis(500)), "half a second after the last one");
        // measured from the picture made, not from the request that waited
        assert!(!clock.allow(t0 + Duration::from_millis(900)));
        assert!(clock.allow(t0 + Duration::from_millis(1000)));
    }

    #[test]
    fn a_waiting_keyframe_request_becomes_an_idr_picture_once_the_clock_allows() {
        ironav_live::net::allow_loopback_for_tests();
        let url = "srt://127.0.0.1:9?passphrase=0123456789abcdef";
        let publisher = Publisher::new(url, check(url).unwrap());
        let cell = publisher.cell();
        publisher.set_transcoded();
        let opts: TranscodeOptions = serde_json::from_value(json!({ "output": "unused.ts" })).unwrap();
        let mut work = Work::new(opts, &[], 0, Arc::new(Mutex::new(Value::Null)));
        work.publisher = Some(publisher);
        let params = StreamParams::Video { width: 64, height: 48, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(25, 1) };
        work.encoder = Some(H264Encoder::new(&params, Some(26), None).unwrap().low_latency());
        let frame = |t: i64| Frame::Video(VideoFrame { width: 64, height: 48, pix_fmt: PixelFormat::Yuv420p, pts: t, planes: vec![vec![90; 64 * 48], vec![128; 32 * 24], vec![128; 32 * 24]] });
        // one picture through the encoder as the transcoder does it, and whether it came out as an IDR picture
        let code = |work: &mut Work, t: i64, now: Instant| -> bool {
            work.honour_keyframe_request(now);
            let pkts = work.encoder.as_mut().unwrap().encode(&frame(t)).unwrap();
            pkts.iter().any(|p| ironav_formats::is_keyframe(CodecId::H264, &p.data) == Some(true))
        };
        let t0 = Instant::now();
        assert!(code(&mut work, 0, t0), "the first picture is an IDR picture anyway");
        // a request is made into the next picture, as no IDR picture was made for one before
        cell.keyframe_asked();
        assert!(code(&mut work, 1, t0), "the request is made at once");
        // a second request within half a second waits: the picture is coded as P
        cell.keyframe_asked();
        assert!(!code(&mut work, 2, t0 + Duration::from_millis(100)));
        assert!(!code(&mut work, 3, t0 + Duration::from_millis(499)), "still waiting");
        assert!(code(&mut work, 4, t0 + Duration::from_millis(500)), "made once half a second has passed");
        let st = cell.status();
        assert_eq!((st["keyframe_requests"].as_u64(), st["keyframes_forced"].as_u64(), st["keyframes_ignored"].as_u64()), (Some(2), Some(2), Some(0)), "{st}");
    }
}
