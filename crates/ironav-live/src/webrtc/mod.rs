//! WebRTC ingest: a WHIP endpoint (RFC 9725) that a publisher (OBS, FFmpeg, GStreamer, a browser)
//! sends H.264 and Opus to, read as the RTP stream framing an RTSP source gives.
//!
//! `whip://[host]:port[/path][?token=SECRET]` listens for HTTP on `host:port` (loopback unless a
//! host is given) and for the media on UDP at the same port. `whips://` is the same endpoint with
//! its HTTP served over TLS 1.3 (IronSocketLayer's server, the FIPS 140-3 profile in FIPS mode),
//! the certificate chain and private key named by `cert=` and `key=` (PEM files, read once at
//! open); FIPS mode refuses `whip://`, whose signalling is plaintext. One publisher at a time: it POSTs its
//! SDP offer to the path, gets a `201 Created` with the answer and a `Location`, and DELETEs that
//! location to stop. Receive-only and ICE-lite with one host candidate, BUNDLE and `rtcp-mux`, so
//! everything arrives on that one port: STUN checks first (`stun`), then a DTLS 1.2 handshake in
//! which the publisher's certificate must match its offer's fingerprint (`dtls`), then SRTP and
//! SRTCP under the keys it exports (`srtp`).
//!
//! What it trusts: nothing from the network. With `?token=` every POST and DELETE must carry
//! `Authorization: Bearer SECRET` (compared in constant time); without one, anyone who can reach
//! the port may publish, which is why the endpoint binds loopback unless told otherwise. HTTP and
//! media are taken only from loopback or from addresses `IRONAV_NET_ALLOW` admits (see
//! [`crate::net`]); media only from an address that has passed a STUN check with the session's
//! credentials, and only once it decrypts and authenticates. Every buffer is bounded: the offer,
//! HTTP connections and headers, DTLS messages and fragments, SRTP streams, frames held for the
//! reader; a session that does not come up within [`SETUP_TIMEOUT`] is dropped and the endpoint
//! waits for another.
//!
//! The bytes it gives: the description (an SDP of the tracks taken, each on its channel, their
//! RTP clocks lined up by arrival), then each decrypted RTP packet on its track's channel and
//! each SRTCP compound packet on the channel after it, so `ironav_formats::rtp` depacketizes them
//! exactly as it does an RTSP camera's. Toward the publisher it sends RTCP receiver reports every
//! second, NACKs for small gaps in the video, and picture loss indications at the start (until a
//! keyframe comes) and after a large gap. The source ends when the publisher DELETEs its session or
//! closes DTLS, or when its media stops for [`MEDIA_TIMEOUT`].
//!
//! The other direction is [`WhipPublisher`], a WHIP client over the same STUN, DTLS, SRTP and SDP code.

mod dtls;
mod http;
mod packetize;
mod publisher;
mod sdp;
mod srtp;
mod stun;

pub use publisher::{WhipOptions, WhipPublisher};
#[cfg(test)]
mod tests;

use crate::net::Allow;
use crate::source::{Chunk, Source, SourceInfo};
use crate::url::SourceUrl;
use ironav_core::{Error, ErrorCode, Result};
use ironav_formats::rtp::{description_frame, frame, Description, TrackSetup};
use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr, TcpListener, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, Receiver, RecvTimeoutError, SyncSender};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How long a publisher has, from its POST, to finish ICE and DTLS.
pub(crate) const SETUP_TIMEOUT: Duration = Duration::from_secs(15);
/// How long the media may stop before the session counts as lost.
pub(crate) const MEDIA_TIMEOUT: Duration = Duration::from_secs(10);
const REPORT_EVERY: Duration = Duration::from_secs(1);
const PLI_EVERY: Duration = Duration::from_secs(1);
/// How long the description waits for every track's first packet, to line their clocks up.
const ALIGN_WAIT: Duration = Duration::from_millis(1500);
/// Frames held while it waits, at most.
const MAX_HELD: usize = 4096;
/// How long each turn of the endpoint's loop waits on its UDP socket.
const POLL: Duration = Duration::from_millis(5);
/// How long the endpoint still answers HTTP after its session ends, so a publisher that closes DTLS
/// before it DELETEs (a browser that closes its peer connection first) has its DELETE answered.
const LINGER: Duration = Duration::from_secs(2);
/// Addresses a session takes media from: those that passed a STUN check, at most this many.
const MAX_PEERS: usize = 4;
/// A video gap up to this many packets is NACKed; a larger one asks for a keyframe instead.
const MAX_NACK: u64 = 16;

/// What the endpoint's thread hands the reader.
enum Msg {
    Data(Vec<u8>),
    End,
    Error(Error),
}

