//! Model sidecars: analyzers that run as child processes speaking JSON lines, so IronAV links no
//! model runtime and a crash or a leak stays in the child. Whoever runs IronAV lists them in a JSON
//! file named by `IRONAV_SIDECARS`; a request names one and cannot start anything else.
//!
//! The child says what it wants first, then IronAV sends it decoded media and it answers with
//! observations:
//!
//! ```text
//! child:  {"op":"hello","protocol":1,"wants":{"audio":{"rate":16000,"format":"s16","chunk_seconds":5,"gate":"speech"}}}
//! ironav: {"op":"start","protocol":1,"name":"asr","live":false,"stream":{"index":1,"media":"audio","codec":"aac",...}}
//! ironav: {"op":"audio","t":12.48,"dur":2.0,"rate":16000,"format":"s16","final":false,"data":"<base64>"}
//! child:  {"op":"observe","kind":"transcript","t":12.48,"t_end":14.4,"data":{"text":"..."}}
//! ironav: {"op":"end"}
//! ```
//!
//! For files the analysis waits for the child, so nothing is skipped; for live sources inputs are
//! dropped while it is behind. A child that crashes, or takes nothing for `hang_seconds`, is
//! restarted with back-off.

use ironav_analyze::{intern, kind_info, Analyzer, Input, Observation, VoiceActivity, ANALYZERS};
use ironav_core::{AudioFrame, Error, ErrorCode, Filter, Frame, MediaType, Result, StreamInfo, StreamParams, VideoFrame};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, VecDeque};
use std::io::{BufRead, BufReader, Read, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender, TrySendError};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// The environment variable naming the sidecar list.
pub const CONFIG_VAR: &str = "IRONAV_SIDECARS";
const PROTOCOL: u64 = 1;
/// The longest line a child may write.
const MAX_LINE: usize = 256 << 10;
/// Inputs waiting for a child.
const QUEUE: usize = 16;
/// Observations from a child waiting to be logged; a child that writes faster waits.
const EVENTS: usize = 256;
const MAX_RESTARTS: u32 = 5;
/// Transcript text kept per digest and in the summary.
const MAX_TEXT: usize = 4000;
/// Audio kept from before speech starts, so its first syllable is not cut off.
const PREROLL: f64 = 0.25;
/// Audio kept while nobody speaks: the pre-roll and the voice-activity detector's delay.
const IDLE_KEEP: f64 = 1.0;
/// Lines of a child's error output kept for `analyzer_failed`.
const STDERR_LINES: usize = 8;
const LAG_REPORT: Duration = Duration::from_secs(10);
/// A child that has run this long has earned its restarts back.
const HEALTHY: Duration = Duration::from_secs(300);
/// A jump in the audio's timestamps larger than this starts its timeline again.
const MAX_SLIP: f64 = 0.05;

/// One entry of the sidecar list.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SidecarConfig {
    /// The program and its arguments.
    pub command: Vec<String>,
    /// How long it may take to say hello (loading a model can be slow).
    #[serde(default = "default_start")]
    pub start_seconds: f64,
    /// How long it may take to finish after the end of the input.
    #[serde(default = "default_finish")]
    pub finish_seconds: f64,
    /// A child that takes nothing for this long while inputs wait is restarted.
    #[serde(default = "default_hang")]
    pub hang_seconds: f64,
}

fn default_start() -> f64 {
    30.0
}
fn default_finish() -> f64 {
    120.0
}
fn default_hang() -> f64 {
    60.0
}

/// Reads and checks a sidecar list: a JSON object from names to [`SidecarConfig`]s.
pub fn load(path: &str) -> Result<BTreeMap<String, SidecarConfig>> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::new(ErrorCode::Io, format!("{CONFIG_VAR}: cannot read {path}: {e}")))?;
    let list: BTreeMap<String, SidecarConfig> = serde_json::from_str(&text).map_err(|e| Error::invalid_arg(format!("{CONFIG_VAR}: {path} is not a sidecar list: {e}")))?;
    for (name, c) in &list {
        let ok = !name.is_empty() && name.len() <= 32 && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-');
        if !ok || ANALYZERS.iter().any(|a| a.name == name) || matches!(name.as_str(), "digest" | "summary" | "decoder") {
            return Err(Error::invalid_arg(format!("{CONFIG_VAR}: '{name}' cannot name a sidecar")).hint("use up to 32 lowercase letters, digits, '_' or '-', other than a built-in analyzer's name"));
        }
        if c.command.first().is_none_or(|p| p.is_empty()) {
            return Err(Error::invalid_arg(format!("{CONFIG_VAR}: sidecar '{name}' has no command")));
        }
        for (field, v) in [("start_seconds", c.start_seconds), ("finish_seconds", c.finish_seconds), ("hang_seconds", c.hang_seconds)] {
            if !(v > 0.0 && v <= 86400.0) {
                return Err(Error::invalid_arg(format!("{CONFIG_VAR}: sidecar '{name}': {field} must be above 0 and at most a day")));
            }
        }
    }
    Ok(list)
}

