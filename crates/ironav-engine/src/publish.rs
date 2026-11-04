//! Publishing a live session as it runs: the media the session records (or, with a transcode, the
//! transcoder's output) is sent to a destination. To `srt://` it is muxed into MPEG-TS (see
//! `ironav_live::open_sink`): the transport stream a `.ts` recording would hold, cut at no point, so a
//! receiver that joins late starts at the next video keyframe. To `whip://` or `whips://` it goes to a
//! WHIP endpoint as H.264 access units and Opus packets (see `ironav_live::WhipPublisher`): length-prefixed
//! H.264 is converted to Annex B, with the parameter sets before each keyframe; other audio is not sent,
//! and video that is not H.264 ends the publishing.
//!
//! The work runs on its own thread behind a bounded queue, so a slow or lost destination never holds
//! up the session: when the queue is full, video is dropped up to the next keyframe (a decoder cannot
//! start anywhere else) and counted. A destination that fails is reported to the session (as the
//! observations `publish_lost`, `publish_restored` and `publish_failed`) and the session goes on; a
//! caller is opened again with a growing wait between attempts, as a lost network source is, unless
//! the URL says `reconnect=0`. What cannot be put right by trying again (an address the allow-list
//! refuses, a codec a transport stream cannot hold) ends the publishing at once.

