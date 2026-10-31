//! Model sidecars end to end, with the example sidecar built from this crate.

use ironav_core::ErrorCode;
use ironav_engine::call_tool;
use serde_json::{json, Value};
use std::f64::consts::PI;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ironav-sidecar-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Points IRONAV_SIDECARS at a list of the example's modes, once for every test here.
fn configure() -> &'static Path {
    static DIR: OnceLock<PathBuf> = OnceLock::new();
    DIR.get_or_init(|| {
        let dir = scratch("config");
        let exe = env!("CARGO_BIN_EXE_ironav-sidecar-example");
        let marker = dir.join("crashed-once");
        let list = json!({
            "dtmf": { "command": [exe, "dtmf"] },
            "echo": { "command": [exe, "echo"] },
            "pictures": { "command": [exe, "pictures"] },
            "crash": { "command": [exe, "crash", "2", marker.to_str().unwrap()] },
            "mute": { "command": [exe, "mute"], "start_seconds": 5 },
            "absent": { "command": ["ironav-no-such-sidecar-program"], "media": "video" },
            "liar": { "command": [exe, "dtmf"], "media": "video" },
            "missing": { "command": ["ironav-no-such-sidecar-program"] },
            "slow": { "command": [exe, "slow", "1", dir.join("slow.pid").to_str().unwrap()] },
        });
        let path = dir.join("sidecars.json");
        std::fs::write(&path, list.to_string()).unwrap();
        std::env::set_var("IRONAV_SIDECARS", &path);
        dir
    })
}

