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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const INDEX_HTML: &str = include_str!("ui.html");
const MAX_BODY: usize = 4 << 20;

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
    let state = Arc::new(State { token: random_token(), port, next_job: AtomicU64::new(1), jobs: Arc::new(Mutex::new(HashMap::new())) });
    let url = format!("http://127.0.0.1:{port}/?t={}", state.token);
    println!("{}", json!({ "ok": true, "result": { "gui_url": url, "note": "serving on loopback only; press Ctrl+C to stop" } }));
    if opts.open_browser {
        if let Err(e) = open_launch_page(&url) {
            eprintln!("could not open a browser ({e}); open the gui_url printed above");
        }
    }
    for conn in listener.incoming() {
        let Ok(stream) = conn else { continue };
        let st = Arc::clone(&state);
        std::thread::spawn(move || {
            let _ = handle(stream, &st);
        });
    }
    Ok(())
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

fn read_request(stream: &mut TcpStream) -> std::io::Result<Option<Request>> {
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
    let len: usize = headers.get("content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
    if len > MAX_BODY {
        return Ok(None);
    }
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < len {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            return Ok(None);
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(len);
    let (path, q) = target.split_once('?').unwrap_or((&target, ""));
    let query = q
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (url_decode(k), url_decode(v))
        })
        .collect();
    Ok(Some(Request { method, path: url_decode(path), query, headers, body }))
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
    stream.set_read_timeout(Some(Duration::from_secs(15)))?;
    let Some(req) = read_request(&mut stream)? else {
        return respond(&mut stream, "400 Bad Request", "text/plain", b"bad request");
    };
    let host_ok = req
        .headers
        .get("host")
        .is_some_and(|h| *h == format!("127.0.0.1:{}", st.port) || *h == format!("localhost:{}", st.port));
    if !host_ok {
        return respond(&mut stream, "403 Forbidden", "text/plain", b"bad host");
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
    let id = st.next_job.fetch_add(1, Ordering::Relaxed);
    let jobs = Arc::clone(&st.jobs);
    let put = move |v: Value| {
        if let Ok(mut j) = jobs.lock() {
            j.insert(id, v);
        }
    };
    put(json!({ "state": "running", "progress": null }));
    std::thread::spawn(move || {
        let result = ironav_engine::run(&job, &mut |p| put(json!({ "state": "running", "progress": p })));
        put(match result {
            Ok(report) => json!({ "state": "done", "report": report }),
            Err(e) => json!({ "state": "failed", "error": e }),
        });
    });
    Ok(json!({ "id": id }))
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
