//! Live sessions: a source read on a thread of its own, demuxed as it arrives, analyzed, and
//! logged, so an agent can open a stream, poll what has happened since it last looked (waiting for
//! something new, if it likes), take a snapshot or a clip from the last minute, and close it.
//!
//! A live source is never held up by its readers. The observation log and the packet ring are
//! bounded; a reader that falls behind finds the oldest entries gone and is told how many.

use crate::analyze::{png_of, AnalyzeOptions, Analysis};
use crate::record::{RecordOptions, Recorder};
use ironav_analyze::{Level, Observation};
use ironav_core::{Error, ErrorCode, Rational, Result, StreamInfo, VideoFrame};
use ironav_formats::stream::{stream_demuxer, StreamOptions, StreamPacket, SNIFF_BYTES};
use ironav_live::{open_source, Chunk, Source, SourceInfo, SourceUrl};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap, VecDeque};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Live sessions open at once.
pub const MAX_SESSIONS: usize = 8;
/// Observations kept per session; older ones are dropped.
const LOG_CAP: usize = 20_000;
/// The packet ring keeps at most this many bytes, whatever `retain_seconds` says.
const RING_BYTES: usize = 256 << 20;
/// How long one read of the source may wait, so a stop request is noticed quickly.
const READ_WAIT: Duration = Duration::from_millis(100);
/// The longest a `stream_events` call waits.
pub const MAX_WAIT_MS: u64 = 30_000;
/// Latencies kept for the percentiles in the status.
const LATENCY_SAMPLES: usize = 512;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OpenArgs {
    pub source: String,
    #[serde(default)]
    pub analyzers: Vec<String>,
    #[serde(default = "default_level")]
    pub level: String,
    #[serde(default = "default_digest")]
    pub digest_seconds: f64,
    #[serde(default = "default_retain")]
    pub retain_seconds: f64,
    #[serde(default)]
    pub format: Option<String>,
    #[serde(default)]
    pub fps: Option<f64>,
    #[serde(default)]
    pub pace: Option<String>,
    #[serde(default)]
    pub thumbnail_seconds: Option<f64>,
    #[serde(default = "default_hwaccel")]
    pub hwaccel: String,
    #[serde(default)]
    pub sidecars: Vec<String>,
    /// Record the media as it arrives: a file, or an `.m3u8` path for HLS segments and a playlist.
    #[serde(default)]
    pub record: Option<String>,
    #[serde(default)]
    pub segment_seconds: Option<f64>,
    #[serde(default)]
    pub keep_segments: Option<usize>,
    #[serde(default)]
    pub overwrite: bool,
}

fn default_level() -> String {
    "events".into()
}
fn default_digest() -> f64 {
    10.0
}
fn default_retain() -> f64 {
    60.0
}
fn default_hwaccel() -> String {
    "auto".into()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// Waiting for the first bytes (a TCP listener waits for its sender here).
    Waiting,
    Running,
    /// The source ended; everything it sent has been analyzed.
    Ended,
    Failed,
    Closed,
}

/// A packet kept for snapshots and clips.
struct Kept {
    packet: StreamPacket,
    t: f64,
}

struct Shared {
    state: State,
    error: Option<Error>,
    format: Option<&'static str>,
    streams: Vec<StreamInfo>,
    analyzed: Vec<Value>,
    warnings: Vec<String>,
    log: VecDeque<Observation>,
    next_seq: u64,
    ring: VecDeque<Kept>,
    ring_bytes: usize,
    picture: Option<(f64, VideoFrame)>,
    bytes_in: u64,
    packets: u64,
    media_seconds: f64,
    latencies: VecDeque<f64>,
    summary: Option<Value>,
    /// Thumbnails since the last snapshot of them was read, newest last (at most a few).
    thumbnails: VecDeque<(f64, VideoFrame)>,
    /// The recorder's status, as last published by the session thread.
    record: Option<Value>,
}

pub struct Session {
    pub id: String,
    pub source: SourceInfo,
    opened: SystemTime,
    retain: f64,
    shared: Mutex<Shared>,
    changed: Condvar,
    stop: AtomicBool,
    thread: Mutex<Option<JoinHandle<()>>>,
}

fn sessions() -> &'static Mutex<(u64, HashMap<String, Arc<Session>>)> {
    static S: OnceLock<Mutex<(u64, HashMap<String, Arc<Session>>)>> = OnceLock::new();
    S.get_or_init(Default::default)
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub fn get(id: &str) -> Result<Arc<Session>> {
    let s = lock(sessions());
    s.1.get(id).cloned().ok_or_else(|| {
        Error::new(ErrorCode::NotFound, format!("no live session '{id}'")).alternatives(s.1.keys().cloned().collect::<Vec<_>>())
    })
}

