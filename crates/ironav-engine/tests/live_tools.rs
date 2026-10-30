//! stream_configure, stream_query and the idle timeout, over the tool interface.

use ironav_core::ErrorCode;
use ironav_engine::call_tool;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ironav-live-tools-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// `secs` of 16 kHz mono: a 1 kHz tone, silent from 1 s to 2.5 s.
fn wav(path: &Path, secs: f64) {
    let rate = 16000u32;
    let samples: Vec<i16> = (0..(rate as f64 * secs) as usize)
        .map(|i| {
            let t = i as f64 / rate as f64;
            if (1.0..2.5).contains(&t) { 0 } else { (6000.0 * (2.0 * std::f64::consts::PI * 1000.0 * t).sin()) as i16 }
        })
        .collect();
    let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    let mut f = b"RIFF".to_vec();
    f.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    f.extend_from_slice(b"WAVEfmt ");
    for v in [16u32.to_le_bytes().to_vec(), 1u16.to_le_bytes().to_vec(), 1u16.to_le_bytes().to_vec(), rate.to_le_bytes().to_vec(), (rate * 2).to_le_bytes().to_vec(), 2u16.to_le_bytes().to_vec(), 16u16.to_le_bytes().to_vec()] {
        f.extend_from_slice(&v);
    }
    f.extend_from_slice(b"data");
    f.extend_from_slice(&(data.len() as u32).to_le_bytes());
    f.extend_from_slice(&data);
    std::fs::write(path, f).unwrap();
}

fn wait_end(id: &str) {
    for _ in 0..100 {
        let ev = call_tool("stream_events", &json!({ "session": id, "after": u64::MAX, "wait_ms": 200 })).unwrap();
        if !matches!(ev["state"].as_str(), Some("running" | "waiting")) {
            return;
        }
    }
    panic!("session {id} did not end");
}

fn analyzers(status: &Value) -> Vec<String> {
    status["analyzed"][0]["analyzers"].as_array().unwrap().iter().map(|a| a.as_str().unwrap().to_string()).collect()
}

#[test]
fn a_running_session_is_reconfigured_and_its_log_searched() {
    let dir = scratch("configure");
    let src = dir.join("tone.wav");
    wav(&src, 4.0);
    let s = call_tool("stream_open", &json!({ "source": format!("{}?pace=realtime", src.display()), "analyzers": ["levels"], "digest_seconds": 10 })).unwrap();
    let id = s["session"].as_str().unwrap().to_string();
    // before the silence starts
    let c = call_tool("stream_configure", &json!({ "session": id, "add": ["silence"], "remove": ["levels"], "digest_seconds": 1 })).unwrap();
    assert_eq!(c["applied"], true, "{c}");
    if !c["analyzed"].as_array().unwrap().is_empty() {
        assert_eq!(analyzers(&c), ["silence"], "{c}");
    }
    let e = call_tool("stream_configure", &json!({ "session": id, "add": ["nope"] })).unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    wait_end(&id);
    let st = call_tool("stream_status", &json!({ "session": id })).unwrap();
    assert_eq!(analyzers(&st), ["silence"], "{st}");
    let all = call_tool("stream_query", &json!({ "session": id, "max": 1000 })).unwrap();
    let digests: Vec<&Value> = all["events"].as_array().unwrap().iter().filter(|o| o["kind"] == "digest").collect();
    assert!(digests.len() >= 3, "a digest a second after the change: {all}");
    assert!(digests.last().unwrap()["data"]["audio0"].get("levels").is_none(), "{all}");
    // the silence, found by kind, by text and by time
    let q = call_tool("stream_query", &json!({ "session": id, "kinds": ["silence_end"], "newest_first": true })).unwrap();
    assert_eq!(q["matched"], 1, "{q}");
    let t = q["events"][0]["t"].as_f64().unwrap();
    assert!((t - 1.0).abs() < 0.1, "{q}");
    let q = call_tool("stream_query", &json!({ "session": id, "text": "DURATION" })).unwrap();
    assert_eq!(q["events"][0]["kind"], "silence_end", "{q}");
    let q = call_tool("stream_query", &json!({ "session": id, "kinds": ["silence_start", "silence_end"], "from": 3.0 })).unwrap();
    assert_eq!(q["matched"], 0, "{q}");
    let e = call_tool("stream_configure", &json!({ "session": id, "level": "raw" })).unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument, "an ended session cannot change");
    call_tool("stream_close", &json!({ "session": id })).unwrap();
}

#[test]
fn a_session_nobody_asks_about_is_closed() {
    let dir = scratch("idle");
    let src = dir.join("long.wav");
    wav(&src, 30.0);
    let s = call_tool("stream_open", &json!({ "source": format!("{}?pace=realtime", src.display()), "analyzers": ["levels"], "idle_timeout_seconds": 0.5 })).unwrap();
    let id = s["session"].as_str().unwrap().to_string();
    // asking keeps it open
    for _ in 0..4 {
        std::thread::sleep(std::time::Duration::from_millis(300));
        call_tool("stream_status", &json!({ "session": id })).unwrap();
    }
    std::thread::sleep(std::time::Duration::from_millis(900));
    let all = call_tool("stream_status", &json!({})).unwrap();
    assert!(!all["sessions"].as_array().unwrap().iter().any(|x| x["session"] == id), "{all}");
    assert_eq!(call_tool("stream_status", &json!({ "session": id })).unwrap_err().code, ErrorCode::NotFound);
}

#[test]
fn a_change_before_the_first_packet_is_kept_for_the_analysis() {
    let dir = scratch("early");
    let src = dir.join("tone.wav");
    wav(&src, 1.5);
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let s = call_tool("stream_open", &json!({ "source": format!("tcp://127.0.0.1:{port}?listen=1"), "analyzers": ["silence"] })).unwrap();
    let id = s["session"].as_str().unwrap().to_string();
    assert_eq!(s["state"], "waiting", "{s}");
    // every analyzer it had removed and another added, before anything arrives
    let c = call_tool("stream_configure", &json!({ "session": id, "remove": ["silence"], "add": ["levels"] })).unwrap();
    assert_eq!(c["applied"], true, "{c}");
    let mut tx = std::net::TcpStream::connect(("127.0.0.1", port)).unwrap();
    std::io::Write::write_all(&mut tx, &std::fs::read(&src).unwrap()).unwrap();
    drop(tx);
    wait_end(&id);
    let st = call_tool("stream_status", &json!({ "session": id })).unwrap();
    assert_eq!(analyzers(&st), ["levels"], "{st}");
    call_tool("stream_close", &json!({ "session": id })).unwrap();
}
