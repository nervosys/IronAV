//! `stream_chunks` end to end: a file played as if it were live (paced to its own clock), cut into
//! chunks as media time passes.

use ironav_core::ErrorCode;
use ironav_engine::{call_tool, chunk, tools};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ironav-live-chunks-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// 2 s of H.264 and AAC: 145 packets.
fn ts_fixture() -> String {
    concat!(env!("CARGO_MANIFEST_DIR"), "/../ironav-formats/tests/data/h264_aac.ts").to_string()
}

/// 20 frames at 10 fps: dark for one second, then bright, so there is a cut at 1 s.
fn y4m_with_cut(path: &Path) {
    let mut f = b"YUV4MPEG2 W64 H64 F10:1 Ip C420jpeg\n".to_vec();
    for i in 0..20 {
        f.extend(b"FRAME\n");
        f.extend(vec![if i < 10 { 30u8 } else { 220 }; 64 * 64]);
        f.extend(vec![128u8; 2 * 32 * 32]);
    }
    std::fs::write(path, f).unwrap();
}

fn open_paced(source: &str, extra: Value) -> String {
    let mut a = json!({ "source": format!("{source}?pace=realtime"), "digest_seconds": 0.5, "hwaccel": "off" });
    for (k, v) in extra.as_object().unwrap() {
        a[k] = v.clone();
    }
    let s = call_tool("stream_open", &a).unwrap();
    s["session"].as_str().unwrap().to_string()
}

fn wait_end(id: &str) {
    let t0 = Instant::now();
    loop {
        let st = call_tool("stream_status", &json!({ "session": id })).unwrap();
        if matches!(st["state"].as_str(), Some("ended" | "failed")) {
            assert_eq!(st["state"], "ended", "{st}");
            return;
        }
        assert!(t0.elapsed() < Duration::from_secs(60), "session did not end: {st}");
        std::thread::sleep(Duration::from_millis(20));
    }
}

fn ranges(chunks: &[Value]) -> Vec<(f64, f64)> {
    chunks.iter().map(|c| (c["t"].as_f64().unwrap(), c["t_end"].as_f64().unwrap())).collect()
}

fn assert_contiguous(chunks: &[Value]) {
    for w in chunks.windows(2) {
        assert_eq!(w[0]["t_end"], w[1]["t"], "gap or overlap between {} and {}", w[0], w[1]);
        assert_eq!(w[0]["index"].as_u64().unwrap() + 1, w[1]["index"].as_u64().unwrap(), "{} {}", w[0], w[1]);
    }
}

