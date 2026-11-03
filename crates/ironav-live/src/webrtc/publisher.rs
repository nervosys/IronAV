//! A WHIP publisher (RFC 9725): pushes H.264 and Opus to a WHIP endpoint, the client to
//! [`whip://`](super)'s server.
//!
//! [`WhipPublisher::connect`] POSTs an SDP offer (send-only, BUNDLE, `rtcp-mux`, `a=setup:actpass`, the
//! fingerprint of a certificate made for the session) to an `http://` or `https://` URL, with an optional
//! `Authorization: Bearer`, and expects `201 Created`, the answer and a `Location`. HTTPS goes through the
//! TLS stack `https://` sources use (IronSocketLayer, IronPKI validating the certificate); in FIPS mode
//! plain `http://` is refused, as `whip://` is. Then:
//!
//! 1. ICE (`stun`): connectivity checks with USE-CANDIDATE, signed with the answer's password, go to the
//!    answer's host candidates until one answers. The server is taken to be ICE-lite, as WHIP servers
//!    are, so the publisher is the controlling agent. Consent is checked every 5 s afterwards, and the
//!    session ends after 30 s without an answer.
//! 2. DTLS (`dtls`): the publisher is the DTLS client, and the server's certificate must have the
//!    SHA-256 fingerprint of its answer. The SRTP keys come from the exporter.
//! 3. RTP (`packetize`) under SRTP (`srtp`): H.264 access units in Annex B form as single NAL unit packets
//!    and FU-A fragments, Opus packets as they are. A thread then keeps the session: RTCP sender reports
//!    every second, consent checks, the server's picture loss indications and full intra requests
//!    (surfaced by [`WhipPublisher::keyframe_requested`]) and NACKs (answered from the last 512 packets
//!    sent of each stream).
//!
//! [`WhipPublisher::close`] says goodbye in RTCP and DTLS and DELETEs the session's `Location`.
//!
//! What it trusts: nothing it is sent. The answer is parsed within fixed bounds; media goes only to
//! addresses the answer named that `IRONAV_NET_ALLOW` permits (loopback, unless told otherwise, see
//! [`crate::net`]); everything from the server's UDP port is dropped unless it comes from that address,
//! and RTCP unless it authenticates. The bearer token goes only to the host and port it was given for.

use super::{dtls, packetize, sdp, srtp, stun};
use crate::http::HttpUrl;
use crate::net::{self, Allow};
use crate::tls::{self, Conn};
use ironav_core::{Error, ErrorCode, Result};
use ironsocketlayer::config::ClientConfig;
use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

const VIDEO_PT: u8 = 102;
const AUDIO_PT: u8 = 111;
/// Packets kept per stream to answer NACKs with.
const HISTORY: usize = 512;
/// A packet is sent again at most this often.
const RESEND_EVERY: Duration = Duration::from_millis(50);
const SR_EVERY: Duration = Duration::from_secs(1);
const CONSENT_EVERY: Duration = Duration::from_secs(5);
const CONSENT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long connectivity checks go unanswered before the session fails to start.
const ICE_TIMEOUT: Duration = Duration::from_secs(10);
const FIRST_CHECK: Duration = Duration::from_millis(250);
const MAX_CHECK: Duration = Duration::from_secs(1);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const IO_TIMEOUT: Duration = Duration::from_secs(10);
const MAX_HEAD: usize = 64 << 10;
const MAX_BODY: usize = sdp::MAX_OFFER;
/// How long one turn of the session thread waits for a datagram.
const POLL: Duration = Duration::from_millis(20);
/// A burst of this many packets is followed by a millisecond's pause, so a keyframe's packets
/// do not overflow the receiver's socket buffer.
const BURST: usize = 40;
const USER_AGENT: &str = concat!("IronAV/", env!("CARGO_PKG_VERSION"));

/// How a WHIP publisher is set up.
#[non_exhaustive]
#[derive(Clone)]
pub struct WhipOptions {
    /// Offer H.264 video.
    pub video: bool,
    /// Offer Opus audio.
    pub audio: bool,
    /// The `profile-level-id` the offer gives for the H.264 (six hex digits); servers may match it
    /// against the other end's. The default is Constrained Baseline, level 3.1.
    pub profile_level_id: String,
    /// TLS client configuration to use instead of the platform's roots (for tests).
    pub(crate) tls: Option<Arc<ClientConfig>>,
}

impl Default for WhipOptions {
    fn default() -> Self {
        WhipOptions { video: true, audio: true, profile_level_id: "42e01f".into(), tls: None }
    }
}

/// One stream sent: its RTP state, and the packets kept for NACKs.
struct Stream {
    payload: u8,
    ssrc: u32,
    clock: u32,
    seq: u16,
    /// Added to the caller's timestamps, so they do not start at 0 (RFC 3550 §5.1).
    offset: u32,
    packets: u32,
    octets: u32,
    /// The RTP timestamp of the last packet sent, and when.
    last: Option<(u32, Instant)>,
    history: VecDeque<Sent>,
}

struct Sent {
    seq: u16,
    packet: Vec<u8>,
    resent: Option<Instant>,
}

impl Stream {
    fn new(payload: u8, ssrc: u32, clock: u32) -> Stream {
        let r = crate::auth::random_bytes::<6>();
        Stream { payload, ssrc, clock, seq: u16::from_be_bytes([r[0], r[1]]) & 0x7FFF, offset: u32::from_be_bytes([r[2], r[3], r[4], r[5]]), packets: 0, octets: 0, last: None, history: VecDeque::new() }
    }

    /// The SRTP packet carrying `payload` at `pts` (in this stream's clock), kept for a NACK.
    fn packet(&mut self, tx: &mut srtp::Context, marker: bool, pts: u64, payload: &[u8], now: Instant) -> Result<Vec<u8>> {
        let ts = (pts as u32).wrapping_add(self.offset);
        let plain = packetize::rtp_packet(self.payload, marker, self.seq, ts, self.ssrc, payload);
        let protected = tx.protect_rtp(&plain).map_err(|e| Error::new(ErrorCode::Internal, format!("SRTP could not protect a packet: {e:?}")))?;
        if self.history.len() == HISTORY {
            self.history.pop_front();
        }
        self.history.push_back(Sent { seq: self.seq, packet: protected.clone(), resent: None });
        self.seq = self.seq.wrapping_add(1);
        self.packets = self.packets.wrapping_add(1);
        self.octets = self.octets.wrapping_add(payload.len() as u32);
        self.last = Some((ts, now));
        Ok(protected)
    }

    /// The RTP timestamp that `now` stands for, by the clock from the last packet sent.
    fn timestamp_at(&self, now: Instant) -> Option<u32> {
        let (ts, at) = self.last?;
        Some(ts.wrapping_add((now.saturating_duration_since(at).as_secs_f64() * self.clock as f64) as u32))
    }

