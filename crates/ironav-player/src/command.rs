//! The JSON command interface: one function an agent (or a script on stdin) drives the player
//! with. Every command returns the player's status afterwards, so one call both acts and shows
//! the effect.

use crate::{base64, rgb_frame, to_png, Player, State};
use ironav_core::{Error, ErrorCode, Result};
use serde_json::{json, Value};

pub const COMMANDS: &[(&str, &str)] = &[
    ("status", "report the playback state, position, streams and counters"),
    ("play", "start or resume playback (restarts from the beginning if it had ended)"),
    ("pause", "pause playback, keeping the position"),
    ("toggle", "switch between playing and paused"),
    ("seek", "jump to a position: {\"to\": seconds} or {\"by\": seconds} or {\"fraction\": 0..1}"),
    ("volume", "set the volume: {\"value\": 0..2}"),
    ("mute", "mute or unmute: {\"on\": bool} (toggles without it)"),
    ("loop", "repeat from the start at the end: {\"on\": bool}"),
    ("snapshot", "the current picture as a PNG: {\"max_width\",\"max_height\",\"path\"}; inline base64 without a path"),
    ("wait", "block until a position or state is reached: {\"position\": s} or {\"state\": \"ended\"}, {\"timeout\": s}"),
    ("help", "list these commands"),
];

fn num(v: &Value, key: &str) -> Result<Option<f64>> {
    match v.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(x) => x.as_f64().filter(|n| n.is_finite()).map(Some).ok_or_else(|| Error::invalid_arg(format!("'{key}' must be a number"))),
    }
}

impl Player {
    /// Everything an agent needs to know about the player right now.
    pub fn status(&self) -> Value {
        let state = self.state();
        let pos = self.position();
        let info = self.info();
        let (volume, muted, looping) = {
            let c = self.inner.clock.lock().unwrap();
            (c.volume, c.muted, c.looping)
        };
        let q = self.inner.video.lock().unwrap();
        let (shown, skipped, queued) = (q.shown, q.skipped, q.frames.len());
        drop(q);
        let audio = info.audio.as_ref().map(|a| {
            let out = self.inner.out.as_ref();
            json!({
                "codec": a.codec, "sample_rate": a.sample_rate, "channels": a.channels, "device": a.device,
                "level": out.map_or(0.0, |o| o.level()),
                "queued_ms": out.map_or(0, |o| (o.queued_frames() as f64 * 1000.0 / o.rate() as f64) as u64),
            })
        });
        let video = info.video.as_ref().map(|v| {
            json!({ "codec": v.codec, "width": v.width, "height": v.height, "fps": v.fps, "decoder": v.decoder,
                    "frames_shown": shown, "frames_skipped": skipped, "frames_queued": queued })
        });
        json!({
            "path": info.path, "format": info.format,
            "state": state, "position": round3(pos), "duration": info.duration,
            "volume": volume, "muted": muted, "looping": looping,
            "audio": audio, "video": video,
            "warnings": info.warnings,
            "error": self.error().map(|e| json!({ "code": e.code, "message": e.message, "hint": e.hint })),
        })
    }

    /// PNG of the picture now on screen, scaled down to fit the limits.
    pub fn snapshot_png(&self, max_w: Option<u32>, max_h: Option<u32>) -> Result<(u32, u32, Vec<u8>)> {
        if !self.has_video() {
            return Err(Error::new(ErrorCode::InvalidArgument, "this file has no video to take a picture of"));
        }
        let f = self.frame().ok_or_else(|| Error::new(ErrorCode::NotFound, "no picture is available yet").hint("wait for the state to leave \"buffering\""))?;
        let (w, h, rgb) = rgb_frame(&f.frame, max_w, max_h);
        Ok((w, h, to_png(w, h, &rgb)?))
    }

