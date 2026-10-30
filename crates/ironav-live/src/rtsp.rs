//! An RTSP 1.0 client (RFC 2326) that plays a camera's or server's stream and hands on its RTP.
//!
//! `open` makes the connection through the address allow-list, sends OPTIONS, DESCRIBE, a SETUP
//! for each track IronAV can read, and PLAY. RTP comes interleaved on the RTSP connection by
//! default, so no UDP ports are needed and nothing arrives out of order; with `?transport=udp` it
//! comes over UDP to a pair of local ports per track instead, taken only from the server's address
//! (so another host cannot inject packets), with an RTCP receiver report every few seconds. It answers Basic and Digest challenges with
//! the URL's credentials and follows redirects, each one checked against the allow-list again.
//! The bytes it gives are the RTP stream framing `ironav_formats::rtp` reads: first a description
//! (the SDP, each track's channel, and the RTP-Info times), then each interleaved RTP and RTCP
//! frame as it arrived. RTSP replies on the same connection are taken out. While playing it sends
//! a keepalive at half the session timeout, and TEARDOWN when it is dropped.

use crate::md5::md5_hex;
use crate::net::{self, Allow};
use crate::source::{Chunk, Source, SourceInfo};
use crate::url::{percent_decode, split_authority, SourceUrl};
use ironav_core::{Error, ErrorCode, Result};
use ironav_formats::rtp::{description_frame, Description, Sdp, TrackSetup};
use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpStream, UdpSocket};
use std::time::{Duration, Instant};

const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a reply may take.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_HEADER: usize = 64 << 10;
const MAX_BODY: usize = 1 << 20;
/// Frames received but not yet read, at most.
const MAX_OUT: usize = 64 << 20;
const MAX_REDIRECTS: usize = 5;
/// Tracks set up at most, which keeps every interleaved channel below the description's.
const MAX_TRACKS: usize = 32;
/// How often an RTCP receiver report goes to a server sending over UDP.
const REPORT_EVERY: Duration = Duration::from_secs(5);
/// How long a read over UDP sleeps between looks at its sockets.
const UDP_POLL: Duration = Duration::from_millis(2);
const USER_AGENT: &str = concat!("IronAV/", env!("CARGO_PKG_VERSION"));

/// Where to connect, the URL to send (without credentials), and the credentials.
#[derive(Debug, Clone, PartialEq)]
struct Target {
    host: String,
    port: u16,
    url: String,
    user: Option<(String, String)>,
}

/// `rest` is what follows `rtsp://`. A redirect's target keeps the credentials of `from` only when it
/// names the same host and port: they are never offered to another server.
fn parse_target(rest: &str, from: Option<&Target>) -> Result<Target> {
    let bad = || Error::invalid_arg("an RTSP source needs rtsp://[user[:password]@]host[:port][/path]");
    let (cred, hostport, path) = split_authority(rest);
    let (host, port) = if let Some(v6) = hostport.strip_prefix('[') {
        let (h, after) = v6.split_once(']').ok_or_else(bad)?;
        (h, after.strip_prefix(':'))
    } else {
        match hostport.rsplit_once(':') {
            Some((h, p)) => (h, Some(p)),
            None => (hostport, None),
        }
    };
    if host.is_empty() {
        return Err(bad());
    }
    // the port is not echoed: a malformed URL may have put part of a password there
    let port = port.map(|p| p.parse::<u16>().map_err(|_| Error::invalid_arg("the RTSP URL's port is not a number"))).transpose()?.unwrap_or(554);
    let user = match cred {
        Some(c) => {
            let (u, p) = c.split_once(':').unwrap_or((c, ""));
            let (u, p) = (percent_decode(u), percent_decode(p));
            if u.chars().chain(p.chars()).any(|c| c.is_control() || c == '"') {
                return Err(Error::invalid_arg("the RTSP URL's credentials hold control characters or quotes, which cannot be sent"));
            }
            Some((u, p))
        }
        None => from.filter(|f| f.host.eq_ignore_ascii_case(host) && f.port == port).and_then(|f| f.user.clone()),
    };
    Ok(Target { host: host.to_string(), port, url: format!("rtsp://{hostport}{}", if path.is_empty() { "/" } else { path }), user })
}

