//! An operator's audit trail. With `IRONAV_AUDIT_LOG=<path>` set, one JSON line is appended for each
//! security-relevant event: a tool call starting and ending, a file about to be written, a network
//! source or a capture device being opened, and every refusal by the policy (roots, read-only,
//! devices, cookies, size caps). Lines carry a UTC RFC 3339 time and never carry credentials (URLs are
//! redacted) or media.
//!
//! Off by default. The log is opened for each line with append and create, so it can be rotated while
//! the server runs; a line that cannot be written is dropped rather than failing the call.

use serde_json::{json, Map, Value};
use std::cell::RefCell;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;

pub const AUDIT_VAR: &str = "IRONAV_AUDIT_LOG";

/// Who is calling: the front end that started the process.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Caller {
    Cli = 0,
    Mcp = 1,
    WebUi = 2,
}

static CALLER: AtomicU8 = AtomicU8::new(0);

pub fn set_caller(c: Caller) {
    CALLER.store(c as u8, Ordering::Relaxed);
}

pub fn caller() -> &'static str {
    match CALLER.load(Ordering::Relaxed) {
        1 => "mcp",
        2 => "webui",
        _ => "cli",
    }
}

thread_local! {
    static LOG: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// Runs `f` with the log going to `path` on this thread (for tests and embedders).
pub fn with_log<T>(path: PathBuf, f: impl FnOnce() -> T) -> T {
    let old = LOG.with(|l| l.borrow_mut().replace(path));
    struct Restore(Option<PathBuf>);
    impl Drop for Restore {
        fn drop(&mut self) {
            LOG.with(|l| *l.borrow_mut() = self.0.take());
        }
    }
    let _r = Restore(old);
    f()
}

fn target() -> Option<PathBuf> {
    LOG.with(|l| l.borrow().clone()).or_else(|| std::env::var_os(AUDIT_VAR).filter(|v| !v.is_empty()).map(PathBuf::from))
}

/// Appends one event. `fields` is a JSON object of what is specific to it.
pub fn event(kind: &str, fields: Value) {
    let Some(path) = target() else { return };
    let mut m = Map::new();
    m.insert("ts".into(), json!(crate::live::now_iso()));
    m.insert("event".into(), json!(kind));
    m.insert("caller".into(), json!(caller()));
    m.insert("pid".into(), json!(std::process::id()));
    if let Value::Object(f) = fields {
        m.extend(f);
    }
    let line = Value::Object(m).to_string();
    // lines from several threads must not interleave
    static WRITE: Mutex<()> = Mutex::new(());
    let _g = WRITE.lock().unwrap_or_else(|e| e.into_inner());
    if let Ok(mut f) = std::fs::OpenOptions::new().append(true).create(true).open(path) {
        let _ = writeln!(f, "{line}");
    }
}

/// A refusal by the operator's policy.
pub fn refused(reason: &str, subject: &str) {
    event("refused", json!({ "reason": reason, "subject": subject }));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::policy::{with_policy, Policy};
    use crate::tools::call_tool;

    fn lines(p: &std::path::Path) -> Vec<Value> {
        std::fs::read_to_string(p).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect()
    }

    #[test]
    fn a_refused_path_and_a_successful_run_leave_the_expected_lines() {
        let d = std::env::temp_dir().join(format!("ironav-audit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let (root, other) = (d.join("root"), d.join("other"));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::create_dir_all(&other).unwrap();
        let log = d.join("audit.jsonl");
        let (inp, out, bad) = (root.join("in.y4m"), root.join("out.y4m"), other.join("out.y4m"));
        std::fs::write(&inp, b"YUV4MPEG2 W4 H2 F30:1 Ip A1:1 C420jpeg\nFRAME\n\x07\x07\x07\x07\x07\x07\x07\x07\x07\x07\x07\x07").unwrap();
        let job = |o: &std::path::Path| json!({ "input": inp.to_str().unwrap(), "output": o.to_str().unwrap(), "hwaccel": "off" });
        with_log(log.clone(), || {
            with_policy(Policy::open().with_roots(&[&root]), || {
                let e = call_tool("run", &job(&bad)).unwrap_err();
                assert_eq!(e.code, ironav_core::ErrorCode::PermissionDenied);
                call_tool("run", &job(&out)).unwrap();
                // a device in MCP-style policy, and a network source with a password in it
                let mut strict = Policy::open();
                strict.devices = false;
                with_policy(strict, || {
                    assert!(call_tool("stream_open", &json!({ "source": "device://video/0" })).is_err());
                });
            });
        });
        let ev = lines(&log);
        let kinds: Vec<String> = ev.iter().map(|e| format!("{}:{}", e["event"].as_str().unwrap(), e["tool"].as_str().or(e["reason"].as_str()).unwrap_or(""))).collect();
        // the refused run: start, refusal, end with its error code
        assert_eq!(kinds[0], "tool_start:run");
        assert_eq!(kinds[1], "refused:roots");
        assert_eq!(kinds[2], "tool_end:run");
        assert_eq!(ev[2]["outcome"], "permission-denied");
        // the successful one: start, write, end ok with a duration
        assert_eq!(kinds[3], "tool_start:run");
        assert_eq!(kinds[4], "file_write:");
        assert_eq!(ev[4]["path"], out.to_str().unwrap());
        assert_eq!(kinds[5], "tool_end:run");
        assert_eq!(ev[5]["outcome"], "ok");
        assert!(ev[5]["duration_ms"].is_u64());
        // the device refusal
        assert!(kinds.iter().any(|k| k == "refused:devices"));
        for e in &ev {
            assert!(e["ts"].as_str().unwrap().ends_with('Z') && e["caller"] == "cli");
        }
        // no media, no arguments beyond what is listed
        assert!(!std::fs::read_to_string(&log).unwrap().contains("FRAME"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn network_and_device_opens_are_logged_with_redacted_urls() {
        let d = std::env::temp_dir().join(format!("ironav-audit-net-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        let log = d.join("a.jsonl");
        with_log(log.clone(), || {
            let u = ironav_live::SourceUrl::parse("rtsp://admin:hunter2@192.0.2.1:554/stream").unwrap();
            crate::policy::check_source("rtsp://admin:hunter2@192.0.2.1:554/stream", &u).unwrap();
            let u = ironav_live::SourceUrl::parse("device://video/0").unwrap();
            crate::policy::check_source("device://video/0", &u).unwrap();
        });
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(!text.contains("hunter2"), "{text}");
        let ev = lines(&log);
        assert_eq!(ev[0]["event"], "network_open");
        assert!(ev[0]["url"].as_str().unwrap().contains("192.0.2.1"));
        assert_eq!(ev[1]["event"], "device_open");
        let _ = std::fs::remove_dir_all(&d);
    }
}