#[test]
fn chunks_close_in_order_page_without_gaps_and_the_last_is_final() {
    let id = open_paced(&ts_fixture(), json!({ "analyzers": ["structure", "levels", "scene"] }));
    let args = |after: u64, wait: u64| json!({ "session": id, "seconds": 0.5, "after": after, "wait_ms": wait, "include": ["thumbnail", "audio", "stats"] });

    // a cursor far ahead has nothing to return: the call waits out its time and gives the cursor back
    let t0 = Instant::now();
    let r = call_tool("stream_chunks", &args(1000, 300)).unwrap();
    assert!(r["chunks"].as_array().unwrap().is_empty(), "{r}");
    assert_eq!((r["next_after"].as_u64(), r["missed"].as_u64()), (Some(1000), Some(0)), "{r}");
    assert!(t0.elapsed() >= Duration::from_millis(250), "returned early: {:?}", t0.elapsed());

    // the first call returns as soon as a chunk closes, long before the 2 s source has ended
    let t0 = Instant::now();
    let first = call_tool("stream_chunks", &args(0, 10_000)).unwrap();
    assert!(!first["chunks"].as_array().unwrap().is_empty(), "{first}");
    assert!(t0.elapsed() < Duration::from_millis(1900), "waited {:?} for the first chunk", t0.elapsed());
    assert_eq!(first["chunks"][0]["final"], false);

    // follow the stream to its end, a page at a time
    let mut all: Vec<Value> = first["chunks"].as_array().unwrap().clone();
    let mut after = first["next_after"].as_u64().unwrap();
    let mut state = first["state"].clone();
    let mut images = first["images"].as_array().map_or(0, Vec::len);
    let t0 = Instant::now();
    while state == "running" || state == "waiting" {
        let r = call_tool("stream_chunks", &args(after, 5000)).unwrap();
        let got = r["chunks"].as_array().unwrap();
        if let (Some(a), Some(b)) = (all.last(), got.first()) {
            assert_eq!(a["index"].as_u64().unwrap() + 1, b["index"].as_u64().unwrap(), "no duplicate and no gap: {r}");
        }
        all.extend(got.iter().cloned());
        images += r["images"].as_array().map_or(0, Vec::len);
        let next = r["next_after"].as_u64().unwrap();
        assert!(next >= after);
        assert_eq!(r["missed"], 0);
        after = next;
        state = r["state"].clone();
        assert!(t0.elapsed() < Duration::from_secs(60));
    }
    wait_end(&id);
    // the call that sees the end brings the closing chunk
    let r = call_tool("stream_chunks", &args(after, 0)).unwrap();
    all.extend(r["chunks"].as_array().unwrap().iter().cloned());
    assert_eq!(r["state"], "ended");
    assert_eq!(r["more"], false, "{r}");

    assert!(all.len() >= 4, "{all:?}");
    assert_contiguous(&all);
    let ids: Vec<&str> = all.iter().map(|c| c["id"].as_str().unwrap()).collect();
    let mut unique = ids.clone();
    unique.sort();
    unique.dedup();
    assert_eq!(unique.len(), ids.len(), "ids are unique: {ids:?}");
    assert!(ids.iter().all(|i| i.starts_with(&format!("live-{id}-"))), "{ids:?}");
    let finals: Vec<bool> = all.iter().map(|c| c["final"].as_bool().unwrap()).collect();
    assert_eq!(finals.iter().filter(|f| **f).count(), 1, "{finals:?}");
    assert!(*finals.last().unwrap(), "{finals:?}");
    // every window is half a second, but the last, which is what was left
    for (t, t_end) in &ranges(&all)[..all.len() - 1] {
        assert!((t_end - t - 0.5).abs() < 1e-6, "{t} {t_end}");
    }
    assert!(all.last().unwrap()["t_end"].as_f64().unwrap() > 1.9);

    // what was asked for is there: figures of the digests, and pictures
    assert!(all.iter().any(|c| c["audio"].is_object()), "{all:?}");
    assert!(all.iter().any(|c| c["stats"].is_object()), "{all:?}");
    assert!(all.iter().any(|c| c["thumbnail"].is_object()), "{all:?}");
    assert!(images > 0);
    assert!(all.iter().all(|c| c.get("transcript").is_none()));

    // the same call gives the same ids; everything still held comes from 0
    let again = call_tool("stream_chunks", &json!({ "session": id, "seconds": 0.5, "include": [], "limit": 1000 })).unwrap();
    let again_ids: Vec<&str> = again["chunks"].as_array().unwrap().iter().map(|c| c["id"].as_str().unwrap()).collect();
    assert_eq!(again_ids, ids);
    assert!(again["chunks"][0].get("audio").is_none() && again["chunks"][0].get("thumbnail").is_none());
    // one chunk at a time, by cursor: the same chunks, none twice
    let mut by_one = Vec::new();
    let mut after = 0;
    loop {
        let r = call_tool("stream_chunks", &json!({ "session": id, "seconds": 0.5, "include": [], "limit": 1, "after": after })).unwrap();
        let Some(c) = r["chunks"].get(0) else { break };
        by_one.push(c["id"].as_str().unwrap().to_string());
        after = r["next_after"].as_u64().unwrap();
        assert_eq!(r["more"], by_one.len() < ids.len());
    }
    assert_eq!(by_one, ids);
    // other parameters are another table, with other ids
    let other = call_tool("stream_chunks", &json!({ "session": id, "seconds": 1, "include": [] })).unwrap();
    assert_ne!(other["chunks"][0]["id"], again["chunks"][0]["id"]);
    // a budget too small for a thumbnail drops it, and the cursor follows the chunks returned
    let tight = call_tool("stream_chunks", &json!({ "session": id, "seconds": 0.5, "include": ["thumbnail", "audio", "stats"], "budget_tokens": 150 })).unwrap();
    assert!(tight["trimmed"]["dropped"].as_array().unwrap().contains(&json!("thumbnail")), "{tight}");
    assert!(tight["chunks"].as_array().unwrap().iter().all(|c| c.get("thumbnail").is_none()));
    assert_eq!(tight["next_after"].as_u64().unwrap(), tight["chunks"].as_array().unwrap().last().unwrap()["index"].as_u64().unwrap() + 1);

    // a live chunk is a resource while the session holds it ...
    let uri = all[1]["uri"].as_str().unwrap().to_string();
    assert_eq!(uri, format!("ironav://chunks/{}", all[1]["id"].as_str().unwrap()));
    let d = chunk::resource(&uri).unwrap();
    assert_eq!((&d["id"], &d["session"], &d["t"]), (&all[1]["id"], &json!(id), &all[1]["t"]));
    assert!(d["audio"].is_object() || d["stats"].is_object(), "{d}");
    // ... an id of a table that does not hold it is an error with a hint
    let e = chunk::resource(&format!("ironav://chunks/live-{id}-00000000-5-1000-2000")).unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
    assert!(e.hint.unwrap().contains("stream_chunks"));

    // and chunk_media takes its id alone, and reads the retained media
    let mid = all[1]["id"].as_str().unwrap();
    let m = call_tool("chunk_media", &json!({ "id": mid, "frames": 2, "audio": true, "clip": true, "max_side": 32 })).unwrap();
    assert_eq!(m["frames"].as_array().map(Vec::len), Some(2), "{m}");
    assert!(m["frames"][0]["jpeg_base64"].as_str().is_some_and(|b| b.len() > 20));
    assert!(m["audio"]["wav_base64"].as_str().is_some_and(|b| b.len() > 20), "{m}");
    assert!(m["clip"]["mp4_base64"].as_str().is_some_and(|b| b.len() > 20), "{m}");
    assert_eq!(m["session"], json!(id));
    let e = call_tool("chunk_media", &json!({ "id": mid, "input": "x.mp4" })).unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    assert_eq!(call_tool("chunk_media", &json!({ "t": 0, "t_end": 1 })).unwrap_err().code, ErrorCode::InvalidArgument);

    call_tool("stream_close", &json!({ "session": id })).unwrap();
    assert_eq!(chunk::resource(&uri).unwrap_err().code, ErrorCode::NotFound);
    assert_eq!(call_tool("stream_chunks", &json!({ "session": id })).unwrap_err().code, ErrorCode::NotFound);
}

