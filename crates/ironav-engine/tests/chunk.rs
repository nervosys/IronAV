//! The `chunk` and `chunk_media` tools end to end, on files written here with known content.

use ironav_core::ErrorCode;
use ironav_engine::chunk;
use ironav_engine::policy::{self, Policy};
use ironav_engine::{call_tool, Job};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ironav-chunk-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn write_wav(path: &Path, samples: &[i16], channels: u16, rate: u32) {
    let data: Vec<u8> = samples.iter().flat_map(|s| s.to_le_bytes()).collect();
    let mut f = Vec::new();
    f.extend_from_slice(b"RIFF");
    f.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    f.extend_from_slice(b"WAVEfmt ");
    f.extend_from_slice(&16u32.to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&channels.to_le_bytes());
    f.extend_from_slice(&rate.to_le_bytes());
    f.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
    f.extend_from_slice(&(channels * 2).to_le_bytes());
    f.extend_from_slice(&16u16.to_le_bytes());
    f.extend_from_slice(b"data");
    f.extend_from_slice(&(data.len() as u32).to_le_bytes());
    f.extend_from_slice(&data);
    std::fs::write(path, f).unwrap();
}

/// 2 s of a 1 kHz stereo tone at -23 dBFS, 3 s of silence, 2 s of the tone: 7 s.
fn tone_silence_tone(path: &Path) {
    let rate = 48000;
    let amp = 10f64.powf(-23.0 / 20.0) * 32767.0;
    let samples: Vec<i16> = (0..rate * 7)
        .flat_map(|i| {
            let on = !(2 * rate..5 * rate).contains(&i);
            let v = if on { (amp * (2.0 * std::f64::consts::PI * 1000.0 * i as f64 / rate as f64).sin()).round() as i16 } else { 0 };
            [v, v]
        })
        .collect();
    write_wav(path, &samples, 2, rate as u32);
}

/// 64x48 4:2:0 at 25 fps: 1 s of a bright moving square on grey, 1 s of black, 1 s of a different picture.
fn cut_and_black(path: &Path) {
    let (w, h) = (64usize, 48usize);
    let mut f = b"YUV4MPEG2 W64 H48 F25:1 Ip A1:1 C420jpeg\n".to_vec();
    for i in 0..75 {
        let mut y = vec![0u8; w * h];
        for (k, v) in y.iter_mut().enumerate() {
            let (x, r) = (k % w, k / w);
            *v = match i {
                0..25 => if (10 + i..26 + i).contains(&x) && (8..24).contains(&r) { 230 } else { 90 },
                25..50 => 16,
                _ => ((x * 3 + r) % 200) as u8 + 40,
            };
        }
        f.extend_from_slice(b"FRAME\n");
        f.extend_from_slice(&y);
        f.extend(std::iter::repeat_n(128u8, w * h / 2));
    }
    std::fs::write(path, f).unwrap();
}

fn s(p: &Path) -> String {
    p.display().to_string()
}

fn chunks(r: &Value) -> &Vec<Value> {
    r["chunks"].as_array().unwrap()
}

#[test]
fn ids_are_the_same_on_every_run_and_change_with_the_arguments_and_the_file() {
    let d = scratch("ids");
    let wav = d.join("a.wav");
    tone_silence_tone(&wav);
    let input = s(&wav);
    let ids = |r: &Value| chunks(r).iter().map(|c| c["id"].as_str().unwrap().to_string()).collect::<Vec<_>>();
    let a = call_tool("chunk", &json!({ "input": input, "seconds": 2 })).unwrap();
    assert_eq!(a["cached"], false);
    // a second call is served from the table in memory
    let again = call_tool("chunk", &json!({ "input": input, "seconds": 2 })).unwrap();
    assert_eq!(again["cached"], true);
    // and with the table gone, the same ids come out of a new analysis
    chunk::forget(&input);
    let b = call_tool("chunk", &json!({ "input": input, "seconds": 2 })).unwrap();
    assert_eq!(b["cached"], false);
    assert_eq!(ids(&a), ids(&b));
    assert_eq!(ids(&a), ids(&again));
    assert_eq!(a["fingerprint"], b["fingerprint"]);
    assert_eq!(chunks(&a)[0]["uri"], format!("ironav://chunks/{}", ids(&a)[0]));
    // other parameters, other ids
    let c = call_tool("chunk", &json!({ "input": input, "seconds": 3 })).unwrap();
    assert!(ids(&c).iter().all(|i| !ids(&a).contains(i)));
    // the ids are distinct within a table
    let mut sorted = ids(&a);
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), ids(&a).len());
    // another file's content is another fingerprint
    let wav2 = d.join("b.wav");
    std::fs::copy(&wav, &wav2).unwrap();
    let mut bytes = std::fs::read(&wav2).unwrap();
    let last = bytes.len() - 1;
    bytes[last] ^= 1;
    std::fs::write(&wav2, bytes).unwrap();
    let e = call_tool("chunk", &json!({ "input": s(&wav2), "seconds": 2 })).unwrap();
    assert_ne!(e["fingerprint"], a["fingerprint"]);
    assert!(ids(&e).iter().all(|i| !ids(&a).contains(i)));
}

