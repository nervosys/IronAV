//! IronAV's graphical front end: a local web app in the HandBrake mould (source, summary,
//! video/audio/filter/hardware tabs, job queue) served by a tiny dependency-free HTTP server.
//!
//! It is a *client of the same tool registry* the CLI and MCP server use, so the GUI can
//! never offer something the engine cannot do, and its "Agent" tab shows the exact job
//! JSON and command line behind whatever the user has clicked.
//!
//! Security model: binds to 127.0.0.1 only; every request must carry the per-launch random
//! token and a loopback `Host` header (defeats DNS rebinding and cross-site requests).
//! The token is handed to the page in the launch URL. The file browser can list any
//! directory the process can read, which is why the token matters.

use ironav_core::{Error, ErrorCode, Result};
use ironav_engine::{call_tool, Job};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const INDEX_HTML: &str = include_str!("ui.html");
const MAX_BODY: usize = 4 << 20;
/// Connections served at once; one more is closed without an answer.
const MAX_CONNECTIONS: usize = 32;
/// Jobs running at once; one more is refused as busy.
const MAX_RUNNING_JOBS: usize = 4;
/// Finished jobs kept for the page to read; the oldest are dropped.
const MAX_FINISHED_JOBS: usize = 64;
/// How long a connection may stall on a read or a write.
const IO_TIMEOUT: Duration = Duration::from_secs(15);

pub struct Options {
    /// 0 picks a free port.
    pub port: u16,
    pub open_browser: bool,
}

struct State {
    token: String,
    port: u16,
    next_job: AtomicU64,
    jobs: Arc<Mutex<HashMap<u64, Value>>>,
    running: Arc<AtomicUsize>,
}

impl State {
    fn new(port: u16) -> Self {
        State { token: random_token(), port, next_job: AtomicU64::new(1), jobs: Arc::new(Mutex::new(HashMap::new())), running: Arc::new(AtomicUsize::new(0)) }
    }
}

/// `N` random bytes, hex encoded: the operating system's entropy through IronCrypto's SP 800-90A DRBG.
/// If the operating system cannot supply entropy the process stops rather than hand out a guessable value.
fn random_hex<const N: usize>() -> String {
    let bytes: [u8; N] = ic_drbg::Rng::from_os().and_then(|mut rng| rng.random_array()).expect("the operating system's entropy source failed; no random value can be made safely");
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// The launch token: 128 random bits.
fn random_token() -> String {
    random_hex::<16>()
}

/// Whether two tokens are equal, in a time that depends only on their lengths (which are not secret),
/// not on where they first differ: a comparison that stops early would let a caller learn the token a
/// byte at a time.
fn token_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    a.len() == b.len() && a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

pub fn serve(opts: Options) -> Result<()> {
    let listener = TcpListener::bind(("127.0.0.1", opts.port))?;
    let port = listener.local_addr()?.port();
    let state = Arc::new(State::new(port));
    let url = format!("http://127.0.0.1:{port}/?t={}", state.token);
    println!("{}", json!({ "ok": true, "result": { "gui_url": url, "note": "serving on loopback only; press Ctrl+C to stop" } }));
    if opts.open_browser {
        if let Err(e) = open_launch_page(&url) {
            eprintln!("could not open a browser ({e}); open the gui_url printed above");
        }
    }
    accept_loop(listener, state);
    Ok(())
}

/// Serves connections until the listener fails. A connection beyond `MAX_CONNECTIONS` is closed at once.
fn accept_loop(listener: TcpListener, state: Arc<State>) {
    let active = Arc::new(AtomicUsize::new(0));
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        // only this loop adds to the count, so checking and then adding cannot overshoot the cap
        if active.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
            drop(stream);
            continue;
        }
        active.fetch_add(1, Ordering::SeqCst);
        let slot = ConnectionSlot(Arc::clone(&active));
        let st = Arc::clone(&state);
        std::thread::spawn(move || {
            let _slot = slot;
            let _ = handle(stream, &st);
        });
    }
}

/// A connection's place under the cap, given back when its thread ends.
struct ConnectionSlot(Arc<AtomicUsize>);

impl Drop for ConnectionSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Opens the launch URL in the browser without putting it on a command line, where other users can read
/// it in the process list. A small page that redirects to the URL is written into a fresh directory under
/// the OS temp dir, and the page's path is opened. The directory is removed a minute later, when the
/// browser has read the page.
fn open_launch_page(url: &str) -> std::io::Result<()> {
    let dir = stage_launch_page(url)?;
    let opened = open_path(&dir.join(LAUNCH_PAGE));
    if opened.is_ok() {
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_secs(60));
            let _ = std::fs::remove_dir_all(&dir);
        });
    } else {
        let _ = std::fs::remove_dir_all(&dir);
    }
    opened
}