    /// The packets of `seqs` that are kept and have not been sent again just now.
    fn resend(&mut self, seqs: &[u16], now: Instant) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for s in seqs {
            if let Some(h) = self.history.iter_mut().rev().find(|h| h.seq == *s) {
                if h.resent.is_none_or(|t| now.duration_since(t) >= RESEND_EVERY) {
                    h.resent = Some(now);
                    out.push(h.packet.clone());
                }
            }
        }
        out
    }
}

struct Media {
    tx: srtp::Context,
    video: Option<Stream>,
    audio: Option<Stream>,
}

/// What the publisher's thread and its caller share.
struct Shared {
    media: Mutex<Media>,
    keyframe: AtomicBool,
    stop: AtomicBool,
    /// Why the session ended on its own.
    failure: Mutex<Option<String>>,
}

impl Shared {
    fn media(&self) -> MutexGuard<'_, Media> {
        self.media.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn failure(&self) -> MutexGuard<'_, Option<String>> {
        self.failure.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn fail(&self, why: impl Into<String>) {
        self.failure().get_or_insert(why.into());
    }
}

/// ICE as a controlling agent toward an ICE-lite server (RFC 8445 §2.5): connectivity checks, and the
/// consent checks (RFC 7675) that follow.
pub(crate) struct Ice {
    username: String,
    local_ufrag: String,
    remote_ufrag: String,
    remote_pwd: String,
    local_pwd: String,
    tiebreaker: [u8; 8],
    /// Transaction ids of requests sent that have no answer yet, the last few.
    pending: VecDeque<[u8; 12]>,
    last_sent: Option<Instant>,
    last_answer: Instant,
}

impl Ice {
    pub(crate) fn new(local_ufrag: &str, local_pwd: &str, remote: &sdp::Answer, now: Instant) -> Ice {
        Ice {
            username: format!("{}:{local_ufrag}", remote.ufrag),
            local_ufrag: local_ufrag.into(),
            remote_ufrag: remote.ufrag.clone(),
            remote_pwd: remote.pwd.clone(),
            local_pwd: local_pwd.into(),
            tiebreaker: crate::auth::random_bytes::<8>(),
            pending: VecDeque::new(),
            last_sent: None,
            last_answer: now,
        }
    }

    /// A connectivity or consent check, with USE-CANDIDATE, signed with the server's password.
    pub(crate) fn request(&mut self, now: Instant) -> Vec<u8> {
        let txid = crate::auth::random_bytes::<12>();
        if self.pending.len() == 16 {
            self.pending.pop_front();
        }
        self.pending.push_back(txid);
        self.last_sent = Some(now);
        stun::request(&txid, &self.username, &self.remote_pwd, true, self.tiebreaker)
    }

    /// Whether `d` is an answer to a request sent, signed with the server's password; it counts as
    /// the server's consent.
    pub(crate) fn answered(&mut self, d: &[u8], now: Instant) -> bool {
        if d.len() < 20 {
            return false;
        }
        let Some(i) = self.pending.iter().position(|t| d[8..20] == t[..]) else { return false };
        if !stun::is_success(d, &self.pending[i], &self.remote_pwd) {
            return false;
        }
        self.pending.remove(i);
        self.last_answer = now;
        true
    }

    /// The success response to a check the server sent (a full ICE agent does), if `d` is one for us.
    pub(crate) fn respond(&self, d: &[u8], from: SocketAddr) -> Option<Vec<u8>> {
        let b = stun::check_request(d, &self.local_ufrag, &self.remote_ufrag, &self.local_pwd)?;
        Some(stun::success(&b.txid, from, &self.local_pwd))
    }

    /// Whether it is time for the next consent check.
    pub(crate) fn due(&self, now: Instant) -> bool {
        self.last_sent.is_none_or(|t| now.duration_since(t) >= CONSENT_EVERY)
    }

    /// Whether the server has not answered for [`CONSENT_TIMEOUT`] (RFC 7675 §5.1).
    pub(crate) fn lapsed(&self, now: Instant) -> bool {
        now.duration_since(self.last_answer) > CONSENT_TIMEOUT
    }
}

/// A WHIP session being published: see the [module](self).
pub struct WhipPublisher {
    shared: Arc<Shared>,
    sock: UdpSocket,
    peer: SocketAddr,
    worker: Option<std::thread::JoinHandle<()>>,
    h264: packetize::H264Packetizer,
    resource: Option<Resource>,
    http: Http,
    shown: String,
}

/// The session's `Location`, and the token that may go to it.
struct Resource {
    url: HttpUrl,
    token: Option<String>,
}

/// How HTTP requests are made.
#[derive(Clone)]
struct Http {
    allow: Allow,
    tls: Option<Arc<ClientConfig>>,
}

fn io_error(what: &str, e: impl std::fmt::Display) -> Error {
    Error::new(ErrorCode::Io, format!("{what}: {e}"))
}

fn transient(e: &std::io::Error) -> bool {
    use std::io::ErrorKind::{ConnectionRefused, ConnectionReset, Interrupted, TimedOut, WouldBlock};
    matches!(e.kind(), ConnectionRefused | ConnectionReset | Interrupted | TimedOut | WouldBlock)
}

impl WhipPublisher {
    /// Connects to the WHIP endpoint at `url` (`http://` or `https://`; `token` is its bearer token,
    /// if it takes one) and returns once media can be sent: the offer is answered, ICE has found the
    /// server and DTLS is done. Fails after about 25 s at most.
    pub fn connect(url: &str, token: Option<&str>, opts: WhipOptions) -> Result<WhipPublisher> {
        crate::fips::activate()?;
        if !opts.video && !opts.audio {
            return Err(Error::invalid_arg("a WHIP publisher needs video or audio"));
        }
        let level = &opts.profile_level_id;
        if level.len() != 6 || !level.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(Error::invalid_arg("profile_level_id is six hexadecimal digits, such as 42e01f"));
        }
        if token.is_some_and(|t| t.is_empty() || t.len() > 512 || !t.bytes().all(|b| b.is_ascii_graphic())) {
            return Err(Error::invalid_arg("the WHIP token must be 1 to 512 printable characters, without spaces"));
        }
        let target = HttpUrl::parse(url)?;
        // plaintext signalling is refused in FIPS mode before anything is made or sent
        if !target.tls {
            crate::fips::check_transport("http")?;
        }
        let shown = crate::url::redact(&target.plain());
        let http = Http { allow: Allow::from_env()?, tls: opts.tls.clone() };

        let id = Arc::new(dtls::Identity::generate("IronAV WHIP publisher").map_err(|e| Error::new(ErrorCode::Internal, format!("could not make the publisher's DTLS certificate: {e}")))?);
        let (ufrag, pwd) = (super::random_text(8), super::random_text(24));
        let ssrcs = crate::auth::random_bytes::<8>();
        let (video_ssrc, audio_ssrc) = (u32::from_be_bytes([ssrcs[0], ssrcs[1], ssrcs[2], ssrcs[3]]) | 1, u32::from_be_bytes([ssrcs[4], ssrcs[5], ssrcs[6], ssrcs[7]]) & !1 | 2);
        let cname = format!("ironav-{}", super::random_text(10));
        let offer = sdp::offer(&sdp::PubOffer {
            ufrag: &ufrag,
            pwd: &pwd,
            fingerprint: &id.fingerprint,
            session_id: u64::from_be_bytes(crate::auth::random_bytes::<8>()) >> 2,
            cname: &cname,
            profile_level_id: &opts.profile_level_id,
            video: opts.video.then_some((VIDEO_PT, video_ssrc)),
            audio: opts.audio.then_some((AUDIO_PT, audio_ssrc)),
        });