#[test]
fn paging_covers_the_file_with_no_gaps_or_overlaps() {
    let d = scratch("paging");
    let wav = d.join("a.wav");
    tone_silence_tone(&wav);
    let input = s(&wav);
    let mut all: Vec<Value> = Vec::new();
    let mut cursor: Option<String> = None;
    let mut pages = 0;
    loop {
        let mut args = json!({ "input": input, "seconds": 2, "limit": 3 });
        if let Some(c) = &cursor {
            args["cursor"] = json!(c);
        }
        let r = call_tool("chunk", &args).unwrap();
        assert_eq!(r["total_chunks"], 4);
        assert_eq!(r["cached"], pages > 0, "only the first page analyzes");
        all.extend(chunks(&r).iter().cloned());
        pages += 1;
        match r["next_cursor"].as_str() {
            Some(c) => cursor = Some(c.to_string()),
            None => break,
        }
    }
    assert_eq!(pages, 2);
    assert_eq!(all.len(), 4);
    assert_eq!(all[0]["t"], 0.0);
    for (i, c) in all.iter().enumerate() {
        assert_eq!(c["index"], i);
        if i > 0 {
            assert_eq!(all[i - 1]["t_end"], c["t"], "no gap or overlap at {i}");
        }
    }
    assert_eq!(all[3]["t_end"], 7.0);
    // the figures follow the sound: the second chunk (2 s to 4 s) is all silence
    let silent = all[1]["audio"]["audio0"]["silent_seconds"].as_f64().unwrap();
    assert!(silent > 1.9, "{}", all[1]);
    assert!(all[0]["audio"]["audio0"]["silent_seconds"].as_f64().unwrap() < 0.1);
    // a cursor of other arguments is refused
    let e = call_tool("chunk", &json!({ "input": input, "seconds": 3, "cursor": "00000000.1" })).unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
}

#[test]
fn scene_mode_splits_at_the_cuts_merges_short_scenes_and_splits_long_ones() {
    let d = scratch("scene");
    let y4m = d.join("cut.y4m");
    cut_and_black(&y4m);
    let input = s(&y4m);
    let r = call_tool("chunk", &json!({ "input": input, "mode": "scene", "min_seconds": 0.5, "max_seconds": 60 })).unwrap();
    let list = chunks(&r);
    // into black is a cut, and out of it: three scenes
    let spans: Vec<(f64, f64)> = list.iter().map(|c| (c["t"].as_f64().unwrap(), c["t_end"].as_f64().unwrap())).collect();
    assert_eq!(spans, [(0.0, 1.0), (1.0, 2.0), (2.0, 3.0)], "{r}");
    assert_eq!(list.iter().map(|c| c["scene_id"].as_u64().unwrap()).collect::<Vec<_>>(), [0, 1, 2]);
    // the black second says so
    let black = list[1]["stats"]["video0"]["black_seconds"].as_f64().unwrap();
    assert!(black > 0.9, "{}", list[1]);
    assert!(list[0]["stats"]["video0"]["black_seconds"].as_f64().unwrap() < 0.1);
    // a minimum of 1.5 s leaves one scene: the cut at 1 s would make a scene shorter than that
    let r = call_tool("chunk", &json!({ "input": input, "mode": "scene", "min_seconds": 1.5 })).unwrap();
    assert_eq!(r["total_chunks"], 1, "{r}");
    // a maximum of 0.5 s cuts each scene in two parts that keep its scene_id
    let r = call_tool("chunk", &json!({ "input": input, "mode": "scene", "min_seconds": 0.5, "max_seconds": 0.5 })).unwrap();
    assert_eq!(r["total_chunks"], 6);
    let list = chunks(&r);
    assert_eq!(list.iter().map(|c| c["scene_id"].as_u64().unwrap()).collect::<Vec<_>>(), [0, 0, 1, 1, 2, 2]);
    assert_eq!((list[1]["part"].clone(), list[1]["parts"].clone()), (json!(1), json!(2)));
    for w in list.windows(2) {
        assert_eq!(w[0]["t_end"], w[1]["t"]);
    }
    // scene mode needs pictures
    let wav = d.join("a.wav");
    tone_silence_tone(&wav);
    let e = call_tool("chunk", &json!({ "input": s(&wav), "mode": "scene" })).unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    // the two modes' options are not mixed
    let e = call_tool("chunk", &json!({ "input": input, "seconds": 2, "min_seconds": 1 })).unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
}

