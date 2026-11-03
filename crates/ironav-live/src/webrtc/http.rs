//! The WHIP endpoint's HTTP/1.1 server: a few connections at a time, each read without blocking
//! until a whole request has come (headers and a body by Content-Length, both bounded) or its time
//! is up, answered once, and closed. Connections from an address the allow-list does not admit
//! (loopback always is) are closed unread.

use crate::net::Allow;
use std::io::{ErrorKind, Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::time::{Duration, Instant};

const MAX_CONNECTIONS: usize = 8;
const MAX_HEADER: usize = 16 << 10;
/// How long a client has to send its request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Request {
    pub method: String,
    /// The path, without any query.
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    pub(crate) fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// What a buffer holds so far.
#[derive(Debug, PartialEq)]
pub(crate) enum Parsed {
    Partial,
    Request(Request),
    /// A status to answer with, and why.
    Bad(u16, &'static str),
}

/// Parses a request from the start of `buf`, with a body of at most `max_body` bytes.
pub(crate) fn parse(buf: &[u8], max_body: usize) -> Parsed {
    let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") else {
        return if buf.len() > MAX_HEADER { Parsed::Bad(431, "the request's headers are too long") } else { Parsed::Partial };
    };
    if end > MAX_HEADER {
        return Parsed::Bad(431, "the request's headers are too long");
    }
    let Ok(head) = std::str::from_utf8(&buf[..end]) else { return Parsed::Bad(400, "the request's headers are not text") };
    let mut lines = head.split("\r\n");
    let mut first = lines.next().unwrap_or("").split(' ');
    let (Some(method), Some(target), Some(version)) = (first.next(), first.next(), first.next()) else { return Parsed::Bad(400, "malformed request line") };
    if !version.starts_with("HTTP/1.") || method.is_empty() || !method.bytes().all(|b| b.is_ascii_uppercase()) || !target.starts_with('/') {
        return Parsed::Bad(400, "malformed request line");
    }
    let mut headers = Vec::new();
    for l in lines {
        let Some((k, v)) = l.split_once(':') else { return Parsed::Bad(400, "malformed header") };
        if headers.len() == 64 {
            return Parsed::Bad(431, "too many headers");
        }
        headers.push((k.trim().to_string(), v.trim().to_string()));
    }
    let mut r = Request { method: method.to_string(), path: target.split('?').next().unwrap_or("/").to_string(), headers, body: Vec::new() };
    if let Some(te) = r.header("transfer-encoding") {
        // a client may send even an empty body chunked
        if !te.eq_ignore_ascii_case("chunked") || r.header("content-length").is_some() {
            return Parsed::Bad(400, "unsupported Transfer-Encoding");
        }
        return match chunked(&buf[end + 4..], max_body) {
            Ok(Some(body)) => {
                r.body = body;
                Parsed::Request(r)
            }
            Ok(None) => Parsed::Partial,
            Err((status, why)) => Parsed::Bad(status, why),
        };
    }
    let len = match r.header("content-length").map(|v| v.parse::<usize>()) {
        None => 0,
        Some(Ok(n)) if n <= max_body => n,
        Some(Ok(_)) => return Parsed::Bad(413, "the body is too long"),
        Some(Err(_)) => return Parsed::Bad(400, "malformed Content-Length"),
    };
    let Some(body) = buf.get(end + 4..end + 4 + len) else { return Parsed::Partial };
    r.body = body.to_vec();
    Parsed::Request(r)
}

/// A chunked body (RFC 9112 §7.1) of at most `max` bytes: `None` until all of it (and the
/// trailer) has come.
fn chunked(mut b: &[u8], max: usize) -> std::result::Result<Option<Vec<u8>>, (u16, &'static str)> {
    let bad = (400, "malformed chunked body");
    let mut body = Vec::new();
    loop {
        let Some(eol) = b.windows(2).position(|w| w == b"\r\n") else {
            return if b.len() > 64 { Err(bad) } else { Ok(None) };
        };
        let line = std::str::from_utf8(&b[..eol]).map_err(|_| bad)?;
        let size = usize::from_str_radix(line.split(';').next().unwrap_or("").trim(), 16).map_err(|_| bad)?;
        b = &b[eol + 2..];
        if size == 0 {
            // trailer fields, then an empty line
            loop {
                let Some(eol) = b.windows(2).position(|w| w == b"\r\n") else {
                    return if b.len() > MAX_HEADER { Err(bad) } else { Ok(None) };
                };
                b = &b[eol + 2..];
                if eol == 0 {
                    return Ok(Some(body));
                }
            }
        }
        if body.len() + size > max {
            return Err((413, "the body is too long"));
        }
        let Some(data) = b.get(..size + 2) else { return Ok(None) };
        if &data[size..] != b"\r\n" {
            return Err(bad);
        }
        body.extend_from_slice(&data[..size]);
        b = &b[size + 2..];
    }
}

/// A response, closing the connection after it.
pub(crate) fn response(status: u16, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let reason = match status {
        200 => "OK",
        201 => "Created",
        204 => "No Content",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        409 => "Conflict",
        411 => "Length Required",
        413 => "Content Too Large",
        415 => "Unsupported Media Type",
        431 => "Request Header Fields Too Large",
        500 => "Internal Server Error",
        _ => "Error",
    };
    let mut out = format!("HTTP/1.1 {status} {reason}\r\nConnection: close\r\nContent-Length: {}\r\n", body.len());
    for (k, v) in headers {
        out.push_str(&format!("{k}: {v}\r\n"));
    }
    out.push_str("\r\n");
    let mut out = out.into_bytes();
    out.extend_from_slice(body);
    out
}

struct Conn {
    stream: TcpStream,
    peer: SocketAddr,
    buf: Vec<u8>,
    deadline: Instant,
}

pub(crate) struct Server {
    listener: TcpListener,
    conns: Vec<Conn>,
    allow: Allow,
    max_body: usize,
}

impl Server {
    pub(crate) fn new(listener: TcpListener, allow: Allow, max_body: usize) -> std::io::Result<Server> {
        listener.set_nonblocking(true)?;
        Ok(Server { listener, conns: Vec::new(), allow, max_body })
    }

    pub(crate) fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.listener.local_addr()
    }

    /// Accepts and reads what has come, answering malformed requests itself, and returns the whole
    /// requests with a handle to answer each by.
    pub(crate) fn poll(&mut self, now: Instant) -> Vec<(Request, SocketAddr, Responder)> {
        while self.conns.len() < MAX_CONNECTIONS {
            match self.listener.accept() {
                Ok((s, peer)) => {
                    if !(peer.ip().is_loopback() || self.allow.permits(peer.ip())) || s.set_nonblocking(true).is_err() {
                        continue; // dropped unread
                    }
                    self.conns.push(Conn { stream: s, peer, buf: Vec::new(), deadline: now + REQUEST_TIMEOUT });
                }
                Err(_) => break,
            }
        }
        let mut out = Vec::new();
        let mut k = 0;
        while k < self.conns.len() {
            let c = &mut self.conns[k];
            let mut tmp = [0u8; 4096];
            let mut closed = false;
            loop {
                match c.stream.read(&mut tmp) {
                    Ok(0) => {
                        closed = true;
                        break;
                    }
                    Ok(n) => {
                        c.buf.extend_from_slice(&tmp[..n]);
                        if c.buf.len() > MAX_HEADER + self.max_body + 4 {
                            break;
                        }
                    }
                    Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                    Err(e) if e.kind() == ErrorKind::Interrupted => {}
                    Err(_) => {
                        closed = true;
                        break;
                    }
                }
            }
            let parsed = parse(&c.buf, self.max_body);
            match parsed {
                Parsed::Partial if !closed && now < c.deadline => {
                    k += 1;
                    continue;
                }
                Parsed::Partial => {}
                Parsed::Bad(status, why) => Responder(c.stream.try_clone().ok()).send(&response(status, &[("Content-Type", "text/plain")], why.as_bytes())),
                Parsed::Request(r) => out.push((r, c.peer, Responder(c.stream.try_clone().ok()))),
            }
            self.conns.swap_remove(k);
        }
        out
    }
}