pub(crate) struct WhipSource {
    info: SourceInfo,
    rx: Receiver<Msg>,
    held: Vec<u8>,
    at: usize,
    ended: bool,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl Drop for WhipSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        // drain, so a worker blocked on a full channel can see the flag
        while self.rx.try_recv().is_ok() {}
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

impl Source for WhipSource {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    fn read(&mut self, buf: &mut [u8], wait: Duration) -> Result<Chunk> {
        if self.at == self.held.len() {
            if self.ended {
                return Ok(Chunk::End);
            }
            match self.rx.recv_timeout(wait) {
                Ok(Msg::Data(b)) => (self.held, self.at) = (b, 0),
                Ok(Msg::End) | Err(RecvTimeoutError::Disconnected) => {
                    self.ended = true;
                    return Ok(Chunk::End);
                }
                Ok(Msg::Error(e)) => {
                    self.ended = true;
                    return Err(e);
                }
                Err(RecvTimeoutError::Timeout) => return Ok(Chunk::Idle),
            }
        }
        let n = buf.len().min(self.held.len() - self.at);
        buf[..n].copy_from_slice(&self.held[self.at..self.at + n]);
        self.at += n;
        Ok(Chunk::Data(n))
    }
}

/// `host:port/path`, the host optional (loopback) and in brackets for IPv6.
fn parse_target(target: &str, scheme: &str) -> Result<(String, u16, String)> {
    let bad = || Error::invalid_arg(format!("a WHIP source needs {scheme}://[host]:port[/path], got '{scheme}://{target}'"));
    let (hostport, path) = match target.find('/') {
        Some(i) => (&target[..i], &target[i..]),
        None => (target, "/"),
    };
    let (host, port) = hostport.rsplit_once(':').ok_or_else(bad)?;
    let port = port.parse::<u16>().map_err(|_| bad())?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    let host = if host.is_empty() { "127.0.0.1" } else { host };
    if path.len() > 256 || !path.bytes().all(|b| b.is_ascii_graphic() && b != b'?' && b != b'%') {
        return Err(bad());
    }
    let path = if path.len() > 1 { path.trim_end_matches('/') } else { path };
    Ok((host.to_string(), port, path.to_string()))
}

/// Opens the endpoint `u` names and starts listening.
pub(crate) fn open(u: &SourceUrl, shown: String) -> Result<WhipSource> {
    crate::fips::activate()?;
    let tls = u.scheme == "whips";
    let (host, port, path) = parse_target(&u.target, &u.scheme)?;
    let tls = if tls {
        let (Some(cert), Some(key)) = (u.option("cert"), u.option("key")) else {
            return Err(Error::invalid_arg("whips:// needs cert= and key=, PEM files of the TLS certificate chain and the private key").hint("whips://:8443/live?cert=chain.pem&key=key.pem, or use whip:// for plain HTTP signalling (refused in FIPS mode)"));
        };
        Some(crate::tls::server_config(cert, key)?)
    } else if u.option("cert").is_some() || u.option("key").is_some() {
        return Err(Error::invalid_arg("cert= and key= belong to whips://; whip:// serves plain HTTP").hint("use whips://"));
    } else {
        None
    };
    let token = u.option("token").map(crate::url::percent_decode);
    if token.as_ref().is_some_and(|t| t.is_empty() || t.len() > 512 || !t.bytes().all(|b| b.is_ascii_graphic())) {
        return Err(Error::invalid_arg("the WHIP token must be 1 to 512 printable characters, without spaces"));
    }
    let cors = match u.option("cors").map(crate::url::percent_decode) {
        None => None,
        Some(o) if o == "*" || is_origin(&o) => Some(o),
        Some(_) => return Err(Error::invalid_arg("cors= takes an origin, such as https://studio.example.com or http://localhost:3000, or *").hint("an origin is a scheme, a host and perhaps a port, without a path")),
    };
    let allow = Allow::from_env()?;
    let unreachable = |e: std::io::Error| Error::new(ErrorCode::Io, format!("could not listen on {shown}: {e}")).hint("check the address, and that nothing else uses the port (TCP for HTTP, UDP for the media)");
    let listener = TcpListener::bind((host.as_str(), port)).map_err(unreachable)?;
    let local = listener.local_addr()?;
    let udp = UdpSocket::bind(local).map_err(unreachable)?;
    grow_receive_buffer(&udp);
    let id = dtls::Identity::generate("IronAV WHIP").map_err(|e| Error::new(ErrorCode::Internal, format!("could not make the WHIP endpoint's DTLS certificate: {e}")))?;
    let http = http::Server::new(listener, allow.clone(), sdp::MAX_OFFER, tls)?;
    let (tx, rx) = sync_channel(1024);
    let stop = Arc::new(AtomicBool::new(false));
    let mut endpoint = Endpoint { shown: shown.clone(), path, token, cors, http, udp, id: Arc::new(id), allow, session: None, tx, stop: stop.clone(), start: Instant::now() };
    let worker = std::thread::Builder::new().name("ironav-whip".into()).spawn(move || endpoint.run()).map_err(|e| Error::new(ErrorCode::Internal, format!("could not start the WHIP endpoint: {e}")))?;
    Ok(WhipSource { info: SourceInfo { url: shown, kind: "whip", live: true, synthetic: false }, rx, held: Vec::new(), at: 0, ended: false, stop, worker: Some(worker) })
}

/// Receive statistics for one SSRC (RFC 3550 §6.4.1, Appendix A.3 and A.8).
#[derive(Debug, Default)]
struct Stats {
    max: Option<u64>,
    base: u64,
    received: u64,
    expected_prior: u64,
    received_prior: u64,
    transit: Option<i64>,
    jitter: f64,
    /// The middle of the last sender report's NTP time, and when it came.
    lsr: Option<(u32, Instant)>,
}

impl Stats {
    /// Counts a packet, returning its extended sequence number and how many went missing just
    /// before it.
    fn packet(&mut self, seq: u16, ts: u32, arrival: f64, clock: u32) -> (u64, u64) {
        let ext = match self.max {
            None => {
                self.base = seq as u64 + (1 << 16);
                seq as u64 + (1 << 16)
            }
            Some(m) => {
                let c = (m & !0xFFFF) | seq as u64;
                if c + 0x8000 < m {
                    c + 0x10000
                } else if c > m + 0x8000 && c >= 0x10000 {
                    c - 0x10000
                } else {
                    c
                }
            }
        };
        let gap = match self.max {
            Some(m) if ext > m => ext - m - 1,
            _ => 0,
        };
        if self.max.is_none_or(|m| ext > m) {
            self.max = Some(ext);
        }
        self.received += 1;
        let transit = (arrival * clock as f64) as i64 - ts as i64;
        if let Some(prev) = self.transit {
            let d = (transit - prev).unsigned_abs() as f64;
            self.jitter += (d - self.jitter) / 16.0;
        }
        self.transit = Some(transit);
        (ext, gap)
    }