const LAUNCH_PAGE: &str = "open.html";

/// Writes the redirect page into a new directory with a random name, readable by this user only, and
/// returns the directory. The name is not the token: it is only a name.
fn stage_launch_page(url: &str) -> std::io::Result<PathBuf> {
    let dir = std::env::temp_dir().join(format!("ironav-gui-{}", random_hex::<8>()));
    create_private_dir(&dir)?;
    let escaped = url.replace('&', "&amp;").replace('"', "&quot;").replace('<', "&lt;").replace('>', "&gt;");
    let page = format!("<!doctype html>\n<meta charset=\"utf-8\">\n<meta http-equiv=\"refresh\" content=\"0; url={escaped}\">\n<title>IronAV</title>\n<p>Opening IronAV: <a href=\"{escaped}\">{escaped}</a></p>\n");
    write_private_file(&dir.join(LAUNCH_PAGE), page.as_bytes())?;
    Ok(dir)
}

fn create_private_dir(dir: &Path) -> std::io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        std::fs::DirBuilder::new().mode(0o700).create(dir)
    }
    #[cfg(not(unix))]
    {
        // the temp directory on Windows is per user, so a new directory in it is private already
        std::fs::create_dir(dir)
    }
}

fn write_private_file(path: &Path, data: &[u8]) -> std::io::Result<()> {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path)?.write_all(data)
}

fn open_path(path: &Path) -> std::io::Result<()> {
    let mut command = if cfg!(target_os = "windows") {
        let mut c = std::process::Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler").arg(path);
        c
    } else if cfg!(target_os = "macos") {
        let mut c = std::process::Command::new("open");
        c.arg(path);
        c
    } else {
        let mut c = std::process::Command::new("xdg-open");
        c.arg(path);
        c
    };
    command.spawn().map(drop)
}

struct Request {
    method: String,
    path: String,
    query: HashMap<String, String>,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

fn url_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'%' if i + 2 < b.len() => {
                if let Ok(v) = u8::from_str_radix(&s[i + 1..i + 3], 16) {
                    out.push(v);
                    i += 3;
                    continue;
                }
                out.push(b'%');
            }
            b'+' => out.push(b' '),
            c => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The request line and headers, and the bytes read past them. The body is read after the head has been
/// checked, so a request that is refused costs no more than its head.
fn read_head(stream: &mut TcpStream) -> std::io::Result<Option<(Request, Vec<u8>)>> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break p;
        }
        if buf.len() > 64 * 1024 {
            return Ok(None);
        }
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let (method, target) = (first.next().unwrap_or("").to_string(), first.next().unwrap_or("/").to_string());
    let headers: HashMap<String, String> = lines
        .filter_map(|l| l.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let (path, q) = target.split_once('?').unwrap_or((&target, ""));
    let query = q
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (url_decode(k), url_decode(v))
        })
        .collect();
    let past = buf[header_end + 4..].to_vec();
    Ok(Some((Request { method, path: url_decode(path), query, headers, body: Vec::new() }, past)))
}

/// The rest of a body of `len` bytes, of which `body` has come already. None when the peer stops early.
fn read_body(stream: &mut TcpStream, mut body: Vec<u8>, len: usize) -> std::io::Result<Option<Vec<u8>>> {
    let mut chunk = [0u8; 4096];
    while body.len() < len {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(None);
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    Ok(Some(body))
}

/// The Content-Length of a request, or None when it is over the cap.
fn body_len(req: &Request) -> Option<usize> {
    let len: usize = req.headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    (len <= MAX_BODY).then_some(len)
}

fn respond(stream: &mut TcpStream, status: &str, ctype: &str, body: &[u8]) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {}\r\nConnection: close\r\nCache-Control: no-store\r\n\
         X-Content-Type-Options: nosniff\r\nReferrer-Policy: no-referrer\r\n\
         Content-Security-Policy: default-src 'none'; script-src 'unsafe-inline'; style-src 'unsafe-inline'; connect-src 'self'; frame-ancestors 'none'\r\n\r\n",
        body.len()
    )?;
    stream.write_all(body)
}

