//! A server-side WebSocket (RFC 6455) over the HTTP server's connections: the upgrade request's
//! checks and its 101 response, then framing over the same stream, plain or TLS.
//!
//! A caller gets a [`Connection`] from `HttpReply::upgrade_websocket` (see [`crate::WhepOptions::fallback`]),
//! already past the handshake, and runs it on a thread of its own: [`Connection::recv`] for what
//! the client sends, `send_text` and `send_binary` for what it is sent.
//!
//! The input is untrusted, so the reader is strict. A client's frames must be masked, no
//! extension is negotiated (so the RSV bits must be zero), reserved opcodes and malformed control
//! frames are refused, and a text message must be UTF-8 once it is whole. A length field is
//! checked against the message limit before anything is allocated for it. Each refusal sends a
//! close frame with its code (1002, 1007 or 1009) and ends the connection. Frames the server
//! sends are never masked.
//!
//! `handshake` checks a request and gives the key to answer with; `upgrade_response` is the
//! 101. After sending it, `Responder::into_stream` gives the stream, and `Connection::new` runs
//! the framing over it.

use super::http::{response, Request, Slot};
use crate::http::base64;
use crate::sha1::sha1;
use crate::tls::unbase64;
use ironsocketlayer::stream::TlsStream;
use std::io::{ErrorKind, Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant};

/// The GUID the RFC has the accept value hashed with.
const GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";
/// The longest control-frame payload.
const MAX_CONTROL: usize = 125;
/// How much of a payload is read into memory at a time.
const CHUNK: usize = 8 << 10;

enum Inner {
    Plain(TcpStream),
    Tls(Box<TlsStream<TcpStream>>),
}

/// The stream of an upgraded connection, plain or TLS. It holds one of the server's places for
/// upgraded connections until it is dropped.
pub struct Stream {
    inner: Inner,
    _slot: Option<Slot>,
}

impl Stream {
    pub(crate) fn plain(s: TcpStream) -> Stream {
        Stream { inner: Inner::Plain(s), _slot: None }
    }

    pub(crate) fn tls(t: Box<TlsStream<TcpStream>>) -> Stream {
        Stream { inner: Inner::Tls(t), _slot: None }
    }

    /// Has this stream give back `slot` when it is dropped.
    pub(crate) fn holding(mut self, slot: Slot) -> Stream {
        self._slot = Some(slot);
        self
    }
}

impl Read for Stream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match &mut self.inner {
            Inner::Plain(s) => s.read(buf),
            Inner::Tls(t) => t.read(buf),
        }
    }
}

impl Write for Stream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match &mut self.inner {
            Inner::Plain(s) => s.write(buf),
            Inner::Tls(t) => t.write(buf),
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match &mut self.inner {
            Inner::Plain(s) => s.flush(),
            Inner::Tls(t) => t.flush(),
        }
    }
}

/// The `Sec-WebSocket-Accept` value for a `Sec-WebSocket-Key`.
pub(crate) fn accept_key(key: &str) -> String {
    let mut data = key.as_bytes().to_vec();
    data.extend_from_slice(GUID.as_bytes());
    base64(&sha1(&data))
}

/// Why an upgrade was refused. Unless `status` is 0 the refusal has been sent to the client
/// already, as an HTTP response with that status.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refused {
    /// The status answered with: 400, 403 (the `Origin` is not allowed), 405, 426, or 503 (the
    /// server has all the upgraded connections it takes). 0 if the connection failed before
    /// anything could be sent.
    pub status: u16,
    /// Why, as the client was told.
    pub why: &'static str,
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "WebSocket upgrade refused ({}): {}", self.status, self.why)
    }
}

impl std::error::Error for Refused {}

impl Refused {
    /// The refusal as an HTTP response.
    pub(crate) fn response(&self) -> Vec<u8> {
        let mut headers = vec![("Content-Type", "text/plain")];
        match self.status {
            426 => headers.push(("Sec-WebSocket-Version", "13")),
            503 => headers.push(("Retry-After", "5")),
            _ => {}
        }
        response(self.status, &headers, self.why.as_bytes())
    }
}

/// Whether a comma-separated header value has `token` (ignoring case).
fn has_token(value: &str, token: &str) -> bool {
    value.split(',').any(|t| t.trim().eq_ignore_ascii_case(token))
}

/// Checks an upgrade request, and gives the `Sec-WebSocket-Accept` value to answer it with.
/// Its `Origin` must be one of `allowed_origins` (compared ignoring case); with no `Origin`, or
/// none allowed, it is refused with 403.
pub(crate) fn handshake(req: &Request, allowed_origins: &[&str]) -> std::result::Result<String, Refused> {
    let no = |status, why| Err(Refused { status, why });
    if req.method != "GET" {
        return no(405, "a WebSocket upgrade is a GET");
    }
    if req.version != "HTTP/1.1" {
        return no(400, "a WebSocket upgrade needs HTTP/1.1");
    }
    if !req.header("upgrade").is_some_and(|v| has_token(v, "websocket")) || !req.header("connection").is_some_and(|v| has_token(v, "upgrade")) {
        return no(400, "not a WebSocket upgrade");
    }
    if req.header("sec-websocket-version") != Some("13") {
        return no(426, "only WebSocket version 13 is served");
    }
    let Some(key) = req.header("sec-websocket-key") else { return no(400, "no Sec-WebSocket-Key") };
    // 16 bytes, written the one canonical way
    if !unbase64(key).is_some_and(|d| d.len() == 16 && base64(&d) == key) {
        return no(400, "a malformed Sec-WebSocket-Key");
    }
    match req.header("origin") {
        Some(o) if allowed_origins.iter().any(|a| a.eq_ignore_ascii_case(o)) => {}
        _ => return no(403, "the Origin is not allowed"),
    }
    Ok(accept_key(key))
}