fn write_wav(path: &Path, samples: &[f64], channels: u16, rate: u32) {
    let data: Vec<u8> = samples.iter().flat_map(|s| ((s.clamp(-1.0, 1.0) * 32767.0).round() as i16).to_le_bytes()).collect();
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

#[test]
fn a_sidecar_is_added_to_a_running_session_and_removed() {
    configure();
    let dir = scratch("added");
    let wav = dir.join("late-keys.wav");
    // the keys come late enough for the sidecar to join first, even on a loaded machine
    dtmf_wav_at(&wav, 4.0, 6.0);
    let s = call_tool("stream_open", &json!({ "source": format!("{}?pace=realtime", wav.display()), "analyzers": ["levels"] })).unwrap();
    let id = s["session"].as_str().unwrap().to_string();
    let e = call_tool("stream_configure", &json!({ "session": id, "add": ["nope"] })).unwrap_err();
    assert!(e.alternatives.iter().any(|a| a == "dtmf") && e.alternatives.iter().any(|a| a == "silence"), "{e:?}");
    // one that wants video is not started for audio alone; one that cannot start says so in the log
    let c = call_tool("stream_configure", &json!({ "session": id, "add": ["dtmf", "absent", "missing"] })).unwrap();
    assert_eq!(c["applied"], true, "{c}");
    assert!(c["warnings"].to_string().contains("sidecar 'absent' not run"), "{c}");
    let mut digits = Vec::new();
    let mut after = 0;
    let mut joined = false;
    for _ in 0..40 {
        let ev = call_tool("stream_events", &json!({ "session": id, "after": after, "kinds": ["dtmf"], "wait_ms": 500 })).unwrap();
        for o in ev["events"].as_array().unwrap() {
            digits.push(o["data"]["digit"].as_str().unwrap().to_string());
        }
        after = ev["next_after"].as_u64().unwrap();
        let st = call_tool("stream_status", &json!({ "session": id })).unwrap();
        joined |= st["analyzed"][0]["analyzers"].as_array().unwrap().contains(&json!("dtmf"));
        if digits.len() == 3 {
            break;
        }
    }
    assert_eq!(digits, ["1", "5", "#"]);
    assert!(joined, "the status names the sidecar once it has joined");
    let failed = call_tool("stream_query", &json!({ "session": id, "kinds": ["analyzer_failed"] })).unwrap();
    assert_eq!(failed["matched"], 1, "{failed}");
    assert_eq!(failed["events"][0]["analyzer"], "missing", "{failed}");
    assert!(failed["events"][0].get("stream").is_none_or(|s| s.is_null()), "a failure to start belongs to no stream: {failed}");
    let c = call_tool("stream_configure", &json!({ "session": id, "remove": ["dtmf"] })).unwrap();
    assert_eq!(c["applied"], true, "{c}");
    assert!(!c["analyzed"][0]["analyzers"].as_array().unwrap().contains(&json!("dtmf")), "{c}");
    let _ = call_tool("stream_close", &json!({ "session": id }));
}

/// Whether process `pid` is still running.
fn alive(pid: u32) -> bool {
    #[cfg(windows)]
    {
        let out = std::process::Command::new("tasklist").args(["/FI", &format!("PID eq {pid}"), "/NH"]).output().unwrap();
        String::from_utf8_lossy(&out.stdout).contains(&pid.to_string())
    }
    #[cfg(not(windows))]
    {
        // a zombie still has its /proc entry, so this also tells whether it was waited for
        Path::new(&format!("/proc/{pid}")).exists()
    }
}

#[test]
fn a_sidecar_removed_while_starting_is_stopped_and_never_joins() {
    let dir = configure();
    let pidfile = dir.join("slow.pid");
    let _ = std::fs::remove_file(&pidfile);
    let wav = scratch("slow").join("tone.wav");
    let x: Vec<f64> = (0..48000 * 6).map(|i| 0.3 * (2.0 * PI * 440.0 * i as f64 / 48000.0).sin()).collect();
    write_wav(&wav, &x, 1, 48000);
    let s = call_tool("stream_open", &json!({ "source": format!("{}?pace=realtime", wav.display()), "analyzers": ["levels"] })).unwrap();
    let id = s["session"].as_str().unwrap().to_string();
    call_tool("stream_configure", &json!({ "session": id, "add": ["slow"] })).unwrap();
    // it has started, and is waiting before its hello
    let t0 = std::time::Instant::now();
    let pid = loop {
        if let Some(p) = std::fs::read_to_string(&pidfile).ok().and_then(|t| t.trim().parse::<u32>().ok()) {
            break p;
        }
        assert!(t0.elapsed().as_secs() < 10, "the sidecar never started");
        std::thread::sleep(std::time::Duration::from_millis(20));
    };
    assert!(alive(pid), "the sidecar waits a second before its hello");
    let c = call_tool("stream_configure", &json!({ "session": id, "remove": ["slow"] })).unwrap();
    assert_eq!(c["applied"], true, "{c}");
    // once its start ends (after its hello) it is stopped rather than attached
    let t0 = std::time::Instant::now();
    while alive(pid) {
        assert!(t0.elapsed().as_secs() < 10, "the removed sidecar is still running");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    std::thread::sleep(std::time::Duration::from_millis(300));
    let st = call_tool("stream_status", &json!({ "session": id })).unwrap();
    assert!(!st["analyzed"][0]["analyzers"].as_array().unwrap().contains(&json!("slow")), "{st}");
    let failed = call_tool("stream_query", &json!({ "session": id, "kinds": ["analyzer_failed"] })).unwrap();
    assert_eq!(failed["matched"], 0, "a removal is not a failure: {failed}");
    let _ = call_tool("stream_close", &json!({ "session": id }));
}

/// Keys 1, 5 and # for 120 ms each, 120 ms apart, from 0.5 s, in 48 kHz stereo.
fn dtmf_wav(path: &Path) {
    dtmf_wav_at(path, 0.5, 1.5);
}

/// The same keys from `start` seconds, in `secs` of audio.
fn dtmf_wav_at(path: &Path, start: f64, secs: f64) {
    let rate = 48000.0;
    let tones = [(697.0, 1209.0), (770.0, 1336.0), (941.0, 1477.0)];
    let x: Vec<f64> = (0..(rate * secs) as usize)
        .flat_map(|i| {
            let t = i as f64 / rate;
            let k = ((t - start) / 0.24).floor();
            let on = t >= start && k < 3.0 && (t - start - k * 0.24) < 0.12;
            let v = if on {
                let (a, b) = tones[k as usize];
                0.3 * ((2.0 * PI * a * t).sin() + (2.0 * PI * b * t).sin())
            } else {
                0.0
            };
            [v, v]
        })
        .collect();
    write_wav(path, &x, 2, rate as u32);
}

/// A voiced sound (harmonics of 120 Hz under a 4 Hz syllable envelope) from 1 s to 4 s of 6.
fn speech_wav(path: &Path) {
    let rate = 16000.0;
    let x: Vec<f64> = (0..(rate * 6.0) as usize)
        .map(|i| {
            let t = i as f64 / rate;
            let noise = 0.001 * ((i * 7919 % 1000) as f64 / 500.0 - 1.0);
            if !(1.0..4.0).contains(&t) {
                return noise;
            }
            let env = 0.5 - 0.5 * (2.0 * PI * 4.0 * (t - 1.0)).cos();
            noise + 0.2 * env * (1..8).map(|k| (2.0 * PI * 120.0 * k as f64 * t).sin() / k as f64).sum::<f64>()
        })
        .collect();
    write_wav(path, &x, 1, rate as u32);
}

fn of_kind<'a>(r: &'a Value, kind: &str) -> Vec<&'a Value> {
    r["observations"].as_array().unwrap().iter().filter(|o| o["kind"] == kind).collect()
}