pub fn list() -> Vec<Value> {
    let s = lock(sessions());
    let mut v: Vec<_> = s.1.values().map(|x| x.status()).collect();
    v.sort_by(|a, b| a["session"].as_str().cmp(&b["session"].as_str()));
    v
}

fn parse_fps(s: &str) -> Result<Rational> {
    let bad = || Error::invalid_arg(format!("fps must be a number or a ratio like 30000/1001, got '{s}'"));
    if let Some((n, d)) = s.split_once('/') {
        let (n, d) = (n.trim().parse::<i64>().map_err(|_| bad())?, d.trim().parse::<i64>().map_err(|_| bad())?);
        return if n > 0 && d > 0 { Ok(Rational::new(n, d)) } else { Err(bad()) };
    }
    let f: f64 = s.trim().parse().map_err(|_| bad())?;
    if !(f > 0.0 && f < 1000.0) {
        return Err(bad());
    }
    // 29.97 and friends as the exact NTSC rates
    for (n, d) in [(24000, 1001), (30000, 1001), (60000, 1001)] {
        if (f - n as f64 / d as f64).abs() < 0.01 {
            return Ok(Rational::new(n, d));
        }
    }
    Ok(Rational::new((f * 1000.0).round() as i64, 1000))
}

/// Opens a live session on `args.source` and starts reading it.
pub fn open(args: &OpenArgs) -> Result<Arc<Session>> {
    let url = SourceUrl::parse(&args.source)?;
    let opts = AnalyzeOptions {
        analyzers: args.analyzers.clone(),
        level: args.level.clone(),
        digest_seconds: args.digest_seconds,
        thumbnail_seconds: args.thumbnail_seconds,
        hwaccel: args.hwaccel.clone(),
        sidecars: args.sidecars.clone(),
        ..AnalyzeOptions::default()
    };
    // check the analysis options now rather than when the first packet arrives
    Analysis::new(&[], &opts, 0.0, f64::INFINITY, true)?;
    let stream_opts = StreamOptions {
        format: args.format.clone().or_else(|| url.option("format").map(str::to_string)),
        frame_rate: match args.fps.map(|f| f.to_string()).or_else(|| url.option("fps").map(str::to_string)) {
            Some(f) => Some(parse_fps(&f)?),
            None => None,
        },
    };
    let pace = args.pace.as_deref().or(url.option("pace")).map(str::to_ascii_lowercase);
    let realtime = match pace.as_deref() {
        None | Some("none" | "off" | "0") => false,
        Some("realtime" | "1" | "on") => true,
        Some(p) => return Err(Error::invalid_arg(format!("unknown pace '{p}'")).alternatives(["realtime", "none"])),
    };
    let record = args.record.as_ref().map(|path| RecordOptions { path: path.clone(), segment_seconds: args.segment_seconds, keep_segments: args.keep_segments, overwrite: args.overwrite });
    match &record {
        Some(r) => r.check()?,
        None if args.segment_seconds.is_some() || args.keep_segments.is_some() => {
            return Err(Error::invalid_arg("segment_seconds and keep_segments go with record"));
        }
        None => {}
    }
    if !(args.retain_seconds >= 0.0) {
        return Err(Error::invalid_arg("retain_seconds must be 0 or more"));
    }
    {
        let s = lock(sessions());
        if s.1.len() >= MAX_SESSIONS {
            return Err(Error::new(ErrorCode::Busy, format!("{MAX_SESSIONS} live sessions are open already")).hint("close one with stream_close"));
        }
    }
    let source = open_source(&args.source)?;
    let info = source.info().clone();
    let id = {
        let mut s = lock(sessions());
        s.0 += 1;
        format!("s{}", s.0)
    };
    let session = Arc::new(Session {
        id: id.clone(),
        source: info,
        opened: SystemTime::now(),
        retain: args.retain_seconds,
        shared: Mutex::new(Shared {
            state: State::Waiting,
            error: None,
            format: None,
            streams: Vec::new(),
            analyzed: Vec::new(),
            warnings: Vec::new(),
            log: VecDeque::new(),
            next_seq: 1,
            ring: VecDeque::new(),
            ring_bytes: 0,
            picture: None,
            bytes_in: 0,
            packets: 0,
            media_seconds: 0.0,
            latencies: VecDeque::new(),
            summary: None,
            thumbnails: VecDeque::new(),
            record: None,
        }),
        changed: Condvar::new(),
        stop: AtomicBool::new(false),
        thread: Mutex::new(None),
    });
    let worker = session.clone();
    let handle = std::thread::Builder::new()
        .name(format!("ironav-live-{id}"))
        .spawn(move || {
            let mut rec = record.map(Recorder::new);
            let r = worker.run(source, &opts, &stream_opts, realtime, rec.as_mut());
            // a recording is closed however the session ended
            if let Some(rec) = rec.as_mut() {
                let end = lock(&worker.shared).media_seconds;
                rec.finish(end);
                lock(&worker.shared).record = Some(rec.status());
            }
            let mut sh = lock(&worker.shared);
            if let Err(e) = r {
                sh.state = State::Failed;
                sh.error = Some(e);
            } else if worker.stop.load(Ordering::Relaxed) {
                sh.state = State::Closed;
            } else {
                sh.state = State::Ended;
            }
            drop(sh);
            worker.changed.notify_all();
        })
        .map_err(|e| Error::new(ErrorCode::Internal, format!("could not start the session thread: {e}")))?;
    *lock(&session.thread) = Some(handle);
    lock(sessions()).1.insert(id, session.clone());
    Ok(session)
}