    /// Run one command, `{"cmd": "seek", "to": 12.5}`. Returns the resulting status, plus any
    /// command-specific fields.
    pub fn command(&self, req: &Value) -> Result<Value> {
        let cmd = req.get("cmd").and_then(Value::as_str).ok_or_else(|| Error::invalid_arg("missing \"cmd\"").hint("send {\"cmd\":\"help\"} for the list"))?;
        let mut extra = json!({});
        match cmd {
            "status" => {}
            "help" => extra = json!({ "commands": COMMANDS.iter().map(|(n, d)| json!({ "cmd": n, "does": d })).collect::<Vec<_>>() }),
            "play" => self.play(),
            "pause" => self.pause(),
            "toggle" => self.toggle(),
            "seek" => {
                let dur = self.info().duration;
                match (num(req, "to")?, num(req, "by")?, num(req, "fraction")?) {
                    (Some(t), None, None) => self.seek(t),
                    (None, Some(d), None) => self.seek_by(d),
                    (None, None, Some(f)) => {
                        let d = dur.ok_or_else(|| Error::invalid_arg("the duration is unknown, so a fraction cannot be used").hint("use \"to\" or \"by\""))?;
                        self.seek(f.clamp(0.0, 1.0) * d)
                    }
                    _ => return Err(Error::invalid_arg("give exactly one of \"to\", \"by\" or \"fraction\"")),
                }
            }
            "volume" => {
                let v = num(req, "value")?.ok_or_else(|| Error::invalid_arg("missing \"value\""))?;
                if !(0.0..=2.0).contains(&v) {
                    return Err(Error::invalid_arg("volume must be between 0 and 2").hint("1 is the original level"));
                }
                self.set_volume(v as f32);
            }
            "mute" => {
                let on = req.get("on").and_then(Value::as_bool).unwrap_or_else(|| !self.inner.clock.lock().unwrap().muted);
                self.set_muted(on);
            }
            "loop" => {
                let on = req.get("on").and_then(Value::as_bool).unwrap_or_else(|| !self.inner.clock.lock().unwrap().looping);
                self.set_looping(on);
            }
            "snapshot" => {
                let dim = |k: &str| num(req, k).map(|o| o.map(|v| v.max(2.0) as u32));
                let (w, h, png) = self.snapshot_png(dim("max_width")?, dim("max_height")?)?;
                extra = json!({ "snapshot": { "width": w, "height": h, "bytes": png.len() } });
                if let Some(path) = req.get("path").and_then(Value::as_str) {
                    std::fs::write(path, &png).map_err(|e| Error::new(ErrorCode::Io, format!("cannot write '{path}': {e}")))?;
                    extra["snapshot"]["path"] = json!(path);
                } else {
                    extra["snapshot"]["png_base64"] = json!(base64(&png));
                }
            }
            "wait" => {
                let timeout = num(req, "timeout")?.unwrap_or(30.0).clamp(0.0, 3600.0);
                let (pos, want_end) = (num(req, "position")?, req.get("state").and_then(Value::as_str));
                if pos.is_none() && want_end.is_none() {
                    return Err(Error::invalid_arg("give \"position\" or \"state\""));
                }
                let t0 = std::time::Instant::now();
                let reached = loop {
                    let st = self.state();
                    if pos.is_some_and(|p| self.position() >= p) || want_end.is_some_and(|s| format!("{st:?}").eq_ignore_ascii_case(s)) || st == State::Error {
                        break true;
                    }
                    if st == State::Ended || t0.elapsed().as_secs_f64() >= timeout {
                        break st == State::Ended && pos.is_none();
                    }
                    self.frame(); // keep the picture queue draining while we wait
                    std::thread::sleep(std::time::Duration::from_millis(10));
                };
                extra = json!({ "reached": reached, "waited": round3(t0.elapsed().as_secs_f64()) });
            }
            other => {
                return Err(Error::new(ErrorCode::InvalidArgument, format!("unknown command '{other}'"))
                    .hint("send {\"cmd\":\"help\"} for the list")
                    .alternatives(COMMANDS.iter().map(|(n, _)| *n)));
            }
        }
        let mut s = self.status();
        if let (Some(o), Some(e)) = (s.as_object_mut(), extra.as_object()) {
            o.extend(e.clone());
        }
        Ok(s)
    }
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}
