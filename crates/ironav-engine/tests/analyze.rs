//! The `analyze` tool end to end, on files written here with known content.

use ironav_core::ErrorCode;
use ironav_engine::{call_tool, call_tool_with, Ctx, Job};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ironav-analyze-{}-{name}", std::process::id()));
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

/// 2 s of a 1 kHz stereo tone at -23 dBFS, 3 s of silence, 2 s of the tone.
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

fn kinds(r: &Value) -> Vec<(String, f64, Option<f64>)> {
    r["observations"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|o| !matches!(o["kind"].as_str(), Some("digest" | "summary" | "thumbnail")))
        .map(|o| (o["kind"].as_str().unwrap().to_string(), o["t"].as_f64().unwrap(), o["t_end"].as_f64()))
        .collect()
}

fn summary(r: &Value) -> &Value {
    let obs = r["observations"].as_array().unwrap();
    let last = obs.last().unwrap();
    assert_eq!(last["kind"], "summary");
    &last["data"]
}

#[test]
fn silence_and_loudness_of_a_wav() {
    let d = scratch("wav");
    let wav = d.join("tst.wav");
    tone_silence_tone(&wav);
    let r = call_tool("analyze", &json!({ "input": wav.display().to_string() })).unwrap();
    assert_eq!(kinds(&r), [("silence_start".to_string(), 2.0, None), ("silence_end".to_string(), 2.0, Some(5.0))], "{r}");
    let a = &summary(&r)["audio0"];
    // the tone is -23 LUFS; the silence is below the absolute gate, so it does not pull that
    // down, though the blocks that straddle its edges (partly tone) do a little
    let i = a["loudness"]["integrated_lufs"].as_f64().unwrap();
    assert!((i + 23.0).abs() < 0.5, "{a}");
    assert_eq!(a["silence"]["spans"], 1);
    // a digest every 10 s by default: one window here, plus the summary
    assert_eq!(r["counts"]["digest"], 1);
}

#[test]
fn cuts_and_black_in_a_y4m() {
    let d = scratch("y4m");
    let y4m = d.join("cut.y4m");
    cut_and_black(&y4m);
    let thumbs = d.join("thumbs");
    let r = call_tool(
        "analyze",
        &json!({ "input": y4m.display().to_string(), "digest_seconds": 1, "thumbnail_dir": thumbs.display().to_string(), "thumbnail_seconds": 2 }),
    )
    .unwrap();
    let got = kinds(&r);
    let at = |k: &str| got.iter().filter(|g| g.0 == k).map(|g| (g.1, g.2)).collect::<Vec<_>>();
    // into black is a cut, and out of it
    assert_eq!(at("scene_cut"), [(1.0, None), (2.0, None)], "{got:?}");
    assert_eq!(at("black_end"), [(1.0, Some(2.0))], "{got:?}");
    // the black second and the last second are each one still picture
    assert_eq!(at("freeze_end"), [(1.0, Some(2.0)), (2.0, Some(3.0))], "{got:?}");
    assert_eq!(r["counts"]["digest"], 3, "{r}");
    // thumbnails were written and the observations say where
    let shots: Vec<_> = r["observations"].as_array().unwrap().iter().filter(|o| o["kind"] == "thumbnail").collect();
    assert!(!shots.is_empty());
    for s in shots {
        let p = s["refs"]["path"].as_str().unwrap();
        assert!(std::fs::read(p).unwrap().starts_with(b"\x89PNG"), "{p}");
    }
}

#[test]
fn the_digest_level_returns_digests_only_and_raw_adds_measurements() {
    let input = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/bframes_aac.mkv");
    let r = call_tool("analyze", &json!({ "input": input, "level": "digest", "hwaccel": "off" })).unwrap();
    let ks: Vec<_> = r["observations"].as_array().unwrap().iter().map(|o| o["kind"].as_str().unwrap()).collect();
    assert_eq!(ks, ["digest", "summary"], "{r}");
    let r = call_tool("analyze", &json!({ "input": input, "level": "raw", "hwaccel": "off", "analyzers": ["scene"] })).unwrap();
    // one change score per picture after the first: 20 pictures
    assert_eq!(r["counts"]["change"], 19, "{r}");
    assert_eq!(r["streams"].as_array().unwrap().len(), 2);
}

#[test]
fn bad_arguments_name_the_alternatives() {
    let input = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/bframes_aac.mkv");
    let e = call_tool("analyze", &json!({ "input": input, "analyzers": ["colour"] })).unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    assert!(e.alternatives.iter().any(|a| a == "loudness"), "{e:?}");
    let e = call_tool("analyze", &json!({ "input": input, "level": "everything" })).unwrap_err();
    assert!(e.alternatives.iter().any(|a| a == "digest"), "{e:?}");
    // a misspelt option is an error, not ignored
    let e = call_tool("analyze", &json!({ "input": input, "digest_secs": 5 })).unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument, "{e:?}");
}

#[test]
fn analyze_and_run_stop_when_cancelled() {
    let d = scratch("cancel");
    let wav = d.join("tst.wav");
    tone_silence_tone(&wav);
    let cancel = AtomicBool::new(true);
    let ctx = Ctx { progress: None, cancel: Some(&cancel) };
    let e = call_tool_with("analyze", &json!({ "input": wav.display().to_string() }), &ctx).unwrap_err();
    assert_eq!(e.code, ErrorCode::Cancelled);
    let out = d.join("out.wav");
    let job = Job { input: wav.display().to_string(), output: out.display().to_string(), hwaccel: "auto".into(), ..Job::default() };
    let e = call_tool_with("run", &serde_json::to_value(&job).unwrap(), &ctx).unwrap_err();
    assert_eq!(e.code, ErrorCode::Cancelled);
    assert!(!out.exists() && !d.join("out.wav.partial").exists());
}

#[test]
fn keyframes_only_decodes_a_picture_a_keyframe() {
    let d = scratch("keyframes");
    let y4m = d.join("cut.y4m");
    cut_and_black(&y4m);
    // H.264 with a keyframe every second, which is where this clip changes
    let mp4 = d.join("cut.mp4");
    let mut job = Job { input: y4m.display().to_string(), output: mp4.display().to_string(), hwaccel: "off".into(), ..Job::default() };
    job.video.codec = Some("h264".into());
    job.video.latency = Some(ironav_engine::Latency::Low);
    call_tool("run", &serde_json::to_value(&job).unwrap()).unwrap();
    let input = mp4.display().to_string();
    let r = call_tool("analyze", &json!({ "input": input, "keyframes_only": true, "level": "raw", "digest_seconds": 1, "hwaccel": "off" })).unwrap();
    // three pictures decoded, so two change scores
    assert_eq!(r["counts"]["change"], 2, "{r}");
    let got = kinds(&r);
    let at = |k: &str| got.iter().filter(|g| g.0 == k).map(|g| (g.1, g.2)).collect::<Vec<_>>();
    assert_eq!(at("scene_cut"), [(1.0, None), (2.0, None)], "{got:?}");
    assert_eq!(at("black_end"), [(1.0, Some(2.0))], "{got:?}");
    // freeze needs every picture: not run, and refused when asked for
    assert!(!got.iter().any(|g| g.0.starts_with("freeze")), "{got:?}");
    let e = call_tool("analyze", &json!({ "input": input, "keyframes_only": true, "analyzers": ["freeze"] })).unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument, "{e:?}");
}
