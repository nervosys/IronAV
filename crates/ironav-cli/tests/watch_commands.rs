//! `ironav watch --commands`: commands on stdin, replies among the observations on stdout.

use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

/// `secs` of 16 kHz mono: a 1 kHz tone, silent from 1 s to 2.5 s.
fn wav(path: &std::path::Path, secs: f64) {
    let rate = 16000u32;
    let data: Vec<u8> = (0..(rate as f64 * secs) as usize)
        .flat_map(|i| {
            let t = i as f64 / rate as f64;
            let s = if (1.0..2.5).contains(&t) { 0 } else { (6000.0 * (2.0 * std::f64::consts::PI * 1000.0 * t).sin()) as i16 };
            s.to_le_bytes()
        })
        .collect();
    let mut f = b"RIFF".to_vec();
    f.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    f.extend_from_slice(b"WAVEfmt ");
    f.extend_from_slice(&16u32.to_le_bytes());
    f.extend_from_slice(&[1, 0, 1, 0]);
    f.extend_from_slice(&rate.to_le_bytes());
    f.extend_from_slice(&(rate * 2).to_le_bytes());
    f.extend_from_slice(&[2, 0, 16, 0]);
    f.extend_from_slice(b"data");
    f.extend_from_slice(&(data.len() as u32).to_le_bytes());
    f.extend_from_slice(&data);
    std::fs::write(path, f).unwrap();
}

/// Reads lines until one is the reply to `cmd`, keeping the observations seen on the way.
fn reply(lines: &mut impl Iterator<Item = Value>, cmd: &str, seen: &mut Vec<Value>) -> Value {
    for v in lines.by_ref() {
        if v["reply"] == cmd {
            return v;
        }
        seen.push(v);
    }
    panic!("no reply to {cmd}; saw {seen:?}");
}

#[test]
fn watch_answers_commands_on_stdin() {
    let dir = std::env::temp_dir().join(format!("ironav-watch-commands-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let src = dir.join("tone.wav");
    wav(&src, 30.0);
    let mut child = Command::new(env!("CARGO_BIN_EXE_ironav"))
        .args(["watch", &format!("{}?pace=realtime", src.display()), "-a", "levels", "--level", "events", "--commands"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines().map(|l| serde_json::from_str::<Value>(&l.unwrap()).unwrap());
    let mut seen = Vec::new();

    writeln!(stdin, "configure add=silence remove=levels digest_seconds=1").unwrap();
    let r = reply(&mut lines, "configure", &mut seen);
    assert_eq!(r["ok"], true, "{r}");
    assert_eq!(r["result"]["applied"], true, "{r}");

    // wait for the silence to end, then find it by query
    while !seen.iter().any(|o| o["kind"] == "silence_end") {
        seen.push(lines.next().expect("the watch ended early"));
    }
    writeln!(stdin, r#"{{"cmd":"query","kinds":["silence_end"],"id":7}}"#).unwrap();
    let r = reply(&mut lines, "query", &mut seen);
    assert_eq!(r["id"], 7, "{r}");
    assert_eq!(r["result"]["matched"], 1, "{r}");
    // a text value that looks like a number stays text
    writeln!(stdin, "query kinds=silence_end text=1").unwrap();
    let r = reply(&mut lines, "query", &mut seen);
    assert_eq!(r["result"]["matched"], 1, "{r}");

    writeln!(stdin, "rewind").unwrap();
    let r = reply(&mut lines, "rewind", &mut seen);
    assert_eq!(r["ok"], false, "{r}");
    assert_eq!(r["error"]["code"], "invalid-argument", "{r}");

    writeln!(stdin, "close").unwrap();
    let r = reply(&mut lines, "close", &mut seen);
    assert_eq!(r["ok"], true, "{r}");
    // then the closing envelope, long before the 30 s source ends
    let last = lines.next().expect("a closing envelope");
    assert_eq!(last["ok"], true, "{last}");
    assert_eq!(last["result"]["analyzed"][0]["analyzers"][0], "silence", "{last}");
    assert!(child.wait().unwrap().success());
    let _ = std::fs::remove_dir_all(&dir);
}