#[test]
fn a_range_limits_the_chunks() {
    let d = scratch("range");
    let y4m = d.join("cut.y4m");
    cut_and_black(&y4m);
    let r = call_tool("chunk", &json!({ "input": s(&y4m), "seconds": 0.5, "from": 1, "to": 2 })).unwrap();
    assert_eq!(chunks(&r).iter().map(|c| c["t"].as_f64().unwrap()).collect::<Vec<_>>(), [1.0, 1.5], "{r}");
    assert_eq!(chunks(&r)[1]["t_end"], 2.0);
}

#[test]
fn thumbnails_come_as_images_and_a_budget_trims_in_a_stated_order() {
    let d = scratch("budget");
    let y4m = d.join("cut.y4m");
    cut_and_black(&y4m);
    let input = s(&y4m);
    let args = json!({ "input": input, "seconds": 1, "include": ["thumbnail", "audio", "stats"] });
    let full = call_tool("chunk", &args).unwrap();
    assert!(full.get("trimmed").is_none());
    let images = full["images"].as_array().unwrap();
    assert!(!images.is_empty());
    for c in chunks(&full) {
        let k = c["thumbnail"]["image"].as_u64().unwrap() as usize;
        assert!(images[k]["png_base64"].as_str().unwrap().starts_with("iVBOR"), "a PNG");
    }
    // a budget below the size of the pictures drops them first, and keeps the rest
    let plain = call_tool("chunk", &json!({ "input": input, "seconds": 1, "include": ["audio", "stats"] })).unwrap();
    let plain_size: usize = chunks(&plain).iter().map(|c| c.to_string().len()).sum();
    let mut tight = args.clone();
    tight["budget_tokens"] = json!(plain_size / 4 + 3);
    let r = call_tool("chunk", &tight).unwrap();
    assert_eq!(r["trimmed"]["dropped"], json!(["thumbnail"]), "{r}");
    assert!(r.get("images").is_none());
    assert!(chunks(&r).iter().all(|c| c.get("thumbnail").is_none() && c.get("stats").is_some() && c.get("id").is_some()));
    // a smaller one drops stats too, in the order thumbnail, stats, audio
    let mut tighter = args.clone();
    tighter["budget_tokens"] = json!(150);
    let r = call_tool("chunk", &tighter).unwrap();
    let dropped = r["trimmed"]["dropped"].as_array().unwrap();
    assert_eq!(dropped[..2], [json!("thumbnail"), json!("stats")], "{r}");
    assert!(chunks(&r).iter().all(|c| c.get("stats").is_none()));
    // and when even the places do not fit, the page holds fewer chunks and says where to go on
    let mut tiny = json!({ "input": input, "seconds": 0.2, "limit": 15, "budget_tokens": 120 });
    tiny["include"] = json!(["audio"]);
    let r = call_tool("chunk", &tiny).unwrap();
    let n = chunks(&r).len();
    assert!((1..15).contains(&n), "{r}");
    assert_eq!(r["trimmed"]["chunks_on_page"], n);
    assert!(r["next_cursor"].is_string());
    assert!(r["chunks"].to_string().len() <= 120 * 4 || n == 1);
}