#[test]
fn a_sidecar_finds_keypad_tones_in_resampled_downmixed_audio() {
    configure();
    let dir = scratch("dtmf");
    let wav = dir.join("keys.wav");
    dtmf_wav(&wav);
    let r = call_tool("analyze", &json!({ "input": wav.to_str().unwrap(), "analyzers": ["levels"], "sidecars": ["dtmf"] })).unwrap();
    let keys = of_kind(&r, "dtmf");
    let digits: Vec<&str> = keys.iter().map(|o| o["data"]["digit"].as_str().unwrap()).collect();
    assert_eq!(digits, ["1", "5", "#"], "{r}");
    for (k, o) in keys.iter().enumerate() {
        let t = o["t"].as_f64().unwrap();
        assert!((t - (0.5 + 0.24 * k as f64)).abs() < 0.06, "{o}");
        assert_eq!(o["analyzer"], "dtmf");
        assert_eq!(o["stream"], 0);
    }
    let summary = &of_kind(&r, "summary")[0]["data"]["audio0"]["dtmf"];
    assert_eq!(summary["kinds"]["dtmf"], 3, "{r}");
    assert!(r["streams"][0]["analyzers"].as_array().unwrap().contains(&json!("dtmf")), "{r}");
}

#[test]
fn speech_gating_sends_only_the_utterance_with_its_pre_roll() {
    configure();
    let dir = scratch("echo");
    let wav = dir.join("speech.wav");
    speech_wav(&wav);
    let r = call_tool("analyze", &json!({ "input": wav.to_str().unwrap(), "analyzers": ["voice_activity"], "sidecars": ["echo"] })).unwrap();
    let chunks = of_kind(&r, "chunk");
    assert!(!chunks.is_empty(), "{r}");
    let first = chunks[0]["t"].as_f64().unwrap();
    assert!((0.6..1.05).contains(&first), "the utterance starts near 1 s, with a little before it: {r}");
    let last = chunks.last().unwrap();
    assert_eq!(last["data"]["final"], true, "{r}");
    assert!(chunks[..chunks.len() - 1].iter().all(|c| c["data"]["final"] == false), "{r}");
    let end = last["t_end"].as_f64().unwrap();
    assert!((4.0..5.0).contains(&end), "{r}");
    // the chunks join up without a gap
    for w in chunks.windows(2) {
        assert!((w[0]["t_end"].as_f64().unwrap() - w[1]["t"].as_f64().unwrap()).abs() < 0.002, "{r}");
    }
    // the voice-activity analyzer agrees, and the summary carries the sidecar's text (a digest may
    // close before the sidecar's answers arrive; the summary waits for them)
    assert_eq!(of_kind(&r, "speech_start").len(), 1, "{r}");
    let digest = &of_kind(&r, "digest")[0]["data"]["audio0"];
    assert!(digest["voice_activity"]["speech_fraction"].as_f64().unwrap() > 0.2, "{r}");
    let summary = &of_kind(&r, "summary")[0]["data"]["audio0"];
    assert!(summary["echo"]["text"].as_str().unwrap().starts_with("chunk1 chunk2"), "{r}");
}

#[test]
fn a_video_sidecar_gets_pictures_at_its_interval() {
    configure();
    let dir = scratch("pictures");
    // 64x48 at 25 fps: 1 s of grey, 1 s of black
    let mut f = b"YUV4MPEG2 W64 H48 F25:1 Ip A1:1 C420jpeg\n".to_vec();
    for i in 0..50 {
        f.extend_from_slice(b"FRAME\n");
        f.extend(std::iter::repeat(if i < 25 { 150u8 } else { 16 }).take(64 * 48));
        f.extend(std::iter::repeat(128u8).take(64 * 48 / 2));
    }
    let y4m = dir.join("v.y4m");
    std::fs::write(&y4m, f).unwrap();
    let r = call_tool("analyze", &json!({ "input": y4m.to_str().unwrap(), "analyzers": ["black"], "sidecars": ["pictures"], "hwaccel": "off" })).unwrap();
    let pics = of_kind(&r, "picture");
    let ts: Vec<f64> = pics.iter().map(|p| p["t"].as_f64().unwrap()).collect();
    // the first picture at or after each due time, which do not drift
    assert_eq!(ts, [0.0, 0.52, 1.0, 1.52], "{r}");
    assert_eq!(pics[0]["data"]["width"], 32);
    assert_eq!(pics[0]["data"]["height"], 24);
    assert!(pics[0]["data"]["mean"].as_f64().unwrap() > 100.0 && pics[2]["data"]["mean"].as_f64().unwrap() < 5.0, "{r}");
}

