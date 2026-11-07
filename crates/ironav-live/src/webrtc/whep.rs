//! A send-only WHEP endpoint: a browser (or any WHEP client) POSTs an SDP offer with `recvonly` video,
//! gets a `201 Created` with a `sendonly` answer and a `Location`, and DELETEs that location to leave.
//! The owner of the [`WhepServer`] pushes H.264 access units, and each viewer whose ICE and DTLS have
//! finished gets them as SRTP.
//!
//! It is the WHIP endpoint's server side turned around: the same HTTP server (`http`), ICE-lite
//! with one host candidate (`stun`), DTLS 1.2 server whose peer certificate must match the offer's
//! fingerprint (`dtls`) and SRTP (`srtp`), and the publisher's packetizer, RTP stream and NACK history
//! (`packetize`, `publisher::Stream`). Video only: audio sections of an offer are declined. The offer's
//! H.264 must be `packetization-mode=1`; of several, Constrained Baseline 42e01f is taken.
//!
//! `whep://[host]:port[/path][?token=SECRET]` is the URL form, `wheps://` serves the signalling over
//! TLS with `cert=` and `key=` as `whips://` does, and FIPS mode refuses `whep://`. Several viewers may
//! watch at once, [`WhepOptions::max_viewers`] at most (4 by default); each has its own SSRC, sequence
//! numbers, SRTP keys and history for NACKs. A viewer that has not finished ICE and DTLS within
//! [`WhepOptions::setup_timeout`], or that sends nothing authentic for [`WhepOptions::consent_timeout`], is dropped.
//!
//! One origin can serve more than the signalling. With [`WhepOptions::fallback`] set, a request whose
//! path is not the WHEP path or one of its session resources is handed to the caller as an
//! [`HttpRequest`] and an [`HttpReply`]: the caller answers with a page, or upgrades the connection to a
//! [WebSocket](super::websocket) and runs it on a thread of its own. Upgraded connections are counted
//! apart from the HTTP server's small pool, [`WhepOptions::max_websockets`] at most.
//!
//! What it trusts: nothing from the network. The caller's `authorize` function sees each request's
//! `Authorization` header and nothing is logged of it; a refusal is a `401` with no body. It covers
//! the WHEP path and its sessions only: the paths of the fallback are the caller's to protect. HTTP and
//! media are taken only from loopback or from addresses `IRONAV_NET_ALLOW` admits; media only from an
//! address that has passed a STUN check with the viewer's credentials, and only once it authenticates.
//!
//! Toward a viewer it sends RTCP sender reports every second. From it, it takes NACKs, answered from
//! the last 512 packets, and picture loss indications and full intra requests, which raise a flag
//! the owner reads with [`WhepServer::keyframe_requested`]. The flag is raised too when a viewer
//! connects and until it has a keyframe, and a viewer is sent nothing before the first keyframe.