/// Where a request's response goes.
pub(crate) struct Responder(Option<TcpStream>);

impl Responder {
    pub(crate) fn send(self, bytes: &[u8]) {
        if let Some(mut s) = self.0 {
            let _ = s.set_nonblocking(false);
            let _ = s.set_write_timeout(Some(WRITE_TIMEOUT));
            let _ = s.write_all(bytes);
            let _ = s.shutdown(std::net::Shutdown::Write);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requests_are_read_whole_and_bounded() {
        let r = b"POST /live?x=1 HTTP/1.1\r\nHost: a\r\nContent-Type: application/sdp\r\nContent-Length: 5\r\n\r\nv=0\r\n";
        match parse(r, 100) {
            Parsed::Request(q) => {
                assert_eq!((q.method.as_str(), q.path.as_str(), q.body.as_slice()), ("POST", "/live", &b"v=0\r\n"[..]));
                assert_eq!(q.header("content-type"), Some("application/sdp"));
            }
            other => panic!("{other:?}"),
        }
        for n in 0..r.len() {
            assert_eq!(parse(&r[..n], 100), Parsed::Partial);
        }
        assert!(matches!(parse(r, 4), Parsed::Bad(413, _)));
        let c = b"DELETE /s HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\n3;x=y\r\nabc\r\n2\r\nde\r\n0\r\nT: 1\r\n\r\n";
        match parse(c, 100) {
            Parsed::Request(q) => assert_eq!((q.method.as_str(), q.body.as_slice()), ("DELETE", &b"abcde"[..])),
            other => panic!("{other:?}"),
        }
        for n in 0..c.len() {
            assert_eq!(parse(&c[..n], 100), Parsed::Partial, "{n}");
        }
        assert!(matches!(parse(c, 4), Parsed::Bad(413, _)));
        assert!(matches!(parse(b"POST / HTTP/1.1\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n", 100), Parsed::Bad(400, _)));
        assert!(matches!(parse(b"POST / HTTP/1.1\r\nTransfer-Encoding: gzip\r\n\r\n", 100), Parsed::Bad(400, _)));
        assert!(matches!(parse(b"GET http://x/ HTTP/1.1\r\n\r\n", 100), Parsed::Bad(400, _)));
        assert!(matches!(parse(&vec![b'a'; MAX_HEADER + 10], 100), Parsed::Bad(431, _)));
        assert!(matches!(parse(b"POST / HTTP/1.1\r\nContent-Length: -1\r\n\r\n", 100), Parsed::Bad(400, _)));
    }
}