/// Stops a session, waits for its thread, and removes it. Returns its final status.
pub fn close(id: &str) -> Result<Value> {
    let s = get(id)?;
    s.stop.store(true, Ordering::Relaxed);
    if let Some(h) = lock(&s.thread).take() {
        let _ = h.join();
    }
    {
        let mut sh = lock(&s.shared);
        if sh.state == State::Running || sh.state == State::Waiting {
            sh.state = State::Closed;
        }
    }
    lock(sessions()).1.remove(id);
    let mut v = s.status();
    v["summary"] = lock(&s.shared).summary.clone().unwrap_or(Value::Null);
    Ok(v)
}

fn now_iso() -> String {
    let d = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = d.as_secs();
    let (days, rem) = (secs / 86_400, secs % 86_400);
    // civil date from days since 1970-01-01 (Howard Hinnant's algorithm)
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + (month <= 2) as i64;
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z", rem / 3600, rem / 60 % 60, rem % 60, d.subsec_millis())
}

impl Session {
    fn run(&self, mut source: Box<dyn Source>, opts: &AnalyzeOptions, sopts: &StreamOptions, realtime: bool, mut rec: Option<&mut Recorder>) -> Result<()> {
        let mut published = Instant::now();
        let mut buf = vec![0u8; 64 << 10];
        let mut head = Vec::new();
        let mut demux = None;
        let mut analysis: Option<Analysis> = None;
        let mut pace: Option<(Instant, f64)> = None;
        let mut ended = false;
        while !ended {
            if self.stop.load(Ordering::Relaxed) {
                break;
            }
            let chunk = source.read(&mut buf, READ_WAIT)?;
            let arrival = Instant::now();
            let bytes: &[u8] = match chunk {
                Chunk::Data(n) => {
                    lock(&self.shared).bytes_in += n as u64;
                    &buf[..n]
                }
                Chunk::Idle => continue,
                Chunk::End => {
                    ended = true;
                    &[]
                }
            };
            if demux.is_none() {
                head.extend_from_slice(bytes);
                if head.len() < SNIFF_BYTES && !ended {
                    continue;
                }
                if head.is_empty() {
                    break;
                }
                let mut d = stream_demuxer(&head, sopts)?;
                d.feed(&head)?;
                lock(&self.shared).format = Some(d.format_name());
                demux = Some(d);
                head = Vec::new();
            } else {
                demux.as_mut().unwrap().feed(bytes)?;
            }
            let d = demux.as_mut().unwrap();
            if ended {
                d.end();
            }
            while let Some(sp) = d.next_packet()? {
                if self.stop.load(Ordering::Relaxed) {
                    break;
                }
                if analysis.is_none() {
                    let streams = d.streams().to_vec();
                    let a = Analysis::new(&streams, opts, 0.0, f64::INFINITY, true)?;
                    let mut sh = lock(&self.shared);
                    sh.state = State::Running;
                    sh.analyzed = a.describe();
                    sh.warnings = a.warnings().to_vec();
                    if let Some(rec) = rec.as_deref_mut() {
                        rec.start(&streams);
                        sh.record = Some(rec.status());
                    }
                    sh.streams = streams;
                    drop(sh);
                    analysis = Some(a);
                }
                let a = analysis.as_mut().unwrap();
                let tb = d.streams().get(sp.packet.stream).map_or(0.0, |s| s.time_base.as_f64());
                let t = sp.packet.pts as f64 * tb;
                if realtime {
                    // release packets no faster than they play
                    let (start, t0) = *pace.get_or_insert((Instant::now(), t));
                    let due = start + Duration::from_secs_f64((t - t0).max(0.0));
                    while Instant::now() < due && !self.stop.load(Ordering::Relaxed) {
                        std::thread::sleep((due - Instant::now()).min(READ_WAIT));
                    }
                }
                let arrived = if realtime { Instant::now() } else { arrival };
                self.keep(&sp, t);
                // written before the shared state is locked: a slow disk holds up only this thread
                if let Some(rec) = rec.as_deref_mut() {
                    rec.packet(&sp.packet, t, sp.key);
                    if published.elapsed() >= Duration::from_millis(250) {
                        published = Instant::now();
                        lock(&self.shared).record = Some(rec.status());
                    }
                }
                a.packet(&sp.packet, &mut |o| self.log(o, Some(arrived)))?;
                let mut sh = lock(&self.shared);
                sh.packets += 1;
                sh.media_seconds = sh.media_seconds.max(a.position());
                if let Some(p) = a.last_picture.take() {
                    sh.picture = Some(p);
                }
            }
        }
        if let Some(a) = analysis.as_mut() {
            let mut summary = None;
            a.finish(&mut |o| {
                if o.kind == "summary" {
                    summary = Some(o.data.clone());
                }
                self.log(o, None)
            })?;
            let mut sh = lock(&self.shared);
            sh.summary = summary;
            if let Some(p) = a.last_picture.take() {
                sh.picture = Some(p);
            }
        }
        Ok(())
    }