        let reply = request(&http, "POST", &target, &shown, Some(("application/sdp", offer.as_bytes())), token)?;
        // once the server has created a session, a failure to start must end it
        let resource = if reply.status == 201 { resource_of(&reply, &target, token) } else { None };
        let started = Self::start(&http, &opts, &id, (&ufrag, &pwd), (video_ssrc, audio_ssrc), cname, &shown, &reply, resource.is_some());
        match started {
            Ok((shared, sock, peer, worker)) => Ok(WhipPublisher { shared, sock, peer, worker: Some(worker), h264: packetize::H264Packetizer::default(), resource, http, shown }),
            Err(e) => {
                if let Some(r) = resource {
                    let _ = request(&http, "DELETE", &r.url, &shown, None, r.token.as_deref());
                }
                Err(e)
            }
        }
    }

    /// From the server's answer to a session that is up: ICE, DTLS, and the thread that keeps it.
    fn start(http: &Http, opts: &WhipOptions, id: &Arc<dtls::Identity>, creds: (&str, &str), ssrcs: (u32, u32), cname: String, shown: &str, reply: &Reply, located: bool) -> Result<(Arc<Shared>, UdpSocket, SocketAddr, std::thread::JoinHandle<()>)> {
        if reply.status != 201 {
            return Err(rejected(shown, reply));
        }
        if !located {
            return Err(Error::new(ErrorCode::Io, format!("{shown} answered 201 without a usable Location")).hint("a WHIP server must say where the session is, so that it can be ended"));
        }
        let text = std::str::from_utf8(&reply.body).map_err(|_| Error::new(ErrorCode::Io, format!("{shown} sent an answer that is not text")))?;
        let answer = sdp::parse_answer(text).map_err(|e| Error::new(ErrorCode::Io, format!("{shown}'s answer: {e}")))?;
        let video = opts.video.then(|| answer.payload(sdp::Kind::Video)).flatten().map(|pt| Stream::new(pt, ssrcs.0, 90_000));
        let audio = opts.audio.then(|| answer.payload(sdp::Kind::Audio)).flatten().map(|pt| Stream::new(pt, ssrcs.1, 48_000));
        if video.is_none() && audio.is_none() {
            return Err(Error::new(ErrorCode::Io, format!("{shown} accepted neither the H.264 video nor the Opus audio offered")));
        }
        let (sock, peer, ice) = find_server(&http.allow, &answer, creds, shown)?;
        let mut client = dtls::Client::new(id.clone(), answer.fingerprint).map_err(|e| Error::new(ErrorCode::Internal, format!("DTLS: {e}")))?;
        let keys = handshake(&sock, peer, &mut client).map_err(|e| Error::new(ErrorCode::Io, format!("DTLS with {shown} failed: {e}")).hint("the server's certificate must be the one its answer's fingerprint names"))?;
        let ctx = |key: &[u8], salt: &[u8]| srtp::Context::new(keys.profile, key, salt).map_err(|e| Error::new(ErrorCode::Internal, format!("SRTP keys: {e:?}")));
        let shared = Arc::new(Shared { media: Mutex::new(Media { tx: ctx(&keys.client_key, &keys.client_salt)?, video, audio }), keyframe: AtomicBool::new(false), stop: AtomicBool::new(false), failure: Mutex::new(None) });
        let session = Session { sock: sock.try_clone()?, peer, client, ice, rx: ctx(&keys.server_key, &keys.server_salt)?, shared: shared.clone(), cname, last_report: Instant::now() };
        let worker = std::thread::Builder::new().name("ironav-whip-publisher".into()).spawn(move || session.run()).map_err(|e| Error::new(ErrorCode::Internal, format!("could not start the WHIP publisher: {e}")))?;
        Ok((shared, sock, peer, worker))
    }

    /// Why the session has ended on its own (the server stopped answering, closed DTLS, ...), if it has.
    pub fn failure(&self) -> Option<String> {
        self.shared.failure().clone()
    }

    fn check(&self) -> Result<()> {
        match self.failure() {
            Some(why) => Err(Error::new(ErrorCode::Io, format!("the WHIP session with {} ended: {why}", self.shown)).hint("connect again to publish")),
            None => Ok(()),
        }
    }

    /// Sends one H.264 access unit, in Annex B form (start codes), whose presentation time is
    /// `pts_90k` on a 90 kHz clock. Access unit delimiters and filler are left out, and a keyframe
    /// that has no parameter sets gets the last ones seen.
    pub fn send_h264_access_unit(&mut self, annexb: &[u8], pts_90k: u64) -> Result<()> {
        self.check()?;
        let (payloads, _key) = self.h264.access_unit(annexb, packetize::MAX_PAYLOAD).map_err(Error::invalid_input)?;
        let now = Instant::now();
        let mut out = Vec::with_capacity(payloads.len());
        {
            let mut m = self.shared.media();
            let Media { tx, video, .. } = &mut *m;
            let Some(v) = video.as_mut() else { return Err(Error::invalid_arg("the WHIP server did not accept video")) };
            for (i, p) in payloads.iter().enumerate() {
                out.push(v.packet(tx, i + 1 == payloads.len(), pts_90k, p, now)?);
            }
        }
        self.send_all(&out)
    }

    /// Sends one Opus packet whose presentation time is `pts_48k` on a 48 kHz clock.
    pub fn send_opus(&mut self, packet: &[u8], pts_48k: u64) -> Result<()> {
        self.check()?;
        let payload = packetize::opus_payload(packet).map_err(Error::invalid_input)?;
        let now = Instant::now();
        let out = {
            let mut m = self.shared.media();
            let Media { tx, audio, .. } = &mut *m;
            let Some(a) = audio.as_mut() else { return Err(Error::invalid_arg("the WHIP server did not accept audio")) };
            // the first packet starts a talkspurt (RFC 7587 §4.2)
            let marker = a.packets == 0;
            a.packet(tx, marker, pts_48k, payload, now)?
        };
        self.send_all(&[out])
    }

    /// Whether the server has asked for a keyframe (a picture loss indication or a full intra
    /// request) since the last call.
    pub fn keyframe_requested(&mut self) -> bool {
        self.shared.keyframe.swap(false, Ordering::Relaxed)
    }

    fn send_all(&self, packets: &[Vec<u8>]) -> Result<()> {
        for (i, p) in packets.iter().enumerate() {
            if let Err(e) = self.sock.send_to(p, self.peer) {
                if !transient(&e) {
                    return Err(io_error(&format!("could not send to {}", self.peer), e));
                }
            }
            if i % BURST == BURST - 1 {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        Ok(())
    }

    /// Ends the session: RTCP BYE and DTLS close_notify, then a DELETE of its `Location`. Fails only if the
    /// DELETE could not be made or the server refused it.
    pub fn close(mut self) -> Result<()> {
        self.shutdown()
    }

    fn shutdown(&mut self) -> Result<()> {
        self.shared.stop.store(true, Ordering::Relaxed);
        if let Some(w) = self.worker.take() {
            let _ = w.join();
        }
        let Some(r) = self.resource.take() else { return Ok(()) };
        let reply = request(&self.http, "DELETE", &r.url, &self.shown, None, r.token.as_deref())?;
        // gone already is as good as deleted
        match reply.status {
            200..=299 | 404 | 410 => Ok(()),
            _ => Err(rejected(&self.shown, &reply)),
        }
    }
}

impl Drop for WhipPublisher {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

/// The checks until a candidate answers: the socket they were sent from and the address that answered.
fn find_server(allow: &Allow, answer: &sdp::Answer, creds: (&str, &str), shown: &str) -> Result<(UdpSocket, SocketAddr, Ice)> {
    let permitted: Vec<SocketAddr> = answer.candidates.iter().copied().filter(|c| allow.permits(c.ip())).collect();
    let Some(first) = permitted.first() else {
        let ips: Vec<String> = answer.candidates.iter().map(|c| c.ip().to_string()).collect();
        return Err(Error::new(ErrorCode::InvalidArgument, format!("{shown}'s answer names only addresses that {} does not permit ({})", crate::net::ALLOW_VAR, ips.join(", "))).hint(format!("set {}=private, public or an address or range to allow them", crate::net::ALLOW_VAR)));
    };
    // one address family: the socket's
    let permitted: Vec<SocketAddr> = permitted.iter().copied().filter(|c| c.is_ipv4() == first.is_ipv4()).collect();
    let sock = UdpSocket::bind(if first.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }).map_err(|e| io_error("could not open a UDP socket", e))?;
    let mut ice = Ice::new(creds.0, creds.1, answer, Instant::now());
    let end = Instant::now() + ICE_TIMEOUT;
    let mut rto = FIRST_CHECK;
    let mut buf = vec![0u8; 2048];
    while Instant::now() < end {
        let request = ice.request(Instant::now());
        for c in &permitted {
            let _ = sock.send_to(&request, c);
        }
        let until = Instant::now() + rto;
        while let Some(left) = until.checked_duration_since(Instant::now()) {
            match net::recv_from_within(&sock, &mut buf, left) {
                Ok((n, from)) => {
                    if permitted.contains(&from) && stun::is_stun(&buf[..n]) && ice.answered(&buf[..n], Instant::now()) {
                        return Ok((sock, from, ice));
                    }
                }
                Err(e) if transient(&e) => {}
                Err(e) => return Err(io_error("could not receive", e)),
            }
        }
        rto = (rto * 2).min(MAX_CHECK);
    }
    Err(Error::new(ErrorCode::Io, format!("no answer to ICE checks from {shown}'s candidates ({}) in {} s", permitted.iter().map(|c| c.to_string()).collect::<Vec<_>>().join(", "), ICE_TIMEOUT.as_secs())).hint("the server's UDP port must be reachable, and the answer's credentials are what the checks use"))
}

/// The DTLS handshake with the server at `peer`, until it has the SRTP keys.
fn handshake(sock: &UdpSocket, peer: SocketAddr, client: &mut dtls::Client) -> std::result::Result<dtls::SrtpKeys, String> {
    let mut buf = vec![0u8; 4096];
    let mut out = client.start(Instant::now())?;
    loop {
        for d in out.drain(..) {
            let _ = sock.send_to(&d, peer);
        }
        if let Some(k) = client.keys() {
            return Ok(k.clone());
        }
        match net::recv_from_within(sock, &mut buf, Duration::from_millis(50)) {
            // DTLS is told by its first byte (RFC 7983); STUN answers that come late are not for it
            Ok((n, from)) if from == peer && n > 0 && (20..=63).contains(&buf[0]) => out.extend(client.handle(&buf[..n], Instant::now())?),
            Ok(_) => {}
            Err(e) if transient(&e) => {}
            Err(e) => return Err(e.to_string()),
        }
        out.extend(client.poll(Instant::now())?);
    }
}

/// The publisher's thread: what the session needs while the caller sends or does nothing.
struct Session {
    sock: UdpSocket,
    peer: SocketAddr,
    client: dtls::Client,
    ice: Ice,
    rx: srtp::Context,
    shared: Arc<Shared>,
    cname: String,
    last_report: Instant,
}

impl Session {
    fn run(mut self) {
        let mut buf = vec![0u8; 4096];
        while !self.shared.stop.load(Ordering::Relaxed) {
            match net::recv_from_within(&self.sock, &mut buf, POLL) {
                Ok((n, from)) if from == self.peer && n > 0 => self.datagram(&buf[..n], Instant::now()),
                Ok(_) => {}
                // an ICMP error from an earlier send, or no datagram yet
                Err(e) if transient(&e) => {}
                Err(e) => {
                    self.shared.fail(format!("receiving failed: {e}"));
                    break;
                }
            }
            if self.shared.failure().is_some() || !self.timers(Instant::now()) {
                break;
            }
        }
        // goodbye, to a server that is still there
        if self.shared.failure().is_none() {
            self.goodbye();
        }
    }

    fn send(&self, d: &[u8]) {
        let _ = self.sock.send_to(d, self.peer);
    }

    fn datagram(&mut self, d: &[u8], now: Instant) {
        match d[0] {
            0..=3 => {
                if self.ice.answered(d, now) {
                    return;
                }
                if let Some(r) = self.ice.respond(d, self.peer) {
                    self.send(&r);
                }
            }
            20..=63 => match self.client.handle(d, now) {
                Ok(out) => {
                    for o in out {
                        self.send(&o);
                    }
                    if self.client.closed() {
                        self.shared.fail("the server closed the DTLS association");
                    }
                }
                Err(e) => self.shared.fail(format!("DTLS: {e}")),
            },
            128..=191 if d.len() >= 8 && (192..=223).contains(&d[1]) => {
                let Ok(p) = self.rx.unprotect_rtcp(d) else { return };
                // an authentic RTCP packet shows the server is there
                self.ice.last_answer = now;
                self.feedback(&p, now);
            }
            _ => {}
        }
    }

    fn feedback(&mut self, rtcp: &[u8], now: Instant) {
        let mut resend = Vec::new();
        for f in packetize::feedback(rtcp) {
            let mut m = self.shared.media();
            let Media { video, audio, .. } = &mut *m;
            match f {
                packetize::Feedback::Keyframe(ssrcs) => {
                    if video.as_ref().is_some_and(|v| ssrcs.contains(&v.ssrc)) {
                        self.shared.keyframe.store(true, Ordering::Relaxed);
                    }
                }
                packetize::Feedback::Nack { media, seqs } => {
                    for s in [video.as_mut(), audio.as_mut()].into_iter().flatten().filter(|s| s.ssrc == media) {
                        resend.extend(s.resend(&seqs, now));
                    }
                }
            }
        }
        for p in resend {
            self.send(&p);
        }
    }

    /// Consent checks, sender reports and the consent timeout; false when the session is over.
    fn timers(&mut self, now: Instant) -> bool {
        if self.ice.lapsed(now) {
            self.shared.fail(format!("the server has not answered for {} s", CONSENT_TIMEOUT.as_secs()));
            return false;
        }
        if self.ice.due(now) {
            let r = self.ice.request(now);
            self.send(&r);
        }
        if now.duration_since(self.last_report) >= SR_EVERY {
            self.last_report = now;
            let wall = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
            let ntp = packetize::ntp(wall);
            let mut reports = Vec::new();
            {
                let mut m = self.shared.media();
                let Media { tx, video, audio } = &mut *m;
                for s in [video.as_ref(), audio.as_ref()].into_iter().flatten() {
                    let Some(ts) = s.timestamp_at(now) else { continue };
                    let sr = packetize::sender_report(s.ssrc, ntp, ts, s.packets, s.octets, &self.cname);
                    if let Ok(p) = tx.protect_rtcp(&sr) {
                        reports.push(p);
                    }
                }
            }
            for r in reports {
                self.send(&r);
            }
        }
        true
    }

    fn goodbye(&mut self) {
        let bye = {
            let mut m = self.shared.media();
            let Media { tx, video, audio } = &mut *m;
            let mut out = Vec::new();
            for s in [video.as_ref(), audio.as_ref()].into_iter().flatten().filter(|s| s.packets > 0) {
                if let Ok(p) = tx.protect_rtcp(&packetize::bye(s.ssrc)) {
                    out.push(p);
                }
            }
            out
        };
        for b in bye {
            self.send(&b);
        }
        for d in self.client.close_notify() {
            self.send(&d);
        }
    }
}

/// An HTTP answer.
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(name)).map(|(_, v)| v.as_str())
    }
}

