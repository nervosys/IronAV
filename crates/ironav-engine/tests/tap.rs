//! Taps on a live session: decoded pictures and sound handed to code in the same process.

use ironav_engine::live::{self, OpenArgs};
use ironav_engine::tap::{PixelLayout, Placement, Tap, TapConfig, TapData, TapFrame};
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
fn taps_given_at_open_see_the_first_picture_and_sound() {
    // a test:// source starts at once, so a tap added after open would miss its first frames
    let open = |src: &str, c: TapConfig| {
        let args: OpenArgs = serde_json::from_value(json!({ "source": src })).unwrap();
        let (s, mut taps) = live::open_with_taps(&args, vec![TapConfig { queue: 64, ..c }]).unwrap();
        (s, taps.pop().unwrap())
    };
    let (_v, video) = open("test://video?duration=1&fps=20&size=160x90", TapConfig::video());
    let pictures = drain(&video);
    assert_eq!(pictures.len(), 20, "every picture");
    assert_eq!((pictures[0].seq, pictures[0].t), (0, 0.0));
    let (_a, audio) = open("test://audio?duration=1", TapConfig::audio());
    let sound = drain(&audio);
    assert!(!sound.is_empty() && sound[0].t == 0.0, "{:?}", sound.first().map(|f| f.t));
}

