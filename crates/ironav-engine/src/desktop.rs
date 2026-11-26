//! The opt-in loopback connection between a desktop player and an MCP process.
use crate::policy::Policy;
use ironav_core::{Error, ErrorCode, Result};
use ironav_player::{Player, Waker};
use serde::Deserialize;
use serde_json::{json, Value};
use std::io::{self, Read, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::{Duration, Instant};

pub const SESSION: &str = "desktop";
pub const CONNECTION_ENV: &str = "IRONAV_GUI_ATTACH";
pub const COMMANDS: &[&str] = &["status", "play", "pause", "toggle", "seek", "volume", "mute", "loop", "snapshot"];
pub const SNAPSHOT_SIDE: u32 = 640;
const KEY_LEN: usize = 32;
const REQUEST_LIMIT: usize = 16 << 10;
const REPLY_LIMIT: usize = 2 << 20;
const REQUEST_TIME: Duration = Duration::from_secs(2);
const REPLY_TIME: Duration = Duration::from_secs(8);

pub fn attached_tool(name: &str) -> bool {
    matches!(name, "play_status" | "play_control")
}

#[derive(Clone)]
pub struct Client {
    port: u16,
    key: [u8; KEY_LEN],
}

impl Client {
    pub fn parse(value: &str) -> Result<Self> {
        let bad = || Error::invalid_arg("invalid desktop connection value").hint("copy the MCP configuration from the player's Agent view");
        let (port, key) = value.split_once(':').ok_or_else(bad)?;
        let port = port.parse::<u16>().ok().filter(|p| *p != 0).ok_or_else(bad)?;
        let key = <[u8; KEY_LEN]>::try_from(key.as_bytes()).ok().filter(|k| k.iter().all(u8::is_ascii_hexdigit)).ok_or_else(bad)?;
        Ok(Self { port, key })
    }

    pub fn call(&self, name: &str, args: &Value) -> Result<Value> {
        if !attached_tool(name) {
            return Err(unknown_tool());
        }
        self.request(name, args)
            .map_err(|e| e.hint("enable MCP in the player's Agent view and use its current configuration"))?
    }

    fn request(&self, name: &str, args: &Value) -> Result<Result<Value>> {
        let addr = SocketAddr::from((Ipv4Addr::LOCALHOST, self.port));
        let mut stream = TcpStream::connect_timeout(&addr, REQUEST_TIME)?;
        let deadline = Instant::now() + REPLY_TIME;
        stream.set_nodelay(true)?;
        stream.set_write_timeout(Some(REQUEST_TIME))?;
        stream.write_all(&self.key)?;
        let mut accepted = [0];
        read_exact(&mut stream, &mut accepted, deadline)?;
        if accepted != [1] {
            return Err(Error::new(ErrorCode::PermissionDenied, "the desktop connection key was refused"));
        }
        let request = serde_json::to_vec(&json!({ "name": name, "args": args }))?;
        write_message(&mut stream, &request, REQUEST_LIMIT)?;
        let reply = read_message(&mut stream, REPLY_LIMIT, deadline)?;
        Ok(serde_json::from_slice(&reply)?)
    }
}

struct Shared {
    player: Mutex<Option<Weak<Player>>>,
    available: AtomicBool,
    stopped: AtomicBool,
    policy: Policy,
    wake: Waker,
}

/// Owns the listener for one window. Dropping it revokes the connection key.
pub struct Link {
    client: Client,
    shared: Arc<Shared>,
    worker: thread::Thread,
}

impl Link {
    pub fn start(wake: Waker) -> Result<Self> {
        let bytes: [u8; KEY_LEN / 2] = ic_drbg::Rng::from_os().and_then(|mut rng| rng.random_array())
            .map_err(|_| Error::new(ErrorCode::Internal, "could not generate a desktop connection key"))?;
        let mut key = [0; KEY_LEN];
        for (chunk, byte) in key.as_chunks_mut::<2>().0.iter_mut().zip(bytes) {
            const HEX: &[u8; 16] = b"0123456789abcdef";
            *chunk = [HEX[(byte >> 4) as usize], HEX[(byte & 15) as usize]];
        }
        let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0))?;
        listener.set_nonblocking(true)?;
        let client = Client { port: listener.local_addr()?.port(), key };
        // The operator shares media selected in the window. Explicit file roots set in the
        // desktop process still apply.
        let shared = Arc::new(Shared { player: Mutex::new(None), available: AtomicBool::new(false), stopped: AtomicBool::new(false), policy: Policy::from_env(false), wake });
        let state = shared.clone();
        let worker = thread::Builder::new().name("ironav-desktop-link".into()).spawn(move || {
            while !state.stopped.load(Ordering::Acquire) {
                match listener.accept() {
                    Ok((stream, _)) => { let _ = respond(stream, &key, &state); }
                    Err(e) if e.kind() == io::ErrorKind::WouldBlock => thread::park_timeout(Duration::from_millis(25)),
                    Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
                    Err(_) => break,
                }
            }
        })?;
        Ok(Self { client, shared, worker: worker.thread().clone() })
    }

    pub fn set_player(&self, player: Option<&Arc<Player>>) {
        let mut slot = self.shared.player.lock().unwrap_or_else(|e| e.into_inner());
        *slot = player.map(Arc::downgrade);
        self.shared.available.store(player.is_some(), Ordering::Release);
    }

    /// Stop admitting calls for this player. Run the returned closure on the media worker to
    /// wait for any outstanding command to release its reference to the decoder.
    pub fn detach_player(&self) -> impl FnOnce() + Send + 'static {
        self.shared.available.store(false, Ordering::Release);
        let shared = self.shared.clone();
        move || drop(shared.player.lock().unwrap_or_else(|e| e.into_inner()))
    }

    pub fn connection_value(&self) -> String {
        format!("{}:{}", self.client.port, String::from_utf8_lossy(&self.client.key))
    }
}