#[derive(Debug, Default)]
struct Response {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
    fn all<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> {
        self.headers.iter().filter(move |(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

enum Message {
    Frame(Vec<u8>),
    Response(Response),
    /// A request from the server (ANNOUNCE, GET_PARAMETER, ...), which needs no answer here.
    Request,
}

enum Auth {
    Basic(String),
    Digest { user: String, pass: String, realm: String, nonce: String, opaque: Option<String>, qop: bool, nc: u32 },
}

impl Auth {
    /// The credentials answering `challenge` (a `WWW-Authenticate` value), if it is one we speak.
    fn answer(challenge: &str, user: &str, pass: &str) -> Option<Auth> {
        let (scheme, params) = challenge.trim().split_once(' ').unwrap_or((challenge.trim(), ""));
        if scheme.eq_ignore_ascii_case("basic") {
            return Some(Auth::Basic(format!("Basic {}", base64(format!("{user}:{pass}").as_bytes()))));
        }
        if !scheme.eq_ignore_ascii_case("digest") {
            return None;
        }
        let p = auth_params(params);
        let get = |k: &str| p.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)).map(|(_, v)| v.clone());
        if get("algorithm").is_some_and(|a| !a.eq_ignore_ascii_case("md5")) {
            return None;
        }
        Some(Auth::Digest {
            user: user.into(),
            pass: pass.into(),
            realm: get("realm")?,
            nonce: get("nonce")?,
            opaque: get("opaque"),
            qop: get("qop").is_some_and(|q| q.split(',').any(|o| o.trim().eq_ignore_ascii_case("auth"))),
            nc: 0,
        })
    }

    fn header(&mut self, method: &str, uri: &str) -> String {
        match self {
            Auth::Basic(h) => h.clone(),
            Auth::Digest { user, pass, realm, nonce, opaque, qop, nc } => {
                let ha1 = md5_hex(format!("{user}:{realm}:{pass}").as_bytes());
                let ha2 = md5_hex(format!("{method}:{uri}").as_bytes());
                let mut h = format!("Digest username=\"{user}\", realm=\"{realm}\", nonce=\"{nonce}\", uri=\"{uri}\"");
                if *qop {
                    *nc += 1;
                    let cnonce = md5_hex(format!("{nonce}{nc}{:?}", Instant::now()).as_bytes())[..16].to_string();
                    let response = md5_hex(format!("{ha1}:{nonce}:{nc:08x}:{cnonce}:auth:{ha2}").as_bytes());
                    h += &format!(", response=\"{response}\", qop=auth, nc={nc:08x}, cnonce=\"{cnonce}\"");
                } else {
                    h += &format!(", response=\"{}\"", md5_hex(format!("{ha1}:{nonce}:{ha2}").as_bytes()));
                }
                if let Some(o) = opaque {
                    h += &format!(", opaque=\"{o}\"");
                }
                h
            }
        }
    }
}

/// `name=value` pairs of an authentication challenge; values may be quoted and hold commas.
fn auth_params(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = s.trim();
    while !rest.is_empty() {
        let Some((name, after)) = rest.split_once('=') else { break };
        let after = after.trim_start();
        let (value, tail) = if let Some(q) = after.strip_prefix('"') {
            match q.find('"') {
                Some(e) => (&q[..e], &q[e + 1..]),
                None => (q, ""),
            }
        } else {
            match after.find(',') {
                Some(e) => (&after[..e], &after[e..]),
                None => (after, ""),
            }
        };
        out.push((name.trim().trim_start_matches(',').trim().to_string(), value.to_string()));
        rest = tail.trim_start().trim_start_matches(',').trim_start();
    }
    out
}

fn base64(d: &[u8]) -> String {
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

/// A track's URL: its control attribute against the base.
fn resolve(base: &str, control: Option<&str>) -> String {
    match control.map(str::trim) {
        None | Some("") | Some("*") => base.to_string(),
        Some(c) if c.to_ascii_lowercase().starts_with("rtsp://") => c.to_string(),
        Some(c) if base.ends_with('/') => format!("{base}{c}"),
        Some(c) => format!("{base}/{c}"),
    }
}

pub(crate) struct RtspSource {
    info: SourceInfo,
    conn: TcpStream,
    /// Received, not yet parsed.
    buf: Vec<u8>,
    /// Frames ready to be read.
    out: VecDeque<u8>,
    cseq: u32,
    auth: Option<Auth>,
    user: Option<(String, String)>,
    session: Option<String>,
    /// The URL aggregate requests (PLAY, keepalives, TEARDOWN) go to.
    control: String,
    keepalive: Duration,
    last_sent: Instant,
    get_parameter: bool,
    ended: bool,
    /// Ask for RTP over UDP, and the sockets it arrives on.
    want_udp: bool,
    udp: Vec<UdpTrack>,
    last_report: Instant,
}

/// One track's RTP and RTCP sockets when RTP comes over UDP.
struct UdpTrack {
    rtp: UdpSocket,
    rtcp: UdpSocket,
    channel: u8,
    /// The addresses datagrams are taken from, and where receiver reports go.
    from: Vec<IpAddr>,
    report_to: Option<SocketAddr>,
}

/// An even local port for RTP and the odd one above it for RTCP, as RFC 3550 §11 asks.
fn bind_pair(ip: IpAddr) -> Result<(UdpSocket, UdpSocket)> {
    for _ in 0..32 {
        let rtp = UdpSocket::bind((ip, 0))?;
        let port = rtp.local_addr()?.port();
        if port % 2 != 0 || port == u16::MAX {
            continue;
        }
        if let Ok(rtcp) = UdpSocket::bind((ip, port + 1)) {
            rtp.set_nonblocking(true)?;
            rtcp.set_nonblocking(true)?;
            return Ok((rtp, rtcp));
        }
    }
    Err(Error::new(ErrorCode::Io, "could not find a free pair of UDP ports for RTP"))
}

impl RtspSource {
    pub(crate) fn open(u: &SourceUrl, shown: String) -> Result<RtspSource> {
        let allow = Allow::from_env()?;
        let want_udp = match u.option("transport").map(str::to_ascii_lowercase).as_deref() {
            None | Some("tcp") => false,
            Some("udp") => true,
            Some(other) => return Err(Error::invalid_arg(format!("transport must be tcp or udp, got '{other}'"))),
        };
        let mut target = parse_target(&u.target, None)?;
        for _ in 0..=MAX_REDIRECTS {
            let conn = net::connect(&target.host, target.port, CONNECT_TIMEOUT, &allow, &shown)?;
            let _ = conn.set_nodelay(true);
            // a server that stops reading must not hold a write forever
            conn.set_write_timeout(Some(REPLY_TIMEOUT))?;
            let mut s = RtspSource {
                info: SourceInfo { url: shown.clone(), kind: "rtsp", live: true },
                conn,
                buf: Vec::new(),
                out: VecDeque::new(),
                cseq: 0,
                auth: None,
                user: target.user.clone(),
                session: None,
                control: target.url.clone(),
                keepalive: Duration::from_secs(30),
                last_sent: Instant::now(),
                get_parameter: false,
                ended: false,
                want_udp,
                udp: Vec::new(),
                last_report: Instant::now(),
            };
            match s.start(&target.url, &shown)? {
                None => return Ok(s),
                Some(location) => {
                    let rest = location.split_once("://").filter(|(sch, _)| sch.eq_ignore_ascii_case("rtsp")).map(|(_, r)| r).ok_or_else(|| Error::new(ErrorCode::UnsupportedFormat, format!("{shown} redirects to a URL that is not rtsp://")))?;
                    target = parse_target(rest, Some(&target))?;
                }
            }
        }
        Err(Error::new(ErrorCode::Io, format!("{shown} redirects more than {MAX_REDIRECTS} times")))
    }

    /// DESCRIBE, SETUP and PLAY; `Some(location)` when the server redirects.
    fn start(&mut self, url: &str, shown: &str) -> Result<Option<String>> {
        let r = self.request("OPTIONS", url, &[], shown)?;
        if (300..400).contains(&r.status) {
            return r.header("location").map(|l| Ok(Some(l.to_string()))).unwrap_or_else(|| Err(Error::new(ErrorCode::Io, format!("{shown} redirects without saying where"))));
        }
        self.get_parameter = r.header("public").is_some_and(|p| p.split(',').any(|m| m.trim().eq_ignore_ascii_case("GET_PARAMETER")));
        let r = self.request("DESCRIBE", url, &[("Accept", "application/sdp")], shown)?;
        match r.status {
            200 => {}
            300..=399 => return r.header("location").map(|l| Ok(Some(l.to_string()))).unwrap_or_else(|| Err(Error::new(ErrorCode::Io, format!("{shown} redirects without saying where")))),
            _ => return Err(status_error(&r, "DESCRIBE", shown)),
        }
        let text = String::from_utf8_lossy(&r.body).into_owned();
        let sdp = Sdp::parse(&text)?;
        let base = r.header("content-base").or(r.header("content-location")).unwrap_or(url).trim().to_string();
        self.control = resolve(&base, sdp.control.as_deref());

        let mut tracks = Vec::new();
        let mut urls = Vec::new();
        let mut skipped = Vec::new();
        for (i, m) in sdp.media.iter().enumerate() {
            if let Err(why) = m.codec() {
                skipped.push(format!("{} ({why})", m.kind));
                continue;
            }
            if tracks.len() == MAX_TRACKS {
                skipped.push(format!("{} (more than {MAX_TRACKS} tracks)", m.kind));
                continue;
            }
            let track_url = resolve(&base, m.control.as_deref());
            let ch = 2 * tracks.len() as u8;
            let pair = if self.want_udp { Some(bind_pair(self.conn.local_addr()?.ip())?) } else { None };
            let transport = match &pair {
                Some((rtp, rtcp)) => format!("RTP/AVP;unicast;client_port={}-{}", rtp.local_addr()?.port(), rtcp.local_addr()?.port()),
                None => format!("RTP/AVP/TCP;unicast;interleaved={}-{}", ch, ch + 1),
            };
            let r = self.request("SETUP", &track_url, &[("Transport", &transport)], shown)?;
            if r.status == 461 {
                return Err(if self.want_udp {
                    Error::new(ErrorCode::UnsupportedFormat, format!("{shown} does not send RTP over UDP")).hint("leave out ?transport=udp to have RTP sent on the RTSP connection")
                } else {
                    Error::new(ErrorCode::UnsupportedFormat, format!("{shown} does not send RTP over the RTSP connection (interleaved TCP)")).hint("add ?transport=udp to have it sent over UDP")
                });
            }
            if r.status != 200 {
                return Err(status_error(&r, "SETUP", shown));
            }
            if self.session.is_none() {
                let s = r.header("session").ok_or_else(|| Error::new(ErrorCode::Io, format!("{shown} set up a track without a session")))?;
                self.session = Some(s.split(';').next().unwrap_or("").trim().to_string());
                if let Some(t) = param(s, "timeout").and_then(|t| t.parse::<u64>().ok()) {
                    self.keepalive = Duration::from_secs((t / 2).clamp(1, 300));
                }
            }
            if let Some((rtp, rtcp)) = pair {
                // datagrams are taken only from the server, or a source it names that the allow-list permits
                let reply = r.header("transport").unwrap_or("");
                let peer = self.conn.peer_addr()?;
                let mut from = vec![peer.ip()];
                if let Some(ip) = param(reply, "source").and_then(|s| s.parse::<IpAddr>().ok()).filter(|ip| Allow::from_env().is_ok_and(|a| a.permits(*ip))) {
                    from.push(ip);
                }
                let report_to = param(reply, "server_port").and_then(|p| {
                    let mut ports = p.split('-').map(|x| x.trim().parse::<u16>().ok());
                    let first = ports.next()??;
                    Some(SocketAddr::new(peer.ip(), ports.next().flatten().unwrap_or(first.wrapping_add(1))))
                });
                self.udp.push(UdpTrack { rtp, rtcp, channel: ch, from, report_to });
            }
            // the server may choose other channels
            let channel = if self.want_udp { ch } else { r.header("transport").and_then(|t| param(t, "interleaved")).and_then(|c| c.split('-').next()?.trim().parse::<u8>().ok()).unwrap_or(ch) };
            if channel >= 254 || tracks.iter().any(|t: &TrackSetup| t.channel.abs_diff(channel) < 2) {
                return Err(Error::new(ErrorCode::Io, format!("{shown} chose interleaved channel {channel}, which another track or the description uses")));
            }
            tracks.push(TrackSetup { media: i, channel, rtptime: None, seq: None });
            urls.push(track_url);
        }
        if tracks.is_empty() {
            return Err(Error::new(ErrorCode::UnsupportedCodec, format!("{shown} offers no track IronAV can read: {}", skipped.join(", "))));
        }
        let control = self.control.clone();
        let r = self.request("PLAY", &control, &[("Range", "npt=0.000-")], shown)?;
        if r.status != 200 {
            return Err(status_error(&r, "PLAY", shown));
        }
        if let Some(info) = r.header("rtp-info") {
            let entries: Vec<&str> = info.split(',').collect();
            // a track is matched by its whole URL, or else by a last path segment no other track shares
            let segment = |s: &str| s.trim_end_matches('/').rsplit('/').next().unwrap_or("").to_string();
            for entry in &entries {
                let (url, seq, rtptime) = (param(entry, "url"), param(entry, "seq").and_then(|v| v.parse::<u16>().ok()), param(entry, "rtptime").and_then(|v| v.parse::<u32>().ok()));
                let k = match url {
                    _ if entries.len() == 1 && tracks.len() == 1 => Some(0),
                    Some(u) => urls.iter().position(|t| t == u).or_else(|| {
                        let s = segment(u);
                        let hits: Vec<usize> = (0..urls.len()).filter(|&k| !s.is_empty() && segment(&urls[k]) == s).collect();
                        (hits.len() == 1).then(|| hits[0])
                    }),
                    None => None,
                };
                if let Some(k) = k {
                    tracks[k].seq = seq;
                    tracks[k].rtptime = rtptime;
                }
            }
        }
        let desc = description_frame(&Description { sdp: text, tracks })?;
        // the description goes before any frame that came with the PLAY reply
        for &b in desc.iter().rev() {
            self.out.push_front(b);
        }
        self.last_sent = Instant::now();
        Ok(None)
    }

    fn send(&mut self, method: &str, url: &str, headers: &[(&str, &str)]) -> Result<u32> {
        self.cseq += 1;
        let mut req = format!("{method} {url} RTSP/1.0\r\nCSeq: {}\r\nUser-Agent: {USER_AGENT}\r\n", self.cseq);
        if let Some(s) = &self.session {
            req += &format!("Session: {s}\r\n");
        }
        if let Some(a) = self.auth.as_mut() {
            req += &format!("Authorization: {}\r\n", a.header(method, url));
        }
        for (k, v) in headers {
            req += &format!("{k}: {v}\r\n");
        }
        req += "\r\n";
        self.conn.write_all(req.as_bytes())?;
        self.last_sent = Instant::now();
        Ok(self.cseq)
    }

    /// Sends a request and waits for its reply, answering one authentication challenge.
    fn request(&mut self, method: &str, url: &str, headers: &[(&str, &str)], shown: &str) -> Result<Response> {
        let mut answered = false;
        loop {
            let cseq = self.send(method, url, headers)?;
            let r = self.reply(cseq, shown)?;
            if r.status != 401 || answered {
                if r.status == 401 {
                    return Err(Error::new(ErrorCode::InvalidArgument, format!("{shown} refused the credentials")).hint("check the user name and password in the URL"));
                }
                return Ok(r);
            }
            let Some((user, pass)) = self.user.clone() else {
                return Err(Error::new(ErrorCode::InvalidArgument, format!("{shown} asks for credentials")).hint("put them in the URL: rtsp://user:password@host/path"));
            };
            let challenges: Vec<String> = r.all("www-authenticate").map(str::to_string).collect();
            self.auth = Some(choose_auth(&challenges, &user, &pass).map_err(|why| Error::new(ErrorCode::UnsupportedFormat, format!("{shown} {why}")))?);
            answered = true;
        }
    }

    fn reply(&mut self, cseq: u32, shown: &str) -> Result<Response> {
        let deadline = Instant::now() + REPLY_TIMEOUT;
        loop {
            while let Some(m) = self.message()? {
                match m {
                    Message::Frame(f) => {
                        self.out.extend(f);
                        if self.out.len() > MAX_OUT {
                            return Err(Error::new(ErrorCode::Io, format!("{shown} sent more than {} MiB before replying", MAX_OUT >> 20)));
                        }
                    }
                    Message::Response(r) if r.header("cseq").and_then(|c| c.trim().parse::<u32>().ok()) == Some(cseq) => return Ok(r),
                    _ => {}
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(Error::new(ErrorCode::Io, format!("{shown} did not reply within {} s", REPLY_TIMEOUT.as_secs())));
            }
            if !self.fill(left)? {
                return Err(Error::new(ErrorCode::Io, format!("{shown} closed the connection")));
            }
        }
    }

    /// Reads what has arrived, waiting at most `wait`; false at the end of the connection.
    fn fill(&mut self, wait: Duration) -> Result<bool> {
        self.conn.set_read_timeout(Some(wait.max(Duration::from_millis(1))))?;
        let mut tmp = [0u8; 65536];
        match self.conn.read(&mut tmp) {
            Ok(0) => Ok(false),
            Ok(n) => {
                self.buf.extend_from_slice(&tmp[..n]);
                Ok(true)
            }
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted) => Ok(true),
            Err(e) => Err(e.into()),
        }
    }

    /// The next whole frame or message in what has arrived.
    fn message(&mut self) -> Result<Option<Message>> {
        let b = &self.buf;
        if b.is_empty() {
            return Ok(None);
        }
        if b[0] == b'$' {
            if b.len() < 4 {
                return Ok(None);
            }
            let n = 4 + u16::from_be_bytes([b[2], b[3]]) as usize;
            if b.len() < n {
                return Ok(None);
            }
            return Ok(Some(Message::Frame(self.buf.drain(..n).collect())));
        }
        let Some(head_end) = b.windows(4).take(MAX_HEADER).position(|w| w == b"\r\n\r\n") else {
            if b.len() > MAX_HEADER {
                return Err(Error::invalid_input("the RTSP server sent something that is neither a reply nor an RTP frame"));
            }
            return Ok(None);
        };
        let head = String::from_utf8_lossy(&b[..head_end]).into_owned();
        let mut lines = head.split("\r\n");
        let first = lines.next().unwrap_or("");
        let headers: Vec<(String, String)> = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect();
        let len = match headers.iter().find(|(k, _)| k.eq_ignore_ascii_case("content-length")) {
            Some((_, v)) => v.parse::<usize>().map_err(|_| Error::invalid_input(format!("an RTSP message has a Content-Length of '{v}'")))?,
            None => 0,
        };
        if len > MAX_BODY {
            return Err(Error::invalid_input(format!("an RTSP reply claims a body of {len} bytes")));
        }
        let total = head_end + 4 + len;
        if b.len() < total {
            return Ok(None);
        }
        let body = b[head_end + 4..total].to_vec();
        self.buf.drain(..total);
        if let Some(rest) = first.strip_prefix("RTSP/") {
            let status = rest.split_whitespace().nth(1).and_then(|s| s.parse().ok()).unwrap_or(0);
            return Ok(Some(Message::Response(Response { status, headers, body })));
        }
        Ok(Some(Message::Request))
    }
}

/// A `;`-separated parameter of a header value (`Transport`, `Session`, an RTP-Info entry), its name
/// matched without regard to case.
fn param<'a>(value: &'a str, name: &str) -> Option<&'a str> {
    value.split(';').find_map(|p| {
        let (k, v) = p.trim().split_once('=')?;
        k.trim().eq_ignore_ascii_case(name).then_some(v.trim())
    })
}

/// The answer to a 401's challenges. Digest is preferred, since it does not send the password; if
/// the server offers Digest only in a form IronAV does not speak, the password is not sent in the
/// clear instead, even if Basic is offered too.
fn choose_auth(challenges: &[String], user: &str, pass: &str) -> std::result::Result<Auth, String> {
    let digest: Vec<&String> = challenges.iter().filter(|c| c.trim_start().to_ascii_lowercase().starts_with("digest")).collect();
    if !digest.is_empty() {
        return digest.iter().find_map(|c| Auth::answer(c, user, pass)).ok_or_else(|| format!("asks for Digest authentication in a form IronAV does not speak ({}), and the password is not sent in the clear instead", digest.iter().map(|s| s.as_str()).collect::<Vec<_>>().join("; ")));
    }
    challenges.iter().find_map(|c| Auth::answer(c, user, pass)).ok_or_else(|| format!("asks for an authentication scheme IronAV does not speak: {}", challenges.join("; ")))
}

fn status_error(r: &Response, method: &str, shown: &str) -> Error {
    let code = match r.status {
        404 => ErrorCode::NotFound,
        415 | 451 | 461 => ErrorCode::UnsupportedFormat,
        _ => ErrorCode::Io,
    };
    Error::new(code, format!("{shown} answered {method} with status {}", r.status))
}

impl Source for RtspSource {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    fn read(&mut self, buf: &mut [u8], wait: Duration) -> Result<Chunk> {
        if !self.udp.is_empty() && self.out.is_empty() && !self.ended {
            self.read_udp(wait)?;
        }
        if self.out.is_empty() && !self.ended && self.udp.is_empty() {
            if self.last_sent.elapsed() >= self.keepalive {
                // the reply is taken out of the stream like any other
                let method = if self.get_parameter { "GET_PARAMETER" } else { "OPTIONS" };
                let control = self.control.clone();
                self.send(method, &control, &[])?;
            }
            self.ended = !self.fill(wait)?;
            while let Some(m) = self.message()? {
                if let Message::Frame(f) = m {
                    self.out.extend(f);
                }
            }
            if self.out.len() > MAX_OUT {
                return Err(Error::new(ErrorCode::Io, format!("{} sent more than {} MiB that was not read", self.info.url, MAX_OUT >> 20)));
            }
        }
        if self.out.is_empty() {
            return Ok(if self.ended { Chunk::End } else { Chunk::Idle });
        }
        let n = buf.len().min(self.out.len());
        for (d, s) in buf.iter_mut().zip(self.out.drain(..n)) {
            *d = s;
        }
        Ok(Chunk::Data(n))
    }
}

impl RtspSource {
    /// Gathers datagrams from the tracks' sockets (and replies from the RTSP connection) for at
    /// most `wait`, framed as interleaved RTP would be.
    fn read_udp(&mut self, wait: Duration) -> Result<()> {
        let deadline = Instant::now() + wait;
        let mut tmp = vec![0u8; 65536];
        loop {
            if self.last_sent.elapsed() >= self.keepalive {
                let method = if self.get_parameter { "GET_PARAMETER" } else { "OPTIONS" };
                let control = self.control.clone();
                self.send(method, &control, &[])?;
            }
            if self.last_report.elapsed() >= REPORT_EVERY {
                self.last_report = Instant::now();
                // an empty receiver report from a fixed SSRC: enough to show the server we are here
                let rr = [0x80, 201, 0, 1, 0x49, 0x72, 0x41, 0x56];
                for t in &self.udp {
                    if let Some(to) = t.report_to {
                        let _ = t.rtcp.send_to(&rr, to);
                    }
                }
            }
            for t in &self.udp {
                for (sock, channel) in [(&t.rtp, t.channel), (&t.rtcp, t.channel + 1)] {
                    loop {
                        match sock.recv_from(&mut tmp) {
                            Ok((n, from)) if t.from.contains(&from.ip()) && n <= u16::MAX as usize => {
                                self.out.extend([b'$', channel]);
                                self.out.extend((n as u16).to_be_bytes());
                                self.out.extend(&tmp[..n]);
                            }
                            Ok(_) => {} // from elsewhere: not ours to read
                            Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                            Err(e) if matches!(e.kind(), ErrorKind::ConnectionReset | ErrorKind::Interrupted) => {}
                            Err(e) => return Err(e.into()),
                        }
                    }
                }
            }
            // replies to keepalives, and the end of the connection
            self.ended = !self.fill(Duration::from_millis(1))?;
            while let Some(m) = self.message()? {
                if let Message::Frame(f) = m {
                    self.out.extend(f);
                }
            }
            if self.out.len() > MAX_OUT {
                return Err(Error::new(ErrorCode::Io, format!("{} sent more than {} MiB that was not read", self.info.url, MAX_OUT >> 20)));
            }
            if !self.out.is_empty() || self.ended || Instant::now() >= deadline {
                return Ok(());
            }
            std::thread::sleep(UDP_POLL);
        }
    }
}

impl Drop for RtspSource {
    fn drop(&mut self) {
        if self.session.is_some() && !self.ended {
            let _ = self.conn.set_write_timeout(Some(Duration::from_secs(1)));
            let control = self.control.clone();
            let _ = self.send("TEARDOWN", &control, &[]);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;
    use std::sync::mpsc;

    #[test]
    fn targets_and_challenges() {
        let t = parse_target("bob:p%40ss@cam.local:8554/live/main?x=1", None).unwrap();
        assert_eq!(t, Target { host: "cam.local".into(), port: 8554, url: "rtsp://cam.local:8554/live/main?x=1".into(), user: Some(("bob".into(), "p@ss".into())) });
        let t = parse_target("[fe80::1]/s", None).unwrap();
        assert_eq!((t.host.as_str(), t.port, t.url.as_str()), ("fe80::1", 554, "rtsp://[fe80::1]/s"));
        assert!(parse_target("/nohost", None).is_err());
        assert_eq!(resolve("rtsp://h/a/", Some("trackID=1")), "rtsp://h/a/trackID=1");
        assert_eq!(resolve("rtsp://h/a", Some("trackID=1")), "rtsp://h/a/trackID=1");
        assert_eq!(resolve("rtsp://h/a", Some("rtsp://other/x")), "rtsp://other/x");
        assert_eq!(resolve("rtsp://h/a", Some("*")), "rtsp://h/a");

        // RFC 2617 §3.5's example, with RTSP's method and URI in place of HTTP's
        let mut a = Auth::answer(r#"Digest realm="testrealm@host.com", qop="auth,auth-int", nonce="dcd98b7102dd2f0e8b11d0f600bfb0c093", opaque="5ccc069c403ebaf9f0171e9517f40e41""#, "Mufasa", "Circle Of Life").unwrap();
        let h = a.header("GET", "/dir/index.html");
        assert!(h.contains(r#"realm="testrealm@host.com""#) && h.contains("qop=auth, nc=00000001") && h.contains(r#"opaque="5ccc069c403ebaf9f0171e9517f40e41""#), "{h}");
        // without qop: MD5(HA1:nonce:HA2)
        let mut a = Auth::answer(r#"Digest realm="r", nonce="n""#, "u", "p").unwrap();
        let want = md5_hex(format!("{}:n:{}", md5_hex(b"u:r:p"), md5_hex(b"DESCRIBE:rtsp://h/s")).as_bytes());
        assert!(a.header("DESCRIBE", "rtsp://h/s").contains(&format!("response=\"{want}\"")));
        assert_eq!(Auth::answer("Basic realm=\"x\"", "Aladdin", "open sesame").unwrap().header("X", "y"), "Basic QWxhZGRpbjpvcGVuIHNlc2FtZQ==");
        assert!(Auth::answer("Digest realm=\"r\", nonce=\"n\", algorithm=SHA-256", "u", "p").is_none());
        // Digest in a form we do not speak does not fall back to Basic
        let offered = ["Digest realm=\"r\", nonce=\"n\", algorithm=SHA-256".to_string(), "Basic realm=\"r\"".to_string()];
        assert!(choose_auth(&offered, "u", "p").is_err());
        assert!(matches!(choose_auth(&offered[1..], "u", "p"), Ok(Auth::Basic(_))));
        assert_eq!(param("RTP/AVP/TCP;unicast;Interleaved=4-5", "interleaved"), Some("4-5"));

        // a password with an unescaped '/' is taken whole, and redirects keep credentials only on the same server
        let t = parse_target("bob:pa/ss@cam:8554/live", None).unwrap();
        assert_eq!((t.host.as_str(), t.port, t.url.as_str(), t.user.as_ref().unwrap().1.as_str()), ("cam", 8554, "rtsp://cam:8554/live", "pa/ss"));
        assert_eq!(parse_target("cam:8554/other", Some(&t)).unwrap().user, t.user);
        assert_eq!(parse_target("elsewhere:8554/live", Some(&t)).unwrap().user, None);
        assert_eq!(parse_target("cam:554/live", Some(&t)).unwrap().user, None);
        assert!(parse_target("bob:p%0d%0aX@cam/live", None).is_err());
        let e = parse_target("bob:secret:x@cam:9z/live", None).unwrap_err();
        assert!(!e.message.contains("secret") && !e.message.contains("9z"));
    }

    /// The RTP stream FFmpeg's muxer sent (H.264 with B-frames and AAC), framed as this source frames it.
    fn fixture() -> (String, Vec<u32>, Vec<Vec<u8>>) {
        let d = std::fs::read(format!("{}/../ironav-formats/tests/data/h264_aac.rtp", env!("CARGO_MANIFEST_DIR"))).unwrap();
        let mut frames = Vec::new();
        let mut i = 0;
        while i + 4 <= d.len() {
            let n = 4 + u16::from_be_bytes([d[i + 2], d[i + 3]]) as usize;
            frames.push(d[i..i + n].to_vec());
            i += n;
        }
        let desc: Description = serde_json::from_slice(&frames.remove(0)[4..]).unwrap();
        (desc.sdp, desc.tracks.iter().map(|t| t.rtptime.unwrap()).collect(), frames)
    }

    /// A camera that wants Digest credentials, serves the fixture, and reports the requests it saw.
    fn camera(listener: TcpListener, seen: mpsc::Sender<String>) {
        let (sdp, rtptimes, frames) = fixture();
        let (mut c, _) = listener.accept().unwrap();
        c.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
        let mut buf = Vec::new();
        let mut authorized = false;
        let udp = UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut client_ports: Vec<u16> = Vec::new();
        loop {
            let mut tmp = [0u8; 4096];
            let n = match c.read(&mut tmp) {
                Ok(0) | Err(_) => return,
                Ok(n) => n,
            };
            buf.extend_from_slice(&tmp[..n]);
            while let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                let req = String::from_utf8_lossy(&buf[..end]).into_owned();
                buf.drain(..end + 4);
                let method = req.split_whitespace().next().unwrap_or("").to_string();
                let cseq = req.lines().find_map(|l| l.strip_prefix("CSeq: ")).unwrap_or("0").to_string();
                seen.send(req.clone()).unwrap();
                let auth = req.lines().find_map(|l| l.strip_prefix("Authorization: "));
                if method != "OPTIONS" && !authorized {
                    let ok = auth.is_some_and(|a| {
                        let want = md5_hex(format!("{}:abc:{}", md5_hex(b"admin:cam:s3cret"), md5_hex(format!("{method}:rtsp://127.0.0.1:{}/live", c.local_addr().unwrap().port()).as_bytes())).as_bytes());
                        a.contains(&format!("response=\"{want}\""))
                    });
                    if !ok {
                        write!(c, "RTSP/1.0 401 Unauthorized\r\nCSeq: {cseq}\r\nWWW-Authenticate: Basic realm=\"cam\"\r\nWWW-Authenticate: Digest realm=\"cam\", nonce=\"abc\"\r\n\r\n").unwrap();
                        continue;
                    }
                    authorized = true;
                }
                let port = c.local_addr().unwrap().port();
                match method.as_str() {
                    "OPTIONS" => write!(c, "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nPublic: OPTIONS, DESCRIBE, SETUP, PLAY, TEARDOWN, GET_PARAMETER\r\n\r\n").unwrap(),
                    "DESCRIBE" => write!(c, "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nContent-Base: rtsp://127.0.0.1:{port}/live/\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{sdp}", sdp.len()).unwrap(),
                    "SETUP" => {
                        let t = req.lines().find_map(|l| l.strip_prefix("Transport: ")).unwrap().to_string();
                        let reply = match param(&t, "client_port") {
                            Some(p) => {
                                client_ports.push(p.split('-').next().unwrap().parse::<u16>().unwrap());
                                format!("{t};server_port={}-{}", udp.local_addr().unwrap().port(), udp.local_addr().unwrap().port() + 1)
                            }
                            None => t,
                        };
                        write!(c, "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: 12345678;timeout=2\r\nTransport: {reply}\r\n\r\n").unwrap();
                    }
                    "PLAY" => {
                        write!(c, "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\nSession: 12345678\r\nRTP-Info: url=rtsp://127.0.0.1:{port}/live/streamid=0;seq=1;rtptime={},url=rtsp://127.0.0.1:{port}/live/streamid=1;seq=1;rtptime={}\r\n\r\n", rtptimes[0], rtptimes[1]).unwrap();
                        for (k, f) in frames.iter().enumerate() {
                            if client_ports.is_empty() {
                                c.write_all(f).unwrap();
                            } else {
                                // RTP to the track's port, RTCP to the one above
                                let port = client_ports[f[1] as usize / 2] + (f[1] % 2) as u16;
                                udp.send_to(&f[4..], ("127.0.0.1", port)).unwrap();
                                if k % 8 == 0 {
                                    std::thread::sleep(Duration::from_millis(1));
                                }
                            }
                            if k == frames.len() / 2 {
                                // a reply in the middle of the media, as to a keepalive
                                c.write_all(b"RTSP/1.0 200 OK\r\nCSeq: 99\r\nContent-Length: 2\r\n\r\nok").unwrap();
                                std::thread::sleep(Duration::from_millis(1200));
                            }
                        }
                    }
                    "GET_PARAMETER" => write!(c, "RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\n\r\n").unwrap(),
                    "TEARDOWN" => return,
                    _ => write!(c, "RTSP/1.0 501 Not Implemented\r\nCSeq: {cseq}\r\n\r\n").unwrap(),
                }
            }
        }
    }

    #[test]
    fn plays_a_camera_with_digest_credentials() {
        plays_a_camera("");
    }

    #[test]
    fn plays_a_camera_over_udp() {
        plays_a_camera("?transport=udp");
    }

    fn plays_a_camera(options: &str) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let (tx, rx) = mpsc::channel();
        let cam = std::thread::spawn(move || camera(l, tx));
        let url = format!("rtsp://admin:s3cret@127.0.0.1:{port}/live{options}");
        let u = SourceUrl::parse(&url).unwrap();
        let shown = crate::url::redact(&url);
        let mut s = RtspSource::open(&u, shown.clone()).unwrap();
        assert_eq!(s.info().url, format!("rtsp://admin:***@127.0.0.1:{port}/live{options}"));
        let (_, _, frames) = fixture();
        let mut got = Vec::new();
        let mut buf = vec![0u8; 3000];
        let started = Instant::now();
        while started.elapsed() < Duration::from_secs(20) {
            match s.read(&mut buf, Duration::from_millis(50)).unwrap() {
                Chunk::Data(n) => got.extend_from_slice(&buf[..n]),
                Chunk::Idle if got.len() >= frames.iter().map(Vec::len).sum::<usize>() => break,
                Chunk::Idle => {}
                Chunk::End => break,
            }
        }
        // the description first, then exactly the frames the camera sent
        assert_eq!(&got[..2], &[b'$', 0xFF]);
        let n = 4 + u16::from_be_bytes([got[2], got[3]]) as usize;
        let desc: Description = serde_json::from_slice(&got[4..n]).unwrap();
        assert_eq!(desc.tracks.iter().map(|t| (t.media, t.channel, t.seq)).collect::<Vec<_>>(), [(0, 0, Some(1)), (1, 2, Some(1))]);
        // each track's frames in order (over UDP the tracks' sockets are read in turn, so the order
        // between tracks is not kept)
        let by_channel = |frames: &mut dyn Iterator<Item = Vec<u8>>| {
            let mut m: std::collections::BTreeMap<u8, Vec<Vec<u8>>> = Default::default();
            for f in frames {
                m.entry(f[1]).or_default().push(f);
            }
            m
        };
        let mut rest = &got[n..];
        let mut seen = Vec::new();
        while rest.len() >= 4 {
            let len = 4 + u16::from_be_bytes([rest[2], rest[3]]) as usize;
            seen.push(rest[..len].to_vec());
            rest = &rest[len..];
        }
        assert!(rest.is_empty());
        if options.is_empty() {
            assert!(seen.concat() == frames.concat(), "the frames pass through untouched and the reply is taken out");
        }
        assert!(by_channel(&mut seen.into_iter()) == by_channel(&mut frames.clone().into_iter()), "every track's frames, in order");
        drop(s);
        cam.join().unwrap();
        let reqs: Vec<String> = rx.try_iter().collect();
        let methods: Vec<&str> = reqs.iter().map(|r| r.split_whitespace().next().unwrap()).collect();
        assert_eq!(&methods[..2], ["OPTIONS", "DESCRIBE"]);
        assert!(methods.contains(&"PLAY") && methods.last() == Some(&"TEARDOWN"), "{methods:?}");
        assert!(methods.contains(&"GET_PARAMETER"), "a keepalive within the 2 s session timeout: {methods:?}");
        assert!(reqs.iter().any(|r| r.starts_with("SETUP rtsp://127.0.0.1:") && r.contains("/live/streamid=1 ")));
        assert_eq!(reqs.iter().any(|r| r.contains("client_port=")), !options.is_empty());
        // the password never goes over the wire, and Basic is not used when Digest is offered
        assert!(reqs.iter().all(|r| !r.contains("s3cret") && !r.contains("Basic")));
    }

    #[test]
    fn a_camera_outside_the_allow_list_is_not_dialled() {
        let u = SourceUrl::parse("rtsp://user:pw@8.8.8.8/live").unwrap();
        let e = RtspSource::open(&u, crate::url::redact("rtsp://user:pw@8.8.8.8/live")).err().unwrap();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
        assert!(!e.message.contains("pw@"), "{}", e.message);
    }
}
