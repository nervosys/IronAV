//! A live session on a source that drops and comes back.

use ironav_engine::call_tool;
use serde_json::{json, Value};
use std::io::Write;
use std::net::TcpListener;
use std::time::{Duration, Instant};

/// One second of 64x48 Y4M at 25 fps, grey.
fn y4m() -> Vec<u8> {
    let mut v = b"YUV4MPEG2 W64 H48 F25:1 Ip A1:1 C420jpeg\n".to_vec();
    for k in 0..25u8 {
        v.extend_from_slice(b"FRAME\n");
        v.extend(std::iter::repeat_n(100 + k, 64 * 48));
        v.extend(std::iter::repeat_n(128, 2 * 32 * 24));
    }
    v
}

fn events(id: &str, kind: &str) -> Vec<Value> {
    let q = call_tool("stream_query", &json!({ "session": id, "kinds": [kind] })).unwrap();
    q["events"].as_array().unwrap().clone()
}

#[test]
fn a_dropped_source_is_reconnected_and_its_time_goes_on() {
    ironav_live::net::allow_loopback_for_tests();
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = l.local_addr().unwrap().port();
    std::thread::spawn(move || {
        // a second, then the connection drops; a second more, from the start of the stream again
        for _ in 0..2 {
            let (mut c, _) = l.accept().unwrap();
            c.write_all(&y4m()).unwrap();
            std::thread::sleep(Duration::from_millis(200));
        }
        // then the sender is gone for good
    });
    let s = call_tool("stream_open", &json!({ "source": format!("tcp://127.0.0.1:{port}?reconnect&retries=1"), "analyzers": ["black", "structure"] })).unwrap();
    let id = s["session"].as_str().unwrap().to_string();
    let start = Instant::now();
    let st = loop {
        let st = call_tool("stream_status", &json!({ "session": id })).unwrap();
        if !matches!(st["state"].as_str(), Some("running" | "waiting")) {
            break st;
        }
        assert!(start.elapsed() < Duration::from_secs(30), "{st}");
        std::thread::sleep(Duration::from_millis(100));
    };
    // it gave up on the third connection
    assert_eq!(st["state"], "failed", "{st}");
    assert_eq!(st["source_health"]["state"], "failed", "{st}");
    assert_eq!(st["source_health"]["reconnects"], 1, "{st}");
    let lost = events(&id, "source_lost");
    assert_eq!(lost.len(), 2, "{lost:?}");
    let back = events(&id, "source_restored");
    assert_eq!(back.len(), 1, "{back:?}");
    assert!((back[0]["t"].as_f64().unwrap() - 0.96).abs() < 0.1, "{back:?}");
    assert_eq!(events(&id, "source_failed").len(), 1);
    // the second connection's pictures come after the first's, a gap between, none before
    let t = st["media_seconds"].as_f64().unwrap();
    assert!(t > 2.0 && t < 6.0, "{st}");
    assert!(events(&id, "discontinuity").is_empty(), "time went backwards");
    assert_eq!(call_tool("stream_snapshot", &json!({ "session": id })).unwrap()["snapshot"]["t"].as_f64().map(|v| v > 1.9), Some(true));
}