/// The error for a server that refused: its status, and the start of what it said.
fn rejected(shown: &str, r: &Reply) -> Error {
    let said: String = String::from_utf8_lossy(&r.body).chars().filter(|c| !c.is_control() || *c == ' ').take(200).collect();
    let said = if said.trim().is_empty() { String::new() } else { format!(": {}", said.trim()) };
    let e = Error::new(ErrorCode::Io, format!("{shown} answered {}{said}", r.status));
    match r.status {
        401 | 403 => e.hint("the endpoint wants a bearer token (or another one)"),
        307 | 308 | 301 | 302 | 303 => e.hint(format!("redirected to {}; use that URL", r.header("location").map_or("elsewhere".to_string(), crate::url::redact))),
        _ => e,
    }
}

/// The session the `201` answer names: its `Location`, resolved against the endpoint, and the token if that is the
/// endpoint's own host and port. An `https` endpoint's session is never one on plain `http`.
fn resource_of(reply: &Reply, target: &HttpUrl, token: Option<&str>) -> Option<Resource> {
    let url = HttpUrl::parse(&crate::http::join(&target.plain(), reply.header("location")?)).ok()?;
    if target.tls && !url.tls {
        return None;
    }
    let same = url.tls == target.tls && url.host.eq_ignore_ascii_case(&target.host) && url.port == target.port;
    Some(Resource { url, token: token.filter(|_| same).map(str::to_string) })
}

