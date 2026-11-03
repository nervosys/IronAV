//! An SRT sender, in live mode: it takes MPEG-TS bytes from `write`, puts them in 1316-byte
//! payloads (seven TS packets), and sends them over SRT for a receiver (an `srt-live-transmit`, a
//! player, ffmpeg, IronAV's own `srt://` source) to play.
//!
//! `srt://host:port` calls a receiver that listens; `srt://:port?mode=listener` waits for one
//! receiver to call, and goes back to waiting when it leaves. The handshake and the key material
//! are the receiver's (see the parent module), seen from the other end.
//!
//! What the sender does once connected:
//! - each payload goes out at once, stamped with the microseconds since the connection began (the
//!   monotonic clock), and is kept until the receiver's ACK passes it;
//! - a loss report (NAK) is answered with the packets named, marked as retransmissions; a packet
//!   that has been given up on is answered with a drop request, so the receiver stops asking;
//! - a full ACK is answered with an ACKACK at once, and its round-trip figures pace the timer that
//!   sends the oldest unacknowledged packets again when no ACK has moved for a while (the last
//!   packets of a burst, whose loss no later packet reveals);
//! - a packet still unacknowledged after the latency (and a quarter more) is given up on rather
//!   than kept, so the send buffer is bounded by time as well as by 8192 packets: too-late packet
//!   drop;
//! - keep-alives go out when the line is quiet, a SHUTDOWN from the receiver ends the connection,
//!   and five seconds of silence breaks it. `finish` waits for the receiver to acknowledge what
//!   was sent, and for the receiver to have played it (a latency; not longer than a few latencies
//!   in all), before it says goodbye, since a receiver that hears goodbye drops what it holds.
//!
//! With `?passphrase=` the sender makes the stream encrypting key (`?pbkeylen=` 16, 24 or 32 bytes,
//! 16 by default) and its salt, sends them wrapped in the key material of its handshake, and
//! encrypts each payload with AES-CTR under the even key. As a listener it uses the key the caller
//! sends, as libsrt does. The key is not refreshed: a connection that has sent 2^30 packets (some
//! 1.4 TB) is ended, so that a key is never used past its counter space, and a caller starts again
//! with a new one.
//!
//! The caller of `write` supplies the bytes at the pace of the media. `write` never waits for the
//! network: the bytes go into a bounded queue, and a queue that is full (a line slower than the
//! stream) drops what does not fit and counts it. A connection that is gone makes `write` fail.

use super::*;
use crate::sink::{Sink, SinkInfo, SinkStats};
use std::sync::Mutex;
use std::thread::JoinHandle;

/// Seven TS packets: what fits a 1500-byte MTU under the SRT header.
const PAYLOAD: usize = 1316;
/// A partial payload waits this long for more bytes before it is sent as it is.
const HOLD: Duration = Duration::from_millis(10);
/// The most payloads waiting for the sending thread.
const SEND_QUEUE: usize = 2048;
/// The most packets the send buffer keeps for retransmission.
const SEND_WINDOW: usize = WINDOW;
/// The longest `finish` waits for the receiver to acknowledge the end of the stream.
const DRAIN_MAX: Duration = Duration::from_secs(2);
/// The packets one key encrypts at most.
const KEY_LIFE: u64 = 1 << 30;
/// How long a socket read waits before the timers are looked at.
const SEND_POLL: Duration = Duration::from_millis(1);

/// A packet sent and not yet acknowledged.
struct Sent {
    seq: u32,
    ts: u32,
    kk: u8,
    /// As it goes on the line (encrypted).
    payload: Vec<u8>,
    /// When it was first sent, and when last (microseconds).
    born: i64,
    sent: i64,
}

#[derive(Debug, Clone, Default)]
struct SendStats {
    packets: u64,
    bytes: u64,
    retransmitted: u64,
    /// Given up on before the receiver acknowledged them.
    dropped: u64,
}

struct TxConfig {
    my_id: u32,
    peer_id: u32,
    isn: u32,
    latency_ms: u16,
    crypto: Option<Decryptor>,
    /// The listener's conclusion response, sent again if the caller asks again.
    hs_reply: Option<Vec<u8>>,
}

/// The protocol state of a connection from the sending end, with no socket: payloads in (`push`),
/// datagrams in (`on_datagram`), time passing (`tick`) and datagrams out (`take_output`).
struct Sender {
    my_id: u32,
    peer_id: u32,
    isn: u32,
    latency_us: i64,
    t0: Instant,
    next_seq: u32,
    /// Sent and not acknowledged, oldest first, one for each sequence number from the first up
    /// to `next_seq`.
    buf: VecDeque<Sent>,
    crypto: Option<Decryptor>,
    hs_reply: Option<Vec<u8>>,
    out: Vec<Vec<u8>>,
    rtt: i64,
    rttvar: i64,
    /// Where the receiver's ACKs have got to.
    acked_to: u32,
    /// When an ACK last moved, and when the timer last sent packets again.
    last_progress: i64,
    last_rexmit: i64,
    /// When the last payload was pushed.
    last_data: i64,
    last_sent: i64,
    last_recv: i64,
    state: State,
    stats: SendStats,
}

impl Sender {
    fn new(c: TxConfig, now: Instant) -> Sender {
        Sender {
            my_id: c.my_id,
            peer_id: c.peer_id,
            isn: c.isn & SEQ_MASK,
            latency_us: c.latency_ms as i64 * 1000,
            t0: now,
            next_seq: c.isn & SEQ_MASK,
            buf: VecDeque::new(),
            crypto: c.crypto,
            hs_reply: c.hs_reply,
            out: Vec::new(),
            rtt: 100_000,
            rttvar: 50_000,
            acked_to: c.isn & SEQ_MASK,
            last_progress: 0,
            last_rexmit: 0,
            last_data: 0,
            last_sent: 0,
            last_recv: 0,
            state: State::Open,
            stats: SendStats::default(),
        }
    }

    fn us(&self, t: Instant) -> i64 {
        t.saturating_duration_since(self.t0).as_micros() as i64
    }

    fn send(&mut self, kind: u16, sub: u16, info: u32, cif: &[u8], now: i64) {
        self.out.push(control(kind, sub, info, now as u32, self.peer_id, cif));
        self.last_sent = now;
    }