use ironav_core::{CodecId, Error, ErrorCode, MediaType, Muxer, Packet, Rational, Result, StreamInfo};
use ironav_live::{open_sink, whip_endpoint, Sink, SinkPlan, WhipOptions, WhipPublisher};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{sync_channel, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// Packets waiting for the publisher, at most.
const QUEUE: usize = 512;
/// The first wait before opening a lost destination again, doubled after each failed attempt up to
/// [`MAX_BACKOFF`]: the source side's.
const FIRST_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(30);
/// Events kept for the session to take; a destination that flaps does not grow them.
const MAX_EVENTS: usize = 64;

/// Checks a destination before a session starts: its scheme and options, FIPS mode's rule about
/// plaintext transports, and the audit trail. The address allow-list applies when it connects.
pub fn check(url: &str) -> Result<SinkPlan> {
    crate::policy::check_publish(url)
}

/// A change in how publishing is going, for the session's log.
#[derive(Debug, Clone)]
pub struct Event {
    /// `publish_lost`, `publish_restored` or `publish_failed`.
    pub kind: &'static str,
    pub data: Value,
}

/// What the publisher's thread tells the session: its status, and the events not yet taken.
pub struct PublishCell {
    status: Mutex<Value>,
    events: Mutex<VecDeque<Event>>,
    /// Packets the session could not queue for the publisher.
    dropped: AtomicU64,
    /// Keyframes the destination asked for (a WHIP server's picture loss indication or full intra request).
    keyframes: AtomicU64,
    /// A keyframe the destination asked for that the transcoder has not made yet.
    idr_wanted: AtomicBool,
    /// The session's output is made by a transcoder, which can make the keyframes asked for.
    transcoded: AtomicBool,
    /// Requests answered with a forced IDR picture.
    keyframes_forced: AtomicU64,
    /// Requests not answered: nothing transcodes, or the request came while one was already waiting, or the
    /// session ended before it was made.
    keyframes_ignored: AtomicU64,
}

impl PublishCell {
    fn new(url: &str) -> PublishCell {
        PublishCell {
            status: Mutex::new(json!({ "url": url, "state": "waiting" })),
            events: Mutex::new(VecDeque::new()),
            dropped: AtomicU64::new(0),
            keyframes: AtomicU64::new(0),
            idr_wanted: AtomicBool::new(false),
            transcoded: AtomicBool::new(false),
            keyframes_forced: AtomicU64::new(0),
            keyframes_ignored: AtomicU64::new(0),
        }
    }

    pub fn status(&self) -> Value {
        let mut v = lock(&self.status).clone();
        v["dropped_packets"] = json!(self.dropped.load(Ordering::Relaxed));
        v["keyframe_requests"] = json!(self.keyframes.load(Ordering::Relaxed));
        v["keyframes_forced"] = json!(self.keyframes_forced.load(Ordering::Relaxed));
        v["keyframes_ignored"] = json!(self.keyframes_ignored.load(Ordering::Relaxed));
        v
    }

    /// The destination asked for a keyframe. The transcoder is asked to make an IDR picture, unless one is
    /// already waiting for it; with nothing transcoding, the request is only counted, as ignored.
    pub(crate) fn keyframe_asked(&self) {
        self.keyframes.fetch_add(1, Ordering::Relaxed);
        if !self.transcoded.load(Ordering::Relaxed) || self.idr_wanted.swap(true, Ordering::Relaxed) {
            self.keyframes_ignored.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// The events since the last call, oldest first.
    pub fn take_events(&self) -> Vec<Event> {
        lock(&self.events).drain(..).collect()
    }

    fn event(&self, kind: &'static str, data: Value) {
        let mut q = lock(&self.events);
        q.push_back(Event { kind, data });
        while q.len() > MAX_EVENTS {
            q.pop_front();
        }
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// What the session thread (or the transcoder's) holds: the queue to the publisher's thread.
pub struct Publisher {
    url: String,
    plan: SinkPlan,
    cell: Arc<PublishCell>,
    tx: Option<SyncSender<(Packet, bool)>>,
    thread: Option<JoinHandle<()>>,
    video: Option<usize>,
    /// Video packets are dropped until the next keyframe (the queue was full).
    dropping: bool,
}

impl Publisher {
    /// `url` has been through [`check`], which gave `plan`.
    pub fn new(url: &str, plan: SinkPlan) -> Publisher {
        let cell = Arc::new(PublishCell::new(&plan.shown));
        Publisher { url: url.to_string(), plan, cell, tx: None, thread: None, video: None, dropping: false }
    }

    /// The status and events, for whoever reports them.
    pub fn cell(&self) -> Arc<PublishCell> {
        self.cell.clone()
    }

    /// The streams are known: starts the publisher's thread.
    pub fn start(&mut self, streams: &[StreamInfo]) {
        if self.thread.is_some() {
            return;
        }
        self.video = streams.iter().position(|s| s.params.media_type() == MediaType::Video);
        let (tx, rx) = sync_channel(QUEUE);
        let work = Work { url: self.url.clone(), plan: self.plan.clone(), streams: streams.to_vec(), video: self.video, cell: self.cell.clone() };
        let cell = self.cell.clone();
        let spawned = std::thread::Builder::new().name("ironav-publish".into()).spawn(move || {
            // a panic is reported, not left looking like publishing still running
            if std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work.run(rx))).is_err() {
                let e = Error::new(ErrorCode::Internal, "the publisher stopped unexpectedly");
                cell.event("publish_failed", json!({ "reason": e.message }));
                let mut st = lock(&cell.status);
                st["state"] = json!("failed");
                st["error"] = json!(e);
            }
        });
        match spawned {
            Ok(t) => (self.tx, self.thread) = (Some(tx), Some(t)),
            Err(e) => {
                let e = Error::new(ErrorCode::Internal, format!("the publisher could not start: {e}"));
                self.cell.event("publish_failed", json!({ "reason": e.message }));
                *lock(&self.cell.status) = json!({ "url": self.plan.shown, "state": "failed", "error": e });
            }
        }
    }

    /// One packet of the session; `key` says whether a decoder can start from it.
    pub fn packet(&mut self, p: &Packet, key: bool) {
        let Some(tx) = &self.tx else { return };
        let video = Some(p.stream) == self.video;
        if video && self.dropping {
            if !key {
                self.cell.dropped.fetch_add(1, Ordering::Relaxed);
                return;
            }
            self.dropping = false;
        }
        match tx.try_send((p.clone(), key)) {
            Ok(()) => {}
            Err(TrySendError::Full(_)) => {
                // audio is dropped too, rather than wait
                self.dropping |= video;
                self.cell.dropped.fetch_add(1, Ordering::Relaxed);
            }
            Err(TrySendError::Disconnected(_)) => self.tx = None,
        }
    }

    /// The session's output is made by a transcoder, which can make the keyframes the destination asks for.
    pub fn set_transcoded(&self) {
        self.cell.transcoded.store(true, Ordering::Relaxed);
    }

    /// Whether the destination has asked for a keyframe that the transcoder has not made yet.
    pub fn keyframe_wanted(&self) -> bool {
        self.cell.idr_wanted.load(Ordering::Relaxed)
    }

    /// The transcoder has made the keyframe the destination asked for.
    pub fn keyframe_made(&self) {
        self.cell.idr_wanted.store(false, Ordering::Relaxed);
        self.cell.keyframes_forced.fetch_add(1, Ordering::Relaxed);
    }

    /// The session is over: what is queued is sent, and the destination told goodbye.
    pub fn finish(&mut self) {
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        // a request still waiting when the session ends is not answered
        if self.cell.idr_wanted.swap(false, Ordering::Relaxed) {
            self.cell.keyframes_ignored.fetch_add(1, Ordering::Relaxed);
        }
    }
}

impl Drop for Publisher {
    fn drop(&mut self) {
        self.finish();
    }
}

/// The muxer's output, collected for the sink: a transport stream is written and never read back.
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Write for Capture {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        lock(&self.0).extend_from_slice(b);
        Ok(b.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Seek for Capture {
    fn seek(&mut self, _: SeekFrom) -> std::io::Result<u64> {
        Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "a live output cannot seek"))
    }
}

/// A destination connected: an SRT sink with the muxer writing to it, or a WHIP session.
struct Conn {
    link: Link,
    /// Nothing is sent until a picture a decoder can start from.
    waiting_key: bool,
}

enum Link {
    Ts { sink: Box<dyn Sink>, mux: Box<dyn Muxer>, out: Arc<Mutex<Vec<u8>>> },
    Whip(Box<WhipLink>),
}

/// A WHIP session and what it is sent: the streams that go (H.264 video, Opus audio), and the parameter
/// sets and length size that H.264 packets carry when they are length-prefixed (as MP4 holds them).
struct WhipLink {
    /// `None` once the session has been closed.
    publisher: Option<WhipPublisher>,
    video: Option<(usize, Rational)>,
    audio: Option<(usize, Rational)>,
    /// The length of each NAL unit's prefix, when the packets have one.
    length_size: Option<usize>,
    /// The stream's SPS and PPS in Annex B form (empty when it has none).
    params: Vec<u8>,
}

impl Conn {
    /// Sends one packet of the session. `Ok(None)` when this destination does not take its stream.
    fn write(&mut self, p: &Packet) -> Result<Option<usize>> {
        match &mut self.link {
            Link::Ts { mux, .. } => {
                mux.write_packet(p)?;
                self.flush().map(Some)
            }
            Link::Whip(w) => w.send(p),
        }
    }

    /// Sends what the muxer has written (nothing to send for a WHIP session).
    fn flush(&mut self) -> Result<usize> {
        let Link::Ts { sink, out, .. } = &mut self.link else { return Ok(0) };
        let bytes = std::mem::take(&mut *lock(out));
        if !bytes.is_empty() {
            sink.write(&bytes)?;
        }
        Ok(bytes.len())
    }

    /// Whether the destination has asked for a keyframe since it was last asked.
    fn keyframe_requested(&mut self) -> bool {
        match &mut self.link {
            Link::Ts { .. } => false,
            Link::Whip(w) => w.publisher.as_mut().is_some_and(|p| p.keyframe_requested()),
        }
    }

    /// Ends the destination: what is queued is sent, and the destination is told goodbye.
    fn finish(&mut self) -> Result<()> {
        if let Link::Ts { mux, .. } = &mut self.link {
            mux.finish()?;
        }
        self.flush()?;
        match &mut self.link {
            Link::Ts { sink, .. } => sink.finish(),
            Link::Whip(w) => w.close(),
        }
    }

    /// Gives up on a destination that failed: a sink is finished, and a WHIP session is ended by dropping it
    /// (which says goodbye to the server).
    fn abandon(self) {
        if let Link::Ts { mut sink, .. } = self.link {
            let _ = sink.finish();
        }
    }

    /// The destination's status, as the publishing status reports it.
    fn sink_status(&self) -> Value {
        match &self.link {
            Link::Ts { sink, .. } => json!(sink.stats()),
            Link::Whip(w) => json!({ "kind": "whip", "connected": w.publisher.as_ref().is_some_and(|p| p.failure().is_none()) }),
        }
    }
}

impl WhipLink {
    /// Sends one packet if its stream is one WHIP takes: H.264 video, or Opus audio.
    fn send(&mut self, p: &Packet) -> Result<Option<usize>> {
        let (video, audio) = (self.video, self.audio);
        match (video, audio) {
            (Some((s, tb)), _) if s == p.stream => {
                let au = annexb_access_unit(&p.data, self.length_size, &self.params)?;
                let pts = to_clock(p.pts, tb, 90_000);
                self.open()?.send_h264_access_unit(&au, pts)?;
                Ok(Some(au.len()))
            }
            (_, Some((s, tb))) if s == p.stream => {
                let pts = to_clock(p.pts, tb, 48_000);
                self.open()?.send_opus(&p.data, pts)?;
                Ok(Some(p.data.len()))
            }
            _ => Ok(None),
        }
    }

    fn open(&mut self) -> Result<&mut WhipPublisher> {
        self.publisher.as_mut().ok_or_else(|| Error::new(ErrorCode::Io, "the WHIP session is closed"))
    }

    fn close(&mut self) -> Result<()> {
        match self.publisher.take() {
            Some(p) => p.close(),
            None => Ok(()),
        }
    }
}

/// An H.264 access unit as a WHIP publisher takes it: Annex B. A packet that is length-prefixed is converted,
/// and an IDR picture gets the stream's parameter sets in front of it when it has none of its own.
fn annexb_access_unit(data: &[u8], length_size: Option<usize>, params: &[u8]) -> Result<Vec<u8>> {
    let au = match length_size {
        Some(n) if !starts_with_start_code(data) => avcc_to_annexb(data, n)?,
        _ => data.to_vec(),
    };
    let types = nal_types(&au);
    if params.is_empty() || !types.contains(&5) || types.contains(&7) {
        return Ok(au);
    }
    let mut out = params.to_vec();
    out.extend_from_slice(&au);
    Ok(out)
}

/// Whether `d` starts with an Annex B start code.
fn starts_with_start_code(d: &[u8]) -> bool {
    d.starts_with(&[0, 0, 1]) || d.starts_with(&[0, 0, 0, 1])
}

/// The NAL unit types of an Annex B access unit, in order.
fn nal_types(au: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 3 <= au.len() {
        if au[i..i + 3] == [0, 0, 1] {
            if let Some(h) = au.get(i + 3) {
                out.push(h & 31);
            }
            i += 3;
        } else {
            i += 1;
        }
    }
    out
}

/// Length-prefixed NAL units (`length_size` bytes of length before each) in Annex B form.
fn avcc_to_annexb(data: &[u8], length_size: usize) -> Result<Vec<u8>> {
    let bad = || Error::invalid_input("an H.264 packet's length prefixes do not match its size");
    let mut out = Vec::with_capacity(data.len() + 8);
    let mut rest = data;
    while !rest.is_empty() {
        let len = rest.get(..length_size).ok_or_else(bad)?.iter().fold(0usize, |n, b| n << 8 | *b as usize);
        let nal = rest.get(length_size..length_size + len).ok_or_else(bad)?;
        if !nal.is_empty() {
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(nal);
        }
        rest = &rest[length_size + len..];
    }
    Ok(out)
}

/// The NAL unit length size, and the SPS and PPS, of an `avcC` configuration record (the codec extradata an
/// MP4 stream carries); the parameter sets come in Annex B form. `None` when there is no such record.
fn avcc_config(extra: &[u8]) -> Option<(usize, Vec<u8>)> {
    if extra.len() < 7 || extra[0] != 1 || (extra[4] & 3) == 2 {
        return None;
    }
    let length_size = (extra[4] & 3) as usize + 1;
    let mut out = Vec::new();
    let mut r = &extra[5..];
    // the SPS count is in the low five bits of its byte, the PPS count is the whole byte
    for sps in [true, false] {
        let n = if sps { *r.first()? & 31 } else { *r.first()? };
        r = &r[1..];
        for _ in 0..n {
            let len = u16::from_be_bytes([*r.first()?, *r.get(1)?]) as usize;
            let nal = r.get(2..2 + len)?;
            out.extend_from_slice(&[0, 0, 0, 1]);
            out.extend_from_slice(nal);
            r = &r[2 + len..];
        }
    }
    Some((length_size, out))
}

/// A presentation time in a stream's time base, as ticks of a clock at `rate` (never negative).
fn to_clock(pts: i64, tb: Rational, rate: i64) -> u64 {
    let den = i128::from(tb.den.max(1));
    (i128::from(pts) * i128::from(tb.num) * i128::from(rate) / den).max(0) as u64
}

struct Work {
    url: String,
    plan: SinkPlan,
    streams: Vec<StreamInfo>,
    video: Option<usize>,
    cell: Arc<PublishCell>,
}

/// Whether trying again could help: not for what is wrong with the request itself.
fn permanent(e: &Error) -> bool {
    matches!(e.code, ErrorCode::InvalidArgument | ErrorCode::PermissionDenied | ErrorCode::UnsupportedFormat | ErrorCode::UnsupportedCodec)
}

impl Work {
    fn connect(&self) -> Result<Conn> {
        if self.whip() {
            self.connect_whip()
        } else {
            self.connect_ts()
        }
    }

    /// Whether the destination is a WHIP endpoint (`whip://`, `whips://`), not a byte sink.
    fn whip(&self) -> bool {
        matches!(self.plan.scheme.as_str(), "whip" | "whips")
    }

    fn connect_ts(&self) -> Result<Conn> {
        let fmt = ironav_formats::find_format("mpegts")?;
        if let Some(s) = self.streams.iter().find(|s| !fmt.codecs.contains(&s.codec)) {
            return Err(Error::new(ErrorCode::UnsupportedFormat, format!("a transport stream cannot hold {}", s.codec.name()))
                .hint("publish a source that holds H.264, HEVC, AAC or MP3 (an .m3u8 or .ts recording holds the same)"));
        }
        let sink = open_sink(&self.url)?;
        let out = Arc::new(Mutex::new(Vec::new()));
        let mut mux: Box<dyn Muxer> = Box::new(ironav_formats::ts::TsMuxer::new(Box::new(Capture(out.clone()))));
        mux.write_header(&self.streams)?;
        Ok(Conn { link: Link::Ts { sink, mux, out }, waiting_key: true })
    }

    /// A WHIP session to the endpoint the URL names. The video must be H.264; Opus audio is sent, and
    /// other audio is not (see [`Work::say_unsent_audio`]).
    fn connect_whip(&self) -> Result<Conn> {
        let ep = whip_endpoint(&self.url)?;
        if let Some(i) = self.video {
            let s = &self.streams[i];
            if s.codec != CodecId::H264 {
                return Err(Error::new(ErrorCode::UnsupportedCodec, format!("WHIP carries H.264 video, and this video is {}", s.codec.name()))
                    .hint("publish with --transcode OUT.ts: its output is H.264, and that is what is sent"));
            }
        }
        let audio = self.streams.iter().position(|s| s.codec == CodecId::Opus);
        if self.video.is_none() && audio.is_none() {
            return Err(Error::new(ErrorCode::UnsupportedFormat, "nothing to send over WHIP: the source has no H.264 video and no Opus audio")
                .hint("WHIP carries H.264 video and Opus audio; --transcode makes H.264 video from any video"));
        }
        let mut opts = WhipOptions::default();
        opts.video = self.video.is_some();
        opts.audio = audio.is_some();
        let publisher = WhipPublisher::connect(&ep.url, ep.token.as_deref(), opts)?;
        let (length_size, params) = match self.video.and_then(|i| avcc_config(&self.streams[i].extradata)) {
            Some((n, p)) => (Some(n), p),
            None => (None, Vec::new()),
        };
        let link = WhipLink {
            publisher: Some(publisher),
            video: self.video.map(|i| (i, self.streams[i].time_base)),
            audio: audio.map(|i| (i, self.streams[i].time_base)),
            length_size,
            params,
        };
        Ok(Conn { link: Link::Whip(Box::new(link)), waiting_key: true })
    }

    /// Says, once, that the audio WHIP does not take is not sent.
    fn say_unsent_audio(&self) {
        let codecs: Vec<&str> = self.streams.iter().filter(|s| s.params.media_type() == MediaType::Audio && s.codec != CodecId::Opus).map(|s| s.codec.name()).collect();
        if !codecs.is_empty() {
            self.cell.event("publish_audio_skipped", json!({ "codecs": codecs, "reason": "WHIP carries Opus audio only, so this audio is not sent; the video is" }));
        }
    }

    fn run(self, rx: std::sync::mpsc::Receiver<(Packet, bool)>) {
        let started = Instant::now();
        if self.whip() {
            self.say_unsent_audio();
        }
        let (mut conn, mut failed): (Option<Conn>, Option<Error>) = (None, None);
        let (mut next_try, mut backoff, mut attempts, mut reconnects) = (Instant::now(), FIRST_BACKOFF, 0u32, 0u32);
        // whether the destination has been up at all, and whether its loss has been reported
        let (mut ever, mut lost_reported) = (false, false);
        let (mut packets, mut bytes, mut skipped) = (0u64, 0u64, 0u64);
        let mut last_error: Option<String> = None;
        let mut published = Instant::now() - Duration::from_secs(1);
        loop {
            if conn.is_none() && failed.is_none() && Instant::now() >= next_try {
                match self.connect() {
                    Ok(c) => {
                        conn = Some(c);
                        if ever {
                            reconnects += 1;
                            self.cell.event("publish_restored", json!({ "attempts": attempts + 1, "reconnects": reconnects }));
                        }
                        (ever, lost_reported, attempts, backoff) = (true, false, 0, FIRST_BACKOFF);
                    }
                    Err(e) => {
                        attempts += 1;
                        last_error = Some(e.message.clone());
                        if permanent(&e) || !self.plan.reconnect {
                            self.cell.event("publish_failed", json!({ "reason": e.message, "attempts": attempts }));
                            failed = Some(e);
                        } else {
                            if !lost_reported {
                                lost_reported = true;
                                self.cell.event("publish_lost", json!({ "reason": e.message }));
                            }
                            next_try = Instant::now() + backoff;
                            backoff = (backoff * 2).min(MAX_BACKOFF);
                        }
                    }
                }
            }
            if published.elapsed() >= Duration::from_millis(250) {
                published = Instant::now();
                self.report(&conn, &failed, &last_error, (packets, bytes, skipped), (attempts, reconnects), started);
            }
            let (p, key) = match rx.recv_timeout(Duration::from_millis(100)) {
                Ok(i) => i,
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => break,
            };
            if let Some(c) = conn.as_mut() {
                if c.keyframe_requested() {
                    self.cell.keyframe_asked();
                }
            }
            let Some(c) = conn.as_mut() else {
                skipped += 1;
                continue;
            };
            if c.waiting_key {
                if !(key && self.video.is_none_or(|v| v == p.stream)) {
                    skipped += 1;
                    continue;
                }
                c.waiting_key = false;
            }
            match c.write(&p) {
                Ok(Some(n)) => {
                    packets += 1;
                    bytes += n as u64;
                }
                Ok(None) => {}
                Err(e) => {
                    last_error = Some(e.message.clone());
                    if let Some(c) = conn.take() {
                        c.abandon();
                    }
                    if self.plan.reconnect && !permanent(&e) {
                        lost_reported = true;
                        self.cell.event("publish_lost", json!({ "reason": e.message }));
                        (next_try, attempts, backoff) = (Instant::now() + FIRST_BACKOFF, 0, FIRST_BACKOFF * 2);
                    } else {
                        self.cell.event("publish_failed", json!({ "reason": e.message, "attempts": 0 }));
                        failed = Some(e);
                    }
                }
            }
        }
        if let Some(mut c) = conn.take() {
            if let Err(e) = c.finish() {
                last_error = Some(e.message.clone());
            }
            let st = c.sink_status();
            *lock(&self.cell.status) = self.status("finished", &failed, &last_error, (packets, bytes, skipped), (attempts, reconnects), Some(st), started);
        } else {
            *lock(&self.cell.status) = self.status(if failed.is_some() { "failed" } else { "finished" }, &failed, &last_error, (packets, bytes, skipped), (attempts, reconnects), None, started);
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn status(&self, state: &str, failed: &Option<Error>, last_error: &Option<String>, counts: (u64, u64, u64), tries: (u32, u32), sink: Option<Value>, started: Instant) -> Value {
        let mut st = lock(&self.cell.status).clone();
        st["url"] = json!(self.plan.shown);
        st["state"] = json!(state);
        st["packets"] = json!(counts.0);
        st["bytes"] = json!(counts.1);
        st["skipped_packets"] = json!(counts.2);
        st["attempts"] = json!(tries.0);
        st["reconnects"] = json!(tries.1);
        st["seconds"] = json!((started.elapsed().as_secs_f64() * 10.0).round() / 10.0);
        st["sink"] = sink.unwrap_or(Value::Null);
        st["last_error"] = json!(last_error);
        st["error"] = json!(failed);
        st
    }

    fn report(&self, conn: &Option<Conn>, failed: &Option<Error>, last_error: &Option<String>, counts: (u64, u64, u64), tries: (u32, u32), started: Instant) {
        let stats = conn.as_ref().map(Conn::sink_status);
        let state = match (&stats, failed) {
            (_, Some(_)) => "failed",
            (Some(s), None) if s["connected"] == true => "connected",
            // a listener with no receiver yet
            (Some(_), None) => "listening",
            (None, None) if tries.1 > 0 || last_error.is_some() => "reconnecting",
            (None, None) => "connecting",
        };
        let st = self.status(state, failed, last_error, counts, tries, stats, started);
        *lock(&self.cell.status) = st;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ironav_core::{CodecId, Demuxer};
    use ironav_live::{open_source, Chunk};
    use std::io::Cursor;
    use std::net::UdpSocket;
    use std::sync::atomic::AtomicBool;

    const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../ironav-formats/tests/data/h264_aac.ts");
    const PASS: &str = "passphrase=0123456789abcdef";

    fn free_port() -> u16 {
        UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    /// A receiver on its own thread: everything it gets until told to stop.
    fn receiver(port: u16) -> (Arc<AtomicBool>, JoinHandle<Vec<u8>>) {
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let t = std::thread::spawn(move || {
            let mut rx = open_source(&format!("srt://127.0.0.1:{port}?mode=listener&reconnect=0&{PASS}")).unwrap();
            let (mut out, mut buf) = (Vec::new(), vec![0u8; 64 << 10]);
            while !flag.load(Ordering::Relaxed) {
                match rx.read(&mut buf, Duration::from_millis(20)).unwrap() {
                    Chunk::Data(n) => out.extend_from_slice(&buf[..n]),
                    Chunk::Idle | Chunk::Restart => {}
                    Chunk::End => break,
                }
            }
            out
        });
        (stop, t)
    }

    fn demux(ts: Vec<u8>) -> Vec<Packet> {
        let mut d = ironav_formats::ts::TsDemuxer::open(Cursor::new(ts)).unwrap();
        std::iter::from_fn(|| d.read_packet().unwrap()).collect()
    }

    #[test]
    fn a_lost_destination_is_opened_again_and_the_stream_resumes_at_a_keyframe() {
        ironav_live::net::allow_loopback_for_tests();
        let mut d = ironav_formats::open_demuxer(std::path::Path::new(FIXTURE)).unwrap();
        let streams = d.streams().to_vec();
        let packets: Vec<Packet> = std::iter::from_fn(|| d.read_packet().unwrap()).collect();
        let tb = |p: &Packet| streams[p.stream].time_base.as_f64();
        // the fixture lasts 2.03 s: played twice, the second time with its times moved on
        let round = 2.04;
        let url = {
            let port = free_port();
            format!("srt://127.0.0.1:{port}?{PASS}")
        };
        let port: u16 = url.split(':').nth(2).unwrap().split('?').next().unwrap().parse().unwrap();
        let plan = check(&url).unwrap();
        let mut publisher = Publisher::new(&url, plan);
        let cell = publisher.cell();
        let first = receiver(port);
        // the caller finds it listening
        std::thread::sleep(Duration::from_millis(300));
        publisher.start(&streams);
        let (mut first, mut second) = (Some(first), None);
        let start = Instant::now();
        let t0 = packets.iter().map(|p| p.pts as f64 * tb(p)).fold(f64::MAX, f64::min);
        for k in 0..2 {
            for p in &packets {
                let t = p.pts as f64 * tb(p) - t0 + k as f64 * round;
                let due = Duration::from_secs_f64(t.max(0.0));
                if let Some(wait) = due.checked_sub(start.elapsed()) {
                    std::thread::sleep(wait);
                }
                // the first receiver leaves a little before a second in; a second one is up soon after
                if start.elapsed() > Duration::from_millis(900) {
                    if let Some((stop, t)) = first.take() {
                        stop.store(true, Ordering::Relaxed);
                        let got = t.join().unwrap();
                        assert!(!demux(got).is_empty(), "the first receiver got nothing");
                    }
                }
                if start.elapsed() > Duration::from_millis(1100) && second.is_none() {
                    second = Some(receiver(port));
                }
                let moved = Packet { pts: p.pts + (k as f64 * round / tb(p)).round() as i64, ..p.clone() };
                let key = ironav_formats::is_keyframe(streams[p.stream].codec, &p.data).unwrap_or(true);
                publisher.packet(&moved, key);
            }
        }
        std::thread::sleep(Duration::from_millis(300));
        publisher.finish();
        let (stop, t) = second.take().expect("a second receiver");
        stop.store(true, Ordering::Relaxed);
        let got = demux(t.join().unwrap());
        let kinds: Vec<&str> = cell.take_events().iter().map(|e| e.kind).collect();
        assert_eq!(kinds, ["publish_lost", "publish_restored"], "{}", cell.status());
        let st = cell.status();
        assert_eq!((st["reconnects"].as_u64(), st["state"].as_str()), (Some(1), Some("finished")), "{st}");
        // what the second receiver has starts with a picture it can decode, and goes on
        let video: Vec<&Packet> = got.iter().filter(|p| p.stream == 0).collect();
        assert!(video.len() > 20, "{}", video.len());
        assert_eq!(ironav_formats::is_keyframe(CodecId::H264, &video[0].data), Some(true));
        assert!(got.iter().any(|p| p.stream == 1));
    }

    #[test]
    fn what_a_transport_stream_cannot_hold_ends_publishing_at_once_and_a_full_queue_drops_to_a_keyframe() {
        ironav_live::net::allow_loopback_for_tests();
        let mut d = ironav_formats::open_demuxer(std::path::Path::new(FIXTURE)).unwrap();
        let mut streams = d.streams().to_vec();
        let packets: Vec<Packet> = std::iter::from_fn(|| d.read_packet().unwrap()).collect();
        // a codec the muxer has no place for
        streams[0].codec = CodecId::Vp9;
        let url = format!("srt://127.0.0.1:{}?{PASS}", free_port());
        let mut p = Publisher::new(&url, check(&url).unwrap());
        let cell = p.cell();
        p.start(&streams);
        for pk in &packets {
            p.packet(pk, true);
        }
        p.finish();
        let st = cell.status();
        assert_eq!(st["state"], "failed", "{st}");
        assert_eq!(st["error"]["code"], "unsupported-format", "{st}");
        assert_eq!(cell.take_events().iter().map(|e| e.kind).collect::<Vec<_>>(), ["publish_failed"]);
        // a destination that never answers: the queue fills, the session's calls do not wait
        let streams = d.streams().to_vec();
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let url = format!("srt://127.0.0.1:{}?{PASS}", sock.local_addr().unwrap().port());
        let mut p = Publisher::new(&url, check(&url).unwrap());
        let cell = p.cell();
        p.start(&streams);
        let t = Instant::now();
        for i in 0..5000 {
            let pk = &packets[i % packets.len()];
            p.packet(pk, pk.stream == 0 && i % 50 == 0);
        }
        assert!(t.elapsed() < Duration::from_secs(2), "{:?}", t.elapsed());
        assert!(cell.status()["dropped_packets"].as_u64().unwrap() > 0);
        drop(p);
    }

    #[test]
    fn a_keyframe_request_is_forced_or_ignored_and_the_counts_say_which() {
        ironav_live::net::allow_loopback_for_tests();
        let url = "srt://127.0.0.1:9?passphrase=0123456789abcdef";
        let mut p = Publisher::new(url, check(url).unwrap());
        let cell = p.cell();
        // nothing transcodes: a request is counted, and nothing waits for the transcoder
        cell.keyframe_asked();
        assert!(!p.keyframe_wanted());
        // a transcoder: one request waits for it, and one that comes while it waits is ignored
        p.set_transcoded();
        cell.keyframe_asked();
        cell.keyframe_asked();
        assert!(p.keyframe_wanted());
        p.keyframe_made();
        assert!(!p.keyframe_wanted());
        let st = cell.status();
        assert_eq!((st["keyframe_requests"].as_u64(), st["keyframes_forced"].as_u64(), st["keyframes_ignored"].as_u64()), (Some(3), Some(1), Some(2)), "{st}");
        // a request still waiting when the session ends is ignored
        cell.keyframe_asked();
        p.finish();
        let st = cell.status();
        assert_eq!((st["keyframe_requests"].as_u64(), st["keyframes_forced"].as_u64(), st["keyframes_ignored"].as_u64()), (Some(4), Some(1), Some(3)), "{st}");
    }

    #[test]
    fn length_prefixed_h264_becomes_annex_b_with_the_parameter_sets_before_each_idr() {
        // an avcC record: version 1, profile, compatibility, level, 4-byte length prefixes, one SPS, one PPS
        let extra = [1, 0x42, 0xC0, 0x1F, 0xFF, 0xE1, 0, 4, 0x67, 1, 2, 3, 1, 0, 3, 0x68, 4, 5];
        let (n, params) = avcc_config(&extra).unwrap();
        assert_eq!(n, 4);
        assert_eq!(params, [0, 0, 0, 1, 0x67, 1, 2, 3, 0, 0, 0, 1, 0x68, 4, 5]);
        // an IDR picture of two NAL units, with no parameter sets of its own: they go in front
        let idr = [0, 0, 0, 2, 0x65, 0x88, 0, 0, 0, 1, 0x41];
        assert_eq!(annexb_access_unit(&idr, Some(4), &params).unwrap(), [0, 0, 0, 1, 0x67, 1, 2, 3, 0, 0, 0, 1, 0x68, 4, 5, 0, 0, 0, 1, 0x65, 0x88, 0, 0, 0, 1, 0x41]);
        // a non-IDR picture gets none
        let p = [0, 0, 0, 2, 0x41, 0x9A];
        assert_eq!(annexb_access_unit(&p, Some(4), &params).unwrap(), [0, 0, 0, 1, 0x41, 0x9A]);
        // an Annex B picture is taken as it is, and an IDR that carries its own SPS gets no second one
        let own = [0, 0, 0, 1, 0x67, 9, 0, 0, 0, 1, 0x65, 1];
        assert_eq!(annexb_access_unit(&own, Some(4), &params).unwrap(), own);
        // a length that runs past the packet, or a packet too short for its prefix, is refused
        assert!(annexb_access_unit(&[0, 0, 0, 9, 0x41], Some(4), &params).is_err());
        assert!(avcc_to_annexb(&[0, 0, 1], 4).is_err());
        // a record that is not avcC is no configuration
        assert!(avcc_config(&[0, 0, 0, 1, 0x67]).is_none());
    }

    #[test]
    fn times_are_converted_to_the_clocks_of_the_two_codecs() {
        // 40 ms of a 1 kHz stream is 3600 ticks of 90 kHz and 1920 of 48 kHz
        assert_eq!(to_clock(40, Rational::new(1, 1000), 90_000), 3600);
        assert_eq!(to_clock(40, Rational::new(1, 1000), 48_000), 1920);
        assert_eq!(to_clock(3000, Rational::new(1, 90_000), 90_000), 3000);
        assert_eq!(to_clock(-5, Rational::new(1, 90_000), 90_000), 0);
    }
}