/// The sidecars the operator configured; none when the variable is unset.
pub fn configured() -> Result<BTreeMap<String, SidecarConfig>> {
    match std::env::var(CONFIG_VAR) {
        Ok(path) if !path.trim().is_empty() => load(path.trim()),
        _ => Ok(BTreeMap::new()),
    }
}

/// Looks up `names` in the configured list.
pub fn resolve(names: &[String]) -> Result<Vec<(&'static str, SidecarConfig)>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let list = configured()?;
    let mut out: Vec<(&'static str, SidecarConfig)> = Vec::new();
    for n in names {
        let Some(c) = list.get(n) else {
            return Err(Error::invalid_arg(format!("no sidecar named '{n}' is configured"))
                .alternatives(list.keys())
                .hint(format!("whoever runs IronAV lists sidecars in a JSON file named by {CONFIG_VAR}")));
        };
        let name = intern(n).ok_or_else(|| Error::invalid_arg("too many distinct sidecar names"))?;
        if !out.iter().any(|(m, _)| *m == name) {
            out.push((name, c.clone()));
        }
    }
    Ok(out)
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum SampleFormat {
    S16,
    F32,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum PictureFormat {
    Png,
    Rgb24,
}

/// What a child asked for in its hello.
#[derive(Debug, Clone, PartialEq)]
enum Wants {
    Audio { rate: u32, format: SampleFormat, chunk: f64, gate_speech: bool },
    Video { every: f64, width: u32, format: PictureFormat },
}

impl Wants {
    fn media(&self) -> MediaType {
        match self {
            Wants::Audio { .. } => MediaType::Audio,
            Wants::Video { .. } => MediaType::Video,
        }
    }

    fn parse(hello: &Value) -> std::result::Result<Wants, String> {
        if hello["op"] != "hello" {
            return Err("its first line was not a hello".into());
        }
        if let Some(p) = hello.get("protocol") {
            if p.as_u64() != Some(PROTOCOL) {
                return Err(format!("it speaks protocol {p}; IronAV speaks {PROTOCOL}"));
            }
        }
        let num = |v: &Value, key: &str, default: f64, lo: f64, hi: f64| -> std::result::Result<f64, String> {
            match v.get(key) {
                None | Some(Value::Null) => Ok(default),
                Some(x) => x.as_f64().filter(|x| (lo..=hi).contains(x)).ok_or(format!("'{key}' must be a number from {lo} to {hi}")),
            }
        };
        let w = &hello["wants"];
        match (w.get("audio"), w.get("video")) {
            (Some(a), None) => {
                let format = match a.get("format").and_then(Value::as_str).unwrap_or("s16") {
                    "s16" => SampleFormat::S16,
                    "f32" => SampleFormat::F32,
                    f => return Err(format!("audio format '{f}' is not s16 or f32")),
                };
                if a.get("channels").is_some_and(|c| c.as_u64() != Some(1)) {
                    return Err("audio is sent as one channel".into());
                }
                let gate_speech = match a.get("gate").and_then(Value::as_str) {
                    None | Some("none") => false,
                    Some("speech") => true,
                    Some(g) => return Err(format!("audio gate '{g}' is not speech or none")),
                };
                Ok(Wants::Audio { rate: num(a, "rate", 16000.0, 8000.0, 192000.0)? as u32, format, chunk: num(a, "chunk_seconds", 2.0, 0.1, 60.0)?, gate_speech })
            }
            (None, Some(v)) => {
                let format = match v.get("format").and_then(Value::as_str).unwrap_or("png") {
                    "png" => PictureFormat::Png,
                    "rgb24" => PictureFormat::Rgb24,
                    f => return Err(format!("picture format '{f}' is not png or rgb24")),
                };
                Ok(Wants::Video { every: num(v, "every_seconds", 1.0, 0.01, 3600.0)?, width: num(v, "width", 320.0, 16.0, 4096.0)? as u32, format })
            }
            _ => Err("its hello must want either audio or video".into()),
        }
    }
}

/// What the analyzer hands the supervisor thread.
enum In {
    Line(String),
    /// The child of this generation closed its output.
    Gone(u32),
    End,
}

/// What comes back from the child and the supervisor.
enum Event {
    Observe(Value),
    Failed { message: String, restarting: bool },
    /// The child finished after the end of the input (or was stopped).
    Finished,
}

/// State the analyzer and the supervisor share.
struct Shared {
    child: Mutex<Option<Child>>,
    closed: AtomicBool,
}

impl Shared {
    fn kill(&self) {
        if let Some(c) = self.child.lock().unwrap_or_else(|e| e.into_inner()).as_mut() {
            let _ = c.kill();
        }
    }

    /// Takes the child out, so waiting for it holds no lock.
    fn take(&self) -> Option<Child> {
        self.child.lock().unwrap_or_else(|e| e.into_inner()).take()
    }
}

/// Waits for `c` to exit until `deadline`, then kills it; returns its exit status.
fn reap(mut c: Child, deadline: Instant) -> Option<std::process::ExitStatus> {
    while Instant::now() < deadline {
        match c.try_wait() {
            Ok(Some(s)) => return Some(s),
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
            Err(_) => break,
        }
    }
    let _ = c.kill();
    c.wait().ok()
}

/// A running child: what the supervisor writes to, and the end of its error output.
struct Running {
    stdin: Option<ChildStdin>,
    stderr: Arc<Mutex<VecDeque<String>>>,
    gen: u32,
}

impl Running {
    /// Writes one line; false when the child is gone.
    fn send(&mut self, line: &str) -> bool {
        self.stdin.as_mut().is_some_and(|w| writeln!(w, "{line}").and_then(|_| w.flush()).is_ok())
    }

    fn tail(&self) -> String {
        self.stderr.lock().unwrap_or_else(|e| e.into_inner()).iter().cloned().collect::<Vec<_>>().join(" | ")
    }
}

/// Channels between the analyzer, the supervisor and the reader threads.
struct Pipes {
    tx: SyncSender<In>,
    rx: Receiver<In>,
    etx: SyncSender<Event>,
    erx: Receiver<Event>,
}

/// Starts the child and waits for its hello; then sends it `start`, when given.
fn spawn(name: &str, cfg: &SidecarConfig, start: Option<&Value>, gen: u32, shared: &Shared, sup: &SyncSender<In>, events: &SyncSender<Event>) -> Result<(Running, Wants)> {
    let mut cmd = Command::new(&cfg.command[0]);
    cmd.args(&cfg.command[1..]).stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: no console window for a child of a windowless process
        cmd.creation_flags(0x0800_0000);
    }
    let mut child = cmd.spawn().map_err(|e| Error::new(ErrorCode::Io, format!("sidecar '{name}': cannot start {}: {e}", cfg.command[0])))?;
    let stdin = child.stdin.take();
    let stdout = child.stdout.take().unwrap();
    let stderr_pipe = child.stderr.take().unwrap();
    let stderr = Arc::new(Mutex::new(VecDeque::new()));
    let tail = stderr.clone();
    std::thread::spawn(move || {
        // read to the end, whatever it holds, so the child never blocks on a full pipe
        let mut r = BufReader::new(stderr_pipe);
        loop {
            let mut line = Vec::new();
            match (&mut r).take(4096).read_until(b'\n', &mut line) {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
            let text: String = String::from_utf8_lossy(&line).trim_end().chars().take(300).collect();
            if text.is_empty() {
                continue;
            }
            let mut t = tail.lock().unwrap_or_else(|e| e.into_inner());
            t.push_back(text);
            if t.len() > STDERR_LINES {
                t.pop_front();
            }
        }
    });
    let (hello_tx, hello_rx) = sync_channel(1);
    let (sup, events) = (sup.clone(), events.clone());
    std::thread::spawn(move || read_child(stdout, hello_tx, events, sup, gen));
    *shared.child.lock().unwrap_or_else(|e| e.into_inner()) = Some(child);
    let mut running = Running { stdin, stderr, gen };
    let fail = |running: &Running, msg: String| {
        shared.kill();
        let tail = running.tail();
        let e = Error::new(ErrorCode::Io, format!("sidecar '{name}' {msg}"));
        if tail.is_empty() { e } else { e.hint(format!("its error output ended with: {tail}")) }
    };
    let hello = match hello_rx.recv_timeout(Duration::from_secs_f64(cfg.start_seconds)) {
        Ok(Ok(v)) => v,
        Ok(Err(m)) => {
            // give its error output a moment to arrive
            std::thread::sleep(Duration::from_millis(100));
            return Err(fail(&running, m));
        }
        Err(_) => return Err(fail(&running, format!("did not say hello within {} s", cfg.start_seconds))),
    };
    let wants = Wants::parse(&hello).map_err(|m| fail(&running, format!("sent a hello IronAV cannot use: {m}")))?;
    if let Some(start) = start {
        if !running.send(&start.to_string()) {
            return Err(fail(&running, "exited after its hello".into()));
        }
    }
    Ok((running, wants))
}

/// Reads the child's lines: the first is its hello, then observations.
fn read_child(out: impl Read, hello: SyncSender<std::result::Result<Value, String>>, events: SyncSender<Event>, sup: SyncSender<In>, gen: u32) {
    let mut r = BufReader::new(out);
    let mut first = true;
    loop {
        let mut line = Vec::new();
        match (&mut r).take(MAX_LINE as u64 + 1).read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {}
        }
        if line.len() > MAX_LINE {
            // there is no finding the next line again: treat it as a crash
            break;
        }
        let Ok(v) = serde_json::from_slice::<Value>(&line) else { continue };
        if first {
            first = false;
            let _ = hello.send(Ok(v));
            continue;
        }
        if v["op"] == "observe" && events.send(Event::Observe(v)).is_err() {
            break;
        }
    }
    if first {
        let _ = hello.send(Err("exited before saying hello".into()));
    }
    let _ = sup.send(In::Gone(gen));
}

/// Owns the child: writes the analyzer's lines to it, and restarts it with back-off when it dies.
#[allow(clippy::too_many_arguments)]
fn supervise(name: &'static str, cfg: SidecarConfig, wants: Wants, start: Value, first: Running, rx: Receiver<In>, sup: SyncSender<In>, events: SyncSender<Event>, shared: Arc<Shared>) {
    let mut cur = Some(first);
    let mut restarts = 0;
    let mut gen = 0;
    let mut started = Instant::now();
    let mut ending: Option<Instant> = None;
    let finish = Duration::from_secs_f64(cfg.finish_seconds);
    loop {
        // it holds a sender itself (for the readers), so the analyzer's going is seen through `closed`
        if shared.closed.load(Ordering::Relaxed) || ending.is_some_and(|t0| t0.elapsed() >= finish) {
            break;
        }
        let msg = match rx.recv_timeout(Duration::from_millis(200)) {
            Ok(m) => m,
            Err(RecvTimeoutError::Timeout) => continue,
            Err(RecvTimeoutError::Disconnected) => break,
        };
        let failed = match msg {
            In::Line(line) => ending.is_none() && cur.as_mut().is_some_and(|r| !r.send(&line)),
            In::Gone(g) => cur.as_ref().is_some_and(|r| r.gen == g),
            In::End => {
                ending = Some(Instant::now());
                match cur.as_mut() {
                    Some(r) => {
                        r.send(&json!({ "op": "end" }).to_string());
                        // closing its input tells a simple child the same thing
                        r.stdin = None;
                        false
                    }
                    None => break,
                }
            }
        };
        if !failed {
            continue;
        }
        let Some(r) = cur.take() else { continue };
        // after the end it may still be writing its last words; otherwise it is stopped now
        let deadline = match ending {
            Some(t0) => t0 + finish,
            None => Instant::now(),
        };
        let status = shared.take().and_then(|c| reap(c, deadline));
        if ending.is_some() {
            // a child that ends after the input did is done, whatever its exit status
            break;
        }
        // its last words may still be on their way
        std::thread::sleep(Duration::from_millis(50));
        let tail = r.tail();
        let mut message = match status {
            Some(s) => format!("sidecar '{name}' stopped ({s})"),
            None => format!("sidecar '{name}' stopped"),
        };
        if !tail.is_empty() {
            message += &format!("; its error output ended with: {tail}");
        }
        if started.elapsed() >= HEALTHY {
            restarts = 0;
        }
        // restart, with back-off, until it says hello again or the restarts run out
        loop {
            let restarting = restarts < MAX_RESTARTS && !shared.closed.load(Ordering::Relaxed);
            if events.send(Event::Failed { message: message.clone(), restarting }).is_err() || !restarting {
                break;
            }
            let wait = Duration::from_secs(1 << restarts);
            restarts += 1;
            let t0 = Instant::now();
            while t0.elapsed() < wait && !shared.closed.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(20));
            }
            if shared.closed.load(Ordering::Relaxed) {
                break;
            }
            gen += 1;
            match spawn(name, &cfg, Some(&start), gen, &shared, &sup, &events) {
                Ok((r, w)) if w == wants => {
                    cur = Some(r);
                    started = Instant::now();
                    break;
                }
                Ok(_) => {
                    if let Some(c) = shared.take() {
                        reap(c, Instant::now());
                    }
                    message = format!("sidecar '{name}' asked for different media after a restart");
                }
                Err(e) => message = e.message.clone() + &e.hint.map(|h| format!("; {h}")).unwrap_or_default(),
            }
        }
    }
    if let Some(c) = shared.take() {
        reap(c, Instant::now());
    }
    let _ = events.send(Event::Finished);
}