/// One request on a connection of its own: through the address allow-list, over TLS for `https`, and
/// with `Authorization: Bearer` when a token is given. Redirects are not followed.
fn request(h: &Http, method: &str, url: &HttpUrl, shown: &str, payload: Option<(&str, &[u8])>, bearer: Option<&str>) -> Result<Reply> {
    if !url.tls {
        crate::fips::check_transport("http")?;
    }
    let tcp = net::connect(&url.host, url.port, CONNECT_TIMEOUT, &h.allow, shown)?;
    let mut conn = if url.tls {
        match &h.tls {
            Some(c) => tls::connect_with(tcp, &url.host, c.clone(), shown)?,
            None => tls::connect(tcp, &url.host, Some(b"http/1.1"), shown)?,
        }
    } else {
        Conn::Plain(tcp)
    };
    conn.tcp().set_read_timeout(Some(IO_TIMEOUT))?;
    conn.tcp().set_write_timeout(Some(IO_TIMEOUT))?;
    let mut head = format!("{method} {} HTTP/1.1\r\nHost: {}\r\nUser-Agent: {USER_AGENT}\r\nAccept: application/sdp\r\nConnection: close\r\n", url.path, url.authority);
    if let Some(t) = bearer {
        head += &format!("Authorization: Bearer {t}\r\n");
    }
    match payload {
        Some((ct, body)) => head += &format!("Content-Type: {ct}\r\nContent-Length: {}\r\n\r\n", body.len()),
        None => head += "\r\n",
    }
    let mut message = head.into_bytes();
    message.extend_from_slice(payload.map_or(&[][..], |p| p.1));
    conn.write_all(&message).map_err(|e| io_error(&format!("could not send to {shown}"), e))?;
    conn.flush().map_err(|e| io_error(&format!("could not send to {shown}"), e))?;
    let mut buf = Vec::new();
    let mut chunk = vec![0u8; 8192];
    let reply = loop {
        match parse_reply(&buf, false) {
            Parsed::Done(r) => break r,
            Parsed::Bad(why) => return Err(Error::new(ErrorCode::Io, format!("{shown} sent a bad HTTP answer: {why}"))),
            Parsed::More => {}
        }
        match conn.read(&mut chunk) {
            Ok(0) => match parse_reply(&buf, true) {
                Parsed::Done(r) => break r,
                Parsed::Bad(why) => return Err(Error::new(ErrorCode::Io, format!("{shown} sent a bad HTTP answer: {why}"))),
                Parsed::More => return Err(Error::new(ErrorCode::Io, format!("{shown} closed the connection before it answered"))),
            },
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
            // a TLS connection closed without close_notify, after the answer
            Err(e) if !buf.is_empty() && matches!(e.kind(), std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::ConnectionReset) => match parse_reply(&buf, true) {
                Parsed::Done(r) => break r,
                _ => return Err(io_error(&format!("{shown} closed the connection before it finished answering"), e)),
            },
            Err(e) => return Err(io_error(&format!("could not read {shown}'s answer"), e)),
        }
    };
    conn.close();
    Ok(reply)
}

