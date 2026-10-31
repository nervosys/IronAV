//! An example sidecar (see `ironav_engine::sidecar`), for testing the protocol and as a model to
//! copy. Modes:
//!
//! - `dtmf`: telephone keypad tones in 8 kHz audio, reported as `dtmf` spans with the digit.
//! - `echo`: reports each speech-gated audio chunk it is sent (`chunk`, with its length and level).
//! - `pictures`: reports the mean level of each picture it is sent (`picture`).
//! - `crash N MARKER`: like `echo` without the gate, but exits after N chunks unless MARKER exists
//!   (which it then creates), so a restart behaves.
//! - `mute`: exits before its hello, complaining on stderr.
//! - `slow SECONDS PIDFILE`: writes its process id to PIDFILE and waits SECONDS before its hello,
//!   then behaves as `echo`.

use serde_json::{json, Value};
use std::io::{BufRead, Write};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mode = args.first().map(String::as_str).unwrap_or("");
    let hello = match mode {
        "dtmf" => json!({ "audio": { "rate": 8000, "format": "s16", "chunk_seconds": 0.5 } }),
        "slow" => {
            let _ = std::fs::write(&args[2], std::process::id().to_string());
            std::thread::sleep(std::time::Duration::from_secs_f64(args[1].parse().unwrap_or(1.0)));
            json!({ "audio": { "rate": 16000, "format": "s16", "chunk_seconds": 1.0, "gate": "speech" } })
        }
        "echo" => json!({ "audio": { "rate": 16000, "format": "s16", "chunk_seconds": 1.0, "gate": "speech" } }),
        "crash" => json!({ "audio": { "rate": 16000, "format": "f32", "chunk_seconds": 0.5 } }),
        "pictures" => json!({ "video": { "every_seconds": 0.5, "width": 32, "format": "rgb24" } }),
        "mute" => {
            eprintln!("no model here");
            std::process::exit(2);
        }
        _ => {
            eprintln!("usage: ironav-sidecar-example dtmf|echo|pictures|crash N MARKER|mute|slow SECONDS PIDFILE");
            std::process::exit(2);
        }
    };
    let mut crash_after = None;
    if mode == "crash" {
        let marker = std::path::Path::new(&args[2]);
        if !marker.exists() {
            let _ = std::fs::write(marker, b"");
            crash_after = args[1].parse::<u64>().ok();
        }
    }
    let out = std::io::stdout();
    let mut out = out.lock();
    let mut say = |v: Value| {
        let _ = writeln!(out, "{v}");
        let _ = out.flush();
    };
    say(json!({ "op": "hello", "protocol": 1, "wants": hello }));
    let mut dtmf = Dtmf::default();
    let mut chunks = 0u64;
    for line in std::io::stdin().lock().lines() {
        let Ok(line) = line else { break };
        let Ok(m) = serde_json::from_str::<Value>(&line) else { continue };
        let t = m["t"].as_f64().unwrap_or(0.0);
        match m["op"].as_str() {
            Some("audio") => {
                let bytes = unbase64(m["data"].as_str().unwrap_or(""));
                let samples: Vec<f64> = if m["format"] == "f32" {
                    bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]) as f64).collect()
                } else {
                    bytes.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes([b[0], b[1]]) as f64 / 32768.0).collect()
                };
                let dur = m["dur"].as_f64().unwrap_or(0.0);
                chunks += 1;
                if crash_after.is_some_and(|n| chunks > n) {
                    eprintln!("crashing on purpose after {} chunks", chunks - 1);
                    std::process::exit(3);
                }
                if mode == "dtmf" {
                    for (kind, t0, t1, digit) in dtmf.feed(t, &samples) {
                        say(json!({ "op": "observe", "kind": kind, "t": t0, "t_end": t1, "data": { "digit": digit.to_string() } }));
                    }
                } else {
                    let rms = (samples.iter().map(|s| s * s).sum::<f64>() / samples.len().max(1) as f64).sqrt();
                    say(json!({ "op": "observe", "kind": "chunk", "t": t, "t_end": t + dur,
                                "data": { "samples": samples.len(), "final": m["final"], "rms": rms, "text": format!("chunk{chunks}") } }));
                }
            }
            Some("video") => {
                let px = unbase64(m["data"].as_str().unwrap_or(""));
                let mean = px.iter().map(|&b| b as f64).sum::<f64>() / px.len().max(1) as f64;
                say(json!({ "op": "observe", "kind": "picture", "t": t, "data": { "width": m["width"], "height": m["height"], "mean": mean.round() } }));
            }
            Some("end") => {
                if let Some((t0, t1, digit)) = dtmf.close() {
                    say(json!({ "op": "observe", "kind": "dtmf", "t": t0, "t_end": t1, "data": { "digit": digit.to_string() } }));
                }
                break;
            }
            _ => {}
        }
    }
}