    /// Adds a packet to the ring, dropping what is older than the retention (keeping whole groups
    /// from a keyframe, so what is left can still be decoded) or over the byte cap.
    fn keep(&self, sp: &StreamPacket, t: f64) {
        let mut sh = lock(&self.shared);
        sh.ring_bytes += sp.packet.data.len();
        sh.ring.push_back(Kept { packet: sp.clone(), t });
        loop {
            let too_old = sh.ring.front().is_some_and(|k| t - k.t > self.retain);
            if !(too_old || sh.ring_bytes > RING_BYTES) || sh.ring.len() <= 1 {
                break;
            }
            let k = sh.ring.pop_front().unwrap();
            sh.ring_bytes -= k.packet.packet.data.len();
            // drop up to the next keyframe of that stream, which can start a decoder
            while sh.ring.front().is_some_and(|f| f.packet.packet.stream == k.packet.packet.stream && !f.packet.key) {
                let f = sh.ring.pop_front().unwrap();
                sh.ring_bytes -= f.packet.packet.data.len();
            }
        }
    }

    fn log(&self, mut o: Observation, arrival: Option<Instant>) -> Result<()> {
        let mut sh = lock(&self.shared);
        if let Some(p) = o.picture.take() {
            sh.thumbnails.push_back((o.t, p.0));
            while sh.thumbnails.len() > 4 {
                sh.thumbnails.pop_front();
            }
            o.refs.insert("snapshot".into(), json!({ "tool": "stream_snapshot", "at": o.t }));
        }
        if let Some(a) = arrival {
            let ms = (a.elapsed().as_secs_f64() * 1e4).round() / 10.0;
            o.latency_ms = Some(ms);
            sh.latencies.push_back(ms);
            if sh.latencies.len() > LATENCY_SAMPLES {
                sh.latencies.pop_front();
            }
        }
        o.seq = sh.next_seq;
        sh.next_seq += 1;
        o.wall = Some(now_iso());
        sh.log.push_back(o);
        if sh.log.len() > LOG_CAP {
            sh.log.pop_front();
        }
        drop(sh);
        self.changed.notify_all();
        Ok(())
    }

