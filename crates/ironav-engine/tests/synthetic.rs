//! The made-up test sources, through a live session.

use ironav_engine::live::{self, OpenArgs};
use ironav_engine::tap::{PixelLayout, TapConfig, TapData};
use ironav_live::synthetic::frame_number;
use serde_json::json;
use std::time::{Duration, Instant};

fn open(source: &str) -> std::sync::Arc<live::Session> {
    let args: OpenArgs = serde_json::from_value(json!({ "source": source, "analyzers": ["levels", "black"] })).unwrap();
    live::open(&args).unwrap()
}

#[test]
fn a_test_picture_numbers_its_frames_and_says_it_is_synthetic() {
    let started = Instant::now();
    let s = open("test://video?duration=1&fps=20&size=320x180");
    let tap = s.tap(TapConfig { layout: PixelLayout::Gray, queue: 64, ..TapConfig::video() }).unwrap();
    let mut numbers = Vec::new();
    while let Ok(Some(f)) = tap.recv_timeout(Duration::from_secs(10)) {
        let TapData::Video { width, height, pixels, .. } = &f.data else { panic!() };
        assert_eq!((*width, *height), (320, 180));
        let k = frame_number(pixels, 320).unwrap();
        assert!((f.t - k as f64 / 20.0).abs() < 1e-3, "frame {k} at {}", f.t);
        numbers.push(k);
    }
    assert_eq!(numbers, (0..20).collect::<Vec<_>>());
    // made at the pace it plays
    assert!(started.elapsed() > Duration::from_millis(900), "{:?}", started.elapsed());
    let st = s.status();
    assert_eq!(st["source"]["synthetic"], true, "{st}");
    assert_eq!(st["source"]["kind"], "test");
    // each stage's time per packet
    assert_eq!(st["stage_ms"]["packets"], 20, "{st}");
    for stage in ["demux", "decode", "analyze"] {
        let p95 = st["stage_ms"][stage]["p95"].as_f64().unwrap();
        assert!((0.0..500.0).contains(&p95), "{stage}: {st}");
    }
    // measured, though raw video decodes in less than the 0.1 ms it is rounded to
    assert!(st["stage_ms"]["decode"]["max"].as_f64().is_some_and(|v| v >= 0.0), "{st}");
}

#[test]
fn a_test_sound_is_a_tone_half_of_each_second() {
    let s = open("test://audio?duration=2");
    let tap = s.tap(TapConfig { window_ms: 250, queue: 64, ..TapConfig::audio() }).unwrap();
    let mut levels = Vec::new();
    while let Ok(Some(f)) = tap.recv_timeout(Duration::from_secs(10)) {
        let TapData::Audio { samples, .. } = &f.data else { panic!() };
        levels.push((f.t, (samples.iter().map(|v| v * v).sum::<f32>() / samples.len() as f32).sqrt()));
    }
    // the last window waits for the resampler's look-ahead, which the end does not give
    assert!((7..=8).contains(&levels.len()), "{levels:?}");
    for (t, rms) in levels {
        let on = (t + 0.01).fract() < 0.5;
        assert!(if on { rms > 0.15 } else { rms < 0.01 }, "{rms} at {t}");
    }
    assert!(live::open(&serde_json::from_value(json!({ "source": "test://smell" })).unwrap()).is_err());
}

#[test]
fn keyframe_pictures_are_kept_as_jpeg() {
    let args: OpenArgs = serde_json::from_value(json!({ "source": "test://video?duration=1.5&fps=10", "analyzers": ["levels"], "keyframe_jpegs": 3, "keyframe_width": 160 })).unwrap();
    let s = live::open(&args).unwrap();
    let start = Instant::now();
    while !matches!(s.status()["state"].as_str(), Some("ended" | "failed")) {
        assert!(start.elapsed() < Duration::from_secs(10));
        std::thread::sleep(Duration::from_millis(50));
    }
    // every Y4M frame is a keyframe: the newest three of fifteen
    let all = s.keyframes(None, 10);
    assert_eq!(all.iter().map(|k| (k.t * 10.0).round() as i32).collect::<Vec<_>>(), [12, 13, 14]);
    for k in &all {
        assert_eq!((k.width, k.height), (160, 120));
        assert_eq!(ironav_codec::jpeg::probe(&k.jpeg).unwrap().0, 160);
    }
    assert_eq!(s.keyframes(Some(1.3), 10).len(), 1);
    let r = ironav_engine::call_tool("stream_keyframes", &json!({ "session": s.id, "max": 2 })).unwrap();
    let list = r["keyframes"].as_array().unwrap();
    assert_eq!(list.len(), 2, "{r}");
    assert_eq!(list[1]["t"], 1.4);
    assert!(list[1]["jpeg_base64"].as_str().unwrap().starts_with("/9j/"), "a JPEG's first bytes");
}

#[test]
fn an_audio_tap_measures_its_windows() {
    let s = open("test://audio?duration=2");
    let tap = s.tap(TapConfig { window_ms: 250, features: true, queue: 64, ..TapConfig::audio() }).unwrap();
    let mut seen = 0;
    while let Ok(Some(f)) = tap.recv_timeout(Duration::from_secs(10)) {
        let TapData::Audio { features: Some(x), .. } = f.data else { panic!("no features") };
        seen += 1;
        if (f.t + 0.01).fract() < 0.5 {
            // the 440 Hz tone at 0.3
            assert!((x.rms_dbfs - -13.5).abs() < 0.5 && (x.peak_dbfs - -10.5).abs() < 0.3, "{x:?} at {}", f.t);
            assert!((x.centroid_hz - 440.0).abs() < 60.0 && x.flatness < 0.05, "{x:?} at {}", f.t);
        } else {
            assert!(x.rms_dbfs < -90.0, "{x:?} at {}", f.t);
        }
        // a steady tone is not speech, and nor is silence
        assert_eq!(x.speech, 0.0, "{x:?} at {}", f.t);
    }
    assert!(seen >= 7);
}