fn unbase64(s: &str) -> Vec<u8> {
    let val = |c: u8| match c {
        b'A'..=b'Z' => Some(c - b'A'),
        b'a'..=b'z' => Some(c - b'a' + 26),
        b'0'..=b'9' => Some(c - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
    };
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0);
    for v in s.bytes().filter_map(val) {
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

/// DTMF by the Goertzel algorithm over 40 ms blocks of 8 kHz audio.
#[derive(Default)]
struct Dtmf {
    pending: Vec<f64>,
    /// Time of `pending[0]`.
    t: f64,
    /// The digit sounding, from when, and the end of its last block.
    on: Option<(char, f64, f64)>,
}

const LOW: [f64; 4] = [697.0, 770.0, 852.0, 941.0];
const HIGH: [f64; 4] = [1209.0, 1336.0, 1477.0, 1633.0];
const KEYS: [[char; 4]; 4] = [['1', '2', '3', 'A'], ['4', '5', '6', 'B'], ['7', '8', '9', 'C'], ['*', '0', '#', 'D']];
const BLOCK: usize = 320;
const RATE: f64 = 8000.0;

fn goertzel(x: &[f64], f: f64) -> f64 {
    let c = 2.0 * (2.0 * std::f64::consts::PI * f / RATE).cos();
    let (mut s1, mut s2) = (0.0, 0.0);
    for &v in x {
        let s = v + c * s1 - s2;
        s2 = s1;
        s1 = s;
    }
    (s1 * s1 + s2 * s2 - c * s1 * s2) / (x.len() * x.len()) as f64
}

impl Dtmf {
    fn feed(&mut self, t: f64, x: &[f64]) -> Vec<(&'static str, f64, f64, char)> {
        let mut found = Vec::new();
        if self.pending.is_empty() || (t - (self.t + self.pending.len() as f64 / RATE)).abs() > 0.01 {
            self.pending.clear();
            self.t = t;
        }
        self.pending.extend_from_slice(x);
        while self.pending.len() >= BLOCK {
            let b: Vec<f64> = self.pending.drain(..BLOCK).collect();
            let bt = self.t;
            self.t += BLOCK as f64 / RATE;
            let energy = b.iter().map(|v| v * v).sum::<f64>() / BLOCK as f64;
            let best = |fs: &[f64; 4]| fs.iter().map(|&f| goertzel(&b, f)).enumerate().fold((0, 0.0), |a, (i, p)| if p > a.1 { (i, p) } else { a });
            let (li, lp) = best(&LOW);
            let (hi, hp) = best(&HIGH);
            // each tone's power is a quarter of its squared amplitude; together they carry most of the energy
            let digit = (energy > 1e-5 && 2.0 * (lp + hp) > 0.6 * energy).then(|| KEYS[li][hi]);
            match (self.on, digit) {
                (Some((d, _, _)), Some(n)) if d == n => self.on.as_mut().unwrap().2 = self.t,
                (_, n) => {
                    if let Some((t0, t1, d)) = self.close() {
                        found.push(("dtmf", t0, t1, d));
                    }
                    self.on = n.map(|n| (n, bt, self.t));
                }
            }
        }
        found
    }

    /// The digit sounding, if it lasted long enough to count (40 ms).
    fn close(&mut self) -> Option<(f64, f64, char)> {
        let (d, t0, t1) = self.on.take()?;
        (t1 - t0 >= 0.039).then_some(((t0 * 1000.0).round() / 1000.0, (t1 * 1000.0).round() / 1000.0, d))
    }
}