    pub fn status(&self) -> Value {
        let sh = lock(&self.shared);
        let mut lat: Vec<f64> = sh.latencies.iter().copied().collect();
        lat.sort_by(f64::total_cmp);
        let pct = |p: f64| lat.get(((lat.len().max(1) - 1) as f64 * p).round() as usize).copied();
        json!({
            "session": self.id,
            "source": self.source,
            "state": sh.state,
            "error": sh.error,
            "format": sh.format,
            "streams": sh.streams,
            "analyzed": sh.analyzed,
            "warnings": sh.warnings,
            "opened_unix": self.opened.duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0),
            "bytes_in": sh.bytes_in,
            "packets": sh.packets,
            "media_seconds": (sh.media_seconds * 1000.0).round() / 1000.0,
            "latency_ms": { "p50": pct(0.5), "p95": pct(0.95), "samples": lat.len() },
            "log": { "first_seq": sh.log.front().map(|o| o.seq), "last_seq": sh.log.back().map(|o| o.seq) },
            "retained": { "from": sh.ring.front().map(|k| k.t), "to": sh.ring.back().map(|k| k.t), "bytes": sh.ring_bytes },
            "record": sh.record,
        })
    }

    /// Observations after `after`, filtered, waiting up to `wait` for the first one.
    pub fn events(&self, after: u64, kinds: &[String], level: Level, max: usize, wait: Duration) -> Value {
        let deadline = Instant::now() + wait;
        let mut sh = lock(&self.shared);
        loop {
            let matches = |o: &Observation| o.seq > after && o.level() <= level && (kinds.is_empty() || kinds.iter().any(|k| k == o.kind));
            let found: Vec<&Observation> = sh.log.iter().filter(|o| matches(o)).take(max).collect();
            let done = matches!(sh.state, State::Ended | State::Failed | State::Closed);
            let now = Instant::now();
            if !found.is_empty() || done || now >= deadline {
                // the seq to ask after next time: past what was returned, or past everything
                // looked at when nothing matched
                let next = found.last().map(|o| o.seq).unwrap_or_else(|| sh.log.back().map_or(after, |o| o.seq.max(after)));
                let missed = sh.log.front().map_or(0, |o| o.seq.saturating_sub(after + 1));
                return json!({
                    "session": self.id,
                    "state": sh.state,
                    "events": found,
                    "next_after": next,
                    "missed": missed,
                    "more": sh.log.iter().filter(|o| matches(o)).count() > max,
                });
            }
            sh = self.changed.wait_timeout(sh, deadline - now).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    /// A picture: the newest when `at` is `None`, otherwise the one shown at `at`, decoded from
    /// the ring.
    pub fn snapshot(&self, at: Option<f64>, max_width: Option<u32>) -> Result<Value> {
        let (t, frame) = match at {
            None => {
                let sh = lock(&self.shared);
                sh.picture.clone().ok_or_else(|| Error::new(ErrorCode::NotFound, "no picture has been decoded yet").hint("wait for the state to be running, or for a video stream"))?
            }
            Some(at) => self.picture_at(at)?,
        };
        let (w, h, png) = png_of(&frame, max_width)?;
        Ok(json!({ "session": self.id, "snapshot": { "t": (t * 1000.0).round() / 1000.0, "width": w, "height": h, "png_base64": ironav_player::base64(&png) } }))
    }

    fn picture_at(&self, at: f64) -> Result<(f64, VideoFrame)> {
        {
            // a thumbnail at that time is already decoded
            let sh = lock(&self.shared);
            if let Some((t, f)) = sh.thumbnails.iter().find(|(t, _)| (t - at).abs() < 0.0005) {
                return Ok((*t, f.clone()));
            }
        }
        let (info, packets) = {
            let sh = lock(&self.shared);
            let vi = sh.streams.iter().position(|s| s.params.media_type() == ironav_core::MediaType::Video).ok_or_else(|| Error::new(ErrorCode::NotFound, "the source has no video stream"))?;
            let ring: Vec<&Kept> = sh.ring.iter().filter(|k| k.packet.packet.stream == vi).collect();
            let start = ring.iter().rposition(|k| k.packet.key && k.t <= at).ok_or_else(|| self.not_retained(&sh, at))?;
            (sh.streams[vi].clone(), ring[start..].iter().map(|k| k.packet.packet.clone()).collect::<Vec<_>>())
        };
        let (mut dec, _) = ironav_player::open_video_decoder(&info, "off")?;
        let tb = info.time_base.as_f64();
        let mut best: Option<(f64, VideoFrame)> = None;
        let take = |frames: Vec<ironav_core::Frame>, best: &mut Option<(f64, VideoFrame)>| {
            for f in frames {
                if let ironav_core::Frame::Video(v) = f {
                    let t = v.pts as f64 * tb;
                    if t <= at + 1e-6 && best.as_ref().is_none_or(|(bt, _)| t >= *bt) {
                        *best = Some((t, v));
                    }
                }
            }
        };
        for p in &packets {
            if p.pts as f64 * tb > at + 1.0 {
                break;
            }
            take(dec.decode(p)?, &mut best);
        }
        take(dec.flush()?, &mut best);
        best.ok_or_else(|| Error::new(ErrorCode::NotFound, format!("no picture at {at} s could be decoded")))
    }

    fn not_retained(&self, sh: &Shared, at: f64) -> Error {
        let (from, to) = (sh.ring.front().map_or(0.0, |k| k.t), sh.ring.back().map_or(0.0, |k| k.t));
        Error::new(ErrorCode::NotFound, format!("{at} s is not in the retained range {from:.3}-{to:.3} s")).hint("raise retain_seconds when opening the session to keep more")
    }

    /// Writes the retained packets from the keyframe at or before `from` up to `to` into `output`,
    /// without re-encoding.
    pub fn clip(&self, from: f64, to: f64, output: &str, format: Option<&str>, overwrite: bool) -> Result<Value> {
        if to <= from {
            return Err(Error::invalid_arg("'to' must be after 'from'"));
        }
        let path = Path::new(output);
        if path.exists() && !overwrite {
            return Err(Error::new(ErrorCode::OutputExists, format!("{output} exists")).hint("pass overwrite: true to replace it"));
        }
        let fmt = match format {
            Some(f) => ironav_formats::find_format(f)?,
            None => ironav_formats::format_for_path(path)?,
        };
        let (streams, packets) = {
            let sh = lock(&self.shared);
            if sh.streams.is_empty() {
                return Err(Error::new(ErrorCode::NotFound, "nothing has been received yet"));
            }
            // per stream, start at its last keyframe at or before `from`
            let mut starts = BTreeMap::new();
            for (i, k) in sh.ring.iter().enumerate() {
                if k.packet.key && k.t <= from + 1e-6 {
                    starts.insert(k.packet.packet.stream, i);
                }
            }
            if starts.is_empty() {
                return Err(self.not_retained(&sh, from));
            }
            let picked: Vec<_> = sh
                .ring
                .iter()
                .enumerate()
                .filter(|(i, k)| starts.get(&k.packet.packet.stream).is_some_and(|s| i >= s) && k.t < to)
                .map(|(_, k)| k.packet.packet.clone())
                .collect();
            (sh.streams.clone(), picked)
        };
        if let Some(s) = streams.iter().find(|s| !fmt.codecs.is_empty() && !fmt.codecs.contains(&s.codec)) {
            return Err(Error::new(ErrorCode::UnsupportedFormat, format!("{} cannot hold {}", fmt.name, s.codec.name())));
        }
        let partial = format!("{output}.partial");
        let written = (|| -> Result<(u64, f64, f64)> {
            let mut mux = ironav_formats::create_muxer(Path::new(&partial), fmt.name)?;
            mux.write_header(&streams)?;
            // timestamps start at zero in the clip
            let base: BTreeMap<usize, i64> = packets.iter().fold(BTreeMap::new(), |mut m, p| {
                m.entry(p.stream).or_insert(p.pts);
                m
            });
            let (mut first, mut last) = (f64::INFINITY, 0f64);
            for p in &packets {
                let tb = streams[p.stream].time_base.as_f64();
                first = first.min(p.pts as f64 * tb);
                last = last.max((p.pts + p.duration) as f64 * tb);
                let mut q = p.clone();
                q.pts -= base[&p.stream];
                mux.write_packet(&q)?;
            }
            mux.finish()?;
            Ok((packets.len() as u64, first, last))
        })();
        match written {
            Ok((n, first, last)) => {
                // replaced in one step: the old clip stays whole until the new one takes its place
                ironav_core::replace_file(Path::new(&partial), path)?;
                Ok(json!({ "session": self.id, "output": output, "format": fmt.name, "packets": n, "from": (first * 1000.0).round() / 1000.0, "to": (last * 1000.0).round() / 1000.0 }))
            }
            Err(e) => {
                let _ = std::fs::remove_file(&partial);
                Err(e)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 20 frames at 10 fps: dark for one second, then bright, so there is a cut at 1 s.
    fn y4m_with_cut() -> Vec<u8> {
        let mut f = b"YUV4MPEG2 W64 H64 F10:1 Ip C420jpeg\n".to_vec();
        for i in 0..20 {
            f.extend(b"FRAME\n");
            f.extend(vec![if i < 10 { 30u8 } else { 220 }; 64 * 64]);
            f.extend(vec![128u8; 2 * 32 * 32]);
        }
        f
    }

    /// Live tests take turns: one of them fills every session slot.
    fn turn() -> std::sync::MutexGuard<'static, ()> {
        static T: Mutex<()> = Mutex::new(());
        lock(&T)
    }

    fn args(source: &str) -> OpenArgs {
        serde_json::from_value(json!({ "source": source, "analyzers": ["scene", "structure"] })).unwrap()
    }

    fn wait_end(s: &Session) {
        let t0 = Instant::now();
        while !matches!(s.status()["state"].as_str(), Some("ended" | "failed")) {
            assert!(t0.elapsed() < Duration::from_secs(30), "session did not end");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_file_session_observes_snapshots_clips_and_closes() {
        let _turn = turn();
        let dir = std::env::temp_dir().join(format!("ironav-live-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("in.y4m");
        std::fs::write(&src, y4m_with_cut()).unwrap();
        let s = open(&args(src.to_str().unwrap())).unwrap();
        // the long poll returns as soon as the cut is logged
        let r = s.events(0, &["scene_cut".into()], Level::Events, 10, Duration::from_secs(20));
        let cut = &r["events"][0];
        assert_eq!(cut["kind"], "scene_cut");
        assert!((cut["t"].as_f64().unwrap() - 1.0).abs() < 1e-6, "{cut}");
        assert!(cut["wall"].as_str().is_some_and(|w| w.ends_with('Z')));
        wait_end(&s);
        let st = s.status();
        assert_eq!((st["format"].as_str(), st["packets"].as_u64()), (Some("y4m"), Some(20)), "{st}");
        // nothing after the last seq: an empty answer, not a wait, once the stream has ended
        let last = st["log"]["last_seq"].as_u64().unwrap();
        let r = s.events(last, &[], Level::Raw, 10, Duration::from_secs(20));
        assert_eq!((r["events"].as_array().unwrap().len(), r["next_after"].as_u64()), (0, Some(last)));

        let snap = s.snapshot(Some(1.5), Some(32)).unwrap();
        assert_eq!((snap["snapshot"]["t"].as_f64(), snap["snapshot"]["width"].as_u64()), (Some(1.5), Some(32)), "{snap}");
        assert!(s.snapshot(None, None).unwrap()["snapshot"]["png_base64"].as_str().is_some_and(|b| b.len() > 20));

        let out = dir.join("clip.y4m");
        let _ = std::fs::remove_file(&out);
        let c = s.clip(0.5, 1.5, out.to_str().unwrap(), None, false).unwrap();
        assert_eq!(c["packets"], 10, "{c}");
        let info = ironav_formats::open_demuxer(&out).unwrap();
        assert!(matches!(info.streams()[0].params, ironav_core::StreamParams::Video { width: 64, .. }));
        let e = s.clip(0.5, 1.5, out.to_str().unwrap(), None, false).unwrap_err();
        assert_eq!(e.code, ErrorCode::OutputExists);
        assert!(s.clip(0.5, 1.5, dir.join("x.wav").to_str().unwrap(), None, false).is_err());

        let id = s.id.clone();
        let closed = close(&id).unwrap();
        assert_eq!(closed["state"], "ended");
        assert_eq!(closed["summary"]["video0"]["scene"]["cuts"], 1, "{closed}");
        assert_eq!(get(&id).err().map(|e| e.code), Some(ErrorCode::NotFound));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_tcp_session_reads_what_a_peer_sends() {
        let _turn = turn();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let s = open(&args(&format!("tcp://127.0.0.1:{port}"))).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        let data = y4m_with_cut();
        // in pieces, so frames straddle reads
        for c in data.chunks(1000) {
            std::io::Write::write_all(&mut peer, c).unwrap();
        }
        drop(peer);
        wait_end(&s);
        let st = close(&s.id).unwrap();
        assert_eq!((st["source"]["kind"].as_str(), st["packets"].as_u64()), (Some("tcp"), Some(20)), "{st}");
        assert_eq!(st["bytes_in"].as_u64(), Some(data.len() as u64));
    }

    #[test]
    fn a_transport_stream_over_tcp_gives_its_video_and_audio() {
        let _turn = turn();
        let data = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../ironav-formats/tests/data/h264_aac.ts")).unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let a: OpenArgs = serde_json::from_value(json!({ "source": format!("tcp://127.0.0.1:{port}"), "analyzers": ["structure", "levels"], "hwaccel": "off" })).unwrap();
        let s = open(&a).unwrap();
        let (mut peer, _) = listener.accept().unwrap();
        for c in data.chunks(1316) {
            std::io::Write::write_all(&mut peer, c).unwrap();
        }
        drop(peer);
        wait_end(&s);
        let st = close(&s.id).unwrap();
        assert_eq!(st["format"], "mpegts", "{st}");
        assert_eq!(st["streams"].as_array().map(Vec::len), Some(2));
        assert_eq!(st["packets"].as_u64(), Some(145));
        assert!(st["summary"]["audio1"]["levels"].is_object(), "{st}");
    }

    fn record_ts_fixture(dir: &Path, record: &str, extra: Value) -> Value {
        let src = concat!(env!("CARGO_MANIFEST_DIR"), "/../ironav-formats/tests/data/h264_aac.ts");
        let mut a = json!({ "source": src, "analyzers": ["structure"], "record": dir.join(record).to_str().unwrap() });
        for (k, v) in extra.as_object().unwrap() {
            a[k] = v.clone();
        }
        let s = open(&serde_json::from_value(a).unwrap()).unwrap();
        wait_end(&s);
        close(&s.id).unwrap()
    }

    fn scratch(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("ironav-rec-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_session_records_to_one_file() {
        let _turn = turn();
        let dir = scratch("file");
        for name in ["out.ts", "out.mp4"] {
            let st = record_ts_fixture(&dir, name, json!({}));
            assert_eq!(st["record"]["error"], Value::Null, "{st}");
            assert_eq!(st["record"]["packets"], 145, "{st}");
            let mut d = ironav_formats::open_demuxer(&dir.join(name)).unwrap();
            assert_eq!(d.streams().len(), 2);
            let n = std::iter::from_fn(|| d.read_packet().unwrap()).count();
            assert_eq!(n, 145, "{name}");
        }
        // an existing file is not replaced unless asked
        let src = concat!(env!("CARGO_MANIFEST_DIR"), "/../ironav-formats/tests/data/h264_aac.ts");
        let a: OpenArgs = serde_json::from_value(json!({ "source": src, "record": dir.join("out.ts").to_str().unwrap() })).unwrap();
        assert_eq!(open(&a).err().map(|e| e.code), Some(ErrorCode::OutputExists));
        // a Y4M source cannot go into a transport stream; the session goes on and says why
        let y4m = dir.join("in.y4m");
        std::fs::write(&y4m, y4m_with_cut()).unwrap();
        let a: OpenArgs = serde_json::from_value(json!({ "source": y4m.to_str().unwrap(), "analyzers": ["scene"], "record": dir.join("y.ts").to_str().unwrap() })).unwrap();
        let s = open(&a).unwrap();
        wait_end(&s);
        let st = close(&s.id).unwrap();
        assert_eq!(st["state"], "ended");
        assert_eq!(st["record"]["error"]["code"], "unsupported-format", "{st}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_session_records_hls_segments_and_a_playlist() {
        let _turn = turn();
        let dir = scratch("hls");
        let st = record_ts_fixture(&dir, "live.m3u8", json!({ "segment_seconds": 1 }));
        assert_eq!(st["record"]["error"], Value::Null, "{st}");
        let m3u8 = std::fs::read_to_string(dir.join("live.m3u8")).unwrap();
        assert!(m3u8.starts_with("#EXTM3U
") && m3u8.ends_with("#EXT-X-ENDLIST
"), "{m3u8}");
        assert!(m3u8.contains("#EXT-X-PLAYLIST-TYPE:EVENT"));
        let durations: Vec<f64> = m3u8.lines().filter_map(|l| l.strip_prefix("#EXTINF:")).map(|d| d.trim_end_matches(',').parse().unwrap()).collect();
        assert_eq!(durations.len(), 2, "{m3u8}");
        // keyframes every 0.48 s: the first segment is cut at the first one a second or more in
        assert!(durations[0] >= 1.0 && durations[0] < 1.5, "{durations:?}");
        assert!((durations.iter().sum::<f64>() - 2.03).abs() < 0.05, "{durations:?}");
        let mut packets = 0;
        for name in m3u8.lines().filter(|l| l.ends_with(".ts")) {
            let mut d = ironav_formats::open_demuxer(&dir.join(name)).unwrap();
            let first = d.read_packet().unwrap().unwrap();
            packets += 1 + std::iter::from_fn(|| d.read_packet().unwrap()).count();
            // each segment starts with a picture a decoder can start from
            assert!(d.streams().len() == 2 && first.data.windows(4).any(|w| w == [0, 0, 1, 0x65]) || first.stream != 0, "{name}");
        }
        // everything: the first video keyframe is the first packet in the file
        assert_eq!(packets, 145);
        let _ = std::fs::remove_dir_all(&dir);

        // a sliding window keeps only the newest segment
        let dir = scratch("window");
        let st = record_ts_fixture(&dir, "w.m3u8", json!({ "segment_seconds": 1, "keep_segments": 1 }));
        assert_eq!(st["record"]["segments"]["first_seq"], 1, "{st}");
        let m3u8 = std::fs::read_to_string(dir.join("w.m3u8")).unwrap();
        assert!(m3u8.contains("#EXT-X-MEDIA-SEQUENCE:1") && !m3u8.contains("EVENT"), "{m3u8}");
        assert!(!dir.join("w_00000.ts").exists() && dir.join("w_00001.ts").exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn sessions_are_limited_and_arguments_checked() {
        let _turn = turn();
        let mut opened = Vec::new();
        let mut busy = None;
        // listening sessions stay open until closed; other tests may hold some of the slots
        for _ in 0..=MAX_SESSIONS {
            match open(&args("tcp://127.0.0.1:0?listen")) {
                Ok(s) => opened.push(s.id.clone()),
                Err(e) => {
                    busy = Some(e);
                    break;
                }
            }
        }
        for id in &opened {
            assert_eq!(close(id).unwrap()["state"], "closed");
        }
        assert_eq!(busy.expect("the limit was not reached").code, ErrorCode::Busy);
        assert_eq!(open(&serde_json::from_value(json!({ "source": "srt://cam:9000" })).unwrap()).err().map(|e| e.code), Some(ErrorCode::InvalidArgument));
        let bad: OpenArgs = serde_json::from_value(json!({ "source": "-", "analyzers": ["nope"] })).unwrap();
        assert_eq!(open(&bad).err().map(|e| e.code), Some(ErrorCode::InvalidArgument));
        assert!(serde_json::from_value::<OpenArgs>(json!({ "source": "-", "colour": 1 })).is_err());
    }
}