impl Drop for Link {
    fn drop(&mut self) {
        self.shared.stopped.store(true, Ordering::Release);
        self.worker.unpark();
    }
}

#[derive(Deserialize)]
struct Request {
    name: String,
    args: Value,
}

fn respond(mut stream: TcpStream, key: &[u8; KEY_LEN], state: &Shared) -> Result<()> {
    stream.set_nonblocking(false)?;
    stream.set_nodelay(true)?;
    stream.set_write_timeout(Some(REQUEST_TIME))?;
    let deadline = Instant::now() + REQUEST_TIME;
    let mut sent = [0; KEY_LEN];
    read_exact(&mut stream, &mut sent, deadline)?;
    let accepted = key.iter().zip(sent).fold(0, |diff, (a, b)| diff | (a ^ b)) == 0;
    stream.write_all(&[u8::from(accepted)])?;
    if accepted {
        let data = read_message(&mut stream, REQUEST_LIMIT, deadline)?;
        let reply = serde_json::from_slice(&data).map_err(Error::from).and_then(|request| dispatch(state, request));
        write_message(&mut stream, &serde_json::to_vec(&reply)?, REPLY_LIMIT)?;
    }
    Ok(())
}

fn unknown_tool() -> Error {
    Error::invalid_arg("the desktop connection offers playback status and control").alternatives(["play_status", "play_control"])
}

fn dispatch(state: &Shared, mut request: Request) -> Result<Value> {
    if !attached_tool(&request.name) {
        return Err(unknown_tool());
    }
    let args = request.args.as_object_mut().ok_or_else(|| Error::invalid_arg("tool arguments must be an object"))?;
    if args.remove("session").is_some_and(|id| id != SESSION) {
        return Err(Error::new(ErrorCode::NotFound, "no such playback session in this window").alternatives([SESSION]));
    }
    // Serialize attach calls with changing the video in the window. The player clone is
    // dropped before the lock so the desktop can release its decoder before opening a new one.
    let player = state.player.lock().unwrap_or_else(|e| e.into_inner());
    if state.stopped.load(Ordering::Acquire) {
        return Err(Error::new(ErrorCode::NotFound, "the desktop connection was disabled"));
    }
    let p = player.as_ref().filter(|_| state.available.load(Ordering::Acquire)).and_then(Weak::upgrade)
        .ok_or_else(|| Error::new(ErrorCode::NotFound, "the desktop player has no open media").hint("open a media file in the desktop player"))?;
    state.policy.read(&p.info().path)?;
    let mut status = if request.name == "play_status" {
        p.status()
    } else {
        let cmd = args.get("cmd").and_then(Value::as_str).unwrap_or_default();
        if !COMMANDS.contains(&cmd) {
            return Err(Error::invalid_arg("command unavailable on the desktop connection").alternatives(COMMANDS.iter().copied()));
        }
        if cmd == "snapshot" {
            if args.contains_key("path") {
                return Err(Error::new(ErrorCode::PermissionDenied, "desktop snapshots are returned inline"));
            }
            for dim in ["max_width", "max_height"] {
                let size = match args.get(dim) {
                    None => u64::from(SNAPSHOT_SIDE),
                    Some(v) => v.as_u64().filter(|&d| d >= 2).ok_or_else(|| Error::invalid_arg(format!("{dim} must be an integer of at least 2")))?,
                };
                args.insert(dim.into(), json!(size.min(u64::from(SNAPSHOT_SIDE))));
            }
        }
        let status = p.command(&request.args)?;
        (state.wake)();
        status
    };
    status["session"] = json!(SESSION);
    Ok(status)
}

fn read_exact(stream: &mut TcpStream, mut buf: &mut [u8], deadline: Instant) -> io::Result<()> {
    while !buf.is_empty() {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "the desktop connection timed out"));
        }
        stream.set_read_timeout(Some(left))?;
        match stream.read(buf) {
            Ok(0) => return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "the desktop connection closed")),
            Ok(n) => buf = &mut buf[n..],
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

fn read_message(stream: &mut TcpStream, limit: usize, deadline: Instant) -> io::Result<Vec<u8>> {
    let mut length = [0; 4];
    read_exact(stream, &mut length, deadline)?;
    let length = u32::from_be_bytes(length) as usize;
    if length > limit {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "desktop connection message exceeds its size limit"));
    }
    let mut body = vec![0; length];
    read_exact(stream, &mut body, deadline)?;
    Ok(body)
}

fn write_message(stream: &mut TcpStream, body: &[u8], limit: usize) -> io::Result<()> {
    if body.len() > limit {
        return Err(io::Error::new(io::ErrorKind::InvalidData, "desktop connection message exceeds its size limit"));
    }
    stream.write_all(&(body.len() as u32).to_be_bytes())?;
    stream.write_all(body)
}