    fn take_output(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.out)
    }

    fn shutdown_packet(&self, now: Instant) -> Vec<u8> {
        // libsrt drops one without its four (empty) bytes of control information
        control(C_SHUTDOWN, 0, 0, self.us(now) as u32, self.peer_id, &[0; 4])
    }

    /// The first sequence number still held (the next one to send when none is).
    fn floor(&self) -> u32 {
        self.buf.front().map_or(self.next_seq, |p| p.seq)
    }

    /// Everything sent has been acknowledged (or given up on), and the receiver has had the
    /// latency to play the last of it: a receiver that hears goodbye drops what it still holds.
    fn drained(&self, now: Instant) -> bool {
        self.buf.is_empty() && self.us(now) - self.last_data >= self.latency_us + 30_000
    }

    /// The message number of a packet: one message to a packet, counted from 1.
    fn msg_of(&self, seq: u32) -> u32 {
        (seq_off(seq, self.isn).max(0) as u32 % 0x03ff_ffff) + 1
    }

    /// A data packet: a solo message, the key bits, and the retransmission flag.
    fn data(&self, p: &Sent, rexmit: bool) -> Vec<u8> {
        let info = 0xc000_0000 | ((p.kk as u32) << 27) | if rexmit { 1 << 26 } else { 0 } | self.msg_of(p.seq);
        let mut v = Vec::with_capacity(HEADER + p.payload.len());
        v.extend_from_slice(&p.seq.to_be_bytes());
        v.extend_from_slice(&info.to_be_bytes());
        v.extend_from_slice(&p.ts.to_be_bytes());
        v.extend_from_slice(&self.peer_id.to_be_bytes());
        v.extend_from_slice(&p.payload);
        v
    }

    /// Sends a payload that was written at `at`.
    fn push(&mut self, mut data: Vec<u8>, at: Instant, now: Instant) {
        if self.state != State::Open || data.is_empty() {
            return;
        }
        let seq = self.next_seq;
        let kk = match &self.crypto {
            Some(c) => {
                if self.stats.packets >= KEY_LIFE {
                    self.state = State::Broken("the key has encrypted as many packets as it should".into());
                    return;
                }
                if !c.apply(1, seq, &mut data) {
                    self.state = State::Broken("the payload could not be encrypted".into());
                    return;
                }
                1
            }
            None => 0,
        };
        let n = self.us(now);
        if self.buf.is_empty() {
            self.last_progress = n;
        }
        self.last_data = n;
        self.stats.packets += 1;
        self.stats.bytes += data.len() as u64;
        let p = Sent { seq, ts: self.us(at) as u32, kk, payload: data, born: n, sent: n };
        self.next_seq = seq_add(seq, 1);
        self.out.push(self.data(&p, false));
        self.last_sent = n;
        self.buf.push_back(p);
        self.give_up(n);
    }

    fn on_datagram(&mut self, d: &[u8], now: Instant) {
        let n = self.us(now);
        match parse(d) {
            // a caller that asks again sends its conclusion to socket 0, as it did the first time
            Some(Packet::Control { kind, info, cif, dst, .. }) if dst == self.my_id || (kind == C_HANDSHAKE && dst == 0) => {
                self.last_recv = n;
                self.on_control(kind, info, cif, n);
            }
            Some(Packet::Data { dst, .. }) if dst == self.my_id => self.last_recv = n,
            _ => {}
        }
    }

    fn on_control(&mut self, kind: u16, info: u32, cif: &[u8], now: i64) {
        match kind {
            C_HANDSHAKE => {
                // the caller did not get the response: say it again
                if let (Some(h), Some(r)) = (Handshake::parse(cif), &self.hs_reply) {
                    if h.kind == HS_CONCLUSION {
                        self.out.push(r.clone());
                        self.last_sent = now;
                    }
                }
            }
            C_SHUTDOWN => self.state = State::Shutdown,
            C_ACK => self.on_ack(info, cif, now),
            C_NAK => self.on_nak(cif, now),
            // keep-alives, key refreshes the receiver has no reason to send, and the rest
            _ => {}
        }
    }

    fn on_ack(&mut self, info: u32, cif: &[u8], now: i64) {
        if cif.len() < 4 {
            return;
        }
        let point = word(cif, 0) & SEQ_MASK;
        if cif.len() >= 16 {
            // a full ACK carries the receiver's round trip figures; the ACKACK lets it measure them
            let (rtt, var) = (word(cif, 4) as i64, word(cif, 8) as i64);
            if (1..=10_000_000).contains(&rtt) {
                self.rttvar = (3 * self.rttvar + var.min(10_000_000)) / 4;
                self.rtt = (7 * self.rtt + rtt) / 8;
            }
            if info != 0 {
                // libsrt refuses an ACKACK without its four (empty) bytes of control information
                self.send(C_ACKACK, 0, info, &[0; 4], now);
            }
        }
        // an ACK for what was never sent is not believed
        if seq_off(point, self.next_seq) > 0 {
            return;
        }
        let gone = seq_off(point, self.floor());
        if gone > 0 {
            self.buf.drain(..gone as usize);
            self.last_progress = now;
        }
        if seq_off(point, self.acked_to) > 0 {
            self.acked_to = point;
        }
    }

    fn on_nak(&mut self, cif: &[u8], now: i64) {
        let mut it = cif.as_chunks::<4>().0.iter().map(|c| u32::from_be_bytes(*c));
        // a report names ranges of what is missing; a hostile one is bounded
        let mut budget = WINDOW;
        // the ones asked for that are no longer held, and not acknowledged either
        let mut dropped: Option<(u32, u32)> = None;
        while let Some(w) = it.next() {
            let (a, b) = if w & 0x8000_0000 != 0 {
                let Some(e) = it.next() else { break };
                (w & SEQ_MASK, e & SEQ_MASK)
            } else {
                (w, w)
            };
            let len = seq_off(b, a);
            if len < 0 || len as usize >= WINDOW {
                continue;
            }
            for i in 0..=len as u32 {
                if budget == 0 {
                    break;
                }
                budget -= 1;
                let s = seq_add(a, i);
                if seq_off(s, self.next_seq) >= 0 {
                    continue;
                }
                let idx = seq_off(s, self.floor());
                if idx >= 0 {
                    let pkt = self.data(&self.buf[idx as usize], true);
                    self.buf[idx as usize].sent = now;
                    self.out.push(pkt);
                    self.stats.retransmitted += 1;
                } else if seq_off(s, self.acked_to) >= 0 {
                    dropped = Some(match dropped {
                        Some((f, l)) if seq_off(s, l) == 1 => (f, s),
                        Some(run) => {
                            self.drop_request(run, now);
                            (s, s)
                        }
                        None => (s, s),
                    });
                }
            }
        }
        if let Some(run) = dropped {
            self.drop_request(run, now);
        }
        self.last_sent = now;
    }

    /// Tells the receiver the packets `first..=last` will not come.
    fn drop_request(&mut self, (first, last): (u32, u32), now: i64) {
        let mut cif = first.to_be_bytes().to_vec();
        cif.extend_from_slice(&last.to_be_bytes());
        let msg = self.msg_of(first);
        self.send(C_DROPREQ, 0, msg, &cif, now);
    }

    /// Lets go of what is too old to be of use to the receiver (it gives up on a packet after the
    /// latency), or more than the buffer holds, and says so.
    fn give_up(&mut self, now: i64) {
        let limit = self.latency_us * 5 / 4 + 20_000;
        let mut run: Option<(u32, u32)> = None;
        while let Some(p) = self.buf.front() {
            if now - p.born <= limit && self.buf.len() <= SEND_WINDOW {
                break;
            }
            let seq = p.seq;
            self.buf.pop_front();
            self.stats.dropped += 1;
            run = Some((run.map_or(seq, |r| r.0), seq));
        }
        if let Some(r) = run {
            self.drop_request(r, now);
        }
    }

    fn tick(&mut self, now: Instant) {
        let n = self.us(now);
        if n - self.last_recv > EXPIRE {
            self.state = State::Broken(format!("the receiver sent nothing for {} s", EXPIRE / 1_000_000));
            return;
        }
        self.give_up(n);
        // no ACK has moved for a while: the oldest packets may be lost without a later one to show it.
        // The wait is a round trip, four deviations and 20 ms, but not so long that a packet sent
        // again could no longer arrive before the receiver gives up on it (half the latency).
        let rto = (self.rtt + 4 * self.rttvar + 20_000).min(self.latency_us / 2).max(20_000);
        if !self.buf.is_empty() && n - self.last_progress >= rto && n - self.last_rexmit >= rto {
            self.last_rexmit = n;
            for i in 0..self.buf.len().min(32) {
                if n - self.buf[i].sent >= rto {
                    let pkt = self.data(&self.buf[i], true);
                    self.buf[i].sent = n;
                    self.out.push(pkt);
                    self.stats.retransmitted += 1;
                }
            }
            self.last_sent = n;
        }
        if n - self.last_sent >= KEEPALIVE {
            self.send(C_KEEPALIVE, 0, 0, &[], n);
        }
    }

    /// Fills in the figures a sink reports.
    fn report(&self, st: &mut SinkStats, carry: &SendStats) {
        st.packets = carry.packets + self.stats.packets;
        st.bytes = carry.bytes + self.stats.bytes;
        st.retransmitted = carry.retransmitted + self.stats.retransmitted;
        st.dropped = carry.dropped + self.stats.dropped;
        st.rtt_ms = (self.rtt as f64 / 100.0).round() / 10.0;
    }
}