#[test]
fn chunk_media_returns_n_pictures_inside_the_range_and_obeys_max_side() {
    let d = scratch("frames");
    let y4m = d.join("cut.y4m");
    cut_and_black(&y4m);
    let input = s(&y4m);
    let r = call_tool("chunk_media", &json!({ "input": input, "t": 0.0, "t_end": 3.0, "frames": 3, "max_side": 32 })).unwrap();
    let frames = r["frames"].as_array().unwrap();
    assert_eq!(frames.len(), 3, "{r}");
    for (k, f) in frames.iter().enumerate() {
        // the middles of three equal parts
        let target = 0.5 + k as f64;
        assert!((f["t"].as_f64().unwrap() - target).abs() < 0.05, "{f}");
        assert!(f["jpeg_base64"].as_str().unwrap().starts_with("/9j/"), "a JPEG");
        assert!(f["width"].as_u64().unwrap() <= 32 && f["height"].as_u64().unwrap() <= 32);
    }
    // PNG on request, and 4 pictures when nothing is asked for
    let r = call_tool("chunk_media", &json!({ "input": input, "t": 1.0, "t_end": 2.0, "format": "png" })).unwrap();
    assert_eq!(r["frames"].as_array().unwrap().len(), 4);
    assert!(r["frames"][0]["png_base64"].as_str().unwrap().starts_with("iVBOR"));
    // bad ranges and sizes are refused
    for bad in [json!({ "t": 2.0, "t_end": 1.0 }), json!({ "t": 0.0 }), json!({ "id": "nope" }), json!({ "t": 0.0, "t_end": 1.0, "frames": 99 })] {
        let mut a = bad;
        a["input"] = json!(input);
        assert_eq!(call_tool("chunk_media", &a).unwrap_err().code, ErrorCode::InvalidArgument, "{a}");
    }
}

#[test]
fn chunk_media_takes_an_id_with_or_without_the_table_and_returns_capped_sound() {
    let d = scratch("audio");
    let wav = d.join("a.wav");
    tone_silence_tone(&wav);
    let input = s(&wav);
    let r = call_tool("chunk", &json!({ "input": input, "seconds": 2 })).unwrap();
    let id = chunks(&r)[3]["id"].as_str().unwrap().to_string();
    // the last chunk, 6 s to 7 s
    for cached in [true, false] {
        if !cached {
            chunk::forget(&input);
        }
        let m = call_tool("chunk_media", &json!({ "input": input, "id": id, "audio": true })).unwrap();
        assert_eq!(m["t"], 6.0);
        assert_eq!(m["t_end"], 7.0);
        let a = &m["audio"];
        assert_eq!((a["sample_rate"].clone(), a["channels"].clone()), (json!(48000), json!(1)));
        assert!((a["seconds"].as_f64().unwrap() - 1.0).abs() < 0.01, "{a}");
        assert_eq!(a["bytes"], 44 + 48000 * 2);
        assert!(a["wav_base64"].as_str().unwrap().starts_with("UklGR"), "RIFF");
        assert_eq!(a["truncated"], false);
        assert!(m.get("frames").is_none(), "audio asked for, so no pictures by default");
    }
    // a byte cap shortens it, and it says so
    let m = call_tool("chunk_media", &json!({ "input": input, "t": 0.0, "t_end": 7.0, "audio": true, "max_bytes": 4096 })).unwrap();
    assert!(m["audio"]["bytes"].as_u64().unwrap() <= 4096, "{m}");
    assert_eq!(m["audio"]["truncated"], true);
    // so does a cap in seconds
    let m = call_tool("chunk_media", &json!({ "input": input, "t": 0.0, "t_end": 7.0, "audio": true, "audio_seconds": 0.5 })).unwrap();
    assert!((m["audio"]["seconds"].as_f64().unwrap() - 0.5).abs() < 0.01);
    // an id of another file, or of this one changed, is not served
    let other = d.join("b.wav");
    tone_silence_tone(&other);
    let e = call_tool("chunk_media", &json!({ "input": s(&other), "id": id, "audio": true })).unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound, "{e:?}");
    // a picture-only file has no sound to give
    let y4m = d.join("cut.y4m");
    cut_and_black(&y4m);
    let m = call_tool("chunk_media", &json!({ "input": s(&y4m), "t": 0.0, "t_end": 1.0, "audio": true })).unwrap();
    assert!(m.get("audio").is_none() && m["warnings"][0].as_str().unwrap().contains("no audio"), "{m}");
}