fn envelope(r: Result<Value>) -> Vec<u8> {
    match r {
        Ok(v) => json!({ "ok": true, "result": v }),
        Err(e) => json!({ "ok": false, "error": e }),
    }
    .to_string()
    .into_bytes()
}

fn handle(mut stream: TcpStream, st: &State) -> std::io::Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let Some((mut req, past)) = read_head(&mut stream)? else {
        return respond(&mut stream, "400 Bad Request", "text/plain", b"bad request");
    };
    // Every check that needs no body is made before one is read.
    let hosts = [format!("127.0.0.1:{}", st.port), format!("localhost:{}", st.port)];
    if !req.headers.get("host").is_some_and(|h| hosts.contains(h)) {
        return respond(&mut stream, "403 Forbidden", "text/plain", b"bad host");
    }
    // A browser sends Origin on a cross-site request, and a page from anywhere else is not ours to serve.
    let origins = [format!("http://127.0.0.1:{}", st.port), format!("http://localhost:{}", st.port)];
    if req.headers.get("origin").is_some_and(|o| !origins.contains(o)) {
        return respond(&mut stream, "403 Forbidden", "text/plain", b"bad origin");
    }

    if req.method == "GET" && req.path == "/" {
        if !req.query.get("t").is_some_and(|t| token_eq(t, &st.token)) {
            return respond(&mut stream, "403 Forbidden", "text/plain", b"missing or wrong token; use the URL printed by `ironav gui`");
        }
        let page = INDEX_HTML.replace("__IRONAV_TOKEN__", &st.token);
        return respond(&mut stream, "200 OK", "text/html; charset=utf-8", page.as_bytes());
    }

    if !req.headers.get("x-ironav-token").is_some_and(|t| token_eq(t, &st.token)) {
        return respond(&mut stream, "403 Forbidden", "application/json", &envelope(Err(Error::new(ErrorCode::InvalidArgument, "bad token"))));
    }
    let Some(len) = body_len(&req) else {
        return respond(&mut stream, "413 Payload Too Large", "text/plain", b"the body is over the 4 MiB limit");
    };
    let Some(body) = read_body(&mut stream, past, len)? else {
        return respond(&mut stream, "400 Bad Request", "text/plain", b"bad request");
    };
    req.body = body;
    let body: Value = if req.body.is_empty() { json!({}) } else { serde_json::from_slice(&req.body).unwrap_or(Value::Null) };

    let out = match (req.method.as_str(), req.path.as_str()) {
        ("POST", p) if p.starts_with("/api/tool/") => envelope(call_tool(&p["/api/tool/".len()..], &body)),
        ("GET", "/api/fs") => envelope(list_dir(req.query.get("path").map(String::as_str))),
        ("POST", "/api/jobs") => envelope(start_job(st, body)),
        ("GET", p) if p.starts_with("/api/jobs/") => match p["/api/jobs/".len()..].parse::<u64>().ok().and_then(|id| st.jobs.lock().ok()?.get(&id).cloned()) {
            Some(v) => json!({ "ok": true, "result": v }).to_string().into_bytes(),
            None => envelope(Err(Error::new(ErrorCode::NotFound, "no such job"))),
        },
        _ => return respond(&mut stream, "404 Not Found", "application/json", &envelope(Err(Error::new(ErrorCode::NotFound, "no such endpoint")))),
    };
    respond(&mut stream, "200 OK", "application/json", &out)
}

fn start_job(st: &State, body: Value) -> Result<Value> {
    let job: Job = serde_json::from_value(body).map_err(|e| Error::invalid_arg(format!("invalid job: {e}")))?;
    // Surface validation errors synchronously so the UI can show them next to the form.
    ironav_engine::plan(&job)?;
    let slot = reserve_job(&st.running)?;
    let id = st.next_job.fetch_add(1, Ordering::Relaxed);
    let jobs = Arc::clone(&st.jobs);
    let put = move |v: Value| {
        if let Ok(mut j) = jobs.lock() {
            let finished = v["state"] != "running";
            j.insert(id, v);
            if finished {
                evict_finished(&mut j, MAX_FINISHED_JOBS);
            }
        }
    };
    put(json!({ "state": "running", "progress": null }));
    std::thread::spawn(move || {
        let _slot = slot;
        let result = ironav_engine::run(&job, &mut |p| put(json!({ "state": "running", "progress": p })));
        put(match result {
            Ok(report) => json!({ "state": "done", "report": report }),
            Err(e) => json!({ "state": "failed", "error": e }),
        });
    });
    Ok(json!({ "id": id }))
}