// ---------------------------------------------------------------------------------------------
// The sink

#[derive(Debug, Clone, PartialEq)]
enum Conn {
    /// A listener with no receiver.
    Waiting,
    Up,
    Failed(String),
    Done,
}

/// What the writing side and the sending thread share.
struct Shared {
    /// Bytes written that do not make a payload yet, and when the first of them was.
    pending: Vec<u8>,
    pending_at: Instant,
    /// Payloads for the sending thread, with the time their first byte was written.
    ready: VecDeque<(Instant, Vec<u8>)>,
    conn: Conn,
    finishing: bool,
    stats: SinkStats,
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// A connection to a receiver, written to as a sink.
pub(crate) struct SrtSink {
    info: SinkInfo,
    shared: Arc<Mutex<Shared>>,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl SrtSink {
    fn gone(&self, why: &str) -> Error {
        Error::new(ErrorCode::Io, format!("{}: {why}", self.info.url)).hint("the receiver is gone; open the output again to go on sending")
    }

    fn join(&mut self) {
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

impl Drop for SrtSink {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        self.join();
    }
}

impl Sink for SrtSink {
    fn write(&mut self, bytes: &[u8]) -> Result<()> {
        let mut sh = lock(&self.shared);
        match &sh.conn {
            Conn::Failed(why) => return Err(self.gone(why)),
            Conn::Done => return Err(Error::new(ErrorCode::Io, format!("{}: the output is finished", self.info.url))),
            // nobody to send to: live media is not kept for a receiver that may come
            Conn::Waiting => {
                sh.stats.discarded_bytes += bytes.len() as u64;
                return Ok(());
            }
            Conn::Up => {}
        }
        if sh.finishing {
            return Err(Error::new(ErrorCode::Io, format!("{}: the output is finishing", self.info.url)));
        }
        let now = Instant::now();
        if sh.pending.is_empty() {
            sh.pending_at = now;
        }
        sh.pending.extend_from_slice(bytes);
        let mut at = sh.pending_at;
        let mut off = 0;
        while sh.pending.len() - off >= PAYLOAD {
            let chunk = sh.pending[off..off + PAYLOAD].to_vec();
            off += PAYLOAD;
            if sh.ready.len() >= SEND_QUEUE {
                sh.stats.discarded_bytes += PAYLOAD as u64;
            } else {
                sh.ready.push_back((at, chunk));
            }
            at = now;
        }
        sh.pending.drain(..off);
        if off > 0 && !sh.pending.is_empty() {
            sh.pending_at = now;
        }
        Ok(())
    }

    fn finish(&mut self) -> Result<()> {
        {
            let mut sh = lock(&self.shared);
            if sh.conn == Conn::Up {
                sh.finishing = true;
            } else {
                self.stop.store(true, Ordering::Relaxed);
            }
        }
        self.join();
        let mut sh = lock(&self.shared);
        let r = match &sh.conn {
            Conn::Failed(why) => Err(self.gone(why)),
            _ => Ok(()),
        };
        if !matches!(sh.conn, Conn::Failed(_)) {
            sh.conn = Conn::Done;
        }
        r
    }

    fn info(&self) -> &SinkInfo {
        &self.info
    }

    fn stats(&self) -> SinkStats {
        let sh = lock(&self.shared);
        SinkStats { connected: sh.conn == Conn::Up, ..sh.stats.clone() }
    }
}

enum Ended {
    Stopped,
    Finished,
    Gone(String),
}

/// Runs a connection until it ends: payloads from the sink in, protocol, datagrams to the peer.
fn serve(sock: &UdpSocket, peer: SocketAddr, tx: &mut Sender, shared: &Mutex<Shared>, stop: &AtomicBool, carry: &SendStats) -> Ended {
    let mut buf = vec![0u8; MAX_DATAGRAM];
    let mut finishing: Option<Instant> = None;
    let flush = |tx: &mut Sender| {
        for p in tx.take_output() {
            let _ = sock.send_to(&p, peer);
        }
    };
    loop {
        let (items, fin) = {
            let mut sh = lock(shared);
            // a partial payload that has waited is sent as it is
            if !sh.pending.is_empty() && (sh.finishing || sh.pending_at.elapsed() >= HOLD) {
                let (at, p) = (sh.pending_at, std::mem::take(&mut sh.pending));
                sh.ready.push_back((at, p));
            }
            (std::mem::take(&mut sh.ready), sh.finishing)
        };
        let now = Instant::now();
        for (at, p) in items {
            tx.push(p, at, now);
        }
        flush(tx);
        if stop.load(Ordering::Relaxed) {
            let _ = sock.send_to(&tx.shutdown_packet(Instant::now()), peer);
            return Ended::Stopped;
        }
        if fin {
            let since = *finishing.get_or_insert(now);
            let patience = Duration::from_micros((tx.latency_us * 2 + tx.rtt * 4).max(0) as u64 + 200_000).min(DRAIN_MAX);
            if tx.drained(now) || now.duration_since(since) >= patience {
                let bye = tx.shutdown_packet(Instant::now());
                // twice, in case one is lost
                for _ in 0..2 {
                    let _ = sock.send_to(&bye, peer);
                }
                return Ended::Finished;
            }
        }
        match crate::net::recv_from_within(sock, &mut buf, SEND_POLL) {
            Ok((n, from)) if from == peer => tx.on_datagram(&buf[..n], Instant::now()),
            Ok(_) => {}
            // Windows reports an ICMP "port unreachable" from an earlier send as a reset
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted | ErrorKind::ConnectionReset) => {}
            Err(e) => return Ended::Gone(e.to_string()),
        }
        tx.tick(Instant::now());
        flush(tx);
        {
            let mut sh = lock(shared);
            let mut st = std::mem::take(&mut sh.stats);
            tx.report(&mut st, carry);
            sh.stats = st;
        }
        match &tx.state {
            State::Open => {}
            State::Shutdown => return Ended::Gone("the receiver closed the connection".into()),
            State::Broken(why) => return Ended::Gone(why.clone()),
        }
    }
}

/// Checks the options of an SRT output URL, without connecting.
pub(crate) fn check_sender(u: &SourceUrl) -> Result<()> {
    options_of(u).map(|_| ())
}

fn options_of(u: &SourceUrl) -> Result<Options> {
    const KNOWN: [&str; 6] = ["mode", "latency", "streamid", "passphrase", "pbkeylen", "reconnect"];
    if let Some(k) = u.options.keys().find(|k| !KNOWN.contains(&k.as_str())) {
        return Err(Error::invalid_arg(format!("'{k}' is not an option of an srt output")).alternatives(KNOWN));
    }
    options(u)
}

/// Opens an SRT output: a caller makes the connection (through the address allow-list) before
/// returning; a listener is bound and waits for a receiver.
pub(crate) fn open_sender(u: &SourceUrl, shown: String) -> Result<Box<dyn Sink>> {
    let o = options_of(u)?;
    let unreachable = |e: std::io::Error, what: &str| Error::new(ErrorCode::Io, format!("{what} {shown}: {e}")).hint("check the address, and that the receiver is running");
    let info = SinkInfo { url: shown.clone(), kind: "srt", live: true };
    let (sock, first) = if o.listener {
        let host = if o.host.is_empty() { "127.0.0.1" } else { &o.host };
        (UdpSocket::bind((host, o.port)).map_err(|e| unreachable(e, "could not listen on"))?, None)
    } else {
        let peer = net::resolve_udp(&o.host, o.port, &Allow::from_env()?, &shown)?;
        let sock = UdpSocket::bind(if peer.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }).map_err(|e| unreachable(e, "could not open a socket for"))?;
        let c = handshake_caller(&sock, peer, &o, &shown, "receiver")?;
        let latency = o.latency.max(c.hs.peer_delay().unwrap_or(0)).max(c.hs.peer_rcv_delay().unwrap_or(0));
        let tx = Sender::new(TxConfig { my_id: c.my_id, peer_id: c.hs.sock, isn: c.isn, latency_ms: latency, crypto: c.crypto, hs_reply: None }, Instant::now());
        (sock, Some((tx, peer)))
    };
    let shared = Arc::new(Mutex::new(Shared {
        pending: Vec::new(),
        pending_at: Instant::now(),
        ready: VecDeque::new(),
        conn: if first.is_some() { Conn::Up } else { Conn::Waiting },
        finishing: false,
        stats: SinkStats::default(),
    }));
    let stop = Arc::new(AtomicBool::new(false));
    let (state, flag) = (shared.clone(), stop.clone());
    let thread = std::thread::Builder::new()
        .name("ironav-srt-send".into())
        .spawn(move || {
            let mut carry = SendStats::default();
            let mut conn = first;
            loop {
                let (mut tx, peer) = match conn.take() {
                    Some(x) => x,
                    None => {
                        if lock(&state).finishing || flag.load(Ordering::Relaxed) {
                            lock(&state).conn = Conn::Done;
                            return;
                        }
                        match accept(&sock, &o, &flag) {
                            Ok(Some(a)) => {
                                let latency = o.latency.max(a.hs.peer_delay().unwrap_or(0)).max(a.hs.peer_rcv_delay().unwrap_or(0));
                                let tx = Sender::new(TxConfig { my_id: a.my_id, peer_id: a.hs.sock, isn: a.hs.isn, latency_ms: latency, crypto: a.crypto, hs_reply: Some(a.reply) }, Instant::now());
                                let mut sh = lock(&state);
                                sh.conn = Conn::Up;
                                sh.pending.clear();
                                sh.ready.clear();
                                drop(sh);
                                (tx, a.from)
                            }
                            Ok(None) => {
                                lock(&state).conn = Conn::Done;
                                return;
                            }
                            Err(e) => {
                                lock(&state).conn = Conn::Failed(e.to_string());
                                return;
                            }
                        }
                    }
                };
                let ended = serve(&sock, peer, &mut tx, &state, &flag, &carry);
                carry = SendStats {
                    packets: carry.packets + tx.stats.packets,
                    bytes: carry.bytes + tx.stats.bytes,
                    retransmitted: carry.retransmitted + tx.stats.retransmitted,
                    dropped: carry.dropped + tx.stats.dropped,
                };
                let mut sh = lock(&state);
                match ended {
                    Ended::Stopped | Ended::Finished => {
                        sh.conn = Conn::Done;
                        return;
                    }
                    // a listener waits for the next receiver; a caller has nobody else to call
                    Ended::Gone(_) if o.listener && !sh.finishing => sh.conn = Conn::Waiting,
                    Ended::Gone(_) if o.listener => {
                        sh.conn = Conn::Done;
                        return;
                    }
                    Ended::Gone(why) => {
                        sh.conn = Conn::Failed(why);
                        return;
                    }
                }
            }
        })
        .map_err(|e| Error::new(ErrorCode::Internal, format!("could not start the sender's thread: {e}")))?;
    Ok(Box::new(SrtSink { info, shared, stop, thread: Some(thread) }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sink::open_sink;
    use crate::source::open_source;
    use std::net::Ipv4Addr;

    fn rand_bytes(n: usize, seed: u64) -> Vec<u8> {
        let mut x = seed | 1;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    fn free_port() -> u16 {
        crate::net::free_udp_port_for_tests()
    }

    // ---- the protocol state, with no socket and a clock the test moves

    const ISN: u32 = 0x7fff_fff0;

    fn sender(latency: u16, crypto: Option<Decryptor>) -> (Sender, Instant) {
        let t0 = Instant::now();
        (Sender::new(TxConfig { my_id: 0x11, peer_id: 0x22, isn: ISN, latency_ms: latency, crypto, hs_reply: None }, t0), t0)
    }

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    /// Pushes `n` payloads, `gap` milliseconds apart from `t0`.
    fn push_n(s: &mut Sender, t0: Instant, n: usize, gap: u64) {
        for i in 0..n {
            let t = at(t0, i as u64 * gap);
            s.push(vec![i as u8; 100], t, t);
        }
    }

    /// The data packets in `out` as (sequence number, retransmission flag).
    fn data_of(out: &[Vec<u8>]) -> Vec<(u32, bool)> {
        out.iter()
            .filter_map(|d| match parse(d) {
                Some(Packet::Data { seq, .. }) => Some((seq, d[4] & 0x04 != 0)),
                _ => None,
            })
            .collect()
    }

    /// The control packets in `out` as (type, info, body).
    fn controls_of(out: &[Vec<u8>]) -> Vec<(u16, u32, Vec<u8>)> {
        out.iter()
            .filter_map(|d| match parse(d) {
                Some(Packet::Control { kind, info, cif, .. }) => Some((kind, info, cif.to_vec())),
                _ => None,
            })
            .collect()
    }

    fn ack(point: u32, no: u32, rtt: u32) -> Vec<u8> {
        let mut cif = Vec::new();
        for w in [point, rtt, rtt / 2, 8000, 100, 100, 100_000] {
            cif.extend_from_slice(&w.to_be_bytes());
        }
        control(C_ACK, 0, no, 0, 0x11, &cif)
    }

    fn nak(ranges: &[(u32, u32)]) -> Vec<u8> {
        let mut cif = Vec::new();
        for &(a, b) in ranges {
            if a == b {
                cif.extend_from_slice(&a.to_be_bytes());
            } else {
                cif.extend_from_slice(&(a | 0x8000_0000).to_be_bytes());
                cif.extend_from_slice(&b.to_be_bytes());
            }
        }
        control(C_NAK, 0, 0, 0, 0x11, &cif)
    }

    fn keep_alive() -> Vec<u8> {
        control(C_KEEPALIVE, 0, 0, 0, 0x11, &[])
    }

    #[test]
    fn payloads_go_out_numbered_stamped_and_in_order_across_the_sequence_wrap() {
        let (mut s, t0) = sender(120, None);
        push_n(&mut s, t0, 20, 5);
        let out = s.take_output();
        assert_eq!(out.len(), 20);
        for (i, d) in out.iter().enumerate() {
            let Some(Packet::Data { seq, kk, ts, dst, payload }) = parse(d) else { panic!("not data") };
            assert_eq!(seq, seq_add(ISN, i as u32));
            assert_eq!((kk, ts, dst), (0, i as u32 * 5000, 0x22));
            assert_eq!(payload, &[i as u8; 100][..]);
            // a solo message numbered from 1, no retransmission flag
            assert_eq!(word(d, 4), 0xc000_0000 | (i as u32 + 1));
        }
        assert_eq!(seq_add(ISN, 19), 3, "the run crosses the 31-bit wrap");
        assert_eq!((s.stats.packets, s.stats.bytes), (20, 2000));
    }

    #[test]
    fn a_loss_report_is_answered_with_the_packets_marked_as_sent_again() {
        let (mut s, t0) = sender(1000, None);
        push_n(&mut s, t0, 10, 1);
        s.take_output();
        let (a, b) = (seq_add(ISN, 3), seq_add(ISN, 5));
        s.on_datagram(&nak(&[(a, b), (seq_add(ISN, 8), seq_add(ISN, 8))]), at(t0, 20));
        let out = s.take_output();
        assert_eq!(data_of(&out), [(a, true), (seq_add(ISN, 4), true), (b, true), (seq_add(ISN, 8), true)]);
        // the same bytes and stamp as the first time
        let Some(Packet::Data { ts, payload, .. }) = parse(&out[0]) else { panic!() };
        assert_eq!((ts, payload), (3000, &[3u8; 100][..]));
        assert_eq!(s.stats.retransmitted, 4);
        // what the receiver has acknowledged is not sent again, nor is what was never sent
        s.on_datagram(&ack(seq_add(ISN, 6), 1, 2000), at(t0, 25));
        s.take_output();
        s.on_datagram(&nak(&[(a, b), (seq_add(ISN, 50), seq_add(ISN, 60))]), at(t0, 30));
        assert!(data_of(&s.take_output()).is_empty());
    }

    #[test]
    fn an_ack_frees_what_it_covers_is_answered_with_an_ackack_and_measures_the_round_trip() {
        let (mut s, t0) = sender(120, None);
        push_n(&mut s, t0, 10, 1);
        s.take_output();
        s.on_datagram(&ack(seq_add(ISN, 4), 7, 2000), at(t0, 12));
        assert_eq!(s.buf.len(), 6);
        assert_eq!(s.buf[0].seq, seq_add(ISN, 4));
        let c = controls_of(&s.take_output());
        assert_eq!(c.len(), 1);
        assert_eq!((c[0].0, c[0].1), (C_ACKACK, 7));
        assert!(s.rtt < 100_000 && s.rtt > 2000, "{}", s.rtt);
        // an ACK for packets never sent is not believed, and an old one frees nothing
        s.on_datagram(&ack(seq_add(ISN, 99), 8, 2000), at(t0, 13));
        s.on_datagram(&ack(seq_add(ISN, 2), 9, 2000), at(t0, 14));
        assert_eq!(s.buf.len(), 6);
        s.on_datagram(&ack(seq_add(ISN, 10), 10, 2000), at(t0, 15));
        assert!(s.buf.is_empty());
        // a light ACK (the sequence number alone) frees and gets no ACKACK
        push_n(&mut s, at(t0, 20), 2, 1);
        s.take_output();
        s.on_datagram(&control(C_ACK, 0, 0, 0, 0x11, &seq_add(ISN, 11).to_be_bytes()), at(t0, 30));
        assert_eq!(s.buf.len(), 1);
        assert!(controls_of(&s.take_output()).is_empty());
    }

    #[test]
    fn packets_older_than_the_latency_are_given_up_and_the_receiver_is_told() {
        let (mut s, t0) = sender(100, None);
        push_n(&mut s, t0, 3, 1);
        s.take_output();
        // the latency and a quarter more, and 20 ms: nothing is given up before that
        s.on_datagram(&keep_alive(), at(t0, 100));
        s.tick(at(t0, 140));
        assert_eq!(s.buf.len(), 3);
        s.on_datagram(&keep_alive(), at(t0, 150));
        s.tick(at(t0, 180));
        assert!(s.buf.is_empty());
        assert_eq!(s.stats.dropped, 3);
        let c = controls_of(&s.take_output());
        assert_eq!(c.len(), 1);
        assert_eq!(c[0].0, C_DROPREQ);
        assert_eq!((word(&c[0].2, 0), word(&c[0].2, 4)), (ISN, seq_add(ISN, 2)));
        // asked for again, they are answered with the same drop request and no data
        s.on_datagram(&nak(&[(ISN, seq_add(ISN, 2))]), at(t0, 190));
        let out = s.take_output();
        assert!(data_of(&out).is_empty());
        let c = controls_of(&out);
        assert_eq!((c.len(), c[0].0, word(&c[0].2, 0), word(&c[0].2, 4)), (1, C_DROPREQ, ISN, seq_add(ISN, 2)));
    }

    #[test]
    fn the_send_buffer_is_bounded_whatever_the_receiver_does() {
        let (mut s, t0) = sender(60_000, None);
        for _ in 0..(SEND_WINDOW + 5000) {
            s.push(vec![1; 1316], t0, t0);
        }
        assert_eq!(s.buf.len(), SEND_WINDOW);
        assert_eq!(s.stats.dropped, 5000);
    }

    #[test]
    fn unacknowledged_packets_are_sent_again_when_no_ack_moves() {
        let (mut s, t0) = sender(1000, None);
        s.on_datagram(&keep_alive(), t0);
        push_n(&mut s, t0, 3, 1);
        s.take_output();
        s.tick(at(t0, 100));
        assert!(data_of(&s.take_output()).is_empty());
        // the retransmission timeout is a round trip, four deviations and 20 ms (320 ms before any ACK)
        s.on_datagram(&keep_alive(), at(t0, 300));
        s.tick(at(t0, 400));
        assert_eq!(data_of(&s.take_output()), [(ISN, true), (seq_add(ISN, 1), true), (seq_add(ISN, 2), true)]);
        s.tick(at(t0, 450));
        assert!(data_of(&s.take_output()).is_empty());
    }

    #[test]
    fn keep_alives_shutdown_and_silence() {
        let (mut s, t0) = sender(120, None);
        s.on_datagram(&keep_alive(), at(t0, 10));
        s.tick(at(t0, 500));
        assert!(controls_of(&s.take_output()).is_empty());
        s.tick(at(t0, 1100));
        let c = controls_of(&s.take_output());
        assert_eq!(c.iter().map(|x| x.0).collect::<Vec<_>>(), [C_KEEPALIVE]);
        assert_eq!(s.state, State::Open);
        s.on_datagram(&control(C_SHUTDOWN, 0, 0, 0, 0x11, &[]), at(t0, 1200));
        assert_eq!(s.state, State::Shutdown);
        let (mut quiet, t0) = sender(120, None);
        quiet.tick(at(t0, 4900));
        assert_eq!(quiet.state, State::Open);
        quiet.tick(at(t0, 5100));
        assert!(matches!(quiet.state, State::Broken(_)));
        // a datagram for another socket says nothing
        let (mut other, t0) = sender(120, None);
        other.on_datagram(&control(C_KEEPALIVE, 0, 0, 0, 0x99, &[]), at(t0, 4000));
        other.tick(at(t0, 5100));
        assert!(matches!(other.state, State::Broken(_)));
    }

    #[test]
    fn encrypted_payloads_name_their_key_read_back_and_are_sent_the_same_again() {
        for klen in [16usize, 24, 32] {
            let (keys, km) = new_keys("0123456789ab", klen).unwrap();
            assert_eq!(parse_km(&km).unwrap().klen, klen);
            let (mut s, t0) = sender(120, Some(keys));
            let plain = rand_bytes(1316, 3);
            s.push(plain.clone(), t0, t0);
            let first = s.take_output().remove(0);
            let Some(Packet::Data { seq, kk, payload, .. }) = parse(&first) else { panic!() };
            assert_eq!(kk, 1);
            assert_ne!(payload, &plain[..]);
            // the receiver's keys come from the key material alone
            let mut theirs = Decryptor::new("0123456789ab");
            assert!(theirs.install(&parse_km(&km).unwrap()));
            let mut back = payload.to_vec();
            assert!(theirs.apply(kk, seq, &mut back));
            assert_eq!(back, plain);
            // a retransmission is the same bytes
            s.on_datagram(&nak(&[(seq, seq)]), at(t0, 10));
            let again = s.take_output().remove(0);
            assert_eq!(again[16..], first[16..]);
            assert_eq!(again[4] & 0x04, 0x04);
            // another passphrase unwraps nothing
            assert!(!Decryptor::new("another-phrase").install(&parse_km(&km).unwrap()));
        }
    }

    #[test]
    fn hostile_datagrams_are_survived_with_bounded_buffers() {
        let (mut s, t0) = sender(120, None);
        push_n(&mut s, t0, 100, 1);
        let junk = rand_bytes(400_000, 11);
        let mut at_ms = 0;
        for chunk in junk.chunks(97) {
            at_ms += 1;
            // random bytes, and random bytes dressed as the packets this end reads
            let mut d = chunk.to_vec();
            s.on_datagram(&d, at(t0, at_ms));
            if d.len() > 16 {
                d[0] |= 0x80;
                d[1] = [0x03, 0x02, 0x05, 0x07][d[2] as usize % 4];
                d[12..16].copy_from_slice(&0x11u32.to_be_bytes());
                s.on_datagram(&d, at(t0, at_ms));
            }
            s.take_output();
        }
        assert!(s.buf.len() <= SEND_WINDOW);
        // a loss report for the whole sequence space does not make it send millions of packets
        let mut cif = (0x8000_0000u32).to_be_bytes().to_vec();
        cif.extend_from_slice(&SEQ_MASK.to_be_bytes());
        s.on_datagram(&control(C_NAK, 0, 0, 0, 0x11, &cif), at(t0, 5000));
        assert!(s.take_output().len() <= 2 * WINDOW + 8);
    }

    // ---- a path that loses packets

    /// A relay between a client and a server on loopback that loses the original data packets
    /// whose index modulo `m` is `k`, on the way the data goes.
    struct Relay {
        port: u16,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<u64>>,
    }

    impl Relay {
        fn new(server: SocketAddr, lose: Option<(usize, usize)>, data_from_client: bool) -> Relay {
            let front = UdpSocket::bind("127.0.0.1:0").unwrap();
            let back = UdpSocket::bind("127.0.0.1:0").unwrap();
            crate::webrtc::grow_receive_buffer(&front);
            crate::webrtc::grow_receive_buffer(&back);
            front.set_nonblocking(true).unwrap();
            back.set_nonblocking(true).unwrap();
            let port = front.local_addr().unwrap().port();
            let stop = Arc::new(AtomicBool::new(false));
            let flag = stop.clone();
            let thread = std::thread::spawn(move || {
                let (mut client, mut seen, mut lost) = (None::<SocketAddr>, 0usize, 0u64);
                let mut buf = vec![0u8; 65536];
                let mut drop_it = |d: &[u8]| {
                    // original data packets only: retransmissions and control get through
                    let data = d.len() > HEADER && d[0] & 0x80 == 0 && d[4] & 0x04 == 0;
                    let Some((m, k)) = lose.filter(|_| data) else { return false };
                    seen += 1;
                    let gone = (seen - 1) % m == k;
                    lost += gone as u64;
                    gone
                };
                while !flag.load(Ordering::Relaxed) {
                    let mut idle = true;
                    if let Ok((n, from)) = front.recv_from(&mut buf) {
                        idle = false;
                        client = Some(from);
                        if !(data_from_client && drop_it(&buf[..n])) {
                            let _ = back.send_to(&buf[..n], server);
                        }
                    }
                    if let Ok((n, _)) = back.recv_from(&mut buf) {
                        idle = false;
                        if let Some(c) = client {
                            if data_from_client || !drop_it(&buf[..n]) {
                                let _ = front.send_to(&buf[..n], c);
                            }
                        }
                    }
                    if idle {
                        std::thread::sleep(Duration::from_micros(200));
                    }
                }
                lost
            });
            Relay { port, stop, thread: Some(thread) }
        }

        /// Stops the relay; how many packets it lost.
        fn finish(mut self) -> u64 {
            self.stop.store(true, Ordering::Relaxed);
            self.thread.take().unwrap().join().unwrap()
        }
    }

    impl Drop for Relay {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
        }
    }

    fn drain(s: &mut dyn Source, limit: Duration) -> Vec<u8> {
        let (mut out, mut buf, start) = (Vec::new(), vec![0u8; 64 << 10], Instant::now());
        while start.elapsed() < limit {
            match s.read(&mut buf, Duration::from_millis(20)).unwrap() {
                Chunk::Data(n) => out.extend_from_slice(&buf[..n]),
                Chunk::Idle | Chunk::Restart => {}
                Chunk::End => break,
            }
        }
        out
    }

    /// Writes `data` the way a muxer would, in pieces of uneven size, one every few milliseconds.
    fn write_paced(sink: &mut dyn Sink, data: &[u8]) {
        let mut off = 0;
        let sizes = [5264, 188, 1316, 7520, 376];
        let mut i = 0;
        while off < data.len() {
            let n = sizes[i % sizes.len()].min(data.len() - off);
            sink.write(&data[off..off + n]).unwrap();
            off += n;
            i += 1;
            std::thread::sleep(Duration::from_millis(3));
        }
    }

    fn wait_connected(sink: &dyn Sink) {
        let t = Instant::now();
        while !sink.stats().connected {
            assert!(t.elapsed() < Duration::from_secs(5), "no receiver connected");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Our sender to our receiver, through a path that loses packets: the bytes that come out are
    /// the bytes that went in. `sender_calls` says which end calls; `key` is the key length of an
    /// encrypted stream. The latency is long so that a busy machine does not turn this into a test
    /// of the too-late drop: a stalled thread is not what is being tested.
    fn roundtrip(sender_calls: bool, key: Option<usize>) {
        crate::net::allow_loopback_for_tests();
        // 364 whole payloads and a partial one, which is the 365th and is lost on its way (so the
        // end of the stream is a loss no later packet shows)
        let data = rand_bytes((9 * 40 + 4) * 1316 + 500, 5);
        let pass = key.map(|k| format!("&passphrase=0123456789abcdef&pbkeylen={k}")).unwrap_or_default();
        let (mut src, mut sink, relay) = if sender_calls {
            let rport = free_port();
            let src = open_source(&format!("srt://127.0.0.1:{rport}?mode=listener&reconnect=0{pass}")).unwrap();
            let relay = Relay::new(SocketAddr::from((Ipv4Addr::LOCALHOST, rport)), Some((9, 4)), true);
            let sink = open_sink(&format!("srt://127.0.0.1:{}?latency=1000{pass}", relay.port)).unwrap();
            (src, sink, relay)
        } else {
            let sport = free_port();
            let sink = open_sink(&format!("srt://127.0.0.1:{sport}?mode=listener&latency=1000{pass}")).unwrap();
            let relay = Relay::new(SocketAddr::from((Ipv4Addr::LOCALHOST, sport)), Some((9, 4)), false);
            let src = open_source(&format!("srt://127.0.0.1:{}?reconnect=0{pass}", relay.port)).unwrap();
            (src, sink, relay)
        };
        wait_connected(sink.as_ref());
        let reader = std::thread::spawn(move || drain(src.as_mut(), Duration::from_secs(30)));
        write_paced(sink.as_mut(), &data);
        sink.finish().unwrap();
        let got = reader.join().unwrap();
        let stats = sink.stats();
        let lost = relay.finish();
        // about 40 when every original reaches the relay; a busy machine overflows its socket first
        assert!(lost >= 20, "the path lost {lost}");
        assert!(stats.retransmitted >= lost, "{stats:?}, lost {lost}");
        assert_eq!(got.len(), data.len(), "{stats:?}");
        assert!(got == data, "the bytes differ");
        assert_eq!(stats.bytes as usize, data.len());
    }

    #[cfg_attr(feature = "fips", ignore = "FIPS build: an unencrypted SRT output is refused (tests/fips.rs covers the refusal)")]
    #[test]
    fn a_calling_sender_reaches_a_listening_receiver_with_every_byte_despite_loss() {
        roundtrip(true, None);
    }

    #[cfg_attr(feature = "fips", ignore = "FIPS build: an unencrypted SRT output is refused (tests/fips.rs covers the refusal)")]
    #[test]
    fn a_listening_sender_reaches_a_calling_receiver_with_every_byte_despite_loss() {
        roundtrip(false, None);
    }

    #[test]
    fn an_encrypted_stream_from_a_calling_sender_arrives_whole_at_each_key_length() {
        for k in [16, 24, 32] {
            roundtrip(true, Some(k));
        }
    }

    #[test]
    fn an_encrypted_stream_to_a_calling_receiver_arrives_whole_at_each_key_length() {
        for k in [16, 24, 32] {
            roundtrip(false, Some(k));
        }
    }

    // ---- the handshake, the policy, and a receiver that goes away

    #[test]
    fn the_handshake_carries_what_the_url_asked_for() {
        crate::net::allow_loopback_for_tests();
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = sock.local_addr().unwrap().port();
        let o = options(&SourceUrl::parse(&format!("srt://:{port}?mode=listener&passphrase=0123456789&latency=40")).unwrap()).unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let flag = stop.clone();
        let listener = std::thread::spawn(move || accept(&sock, &o, &flag).unwrap().map(|a| (a.hs, a.crypto.is_some())));
        let mut sink = open_sink(&format!("srt://127.0.0.1:{port}?streamid=%23!%3A%3Ar%3Dlive%2Fcam&passphrase=0123456789&pbkeylen=24&latency=80")).unwrap();
        let (h, keyed) = listener.join().unwrap().expect("a caller");
        assert!(keyed);
        assert_eq!((h.version, h.kind, h.enc), (5, HS_CONCLUSION, 3));
        assert_ne!(h.sock, 0);
        let sid = h.ext(E_SID).expect("a stream ID");
        let mut v = sid.to_vec();
        for c in v.chunks_mut(4) {
            c.reverse();
        }
        assert_eq!(String::from_utf8(v).unwrap().trim_end_matches('\0'), "#!::r=live/cam");
        let req = h.ext(E_HSREQ).expect("HSREQ");
        let flags = word(req, 4);
        assert_eq!(flags & (F_TSBPDSND | F_TSBPDRCV | F_TLPKTDROP | F_CRYPT | F_REXMIT), F_TSBPDSND | F_TSBPDRCV | F_TLPKTDROP | F_CRYPT | F_REXMIT);
        assert_eq!((half(req, 8), half(req, 10)), (80, 80));
        let km = parse_km(h.ext(E_KMREQ).expect("KMREQ")).unwrap();
        assert_eq!((km.klen, km.kk), (24, 1));
        sink.finish().unwrap();
        assert!(!sink.info().url.contains("0123456789") && sink.info().url.contains("passphrase=***"), "{}", sink.info().url);
    }

    #[test]
    fn a_receiver_that_wants_another_passphrase_or_none_refuses_and_the_error_names_neither() {
        crate::net::allow_loopback_for_tests();
        for listener_pass in ["", "&passphrase=aaaaaaaaaaaa"] {
            // FIPS mode does not open a plaintext receiver
            if cfg!(feature = "fips") && listener_pass.is_empty() {
                continue;
            }
            let port = free_port();
            let _rx = open_source(&format!("srt://127.0.0.1:{port}?mode=listener{listener_pass}")).unwrap();
            let e = open_sink(&format!("srt://127.0.0.1:{port}?passphrase=bbbbbbbbbbbb")).err().expect("refused");
            assert_eq!(e.code, ErrorCode::Io);
            assert!(e.message.contains("receiver refused") && !e.message.contains("bbbbbbbbbbbb"), "{}", e.message);
        }
    }

    #[test]
    fn urls_are_validated_and_the_allow_list_applies_to_a_caller() {
        crate::net::allow_loopback_for_tests();
        for bad in [
            "srt://127.0.0.1:9?mode=rendezvous",
            "srt://:9?mode=caller",
            "srt://127.0.0.1:9?latency=70000",
            "srt://127.0.0.1:9?passphrase=short",
            "srt://127.0.0.1:9?pbkeylen=20&passphrase=0123456789",
            "srt://127.0.0.1:9?format=h264&passphrase=0123456789",
            "srt://127.0.0.1",
            "udp://127.0.0.1:9",
            "rtsp://127.0.0.1:9/x",
            "out.ts",
        ] {
            let e = open_sink(bad).err().unwrap_or_else(|| panic!("{bad} opened"));
            assert_eq!(e.code, ErrorCode::InvalidArgument, "{bad}: {e:?}");
        }
        // 10.255.255.1 is not loopback: refused before anything is sent
        let e = open_sink("srt://10.255.255.1:9?passphrase=0123456789").err().expect("not allowed");
        assert_eq!(e.code, ErrorCode::InvalidArgument);
        assert!(e.hint.as_deref().is_some_and(|h| h.contains("IRONAV_NET_ALLOW")), "{e:?}");
    }

    #[cfg_attr(feature = "fips", ignore = "FIPS build: an unencrypted SRT output is refused (tests/fips.rs covers the refusal)")]
    #[test]
    fn a_caller_with_nobody_listening_fails_rather_than_waits() {
        crate::net::allow_loopback_for_tests();
        let t = Instant::now();
        let e = open_sink(&format!("srt://127.0.0.1:{}", free_port())).err().expect("no receiver");
        assert_eq!(e.code, ErrorCode::Io);
        assert!(t.elapsed() < Duration::from_secs(6));
    }

    #[cfg_attr(feature = "fips", ignore = "FIPS build: an unencrypted SRT output is refused (tests/fips.rs covers the refusal)")]
    #[test]
    fn a_caller_is_told_when_the_receiver_is_gone_and_a_listener_waits_for_the_next() {
        crate::net::allow_loopback_for_tests();
        // a calling sender: the receiver leaves, and writes fail with a message that says why
        let port = free_port();
        let rx = open_source(&format!("srt://127.0.0.1:{port}?mode=listener&reconnect=0")).unwrap();
        let mut sink = open_sink(&format!("srt://127.0.0.1:{port}")).unwrap();
        wait_connected(sink.as_ref());
        sink.write(&[0x47; 1316]).unwrap();
        drop(rx);
        let t = Instant::now();
        let e = loop {
            match sink.write(&[0x47; 1316]) {
                Ok(()) => {
                    assert!(t.elapsed() < Duration::from_secs(3), "writes still succeed");
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(e) => break e,
            }
        };
        assert_eq!(e.code, ErrorCode::Io);
        assert!(e.message.contains("receiver") && e.hint.is_some(), "{e:?}");
        assert!(sink.finish().is_err());
        // a listening sender: bytes written with nobody there are discarded and counted, the
        // first receiver leaves, and a second one gets the stream from there on
        let port = free_port();
        let mut sink = open_sink(&format!("srt://127.0.0.1:{port}?mode=listener")).unwrap();
        sink.write(&[1; 3000]).unwrap();
        assert_eq!((sink.stats().connected, sink.stats().discarded_bytes), (false, 3000));
        for round in 0..2u8 {
            let mut rx = open_source(&format!("srt://127.0.0.1:{port}?reconnect=0")).unwrap();
            wait_connected(sink.as_ref());
            let payload = vec![0x10 + round; 2632];
            sink.write(&payload).unwrap();
            let mut got = Vec::new();
            let mut buf = vec![0u8; 65536];
            let t = Instant::now();
            while got.len() < payload.len() && t.elapsed() < Duration::from_secs(5) {
                if let Chunk::Data(n) = rx.read(&mut buf, Duration::from_millis(20)).unwrap() {
                    got.extend_from_slice(&buf[..n]);
                }
            }
            assert_eq!(got, payload);
            drop(rx);
            let t = Instant::now();
            while sink.stats().connected {
                assert!(t.elapsed() < Duration::from_secs(7), "still connected to a receiver that left");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        sink.finish().unwrap();
        assert!(sink.write(&[0]).is_err());
    }

    #[cfg_attr(feature = "fips", ignore = "FIPS build: an unencrypted SRT output is refused (tests/fips.rs covers the refusal)")]
    #[test]
    fn writing_never_waits_for_a_receiver_that_has_stopped_answering() {
        crate::net::allow_loopback_for_tests();
        let port = free_port();
        let mut sink = open_sink(&format!("srt://127.0.0.1:{port}?mode=listener&latency=50")).unwrap();
        // a caller that completes the handshake and then says nothing at all
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let o = options(&SourceUrl::parse(&format!("srt://127.0.0.1:{port}")).unwrap()).unwrap();
        let _c = handshake_caller(&sock, SocketAddr::from((Ipv4Addr::LOCALHOST, port)), &o, "test", "sender").unwrap();
        wait_connected(sink.as_ref());
        let t = Instant::now();
        let chunk = vec![0x47u8; 64 << 10];
        // 128 MiB
        for _ in 0..2048 {
            sink.write(&chunk).unwrap();
        }
        assert!(t.elapsed() < Duration::from_secs(4), "{:?}", t.elapsed());
        let st = sink.stats();
        assert!(st.dropped + st.discarded_bytes / 1316 > 0, "{st:?}");
        // and finishing does not wait out the silence
        let t = Instant::now();
        sink.finish().unwrap();
        assert!(t.elapsed() < Duration::from_secs(4), "{:?}", t.elapsed());
    }

    // ---- FFmpeg (libsrt) as the receiver

    /// With `ffmpeg` and `ffprobe` on the PATH (FFmpeg built with libsrt): this sender feeds FFmpeg
    /// the test transport stream at the pace it plays, FFmpeg writes what it receives to a file, and
    /// that file is probed. Both roles, with no passphrase and with each key length. Files go to
    /// `E:/srt-send-interop` (or the directory in `IRONAV_SRT_INTEROP_DIR`).
    #[ignore = "needs ffmpeg and ffprobe with libsrt on the PATH"]
    #[test]
    fn interop_with_ffmpeg_receiving() {
        use std::process::{Command, Stdio};
        crate::net::allow_loopback_for_tests();
        let src = concat!(env!("CARGO_MANIFEST_DIR"), "/../ironav-formats/tests/data/h264_aac.ts");
        let dir = std::path::PathBuf::from(std::env::var("IRONAV_SRT_INTEROP_DIR").unwrap_or_else(|_| "E:/srt-send-interop".into()));
        std::fs::create_dir_all(&dir).unwrap();
        let input = std::fs::read(src).unwrap();
        let probe = |path: &std::path::Path| -> String {
            let o = Command::new("ffprobe").args(["-v", "error", "-count_packets", "-show_entries", "stream=codec_name,nb_read_packets", "-of", "csv=p=0"]).arg(path).output().expect("ffprobe");
            let mut lines: Vec<String> = String::from_utf8_lossy(&o.stdout).lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect();
            lines.sort();
            format!("{} {}", lines.join(" "), String::from_utf8_lossy(&o.stderr).trim())
        };
        let want = probe(std::path::Path::new(src));
        println!("input: {want}");
        let loglevel = std::env::var("IRONAV_SRT_INTEROP_LOG").unwrap_or_else(|_| "info".into());
        let mut failures = Vec::new();
        for calling in [true, false] {
            for key in [None, Some(16usize), Some(24), Some(32)] {
                let name = format!("{}-{}", if calling { "sender-calls" } else { "sender-listens" }, key.map_or("clear".into(), |k| format!("aes{}", k * 8)));
                if std::env::var("IRONAV_SRT_INTEROP_ONLY").is_ok_and(|only| !name.contains(&only)) {
                    continue;
                }
                let out = dir.join(format!("{name}.ts"));
                let _ = std::fs::remove_file(&out);
                let port = free_port();
                let pass = key.map(|k| format!("&passphrase=0123456789abcdef&pbkeylen={k}")).unwrap_or_default();
                let mode = if calling { "listener" } else { "caller" };
                let log = std::fs::File::create(dir.join(format!("{name}.log"))).unwrap();
                let mut ff = Command::new("ffmpeg")
                    .args(["-nostdin", "-hide_banner", "-loglevel", &loglevel, "-y", "-i", &format!("srt://127.0.0.1:{port}?mode={mode}{pass}"), "-c", "copy", "-f", "mpegts"])
                    .arg(&out)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(log)
                    .spawn()
                    .expect("ffmpeg");
                let result = (|| -> std::result::Result<(), String> {
                    let url = if calling { format!("srt://127.0.0.1:{port}?latency=120{pass}") } else { format!("srt://127.0.0.1:{port}?mode=listener&latency=120{pass}") };
                    let mut sink = None;
                    let t = Instant::now();
                    // FFmpeg takes a moment to start listening: a caller tries again until it does
                    while sink.is_none() {
                        match open_sink(&url) {
                            Ok(s) => sink = Some(s),
                            Err(_) if t.elapsed() < Duration::from_secs(15) => std::thread::sleep(Duration::from_millis(200)),
                            Err(e) => return Err(format!("could not open the output: {e:?}")),
                        }
                    }
                    let mut sink = sink.unwrap();
                    while !sink.stats().connected {
                        if t.elapsed() > Duration::from_secs(20) {
                            return Err("FFmpeg never connected".into());
                        }
                        std::thread::sleep(Duration::from_millis(10));
                    }
                    // at the pace the stream plays: 2.03 s in the file
                    let start = Instant::now();
                    let mut off = 0;
                    while off < input.len() {
                        let n = 1316usize.min(input.len() - off);
                        sink.write(&input[off..off + n]).map_err(|e| format!("write failed: {e:?}"))?;
                        off += n;
                        let due = Duration::from_secs_f64(2.027 * off as f64 / input.len() as f64);
                        if let Some(wait) = due.checked_sub(start.elapsed()) {
                            std::thread::sleep(wait);
                        }
                    }
                    sink.finish().map_err(|e| format!("finish failed: {e:?}"))?;
                    println!("{name}: sent {:?}", sink.stats());
                    Ok(())
                })();
                // FFmpeg ends on the goodbye; one that does not is stopped (it is ours)
                let t = Instant::now();
                let status = loop {
                    if let Some(s) = ff.try_wait().unwrap() {
                        break Some(s);
                    }
                    if t.elapsed() > Duration::from_secs(15) {
                        let _ = ff.kill();
                        let _ = ff.wait();
                        break None;
                    }
                    std::thread::sleep(Duration::from_millis(50));
                };
                let got = probe(&out);
                println!("{name}: ffmpeg exit {status:?}; output {} bytes; probe: {got}", std::fs::metadata(&out).map_or(0, |m| m.len()));
                match (result, status) {
                    (Err(e), _) => failures.push(format!("{name}: {e}")),
                    (_, None) => failures.push(format!("{name}: FFmpeg did not end after the goodbye")),
                    (_, Some(s)) if !s.success() => failures.push(format!("{name}: FFmpeg exited with {s}")),
                    _ if got != want => failures.push(format!("{name}: probe says {got:?}, the input is {want:?}")),
                    _ => {}
                }
            }
        }
        assert!(failures.is_empty(), "{failures:#?}");
    }
}