/// The 101 response that accepts an upgrade, given [`handshake`]'s result. Unlike the other
/// responses it keeps the connection open.
pub(crate) fn upgrade_response(accept: &str) -> Vec<u8> {
    format!("HTTP/1.1 101 Switching Protocols\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Accept: {accept}\r\n\r\n").into_bytes()
}

/// A message, whole.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// A text message, valid UTF-8.
    Text(String),
    /// A binary message.
    Binary(Vec<u8>),
}

/// Limits for a connection.
#[derive(Debug, Clone, Copy)]
#[non_exhaustive]
pub struct Config {
    /// The longest message accepted, over all its fragments. Longer is closed with 1009.
    pub max_message: usize,
    /// How long [`Connection::recv`] waits for the client to send anything.
    pub idle: Duration,
}

impl Default for Config {
    fn default() -> Config {
        Config { max_message: 64 << 10, idle: Duration::from_secs(60) }
    }
}

/// Why a connection gave no message.
#[derive(Debug)]
pub enum Error {
    /// The client broke the protocol; a close frame with this code has been sent.
    Protocol(u16, &'static str),
    /// Nothing came in [`Config::idle`]. If it was between frames the connection is usable still
    /// (a ping would test it); inside a frame it is dead.
    Idle,
    /// The connection is closed, or a write or read failed.
    Io(std::io::Error),
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Protocol(code, why) => write!(f, "WebSocket protocol error {code}: {why}"),
            Error::Idle => f.write_str("the WebSocket client sent nothing in time"),
            Error::Io(e) => write!(f, "WebSocket I/O error: {e}"),
        }
    }
}

impl std::error::Error for Error {}

/// What a connection's calls give.
pub type Result<T> = std::result::Result<T, Error>;

fn closed() -> Error {
    Error::Io(ErrorKind::NotConnected.into())
}

mod op {
    pub const CONT: u8 = 0;
    pub const TEXT: u8 = 1;
    pub const BINARY: u8 = 2;
    pub const CLOSE: u8 = 8;
    pub const PING: u8 = 9;
    pub const PONG: u8 = 10;
}

/// A WebSocket over `S`, after the 101 has been sent. Messages are whole: the reader joins
/// fragments and answers pings itself.
pub struct Connection<S> {
    stream: S,
    config: Config,
    /// A message in pieces so far: its opcode and what has come.
    partial: Option<(u8, Vec<u8>)>,
    /// Set once a close frame has been sent: nothing more can be, and what comes is read only for
    /// the client's close.
    sent_close: bool,
    /// Set once a read or write has failed, or the client has broken the protocol.
    dead: bool,
    peer_close: Option<(u16, String)>,
}

/// Whether a close code may be on the wire (RFC 6455 Â§7.4).
fn valid_close_code(c: u16) -> bool {
    matches!(c, 1000..=1003 | 1007..=1011 | 3000..=4999)
}

impl<S: Read + Write> Connection<S> {
    /// A connection over `stream`, which is past the handshake. Its reads should give up every so
    /// often (`WouldBlock` or `TimedOut`), as a [`Stream`]'s do, so the idle limit can be kept.
    pub fn new(stream: S, config: Config) -> Connection<S> {
        Connection { stream, config, partial: None, sent_close: false, dead: false, peer_close: None }
    }

    /// The code and reason of the close frame the client sent, if one has come. A close with no
    /// code is 1005.
    pub fn peer_close(&self) -> Option<&(u16, String)> {
        self.peer_close.as_ref()
    }

    /// Whether nothing more can be sent.
    pub fn is_closed(&self) -> bool {
        self.sent_close || self.dead
    }

    /// The stream, ending the framing. Nothing is sent: close first if the client should be told.
    pub fn into_inner(self) -> S {
        self.stream
    }

    /// Sends a text message.
    pub fn send_text(&mut self, text: &str) -> Result<()> {
        self.send_frame(op::TEXT, text.as_bytes())
    }

    /// Sends a binary message.
    pub fn send_binary(&mut self, data: &[u8]) -> Result<()> {
        self.send_frame(op::BINARY, data)
    }

    /// Sends a ping with up to 125 bytes of payload.
    pub fn send_ping(&mut self, payload: &[u8]) -> Result<()> {
        if payload.len() > MAX_CONTROL {
            return Err(Error::Io(ErrorKind::InvalidInput.into()));
        }
        self.send_frame(op::PING, payload)
    }