    /// A report block for `ssrc`.
    fn report(&mut self, ssrc: u32, now: Instant) -> Option<Vec<u8>> {
        let max = self.max?;
        let expected = max - self.base + 1;
        let lost = expected as i64 - self.received as i64;
        let (de, dr) = (expected - self.expected_prior, self.received - self.received_prior);
        (self.expected_prior, self.received_prior) = (expected, self.received);
        let fraction = if de == 0 || dr >= de { 0 } else { (((de - dr) << 8) / de) as u8 };
        let mut b = ssrc.to_be_bytes().to_vec();
        b.push(fraction);
        b.extend_from_slice(&(lost.clamp(-0x80_0000, 0x7F_FFFF) as i32).to_be_bytes()[1..]);
        b.extend_from_slice(&((max - (1 << 16)) as u32).to_be_bytes());
        b.extend_from_slice(&(self.jitter as u32).to_be_bytes());
        let (lsr, dlsr) = match self.lsr {
            Some((l, at)) => (l, (now.duration_since(at).as_secs_f64() * 65536.0) as u32),
            None => (0, 0),
        };
        b.extend_from_slice(&lsr.to_be_bytes());
        b.extend_from_slice(&dlsr.to_be_bytes());
        Some(b)
    }
}

/// A track of the session as the media arrives.
struct Track {
    t: sdp::Track,
    channel: u8,
    ssrc: Option<u32>,
    stats: Stats,
    /// The first packet's RTP time and arrival, to line the clocks up.
    first: Option<(u32, Instant)>,
}

/// Whether an H.264 RTP payload starts a keyframe (an IDR slice or a sequence parameter set,
/// alone, first in a STAP-A, or starting a FU-A).
fn h264_key(p: &[u8]) -> bool {
    let nal = |t: u8| matches!(t & 31, 5 | 7);
    match p.first().map(|b| b & 31) {
        Some(24) => p.get(3).is_some_and(|b| nal(*b)),
        Some(28) => p.get(1).is_some_and(|b| b & 0x80 != 0 && nal(*b)),
        Some(t) => nal(t),
        None => false,
    }
}

struct Session {
    id: String,
    offer: sdp::Offer,
    ufrag: String,
    pwd: String,
    created: Instant,
    last_heard: Instant,
    peers: VecDeque<SocketAddr>,
    selected: Option<SocketAddr>,
    dtls: dtls::Server,
    srtp_in: Option<srtp::Context>,
    srtp_out: Option<srtp::Context>,
    tracks: Vec<Track>,
    /// Frames waiting for the description, once SRTP is up; `None` once it has gone out.
    held: Option<Vec<Vec<u8>>>,
    connected_at: Option<Instant>,
    ssrc: u32,
    keyframe: bool,
    last_pli: Option<Instant>,
    last_report: Instant,
}

impl Session {
    /// The video's SSRC, when a keyframe is wanted from it (none has come since the start or the
    /// last large loss) and the last request was long enough ago.
    fn wants_key(&self, now: Instant) -> Option<u32> {
        if self.keyframe || self.last_pli.is_some_and(|t| now.duration_since(t) < PLI_EVERY) {
            return None;
        }
        self.tracks.iter().find(|t| t.t.kind == sdp::Kind::Video)?.ssrc
    }
}

/// Whether `o` is a web origin: `http` or `https`, a host of letters, digits, dots, hyphens (or an
/// IPv6 address in brackets) and perhaps a port, and nothing more, so it is safe in a header.
fn is_origin(o: &str) -> bool {
    let Some(rest) = o.strip_prefix("https://").or_else(|| o.strip_prefix("http://")) else { return false };
    let (host, port) = match rest.rsplit_once(':') {
        Some((h, p)) if !h.is_empty() && !p.is_empty() && !h.ends_with(':') && p.bytes().all(|b| b.is_ascii_digit()) && (!h.contains(':') || h.starts_with('[')) => (h, Some(p)),
        _ => (rest, None),
    };
    let host_ok = if let Some(v6) = host.strip_prefix('[').and_then(|h| h.strip_suffix(']')) {
        !v6.is_empty() && v6.bytes().all(|b| b.is_ascii_hexdigit() || b == b':' || b == b'.')
    } else {
        !host.is_empty() && host.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    };
    o.len() <= 253 && host_ok && port.is_none_or(|p| p.len() <= 5)
}

/// Adds the CORS headers for `origin` to a whole response, after its status line, so a web page
/// there can publish: its preflight is answered, and it may read the `Location` it is given.
fn with_cors(resp: Vec<u8>, origin: &str) -> Vec<u8> {
    let Some(at) = resp.windows(2).position(|w| w == b"\r\n") else { return resp };
    let vary = if origin == "*" { "" } else { "Vary: Origin\r\n" };
    let headers = format!(
        "\r\nAccess-Control-Allow-Origin: {origin}\r\n{vary}Access-Control-Allow-Methods: OPTIONS, POST, DELETE, PATCH\r\nAccess-Control-Allow-Headers: Authorization, Content-Type, If-Match\r\nAccess-Control-Expose-Headers: Location, Link, ETag, Accept-Post, Accept-Patch\r\nAccess-Control-Max-Age: 600"
    );
    let mut out = Vec::with_capacity(resp.len() + headers.len());
    out.extend_from_slice(&resp[..at]);
    out.extend_from_slice(headers.as_bytes());
    out.extend_from_slice(&resp[at..]);
    out
}

/// Random text from the ICE character set, for credentials and the session's id.
fn random_text(n: usize) -> String {
    const CHARS: &[u8] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789";
    let bytes = crate::auth::random_bytes::<64>();
    bytes.iter().take(n).map(|b| CHARS[*b as usize % CHARS.len()] as char).collect()
}

struct Endpoint {
    shown: String,
    path: String,
    token: Option<String>,
    /// The origin a web page publishing from may have (`*` for any), for the CORS headers.
    cors: Option<String>,
    http: http::Server,
    udp: UdpSocket,
    id: Arc<dtls::Identity>,
    allow: Allow,
    session: Option<Session>,
    tx: SyncSender<Msg>,
    stop: Arc<AtomicBool>,
    start: Instant,
}

/// What a turn of the loop decided.
enum Flow {
    Go,
    /// The source ends, with an error or without.
    End(Option<Error>),
}

impl Endpoint {
    fn run(&mut self) {
        let mut buf = vec![0u8; 65536];
        loop {
            if self.stop.load(Ordering::Relaxed) {
                break;
            }
            let now = Instant::now();
            for (req, peer, responder) in self.http.poll(now) {
                let (resp, flow) = self.route(&req, peer, now);
                responder.send(&self.cors(resp));
                if let Flow::End(e) = flow {
                    // a DELETE has had its answer; nothing is left to linger for
                    return self.finish(e, req.method != "DELETE");
                }
            }
            // a burst of datagrams, then the timers
            for _ in 0..256 {
                match crate::net::recv_from_within(&self.udp, &mut buf, POLL) {
                    Ok((n, from)) => {
                        if let Flow::End(e) = self.datagram(&buf[..n], from, Instant::now()) {
                            return self.finish(e, true);
                        }
                    }
                    Err(_) => break, // timed out, or an ICMP error from an earlier send
                }
            }
            if let Flow::End(e) = self.timers(Instant::now()) {
                return self.finish(e, true);
            }
        }
        self.finish(None, false);
    }

