//! `probe` on playlist files: it lists the entries and opens nothing (no file, no URL, no network). HLS
//! playlists are streams and are refused with a pointer to `stream_open`.

use ironav_engine::call_tool;
use serde_json::json;
use std::path::PathBuf;

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ironav-playlist-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[test]
fn an_m3u_lists_its_entries_without_opening_them() {
    let d = scratch("m3u");
    let f = d.join("list.m3u");
    std::fs::write(&f, "#EXTM3U\r\n#EXTINF:12,First song\r\nmusic/one.mp3\r\n#EXTINF:-1,Radio\r\nhttp://127.0.0.1:1/never-fetched\r\n").unwrap();
    let out = call_tool("probe", &json!({ "path": f.to_str().unwrap() })).unwrap();
    assert_eq!(out["format"], "m3u");
    assert_eq!(out["count"], 2);
    assert_eq!(out["untrusted"], true);
    let e = &out["entries"];
    assert_eq!(e[0]["title"], "First song");
    assert_eq!(e[0]["duration_seconds"], 12.0);
    assert_eq!(e[0]["url"], false);
    assert!(e[0]["location"].as_str().unwrap().replace('\\', "/").ends_with("/music/one.mp3"), "{}", e[0]);
    assert_eq!((e[1]["url"].clone(), e[1]["location"].clone()), (json!(true), json!("http://127.0.0.1:1/never-fetched")));
}

#[test]
fn a_pls_lists_its_entries() {
    let d = scratch("pls");
    let f = d.join("list.pls");
    std::fs::write(&f, "[playlist]\nFile1=a.ogg\nTitle1=A\nLength1=30\nFile2=http://example.invalid/b\nNumberOfEntries=2\nVersion=2\n").unwrap();
    let out = call_tool("probe", &json!({ "path": f.to_str().unwrap() })).unwrap();
    assert_eq!(out["format"], "pls");
    assert_eq!(out["count"], 2);
    assert_eq!(out["entries"][0]["title"], "A");
}

#[test]
fn an_hls_playlist_is_a_stream_not_a_list() {
    let d = scratch("hls");
    let f = d.join("live.m3u8");
    std::fs::write(&f, "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXTINF:4,\nseg0.ts\n").unwrap();
    let e = call_tool("probe", &json!({ "path": f.to_str().unwrap() })).unwrap_err();
    assert!(e.hint.as_deref().unwrap_or("").contains("stream_open"), "{e}");
}

#[test]
fn text_that_is_not_a_playlist_is_not_one() {
    let d = scratch("notlist");
    let f = d.join("notes.m3u");
    std::fs::write(&f, [0u8, 1, 2, 3, 0xFF, 0xFE, 0, 0]).unwrap();
    // a binary file with the extension is left to the media readers, which refuse it
    assert!(call_tool("probe", &json!({ "path": f.to_str().unwrap() })).is_err());
}
