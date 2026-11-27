//! End-to-end playback on generated files, with no sound device (the silent clock).

use ironav_player::{AudioEffects, AudioMode, OpenOptions, Player, State, SubtitleTrack};
use serde_json::json;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
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
fn reverse_playback_shows_descending_frames_across_decode_windows() {
    let path = dir("reverse").join("reverse.y4m");
    write_y4m(&path, 30);
    let p = Player::open(path.to_str().unwrap(), &OpenOptions { autoplay: false, ..silent() }).unwrap();
    p.play_at_rate(-2.0).unwrap();
    assert_eq!(p.playback_rate(), -2.0);

    let mut last = f64::INFINITY;
    let mut pictures = 0;
    let mut observe = || {
        if let Some(f) = p.frame() {
            assert!(f.time <= last, "reverse pictures advanced from {last} to {}", f.time);
            if f.time < last {
                pictures += 1;
                last = f.time;
            }
            let index = (f.time * 10.0).round() as usize;
            assert_eq!(usize::from(f.frame.planes[0][0]), 16 + 8 * index);
        }
    };
    until("reverse advances from the end", 5.0, || {
        observe();
        p.state() == State::Playing && p.position() < 2.45
    });
    p.pause();
    let position = p.position();
    std::thread::sleep(Duration::from_millis(60));
    assert_eq!(p.position(), position);
    p.play();
    until("reverse reaches the beginning", 5.0, || {
        observe();
        p.state() == State::Ended
    });
    assert!(pictures >= 6, "only {pictures} distinct reverse pictures observed");
    assert_eq!(p.position(), 0.0);
    assert_eq!(p.frame().unwrap().time, 0.0);

    p.play_at_rate(1.0).unwrap();
    until("forward playback after reversing", 5.0, || p.frame().is_some_and(|f| f.time >= 0.2));
    assert_eq!(p.playback_rate(), 1.0);
}

#[test]
fn forward_speed_keeps_audio_and_the_playback_clock_in_sync() {
    let path = dir("rate").join("tone.wav");
    write_wav(&path, 8000, 4);
    let p = Player::open(path.to_str().unwrap(), &OpenOptions { autoplay: false, ..silent() }).unwrap();
    p.play_at_rate(2.0).unwrap();
    until("playing twice as fast", 5.0, || p.state() == State::Playing);
    let (wall, pos) = (Instant::now(), p.position());
    std::thread::sleep(Duration::from_millis(250));
    let speed = (p.position() - pos) / wall.elapsed().as_secs_f64();
    assert!((1.5..2.5).contains(&speed), "media clock advanced {speed:.2}× wall time");
    assert!(p.status()["audio"]["level"].as_f64().unwrap() > 0.05);
    assert!(p.play_at_rate(-1.0).is_err(), "an audio-only file cannot play silent reverse video");
    assert_eq!(p.playback_rate(), 2.0);
}

#[test]
fn waterfall_tracks_playback_and_discards_old_rows_on_seek() {
    let path = dir("waterfall").join("tone.wav");
    write_wav(&path, 8000, 4);
    let p = Player::open(path.to_str().unwrap(), &OpenOptions { autoplay: false, ..silent() }).unwrap();
    p.set_audio_visualization(true);
    p.set_audio_effects(AudioEffects { compression: 1.0, ..Default::default() });
    p.seek(0.0);
    let mut rows = Vec::new();
    p.audio_spectrum(&mut rows);
    assert!(rows.is_empty(), "preroll is hidden before the clock reaches it");
    p.play();
    until("audible spectrum rows", 5.0, || {
        p.audio_spectrum(&mut rows);
        rows.len() >= 3
    });
    assert!(rows.windows(2).all(|rows| rows[0].time < rows[1].time));
    assert!(rows.last().unwrap().time <= p.position());
    assert!(rows.last().unwrap().bands.iter().any(|&band| band > 100));
    assert!((0.02..0.2).contains(&p.audio_level()), "compression is audible at the output");
    p.pause();
    p.seek(2.0);
    p.audio_spectrum(&mut rows);
    assert!(rows.is_empty());
    p.play();
    until("spectrum rows at the new position", 5.0, || {
        p.audio_spectrum(&mut rows);
        rows.first().is_some_and(|row| row.time >= 2.0)
    });
}