#[test]
fn chunk_media_makes_a_small_mp4_clip_in_memory() {
    let d = scratch("clip");
    let y4m = d.join("cut.y4m");
    cut_and_black(&y4m);
    let mp4 = d.join("cut.mp4");
    let mut job = Job { input: s(&y4m), output: s(&mp4), hwaccel: "off".into(), ..Job::default() };
    job.video.codec = Some("h264".into());
    job.video.latency = Some(ironav_engine::Latency::Low);
    call_tool("run", &serde_json::to_value(&job).unwrap()).unwrap();
    let before: Vec<_> = std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name()).collect();
    let m = call_tool("chunk_media", &json!({ "input": s(&mp4), "t": 1.0, "t_end": 2.0, "clip": true, "frames": 0 })).unwrap();
    let c = &m["clip"];
    assert_eq!(c["mime"], "video/mp4", "{m}");
    assert!(c["bytes"].as_u64().unwrap() > 100);
    // an MP4 starts with an `ftyp` box
    assert!(c["mp4_base64"].as_str().unwrap().starts_with("AAAA"), "{c}");
    assert!(c["from"].as_f64().unwrap() <= 1.0 && c["t_end"].as_f64().unwrap() >= 1.5, "{c}");
    // pictures come from the same file
    let m = call_tool("chunk_media", &json!({ "input": s(&mp4), "t": 0.0, "t_end": 3.0, "frames": 2 })).unwrap();
    assert_eq!(m["frames"].as_array().unwrap().len(), 2);
    // over the byte cap it is cut short (and says so) or refused, and never larger than the cap
    let m = call_tool("chunk_media", &json!({ "input": s(&mp4), "t": 0.0, "t_end": 3.0, "clip": true, "max_bytes": 4096 })).unwrap();
    match m.get("clip") {
        Some(c) => assert!(c["bytes"].as_u64().unwrap() <= 4096, "{m}"),
        None => assert!(m["warnings"][0].as_str().unwrap().starts_with("clip:"), "{m}"),
    }
    // nothing was written
    let after: Vec<_> = std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name()).collect();
    assert_eq!(before, after);
}

#[test]
fn a_chunk_resource_reads_the_cached_descriptor_and_an_uncached_one_says_what_to_do() {
    let d = scratch("resource");
    let wav = d.join("a.wav");
    tone_silence_tone(&wav);
    let input = s(&wav);
    let e = chunk::resource("ironav://chunks/0000000000000000-00000000-0-0-1000").unwrap_err();
    assert_eq!(e.code, ErrorCode::NotFound);
    assert!(e.hint.unwrap().contains("`chunk`"));
    let r = call_tool("chunk", &json!({ "input": input, "seconds": 2 })).unwrap();
    let c = &chunks(&r)[1];
    let got = chunk::resource(c["uri"].as_str().unwrap()).unwrap();
    assert_eq!(got["id"], c["id"]);
    assert_eq!((got["t"].clone(), got["t_end"].clone()), (json!(2.0), json!(4.0)));
    assert_eq!(got["input"], json!(input));
    assert!(got["audio"]["audio0"].is_object());
    assert_eq!(got["total_chunks"], 4);
    // other URIs are not chunks
    assert_eq!(chunk::resource("ironav://chunks/").unwrap_err().code, ErrorCode::InvalidArgument);
    assert_eq!(chunk::resource("ironav://chunks/a/b").unwrap_err().code, ErrorCode::InvalidArgument);
    chunk::forget(&input);
    assert_eq!(chunk::resource(c["uri"].as_str().unwrap()).unwrap_err().code, ErrorCode::NotFound);
}

#[test]
fn both_tools_are_refused_outside_ironav_roots_and_work_inside() {
    let inside = scratch("roots-in");
    let outside = scratch("roots-out");
    let (a, b) = (inside.join("a.wav"), outside.join("b.wav"));
    tone_silence_tone(&a);
    tone_silence_tone(&b);
    policy::with_policy(Policy::open().with_roots(&[inside.as_path()]), || {
        assert!(call_tool("chunk", &json!({ "input": s(&a), "seconds": 3 })).is_ok());
        assert!(call_tool("chunk_media", &json!({ "input": s(&a), "t": 0.0, "t_end": 1.0, "audio": true })).is_ok());
        let e = call_tool("chunk", &json!({ "input": s(&b), "seconds": 3 })).unwrap_err();
        assert_eq!(e.code, ErrorCode::PermissionDenied);
        assert!(e.hint.unwrap().contains("IRONAV_ROOTS"));
        let e = call_tool("chunk_media", &json!({ "input": s(&b), "t": 0.0, "t_end": 1.0, "audio": true })).unwrap_err();
        assert_eq!(e.code, ErrorCode::PermissionDenied);
        // a table cached for an allowed file is not a way to read another
        let e = call_tool("chunk_media", &json!({ "input": s(&b), "id": "x" })).unwrap_err();
        assert_eq!(e.code, ErrorCode::PermissionDenied);
    });
}