/// A sidecar that has said hello, before it is given a stream.
pub struct Pending {
    name: &'static str,
    cfg: SidecarConfig,
    live: bool,
    wants: Wants,
    shared: Arc<Shared>,
    running: Option<(Running, Pipes)>,
}

impl Pending {
    /// Starts `name` and waits for its hello. `live` sources drop inputs a slow child cannot take.
    pub fn start(name: &'static str, cfg: &SidecarConfig, live: bool) -> Result<Pending> {
        let shared = Arc::new(Shared { child: Mutex::new(None), closed: AtomicBool::new(false) });
        let (tx, rx) = sync_channel(QUEUE);
        let (etx, erx) = sync_channel(EVENTS);
        let (running, wants) = spawn(name, cfg, None, 0, &shared, &tx, &etx)?;
        Ok(Pending { name, cfg: cfg.clone(), live, wants, shared, running: Some((running, Pipes { tx, rx, etx, erx })) })
    }

    pub fn name(&self) -> &'static str {
        self.name
    }

    /// The kind of stream it wants.
    pub fn media(&self) -> MediaType {
        self.wants.media()
    }

    /// Gives it stream `s`: it is told which, and the media starts to flow.
    pub fn attach(mut self, s: &StreamInfo) -> Result<Sidecar> {
        let (mut running, p) = self.running.take().unwrap();
        let mut stream = json!({ "index": s.index, "codec": s.codec.name() });
        match &s.params {
            StreamParams::Audio { sample_rate, channels } => {
                stream["media"] = json!("audio");
                stream["sample_rate"] = json!(sample_rate);
                stream["channels"] = json!(channels);
            }
            StreamParams::Video { width, height, frame_rate, .. } => {
                stream["media"] = json!("video");
                stream["width"] = json!(width);
                stream["height"] = json!(height);
                stream["frame_rate"] = json!(frame_rate.as_f64());
            }
        }
        let start = json!({ "op": "start", "protocol": PROTOCOL, "name": self.name, "live": self.live, "stream": stream });
        if !running.send(&start.to_string()) {
            self.shared.kill();
            return Err(Error::new(ErrorCode::Io, format!("sidecar '{}' exited after its hello", self.name)));
        }
        let (name, cfg, shared) = (self.name, self.cfg.clone(), self.shared.clone());
        let (sup, etx) = (p.tx.clone(), p.etx.clone());
        let wants = self.wants.clone();
        std::thread::spawn(move || supervise(name, cfg, wants, start, running, p.rx, sup, etx, shared));
        Ok(Sidecar {
            name: self.name,
            stream: s.index,
            wants: self.wants.clone(),
            live: self.live,
            hang: Duration::from_secs_f64(self.cfg.hang_seconds),
            finish: Duration::from_secs_f64(self.cfg.finish_seconds),
            tx: p.tx,
            events: p.erx,
            shared: self.shared.clone(),
            audio: AudioPath::default(),
            held: Vec::new(),
            next_picture: None,
            window: Stats::default(),
            total: Stats::default(),
            full_since: None,
            lag: (0, None),
            restarts: 0,
            gave_up: false,
            latest: 0.0,
        })
    }
}

