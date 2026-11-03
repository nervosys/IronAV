//! Publishing a live session as it runs: the media the session records (or, with a transcode, the
//! transcoder's output) is muxed into MPEG-TS and sent to a destination, `srt://` today (see
//! `ironav_live::open_sink`). It is the same transport stream a `.ts` recording would hold, cut at
//! no point: a receiver that joins late starts at the next video keyframe.
//!
//! The work runs on its own thread behind a bounded queue, so a slow or lost destination never holds
//! up the session: when the queue is full, video is dropped up to the next keyframe (a decoder cannot
//! start anywhere else) and counted. A destination that fails is reported to the session (as the
//! observations `publish_lost`, `publish_restored` and `publish_failed`) and the session goes on; a
//! caller is opened again with a growing wait between attempts, as a lost network source is, unless
//! the URL says `reconnect=0`. What cannot be put right by trying again (an address the allow-list
//! refuses, a codec a transport stream cannot hold) ends the publishing at once.

use ironav_core::{Error, ErrorCode, MediaType, Muxer, Packet, Result, StreamInfo};
use ironav_live::{open_sink, Sink, SinkPlan};
use serde_json::{json, Value};
use std::collections::VecDeque;
use std::io::{Seek, SeekFrom, Write};
use std::sync::atomic::{AtomicU64, Ordering};
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
}

impl PublishCell {
    fn new(url: &str) -> PublishCell {
        PublishCell { status: Mutex::new(json!({ "url": url, "state": "waiting" })), events: Mutex::new(VecDeque::new()), dropped: AtomicU64::new(0) }
    }

    pub fn status(&self) -> Value {
        let mut v = lock(&self.status).clone();
        v["dropped_packets"] = json!(self.dropped.load(Ordering::Relaxed));
        v
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

    /// The session is over: what is queued is sent, and the destination told goodbye.
    pub fn finish(&mut self) {
        self.tx = None;
        if let Some(t) = self.thread.take() {
            let _ = t.join();
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

/// A destination connected, and the muxer writing to it.
struct Conn {
    sink: Box<dyn Sink>,
    mux: Box<dyn Muxer>,
    out: Arc<Mutex<Vec<u8>>>,
    /// Nothing is sent until a picture a decoder can start from.
    waiting_key: bool,
}

impl Conn {
    /// Sends what the muxer has written.
    fn flush(&mut self) -> Result<usize> {
        let bytes = std::mem::take(&mut *lock(&self.out));
        if !bytes.is_empty() {
            self.sink.write(&bytes)?;
        }
        Ok(bytes.len())
    }
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
        let fmt = ironav_formats::find_format("mpegts")?;
        if let Some(s) = self.streams.iter().find(|s| !fmt.codecs.contains(&s.codec)) {
            return Err(Error::new(ErrorCode::UnsupportedFormat, format!("a transport stream cannot hold {}", s.codec.name()))
                .hint("publish a source that holds H.264, HEVC, AAC or MP3 (an .m3u8 or .ts recording holds the same)"));
        }
        let sink = open_sink(&self.url)?;
        let out = Arc::new(Mutex::new(Vec::new()));
        let mut mux: Box<dyn Muxer> = Box::new(ironav_formats::ts::TsMuxer::new(Box::new(Capture(out.clone()))));
        mux.write_header(&self.streams)?;
        Ok(Conn { sink, mux, out, waiting_key: true })
    }

    fn run(self, rx: std::sync::mpsc::Receiver<(Packet, bool)>) {
        let started = Instant::now();
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
            match c.mux.write_packet(&p).and_then(|_| c.flush()) {
                Ok(n) => {
                    packets += 1;
                    bytes += n as u64;
                }
                Err(e) => {
                    last_error = Some(e.message.clone());
                    if let Some(mut c) = conn.take() {
                        let _ = c.sink.finish();
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
            if let Err(e) = c.mux.finish().and_then(|_| c.flush()).and_then(|_| c.sink.finish()) {
                last_error = Some(e.message.clone());
            }
            let st = c.sink.stats();
            *lock(&self.cell.status) = self.status("finished", &failed, &last_error, (packets, bytes, skipped), (attempts, reconnects), Some(json!(st)), started);
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
        let stats = conn.as_ref().map(|c| c.sink.stats());
        let state = match (&stats, failed) {
            (_, Some(_)) => "failed",
            (Some(s), None) if s.connected => "connected",
            // a listener with no receiver yet
            (Some(_), None) => "listening",
            (None, None) if tries.1 > 0 || last_error.is_some() => "reconnecting",
            (None, None) => "connecting",
        };
        let st = self.status(state, failed, last_error, counts, tries, stats.map(|s| json!(s)), started);
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
}
