//! Playback sessions for the agent tools. A session is a [`Player`] that stays open between
//! calls, so an agent can open a file, seek around, look at pictures and check the meters.
//! Sessions live as long as the process does (an MCP server, for instance).

use ironav_core::{Error, ErrorCode, Result};
use ironav_player::{OpenOptions, Player};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

const MAX_SESSIONS: usize = 8;

struct Registry {
    next: u64,
    players: HashMap<String, Arc<Player>>,
}

fn registry() -> &'static Mutex<Registry> {
    static R: OnceLock<Mutex<Registry>> = OnceLock::new();
    R.get_or_init(|| Mutex::new(Registry { next: 1, players: HashMap::new() }))
}

fn get(args: &Value) -> Result<Arc<Player>> {
    let id = args.get("session").and_then(Value::as_str).ok_or_else(|| Error::invalid_arg("missing \"session\"").hint("call play_open first; it returns the session id"))?;
    let r = registry().lock().unwrap();
    r.players.get(id).cloned().ok_or_else(|| {
        Error::new(ErrorCode::NotFound, format!("no playback session '{id}'")).hint("it may have been closed; play_open starts a new one").alternatives(r.players.keys().cloned())
    })
}

pub fn open(args: &Value) -> Result<Value> {
    let path = args.get("path").and_then(Value::as_str).ok_or_else(|| Error::invalid_arg("missing \"path\""))?;
    let mut opts = OpenOptions::default();
    // Reuse the options' own serde shape so the schema and the behaviour cannot drift.
    let mut o = serde_json::Map::new();
    for k in ["audio", "hwaccel", "start", "autoplay", "volume"] {
        if let Some(v) = args.get(k) {
            o.insert(k.into(), v.clone());
        }
    }
    if let Some(v) = args.get("loop") {
        o.insert("looping".into(), v.clone());
    }
    if !o.is_empty() {
        opts = serde_json::from_value(Value::Object(o)).map_err(|e| Error::invalid_arg(format!("invalid play option: {e}")))?;
    }
    {
        let r = registry().lock().unwrap();
        if r.players.len() >= MAX_SESSIONS {
            return Err(Error::new(ErrorCode::InvalidArgument, format!("{MAX_SESSIONS} playback sessions are already open"))
                .hint("close one with play_close")
                .alternatives(r.players.keys().cloned()));
        }
    }
    let player = Arc::new(Player::open(path, &opts)?);
    let mut r = registry().lock().unwrap();
    let id = format!("p{}", r.next);
    r.next += 1;
    let mut status = player.status();
    status["session"] = json!(id);
    r.players.insert(id, player);
    Ok(status)
}

pub fn control(args: &Value) -> Result<Value> {
    let p = get(args)?;
    let mut req = args.clone();
    if let Some(o) = req.as_object_mut() {
        o.remove("session");
    }
    let mut v = p.command(&req)?;
    v["session"] = args["session"].clone();
    Ok(v)
}

pub fn status(args: &Value) -> Result<Value> {
    let p = get(args)?;
    let mut v = p.status();
    v["session"] = args["session"].clone();
    Ok(v)
}

pub fn close(args: &Value) -> Result<Value> {
    get(args)?; // gives the not-found error with alternatives
    let id = args["session"].as_str().unwrap_or_default().to_string();
    let p = registry().lock().unwrap().players.remove(&id);
    drop(p); // joins the decode threads
    Ok(json!({ "closed": id }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_open_control_and_close() {
        let p = std::env::temp_dir().join(format!("ironav-sess-{}.y4m", std::process::id()));
        let mut f = b"YUV4MPEG2 W32 H32 F10:1 Ip C420jpeg\n".to_vec();
        for _ in 0..20 {
            f.extend(b"FRAME\n");
            f.extend(vec![100u8; 32 * 32 + 2 * 16 * 16]);
        }
        std::fs::write(&p, f).unwrap();
        let s = open(&json!({ "path": p.to_str().unwrap(), "audio": "silent", "autoplay": false })).unwrap();
        let id = s["session"].as_str().unwrap().to_string();
        let r = control(&json!({ "session": id, "cmd": "seek", "fraction": 0.5 })).unwrap();
        assert_eq!(r["position"], 1.0);
        assert!(status(&json!({ "session": id })).unwrap()["state"].as_str().is_some());
        close(&json!({ "session": id })).unwrap();
        let e = status(&json!({ "session": id })).unwrap_err();
        assert_eq!(e.code, ErrorCode::NotFound);
        let e = open(&json!({ "path": "x", "audio": "loud" })).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
    }
}