use super::dtls;
use super::http;
use super::packetize::{self, H264Packetizer};
use super::publisher::Stream;
use super::sdp;
use super::srtp;
use super::stun;
use super::websocket::{self, Refused};
use crate::net::Allow;
use crate::url::SourceUrl;
use ironav_core::{Error, ErrorCode, Result};
use ironsocketlayer::config::ServerConfig;
use std::collections::VecDeque;
use std::net::{IpAddr, SocketAddr, TcpListener, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// How long a viewer has, from its POST, to finish ICE and DTLS, unless the options say otherwise.
const SETUP_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a connected viewer may send nothing authentic, unless the options say otherwise (consent
/// checks come every 5 s, RFC 7675).
const CONSENT_TIMEOUT: Duration = Duration::from_secs(30);
const SR_EVERY: Duration = Duration::from_secs(1);
/// How often a keyframe is asked for again while a viewer waits for one.
const KEY_EVERY: Duration = Duration::from_secs(1);
/// A burst of this many packets is followed by a millisecond's pause, so a keyframe's packets do not
/// overflow the viewer's socket buffer.
const BURST: usize = 40;
/// How long each turn of the endpoint's loop waits on its UDP socket.
const POLL: Duration = Duration::from_millis(5);
/// Addresses a viewer's media is taken from: those that passed a STUN check, at most this many.
const MAX_PEERS: usize = 4;
/// The most viewers a server may be set to take.
const MAX_VIEWERS: usize = 64;
/// The most upgraded connections a server may be set to take.
const MAX_WEBSOCKETS: usize = 1024;
/// How long a read from an upgraded connection waits before giving up, so its owner can look at
/// the clock.
const UPGRADED_SLICE: Duration = Duration::from_millis(250);
const CNAME: &str = "IronAV";

/// The check a request's `Authorization` header must pass: it gets the header's value, if there is one,
/// and says whether to go on.
pub type Authorize = dyn Fn(Option<&str>) -> bool + Send + Sync;

/// What serves the requests that are not WHEP's, see [`WhepOptions::fallback`].
pub type Fallback = dyn Fn(HttpRequest, HttpReply) + Send + Sync;

/// How a WHEP endpoint is set up.
#[non_exhaustive]
#[derive(Clone)]
pub struct WhepOptions {
    /// The most viewers at once, 1 to 64 (4 by default). A POST past it is answered `503`.
    pub max_viewers: usize,
    /// Checks every POST and DELETE's `Authorization` header (the whole value, such as `Bearer abc`).
    /// Without one anyone who can reach the port may watch, which is why the endpoint binds loopback
    /// unless told otherwise.
    ///
    /// It does not cover the requests the `fallback` gets: that code authenticates its own.
    pub authorize: Option<Arc<Authorize>>,
    /// Serves every request whose path is not the WHEP path or one of its session resources, such as
    /// the page of a viewer and its WebSocket. Without it those paths are answered `404`.
    ///
    /// It is called on the endpoint's thread, which also carries the signalling, so it should answer
    /// at once; code that waits or runs long (a WebSocket's loop) must take the [`HttpReply`] or the
    /// [`websocket::Connection`] onto a thread of its own. It runs before `authorize` is looked at and
    /// the CORS headers are not added to its answers. A request's body is at most 64 KiB. If the
    /// function panics, or drops its reply unanswered, the client gets a `500`.
    pub fallback: Option<Arc<Fallback>>,
    /// The most upgraded connections (WebSockets) open at once, 0 to 1024 (8 by default). They do not
    /// use up the HTTP server's connections: a past-the-limit upgrade is answered `503`.
    pub max_websockets: usize,
    /// The web origin (or `*`) a page that watches from another origin has, for the CORS headers.
    pub cors: Option<String>,
    /// How long a viewer has to finish ICE and DTLS (5 s by default).
    pub setup_timeout: Duration,
    /// How long a connected viewer may send nothing authentic (its consent checks and RTCP) before it is
    /// dropped (30 s by default).
    pub consent_timeout: Duration,
    /// Serve the signalling over TLS with this configuration.
    pub(crate) tls: Option<Arc<ServerConfig>>,
}

impl Default for WhepOptions {
    fn default() -> Self {
        WhepOptions { max_viewers: 4, authorize: None, fallback: None, max_websockets: 8, cors: None, setup_timeout: SETUP_TIMEOUT, consent_timeout: CONSENT_TIMEOUT, tls: None }
    }
}

/// A viewer: one WHEP session.
struct Viewer {
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
    video: Stream,
    /// Whether it has been sent its first keyframe, which it needs to start decoding.
    started: bool,
    connected: bool,
    last_sr: Instant,
    last_key: Instant,
}

impl Viewer {
    fn connected_to(&self) -> Option<SocketAddr> {
        self.selected.filter(|_| self.connected && self.srtp_out.is_some())
    }

    /// RTCP BYE and DTLS close_notify, to a viewer that is there to hear them.
    fn goodbye(&mut self, udp: &UdpSocket) {
        let Some(to) = self.selected else { return };
        if let Some(tx) = self.srtp_out.as_mut().filter(|_| self.video.packets > 0) {
            if let Ok(p) = tx.protect_rtcp(&packetize::bye(self.video.ssrc)) {
                let _ = udp.send_to(&p, to);
            }
        }
        for d in self.dtls.close_notify() {
            let _ = udp.send_to(&d, to);
        }
    }
}

struct State {
    viewers: Vec<Viewer>,
    packetizer: H264Packetizer,
}

/// What the endpoint's thread and its owner share.
struct Shared {
    state: Mutex<State>,
    keyframe: AtomicBool,
    stop: AtomicBool,
    /// The media socket, for the owner to send on; the thread reads it.
    udp: UdpSocket,
}

impl Shared {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A send-only WHEP endpoint. Dropping it says goodbye to its viewers and stops it.
pub struct WhepServer {
    shared: Arc<Shared>,
    addr: SocketAddr,
    worker: Option<std::thread::JoinHandle<()>>,
}

impl WhepServer {
    /// Listens for HTTP (TCP) and media (UDP) on `addr`, answering POSTs to `path` (`/` or `/whep`, with
    /// no query). With port 0 a port free for both is chosen, see [`WhepServer::local_addr`].
    pub fn bind(addr: SocketAddr, path: &str, opts: WhepOptions) -> Result<WhepServer> {
        crate::fips::activate()?;
        if !(1..=MAX_VIEWERS).contains(&opts.max_viewers) {
            return Err(Error::invalid_arg(format!("a WHEP endpoint takes 1 to {MAX_VIEWERS} viewers, not {}", opts.max_viewers)));
        }
        if opts.max_websockets > MAX_WEBSOCKETS {
            return Err(Error::invalid_arg(format!("a WHEP endpoint takes at most {MAX_WEBSOCKETS} WebSockets, not {}", opts.max_websockets)));
        }
        if path.is_empty() || !path.starts_with('/') || path.len() > 256 || !path.bytes().all(|b| b.is_ascii_graphic() && b != b'?' && b != b'%') {
            return Err(Error::invalid_arg(format!("a WHEP path starts with / and holds printable characters, without ? or %, got '{path}'")));
        }
        if opts.cors.as_deref().is_some_and(|o| o != "*" && !super::is_origin(o)) {
            return Err(Error::invalid_arg("cors takes an origin, such as https://studio.example.com, or *"));
        }
        let path = if path.len() > 1 { path.trim_end_matches('/') } else { path };
        let allow = Allow::from_env()?;
        let unreachable = |e: std::io::Error| Error::new(ErrorCode::Io, format!("could not listen on {addr}: {e}")).hint("check the address, and that nothing else uses the port (TCP for HTTP, UDP for the media)");
        let (listener, udp) = bind_pair(addr).map_err(unreachable)?;
        let local = listener.local_addr()?;
        super::grow_receive_buffer(&udp);
        let id = dtls::Identity::generate("IronAV WHEP").map_err(|e| Error::new(ErrorCode::Internal, format!("could not make the WHEP endpoint's DTLS certificate: {e}")))?;
        let http = http::Server::new(listener, allow.clone(), sdp::MAX_OFFER, opts.tls.clone())?;
        let shared = Arc::new(Shared { state: Mutex::new(State { viewers: Vec::new(), packetizer: H264Packetizer::default() }), keyframe: AtomicBool::new(false), stop: AtomicBool::new(false), udp: udp.try_clone()? });
        let mut endpoint = Endpoint { path: path.to_string(), opts, upgraded: Arc::new(AtomicUsize::new(0)), http, udp, id: Arc::new(id), allow, shared: shared.clone() };
        let worker = std::thread::Builder::new().name("ironav-whep".into()).spawn(move || endpoint.run()).map_err(|e| Error::new(ErrorCode::Internal, format!("could not start the WHEP endpoint: {e}")))?;
        Ok(WhepServer { shared, addr: local, worker: Some(worker) })
    }

    /// Opens the endpoint a `whep://[host]:port[/path][?token=T]` or `wheps://...` URL names (see
    /// [`WhepServer::bind`]). `wheps://` needs `cert=` and `key=`, PEM files of the TLS certificate
    /// chain and private key. The token is the one `Authorization: Bearer` header that may POST and DELETE.
    pub fn open(url: &str) -> Result<WhepServer> {
        open_whep(&SourceUrl::parse(url)?)
    }

    /// The address it listens on, for HTTP over TCP and for the media over UDP.
    pub fn local_addr(&self) -> SocketAddr {
        self.addr
    }

    /// How many viewers have finished ICE and DTLS and are being sent video.
    pub fn viewers(&self) -> usize {
        self.shared.state().viewers.iter().filter(|v| v.connected).count()
    }

    /// Sends one H.264 access unit, in Annex B form (start codes), whose presentation time is `pts_90k`
    /// on a 90 kHz clock, to every connected viewer. A viewer is sent nothing until the first keyframe
    /// (an IDR slice or a sequence parameter set); a keyframe that has no parameter sets gets the last
    /// ones seen. Access unit delimiters and filler are left out. Does not wait for the network.
    pub fn send_h264_access_unit(&self, annexb: &[u8], pts_90k: u64) -> Result<()> {
        let now = Instant::now();
        let mut out: Vec<(SocketAddr, Vec<u8>)> = Vec::new();
        {
            let mut st = self.shared.state();
            let (payloads, key) = st.packetizer.access_unit(annexb, packetize::MAX_PAYLOAD).map_err(Error::invalid_input)?;
            for v in st.viewers.iter_mut() {
                let Some(to) = v.connected_to() else { continue };
                if !v.started && !key {
                    continue;
                }
                v.started = true;
                let Some(tx) = v.srtp_out.as_mut() else { continue };
                for (i, p) in payloads.iter().enumerate() {
                    out.push((to, v.video.packet(tx, i + 1 == payloads.len(), pts_90k, p, now)?));
                }
            }
        }
        for (i, (to, p)) in out.iter().enumerate() {
            let _ = self.shared.udp.send_to(p, to);
            if i % BURST == BURST - 1 {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        Ok(())
    }

    /// Whether a viewer has asked for a keyframe (a picture loss indication or a full intra request),
    /// or has connected and waits for one, since the last call.
    pub fn keyframe_requested(&self) -> bool {
        self.shared.keyframe.swap(false, Ordering::Relaxed)
    }
}

impl Drop for WhepServer {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
    }
}

/// A TCP listener and a UDP socket on the same address; with port 0, on a port free for both.
fn bind_pair(addr: SocketAddr) -> std::io::Result<(TcpListener, UdpSocket)> {
    let mut last = None;
    for _ in 0..if addr.port() == 0 { 32 } else { 1 } {
        let listener = TcpListener::bind(addr)?;
        match UdpSocket::bind(listener.local_addr()?) {
            Ok(udp) => return Ok((listener, udp)),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap_or_else(|| std::io::ErrorKind::AddrInUse.into()))
}

/// What a `whep://` or `wheps://` URL says.
struct Target {
    addr: SocketAddr,
    path: String,
    token: Option<String>,
    cors: Option<String>,
    /// The certificate chain and key files of `wheps://`.
    tls: Option<(String, String)>,
}

fn target(u: &SourceUrl) -> Result<Target> {
    let (host, port, path) = super::parse_target(&u.target, &u.scheme)?;
    let ip: IpAddr = host.parse().map_err(|_| Error::invalid_arg(format!("'{host}' is not an address this endpoint can listen on")).hint("give an IP address, or leave the host out for loopback"))?;
    let addr = SocketAddr::new(ip, port);
    if let Some(k) = u.options.keys().find(|k| !matches!(k.as_str(), "token" | "cors" | "cert" | "key" | "reconnect")) {
        return Err(Error::invalid_arg(format!("'{k}' is not an option of a WHEP endpoint")).hint("a WHEP endpoint takes token=, cors=, cert= and key= (the last two for wheps://)"));
    }
    let tls = if u.scheme == "wheps" {
        let (Some(cert), Some(key)) = (u.option("cert"), u.option("key")) else {
            return Err(Error::invalid_arg("wheps:// needs cert= and key=, PEM files of the TLS certificate chain and the private key").hint("wheps://:8443/whep?cert=chain.pem&key=key.pem, or use whep:// for plain HTTP signalling (refused in FIPS mode)"));
        };
        Some((cert.to_string(), key.to_string()))
    } else if u.option("cert").is_some() || u.option("key").is_some() {
        return Err(Error::invalid_arg("cert= and key= belong to wheps://; whep:// serves plain HTTP").hint("use wheps://"));
    } else {
        None
    };
    let token = u.option("token").map(crate::url::percent_decode);
    if token.as_ref().is_some_and(|t| t.is_empty() || t.len() > 512 || !t.bytes().all(|b| b.is_ascii_graphic())) {
        return Err(Error::invalid_arg("the WHEP token must be 1 to 512 printable characters, without spaces"));
    }
    let cors = match u.option("cors").map(crate::url::percent_decode) {
        None => None,
        Some(o) if o == "*" || super::is_origin(&o) => Some(o),
        Some(_) => return Err(Error::invalid_arg("cors= takes an origin, such as https://studio.example.com or http://localhost:3000, or *").hint("an origin is a scheme, a host and perhaps a port, without a path")),
    };
    Ok(Target { addr, path, token, cors, tls })
}

/// Checks a `whep://` or `wheps://` URL without listening: its address, path and options, and FIPS
/// mode's rule that the signalling carry TLS.
pub(crate) fn plan_whep(u: &SourceUrl) -> Result<()> {
    crate::fips::check_transport(&u.scheme)?;
    target(u).map(|_| ())
}

/// Opens the endpoint a URL names.
pub(crate) fn open_whep(u: &SourceUrl) -> Result<WhepServer> {
    plan_whep(u)?;
    let t = target(u)?;
    let mut opts = WhepOptions::default();
    if let Some((cert, key)) = &t.tls {
        opts.tls = Some(crate::tls::server_config(cert, key)?);
    }
    if let Some(token) = t.token {
        opts.authorize = Some(Arc::new(move |h| super::bearer_matches(h, &token)));
    }
    opts.cors = t.cors;
    WhepServer::bind(t.addr, &t.path, opts)
}

/// The endpoint's thread.
struct Endpoint {
    path: String,
    opts: WhepOptions,
    /// The upgraded connections open.
    upgraded: Arc<AtomicUsize>,
    http: http::Server,
    udp: UdpSocket,
    id: Arc<dtls::Identity>,
    allow: Allow,
    shared: Arc<Shared>,
}

impl Endpoint {
    fn run(&mut self) {
        let mut buf = vec![0u8; 65536];
        while !self.shared.stop.load(Ordering::Relaxed) {
            let now = Instant::now();
            for (req, peer, responder) in self.http.poll(now) {
                if let Some(f) = self.opts.fallback.as_ref().filter(|_| !self.is_whep(&req)) {
                    let reply = HttpReply { responder: Some(responder), request: req.clone(), upgraded: self.upgraded.clone(), max_upgraded: self.opts.max_websockets };
                    let request = HttpRequest { inner: req, peer };
                    // a panic drops the reply, which answers 500, and the endpoint goes on
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(request, reply)));
                    continue;
                }
                let resp = self.route(&req, peer, now);
                responder.send(&self.cors(resp));
            }
            // a burst of datagrams, then the timers
            for _ in 0..256 {
                match crate::net::recv_from_within(&self.udp, &mut buf, POLL) {
                    Ok((n, from)) => self.datagram(&buf[..n], from, Instant::now()),
                    Err(_) => break, // timed out, or an ICMP error from an earlier send
                }
            }
            self.timers(Instant::now());
        }
        let mut st = self.shared.state();
        for v in st.viewers.iter_mut() {
            v.goodbye(&self.udp);
        }
        st.viewers.clear();
    }

    fn cors(&self, resp: Vec<u8>) -> Vec<u8> {
        match &self.opts.cors {
            Some(o) => super::with_cors(resp, o),
            None => resp,
        }
    }

    fn authorized(&self, req: &http::Request) -> bool {
        self.opts.authorize.as_ref().is_none_or(|f| f(req.header("authorization")))
    }

    /// Whether a request is for the WHEP path or one of its session resources.
    fn is_whep(&self, req: &http::Request) -> bool {
        req.path == self.path || req.path.starts_with(&self.session_path(""))
    }

    fn session_path(&self, id: &str) -> String {
        format!("{}/session/{id}", self.path.trim_end_matches('/'))
    }

    fn route(&mut self, req: &http::Request, peer: SocketAddr, now: Instant) -> Vec<u8> {
        let text = |status: u16, why: &str| http::response(status, &[("Content-Type", "text/plain")], why.as_bytes());
        if req.method == "OPTIONS" {
            return http::response(204, &[("Accept-Post", "application/sdp"), ("Allow", "OPTIONS, POST, DELETE, PATCH")], b"");
        }
        if !self.authorized(req) {
            return http::response(401, &[("WWW-Authenticate", "Bearer")], b"");
        }
        let sessions = self.session_path("");
        if req.path.starts_with(&sessions) {
            let mut st = self.shared.state();
            let Some(at) = st.viewers.iter().position(|v| self.session_path(&v.id) == req.path) else { return text(404, "no such session") };
            return match req.method.as_str() {
                "DELETE" => {
                    st.viewers.remove(at).goodbye(&self.udp);
                    http::response(200, &[], b"")
                }
                // trickle ICE and ICE restarts are not offered
                "PATCH" => http::response(405, &[("Allow", "DELETE")], b""),
                _ => http::response(405, &[("Allow", "DELETE, PATCH")], b""),
            };
        }
        if req.path != self.path {
            return text(404, "no WHEP endpoint here");
        }
        if req.method != "POST" {
            return http::response(405, &[("Allow", "OPTIONS, POST")], b"");
        }
        if !req.header("content-type").is_some_and(|c| c.to_ascii_lowercase().starts_with("application/sdp")) {
            return text(415, "the offer must be application/sdp");
        }
        if self.shared.state().viewers.len() >= self.opts.max_viewers {
            return http::response(503, &[("Content-Type", "text/plain"), ("Retry-After", "5")], b"this endpoint has all the viewers it takes");
        }
        let Ok(body) = std::str::from_utf8(&req.body) else { return text(400, "the offer is not text") };
        let offer = match sdp::parse_offer_as(body, sdp::Role::Viewer) {
            Ok(o) => o,
            Err(why) => return text(400, &why),
        };
        let Some(video) = offer.tracks().next().map(|t| t.payload) else { return text(400, "the offer has no video") };
        let dtls = match dtls::Server::new(self.id.clone(), offer.fingerprint) {
            Ok(d) => d,
            Err(e) => return text(500, &e),
        };
        // the host candidate: the address the viewer reached, when listening on every address
        let mut candidate = self.http.local_addr().unwrap_or(peer);
        if candidate.ip().is_unspecified() {
            candidate.set_ip(super::local_ip_toward(peer.ip()).unwrap_or(candidate.ip()));
        }
        let (ufrag, pwd, id) = (super::random_text(8), super::random_text(24), super::random_text(16));
        let ssrc = u32::from_be_bytes(crate::auth::random_bytes::<4>());
        let local = sdp::Local { ufrag: &ufrag, pwd: &pwd, fingerprint: &self.id.fingerprint, candidate, session_id: u64::from_be_bytes(crate::auth::random_bytes::<8>()) >> 1 };
        let answer = sdp::viewer_answer(&offer, &local, ssrc);
        let location = self.session_path(&id);
        let viewer = Viewer {
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
            video: Stream::new(video, ssrc, 90_000),
            started: false,
            connected: false,
            last_sr: now,
            last_key: now,
        };
        let mut st = self.shared.state();
        // a second POST may have come in while the first was parsed; the limit holds
        if st.viewers.len() >= self.opts.max_viewers {
            return http::response(503, &[("Content-Type", "text/plain"), ("Retry-After", "5")], b"this endpoint has all the viewers it takes");
        }
        st.viewers.push(viewer);
        http::response(201, &[("Content-Type", "application/sdp"), ("Location", &location)], answer.as_bytes())
    }

    fn permitted(&self, ip: IpAddr) -> bool {
        ip.is_loopback() || self.allow.permits(ip)
    }

    fn datagram(&mut self, d: &[u8], from: SocketAddr, now: Instant) {
        if !self.permitted(from.ip()) {
            return;
        }
        let shared = self.shared.clone();
        let mut st = shared.state();
        match d.first() {
            Some(0..=3) => {
                // whichever viewer's credentials the check carries
                for v in st.viewers.iter_mut() {
                    let Some(b) = stun::check_request(d, &v.ufrag, &v.offer.ufrag, &v.pwd) else { continue };
                    let _ = self.udp.send_to(&stun::success(&b.txid, from, &v.pwd), from);
                    if !v.peers.contains(&from) {
                        if v.peers.len() == MAX_PEERS {
                            v.peers.pop_front();
                        }
                        v.peers.push_back(from);
                    }
                    if b.use_candidate || v.selected.is_none() {
                        v.selected = Some(from);
                    }
                    v.last_heard = now;
                    break;
                }
            }
            Some(20..=63) => {
                let Some(at) = st.viewers.iter().position(|v| v.peers.contains(&from)) else { return };
                let v = &mut st.viewers[at];
                let mut addr = from.ip().to_string().into_bytes();
                addr.extend_from_slice(&from.port().to_be_bytes());
                match v.dtls.handle(d, &addr, now) {
                    Ok(out) => {
                        for o in out {
                            let _ = self.udp.send_to(&o, from);
                        }
                    }
                    Err((why, alert)) => {
                        for o in alert {
                            let _ = self.udp.send_to(&o, from);
                        }
                        // this viewer fails; the others go on
                        eprintln!("ironav: WHEP viewer at {from} failed: {why}");
                        st.viewers.remove(at);
                        return;
                    }
                }
                if v.dtls.closed() {
                    st.viewers.remove(at);
                    return;
                }
                if v.srtp_in.is_none() {
                    if let Some(k) = v.dtls.keys().cloned() {
                        v.selected = Some(from);
                        match (srtp::Context::new(k.profile, &k.client_key, &k.client_salt), srtp::Context::new(k.profile, &k.server_key, &k.server_salt)) {
                            (Ok(i), Ok(o)) => {
                                (v.srtp_in, v.srtp_out) = (Some(i), Some(o));
                                v.connected = true;
                                v.last_heard = now;
                                v.last_key = now;
                                // it has nothing to decode until a keyframe comes
                                self.shared.keyframe.store(true, Ordering::Relaxed);
                            }
                            _ => {
                                st.viewers.remove(at);
                            }
                        }
                    }
                }
            }
            Some(128..=191) if d.len() >= 8 && (192..=223).contains(&d[1]) => {
                let Some(v) = st.viewers.iter_mut().find(|v| v.connected && v.peers.contains(&from)) else { return };
                let Some(rx) = v.srtp_in.as_mut() else { return };
                let Ok(p) = rx.unprotect_rtcp(d) else { return };
                v.last_heard = now;
                let mut resend = Vec::new();
                for f in packetize::feedback(&p) {
                    match f {
                        packetize::Feedback::Keyframe(ssrcs) => {
                            if ssrcs.contains(&v.video.ssrc) {
                                self.shared.keyframe.store(true, Ordering::Relaxed);
                            }
                        }
                        packetize::Feedback::Nack { media, seqs } => {
                            if media == v.video.ssrc {
                                resend.extend(v.video.resend(&seqs, now));
                            }
                        }
                    }
                }
                for p in resend {
                    let _ = self.udp.send_to(&p, from);
                }
            }
            _ => {}
        }
    }

    /// Setup and consent timeouts, the DTLS retransmissions, sender reports and keyframe requests.
    fn timers(&mut self, now: Instant) {
        let shared = self.shared.clone();
        let mut st = shared.state();
        let udp = &self.udp;
        let (setup, consent) = (self.opts.setup_timeout, self.opts.consent_timeout);
        st.viewers.retain_mut(|v| {
            if !v.connected {
                if now.duration_since(v.created) > setup {
                    eprintln!("ironav: a WHEP viewer did not connect within {} s", setup.as_secs_f32());
                    return false;
                }
                match v.dtls.poll(now) {
                    Ok(out) => {
                        if let Some(to) = v.selected {
                            for o in out {
                                let _ = udp.send_to(&o, to);
                            }
                        }
                    }
                    Err(why) => {
                        eprintln!("ironav: a WHEP viewer failed: {why}");
                        return false;
                    }
                }
                return true;
            }
            if now.duration_since(v.last_heard) > consent {
                v.goodbye(udp);
                return false;
            }
            if !v.started && now.duration_since(v.last_key) >= KEY_EVERY {
                v.last_key = now;
                self.shared.keyframe.store(true, Ordering::Relaxed);
            }
            if now.duration_since(v.last_sr) >= SR_EVERY {
                v.last_sr = now;
                if let (Some(to), Some(ts), Some(tx)) = (v.selected, v.video.timestamp_at(now), v.srtp_out.as_mut()) {
                    let wall = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
                    let sr = packetize::sender_report(v.video.ssrc, packetize::ntp(wall), ts, v.video.packets, v.video.octets, CNAME);
                    if let Ok(p) = tx.protect_rtcp(&sr) {
                        let _ = udp.send_to(&p, to);
                    }
                }
            }
            true
        });
    }
}

/// A request the [`WhepOptions::fallback`] is given: a read-only view of what the client sent.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    inner: http::Request,
    peer: SocketAddr,
}

impl HttpRequest {
    /// The method, such as `GET`.
    pub fn method(&self) -> &str {
        &self.inner.method
    }

    /// The path, as sent: without the query, and not percent-decoded.
    pub fn path(&self) -> &str {
        &self.inner.path
    }

    /// The query, without the `?`; empty if there is none. Not percent-decoded.
    pub fn query(&self) -> &str {
        &self.inner.query
    }

    /// The value of a header, by name, ignoring case. The first, if it is sent more than once.
    pub fn header(&self, name: &str) -> Option<&str> {
        self.inner.header(name)
    }

    /// The body, at most 64 KiB.
    pub fn body(&self) -> &[u8] {
        &self.inner.body
    }

    /// The address the request came from.
    pub fn peer_addr(&self) -> SocketAddr {
        self.peer
    }
}

/// Where the answer to a request goes. It answers once, and the connection ends with the answer, or
/// becomes a WebSocket. Dropped unanswered it sends a `500`.
pub struct HttpReply {
    responder: Option<http::Responder>,
    request: http::Request,
    upgraded: Arc<AtomicUsize>,
    max_upgraded: usize,
}

impl HttpReply {
    /// Answers with `status`, `headers` and `body`. The server adds `Content-Length` and
    /// `Connection: close`, and `X-Content-Type-Options: nosniff` and `Cache-Control: no-store` unless
    /// the headers have them; the caller gives `Content-Type`, and `Content-Security-Policy` for a page.
    /// A header with a line break in it, or named `Content-Length` or `Connection`, is not sent: the
    /// answer is a `500` instead, so that the mistake shows.
    pub fn send(mut self, status: u16, headers: &[(&str, &str)], body: &[u8]) {
        let Some(responder) = self.responder.take() else { return };
        responder.send(&page_response(status, headers, body));
    }

    /// Takes the request as a WebSocket upgrade. It checks the request (a `GET` with the upgrade
    /// headers, version 13, a well-formed key, and an `Origin` that is one of `allowed_origins`,
    /// compared ignoring case: a request with no `Origin` is refused), takes one of the server's
    /// places for upgraded connections, and answers `101`. The connection it returns is the caller's
    /// to run on a thread of its own; dropping it gives its place back.
    ///
    /// When it refuses it has answered the client already (`400`, `403`, `405`, `426`, or `503` when
    /// [`WhepOptions::max_websockets`] are open) and the error says what was sent.
    pub fn upgrade_websocket(mut self, allowed_origins: &[&str], config: websocket::Config) -> std::result::Result<websocket::Connection<websocket::Stream>, Refused> {
        let Some(responder) = self.responder.take() else { return Err(Refused { status: 0, why: "already answered" }) };
        let refuse = |responder: http::Responder, r: Refused| {
            responder.send(&r.response());
            Err(r)
        };
        let accept = match websocket::handshake(&self.request, allowed_origins) {
            Ok(a) => a,
            Err(r) => return refuse(responder, r),
        };
        let Some(slot) = http::Slot::acquire(&self.upgraded, self.max_upgraded) else {
            return refuse(responder, Refused { status: 503, why: "this endpoint has all the WebSockets it takes" });
        };
        let lost = Refused { status: 0, why: "the connection failed" };
        let Some(stream) = responder.into_stream(UPGRADED_SLICE) else { return Err(lost) };
        let mut stream = stream.holding(slot);
        std::io::Write::write_all(&mut stream, &websocket::upgrade_response(&accept)).and_then(|()| std::io::Write::flush(&mut stream)).map_err(|_| lost)?;
        Ok(websocket::Connection::new(stream, config))
    }
}

impl Drop for HttpReply {
    fn drop(&mut self) {
        if let Some(r) = self.responder.take() {
            r.send(&http::response(500, &[("Content-Type", "text/plain")], b"no answer"));
        }
    }
}

/// A page's response: the caller's headers and the security headers that suit a page.
fn page_response(status: u16, headers: &[(&str, &str)], body: &[u8]) -> Vec<u8> {
    let bad = |(k, v): &(&str, &str)| {
        k.is_empty() || !k.bytes().all(|b| b.is_ascii_graphic() && b != b':') || v.bytes().any(|b| (b < b' ' && b != b'\t') || b == 0x7f) || k.eq_ignore_ascii_case("content-length") || k.eq_ignore_ascii_case("connection")
    };
    if headers.iter().any(bad) || !(200..=599).contains(&status) {
        return http::response(500, &[("Content-Type", "text/plain")], b"the page's response was malformed");
    }
    let mut all = headers.to_vec();
    for (k, v) in [("X-Content-Type-Options", "nosniff"), ("Cache-Control", "no-store")] {
        if !headers.iter().any(|(h, _)| h.eq_ignore_ascii_case(k)) {
            all.push((k, v));
        }
    }
    http::response(status, &all, body)
}
