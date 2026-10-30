//! A small HTTP/1.1 client for sources: GET over plain TCP, every connection (and every redirect)
//! through the address allow-list, one request per connection, bodies by length, chunked or until
//! the connection closes, byte ranges, and timeouts on every read and write. HTTPS goes through
//! [`tls`](crate::tls), and a redirect from it to plain HTTP is refused.

use crate::net::{self, Allow};
use crate::tls::{self, Conn};
use crate::url::{percent_decode, redact, split_authority};
use ironav_core::{Error, ErrorCode, Result};
use std::io::{BufRead, BufReader, Read, Write};
use std::time::Duration;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_HEADER: usize = 64 << 10;
const MAX_REDIRECTS: usize = 5;
const USER_AGENT: &str = concat!("IronAV/", env!("CARGO_PKG_VERSION"));

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct HttpUrl {
    /// Host and port as written (brackets kept), for the Host header and for display.
    pub authority: String,
    pub host: String,
    pub port: u16,
    /// The path and query, starting with `/`.
    pub path: String,
    pub user: Option<(String, String)>,
    /// `https`.
    pub tls: bool,
}

impl HttpUrl {
    pub fn parse(url: &str) -> Result<HttpUrl> {
        let (scheme, rest) = url.split_once("://").ok_or_else(|| Error::invalid_arg(format!("'{}' is not an http:// URL", redact(url))))?;
        let tls = scheme.eq_ignore_ascii_case("https");
        if !tls && !scheme.eq_ignore_ascii_case("http") {
            return Err(Error::invalid_arg(format!("'{scheme}' is not http or https")));
        }
        let (cred, authority, path) = split_authority(rest);
        let (host, port) = if let Some(v6) = authority.strip_prefix('[') {
            let (h, after) = v6.split_once(']').ok_or_else(|| Error::invalid_arg("an http URL has an unclosed '['"))?;
            (h, after.strip_prefix(':'))
        } else {
            match authority.rsplit_once(':') {
                Some((h, p)) => (h, Some(p)),
                None => (authority, None),
            }
        };
        if host.is_empty() {
            return Err(Error::invalid_arg("an http URL needs a host"));
        }
        let port = port.map(|p| p.parse::<u16>().map_err(|_| Error::invalid_arg("the http URL's port is not a number"))).transpose()?.unwrap_or(if tls { 443 } else { 80 });
        let user = cred.map(|c| {
            let (u, p) = c.split_once(':').unwrap_or((c, ""));
            (percent_decode(u), percent_decode(p))
        });
        if user.as_ref().is_some_and(|(u, p)| u.chars().chain(p.chars()).any(|c| c.is_control())) {
            return Err(Error::invalid_arg("the http URL's credentials hold control characters"));
        }
        let path = path.split('#').next().unwrap_or("");
        if path.chars().any(|c| c.is_control()) {
            return Err(Error::invalid_arg("an http URL's path holds control characters"));
        }
        let path = path.replace(' ', "%20");
        let path = if path.is_empty() { "/".to_string() } else if path.starts_with('?') { format!("/{path}") } else { path };
        Ok(HttpUrl { authority: authority.to_string(), host: host.to_string(), port, path, user, tls })
    }

    fn scheme(&self) -> &'static str {
        if self.tls { "https" } else { "http" }
    }

    /// The URL without credentials.
    pub fn plain(&self) -> String {
        format!("{}://{}{}", self.scheme(), self.authority, self.path)
    }

    /// The URL with its credentials, escaped: for resolving references against, never for display.
    pub fn full(&self) -> String {
        let esc = |s: &str| s.bytes().map(|b| if b.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") }).collect::<String>();
        match &self.user {
            Some((u, p)) => format!("{}://{}:{}@{}{}", self.scheme(), esc(u), esc(p), self.authority, self.path),
            None => self.plain(),
        }
    }
}