#[test]
fn a_crashed_sidecar_is_restarted_and_carries_on() {
    configure();
    let dir = scratch("crash");
    let wav = dir.join("tone.wav");
    let x: Vec<f64> = (0..16000 * 4).map(|i| 0.2 * (2.0 * PI * 440.0 * i as f64 / 16000.0).sin()).collect();
    write_wav(&wav, &x, 1, 16000);
    let r = call_tool("analyze", &json!({ "input": wav.to_str().unwrap(), "analyzers": ["levels"], "sidecars": ["crash"], "max_observations": 1000 })).unwrap();
    let failed = of_kind(&r, "analyzer_failed");
    assert_eq!(failed.len(), 1, "{r}");
    assert_eq!(failed[0]["data"]["restarting"], true);
    assert!(failed[0]["data"]["message"].as_str().unwrap().contains("crashing on purpose"), "{r}");
    let chunks = of_kind(&r, "chunk");
    assert!(chunks.iter().any(|c| c["t"].as_f64().unwrap() >= 2.0), "the restarted child carried on: {r}");
    assert_eq!(of_kind(&r, "summary")[0]["data"]["audio0"]["crash"]["restarts"], 1, "{r}");
}

#[test]
fn sidecars_are_named_by_the_operator_and_failures_say_why() {
    let dir = configure();
    let wav = dir.join("short.wav");
    write_wav(&wav, &vec![0.0; 8000], 1, 8000);
    let e = call_tool("analyze", &json!({ "input": wav.to_str().unwrap(), "sidecars": ["nope"] })).unwrap_err();
    assert_eq!(e.code, ErrorCode::InvalidArgument);
    assert!(e.alternatives.iter().any(|a| a == "dtmf"), "{e:?}");
    assert!(e.hint.as_deref().unwrap_or("").contains("IRONAV_SIDECARS"), "{e:?}");
    let e = call_tool("analyze", &json!({ "input": wav.to_str().unwrap(), "sidecars": ["mute"] })).unwrap_err();
    assert_eq!(e.code, ErrorCode::Io);
    assert!(e.message.contains("before saying hello"), "{e:?}");
    assert!(e.hint.as_deref().unwrap_or("").contains("no model here"), "{e:?}");
    // a video sidecar on audio alone is not run, and says so
    let r = call_tool("analyze", &json!({ "input": wav.to_str().unwrap(), "analyzers": ["levels"], "sidecars": ["pictures"] })).unwrap();
    assert!(r["warnings"].to_string().contains("sidecar 'pictures' not run"), "{r}");
    // one that says it wants video is not even started for audio alone (this one could not start)
    let r = call_tool("analyze", &json!({ "input": wav.to_str().unwrap(), "analyzers": ["levels"], "sidecars": ["absent", "dtmf"] })).unwrap();
    assert!(r["warnings"].to_string().contains("sidecar 'absent' not run: it wants video"), "{r}");
    // and one whose hello disagrees with what the list says fails
    let mut f = b"YUV4MPEG2 W16 H16 F25:1 Ip A1:1 C420jpeg
".to_vec();
    for _ in 0..5 {
        f.extend_from_slice(b"FRAME
");
        f.extend(std::iter::repeat(128u8).take(16 * 16 * 3 / 2));
    }
    let y4m = dir.join("tiny.y4m");
    std::fs::write(&y4m, f).unwrap();
    let e = call_tool("analyze", &json!({ "input": y4m.to_str().unwrap(), "sidecars": ["liar"], "hwaccel": "off" })).unwrap_err();
    assert!(e.message.contains("asked for audio but IRONAV_SIDECARS says video"), "{e:?}");
    let caps = call_tool("capabilities", &json!({})).unwrap();
    assert!(caps["sidecars"].as_array().unwrap().contains(&json!("echo")), "{caps}");
}

#[test]
fn a_live_session_runs_a_sidecar() {
    configure();
    let dir = scratch("live");
    let wav = dir.join("keys.wav");
    dtmf_wav(&wav);
    let s = call_tool("stream_open", &json!({ "source": wav.to_str().unwrap(), "analyzers": ["levels"], "sidecars": ["dtmf"] })).unwrap();
    let id = s["session"].as_str().unwrap().to_string();
    let mut digits = Vec::new();
    let mut after = 0;
    for _ in 0..40 {
        let ev = call_tool("stream_events", &json!({ "session": id, "after": after, "kinds": ["dtmf"], "wait_ms": 1000 })).unwrap();
        for o in ev["events"].as_array().unwrap() {
            digits.push(o["data"]["digit"].as_str().unwrap().to_string());
        }
        after = ev["next_after"].as_u64().unwrap();
        if ev["state"] != "running" && ev["state"] != "waiting" && !ev["more"].as_bool().unwrap() {
            break;
        }
    }
    let _ = call_tool("stream_close", &json!({ "session": id }));
    assert_eq!(digits, ["1", "5", "#"]);
}