    /// Ends the source: says goodbye to the publisher, and tells the reader. Then, if `linger`, for
    /// [`LINGER`] a DELETE of the session that ended is still answered (anything else is told it has
    /// gone).
    fn finish(&mut self, e: Option<Error>, linger: bool) {
        let mut ended = None;
        if let Some(s) = &mut self.session {
            if let Some(to) = s.selected {
                for d in s.dtls.close_notify() {
                    let _ = self.udp.send_to(&d, to);
                }
            }
            ended = Some(format!("{}/session/{}", self.path.trim_end_matches('/'), s.id));
        }
        self.session = None;
        let _ = self.send(match e {
            Some(e) => Msg::Error(e),
            None => Msg::End,
        });
        let Some(ended) = ended.filter(|_| linger) else { return };
        let until = Instant::now() + LINGER;
        while Instant::now() < until && !self.stop.load(Ordering::Relaxed) {
            for (req, _, responder) in self.http.poll(Instant::now()) {
                if req.method == "DELETE" && req.path == ended && self.authorized(&req) {
                    responder.send(&self.cors(http::response(200, &[], b"")));
                    return;
                }
                responder.send(&self.cors(http::response(404, &[("Content-Type", "text/plain")], b"the session has ended")));
            }
            std::thread::sleep(POLL);
        }
    }