    /// Sends a close frame; nothing more can be sent after it, but [`recv`](Self::recv) goes on
    /// to the client's reply. The reason is cut to fit (123 bytes). A code that may not be on the
    /// wire (1005, say) is sent as 1000.
    pub fn close(&mut self, code: u16, reason: &str) -> Result<()> {
        if self.is_closed() {
            return Ok(());
        }
        let code = if valid_close_code(code) { code } else { 1000 };
        let mut end = reason.len().min(MAX_CONTROL - 2);
        while !reason.is_char_boundary(end) {
            end -= 1;
        }
        let mut payload = code.to_be_bytes().to_vec();
        payload.extend_from_slice(&reason.as_bytes()[..end]);
        let r = self.send_frame(op::CLOSE, &payload);
        self.sent_close = true;
        r
    }

    fn send_frame(&mut self, opcode: u8, payload: &[u8]) -> Result<()> {
        if self.is_closed() {
            return Err(closed());
        }
        let mut f = Vec::with_capacity(payload.len() + 10);
        f.push(0x80 | opcode);
        match payload.len() {
            n if n < 126 => f.push(n as u8),
            n if n <= 0xffff => {
                f.push(126);
                f.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                f.push(127);
                f.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        f.extend_from_slice(payload);
        self.stream.write_all(&f).and_then(|()| self.stream.flush()).map_err(|e| {
            self.dead = true;
            Error::Io(e)
        })
    }

    /// Ends the connection for a protocol error, telling the client why.
    fn fail(&mut self, code: u16, why: &'static str) -> Error {
        let _ = self.close(code, why);
        self.dead = true;
        Error::Protocol(code, why)
    }

    /// Fills `buf`. `first` is whether nothing of a frame has been read yet: then running out of
    /// time leaves the connection as it was.
    fn read_exact(&mut self, buf: &mut [u8], first: bool) -> Result<()> {
        let mut deadline = Instant::now() + self.config.idle;
        let mut got = 0;
        while got < buf.len() {
            match self.stream.read(&mut buf[got..]) {
                Ok(0) => {
                    self.dead = true;
                    return Err(Error::Io(ErrorKind::UnexpectedEof.into()));
                }
                Ok(n) => {
                    got += n;
                    deadline = Instant::now() + self.config.idle;
                }
                Err(e) if matches!(e.kind(), ErrorKind::TimedOut | ErrorKind::WouldBlock | ErrorKind::Interrupted) => {
                    if Instant::now() >= deadline {
                        if !(first && got == 0) {
                            self.dead = true;
                        }
                        return Err(Error::Idle);
                    }
                }
                Err(e) => {
                    self.dead = true;
                    return Err(Error::Io(e));
                }
            }
        }
        Ok(())
    }

    /// The next message, with pings answered and pongs ignored on the way. `Ok(None)` once the
    /// client has closed (the close is answered, unless one was sent already).
    pub fn recv(&mut self) -> Result<Option<Message>> {
        loop {
            if self.peer_close.is_some() {
                return Ok(None);
            }
            if self.dead {
                return Err(closed());
            }
            let mut head = [0u8; 2];
            self.read_exact(&mut head[..1], true)?;
            self.read_exact(&mut head[1..], false)?;
            let (fin, rsv, opcode) = (head[0] & 0x80 != 0, head[0] & 0x70, head[0] & 0x0f);
            let (masked, len7) = (head[1] & 0x80 != 0, (head[1] & 0x7f) as usize);
            if rsv != 0 {
                return Err(self.fail(1002, "reserved bits set"));
            }
            if !matches!(opcode, op::CONT | op::TEXT | op::BINARY | op::CLOSE | op::PING | op::PONG) {
                return Err(self.fail(1002, "reserved opcode"));
            }
            let control = opcode >= op::CLOSE;
            if control && (!fin || len7 > MAX_CONTROL) {
                return Err(self.fail(1002, "malformed control frame"));
            }
            if !masked {
                return Err(self.fail(1002, "client frames must be masked"));
            }
            let len = match len7 {
                126 => {
                    let mut b = [0u8; 2];
                    self.read_exact(&mut b, false)?;
                    u16::from_be_bytes(b) as u64
                }
                127 => {
                    let mut b = [0u8; 8];
                    self.read_exact(&mut b, false)?;
                    let n = u64::from_be_bytes(b);
                    if n >> 63 != 0 {
                        return Err(self.fail(1002, "length with the top bit set"));
                    }
                    n
                }
                n => n as u64,
            };
            if !control {
                let have = self.partial.as_ref().map_or(0, |(_, d)| d.len()) as u64;
                if have.saturating_add(len) > self.config.max_message as u64 {
                    return Err(self.fail(1009, "message too big"));
                }
            }
            let mut mask = [0u8; 4];
            self.read_exact(&mut mask, false)?;
            // `len` is within the limit (or 125), and it is read in chunks, so a lying length
            // costs only what the client really sends
            let len = len as usize;
            let mut payload = Vec::with_capacity(len.min(CHUNK));
            while payload.len() < len {
                let at = payload.len();
                let n = (len - at).min(CHUNK);
                payload.resize(at + n, 0);
                self.read_exact(&mut payload[at..], false)?;
            }
            for (i, b) in payload.iter_mut().enumerate() {
                *b ^= mask[i % 4];
            }
            match opcode {
                op::PING => {
                    // after our own close nothing is sent
                    if !self.sent_close {
                        self.send_frame(op::PONG, &payload)?;
                    }
                }
                op::PONG => {}
                op::CLOSE => return self.on_close(&payload),
                _ => {
                    let mut data = match (opcode, self.partial.take()) {
                        (op::CONT, Some(p)) => p,
                        (op::CONT, None) => return Err(self.fail(1002, "a continuation with nothing to continue")),
                        (_, None) => (opcode, Vec::new()),
                        (_, Some(_)) => return Err(self.fail(1002, "a new message inside a fragmented one")),
                    };
                    data.1.extend_from_slice(&payload);
                    if !fin {
                        self.partial = Some(data);
                        continue;
                    }
                    if self.sent_close {
                        continue; // closing: only the client's close is of interest
                    }
                    if data.0 == op::TEXT {
                        return match String::from_utf8(data.1) {
                            Ok(s) => Ok(Some(Message::Text(s))),
                            Err(_) => Err(self.fail(1007, "text that is not UTF-8")),
                        };
                    }
                    return Ok(Some(Message::Binary(data.1)));
                }
            }
        }
    }

    fn on_close(&mut self, payload: &[u8]) -> Result<Option<Message>> {
        let parsed = match payload.len() {
            0 => Ok((1005, String::new())),
            1 => Err((1002, "a close frame with a one-byte payload")),
            _ => {
                let code = u16::from_be_bytes([payload[0], payload[1]]);
                match std::str::from_utf8(&payload[2..]) {
                    _ if !valid_close_code(code) => Err((1002, "a close code that is not allowed")),
                    Ok(reason) => Ok((code, reason.to_string())),
                    Err(_) => Err((1007, "a close reason that is not UTF-8")),
                }
            }
        };
        match parsed {
            Ok((code, reason)) => {
                // answer with the same code (none, when none was given)
                if !self.sent_close {
                    let echo = if code == 1005 { Vec::new() } else { code.to_be_bytes().to_vec() };
                    let _ = self.send_frame(op::CLOSE, &echo);
                    self.sent_close = true;
                }
                self.peer_close = Some((code, reason));
                Ok(None)
            }
            Err((code, why)) => Err(self.fail(code, why)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;
    use std::net::TcpListener;

    /// An in-memory stream: reads what was put in, then reports `WouldBlock` (or EOF when
    /// `eof`); keeps what is written.
    struct Mem {
        input: VecDeque<u8>,
        out: Vec<u8>,
        eof: bool,
    }

    impl Read for Mem {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            if self.input.is_empty() {
                return if self.eof { Ok(0) } else { Err(ErrorKind::WouldBlock.into()) };
            }
            let n = buf.len().min(self.input.len()).min(3); // short reads, to test reassembly
            for b in buf.iter_mut().take(n) {
                *b = self.input.pop_front().unwrap();
            }
            Ok(n)
        }
    }

    impl Write for Mem {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.out.extend_from_slice(buf);
            Ok(buf.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    const MASK: [u8; 4] = [0x37, 0xfa, 0x21, 0x3d];

    /// A client frame, masked.
    fn frame(first: u8, payload: &[u8]) -> Vec<u8> {
        let mut f = vec![first];
        match payload.len() {
            n if n < 126 => f.push(0x80 | n as u8),
            n if n <= 0xffff => {
                f.push(0x80 | 126);
                f.extend_from_slice(&(n as u16).to_be_bytes());
            }
            n => {
                f.push(0x80 | 127);
                f.extend_from_slice(&(n as u64).to_be_bytes());
            }
        }
        f.extend_from_slice(&MASK);
        f.extend(payload.iter().enumerate().map(|(i, b)| b ^ MASK[i % 4]));
        f
    }

    fn conn(input: Vec<u8>, eof: bool, max: usize) -> Connection<Mem> {
        let cfg = Config { max_message: max, idle: Duration::from_millis(30) };
        Connection::new(Mem { input: input.into(), out: Vec::new(), eof }, cfg)
    }

    fn cat(parts: &[Vec<u8>]) -> Vec<u8> {
        parts.concat()
    }

    /// The close code the server wrote last, if its output ends in a close frame.
    fn sent_close_code(c: &Connection<Mem>) -> Option<u16> {
        let o = &c.stream.out;
        let at = o.windows(2).rposition(|w| w[0] == 0x88 && w[1] >= 2)?;
        Some(u16::from_be_bytes([o[at + 2], o[at + 3]]))
    }

    fn request(headers: &[(&str, &str)]) -> Request {
        Request {
            method: "GET".into(),
            version: "HTTP/1.1".into(),
            path: "/ws".into(),
            query: String::new(),
            headers: headers.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect(),
            body: Vec::new(),
        }
    }

    fn good() -> Vec<(&'static str, &'static str)> {
        vec![
            ("Host", "x"),
            ("Upgrade", "websocket"),
            ("Connection", "Upgrade"),
            ("Sec-WebSocket-Key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("Sec-WebSocket-Version", "13"),
            ("Origin", "https://a.example"),
        ]
    }

    fn replaced(name: &str, value: Option<&str>) -> Request {
        let mut h: Vec<_> = good().into_iter().filter(|(k, _)| !k.eq_ignore_ascii_case(name)).collect();
        if let Some(v) = value {
            h.push((name, v));
        }
        request(&h)
    }

    const ORIGINS: &[&str] = &["https://a.example"];

    #[test]
    fn the_rfc_sample_handshake() {
        assert_eq!(accept_key("dGhlIHNhbXBsZSBub25jZQ=="), "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
        let accept = handshake(&request(&good()), ORIGINS).unwrap();
        assert_eq!(accept, "s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
        let r = String::from_utf8(upgrade_response(&accept)).unwrap();
        assert!(r.starts_with("HTTP/1.1 101 Switching Protocols\r\n") && r.ends_with("\r\n\r\n"));
        assert!(r.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n"));
        // token lists and case
        let r = request(&[
            ("upgrade", "h2c, WebSocket"),
            ("CONNECTION", "keep-alive, UPGRADE"),
            ("sec-websocket-key", "dGhlIHNhbXBsZSBub25jZQ=="),
            ("sec-websocket-version", "13"),
            ("origin", "HTTPS://A.EXAMPLE"),
        ]);
        assert!(handshake(&r, ORIGINS).is_ok());
    }

    #[test]
    fn upgrade_requests_are_refused() {
        let status = |r: &Request| handshake(r, ORIGINS).unwrap_err().status;
        let mut r = request(&good());
        r.method = "POST".into();
        assert_eq!(status(&r), 405);
        let mut r = request(&good());
        r.version = "HTTP/1.0".into();
        assert_eq!(status(&r), 400);
        assert_eq!(status(&replaced("Upgrade", None)), 400);
        assert_eq!(status(&replaced("Upgrade", Some("h2c"))), 400);
        assert_eq!(status(&replaced("Connection", None)), 400);
        assert_eq!(status(&replaced("Connection", Some("keep-alive"))), 400);
        assert_eq!(status(&replaced("Sec-WebSocket-Version", None)), 426);
        assert_eq!(status(&replaced("Sec-WebSocket-Version", Some("8"))), 426);
        assert_eq!(status(&replaced("Sec-WebSocket-Key", None)), 400);
        for k in ["", "AAAA", "dGhlIHNhbXBsZSBub25jZQ", "dGhlIHNhbXBsZSBub25jZQ==x", "dGhlIHNhbXBsZSBub25jZQ=!", "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA", "dGhlIHNhbXBsZSBub25jZQ=a"] {
            assert_eq!(status(&replaced("Sec-WebSocket-Key", Some(k))), 400, "{k:?}");
        }
        assert_eq!(status(&replaced("Origin", None)), 403);
        assert_eq!(status(&replaced("Origin", Some("https://evil.example"))), 403);
        assert_eq!(status(&replaced("Origin", Some("null"))), 403);
        assert_eq!(handshake(&request(&good()), &[]).unwrap_err().status, 403);
        let two = ["https://b.example", "https://a.example"];
        assert!(handshake(&request(&good()), &two).is_ok());
        let resp = String::from_utf8(handshake(&replaced("Sec-WebSocket-Version", Some("8")), ORIGINS).unwrap_err().response()).unwrap();
        assert!(resp.starts_with("HTTP/1.1 426 Upgrade Required\r\n") && resp.contains("Sec-WebSocket-Version: 13\r\n"));
    }

    #[test]
    fn messages_are_read_masked_and_sent_unmasked() {
        let long = vec![7u8; 300];
        let huge = vec![9u8; 70_000];
        let input = cat(&[frame(0x81, b"hello"), frame(0x82, &[1, 2, 3]), frame(0x82, &long), frame(0x81, b""), frame(0x82, &huge[..60_000])]);
        let mut c = conn(input, false, 64 << 10);
        assert_eq!(c.recv().unwrap(), Some(Message::Text("hello".into())));
        assert_eq!(c.recv().unwrap(), Some(Message::Binary(vec![1, 2, 3])));
        assert_eq!(c.recv().unwrap(), Some(Message::Binary(long.clone())));
        assert_eq!(c.recv().unwrap(), Some(Message::Text(String::new())));
        assert_eq!(c.recv().unwrap(), Some(Message::Binary(huge[..60_000].to_vec())));
        assert!(matches!(c.recv(), Err(Error::Idle)));
        assert!(!c.is_closed());
        // sending: no mask bit, all three length forms
        c.send_text("hi").unwrap();
        c.send_binary(&long).unwrap();
        c.send_binary(&huge).unwrap();
        c.send_ping(b"p").unwrap();
        assert_eq!(&c.stream.out[..4], &[0x81, 2, b'h', b'i']);
        assert_eq!(&c.stream.out[4..8], &[0x82, 126, 0x01, 0x2c]);
        let at = 4 + 4 + 300;
        assert_eq!(&c.stream.out[at..at + 10], &[0x82, 127, 0, 0, 0, 0, 0, 1, 0x11, 0x70]);
        assert_eq!(&c.stream.out[at + 10 + 70_000..], &[0x89, 1, b'p']);
        assert!(c.send_ping(&[0; 126]).is_err());
    }

    #[test]
    fn unmasked_frames_are_refused() {
        let mut f = frame(0x81, b"hi");
        f[1] &= 0x7f;
        let f = [&f[..2], b"hi"].concat();
        let mut c = conn(f, false, 1024);
        assert!(matches!(c.recv(), Err(Error::Protocol(1002, _))));
        assert_eq!(sent_close_code(&c), Some(1002));
        assert!(c.is_closed());
        assert!(c.recv().is_err());
        assert!(c.send_text("x").is_err());
    }

    #[test]
    fn malformed_frames_are_refused_with_1002() {
        let cases: Vec<(&str, Vec<u8>)> = vec![
            ("rsv1", frame(0xc1, b"x")),
            ("rsv2", frame(0xa1, b"x")),
            ("rsv3", frame(0x91, b"x")),
            ("opcode 3", frame(0x83, b"")),
            ("opcode 7", frame(0x87, b"")),
            ("opcode 11", frame(0x8b, b"")),
            ("opcode 15", frame(0x8f, b"")),
            ("fragmented ping", frame(0x09, b"")),
            ("fragmented close", frame(0x08, b"")),
            ("ping of 126", frame(0x89, &[0; 126])),
            ("close of 200", frame(0x88, &[0; 200])),
            ("continuation first", frame(0x80, b"x")),
            ("new message in a fragmented one", cat(&[frame(0x01, b"a"), frame(0x81, b"b")])),
            ("one-byte close", frame(0x88, &[3])),
            ("close code 1005", frame(0x88, &1005u16.to_be_bytes())),
            ("close code 999", frame(0x88, &999u16.to_be_bytes())),
            ("close code 5000", frame(0x88, &5000u16.to_be_bytes())),
            ("64-bit length, top bit", {
                let mut f = vec![0x82, 0x80 | 127];
                f.extend_from_slice(&(1u64 << 63).to_be_bytes());
                f.extend_from_slice(&MASK);
                f
            }),
            ("64-bit length, all ones", {
                let mut f = vec![0x82, 0x80 | 127];
                f.extend_from_slice(&u64::MAX.to_be_bytes());
                f.extend_from_slice(&MASK);
                f
            }),
        ];
        for (name, input) in cases {
            let mut c = conn(input, false, 1024);
            match c.recv() {
                Err(Error::Protocol(1002, _)) => {}
                other => panic!("{name}: {other:?}"),
            }
            assert_eq!(sent_close_code(&c), Some(1002), "{name}");
        }
    }

    #[test]
    fn oversize_messages_are_refused_with_1009_before_they_are_read() {
        // the length alone is enough: no payload follows
        let mut f = vec![0x82, 0x80 | 127];
        f.extend_from_slice(&(1u64 << 40).to_be_bytes());
        f.extend_from_slice(&MASK);
        let mut c = conn(f, false, 64 << 10);
        assert!(matches!(c.recv(), Err(Error::Protocol(1009, _))));
        assert_eq!(sent_close_code(&c), Some(1009));
        let mut c = conn(frame(0x82, &[0; 65]), false, 64);
        assert!(matches!(c.recv(), Err(Error::Protocol(1009, _))));
        // the limit counts fragments together
        let mut c = conn(cat(&[frame(0x02, &[0; 40]), frame(0x00, &[0; 30])]), false, 64);
        assert!(matches!(c.recv(), Err(Error::Protocol(1009, _))));
        // exactly the limit is fine
        let mut c = conn(cat(&[frame(0x02, &[0; 40]), frame(0x80, &[0; 24])]), false, 64);
        assert_eq!(c.recv().unwrap(), Some(Message::Binary(vec![0; 64])));
        // the default is 64 KiB
        assert_eq!(Config::default().max_message, 65536);
    }

    #[test]
    fn fragments_are_reassembled_around_control_frames() {
        let input = cat(&[frame(0x01, b"Hel"), frame(0x89, b"ping1"), frame(0x00, b"lo, "), frame(0x8a, b"unsolicited"), frame(0x89, b""), frame(0x80, b"world")]);
        let mut c = conn(input, false, 1024);
        assert_eq!(c.recv().unwrap(), Some(Message::Text("Hello, world".into())));
        // each ping was answered with its payload; the pong was not
        assert_eq!(c.stream.out, [vec![0x8a, 5], b"ping1".to_vec(), vec![0x8a, 0]].concat());
        // binary too, then another message straight after
        let mut c = conn(cat(&[frame(0x02, &[1]), frame(0x00, &[2]), frame(0x80, &[3]), frame(0x81, b"x")]), false, 1024);
        assert_eq!(c.recv().unwrap(), Some(Message::Binary(vec![1, 2, 3])));
        assert_eq!(c.recv().unwrap(), Some(Message::Text("x".into())));
    }

    #[test]
    fn text_must_be_utf8_once_whole() {
        // a two-byte character split across fragments is fine
        let e = "Ã©".as_bytes();
        let mut c = conn(cat(&[frame(0x01, &e[..1]), frame(0x80, &e[1..])]), false, 1024);
        assert_eq!(c.recv().unwrap(), Some(Message::Text("Ã©".into())));
        // a whole message that is not UTF-8 is 1007
        let mut c = conn(frame(0x81, &[0xff, 0xfe]), false, 1024);
        assert!(matches!(c.recv(), Err(Error::Protocol(1007, _))));
        assert_eq!(sent_close_code(&c), Some(1007));
        // so is one cut mid-character at its end, though each fragment ended there
        let mut c = conn(cat(&[frame(0x01, &e[..1]), frame(0x80, b"")]), false, 1024);
        assert!(matches!(c.recv(), Err(Error::Protocol(1007, _))));
        // an invalid byte split across fragments
        let mut c = conn(cat(&[frame(0x01, &[0xc3]), frame(0x00, &[0x28]), frame(0x80, b"")]), false, 1024);
        assert!(matches!(c.recv(), Err(Error::Protocol(1007, _))));
        // binary is not checked
        let mut c = conn(frame(0x82, &[0xff, 0xfe]), false, 1024);
        assert_eq!(c.recv().unwrap(), Some(Message::Binary(vec![0xff, 0xfe])));
        // a close reason that is not UTF-8 is 1007
        let mut c = conn(frame(0x88, &[0x03, 0xe8, 0xff]), false, 1024);
        assert!(matches!(c.recv(), Err(Error::Protocol(1007, _))));
    }

    #[test]
    fn the_client_closes() {
        let mut p = 1001u16.to_be_bytes().to_vec();
        p.extend_from_slice("bye".as_bytes());
        let mut c = conn(cat(&[frame(0x88, &p), frame(0x81, b"after")]), false, 1024);
        assert_eq!(c.recv().unwrap(), None);
        assert_eq!(c.peer_close(), Some(&(1001, "bye".to_string())));
        assert_eq!(c.stream.out, vec![0x88, 2, 0x03, 0xe9]);
        assert_eq!(c.recv().unwrap(), None);
        assert!(c.is_closed() && c.send_text("x").is_err());
        // a close with no code is answered with none
        let mut c = conn(frame(0x88, b""), false, 1024);
        assert_eq!(c.recv().unwrap(), None);
        assert_eq!(c.peer_close(), Some(&(1005, String::new())));
        assert_eq!(c.stream.out, vec![0x88, 0]);
    }

    #[test]
    fn the_server_closes() {
        let mut c = conn(cat(&[frame(0x81, b"late"), frame(0x89, b"p"), frame(0x88, &1000u16.to_be_bytes())]), false, 1024);
        c.close(1000, "done").unwrap();
        assert_eq!(c.stream.out, [vec![0x88, 6, 0x03, 0xe8], b"done".to_vec()].concat());
        assert!(c.is_closed());
        assert!(c.send_text("x").is_err() && c.send_ping(b"").is_err());
        // a second close sends nothing; what the client sent meanwhile is dropped, with no pong
        c.close(1000, "").unwrap();
        assert_eq!(c.recv().unwrap(), None);
        assert_eq!(c.peer_close(), Some(&(1000, String::new())));
        assert_eq!(c.stream.out.len(), 8);
        // a code that cannot be sent becomes 1000; a long reason is cut on a character boundary
        let mut c = conn(Vec::new(), false, 1024);
        c.close(1005, &"Ã©".repeat(100)).unwrap();
        let o = &c.stream.out;
        assert_eq!((o[0], o[1] as usize, &o[2..4]), (0x88, 124, &[0x03, 0xe8][..]));
        assert!(std::str::from_utf8(&o[4..]).is_ok() && o.len() == 126);
    }

    #[test]
    fn idle_and_dead_connections() {
        let mut c = conn(Vec::new(), false, 1024);
        let t = Instant::now();
        assert!(matches!(c.recv(), Err(Error::Idle)));
        assert!(t.elapsed() >= Duration::from_millis(30));
        assert!(!c.is_closed()); // between frames: still usable
        c.send_text("still here").unwrap();
        // inside a frame it is not
        let f = frame(0x81, b"hello");
        let mut c = conn(f[..4].to_vec(), false, 1024);
        assert!(matches!(c.recv(), Err(Error::Idle)));
        assert!(c.is_closed() && c.recv().is_err());
        // end of stream, whole or in a frame
        let mut c = conn(Vec::new(), true, 1024);
        assert!(matches!(c.recv(), Err(Error::Io(e)) if e.kind() == ErrorKind::UnexpectedEof));
        let mut c = conn(f[..4].to_vec(), true, 1024);
        assert!(matches!(c.recv(), Err(Error::Io(_))));
    }

    #[test]
    fn no_input_panics() {
        // every prefix of valid traffic, and bytes of every value in the header positions
        let all = cat(&[frame(0x01, b"ab"), frame(0x89, b"x"), frame(0x80, b"cd"), frame(0x88, &1000u16.to_be_bytes())]);
        for n in 0..=all.len() {
            let mut c = conn(all[..n].to_vec(), true, 8);
            while let Ok(Some(_)) = c.recv() {}
        }
        for a in 0..=255u8 {
            for b in 0..=255u8 {
                let mut c = conn(vec![a, b, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14], true, 64);
                let _ = c.recv();
                let _ = c.recv();
            }
        }
    }

    // ---- a real loopback connection, with a minimal client written by hand ----

    fn read_head(s: &mut TcpStream) -> String {
        let mut head = Vec::new();
        let mut b = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            s.read_exact(&mut b).unwrap();
            head.push(b[0]);
        }
        String::from_utf8(head).unwrap()
    }

    /// Reads one unmasked server frame.
    fn read_frame(s: &mut TcpStream) -> (u8, Vec<u8>) {
        let mut h = [0u8; 2];
        s.read_exact(&mut h).unwrap();
        assert_eq!(h[1] & 0x80, 0, "a server frame was masked");
        let len = match h[1] {
            126 => {
                let mut b = [0u8; 2];
                s.read_exact(&mut b).unwrap();
                u16::from_be_bytes(b) as usize
            }
            127 => {
                let mut b = [0u8; 8];
                s.read_exact(&mut b).unwrap();
                u64::from_be_bytes(b) as usize
            }
            n => n as usize,
        };
        let mut p = vec![0u8; len];
        s.read_exact(&mut p).unwrap();
        (h[0], p)
    }

    #[test]
    fn a_round_trip_over_loopback() {
        use super::super::http::Server;
        use crate::net::Allow;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut server = Server::new(listener, Allow::default(), 1024, None).unwrap();

        let client = std::thread::spawn(move || {
            let mut s = TcpStream::connect(addr).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let req = "GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nOrigin: https://a.example\r\n\r\n";
            s.write_all(req.as_bytes()).unwrap();
            let head = read_head(&mut s);
            assert!(head.starts_with("HTTP/1.1 101 "), "{head}");
            assert!(head.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n"));
            // a fragmented text with a ping between, then binary
            s.write_all(&frame(0x01, b"mouse ")).unwrap();
            s.write_all(&frame(0x89, b"hb")).unwrap();
            s.write_all(&frame(0x80, b"move")).unwrap();
            assert_eq!(read_frame(&mut s), (0x8a, b"hb".to_vec()));
            assert_eq!(read_frame(&mut s), (0x81, b"got mouse move".to_vec()));
            s.write_all(&frame(0x82, &[1, 2, 3])).unwrap();
            assert_eq!(read_frame(&mut s), (0x82, vec![3, 2, 1]));
            assert_eq!(read_frame(&mut s), (0x89, b"are you there".to_vec()));
            s.write_all(&frame(0x8a, b"are you there")).unwrap();
            // the server closes; the client answers
            let (op, p) = read_frame(&mut s);
            assert_eq!((op, &p[..2], &p[2..]), (0x88, &1000u16.to_be_bytes()[..], &b"bye"[..]));
            s.write_all(&frame(0x88, &1000u16.to_be_bytes())).unwrap();
            let mut rest = Vec::new();
            s.read_to_end(&mut rest).unwrap();
            assert!(rest.is_empty());
        });

        let (req, _peer, responder) = loop {
            if let Some(x) = server.poll(Instant::now()).pop() {
                break x;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        assert_eq!(req.version, "HTTP/1.1");
        let accept = handshake(&req, ORIGINS).unwrap();
        // the 101 goes out on the stream the connection then uses
        let mut stream = responder.into_stream(Duration::from_millis(50)).unwrap();
        stream.write_all(&upgrade_response(&accept)).unwrap();
        let mut c = Connection::new(stream, Config { max_message: 1024, idle: Duration::from_secs(10) });
        let Some(Message::Text(t)) = c.recv().unwrap() else { panic!("not text") };
        c.send_text(&format!("got {t}")).unwrap();
        let Some(Message::Binary(mut b)) = c.recv().unwrap() else { panic!("not binary") };
        b.reverse();
        c.send_binary(&b).unwrap();
        c.send_ping(b"are you there").unwrap();
        c.close(1000, "bye").unwrap();
        // the client's pong is ignored, its close reply ends the exchange
        assert_eq!(c.recv().unwrap(), None);
        assert_eq!(c.peer_close().map(|p| p.0), Some(1000));
        drop(c);
        client.join().unwrap();
    }

    #[test]
    fn an_upgrade_is_refused_over_loopback_with_a_response() {
        use super::super::http::Server;
        use crate::net::Allow;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let mut server = Server::new(listener, Allow::default(), 1024, None).unwrap();
        let client = std::thread::spawn(move || {
            let mut s = TcpStream::connect(addr).unwrap();
            s.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
            let req = "GET /ws HTTP/1.1\r\nHost: x\r\nUpgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\nOrigin: https://evil.example\r\n\r\n";
            s.write_all(req.as_bytes()).unwrap();
            let mut out = String::new();
            s.read_to_string(&mut out).unwrap();
            assert!(out.starts_with("HTTP/1.1 403 Forbidden\r\n"), "{out}");
        });
        let (req, _, responder) = loop {
            if let Some(x) = server.poll(Instant::now()).pop() {
                break x;
            }
            std::thread::sleep(Duration::from_millis(5));
        };
        responder.send(&handshake(&req, ORIGINS).unwrap_err().response());
        client.join().unwrap();
    }
}
