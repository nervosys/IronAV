//! A live session that sends what it makes out over SRT, end to end: a receiver (IronAV's own `srt://`
//! source) takes the transport stream and it demuxes to the media that went in.

use ironav_core::{CodecId, Demuxer, Packet};
use ironav_engine::live::{close, open, OpenArgs};
use ironav_live::{open_source, Chunk};
use serde_json::{json, Value};
use std::io::Cursor;
use std::net::UdpSocket;
use std::time::{Duration, Instant};

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../ironav-formats/tests/data/h264_aac.ts");

fn free_port() -> u16 {
    UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
}

fn args(v: Value) -> OpenArgs {
    serde_json::from_value(v).unwrap()
}

/// Everything a receiver gets until the sender says goodbye.
fn receive(url: &str, limit: Duration) -> Vec<u8> {
    let mut rx = open_source(url).unwrap();
    let (mut out, mut buf, start) = (Vec::new(), vec![0u8; 64 << 10], Instant::now());
    while start.elapsed() < limit {
        match rx.read(&mut buf, Duration::from_millis(20)).unwrap() {
            Chunk::Data(n) => out.extend_from_slice(&buf[..n]),
            Chunk::Idle | Chunk::Restart => {}
            Chunk::End => break,
        }
    }
    out
}

fn packets(ts: Vec<u8>) -> (Vec<CodecId>, Vec<Packet>) {
    let mut d = ironav_formats::ts::TsDemuxer::open(Cursor::new(ts)).unwrap();
    let codecs = d.streams().iter().map(|s| s.codec).collect();
    (codecs, std::iter::from_fn(|| d.read_packet().unwrap()).collect())
}