#[test]
fn reverse_flushes_reordered_video_and_forward_restores_audio() {
    use std::process::Command;

    if !Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success()) {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let path = dir("reverse-av").join("reverse.mp4");
    let result = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-nostdin",
            "-f", "lavfi", "-i", "testsrc2=size=64x48:rate=15",
            "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000",
            "-t", "3", "-c:v", "libx264", "-g", "15", "-bf", "2", "-c:a", "aac"])
        .arg(&path).status().unwrap();
    assert!(result.success(), "could not create the H.264/AAC test file");
    let p = Player::open(path.to_str().unwrap(), &OpenOptions {
        autoplay: false, hwaccel: "off".into(), ..silent()
    }).unwrap();
    until("the opening H.264 frame", 5.0, || p.frame().is_some());
    let first = p.frame().unwrap().time;
    p.play_at_rate(-1.5).unwrap();
    let mut last = f64::INFINITY;
    let mut pictures = 0;
    until("reverse reaches the first H.264 frame", 7.0, || {
        assert!(p.error().is_none(), "{:?}", p.error());
        if let Some(f) = p.frame() {
            assert!(f.time <= last, "reverse pictures advanced from {last} to {}", f.time);
            if f.time < last {
                pictures += 1;
                last = f.time;
            }
        }
        p.state() == State::Ended
    });
    assert!(pictures >= 10, "only {pictures} distinct reverse pictures observed");
    assert_eq!(p.frame().unwrap().time, first);
    assert_eq!(p.status()["audio"]["level"].as_f64(), Some(0.0));
    p.set_audio_visualization(true);
    p.set_audio_effects(AudioEffects { echo: 0.3, ..Default::default() });
    p.play_at_rate(2.0).unwrap();
    let mut rows = Vec::new();
    until("forward video, processed audio and the waterfall advance together", 5.0, || {
        p.audio_spectrum(&mut rows);
        p.state() == State::Playing && p.audio_level() > 0.01 && !rows.is_empty() && p.frame().is_some_and(|f| f.time > first)
    });
}

#[test]
fn standalone_subtitles_follow_the_player_clock_through_paused_seeks() {
    let folder = dir("sidecar");
    let video = folder.join("video.y4m");
    let captions = folder.join("captions.srt");
    write_y4m(&video, 35);
    std::fs::write(&captions, "1\n00:00:00,500 --> 00:00:02,500\n<i>First</i>\n\n2\n00:00:01,000 --> 00:00:01,250\nSecond\n").unwrap();

    let p = Player::open(video.to_str().unwrap(), &OpenOptions { autoplay: false, ..silent() }).unwrap();
    let track = SubtitleTrack::load(&captions, None, &AtomicBool::new(false)).unwrap();
    until("first picture", 5.0, || p.frame().is_some());
    assert_eq!(track.at(p.position()), (String::new(), Some(0.5)));
    for (to, text, next) in [
        (0.5, "First", Some(1.0)),
        (1.125, "First\nSecond", Some(1.25)),
        (1.25, "First", Some(2.5)),
        (2.5, "", None),
        (1.0, "First\nSecond", Some(1.25)),
        (0.0, "", Some(0.5)),
    ] {
        p.seek(to);
        until("paused after seek", 5.0, || p.state() == State::Paused);
        assert_eq!(track.at(p.position()), (text.into(), next), "seek to {to}");
    }
}