/// `reference` resolved against `base` (RFC 3986 §5, the cases playlists use): absolute, scheme-
/// relative, absolute-path and relative references, with `.` and `..` segments removed.
pub(crate) fn join(base: &str, reference: &str) -> String {
    let reference = reference.trim();
    // absolute only when it begins with a scheme, not when a query holds a URL
    if reference.find("://").is_some_and(|i| i > 0 && reference[..i].bytes().all(|b| b.is_ascii_alphanumeric() || b"+-.".contains(&b))) {
        return reference.to_string();
    }
    // an '@' in a path is a plain character, escaped so it is never read as user information
    let escaped;
    let reference = if reference.contains('@') && !reference.starts_with("//") {
        escaped = reference.replace('@', "%40");
        escaped.as_str()
    } else {
        reference
    };
    let (scheme, rest) = base.split_once("://").unwrap_or(("http", base));
    if let Some(r) = reference.strip_prefix("//") {
        return format!("{scheme}://{r}");
    }
    let (_, _, path) = split_authority(rest);
    // everything before the path: user information (the same server keeps it) and authority
    let root = &rest[..rest.len() - path.len()];
    let path = path.split(['?', '#']).next().unwrap_or("");
    let merged = if reference.starts_with('/') {
        reference.to_string()
    } else if reference.starts_with('?') {
        format!("{path}{reference}")
    } else {
        format!("{}{reference}", &path[..path.rfind('/').map_or(0, |i| i + 1)])
    };
    let merged = merged.split('#').next().unwrap_or("").to_string();
    let merged = if merged.starts_with('/') { merged } else { format!("/{merged}") };
    // remove dot segments
    let (p, q) = match merged.find('?') {
        Some(i) => merged.split_at(i),
        None => (merged.as_str(), ""),
    };
    let mut out: Vec<&str> = Vec::new();
    let segs: Vec<&str> = p.split('/').skip(1).collect();
    for (i, s) in segs.iter().enumerate() {
        match *s {
            "." => {
                if i == segs.len() - 1 {
                    out.push("");
                }
            }
            ".." => {
                out.pop();
                if i == segs.len() - 1 {
                    out.push("");
                }
            }
            s => out.push(s),
        }
    }
    format!("{scheme}://{root}/{}{q}", out.join("/"))
}

enum Body {
    Length(u64),
    Chunked { left: u64, done: bool },
    Close,
}

pub(crate) struct Response {
    /// The URL that answered, after redirects, with the credentials that went to it: references are
    /// resolved against it, and it is shown only through `redact`.
    pub url: String,
    conn: BufReader<Conn>,
    body: Body,
}

impl Response {
    /// The whole body, which must be at most `max` bytes.
    pub fn bytes(mut self, max: usize) -> Result<Vec<u8>> {
        let mut v = Vec::new();
        (&mut self).take(max as u64 + 1).read_to_end(&mut v)?;
        if v.len() > max {
            return Err(Error::invalid_input(format!("{} is larger than {} MiB", redact(&self.url), max >> 20)));
        }
        Ok(v)
    }
}

impl Read for Response {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match &mut self.body {
            Body::Length(0) => Ok(0),
            Body::Length(left) => {
                if buf.is_empty() {
                    return Ok(0);
                }
                let k = (*left).min(buf.len() as u64) as usize;
                let n = self.conn.read(&mut buf[..k])?;
                if n == 0 {
                    return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the server closed the connection before the end of the body"));
                }
                *left -= n as u64;
                Ok(n)
            }
            Body::Close => self.conn.read(buf),
            Body::Chunked { done: true, .. } => Ok(0),
            Body::Chunked { left, done } => {
                if *left == 0 {
                    let mut line = String::new();
                    (&mut self.conn).take(1024).read_line(&mut line)?;
                    let size = u64::from_str_radix(line.trim().split(';').next().unwrap_or("").trim(), 16).map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidData, "a bad chunk size"))?;
                    if size == 0 {
                        // trailers, up to the empty line (and not forever)
                        for k in 0.. {
                            let mut t = String::new();
                            if (&mut self.conn).take(8192).read_line(&mut t)? == 0 || t.trim().is_empty() {
                                break;
                            }
                            if k == 64 {
                                return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "more than 64 trailer lines"));
                            }
                        }
                        *done = true;
                        return Ok(0);
                    }
                    *left = size;
                }
                let k = (*left).min(buf.len() as u64) as usize;
                let n = self.conn.read(&mut buf[..k])?;
                if n == 0 {
                    return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "the server closed the connection inside a chunk"));
                }
                *left -= n as u64;
                if *left == 0 {
                    let mut crlf = [0u8; 2];
                    self.conn.read_exact(&mut crlf)?;
                }
                Ok(n)
            }
        }
    }
}