    fn cors(&self, resp: Vec<u8>) -> Vec<u8> {
        match &self.cors {
            Some(o) => with_cors(resp, o),
            None => resp,
        }
    }

    fn send(&self, m: Msg) -> Flow {
        // blocks while the reader is behind; gives up once it has gone
        let mut m = m;
        loop {
            match self.tx.try_send(m) {
                Ok(()) => return Flow::Go,
                Err(std::sync::mpsc::TrySendError::Full(back)) => {
                    if self.stop.load(Ordering::Relaxed) {
                        return Flow::End(None);
                    }
                    m = back;
                    std::thread::sleep(Duration::from_millis(1));
                }
                Err(std::sync::mpsc::TrySendError::Disconnected(_)) => return Flow::End(None),
            }
        }
    }

    fn authorized(&self, req: &http::Request) -> bool {
        let Some(token) = &self.token else { return true };
        let given = req.header("authorization").and_then(|a| a.strip_prefix("Bearer ").or_else(|| a.strip_prefix("bearer "))).unwrap_or("").trim();
        // the comparison takes the same time whatever matches; only the lengths may differ
        let (a, b) = (crate::webrtc::dtls::sha256(given.as_bytes()), crate::webrtc::dtls::sha256(token.as_bytes()));
        ic_core::ct::verify(&a, &b)
    }

    fn route(&mut self, req: &http::Request, peer: SocketAddr, now: Instant) -> (Vec<u8>, Flow) {
        let text = |status: u16, why: &str| http::response(status, &[("Content-Type", "text/plain")], why.as_bytes());
        let session_path = self.session.as_ref().map(|s| format!("{}/session/{}", self.path.trim_end_matches('/'), s.id));
        if req.method == "OPTIONS" {
            return (http::response(204, &[("Accept-Post", "application/sdp"), ("Allow", "OPTIONS, POST, DELETE, PATCH")], b""), Flow::Go);
        }
        if !self.authorized(req) {
            return (http::response(401, &[("WWW-Authenticate", "Bearer"), ("Content-Type", "text/plain")], b"a bearer token is required"), Flow::Go);
        }
        if Some(&req.path) == session_path.as_ref() {
            return match req.method.as_str() {
                "DELETE" => (http::response(200, &[], b""), Flow::End(None)),
                // trickle ICE and ICE restarts are not offered (RFC 9725 §4.3.1)
                "PATCH" => (http::response(405, &[("Allow", "DELETE")], b""), Flow::Go),
                _ => (http::response(405, &[("Allow", "DELETE, PATCH")], b""), Flow::Go),
            };
        }
        if req.path != self.path {
            return (text(404, "no WHIP endpoint here"), Flow::Go);
        }
        if req.method != "POST" {
            return (http::response(405, &[("Allow", "OPTIONS, POST")], b""), Flow::Go);
        }
        if !req.header("content-type").is_some_and(|c| c.to_ascii_lowercase().starts_with("application/sdp")) {
            return (text(415, "the offer must be application/sdp"), Flow::Go);
        }
        if self.session.as_ref().is_some_and(|s| s.connected_at.is_some()) {
            return (text(409, "another publisher is sending to this endpoint"), Flow::Go);
        }
        let Ok(body) = std::str::from_utf8(&req.body) else { return (text(400, "the offer is not text"), Flow::Go) };
        let offer = match sdp::parse_offer(body) {
            Ok(o) => o,
            Err(why) => return (text(400, &why), Flow::Go),
        };
        let dtls = match dtls::Server::new(self.id.clone(), offer.fingerprint) {
            Ok(d) => d,
            Err(e) => return (text(500, &e), Flow::Go),
        };
        // the host candidate: the address the publisher reached, when listening on every address
        let mut candidate = self.http.local_addr().unwrap_or(peer);
        if candidate.ip().is_unspecified() {
            candidate.set_ip(local_ip_toward(peer.ip()).unwrap_or(candidate.ip()));
        }
        let (ufrag, pwd, id) = (random_text(8), random_text(24), random_text(16));
        let ssrc = u32::from_be_bytes(crate::auth::random_bytes::<4>());
        let local = sdp::Local { ufrag: &ufrag, pwd: &pwd, fingerprint: &self.id.fingerprint, candidate, session_id: u64::from_be_bytes(crate::auth::random_bytes::<8>()) >> 1 };
        let answer = sdp::answer(&offer, &local);
        let tracks = offer.tracks().enumerate().map(|(i, t)| Track { t: t.clone(), channel: 2 * i as u8, ssrc: None, stats: Stats::default(), first: None }).collect();
        let location = format!("{}/session/{id}", self.path.trim_end_matches('/'));
        // a session still setting up gives way to the new offer
        self.session = Some(Session {
            id,
            offer,
            ufrag,
            pwd,
            created: now,
            last_heard: now,
            peers: VecDeque::new(),
            selected: None,
            dtls,
            srtp_in: None,
            srtp_out: None,
            tracks,
            held: None,
            connected_at: None,
            ssrc,
            keyframe: false,
            last_pli: None,
            last_report: now,
        });
        (http::response(201, &[("Content-Type", "application/sdp"), ("Location", &location)], answer.as_bytes()), Flow::Go)
    }