/// A running-job slot, given back when the job's thread ends.
struct JobSlot(Arc<AtomicUsize>);

impl Drop for JobSlot {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

fn reserve_job(running: &Arc<AtomicUsize>) -> Result<JobSlot> {
    let mut current = running.load(Ordering::SeqCst);
    loop {
        if current >= MAX_RUNNING_JOBS {
            return Err(Error::new(ErrorCode::Busy, format!("{MAX_RUNNING_JOBS} jobs are already running")).hint("wait for one to finish, then start this one"));
        }
        match running.compare_exchange(current, current + 1, Ordering::SeqCst, Ordering::SeqCst) {
            Ok(_) => return Ok(JobSlot(Arc::clone(running))),
            Err(now) => current = now,
        }
    }
}

/// Drops the oldest finished jobs so that at most `keep` remain. Running jobs are never dropped.
fn evict_finished(jobs: &mut HashMap<u64, Value>, keep: usize) {
    let mut finished: Vec<u64> = jobs.iter().filter(|(_, v)| v["state"] != "running").map(|(id, _)| *id).collect();
    if finished.len() > keep {
        finished.sort_unstable();
        for id in &finished[..finished.len() - keep] {
            jobs.remove(id);
        }
    }
}

fn list_dir(path: Option<&str>) -> Result<Value> {
    let home = std::env::var("USERPROFILE").or_else(|_| std::env::var("HOME")).ok().map(PathBuf::from);
    let mut p = path.filter(|p| !p.is_empty()).map(PathBuf::from).or(home).unwrap_or_else(|| PathBuf::from("."));
    if p.is_file() {
        p.pop();
    }
    let mut entries: Vec<Value> = std::fs::read_dir(&p)?
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                return None;
            }
            let md = e.metadata().ok()?;
            Some(json!({ "name": name, "dir": md.is_dir(), "size": md.len() }))
        })
        .collect();
    entries.sort_by_key(|e| (!e["dir"].as_bool().unwrap_or(false), e["name"].as_str().unwrap_or("").to_lowercase()));
    let roots: Vec<String> = if cfg!(windows) {
        ('A'..='Z').map(|c| format!("{c}:\\")).filter(|d| std::path::Path::new(d).exists()).collect()
    } else {
        vec!["/".into()]
    };
    Ok(json!({
        "path": p.display().to_string(),
        "parent": p.parent().map(|x| x.display().to_string()),
        "roots": roots,
        "entries": entries,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn launch_page_holds_the_url_not_on_any_command_line() {
        let url = "http://127.0.0.1:1/?t=abc&x=<y>";
        let dir = stage_launch_page(url).unwrap();
        let page = std::fs::read_to_string(dir.join(LAUNCH_PAGE)).unwrap();
        assert!(page.contains("url=http://127.0.0.1:1/?t=abc&amp;x=&lt;y&gt;"), "{page}");
        assert!(!dir.file_name().unwrap().to_string_lossy().contains("abc"));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(std::fs::metadata(&dir).unwrap().permissions().mode() & 0o777, 0o700);
            assert_eq!(std::fs::metadata(dir.join(LAUNCH_PAGE)).unwrap().permissions().mode() & 0o777, 0o600);
        }
        // a second page gets its own directory
        let other = stage_launch_page(url).unwrap();
        assert_ne!(dir, other);
        std::fs::remove_dir_all(&dir).unwrap();
        std::fs::remove_dir_all(&other).unwrap();
    }

    #[test]
    fn launch_tokens_are_128_random_bits_and_differ() {
        let (a, b) = (random_token(), random_token());
        for t in [&a, &b] {
            assert_eq!(t.len(), 32, "{t}");
            assert!(t.bytes().all(|c| c.is_ascii_hexdigit()), "{t}");
        }
        assert_ne!(a, b);
    }

    #[test]
    fn jobs_beyond_the_running_limit_are_refused_until_one_ends() {
        let running = Arc::new(AtomicUsize::new(0));
        let slots: Vec<JobSlot> = (0..MAX_RUNNING_JOBS).map(|_| reserve_job(&running).unwrap()).collect();
        assert_eq!(reserve_job(&running).err().unwrap().code, ErrorCode::Busy);
        drop(slots);
        assert_eq!(running.load(Ordering::SeqCst), 0);
        assert!(reserve_job(&running).is_ok());
    }

    #[test]
    fn only_the_newest_finished_jobs_are_kept() {
        let mut jobs: HashMap<u64, Value> = HashMap::new();
        jobs.insert(1, json!({ "state": "running", "progress": null }));
        for id in 2..=(MAX_FINISHED_JOBS as u64 + 10) {
            jobs.insert(id, json!({ "state": "done" }));
        }
        evict_finished(&mut jobs, MAX_FINISHED_JOBS);
        assert!(jobs.contains_key(&1), "a running job is never dropped");
        assert_eq!(jobs.len(), MAX_FINISHED_JOBS + 1);
        assert!(!jobs.contains_key(&2) && !jobs.contains_key(&10) && jobs.contains_key(&11));
    }

    #[test]
    fn connections_beyond_the_cap_are_closed_at_once() {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(State::new(port));
        std::thread::spawn(move || accept_loop(listener, state));
        // silent connections hold their slots until the read timeout
        let idle: Vec<TcpStream> = (0..MAX_CONNECTIONS).map(|_| TcpStream::connect(("127.0.0.1", port)).unwrap()).collect();
        std::thread::sleep(Duration::from_millis(300));
        let mut extra = TcpStream::connect(("127.0.0.1", port)).unwrap();
        extra.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let mut buf = [0u8; 16];
        assert!(matches!(extra.read(&mut buf), Ok(0)), "the connection beyond the cap gets no answer and is closed");
        drop(idle);
    }

    fn test_server() -> (u16, String) {
        let listener = TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let port = listener.local_addr().unwrap().port();
        let state = Arc::new(State::new(port));
        let token = state.token.clone();
        std::thread::spawn(move || accept_loop(listener, state));
        (port, token)
    }

    /// Sends `request` and returns whatever comes back before the server closes or the read times out.
    fn raw_request(port: u16, request: &str) -> String {
        let mut c = TcpStream::connect(("127.0.0.1", port)).unwrap();
        c.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        c.write_all(request.as_bytes()).unwrap();
        let mut out = String::new();
        let _ = c.read_to_string(&mut out);
        out
    }

    #[test]
    fn headers_are_checked_before_the_body_is_read() {
        let (port, token) = test_server();
        // a body of 1000 bytes is promised and never sent: a refused head must not wait for it
        let wrong_host = raw_request(port, "POST /api/jobs HTTP/1.1\r\nHost: evil.example\r\nContent-Length: 1000\r\n\r\n");
        assert!(wrong_host.starts_with("HTTP/1.1 403"), "{wrong_host}");
        let no_token = raw_request(port, &format!("POST /api/jobs HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nContent-Length: 1000\r\n\r\n"));
        assert!(no_token.starts_with("HTTP/1.1 403"), "{no_token}");
        // a body over the cap is refused without being read, with the token
        let too_big = raw_request(port, &format!("POST /api/jobs HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nX-IronAV-Token: {token}\r\nContent-Length: 999999999\r\n\r\n"));
        assert!(too_big.starts_with("HTTP/1.1 413"), "{too_big}");
    }

    #[test]
    fn a_foreign_origin_is_refused_even_with_the_token() {
        let (port, token) = test_server();
        let with = |origin: &str| format!("GET /api/fs HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\n{origin}X-IronAV-Token: {token}\r\n\r\n");
        assert!(raw_request(port, &with("")).starts_with("HTTP/1.1 200"));
        assert!(raw_request(port, &with(&format!("Origin: http://127.0.0.1:{port}\r\n"))).starts_with("HTTP/1.1 200"));
        assert!(raw_request(port, &with("Origin: http://evil.example\r\n")).starts_with("HTTP/1.1 403"));
        assert!(raw_request(port, &with("Origin: null\r\n")).starts_with("HTTP/1.1 403"));
    }

    #[test]
    fn token_comparison_is_exact() {
        let t = random_token();
        assert!(token_eq(&t, &t.clone()));
        assert!(token_eq("", ""));
        // one byte wrong at the start, at the end, or a different length
        let mut first = t.clone().into_bytes();
        first[0] ^= 1;
        let mut last = t.clone().into_bytes();
        last[31] ^= 1;
        assert!(!token_eq(&t, std::str::from_utf8(&first).unwrap()));
        assert!(!token_eq(&t, std::str::from_utf8(&last).unwrap()));
        assert!(!token_eq(&t, &t[..31]));
        assert!(!token_eq(&t, &format!("{t}0")));
        assert!(!token_eq("", &t));
    }
}