/// GETs `url` (bytes `first..=last` of it when `range` is given), following redirects; each
/// connection goes only where `allow` permits, and credentials only to the URL's own server.
pub(crate) fn get(url: &str, range: Option<(u64, u64)>, allow: &Allow) -> Result<Response> {
    let mut u = HttpUrl::parse(url)?;
    for _ in 0..=MAX_REDIRECTS {
        let shown = redact(&u.plain());
        let tcp = net::connect(&u.host, u.port, CONNECT_TIMEOUT, allow, &shown)?;
        let mut conn = if u.tls { tls::connect(tcp, &u.host, Some(b"http/1.1"), &shown)? } else { Conn::Plain(tcp) };
        conn.tcp().set_read_timeout(Some(IO_TIMEOUT))?;
        conn.tcp().set_write_timeout(Some(IO_TIMEOUT))?;
        let mut req = format!("GET {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: {USER_AGENT}\r\nAccept: */*\r\nConnection: close\r\n", u.path, u.authority);
        if let Some((user, pass)) = &u.user {
            req += &format!("Authorization: Basic {}\r\n", base64(format!("{user}:{pass}").as_bytes()));
        }
        if let Some((first, last)) = range {
            req += &format!("Range: bytes={first}-{last}\r\n");
        }
        req += "\r\n";
        conn.write_all(req.as_bytes())?;
        let mut conn = BufReader::new(conn);
        let mut size = 0;
        // interim answers (100 Continue, 103 Early Hints) come before the real one
        let (status, headers) = loop {
            let mut status_line = String::new();
            (&mut conn).take(8192).read_line(&mut status_line)?;
            let status: u16 = status_line.split_whitespace().nth(1).and_then(|s| s.parse().ok()).filter(|_| status_line.starts_with("HTTP/1.")).ok_or_else(|| Error::new(ErrorCode::Io, format!("{shown} did not answer in HTTP/1.x")))?;
            let mut headers = Vec::new();
            size += status_line.len();
            loop {
                let mut line = String::new();
                let n = (&mut conn).take(8192).read_line(&mut line)?;
                size += n;
                if n == 0 || line.trim().is_empty() {
                    break;
                }
                if size > MAX_HEADER {
                    return Err(Error::invalid_input(format!("{shown} sent more than {} KiB of headers", MAX_HEADER >> 10)));
                }
                if let Some((k, v)) = line.split_once(':') {
                    headers.push((k.trim().to_string(), v.trim().to_string()));
                }
            }
            if !(100..200).contains(&status) {
                break (status, headers);
            }
        };
        let header = |n: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| v.clone());
        if matches!(status, 301 | 302 | 303 | 307 | 308) {
            let loc = header("location").ok_or_else(|| Error::new(ErrorCode::Io, format!("{shown} redirects without saying where")))?;
            let next = HttpUrl::parse(&join(&u.plain(), &loc))?;
            if u.tls && !next.tls {
                return Err(Error::new(ErrorCode::Io, format!("{shown} redirects to plain HTTP ({}), which is refused", redact(&next.plain()))));
            }
            let same = next.host.eq_ignore_ascii_case(&u.host) && next.port == u.port;
            u = HttpUrl { user: if next.user.is_some() { next.user.clone() } else if same { u.user.clone() } else { None }, ..next };
            continue;
        }
        if status >= 400 {
            let code = if status == 404 || status == 410 { ErrorCode::NotFound } else { ErrorCode::Io };
            return Err(Error::new(code, format!("{shown} answered {status}")));
        }
        if let Some((first, _)) = range {
            // the part must start where it was asked to
            let starts = header("content-range").and_then(|r| r.trim().strip_prefix("bytes ")?.split('-').next()?.trim().parse::<u64>().ok());
            if status != 206 || starts != Some(first) {
                return Err(Error::new(ErrorCode::Io, format!("{shown} did not answer a byte-range request with the range asked for (status {status})")));
            }
        }
        let body = if header("transfer-encoding").is_some_and(|t| t.to_ascii_lowercase().contains("chunked")) {
            Body::Chunked { left: 0, done: false }
        } else if let Some(len) = header("content-length") {
            Body::Length(len.parse().map_err(|_| Error::invalid_input(format!("{shown} sent a Content-Length of '{len}'")))?)
        } else if status == 204 || status == 304 {
            Body::Length(0)
        } else {
            Body::Close
        };
        return Ok(Response { url: u.full(), conn, body });
    }
    Err(Error::new(ErrorCode::Io, format!("{} redirects more than {MAX_REDIRECTS} times", redact(url))))
}