    fn permitted(&self, ip: IpAddr) -> bool {
        ip.is_loopback() || self.allow.permits(ip)
    }

    fn datagram(&mut self, d: &[u8], from: SocketAddr, now: Instant) -> Flow {
        if !self.permitted(from.ip()) {
            return Flow::Go;
        }
        let Some(s) = &mut self.session else { return Flow::Go };
        match d.first() {
            Some(0..=3) => {
                let Some(b) = stun::check_request(d, &s.ufrag, &s.offer.ufrag, &s.pwd) else { return Flow::Go };
                let _ = self.udp.send_to(&stun::success(&b.txid, from, &s.pwd), from);
                if !s.peers.contains(&from) {
                    if s.peers.len() == MAX_PEERS {
                        s.peers.pop_front();
                    }
                    s.peers.push_back(from);
                }
                if b.use_candidate || s.selected.is_none() {
                    s.selected = Some(from);
                }
                s.last_heard = now;
                Flow::Go
            }
            Some(20..=63) if s.peers.contains(&from) => {
                let mut addr = from.ip().to_string().into_bytes();
                addr.extend_from_slice(&from.port().to_be_bytes());
                match s.dtls.handle(d, &addr, now) {
                    Ok(out) => {
                        for o in out {
                            let _ = self.udp.send_to(&o, from);
                        }
                    }
                    Err((why, alert)) => {
                        for o in alert {
                            let _ = self.udp.send_to(&o, from);
                        }
                        // the attempt fails; the endpoint waits for another
                        eprintln!("ironav: WHIP publisher at {from} failed: {why}");
                        self.session = None;
                        return Flow::Go;
                    }
                }
                if s.dtls.closed() {
                    return if s.connected_at.is_some() { Flow::End(None) } else { self.drop_session() };
                }
                if s.srtp_in.is_none() {
                    if let Some(k) = s.dtls.keys().cloned() {
                        s.selected = Some(from);
                        return self.connected(&k, now);
                    }
                }
                Flow::Go
            }
            Some(128..=191) if s.peers.contains(&from) => self.media(d, now),
            _ => Flow::Go,
        }
    }

    fn drop_session(&mut self) -> Flow {
        self.session = None;
        Flow::Go
    }

    /// DTLS is done: SRTP from the exported keys, and a keyframe asked for.
    fn connected(&mut self, k: &dtls::SrtpKeys, now: Instant) -> Flow {
        let Some(s) = &mut self.session else { return Flow::Go };
        match (srtp::Context::new(k.profile, &k.client_key, &k.client_salt), srtp::Context::new(k.profile, &k.server_key, &k.server_salt)) {
            (Ok(i), Ok(o)) => (s.srtp_in, s.srtp_out) = (Some(i), Some(o)),
            _ => return self.drop_session(),
        }
        s.connected_at = Some(now);
        s.last_heard = now;
        s.held = Some(Vec::new());
        self.feedback(now, &[]);
        Flow::Go
    }

