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

#[test]
fn a_recording_is_started_and_moved_to_a_new_file_while_a_session_runs() {
    let dir = scratch("record");
    let src = concat!(env!("CARGO_MANIFEST_DIR"), "/../ironav-formats/tests/data/h264_aac.ts");
    // two seconds of H.264 and AAC, a keyframe about every 0.44 s
    let s = call_tool("stream_open", &json!({ "source": format!("{src}?pace=realtime"), "analyzers": ["structure"] })).unwrap();
    let id = s["session"].as_str().unwrap().to_string();
    let (a, b) = (dir.join("a.ts"), dir.join("b.mp4"));
    std::thread::sleep(std::time::Duration::from_millis(300));
    let c = call_tool("stream_configure", &json!({ "session": id, "record": a.to_str().unwrap() })).unwrap();
    assert_eq!(c["applied"], true, "{c}");
    assert_eq!(c["record"]["path"], a.to_str().unwrap(), "{c}");
    // a second recording needs the first stopped
    let e = call_tool("stream_configure", &json!({ "session": id, "record": b.to_str().unwrap() })).unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument, "{e:?}");
    let e = call_tool("stream_configure", &json!({ "session": id, "keep_segments": 2 })).unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument, "{e:?}");
    std::thread::sleep(std::time::Duration::from_millis(700));
    let c = call_tool("stream_configure", &json!({ "session": id, "stop_recording": true, "record": b.to_str().unwrap() })).unwrap();
    assert_eq!(c["applied"], true, "{c}");
    wait_end(&id);
    let st = call_tool("stream_status", &json!({ "session": id })).unwrap();
    assert_eq!(st["record"]["path"], b.to_str().unwrap(), "{st}");
    assert_eq!(st["record"]["finished"], true, "{st}");
    assert_eq!(st["record"]["error"], Value::Null, "{st}");
    call_tool("stream_close", &json!({ "session": id })).unwrap();
    // each file starts at a video keyframe, with its timestamps from zero, and holds part of the source
    for f in [&a, &b] {
        let mut d = ironav_formats::open_demuxer(f).unwrap();
        let streams = d.streams().to_vec();
        let (mut first, mut last, mut n) = (f64::INFINITY, 0f64, 0);
        while let Some(p) = d.read_packet().unwrap() {
            let t = p.pts as f64 * streams[p.stream].time_base.as_f64();
            first = first.min(t);
            last = last.max(t);
            n += 1;
        }
        assert!(n > 5 && first.abs() < 0.05 && last < 1.6, "{}: {n} packets from {first} to {last}", f.display());
    }
}

#[test]
fn a_session_transcodes_its_video_to_low_latency_h264() {
    use ironav_core::MediaType;
    let dir = scratch("transcode");
    let src = concat!(env!("CARGO_MANIFEST_DIR"), "/../ironav-formats/tests/data/h264_aac.ts");
    let out = dir.join("small.ts");
    // a width that is not the source's: scaled, the height following
    let s = call_tool("stream_open", &json!({ "source": src, "analyzers": ["structure"], "transcode": { "output": out.to_str().unwrap(), "width": 64, "qp": 30 } })).unwrap();
    let id = s["session"].as_str().unwrap().to_string();
    wait_end(&id);
    let st = call_tool("stream_close", &json!({ "session": id })).unwrap();
    let tc = &st["transcode"];
    assert_eq!(tc["state"], "finished", "{st}");
    assert_eq!(tc["error"], Value::Null, "{st}");
    assert_eq!(tc["video"]["width"], 64, "{st}");
    let pictures = tc["pictures_out"].as_u64().unwrap();
    assert!(pictures >= 45 && pictures == tc["pictures_in"].as_u64().unwrap(), "{st}");
    // H.264 that decodes to as many pictures, each as it arrives (no reordering), with the audio copied
    let mut d = ironav_formats::open_demuxer(&out).unwrap();
    let streams = d.streams().to_vec();
    assert_eq!(streams.len(), 2, "{streams:?}");
    let v = streams.iter().position(|s| s.params.media_type() == MediaType::Video).unwrap();
    assert_eq!(streams[v].codec, ironav_core::CodecId::H264);
    let mut dec = ironav_codec::new_decoder_for(&streams[v]).unwrap();
    let (mut n, mut last) = (0, i64::MIN);
    while let Some(p) = d.read_packet().unwrap() {
        if p.stream == v {
            assert!(p.pts > last, "pictures in order");
            last = p.pts;
            for f in dec.decode(&p).unwrap() {
                let ironav_core::Frame::Video(f) = f else { unreachable!() };
                assert_eq!(f.width, 64);
                n += 1;
            }
        }
    }
    n += dec.flush().unwrap().len();
    assert_eq!(n as u64, pictures);
}

#[test]
fn a_transcode_is_started_and_moved_to_a_new_file_while_a_session_runs() {
    let dir = scratch("transcode_late");
    let src = concat!(env!("CARGO_MANIFEST_DIR"), "/../ironav-formats/tests/data/h264_aac.ts");
    let s = call_tool("stream_open", &json!({ "source": format!("{src}?pace=realtime"), "analyzers": ["structure"] })).unwrap();
    let id = s["session"].as_str().unwrap().to_string();
    let (a, b) = (dir.join("a.ts"), dir.join("b.ts"));
    std::thread::sleep(std::time::Duration::from_millis(300));
    let tc = |p: &std::path::Path| json!({ "output": p.to_str().unwrap(), "width": 64, "qp": 30 });
    let c = call_tool("stream_configure", &json!({ "session": id, "transcode": tc(&a) })).unwrap();
    assert_eq!(c["applied"], true, "{c}");
    // a second transcode needs the first stopped
    let e = call_tool("stream_configure", &json!({ "session": id, "transcode": tc(&b) })).unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument, "{e:?}");
    // well before the 1.1 s clip ends, even on a loaded machine
    std::thread::sleep(std::time::Duration::from_millis(300));
    let c = call_tool("stream_configure", &json!({ "session": id, "stop_transcode": true, "transcode": tc(&b) })).unwrap();
    assert_eq!(c["applied"], true, "{c}");
    wait_end(&id);
    let st = call_tool("stream_close", &json!({ "session": id })).unwrap();
    assert_eq!(st["transcode"]["output"], b.to_str().unwrap(), "{st}");
    assert_eq!(st["transcode"]["state"], "finished", "{st}");
    assert_eq!(st["transcode"]["error"], Value::Null, "{st}");
    // each file starts at a keyframe with its timestamps from zero, and holds part of the source
    for f in [&a, &b] {
        let mut d = ironav_formats::open_demuxer(f).unwrap();
        let streams = d.streams().to_vec();
        let (mut first, mut last, mut n) = (f64::INFINITY, 0f64, 0);
        while let Some(p) = d.read_packet().unwrap() {
            let t = p.pts as f64 * streams[p.stream].time_base.as_f64();
            first = first.min(t);
            last = last.max(t);
            n += 1;
        }
        assert!(n > 5 && first.abs() < 0.05 && last < 1.6, "{}: {n} packets from {first} to {last}", f.display());
    }
}