fn wait_end(id: &str) -> Value {
    let start = Instant::now();
    loop {
        let st = ironav_engine::call_tool("stream_status", &json!({ "session": id })).unwrap();
        if !matches!(st["state"].as_str(), Some("running" | "waiting")) {
            return st;
        }
        assert!(start.elapsed() < Duration::from_secs(30), "{st}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn events(id: &str, kind: &str) -> Vec<Value> {
    let q = ironav_engine::call_tool("stream_query", &json!({ "session": id, "kinds": [kind] })).unwrap();
    q["events"].as_array().unwrap().clone()
}

#[test]
fn a_session_sends_its_media_as_transport_stream_over_srt() {
    ironav_live::net::allow_loopback_for_tests();
    let port = free_port();
    let url = format!("srt://127.0.0.1:{port}?mode=listener&reconnect=0&passphrase=0123456789abcdef");
    let receiver = std::thread::spawn(move || receive(&url, Duration::from_secs(30)));
    // the receiver binds before the session calls
    std::thread::sleep(Duration::from_millis(300));
    let s = open(&args(json!({ "source": FIXTURE, "pace": "realtime", "analyzers": ["structure"], "publish": format!("srt://127.0.0.1:{port}?passphrase=0123456789abcdef&pbkeylen=32") }))).unwrap();
    wait_end(&s.id);
    let st = close(&s.id).unwrap();
    assert_eq!(st["state"], "ended", "{st}");
    assert_eq!(st["publish"]["state"], "finished", "{st}");
    assert_eq!(st["publish"]["error"], Value::Null, "{st}");
    assert_eq!(st["publish"]["packets"], 145, "{st}");
    // the passphrase stays out of the status
    assert!(!st.to_string().contains("0123456789abcdef"), "{st}");
    let (codecs, got) = packets(receiver.join().unwrap());
    assert_eq!(codecs, [CodecId::H264, CodecId::Aac]);
    assert_eq!(got.iter().filter(|p| p.stream == 0).count(), 50);
    assert!(got.iter().filter(|p| p.stream == 1).count() >= 93);
    // the first picture can be decoded from
    let first = got.iter().find(|p| p.stream == 0).unwrap();
    assert_eq!(ironav_formats::is_keyframe(CodecId::H264, &first.data), Some(true));
}

#[test]
fn with_a_transcode_what_is_sent_is_what_the_transcode_writes() {
    ironav_live::net::allow_loopback_for_tests();
    let port = free_port();
    let url = format!("srt://127.0.0.1:{port}?mode=listener&reconnect=0&passphrase=0123456789abcdef");
    let receiver = std::thread::spawn(move || receive(&url, Duration::from_secs(30)));
    std::thread::sleep(Duration::from_millis(300));
    let dir = std::env::temp_dir().join(format!("ironav-publish-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let out = dir.join("tc.ts");
    let s = open(&args(json!({
        "source": FIXTURE, "pace": "realtime", "analyzers": ["structure"],
        "transcode": { "output": out.to_str().unwrap(), "overwrite": true },
        "publish": format!("srt://127.0.0.1:{port}?passphrase=0123456789abcdef"),
    })))
    .unwrap();
    wait_end(&s.id);
    let st = close(&s.id).unwrap();
    assert_eq!(st["state"], "ended", "{st}");
    assert_eq!(st["transcode"]["error"], Value::Null, "{st}");
    assert_eq!(st["publish"]["error"], Value::Null, "{st}");
    let (codecs, got) = packets(receiver.join().unwrap());
    assert_eq!(codecs, [CodecId::H264, CodecId::Aac]);
    // the same pictures as the file the transcode wrote
    let (_, written) = packets(std::fs::read(&out).unwrap());
    let n = |v: &[Packet], s: usize| v.iter().filter(|p| p.stream == s).count();
    assert!(n(&written, 0) > 40, "{}", n(&written, 0));
    assert_eq!((n(&got, 0), n(&got, 1)), (n(&written, 0), n(&written, 1)));
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_destination_that_cannot_be_reached_is_reported_and_the_session_goes_on() {
    ironav_live::net::allow_loopback_for_tests();
    // nobody listens: a caller is tried again with backoff, and says so
    let s = open(&args(json!({ "source": FIXTURE, "pace": "realtime", "publish": format!("srt://127.0.0.1:{}?passphrase=0123456789abcdef", free_port()) }))).unwrap();
    wait_end(&s.id);
    let lost = events(&s.id, "publish_lost");
    let st = close(&s.id).unwrap();
    assert_eq!(st["state"], "ended", "{st}");
    assert_eq!(lost.len(), 1, "{st}");
    assert!(st["publish"]["attempts"].as_u64().unwrap() >= 1, "{st}");
    assert!(st["publish"]["last_error"].is_string(), "{st}");
    // with reconnect=0 the first failure ends the publishing, and the session still ends well
    let s = open(&args(json!({ "source": FIXTURE, "analyzers": ["structure"], "publish": format!("srt://127.0.0.1:{}?passphrase=0123456789abcdef&reconnect=0", free_port()) }))).unwrap();
    wait_end(&s.id);
    let (failed, lost) = (events(&s.id, "publish_failed"), events(&s.id, "publish_lost"));
    let st = close(&s.id).unwrap();
    assert_eq!(st["state"], "ended", "{st}");
    assert_eq!(st["publish"]["state"], "failed", "{st}");
    assert_eq!((failed.len(), lost.len()), (1, 0), "{st}");
    // an address the allow-list refuses is not tried again, with or without reconnect
    let s = open(&args(json!({ "source": FIXTURE, "publish": "srt://10.255.255.1:9000?passphrase=0123456789abcdef" }))).unwrap();
    wait_end(&s.id);
    let st = close(&s.id).unwrap();
    assert_eq!(st["state"], "ended", "{st}");
    assert_eq!(st["publish"]["state"], "failed", "{st}");
    assert!(st["publish"]["error"]["hint"].as_str().is_some_and(|h| h.contains("IRONAV_NET_ALLOW")), "{st}");
}

#[test]
fn a_destination_is_checked_before_the_session_starts() {
    ironav_live::net::allow_loopback_for_tests();
    for bad in ["udp://127.0.0.1:9", "out.ts", "srt://127.0.0.1", "srt://127.0.0.1:9?latency=x", "srt://127.0.0.1:9?format=h264&passphrase=0123456789", "srt://127.0.0.1:9?pbkeylen=7&passphrase=0123456789"] {
        let e = open(&args(json!({ "source": FIXTURE, "publish": bad }))).err().unwrap_or_else(|| panic!("{bad} was accepted"));
        assert_eq!(e.code, ironav_core::ErrorCode::InvalidArgument, "{bad}: {e:?}");
    }
}
