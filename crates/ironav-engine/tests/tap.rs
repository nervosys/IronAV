//! Taps on a live session: decoded pictures and sound handed to code in the same process.

use ironav_engine::live::{self, OpenArgs};
use ironav_engine::tap::{PixelLayout, Tap, TapConfig, TapData, TapFrame};
use serde_json::json;
use std::time::Duration;

/// A live session on the 0.8 s test clip (64x48 H.264 at 25 fps, stereo AAC at 22.05 kHz), played
/// as fast as it plays, with its video decoded from the start as a session watching it would.
fn open() -> std::sync::Arc<live::Session> {
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/bframes_aac.mkv");
    let args: OpenArgs = serde_json::from_value(json!({ "source": format!("{}?pace=realtime", src.display()), "analyzers": ["levels", "black"] })).unwrap();
    live::open(&args).unwrap()
}

/// Everything a tap gives until the session stops feeding it.
fn drain(tap: &Tap) -> Vec<TapFrame> {
    let mut got = Vec::new();
    loop {
        match tap.recv_timeout(Duration::from_secs(10)) {
            Ok(Some(f)) => got.push(f),
            Ok(None) => panic!("nothing for 10 s"),
            Err(_) => return got,
        }
    }
}

#[test]
fn pictures_come_scaled_letterboxed_and_rate_limited() {
    let s = open();
    let config = TapConfig { fps: Some(10.0), width: Some(160), height: Some(90), letterbox: true, queue: 64, ..TapConfig::video() };
    let rgb = s.tap(config.clone()).unwrap();
    let bgr = s.tap(TapConfig { layout: PixelLayout::Bgr, ..config }).unwrap();
    let gray = s.tap(TapConfig { layout: PixelLayout::Gray, queue: 1, ..TapConfig::video() }).unwrap();
    let (a, b) = (drain(&rgb), drain(&bgr));
    assert!((6..=9).contains(&a.len()), "{} pictures at 10 a second from 0.8 s", a.len());
    assert_eq!(a.len(), b.len());
    for (k, f) in a.iter().enumerate() {
        assert_eq!(f.seq, k as u64);
        assert_eq!(f.stream, 0);
        if k > 0 {
            let dt = f.t - a[k - 1].t;
            assert!(dt > 0.07 && dt < 0.13, "{dt}");
        }
        let TapData::Video { width, height, layout, pixels } = &f.data else { panic!("not a picture") };
        assert_eq!((*width, *height, *layout), (160, 90, PixelLayout::Rgb));
        assert_eq!(pixels.len(), 160 * 90 * 3);
        // 4:3 in 16:9: black bars left and right
        assert!(pixels[..3].iter().all(|p| *p == 0) && pixels[160 * 90 * 3 - 3..].iter().all(|p| *p == 0));
        let TapData::Video { pixels: swapped, layout: PixelLayout::Bgr, .. } = &b[k].data else { panic!("not BGR") };
        assert!(pixels.chunks(3).zip(swapped.chunks(3)).all(|(p, q)| p[0] == q[2] && p[1] == q[1] && p[2] == q[0]));
    }
    assert_eq!(rgb.stats().taken, a.len() as u64);
    assert_eq!(rgb.stats().dropped, 0);
    // unread, latest wins: one kept, the rest dropped and counted
    let st = gray.stats();
    assert!(st.made >= 15, "{st:?}");
    assert_eq!((st.queued, st.dropped), (1, st.made - 1));
    let last = drain(&gray);
    assert_eq!(last.len(), 1);
    let TapData::Video { width, height, pixels, .. } = &last[0].data else { panic!() };
    assert_eq!((*width, *height, pixels.len()), (64, 48, 64 * 48));
    assert_eq!(last[0].seq, st.made - 1);
}

#[test]
fn sound_comes_as_overlapping_mono_windows() {
    let s = open();
    let tap = s.tap(TapConfig { rate: 16_000, window_ms: 200, hop_ms: Some(100), queue: 64, ..TapConfig::audio() }).unwrap();
    let got = drain(&tap);
    assert!((4..=7).contains(&got.len()), "{} windows", got.len());
    for (k, f) in got.iter().enumerate() {
        assert_eq!(f.stream, 1);
        let TapData::Audio { rate, samples, .. } = &f.data else { panic!("not sound") };
        assert_eq!((*rate, samples.len()), (16_000, 3200));
        assert!((f.t - 0.1 * k as f64 - got[0].t).abs() < 0.002, "window {k} at {}", f.t);
        assert!(samples.iter().all(|v| v.abs() <= 1.0));
    }
    // overlapping: the second half of one window is the first half of the next
    let (TapData::Audio { samples: x, .. }, TapData::Audio { samples: y, .. }) = (&got[0].data, &got[1].data) else { panic!() };
    assert_eq!(&x[1600..], &y[..1600]);
    assert!(got[0].t < 0.05, "{}", got[0].t);
}

#[test]
fn a_tap_that_cannot_be_fed_says_why() {
    let s = open();
    let tap = s.tap(TapConfig { stream: Some(1), ..TapConfig::video() }).unwrap();
    let e = loop {
        match tap.recv_timeout(Duration::from_secs(10)) {
            Ok(Some(_)) => panic!("a video tap fed from an audio stream"),
            Ok(None) => panic!("no answer for 10 s"),
            Err(e) => break e,
        }
    };
    assert!(e.message.contains("stream 1"), "{}", e.message);
    assert!(s.tap(TapConfig { fps: Some(0.0), ..TapConfig::video() }).is_err());
    assert!(s.tap(TapConfig { letterbox: true, width: Some(100), ..TapConfig::video() }).is_err());
    // a tap let go of is simply no longer fed
    drop(s.tap(TapConfig::audio()).unwrap());
}