enum Parsed {
    Done(Reply),
    More,
    Bad(&'static str),
}

/// An HTTP/1.1 answer in `buf`: complete when its body has come, by `Content-Length`, by chunks, or
/// (`eof`) by the connection's close. Interim 1xx answers are skipped.
fn parse_reply(buf: &[u8], eof: bool) -> Parsed {
    let mut at = 0;
    loop {
        let Some(end) = buf[at..].windows(4).position(|w| w == b"\r\n\r\n") else {
            return if buf.len() - at > MAX_HEAD { Parsed::Bad("headers too long") } else { Parsed::More };
        };
        if end > MAX_HEAD {
            return Parsed::Bad("headers too long");
        }
        let Ok(head) = std::str::from_utf8(&buf[at..at + end]) else { return Parsed::Bad("headers are not text") };
        let mut lines = head.split("\r\n");
        let Some(status) = lines.next().filter(|l| l.starts_with("HTTP/1.")).and_then(|l| l.split_whitespace().nth(1)).and_then(|s| s.parse::<u16>().ok()) else { return Parsed::Bad("not HTTP/1.x") };
        let headers: Vec<(String, String)> = lines.filter_map(|l| l.split_once(':')).map(|(k, v)| (k.trim().to_string(), v.trim().to_string())).collect();
        let body_at = at + end + 4;
        if (100..200).contains(&status) {
            at = body_at;
            continue;
        }
        let header = |n: &str| headers.iter().find(|(k, _)| k.eq_ignore_ascii_case(n)).map(|(_, v)| v.as_str());
        let body = &buf[body_at..];
        let done = |body: Vec<u8>| Parsed::Done(Reply { status, headers: headers.clone(), body });
        if header("transfer-encoding").is_some_and(|t| t.to_ascii_lowercase().contains("chunked")) {
            return match dechunk(body) {
                Chunks::Done(b) => done(b),
                Chunks::More if eof => Parsed::Bad("the body ended inside a chunk"),
                Chunks::More => Parsed::More,
                Chunks::Bad(why) => Parsed::Bad(why),
            };
        }
        if let Some(len) = header("content-length") {
            let Ok(len) = len.parse::<usize>() else { return Parsed::Bad("bad Content-Length") };
            if len > MAX_BODY {
                return Parsed::Bad("body too long");
            }
            return if body.len() >= len {
                done(body[..len].to_vec())
            } else if eof {
                Parsed::Bad("the body ended early")
            } else {
                Parsed::More
            };
        }
        // no length: no body for these, and the connection's close ends any other
        if status == 204 || status == 304 {
            return done(Vec::new());
        }
        if body.len() > MAX_BODY {
            return Parsed::Bad("body too long");
        }
        return if eof { done(body.to_vec()) } else { Parsed::More };
    }
}

enum Chunks {
    Done(Vec<u8>),
    More,
    Bad(&'static str),
}

/// The body of a chunked message (RFC 9112 §7.1) once its last chunk and trailers have come.
fn dechunk(mut b: &[u8]) -> Chunks {
    let mut out = Vec::new();
    loop {
        let Some(nl) = b.windows(2).position(|w| w == b"\r\n") else { return if b.len() > 1024 { Chunks::Bad("a chunk size line is too long") } else { Chunks::More } };
        let Some(size) = std::str::from_utf8(&b[..nl]).ok().and_then(|l| usize::from_str_radix(l.split(';').next().unwrap_or("").trim(), 16).ok()) else { return Chunks::Bad("a bad chunk size") };
        b = &b[nl + 2..];
        if size == 0 {
            // trailers, up to the empty line
            for _ in 0..64 {
                let Some(nl) = b.windows(2).position(|w| w == b"\r\n") else { return Chunks::More };
                let empty = nl == 0;
                b = &b[nl + 2..];
                if empty {
                    return Chunks::Done(out);
                }
            }
            return Chunks::Bad("too many trailer lines");
        }
        if out.len() + size > MAX_BODY {
            return Chunks::Bad("body too long");
        }
        let Some(chunk) = b.get(..size) else { return Chunks::More };
        out.extend_from_slice(chunk);
        b = &b[size..];
        match b.get(..2) {
            Some(b"\r\n") => b = &b[2..],
            Some(_) => return Chunks::Bad("a chunk is not followed by CRLF"),
            None => return Chunks::More,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn answer() -> sdp::Answer {
        sdp::Answer { ufrag: "srvU".into(), pwd: "serverpasswordserverpw1".into(), fingerprint: [0; 32], candidates: vec!["127.0.0.1:9".parse().unwrap()], media: Vec::new() }
    }

    #[test]
    fn checks_are_signed_with_the_servers_password_and_answers_are_matched() {
        let t0 = Instant::now();
        let mut ice = Ice::new("locU", "localpasswordlocalpw12", &answer(), t0);
        let req = ice.request(t0);
        // the server (this crate's own endpoint code) accepts it: its user name is "server:local", with USE-CANDIDATE
        let b = stun::check_request(&req, "srvU", "locU", "serverpasswordserverpw1").expect("the server verifies the check");
        assert!(b.use_candidate);
        // and not under any other password, user name, or with a bit changed
        assert!(stun::check_request(&req, "srvU", "locU", "serverpasswordserverpw2").is_none());
        assert!(stun::check_request(&req, "srvU", "other", "serverpasswordserverpw1").is_none());
        for i in 0..req.len() * 8 {
            let mut bad = req.clone();
            bad[i / 8] ^= 1 << (i % 8);
            assert!(stun::check_request(&bad, "srvU", "locU", "serverpasswordserverpw1").is_none(), "bit {i}");
        }
        // the response must come signed by the server, for the transaction
        let from: SocketAddr = "192.0.2.1:4000".parse().unwrap();
        let forged = stun::success(&b.txid, from, "not-the-servers-password!!");
        assert!(!ice.answered(&forged, t0));
        let other = stun::success(&[7; 12], from, "serverpasswordserverpw1");
        assert!(!ice.answered(&other, t0));
        let good = stun::success(&b.txid, from, "serverpasswordserverpw1");
        let mut flipped = good.clone();
        flipped[22] ^= 1;
        assert!(!ice.answered(&flipped, t0));
        assert!(ice.answered(&good, t0 + Duration::from_millis(5)));
        // a transaction is answered once
        assert!(!ice.answered(&good, t0 + Duration::from_millis(6)));
        // short or empty datagrams are nothing
        for n in 0..20 {
            assert!(!ice.answered(&good[..n], t0));
        }
    }

    #[test]
    fn consent_is_checked_every_5_s_and_lapses_after_30() {
        let t0 = Instant::now();
        let mut ice = Ice::new("locU", "localpasswordlocalpw12", &answer(), t0);
        assert!(ice.due(t0), "the first check goes at once");
        let req = ice.request(t0);
        let txid: [u8; 12] = req[8..20].try_into().unwrap();
        assert!(!ice.due(t0 + Duration::from_millis(4999)) && ice.due(t0 + Duration::from_secs(5)));
        assert!(ice.answered(&stun::success(&txid, "192.0.2.1:1".parse().unwrap(), "serverpasswordserverpw1"), t0 + Duration::from_millis(10)));
        // answers keep it alive
        assert!(!ice.lapsed(t0 + Duration::from_secs(30)));
        assert!(!ice.lapsed(t0 + Duration::from_millis(10) + CONSENT_TIMEOUT));
        // 30 s of silence do not
        assert!(ice.lapsed(t0 + Duration::from_millis(11) + CONSENT_TIMEOUT));
        // requests left unanswered are remembered only a few at a time
        for k in 0..100 {
            ice.request(t0 + Duration::from_secs(k));
        }
        assert!(ice.pending.len() <= 16);
    }

    #[test]
    fn a_servers_own_check_is_answered_only_if_it_authenticates() {
        let ice = Ice::new("locU", "localpasswordlocalpw12", &answer(), Instant::now());
        // the server's check carries "local:server" and is signed with the local password
        let from: SocketAddr = "127.0.0.1:9".parse().unwrap();
        let theirs = stun::request(&[5; 12], "locU:srvU", "localpasswordlocalpw12", false, [1; 8]);
        let r = ice.respond(&theirs, from).expect("answered");
        assert_eq!(stun::check_success(&r, &[5; 12], "localpasswordlocalpw12"), Some(from));
        assert!(ice.respond(&stun::request(&[5; 12], "locU:srvU", "wrong-password-wrong-password", false, [1; 8]), from).is_none());
        assert!(ice.respond(&theirs[..30], from).is_none());
    }

    #[test]
    fn nacks_are_answered_from_the_last_512_packets_not_too_often() {
        let (mut tx, mut rx) = (srtp::Context::new(srtp::Profile::AeadAes128Gcm, &[1; 16], &[2; 12]).unwrap(), srtp::Context::new(srtp::Profile::AeadAes128Gcm, &[1; 16], &[2; 12]).unwrap());
        let mut s = Stream::new(102, 0x1234, 90_000);
        s.seq = 65_000;
        let t0 = Instant::now();
        let mut sent = Vec::new();
        for k in 0..700u64 {
            sent.push(s.packet(&mut tx, k % 3 == 0, k * 3000, &[k as u8; 20], t0).unwrap());
        }
        assert_eq!(s.history.len(), HISTORY);
        // the packets sent decrypt, with timestamps and sequence numbers in order across the wrap
        let p = rx.unprotect_rtp(&sent[0]).unwrap();
        assert_eq!(u16::from_be_bytes([p[2], p[3]]), 65_000);
        assert_eq!(p[1], 0x80 | 102);
        let last = rx.unprotect_rtp(&sent[699]).unwrap();
        assert_eq!(u16::from_be_bytes([last[2], last[3]]), 65_000u16.wrapping_add(699));
        assert_eq!((s.packets, s.octets), (700, 14_000));
        // the oldest 188 are gone, the rest come back as they were sent
        let first_kept = 700 - HISTORY;
        let seq = |k: usize| 65_000u16.wrapping_add(k as u16);
        assert!(s.resend(&[seq(first_kept - 1), seq(5000)], t0).is_empty());
        let got = s.resend(&[seq(first_kept), seq(699)], t0);
        assert_eq!(got, [sent[first_kept].clone(), sent[699].clone()]);
        // and not again at once
        assert!(s.resend(&[seq(699)], t0 + Duration::from_millis(10)).is_empty());
        assert_eq!(s.resend(&[seq(699)], t0 + RESEND_EVERY).len(), 1);
        // the clock runs on from the last packet
        let ts = s.timestamp_at(t0 + Duration::from_secs(1)).unwrap();
        assert_eq!(ts, (699u32 * 3000).wrapping_add(s.offset).wrapping_add(90_000));
    }

    #[test]
    fn http_answers_are_read_by_length_by_chunks_and_by_close() {
        let ok = |p: Parsed| match p {
            Parsed::Done(r) => r,
            Parsed::More => panic!("more"),
            Parsed::Bad(w) => panic!("{w}"),
        };
        let r = ok(parse_reply(b"HTTP/1.1 201 Created\r\nLocation: /s/1\r\nContent-Length: 5\r\n\r\nhelloEXTRA", false));
        assert_eq!((r.status, r.header("LOCATION"), r.body.as_slice()), (201, Some("/s/1"), &b"hello"[..]));
        assert!(matches!(parse_reply(b"HTTP/1.1 201 Created\r\nContent-Length: 5\r\n\r\nhel", false), Parsed::More));
        assert!(matches!(parse_reply(b"HTTP/1.1 201 Created\r\nContent-Length: 5\r\n\r\nhel", true), Parsed::Bad(_)));
        let r = ok(parse_reply(b"HTTP/1.1 100 Continue\r\n\r\nHTTP/1.1 201 Created\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n2;ext=1\r\nde\r\n0\r\nX: y\r\n\r\n", false));
        assert_eq!((r.status, r.body.as_slice()), (201, &b"abcde"[..]));
        assert!(matches!(parse_reply(b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n", false), Parsed::More));
        let r = ok(parse_reply(b"HTTP/1.1 200 OK\r\n\r\nuntil close", true));
        assert_eq!(r.body, b"until close");
        assert!(matches!(parse_reply(b"HTTP/1.1 200 OK\r\n\r\nuntil close", false), Parsed::More));
        assert_eq!(ok(parse_reply(b"HTTP/1.1 204 No Content\r\n\r\n", false)).status, 204);
        // bad ones
        for bad in [&b"SSH-2.0\r\n\r\n"[..], b"HTTP/1.1 abc\r\n\r\n", b"HTTP/1.1 200 OK\r\nContent-Length: x\r\n\r\n", b"HTTP/1.1 200 OK\r\nContent-Length: 99999999\r\n\r\n", b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\nzz\r\n", b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n1\r\nabXX"] {
            assert!(matches!(parse_reply(bad, true), Parsed::Bad(_)), "{:?}", String::from_utf8_lossy(bad));
        }
        // anything cut short or changed is More or Bad or Done, never a panic
        let whole = b"HTTP/1.1 201 Created\r\nLocation: /x\r\nTransfer-Encoding: chunked\r\n\r\n3\r\nabc\r\n0\r\n\r\n";
        for n in 0..whole.len() {
            let _ = parse_reply(&whole[..n], false);
            let mut b = whole.to_vec();
            b[n] ^= 0x55;
            let _ = parse_reply(&b, true);
        }
    }

    #[test]
    fn options_and_arguments_are_checked_before_anything_is_sent() {
        let o = WhipOptions { profile_level_id: "zz".into(), ..WhipOptions::default() };
        assert!(WhipPublisher::connect("http://127.0.0.1:9/whip", None, o).err().unwrap().message.contains("profile_level_id"));
        let o = WhipOptions { video: false, audio: false, ..WhipOptions::default() };
        assert!(WhipPublisher::connect("http://127.0.0.1:9/whip", None, o).err().unwrap().message.contains("video or audio"));
        for token in ["", "a b", "a\r\nX: y"] {
            let e = WhipPublisher::connect("http://127.0.0.1:9/whip", Some(token), WhipOptions::default()).err().unwrap();
            assert!(e.message.contains("token"), "{e:?}");
        }
        let e = WhipPublisher::connect("ftp://127.0.0.1/whip", None, WhipOptions::default()).err().unwrap();
        assert!(e.message.contains("http"), "{e:?}");
    }

    /// A session whose peer is a socket of the test's, with keys the test holds both ends of.
    fn session() -> (Session, UdpSocket, srtp::Context) {
        let server = UdpSocket::bind("127.0.0.1:0").unwrap();
        let gcm = srtp::Profile::AeadAes128Gcm;
        let mut tx = srtp::Context::new(gcm, &[4; 16], &[5; 12]).unwrap();
        let (mut video, mut audio) = (Stream::new(102, 0xAAAA, 90_000), Stream::new(111, 0xBBBB, 48_000));
        let now = Instant::now();
        for k in 0..3u64 {
            video.packet(&mut tx, k == 2, k * 3000, &[k as u8; 10], now).unwrap();
            audio.packet(&mut tx, k == 0, k * 960, &[k as u8; 10], now).unwrap();
        }
        let shared = Arc::new(Shared { media: Mutex::new(Media { tx, video: Some(video), audio: Some(audio) }), keyframe: AtomicBool::new(false), stop: AtomicBool::new(false), failure: Mutex::new(None) });
        let id = Arc::new(dtls::Identity::generate("test").unwrap());
        let session = Session {
            sock: UdpSocket::bind("127.0.0.1:0").unwrap(),
            peer: server.local_addr().unwrap(),
            client: dtls::Client::new(id, [0; 32]).unwrap(),
            ice: Ice::new("locU", "localpasswordlocalpw12", &answer(), now),
            rx: srtp::Context::new(gcm, &[6; 16], &[7; 12]).unwrap(),
            shared,
            cname: "ironav-test".into(),
            last_report: now,
        };
        (session, server, srtp::Context::new(gcm, &[6; 16], &[7; 12]).unwrap())
    }

    #[test]
    fn the_servers_rtcp_asks_for_keyframes_and_resends_and_forgeries_ask_for_nothing() {
        let (mut s, server, mut server_tx) = session();
        let now = Instant::now();
        let flag = |s: &Session| s.shared.keyframe.load(Ordering::Relaxed);
        let heard = |wait: u64| {
            let mut buf = vec![0u8; 2048];
            net::recv_from_within(&server, &mut buf, Duration::from_millis(wait)).ok().map(|(n, _)| buf[..n].to_vec())
        };
        // a loss indication for some other stream says nothing to us
        let pli = |media: u32| [&[0x81u8, 206, 0, 2, 0, 0, 0, 9][..], &media.to_be_bytes()].concat();
        s.datagram(&server_tx.protect_rtcp(&pli(0xCCCC)).unwrap(), now);
        assert!(!flag(&s));
        // one that is not authentic is ignored, and so is noise that looks like RTCP
        let mut forged = server_tx.protect_rtcp(&pli(0xAAAA)).unwrap();
        *forged.last_mut().unwrap() ^= 1;
        s.datagram(&forged, now);
        s.datagram(&[0x80, 206, 0, 2, 0, 0, 0, 9, 0, 0, 0xAA, 0xAA], now);
        assert!(!flag(&s));
        s.datagram(&server_tx.protect_rtcp(&pli(0xAAAA)).unwrap(), now);
        assert!(flag(&s), "a PLI for the video");
        // a NACK for the audio's 2nd packet and the video's 1st and 3rd is answered with those packets as sent
        let (audio_seq, video_seq) = {
            let m = s.shared.media();
            (m.audio.as_ref().unwrap().seq.wrapping_sub(2), m.video.as_ref().unwrap().seq.wrapping_sub(3))
        };
        let (want_audio, want_video) = {
            let m = s.shared.media();
            let video = &m.video.as_ref().unwrap().history;
            (m.audio.as_ref().unwrap().history[1].packet.clone(), [video[0].packet.clone(), video[2].packet.clone()])
        };
        let nack = |media: u32, pid: u16, blp: u16| [&[0x81u8, 205, 0, 3, 0, 0, 0, 9][..], &media.to_be_bytes(), &pid.to_be_bytes(), &blp.to_be_bytes()].concat();
        s.datagram(&server_tx.protect_rtcp(&nack(0xBBBB, audio_seq, 0)).unwrap(), now);
        s.datagram(&server_tx.protect_rtcp(&nack(0xAAAA, video_seq, 0b10)).unwrap(), now);
        assert_eq!(heard(1000), Some(want_audio));
        assert_eq!(heard(1000), Some(want_video[0].clone()));
        assert_eq!(heard(1000), Some(want_video[1].clone()));
        assert_eq!(heard(50), None);
        // the same NACK straight away is not answered again
        s.datagram(&server_tx.protect_rtcp(&nack(0xAAAA, video_seq, 0)).unwrap(), now + Duration::from_millis(5));
        assert_eq!(heard(50), None);
        // sender reports go out each second for each stream that has sent, authentic under the publisher's keys
        assert!(s.timers(now + Duration::from_secs(1)));
        let mut rx = srtp::Context::new(srtp::Profile::AeadAes128Gcm, &[4; 16], &[5; 12]).unwrap();
        let reports: Vec<Vec<u8>> = std::iter::from_fn(|| heard(200)).filter_map(|d| rx.unprotect_rtcp(&d).ok()).collect();
        let ssrcs: Vec<u32> = reports.iter().filter(|r| r[1] == 200).map(|r| u32::from_be_bytes([r[4], r[5], r[6], r[7]])).collect();
        assert_eq!(ssrcs, [0xAAAA, 0xBBBB]);
        assert!(reports.iter().all(|r| r[0] == 0x80 && r[28] == 0x81 && r[29] == 202), "each is a sender report and an SDES");
        let video = &reports[0];
        assert_eq!(u32::from_be_bytes([video[20], video[21], video[22], video[23]]), 3, "packets sent");
        assert_eq!(u32::from_be_bytes([video[24], video[25], video[26], video[27]]), 30, "octets sent");
    }

    #[test]
    fn a_session_that_hears_nothing_for_30_s_ends_and_one_that_is_closed_says_goodbye() {
        let (mut s, server, _) = session();
        let now = Instant::now();
        // consent checks go out on their own: the first at once, signed for the server
        assert!(s.timers(now));
        let mut buf = vec![0u8; 2048];
        let (n, _) = net::recv_from_within(&server, &mut buf, Duration::from_secs(2)).unwrap();
        assert!(stun::check_request(&buf[..n], "srvU", "locU", "serverpasswordserverpw1").is_some());
        assert!(!s.timers(now + CONSENT_TIMEOUT + Duration::from_secs(1)));
        // closing sends a BYE for each stream that sent, and a close_notify
        s.goodbye();
        let mut got = Vec::new();
        while let Ok((n, _)) = net::recv_from_within(&server, &mut buf, Duration::from_millis(200)) {
            got.push(buf[..n].to_vec());
        }
        assert!(got.len() >= 2, "{} datagrams", got.len());
    }
}