impl Drop for Pending {
    fn drop(&mut self) {
        if self.running.is_some() {
            self.shared.kill();
        }
    }
}

#[derive(Default)]
struct Stats {
    observations: u64,
    kinds: BTreeMap<&'static str, u64>,
    text: String,
    dropped: u64,
    rejected: u64,
}

impl Stats {
    fn add_text(&mut self, s: &str) {
        let s = s.trim();
        if s.is_empty() || self.text.len() >= MAX_TEXT {
            return;
        }
        if !self.text.is_empty() {
            self.text.push(' ');
        }
        for c in s.chars() {
            if self.text.len() + c.len_utf8() > MAX_TEXT {
                break;
            }
            self.text.push(c);
        }
    }

    fn json(&self) -> Value {
        let mut m = Map::new();
        m.insert("observations".into(), json!(self.observations));
        if !self.kinds.is_empty() {
            m.insert("kinds".into(), json!(self.kinds));
        }
        if !self.text.is_empty() {
            m.insert("text".into(), json!(self.text));
        }
        if self.dropped > 0 {
            m.insert("dropped_inputs".into(), json!(self.dropped));
        }
        if self.rejected > 0 {
            m.insert("rejected".into(), json!(self.rejected));
        }
        Value::Object(m)
    }
}

/// Audio on its way to a child: mono at the rate it asked for, from `buf_t` on.
#[derive(Default)]
struct AudioPath {
    resample: Option<Box<dyn Filter>>,
    vad: Option<VoiceActivity>,
    /// Where the next input frame is expected, to notice gaps.
    expect: Option<f64>,
    buf: VecDeque<f32>,
    buf_t: f64,
    /// Inside speech (always, without a gate).
    sending: bool,
}