    fn media(&mut self, d: &[u8], now: Instant) -> Flow {
        let start = self.start;
        let Some(s) = &mut self.session else { return Flow::Go };
        let (Some(rx), Some(_)) = (&mut s.srtp_in, &s.srtp_out) else { return Flow::Go };
        // RTCP's packet types sit where RTP's marker and payload type would be 64 to 95 (RFC 5761)
        let rtcp = d.len() >= 2 && (192..=223).contains(&d[1]);
        let mut frames = Vec::new();
        let mut nacks = Vec::new();
        if rtcp {
            let Ok(p) = rx.unprotect_rtcp(d) else { return Flow::Go };
            s.last_heard = now;
            let ssrc = u32::from_be_bytes([p[4], p[5], p[6], p[7]]);
            // sender reports tie the track's clock to the sender's; their NTP middle goes back in reports
            let mut at = 0;
            while let Some(h) = p.get(at..at + 4) {
                let len = 4 * (u16::from_be_bytes([h[2], h[3]]) as usize + 1);
                if h[1] == 200 {
                    if let (Some(sender), Some(ntp)) = (p.get(at + 4..at + 8), p.get(at + 10..at + 14)) {
                        let sender = u32::from_be_bytes([sender[0], sender[1], sender[2], sender[3]]);
                        if let Some(t) = s.tracks.iter_mut().find(|t| t.ssrc == Some(sender)) {
                            t.stats.lsr = Some((u32::from_be_bytes([ntp[0], ntp[1], ntp[2], ntp[3]]), now));
                        }
                    }
                }
                at += len;
            }
            if let Some(t) = s.tracks.iter().find(|t| t.ssrc == Some(ssrc)) {
                if let Ok(f) = frame(t.channel + 1, &p) {
                    frames.push(f);
                }
            }
        } else {
            let Ok(p) = rx.unprotect_rtp(d) else { return Flow::Go };
            s.last_heard = now;
            let (pt, seq) = (p[1] & 0x7F, u16::from_be_bytes([p[2], p[3]]));
            let ts = u32::from_be_bytes([p[4], p[5], p[6], p[7]]);
            let ssrc = u32::from_be_bytes([p[8], p[9], p[10], p[11]]);
            let Some(t) = s.tracks.iter_mut().find(|t| t.t.payload == pt) else { return Flow::Go };
            // the first SSRC a track hears is its own (a retransmission stream would need rtx, which is not offered)
            if *t.ssrc.get_or_insert(ssrc) != ssrc {
                return Flow::Go;
            }
            t.first.get_or_insert((ts, now));
            let (ext, gap) = t.stats.packet(seq, ts, now.duration_since(start).as_secs_f64(), t.t.clock);
            if t.t.kind == sdp::Kind::Video {
                if gap > 0 && gap <= MAX_NACK {
                    nacks.push((ssrc, ext - gap, gap));
                } else if gap > MAX_NACK {
                    s.keyframe = false;
                    s.last_pli = None;
                }
                let h = srtp::rtp_header_len(&p).unwrap_or(p.len());
                if h264_key(&p[h..]) {
                    s.keyframe = true;
                }
            }
            if let Ok(f) = frame(t.channel, &p) {
                frames.push(f);
            }
        }
        match &mut s.held {
            Some(held) => {
                held.extend(frames);
                if held.len() >= MAX_HELD || s.tracks.iter().all(|t| t.first.is_some()) {
                    return self.describe();
                }
                Flow::Go
            }
            None => {
                if !nacks.is_empty() {
                    self.feedback(now, &nacks);
                }
                for f in frames {
                    if let Flow::End(e) = self.send(Msg::Data(f)) {
                        return Flow::End(e);
                    }
                }
                Flow::Go
            }
        }
    }

    /// Sends the description, the tracks' clocks lined up by when their first packets came, and
    /// the frames held for it.
    fn describe(&mut self) -> Flow {
        let Some(s) = &mut self.session else { return Flow::Go };
        let Some(held) = s.held.take() else { return Flow::Go };
        let t0 = s.tracks.iter().filter_map(|t| t.first.map(|f| f.1)).min();
        let tracks = s
            .tracks
            .iter()
            .enumerate()
            .map(|(i, t)| TrackSetup {
                media: i,
                channel: t.channel,
                rtptime: t.first.zip(t0).map(|((ts, at), t0)| ts.wrapping_sub((at.duration_since(t0).as_secs_f64() * t.t.clock as f64) as u32)),
                seq: None,
            })
            .collect();
        let desc = match description_frame(&Description { sdp: sdp::description(&s.offer), tracks }) {
            Ok(d) => d,
            Err(e) => return Flow::End(Some(e)),
        };
        for f in std::iter::once(desc).chain(held) {
            if let Flow::End(e) = self.send(Msg::Data(f)) {
                return Flow::End(e);
            }
        }
        Flow::Go
    }

