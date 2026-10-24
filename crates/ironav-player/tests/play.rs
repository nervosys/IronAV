//! End-to-end playback on generated files, with no sound device (the silent clock).

use ironav_player::{AudioMode, OpenOptions, Player, State};
use serde_json::json;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn dir(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ironav-player-{name}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// `secs` of a 440 Hz tone, 16-bit stereo.
fn write_wav(path: &PathBuf, rate: u32, secs: u32) {
    let frames = rate * secs;
    let mut d = Vec::new();
    for i in 0..frames {
        let v = ((i as f64 * 440.0 * std::f64::consts::TAU / rate as f64).sin() * 12000.0) as i16;
        d.extend(v.to_le_bytes());
        d.extend(v.to_le_bytes());
    }
    let mut f = Vec::new();
    f.extend(b"RIFF");
    f.extend((36 + d.len() as u32).to_le_bytes());
    f.extend(b"WAVEfmt ");
    f.extend(16u32.to_le_bytes());
    f.extend(1u16.to_le_bytes());
    f.extend(2u16.to_le_bytes());
    f.extend(rate.to_le_bytes());
    f.extend((rate * 4).to_le_bytes());
    f.extend(4u16.to_le_bytes());
    f.extend(16u16.to_le_bytes());
    f.extend(b"data");
    f.extend((d.len() as u32).to_le_bytes());
    f.extend(d);
    std::fs::write(path, f).unwrap();
}

/// 10 fps video where frame `i` has luma `16 + 8*i`, so a picture identifies its own index.
fn write_y4m(path: &PathBuf, frames: usize) {
    let (w, h) = (64usize, 48usize);
    let mut f = format!("YUV4MPEG2 W{w} H{h} F10:1 Ip A1:1 C420jpeg\n").into_bytes();
    for i in 0..frames {
        f.extend(b"FRAME\n");
        f.extend(vec![(16 + 8 * (i % 30)) as u8; w * h]);
        f.extend(vec![128u8; w * h / 2]);
    }
    std::fs::write(path, f).unwrap();
}

fn until(what: &str, secs: f64, mut f: impl FnMut() -> bool) {
    let t = Instant::now();
    while !f() {
        assert!(t.elapsed().as_secs_f64() < secs, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn silent() -> OpenOptions {
    OpenOptions { audio: AudioMode::Silent, ..Default::default() }
}

#[test]
fn audio_plays_in_real_time_pauses_seeks_and_ends() {
    let p = dir("audio").join("tone.wav");
    write_wav(&p, 8000, 2);
    let pl = Player::open(p.to_str().unwrap(), &silent()).unwrap();
    assert_eq!(pl.info().duration.map(|d| (d * 10.0).round()), Some(20.0));
    until("playing", 5.0, || pl.state() == State::Playing);

    let t0 = pl.position();
    std::thread::sleep(Duration::from_millis(400));
    let adv = pl.position() - t0;
    assert!((0.25..0.8).contains(&adv), "advanced {adv}s in 0.4s");
    assert!(pl.status()["audio"]["level"].as_f64().unwrap() > 0.1, "the tone is audible in the meter");

    pl.pause();
    assert_eq!(pl.state(), State::Paused);
    let at = pl.position();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(pl.position(), at, "paused means still");

    pl.seek(1.5);
    assert!((pl.position() - 1.5).abs() < 0.01, "seek is exact while paused");
    pl.play();
    until("playing after seek", 5.0, || pl.state() == State::Playing);
    until("the end", 5.0, || pl.state() == State::Ended);
    assert!((pl.position() - 2.0).abs() < 0.05, "ended at {}", pl.position());
}

#[test]
fn video_frames_come_out_in_order_and_seek_lands_on_the_right_picture() {
    let p = dir("video").join("v.y4m");
    write_y4m(&p, 30); // three seconds
    let pl = Player::open(p.to_str().unwrap(), &OpenOptions { autoplay: false, ..silent() }).unwrap();
    until("first frame", 5.0, || pl.frame().is_some());
    assert_eq!(pl.frame().unwrap().frame.planes[0][0], 16, "opens paused on frame 0");

    pl.seek(1.25);
    until("buffered", 5.0, || pl.state() != State::Buffering);
    let f = pl.frame().expect("a picture after the seek");
    assert_eq!(f.frame.planes[0][0], 16 + 8 * 12, "1.25 s is frame 12 (t = 1.2 s)");

    // Playing forward shows increasing frames, never going back.
    pl.play();
    let mut last = f.frame.planes[0][0];
    let t = Instant::now();
    while t.elapsed() < Duration::from_millis(500) {
        let cur = pl.frame().unwrap().frame.planes[0][0];
        assert!(cur >= last, "frames went backwards: {last} then {cur}");
        last = cur;
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(last > 16 + 8 * 12, "playback progressed");
    pl.seek(0.0);
    until("buffered again", 5.0, || pl.state() != State::Buffering);
    assert_eq!(pl.frame().unwrap().frame.planes[0][0], 16, "seeking back works");
}

#[test]
fn video_only_files_are_timed_by_the_wall_clock_and_loop() {
    let p = dir("loop").join("v.y4m");
    write_y4m(&p, 5); // half a second
    let pl = Player::open(p.to_str().unwrap(), &OpenOptions { looping: true, ..silent() }).unwrap();
    let t = Instant::now();
    let mut wrapped = false;
    let mut prev = 0.0;
    while t.elapsed() < Duration::from_secs(3) && !wrapped {
        pl.frame();
        let pos = pl.position();
        wrapped = pos + 0.2 < prev;
        prev = pos;
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(wrapped, "position returned to the start");
}

#[test]
fn json_commands_report_status_and_reject_bad_input() {
    let p = dir("cmd").join("v.y4m");
    write_y4m(&p, 20);
    let pl = Player::open(p.to_str().unwrap(), &OpenOptions { autoplay: false, ..silent() }).unwrap();
    until("ready", 5.0, || pl.state() == State::Paused);

    let s = pl.command(&json!({"cmd": "seek", "fraction": 0.5})).unwrap();
    assert_eq!(s["position"], 1.0);
    assert_eq!(s["video"]["width"], 64);
    assert!(s["video"]["decoder"].is_string());

    let e = pl.command(&json!({"cmd": "seek", "to": 1, "by": 1})).unwrap_err();
    assert_eq!(e.code, ironav_core::ErrorCode::InvalidArgument);
    let e = pl.command(&json!({"cmd": "fast-forward"})).unwrap_err();
    assert!(!e.alternatives.is_empty(), "unknown commands list the real ones");
    assert!(pl.command(&json!({"cmd": "volume", "value": 3})).is_err());

    until("buffered", 5.0, || pl.state() == State::Paused);
    let r = pl.command(&json!({"cmd": "snapshot", "max_width": 32})).unwrap();
    assert_eq!(r["snapshot"]["width"], 32);
    assert!(r["snapshot"]["png_base64"].as_str().unwrap().starts_with("iVBOR"), "a PNG");

    let r = pl.command(&json!({"cmd": "play"})).unwrap();
    assert_eq!(r["state"], "playing");
    let r = pl.command(&json!({"cmd": "wait", "state": "ended", "timeout": 5})).unwrap();
    assert_eq!(r["reached"], true);
    assert_eq!(r["state"], "ended");
}

#[test]
fn missing_and_unplayable_files_fail_with_a_usable_error() {
    let e = Player::open("no/such/file.mp4", &silent()).err().unwrap();
    assert_eq!(e.code, ironav_core::ErrorCode::NotFound);
    let p = dir("bad").join("x.wav");
    write_wav(&p, 8000, 1);
    let e = Player::open(p.to_str().unwrap(), &OpenOptions { audio: AudioMode::Off, ..Default::default() }).err().unwrap();
    assert_eq!(e.code, ironav_core::ErrorCode::UnsupportedCodec, "audio-only file with audio off has nothing to play");
}

#[test]
fn headless_playback_reaches_the_end_without_anyone_drawing_frames() {
    let p = dir("headless").join("v.y4m");
    write_y4m(&p, 40); // more pictures than the queue holds, four seconds
    let pl = Player::open(p.to_str().unwrap(), &silent()).unwrap();
    until("the end", 8.0, || pl.state() == State::Ended);
    assert!((pl.position() - 4.0).abs() < 0.2, "ended at {}", pl.position());
}