/// Whether `k` has the form of a sidecar's observation kind: a short lowercase name.
pub fn is_kind_name(k: &str) -> bool {
    let b = k.as_bytes();
    (1..=48).contains(&b.len()) && b[0].is_ascii_lowercase() && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

/// A kind a child may report: a sidecar kind name that is not one of IronAV's own.
fn valid_kind(k: &str) -> bool {
    is_kind_name(k) && kind_info(k).is_none()
}

/// A sidecar attached to one stream, as an analyzer.
pub struct Sidecar {
    name: &'static str,
    stream: usize,
    wants: Wants,
    live: bool,
    hang: Duration,
    finish: Duration,
    tx: SyncSender<In>,
    events: Receiver<Event>,
    shared: Arc<Shared>,
    audio: AudioPath,
    /// Observations taken from the child while waiting to hand it input, logged with the next input.
    held: Vec<Observation>,
    next_picture: Option<f64>,
    window: Stats,
    total: Stats,
    full_since: Option<Instant>,
    /// Inputs dropped since the last `analyzer_lagging`, and when that was.
    lag: (u64, Option<Instant>),
    restarts: u64,
    gave_up: bool,
    latest: f64,
}

impl Sidecar {
    fn dropped(&mut self) {
        self.window.dropped += 1;
        self.total.dropped += 1;
        self.lag.0 += 1;
    }

    /// Hands a line to the supervisor. A file's analysis waits for room; a live one drops the
    /// line. Either way a child that takes nothing for `hang` is killed, and so restarted.
    fn deliver(&mut self, line: String) {
        if self.gave_up {
            return self.dropped();
        }
        let mut msg = In::Line(line);
        let t0 = Instant::now();
        loop {
            match self.tx.try_send(msg) {
                Ok(()) => {
                    self.full_since = None;
                    return;
                }
                Err(TrySendError::Disconnected(_)) => {
                    self.gave_up = true;
                    return self.dropped();
                }
                Err(TrySendError::Full(m)) => {
                    let since = *self.full_since.get_or_insert(t0);
                    if since.elapsed() > self.hang {
                        self.shared.kill();
                        self.full_since = None;
                        return self.dropped();
                    }
                    if self.live {
                        return self.dropped();
                    }
                    msg = m;
                    let mut held = std::mem::take(&mut self.held);
                    while let Ok(e) = self.events.try_recv() {
                        self.handle(e, &mut held);
                    }
                    self.held = held;
                    std::thread::sleep(Duration::from_millis(2));
                }
            }
        }
    }

    /// Logs what came back; true once the child has finished.
    fn handle(&mut self, e: Event, out: &mut Vec<Observation>) -> bool {
        match e {
            Event::Observe(v) => self.observation(v, out),
            Event::Failed { message, restarting } => {
                if restarting {
                    self.restarts += 1;
                } else {
                    self.gave_up = true;
                }
                let mut o = Observation::new("analyzer_failed", self.name, self.latest, json!({ "message": message, "restarting": restarting }));
                o.stream = Some(self.stream);
                out.push(o);
            }
            Event::Finished => return true,
        }
        false
    }

    fn observation(&mut self, v: Value, out: &mut Vec<Observation>) {
        let kind = v["kind"].as_str().filter(|k| valid_kind(k)).and_then(intern);
        let t = v["t"].as_f64().filter(|t| t.is_finite() && *t >= 0.0);
        let t_end = match v.get("t_end") {
            None | Some(Value::Null) => Some(None),
            Some(e) => e.as_f64().filter(|e| e.is_finite() && t.is_some_and(|t| *e >= t)).map(Some),
        };
        let (Some(kind), Some(t), Some(t_end)) = (kind, t, t_end) else {
            self.window.rejected += 1;
            self.total.rejected += 1;
            return;
        };
        let data = v.get("data").cloned().unwrap_or_else(|| json!({}));
        if let Some(text) = data.get("text").and_then(Value::as_str) {
            self.window.add_text(text);
            self.total.add_text(text);
        }
        for s in [&mut self.window, &mut self.total] {
            s.observations += 1;
            *s.kinds.entry(kind).or_default() += 1;
        }
        let mut o = Observation::new(kind, self.name, t, data);
        if let Some(e) = t_end {
            o = o.until(e);
        }
        o.stream = Some(self.stream);
        out.push(o);
    }

    /// Sends the first `n` buffered samples.
    fn send_audio(&mut self, n: usize, last: bool) {
        let Wants::Audio { rate, format, .. } = self.wants else { return };
        let n = n.min(self.audio.buf.len());
        if n == 0 {
            return;
        }
        let t = self.audio.buf_t;
        let mut bytes = Vec::with_capacity(n * 4);
        for s in self.audio.buf.drain(..n) {
            match format {
                SampleFormat::S16 => bytes.extend_from_slice(&((s.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes()),
                SampleFormat::F32 => bytes.extend_from_slice(&s.to_le_bytes()),
            }
        }
        let dur = n as f64 / rate as f64;
        self.audio.buf_t += dur;
        let fmt = if format == SampleFormat::S16 { "s16" } else { "f32" };
        let line = json!({ "op": "audio", "t": round3(t), "dur": round3(dur), "rate": rate, "format": fmt, "final": last, "data": ironav_player::base64(&bytes) });
        self.deliver(line.to_string());
    }

    /// Drops samples from the front so the buffer starts at `t` (or keeps all when it already does).
    fn trim_to(&mut self, t: f64) {
        let Wants::Audio { rate, .. } = self.wants else { return };
        let n = (((t - self.audio.buf_t) * rate as f64).max(0.0) as usize).min(self.audio.buf.len());
        self.audio.buf.drain(..n);
        self.audio.buf_t += n as f64 / rate as f64;
    }

    fn audio(&mut self, frame: &AudioFrame, t: f64) {
        let Wants::Audio { rate, chunk, gate_speech, .. } = self.wants else { return };
        if frame.sample_rate == 0 {
            return;
        }
        let ch = frame.channels.max(1) as usize;
        let dur = frame.frames() as f64 / frame.sample_rate as f64;
        if self.audio.expect.is_none_or(|e| (t - e).abs() > MAX_SLIP) {
            // a gap or a jump: what was gathered goes out, and the timeline starts again
            if self.audio.sending {
                let n = self.audio.buf.len();
                self.send_audio(n, true);
            }
            self.audio.buf.clear();
            self.audio.buf_t = t;
            self.audio.resample = None;
            self.audio.vad = None;
            self.audio.sending = false;
        }
        self.audio.expect = Some(t + dur);
        let mono = AudioFrame { sample_rate: frame.sample_rate, channels: 1, pts: 0, samples: frame.samples.chunks(ch).map(|c| c.iter().sum::<f64>() / ch as f64).collect() };
        let samples = if frame.sample_rate == rate {
            mono.samples
        } else {
            let r = self.audio.resample.get_or_insert_with(|| ironav_codec::build_filter("resample", json!({ "rate": rate }).as_object().unwrap()).expect("a rate the filter takes"));
            match r.process(Frame::Audio(mono)) {
                Ok(frames) => frames.into_iter().flat_map(|f| if let Frame::Audio(a) = f { a.samples } else { Vec::new() }).collect(),
                // the input changed rate: start a new resampler with the next frame
                Err(_) => {
                    self.audio.resample = None;
                    Vec::new()
                }
            }
        };
        self.audio.buf.extend(samples.into_iter().map(|s| s as f32));
        let per_chunk = ((chunk * rate as f64) as usize).max(1);
        if !gate_speech {
            self.audio.sending = true;
        } else {
            let mut seen = Vec::new();
            self.audio.vad.get_or_insert_with(VoiceActivity::default).observe(&Input::Audio { frame, t }, &mut seen);
            for o in seen {
                if o.kind == "speech_start" {
                    self.audio.sending = true;
                    self.trim_to(o.t - PREROLL);
                } else if o.kind == "speech_end" {
                    // everything up to now, hangover and all, as the end of the utterance
                    while self.audio.buf.len() > per_chunk {
                        self.send_audio(per_chunk, false);
                    }
                    let n = self.audio.buf.len();
                    self.send_audio(n, true);
                    self.audio.sending = false;
                }
            }
            if !self.audio.sending {
                let keep_from = self.audio.buf_t + self.audio.buf.len() as f64 / rate as f64 - IDLE_KEEP;
                self.trim_to(keep_from);
            }
        }
        if self.audio.sending {
            while self.audio.buf.len() >= per_chunk {
                self.send_audio(per_chunk, false);
            }
        }
    }

    fn video(&mut self, frame: &VideoFrame, t: f64) {
        let Wants::Video { every, width, format } = self.wants else { return };
        if self.next_picture.is_some_and(|n| t < n - 1e-6) {
            return;
        }
        self.next_picture = Some(match self.next_picture {
            Some(n) if t < n + every => n + every,
            _ => t + every,
        });
        let (w, h, rgb) = ironav_player::rgb_frame(frame, Some(width), None);
        let (fmt, data) = match format {
            PictureFormat::Png => match ironav_player::to_png(w, h, &rgb) {
                Ok(png) => ("png", png),
                Err(_) => return,
            },
            PictureFormat::Rgb24 => ("rgb24", rgb),
        };
        let line = json!({ "op": "video", "t": round3(t), "width": w, "height": h, "format": fmt, "data": ironav_player::base64(&data) });
        self.deliver(line.to_string());
    }
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

impl Analyzer for Sidecar {
    fn observe(&mut self, input: &Input, out: &mut Vec<Observation>) {
        match input {
            Input::Audio { frame, t } => {
                self.latest = self.latest.max(*t);
                self.audio(frame, *t);
            }
            Input::Video { frame, t, .. } => {
                self.latest = self.latest.max(*t);
                self.video(frame, *t);
            }
            Input::Packet { .. } => {}
        }
        out.append(&mut self.held);
        while let Ok(e) = self.events.try_recv() {
            self.handle(e, out);
        }
        if self.live && self.lag.0 > 0 && self.lag.1.is_none_or(|l| l.elapsed() >= LAG_REPORT) {
            let mut o = Observation::new("analyzer_lagging", self.name, self.latest, json!({ "dropped": self.lag.0 }));
            o.stream = Some(self.stream);
            out.push(o);
            self.lag = (0, Some(Instant::now()));
        }
    }

    fn digest(&mut self, _seconds: f64) -> Option<Value> {
        Some(std::mem::take(&mut self.window).json())
    }

    fn finish(&mut self, _t: f64, out: &mut Vec<Observation>) -> Option<Value> {
        out.append(&mut self.held);
        if self.audio.sending {
            let n = self.audio.buf.len();
            self.send_audio(n, true);
        }
        // the end goes out even for a live source, and the child gets `finish_seconds` to answer
        let deadline = Instant::now() + self.finish + Duration::from_secs(2);
        let mut msg = In::End;
        while Instant::now() < deadline {
            match self.tx.try_send(msg) {
                Err(TrySendError::Full(m)) => {
                    msg = m;
                    std::thread::sleep(Duration::from_millis(2));
                }
                _ => break,
            }
        }
        while let Some(left) = deadline.checked_duration_since(Instant::now()) {
            match self.events.recv_timeout(left) {
                Ok(e) => {
                    if self.handle(e, out) {
                        break;
                    }
                }
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
            }
        }
        self.shared.kill();
        let mut v = self.total.json();
        if self.restarts > 0 {
            v["restarts"] = json!(self.restarts);
        }
        Some(v)
    }
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::Relaxed);
        self.shared.kill();
    }
}