pub(crate) fn base64(d: &[u8]) -> String {
    const A: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut s = String::new();
    for c in d.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            s.push(if i <= c.len() { A[(n >> (18 - 6 * i) & 63) as usize] as char } else { '=' });
        }
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_and_references() {
        let u = HttpUrl::parse("http://bob:p%40ss@cam.local:8080/a/b.m3u8?x=1").unwrap();
        assert_eq!((u.host.as_str(), u.port, u.path.as_str(), u.plain().as_str()), ("cam.local", 8080, "/a/b.m3u8?x=1", "http://cam.local:8080/a/b.m3u8?x=1"));
        assert_eq!(u.user, Some(("bob".into(), "p@ss".into())));
        assert_eq!(HttpUrl::parse("http://[::1]").unwrap().path, "/");
        let s = HttpUrl::parse("https://u:p@x/y").unwrap();
        assert_eq!((s.port, s.tls, s.plain().as_str(), s.full().as_str()), (443, true, "https://x/y", "https://u:p@x/y"));
        assert_eq!(join("https://x/a/i.m3u8", "s.ts"), "https://x/a/s.ts");
        assert!(HttpUrl::parse("ftp://x/y").is_err());
        let base = "http://h:1/live/v/index.m3u8?token=a";
        assert_eq!(join(base, "seg1.ts"), "http://h:1/live/v/seg1.ts");
        assert_eq!(join(base, "../a/seg.ts?x"), "http://h:1/live/a/seg.ts?x");
        assert_eq!(join(base, "/root.ts"), "http://h:1/root.ts");
        assert_eq!(join(base, "//cdn/x.ts"), "http://cdn/x.ts");
        assert_eq!(join(base, "http://other/y.ts"), "http://other/y.ts");
        assert_eq!(join("http://h/index.m3u8", "./a/./b/../c.ts"), "http://h/a/c.ts");
        assert_eq!(join(base, "seg.ts?u=http://x/y#frag"), "http://h:1/live/v/seg.ts?u=http://x/y");
        // an '@' in a segment's name stays in the path, and the playlist's credentials stay with its server
        assert_eq!(join("http://u:p@h/live/i.m3u8", "seg@1.ts"), "http://u:p@h/live/seg%401.ts");
        assert_eq!(HttpUrl::parse(&join("http://u:p@h/live/i.m3u8", "seg@1.ts")).unwrap().host, "h");
        assert_eq!(join("http://u:p@h/live/i.m3u8", "//cdn/x.ts"), "http://cdn/x.ts");
        assert!(HttpUrl::parse("http://h/a\r\nX-Injected: 1").is_err());
        assert_eq!(HttpUrl::parse("http://h/a b.ts").unwrap().path, "/a%20b.ts");
        let u = HttpUrl::parse("http://bob:p%40ss@h/x").unwrap();
        assert_eq!(HttpUrl::parse(&u.full()).unwrap(), u);
    }
}