#[test]
fn a_letterbox_can_sit_top_left_with_its_own_padding() {
    let s = open();
    // as YOLOX takes its input: the picture at the top left, the rest 114
    let tap = s.tap(TapConfig { fps: Some(5.0), width: Some(160), height: Some(90), letterbox: true, placement: Placement::TopLeft, pad: 114, queue: 64, ..TapConfig::video() }).unwrap();
    let got = drain(&tap);
    assert!(!got.is_empty());
    for f in &got {
        let TapData::Video { width: 160, height: 90, pixels, .. } = &f.data else { panic!("not 160x90") };
        // 4:3 in 16:9: the picture is 120 wide, so columns 120 to 159 are padding
        for y in 0..90 {
            assert!(pixels[(y * 160 + 120) * 3..(y + 1) * 160 * 3].iter().all(|&p| p == 114), "row {y}");
        }
        assert!(pixels[..120 * 3].iter().any(|&p| p != 114), "the picture starts at the left");
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
    assert!((5..=8).contains(&got.len()), "{} windows", got.len());
    for (k, f) in got.iter().enumerate() {
        assert_eq!(f.stream, 1);
        let TapData::Audio { rate, samples, .. } = &f.data else { panic!("not sound") };
        // the last, at the end of the stream, may be short
        if k + 1 < got.len() {
            assert_eq!((*rate, samples.len()), (16_000, 3200));
        } else {
            assert!(*rate == 16_000 && !samples.is_empty() && samples.len() <= 3200, "{}", samples.len());
        }
        assert!((f.t - 0.1 * k as f64 - got[0].t).abs() < 0.002, "window {k} at {}", f.t);
        assert!(samples.iter().all(|v| v.abs() <= 1.0));
    }
    // overlapping: the second half of one window is the first half of the next
    let (TapData::Audio { samples: x, .. }, TapData::Audio { samples: y, .. }) = (&got[0].data, &got[1].data) else { panic!() };
    assert_eq!(&x[1600..], &y[..1600]);
    assert!(got[0].t < 0.05, "{}", got[0].t);
}

#[test]
fn the_end_of_the_stream_closes_a_last_short_window() {
    let s = open();
    let tap = s.tap(TapConfig { rate: 16_000, window_ms: 500, queue: 64, ..TapConfig::audio() }).unwrap();
    let got = drain(&tap);
    // 0.8 s of sound: one whole window, then the 0.3 s after it
    assert_eq!(got.len(), 2, "{} windows", got.len());
    let TapData::Audio { samples: first, .. } = &got[0].data else { panic!() };
    let TapData::Audio { samples: last, .. } = &got[1].data else { panic!() };
    assert_eq!(first.len(), 8000);
    assert!((0.25..0.4).contains(&(last.len() as f64 / 16_000.0)), "{} samples", last.len());
    assert!((got[1].t - got[0].t - 0.5).abs() < 0.002);
}

#[test]
fn a_tap_that_cannot_be_fed_says_why() {
    let s = open();
    let tap = s.tap(TapConfig { stream: Some(1), ..TapConfig::video() }).unwrap();
    let e = match tap.recv_timeout(Duration::from_secs(10)) {
        Ok(Some(_)) => panic!("a video tap fed from an audio stream"),
        Ok(None) => panic!("no answer for 10 s"),
        Err(e) => e,
    };
    assert!(e.message.contains("stream 1"), "{}", e.message);
    assert!(s.tap(TapConfig { fps: Some(0.0), ..TapConfig::video() }).is_err());
    assert!(s.tap(TapConfig { letterbox: true, width: Some(100), ..TapConfig::video() }).is_err());
    // a tap let go of is simply no longer fed
    drop(s.tap(TapConfig::audio()).unwrap());
}

/// Two seconds of a 440 Hz tone as an RTSP source sends it (L16 at 16 kHz, 20 ms a packet), with
/// a sender report early on saying that media time 1 s was captured at Unix time 1_700_000_000.
fn rtp_with_sender_report() -> Vec<u8> {
    use ironav_formats::rtp::{description_frame, frame, Description, TrackSetup};
    let sdp = "v=0\r\nm=audio 0 RTP/AVP 96\r\na=rtpmap:96 L16/16000/1\r\n".to_string();
    let base = 7_000u32;
    let mut out = description_frame(&Description { sdp, tracks: vec![TrackSetup { media: 0, channel: 0, rtptime: Some(base), seq: Some(1) }] }).unwrap();
    for k in 0..100u32 {
        let mut p = vec![0x80, 96];
        p.extend_from_slice(&(k as u16 + 1).to_be_bytes());
        p.extend_from_slice(&(base + k * 320).to_be_bytes());
        p.extend_from_slice(&[0, 0, 0, 9]);
        for i in 0..320u32 {
            let t = (k * 320 + i) as f64 / 16_000.0;
            p.extend_from_slice(&(((2.0 * std::f64::consts::PI * 440.0 * t).sin() * 8000.0) as i16).to_be_bytes());
        }
        out.extend(frame(0, &p).unwrap());
        if k == 1 {
            let mut sr = vec![0x80, 200, 0, 6, 0, 0, 0, 9];
            sr.extend_from_slice(&(1_700_000_000u32.wrapping_add(2_208_988_800)).to_be_bytes());
            sr.extend_from_slice(&[0; 4]);
            sr.extend_from_slice(&(base + 16_000).to_be_bytes());
            sr.extend_from_slice(&[0; 8]);
            out.extend(frame(1, &sr).unwrap());
        }
    }
    out
}

#[test]
fn a_senders_clock_dates_what_it_captured() {
    let path = std::env::temp_dir().join(format!("ironav-tap-sr-{}.rtp", std::process::id()));
    std::fs::write(&path, rtp_with_sender_report()).unwrap();
    let args: OpenArgs = serde_json::from_value(json!({ "source": format!("{}?pace=realtime", path.display()), "analyzers": ["levels"] })).unwrap();
    let s = live::open(&args).unwrap();
    let tap = s.tap(TapConfig { window_ms: 200, queue: 64, ..TapConfig::audio() }).unwrap();
    let got = drain(&tap);
    assert!(got.len() >= 9, "{}", got.len());
    for f in &got {
        let at = f.captured_at.expect("a capture time").duration_since(std::time::UNIX_EPOCH).unwrap().as_secs_f64();
        assert!((at - (1_699_999_999.0 + f.t)).abs() < 0.002, "{at} for t {}", f.t);
    }
    assert_eq!(s.status()["sender_clock"][0]["unix_at_zero"], 1_699_999_999.0);
    let _ = std::fs::remove_file(path);
}