#[test]
fn embedded_subtitles_keep_cues_that_begin_before_a_seek_window() {
    use std::process::Command;

    if !Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success()) {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let folder = dir("embedded-subtitles");
    let first = folder.join("first.srt");
    let second = folder.join("second.srt");
    let video = folder.join("video.mkv");
    std::fs::write(&first, "1\n00:00:00,500 --> 00:00:16,000\nLong cue\n\n2\n00:00:12,000 --> 00:00:14,000\nLater cue\n").unwrap();
    std::fs::write(&second, "1\n00:00:12,000 --> 00:00:14,000\n<i>Other track</i>\n").unwrap();
    let result = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-nostdin", "-f", "lavfi", "-i", "testsrc2=size=64x48:rate=5:duration=24", "-i"])
        .arg(&first).arg("-i").arg(&second)
        .args(["-map", "0:v", "-map", "1:s", "-map", "2:s", "-c:v", "mpeg4", "-g", "5", "-c:s:0", "srt", "-c:s:1", "ass"])
        .arg(&video).output().unwrap();
    assert!(result.status.success(), "{}", String::from_utf8_lossy(&result.stderr));

    let p = Player::open(video.to_str().unwrap(), &OpenOptions { autoplay: false, audio: AudioMode::Off, hwaccel: "off".into(), ..Default::default() }).unwrap();
    let [first, second] = p.subtitle_tracks() else { panic!("expected two subtitle tracks") };
    assert_eq!((first.codec, second.codec), (ironav_core::CodecId::SubRip, ironav_core::CodecId::Ass));
    let cancelled = AtomicBool::new(false);
    let srt = SubtitleTrack::load(&video, Some(first.index), &cancelled).unwrap();
    let ass = SubtitleTrack::load(&video, Some(second.index), &cancelled).unwrap();
    p.seek(13.0);
    until("first picture after seek", 5.0, || p.frame().is_some());
    assert_eq!(srt.at(p.position()), ("Long cue\nLater cue".into(), Some(14.0)));
    assert_eq!(ass.at(p.position()), ("Other track".into(), Some(14.0)));
    assert_eq!(srt.at(14.0), ("Long cue".into(), Some(16.0)));
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

// ---- seeking in a file with key frames -------------------------------------------------------

/// `frames` pictures of H.264 at 10 fps, an IDR picture every `keyint`, each picture a little different from the last, written to `path` as `format` ("mp4" or "mkv").
fn write_h264(path: &std::path::Path, format: &str, frames: usize, keyint: u64) {
    use ironav_core::{CodecId, Encoder, Frame, PixelFormat, Rational, StreamInfo, StreamParams, VideoFrame};
    let (w, h) = (64u32, 48u32);
    let params = StreamParams::Video { width: w, height: h, pix_fmt: PixelFormat::Yuv420p, frame_rate: Rational::new(10, 1) };
    let mut enc = ironav_codec::encoders::H264Encoder::new(&params, None, None).unwrap().with_keyint(keyint);
    let info = StreamInfo {
        index: 0,
        codec: CodecId::H264,
        time_base: Rational::new(1, 10),
        duration: Some(frames as i64),
        bits: None,
        extradata: ironav_codec::encoder_extradata(CodecId::H264, &params),
        lead_in: ironav_codec::encoder_lead_in(CodecId::H264),
        params,
    };
    let mut mux = ironav_formats::create_muxer(path, format).unwrap();
    mux.write_header(std::slice::from_ref(&info)).unwrap();
    let mut packets = Vec::new();
    for n in 0..frames {
        // a gradient that drifts a little each picture: smooth enough that the encoder sees no scene cut, so
        // the key frames are the ones asked for
        let (wu, hu) = (w as usize, h as usize);
        let luma: Vec<u8> = (0..wu * hu).map(|k| (40 + ((k % wu) * 2 + (k / wu) + n) % 160) as u8).collect();
        let f = VideoFrame { width: w, height: h, pix_fmt: PixelFormat::Yuv420p, pts: n as i64, planes: vec![luma, vec![128; wu * hu / 4], vec![128; wu * hu / 4]] };
        packets.extend(enc.encode(&Frame::Video(f)).unwrap());
    }
    packets.extend(enc.flush().unwrap());
    for p in packets {
        mux.write_packet(&p).unwrap();
    }
    mux.finish().unwrap();
}

/// The time of the first picture shown after a seek to `to` in `mode`.
fn first_after_seek(pl: &Player, to: f64, mode: ironav_player::SeekMode) -> f64 {
    pl.seek_mode(to, mode);
    until("a picture after the seek", 10.0, || pl.frame().is_some());
    pl.frame().unwrap().time
}

#[test]
fn playback_and_seek_previews_use_the_reported_field_order() {
    use ironav_core::{FieldOrder, Frame};
    use ironav_player::SeekMode::{Accurate, Keyframe};
    use std::process::Command;

    if !Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success()) {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let root = dir("field-order");
    let top = "testsrc2=s=160x96:r=50,interlace=scan=tff";
    let bottom = "testsrc2=s=160x96:r=50,interlace=scan=bff";
    let progressive = "testsrc2=s=160x96:r=25";
    let mut cases = vec![
        ("top", top, "mpeg2video", "off", FieldOrder::TopFirst),
        ("bottom", bottom, "mpeg2video", "off", FieldOrder::BottomFirst),
        ("progressive", progressive, "mpeg2video", "off", FieldOrder::Progressive),
    ];
    // Opt in on a machine with the NVDEC driver and FFmpeg's libx264 encoder.
    if std::env::var_os("IRONAV_TEST_NVDEC").is_some() {
        cases.push(("nvdec-bottom", bottom, "libx264", "cuda", FieldOrder::BottomFirst));
    }
    for (name, source, encoder, hwaccel, want) in cases {
        let path = root.join(format!("{name}.mkv"));
        let mut ffmpeg = Command::new("ffmpeg");
        let flags = if want.is_interlaced() { "+ildct+ilme" } else { "-ildct-ilme" };
        ffmpeg.args(["-v", "error", "-y", "-nostdin", "-f", "lavfi", "-i", source, "-frames:v", "12", "-c:v", encoder,
            "-pix_fmt", "yuv420p", "-flags", flags, "-g", "24", "-bf", "2", "-threads", "1"]);
        if encoder == "libx264" {
            ffmpeg.args(["-x264-params", "interlaced=1:bff=1"]);
        }
        let status = ffmpeg.arg(&path).status().unwrap();
        assert!(status.success(), "encode {name}");

        let mut demux = ironav_formats::open_demuxer_lazy(&path).unwrap();
        let stream = &demux.streams()[0];
        let time_base = stream.time_base.as_f64();
        let (mut decoder, _) = ironav_player::open_video_decoder(stream, hwaccel).unwrap();
        let mut expected = Vec::new();
        let mut take = |batch: Vec<Frame>, orders: Vec<FieldOrder>| {
            assert_eq!(batch.len(), orders.len());
            for (frame, order) in batch.into_iter().zip(orders) {
                assert_eq!(order, want, "{name}");
                let Frame::Video(frame) = frame else { panic!("expected a video picture") };
                let frame = if order.is_interlaced() {
                    let filtered = ironav_codec::deinterlace_frame(&frame, order).unwrap();
                    assert!(filtered.planes != frame.planes, "the {name} fixture must exercise filtering");
                    filtered
                } else {
                    frame
                };
                expected.push(frame);
            }
        };
        while let Some(packet) = demux.read_packet().unwrap() {
            let frames = decoder.decode(&packet).unwrap();
            take(frames, decoder.field_orders());
        }
        let frames = decoder.flush().unwrap();
        assert!(!frames.is_empty(), "the {name} fixture must exercise decoder flush");
        take(frames, decoder.field_orders());
        assert_eq!(expected.len(), 12);
        drop(decoder);

        let pl = Player::open(path.to_str().unwrap(), &OpenOptions { autoplay: false, hwaccel: hwaccel.into(), ..silent() }).unwrap();
        let check = |index: usize| {
            let shown = pl.frame().unwrap();
            assert_eq!(shown.time, expected[index].pts as f64 * time_base, "{name}, picture {index}");
            assert!(shown.frame.planes == expected[index].planes, "{name}, picture {index}: displayed pixels differ");
        };
        until("first picture", 10.0, || pl.frame().is_some());
        check(0);
        let target = expected[5].pts as f64 * time_base + 0.01;
        first_after_seek(&pl, target, Keyframe);
        check(0);
        first_after_seek(&pl, target, Accurate);
        check(5);
        first_after_seek(&pl, expected[11].pts as f64 * time_base + 0.01, Accurate);
        check(11);
    }
}

#[test]
fn accurate_seeks_land_on_the_frame_at_the_target_and_keyframe_seeks_on_the_keyframe_before_it() {
    use ironav_player::SeekMode::{Accurate, Keyframe};
    for format in ["mp4", "mkv"] {
        let p = dir(&format!("gop-{format}")).join(format!("v.{format}"));
        write_h264(&p, format, 60, 10); // six seconds, a key frame each second
        let opts = OpenOptions { autoplay: false, hwaccel: "off".into(), ..silent() };
        let pl = Player::open(p.to_str().unwrap(), &opts).unwrap();
        until("first frame", 10.0, || pl.frame().is_some());
        // a file with reordered pictures starts a frame or so after zero: times below are from its first picture
        let off = pl.frame().unwrap().time;
        assert!((0.0..0.25).contains(&off), "{format}: the first picture is at {off}");

        // (target, the frame at it, the key frame at or before it), in seconds after the first picture
        for (to, exact, key) in [(2.5, 2.5, 2.0), (2.55, 2.5, 2.0), (4.0, 4.0, 4.0), (4.99, 4.9, 4.0), (0.3, 0.3, 0.0), (5.95, 5.9, 5.0)] {
            // the order matters: a keyframe seek leaves the decoder after the keyframe, an accurate one after the target
            let k = first_after_seek(&pl, off + to, Keyframe);
            assert!((k - (off + key)).abs() < 1e-6, "{format}: keyframe seek to {to} showed {}, wanted {key}", k - off);
            let a = first_after_seek(&pl, off + to, Accurate);
            assert!((a - (off + exact)).abs() < 1e-6, "{format}: accurate seek to {to} showed {}, wanted {exact}", a - off);
        }
    }
}

#[test]
fn a_decoder_that_was_reset_for_a_seek_shows_the_same_pictures_as_a_new_one() {
    use ironav_player::SeekMode::Accurate;
    let p = dir("reset").join("v.mp4");
    write_h264(&p, "mp4", 60, 10);
    let opts = OpenOptions { autoplay: false, hwaccel: "off".into(), ..silent() };
    let fresh = |t: f64| {
        let pl = Player::open(p.to_str().unwrap(), &OpenOptions { start: t, ..opts.clone() }).unwrap();
        until("a picture", 10.0, || pl.frame().is_some());
        let f = pl.frame().unwrap();
        (f.time, f.frame.planes[0].clone())
    };
    let pl = Player::open(p.to_str().unwrap(), &opts).unwrap();
    until("first frame", 10.0, || pl.frame().is_some());
    // forwards, backwards and into the same GOP twice, so the decoder holds references from elsewhere each time
    for t in [3.5, 1.2, 5.4, 5.45, 0.0, 3.3] {
        first_after_seek(&pl, t, Accurate);
        let f = pl.frame().unwrap();
        let (ft, fp) = fresh(t);
        assert_eq!(f.time, ft, "seek to {t}");
        assert!(f.frame.planes[0] == fp, "the picture at {t} differs from a freshly opened decoder's");
    }
}

#[test]
fn a_keyframe_seek_while_paused_decodes_one_picture_and_playing_carries_on_from_it() {
    use ironav_player::SeekMode::Keyframe;
    let p = dir("scrub").join("v.mkv");
    write_h264(&p, "mkv", 60, 10);
    let pl = Player::open(p.to_str().unwrap(), &OpenOptions { autoplay: false, hwaccel: "off".into(), ..silent() }).unwrap();
    until("first frame", 10.0, || pl.frame().is_some());
    let off = pl.frame().unwrap().time;
    // a burst of them, as a seek bar being dragged makes
    for t in [1.5, 2.5, 3.5, 4.5, 3.2] {
        pl.seek_mode(off + t, Keyframe);
    }
    until("a picture", 10.0, || pl.frame().is_some());
    let at = pl.frame().unwrap().time;
    assert!((at - (off + 3.0)).abs() < 1e-6, "showed {}", at - off);
    std::thread::sleep(Duration::from_millis(150));
    assert!(pl.status()["video"]["frames_queued"].as_u64().unwrap() <= 2, "paused after the keyframe: nothing more is decoded");
    // playing resumes from the decoder where it stopped, and shows later pictures
    pl.play();
    until("later pictures", 10.0, || pl.frame().is_some_and(|f| f.time > off + 3.2));
}

#[test]
fn the_seek_command_takes_an_optional_mode() {
    let p = dir("cmdmode").join("v.mp4");
    write_h264(&p, "mp4", 60, 10);
    let pl = Player::open(p.to_str().unwrap(), &OpenOptions { autoplay: false, hwaccel: "off".into(), ..silent() }).unwrap();
    until("first frame", 10.0, || pl.frame().is_some());
    let off = pl.frame().unwrap().time;
    let shown = |pl: &Player| pl.frame().unwrap().time - off;
    pl.command(&json!({"cmd": "seek", "to": off + 2.5, "mode": "keyframe"})).unwrap();
    until("a picture", 10.0, || pl.frame().is_some());
    assert!((shown(&pl) - 2.0).abs() < 1e-6, "keyframe seek to 2.5 showed {}", shown(&pl));
    pl.command(&json!({"cmd": "seek", "to": off + 2.5, "mode": "accurate"})).unwrap();
    until("the exact picture", 10.0, || pl.frame().is_some_and(|f| f.time - off > 2.2));
    assert!((shown(&pl) - 2.5).abs() < 1e-6, "showed {}", shown(&pl));
    // without a mode, as before
    pl.command(&json!({"cmd": "seek", "to": off + 3.55})).unwrap();
    until("a picture", 10.0, || pl.frame().is_some());
    assert!((shown(&pl) - 3.5).abs() < 1e-6, "showed {}", shown(&pl));
    let e = pl.command(&json!({"cmd": "seek", "to": 1, "mode": "sloppy"})).unwrap_err();
    assert_eq!(e.code, ironav_core::ErrorCode::InvalidArgument);
    assert!(e.alternatives.iter().any(|a| a == "keyframe"));
}