#[test]
fn scene_chunks_run_from_cut_to_cut() {
    let dir = scratch("scene");
    let src = dir.join("cut.y4m");
    y4m_with_cut(&src);
    let id = open_paced(src.to_str().unwrap(), json!({ "analyzers": ["scene", "structure"] }));
    wait_end(&id);
    let r = call_tool("stream_chunks", &json!({ "session": id, "mode": "scene", "min_seconds": 0.5, "max_seconds": 60, "include": ["stats", "thumbnail"], "thumbnail_width": 32 })).unwrap();
    let c = r["chunks"].as_array().unwrap();
    assert_eq!(c.len(), 2, "{r}");
    assert_eq!(ranges(c), [(0.0, 1.0), (1.0, 2.0)], "{r}");
    assert_eq!((c[0]["scene_id"].as_u64(), c[1]["scene_id"].as_u64()), (Some(0), Some(1)));
    assert_eq!((c[0]["final"].as_bool(), c[1]["final"].as_bool()), (Some(false), Some(true)));
    assert!(c.iter().all(|c| c["thumbnail"]["image"].is_u64()), "{r}");
    assert_eq!(r["images"][0]["width"], 32, "{r}");
    assert_eq!(r["state"], "ended");
    // a scene longer than max_seconds is cut into pieces that share its scene_id
    let r = call_tool("stream_chunks", &json!({ "session": id, "mode": "scene", "min_seconds": 0.5, "max_seconds": 0.5, "include": [] })).unwrap();
    let c = r["chunks"].as_array().unwrap();
    assert_eq!(c.len(), 4, "{r}");
    assert_eq!(c[1]["scene_id"], c[0]["scene_id"], "{r}");
    assert_eq!(c[1]["part"], 1);
    assert_contiguous(c);
    // scene and time parameters do not mix
    assert_eq!(call_tool("stream_chunks", &json!({ "session": id, "mode": "scene", "seconds": 1 })).unwrap_err().code, ErrorCode::InvalidArgument);
    assert_eq!(call_tool("stream_chunks", &json!({ "session": id, "min_seconds": 1 })).unwrap_err().code, ErrorCode::InvalidArgument);
    assert_eq!(call_tool("stream_chunks", &json!({ "session": id, "include": ["pictures"] })).unwrap_err().code, ErrorCode::InvalidArgument);
    call_tool("stream_close", &json!({ "session": id })).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_tool_only_reads() {
    let t = tools().iter().find(|t| t.name == "stream_chunks").unwrap();
    assert!(t.read_only && !t.destructive() && !t.open_world());
    assert!(!ironav_engine::policy::writes_files("stream_chunks", &json!({ "session": "s1" })));
    assert_eq!(call_tool("stream_chunks", &json!({})).unwrap_err().code, ErrorCode::InvalidArgument);
}
