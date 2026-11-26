//! Exercise the stdio adapter against the same player the desktop can share.

use ironav_core::ErrorCode;
use ironav_engine::desktop::{Client, Link, CONNECTION_ENV};
use ironav_player::{AudioMode, OpenOptions, Player, State};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

struct Mcp {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
    id: u64,
}

impl Mcp {
    fn connect(link: &Link) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_ironav"))
            .args(["mcp", "--attach"]).env(CONNECTION_ENV, link.connection_value())
            .stdin(Stdio::piped()).stdout(Stdio::piped()).spawn().unwrap();
        let input = child.stdin.take().unwrap();
        let output = BufReader::new(child.stdout.take().unwrap());
        Self { child, input, output, id: 0 }
    }

    fn request(&mut self, method: &str, params: Value) -> Value {
        self.id += 1;
        writeln!(self.input, "{}", json!({ "jsonrpc": "2.0", "id": self.id, "method": method, "params": params })).unwrap();
        let mut line = String::new();
        self.output.read_line(&mut line).unwrap();
        let reply: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(reply["id"], self.id);
        reply
    }

    fn call(&mut self, name: &str, args: Value) -> Value {
        self.request("tools/call", json!({ "name": name, "arguments": args }))["result"].clone()
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn player(path: &Path, y: u8) -> Arc<Player> {
    let mut media = b"YUV4MPEG2 W64 H48 F10:1 Ip A1:1 C420jpeg\n".to_vec();
    for _ in 0..20 {
        media.extend(b"FRAME\n");
        media.extend(vec![y; 64 * 48]);
        media.extend(vec![128; 64 * 48 / 2]);
    }
    std::fs::write(path, media).unwrap();
    Arc::new(Player::open(path.to_str().unwrap(), &OpenOptions { audio: AudioMode::Off, autoplay: false, hwaccel: "off".into(), ..Default::default() }).unwrap())
}

fn wait_for_picture(player: &Player) {
    let started = Instant::now();
    while player.state() != State::Paused || player.frame().is_none() {
        assert!(started.elapsed() < Duration::from_secs(5), "paused picture did not become ready");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn attached_tools_follow_the_window_player_and_are_revoked_with_the_link() {
    let wakes = Arc::new(AtomicUsize::new(0));
    let wake = wakes.clone();
    let link = Link::start(Arc::new(move || { wake.fetch_add(1, Ordering::Relaxed); })).unwrap();
    let value = link.connection_value();
    let (port, key) = value.split_once(':').unwrap();
    let wrong = if key.starts_with('a') { 'b' } else { 'a' };
    let client = Client::parse(&format!("{port}:{wrong}{}", &key[1..])).unwrap();
    assert_eq!(client.call("play_status", &json!({})).unwrap_err().code, ErrorCode::PermissionDenied);

    let mut mcp = Mcp::connect(&link);
    let init = mcp.request("initialize", json!({ "protocolVersion": "2025-06-18" }));
    assert_eq!(init["result"]["serverInfo"]["name"], "ironav-desktop");
    assert!(init["result"]["capabilities"].get("resources").is_none());
    let list = mcp.request("tools/list", json!({}));
    let listed = list["result"]["tools"].as_array().unwrap();
    assert_eq!(listed.iter().map(|t| t["name"].as_str().unwrap()).collect::<Vec<_>>(), ["play_control", "play_status"]);
    assert!(listed[0]["inputSchema"]["properties"].get("path").is_none());
    assert_eq!(mcp.call("play_open", json!({}))["isError"], true);
    assert_eq!(mcp.call("play_status", json!({}))["isError"], true);

    let dir = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("desktop-mcp-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let first = player(&dir.join("first.y4m"), 32);
    let second_path = dir.join("second.y4m");
    link.set_player(Some(&first));
    wait_for_picture(&first);
    let seek = mcp.call("play_control", json!({ "cmd": "seek", "to": 0.75 }));
    assert_eq!(seek["structuredContent"]["session"], "desktop");
    assert_eq!(first.position(), 0.75);
    assert!(wakes.load(Ordering::Relaxed) > 0);
    first.set_volume(0.4);
    let status = mcp.call("play_status", json!({ "session": "desktop" }));
    assert_eq!(status["structuredContent"]["volume"], json!(0.4f32));
    assert_eq!(mcp.call("play_status", json!({ "session": "other" }))["isError"], true);
    wait_for_picture(&first);

    let snap = mcp.call("play_control", json!({ "cmd": "snapshot", "max_width": 32 }));
    assert_eq!(snap["structuredContent"]["snapshot"]["width"], 32);
    assert_eq!(snap["content"][1]["mimeType"], "image/png");
    assert!(snap["content"][1]["data"].as_str().unwrap().starts_with("iVBORw0K"));
    let output = dir.join("denied.png");
    assert_eq!(mcp.call("play_control", json!({ "cmd": "snapshot", "path": output }))["isError"], true);
    assert!(!output.exists());

    let finish_detach = link.detach_player();
    assert_eq!(mcp.call("play_status", json!({}))["isError"], true);
    drop(first);
    let second = player(&second_path, 128);
    link.set_player(Some(&second));
    finish_detach();
    assert_eq!(mcp.call("play_status", json!({}))["structuredContent"]["path"], json!(second_path));
    drop(link);
    assert_eq!(mcp.call("play_status", json!({}))["isError"], true);
}