    /// Receiver reports, with a picture loss indication when a keyframe is wanted and NACKs for
    /// `nacks` (SSRC, first missing extended sequence number, how many).
    fn feedback(&mut self, now: Instant, nacks: &[(u32, u64, u64)]) {
        let Some(s) = &mut self.session else { return };
        let Some(to) = s.selected else { return };
        if s.srtp_out.is_none() {
            return;
        }
        let ours = s.ssrc;
        let blocks: Vec<Vec<u8>> = s.tracks.iter_mut().filter_map(|t| t.stats.report(t.ssrc?, now)).take(31).collect();
        let mut p = vec![0x80 | blocks.len() as u8, 201];
        p.extend_from_slice(&((1 + 6 * blocks.len()) as u16).to_be_bytes());
        p.extend_from_slice(&ours.to_be_bytes());
        for b in &blocks {
            p.extend_from_slice(b);
        }
        // SDES with a CNAME, as a compound packet must have (RFC 3550 §6.1)
        let mut sdes = ours.to_be_bytes().to_vec();
        sdes.extend_from_slice(&[1, 6]);
        sdes.extend_from_slice(b"IronAV");
        sdes.resize(sdes.len().div_ceil(4) * 4 + if sdes.len() % 4 == 0 { 4 } else { 0 }, 0);
        p.extend_from_slice(&[0x81, 202]);
        p.extend_from_slice(&((sdes.len() / 4) as u16).to_be_bytes());
        p.extend_from_slice(&sdes);
        if let Some(video) = s.wants_key(now) {
            s.last_pli = Some(now);
            p.extend_from_slice(&[0x81, 206, 0, 2]);
            p.extend_from_slice(&ours.to_be_bytes());
            p.extend_from_slice(&video.to_be_bytes());
        }
        for &(ssrc, first, n) in nacks {
            let pid = (first - (1 << 16)) as u16;
            let blp = (1..n.min(17)).fold(0u16, |b, k| b | 1 << (k - 1));
            p.extend_from_slice(&[0x81, 205, 0, 3]);
            p.extend_from_slice(&ours.to_be_bytes());
            p.extend_from_slice(&ssrc.to_be_bytes());
            p.extend_from_slice(&pid.to_be_bytes());
            p.extend_from_slice(&blp.to_be_bytes());
        }
        s.last_report = now;
        if let Some(Ok(d)) = s.srtp_out.as_mut().map(|tx| tx.protect_rtcp(&p)) {
            let _ = self.udp.send_to(&d, to);
        }
    }

    fn timers(&mut self, now: Instant) -> Flow {
        let Some(s) = &mut self.session else { return Flow::Go };
        match s.connected_at {
            None => {
                if now.duration_since(s.created) > SETUP_TIMEOUT {
                    eprintln!("ironav: WHIP publisher did not connect within {} s", SETUP_TIMEOUT.as_secs());
                    return self.drop_session();
                }
                match s.dtls.poll(now) {
                    Ok(out) => {
                        if let Some(to) = s.selected {
                            for o in out {
                                let _ = self.udp.send_to(&o, to);
                            }
                        }
                    }
                    Err(why) => {
                        eprintln!("ironav: WHIP publisher failed: {why}");
                        return self.drop_session();
                    }
                }
                Flow::Go
            }
            Some(at) => {
                if now.duration_since(s.last_heard) > MEDIA_TIMEOUT {
                    return Flow::End(Some(Error::new(ErrorCode::Io, format!("the WHIP publisher at {} sent nothing for {} s", self.shown, MEDIA_TIMEOUT.as_secs())).hint("the publisher stopped or the network dropped; open the source again to take another")));
                }
                if s.held.is_some() && now.duration_since(at) >= ALIGN_WAIT {
                    if let Flow::End(e) = self.describe() {
                        return Flow::End(e);
                    }
                }
                let Some(s) = &self.session else { return Flow::Go };
                if s.wants_key(now).is_some() || now.duration_since(s.last_report) >= REPORT_EVERY {
                    self.feedback(now, &[]);
                }
                Flow::Go
            }
        }
    }
}

/// Asks for a larger receive buffer on the media socket than the system's default (64 KiB on
/// Windows), so a keyframe's burst of packets is not dropped while the endpoint is busy. Best
/// effort: the system may grant less, and elsewhere its default stays.
fn grow_receive_buffer(sock: &UdpSocket) {
    const SIZE: i32 = 4 << 20;
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawSocket;
        use windows::Win32::Networking::WinSock::{setsockopt, SOCKET, SOL_SOCKET, SO_RCVBUF};
        // SAFETY: the socket is open for the call, and SO_RCVBUF takes an int
        unsafe { setsockopt(SOCKET(sock.as_raw_socket() as usize), SOL_SOCKET, SO_RCVBUF, Some(&SIZE.to_ne_bytes())) };
    }
    #[cfg(target_os = "linux")]
    {
        use std::os::fd::AsRawFd;
        // SAFETY: the socket is open for the call, and SO_RCVBUF takes an int
        unsafe { libc::setsockopt(sock.as_raw_fd(), libc::SOL_SOCKET, libc::SO_RCVBUF, (&SIZE as *const i32).cast(), std::mem::size_of::<i32>() as libc::socklen_t) };
    }
    #[cfg(not(any(windows, target_os = "linux")))]
    let _ = (sock, SIZE);
}

/// The local address the system would send to `peer` from: the host candidate when the endpoint
/// listens on every address.
fn local_ip_toward(peer: IpAddr) -> Option<IpAddr> {
    let any: SocketAddr = if peer.is_ipv4() { "0.0.0.0:0".parse().ok()? } else { "[::]:0".parse().ok()? };
    let s = UdpSocket::bind(any).ok()?;
    s.connect((peer, 9)).ok()?;
    Some(s.local_addr().ok()?.ip())
}
