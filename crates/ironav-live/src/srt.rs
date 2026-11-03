//! An SRT receiver (Secure Reliable Transport, draft-sharabayko-srt), in live mode: it takes the
//! MPEG-TS a sender (an encoder, `srt-live-transmit`, ffmpeg) sends over SRT and hands the bytes
//! on in order, one payload per read, as `udp://` does.
//!
//! `srt://host:port` calls a sender that listens; `srt://:port?mode=listener` (or `?mode=listener`
//! with a host to bind to) waits for one sender to call. Both speak handshake version 5 (an
//! induction, then a conclusion carrying the HSREQ extension with the latency and the flags, the
//! stream ID, and the key material when there is a passphrase). IronAV only receives, so the
//! sender's role is the other side's whatever side calls.
//!
//! What the receiver does once connected:
//! - data packets are put in sequence; a gap is reported at once in a NAK (loss report), and
//!   again every few round trips until the packet comes (a retransmission) or is given up on;
//! - a full ACK goes out every 10 ms (and whenever the position moves), the sender's ACKACK
//!   measures the round trip, which paces the repeated NAKs;
//! - timestamp-based packet delivery: a packet is handed on at its timestamp plus the latency
//!   (`?latency=` milliseconds, 120 by default, raised to the sender's own if that is larger), so
//!   the media comes out at the pace it went in and a retransmission has time to arrive; a packet
//!   still missing when a later one is due is dropped (too-late packet drop);
//! - keep-alives go out when the line is quiet, a SHUTDOWN from the sender ends the source, and
//!   five seconds of silence breaks the connection.
//!
//! With `?passphrase=` the stream is AES-CTR encrypted: the key material (KMREQ) carries the
//! stream encrypting key wrapped (RFC 3394) with a key-encrypting key that PBKDF2-HMAC-SHA1 derives
//! from the passphrase, and the receiver decrypts each packet with the key its KK bits name. A
//! sender that does not encrypt, or whose passphrase differs, is refused. AES and the key wrap
//! come from IronCrypto (`ic-cipher`, which IronSocketLayer uses too); PBKDF2-HMAC-SHA1 is here,
//! because IronCrypto has no SHA-1.
//!
//! Everything from the network is untrusted: every length is checked, the receive window is 8192
//! packets and 32 MiB, the loss report and the number of unread payloads are bounded, and a packet
//! that does not parse is dropped. Not supported: rendezvous mode, the message API and its
//! fragments (a payload is handed on as it arrives), AES-GCM, packet filters (FEC), groups, and
//! handshake version 4. The sender's clock is followed (its drift is tracked), the receiver's is
//! never adjusted.

use crate::auth::random_bytes;
use crate::net::{self, Allow};
use crate::sha1::sha1;
use crate::source::{host_port, Chunk, Source, SourceInfo, ThreadSource};
use crate::url::{percent_decode, SourceUrl};
use ic_cipher::keywrap::{Aes128Kw, Aes192Kw, Aes256Kw};
use ic_cipher::{Aes128, Aes192, Aes256};
use ic_core::traits::BlockCipher;
use ironav_core::{Error, ErrorCode, Result};
use std::collections::VecDeque;
use std::io::ErrorKind;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{sync_channel, SyncSender, TrySendError};
use std::sync::Arc;
use std::time::{Duration, Instant};

const HEADER: usize = 16;
const MAX_DATAGRAM: usize = 65536;
/// The most packets the receive buffer spans (and the flow window the handshake offers).
const WINDOW: usize = 8192;
/// The most payload bytes the receive buffer holds.
const MAX_HELD: usize = 32 << 20;
/// The most payloads waiting to be read, once delivered.
const READY_MAX: usize = 512;
/// The most ranges a periodic loss report names.
const MAX_NAK_RANGES: usize = 256;
const MTU: u32 = 1500;
const SEQ_MASK: u32 = 0x7fff_ffff;
/// A full ACK goes out this often (microseconds).
const ACK_PERIOD: i64 = 10_000;
/// A line that has said nothing for this long gets a keep-alive (microseconds).
const KEEPALIVE: i64 = 1_000_000;
/// A sender silent for this long is gone (microseconds).
const EXPIRE: i64 = 5_000_000;
/// How long the handshake may take.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(3);
/// How often a handshake packet is sent again.
const HANDSHAKE_RESEND: Duration = Duration::from_millis(250);
/// How long a socket read waits before the timers are looked at.
const POLL: Duration = Duration::from_millis(2);
const SRT_VERSION: u32 = 0x0001_0500;
/// The extension field of a version 5 induction response.
const MAGIC: u16 = 0x4a17;
/// The default latency, milliseconds.
const DEFAULT_LATENCY: u16 = 120;
const MAX_LATENCY: u16 = 60_000;

// handshake types
const HS_INDUCTION: u32 = 1;
const HS_CONCLUSION: u32 = 0xffff_ffff;
const REJ_BADSECRET: u32 = 1010;
const REJ_UNSECURE: u32 = 1011;
// control packet types
const C_HANDSHAKE: u16 = 0;
const C_KEEPALIVE: u16 = 1;
const C_ACK: u16 = 2;
const C_NAK: u16 = 3;
const C_SHUTDOWN: u16 = 5;
const C_ACKACK: u16 = 6;
const C_DROPREQ: u16 = 7;
const C_USER: u16 = 0x7fff;
// handshake extension types, and the control subtypes of key material
const E_HSREQ: u16 = 1;
const E_HSRSP: u16 = 2;
const E_KMREQ: u16 = 3;
const E_KMRSP: u16 = 4;
const E_SID: u16 = 5;
// handshake flags
const F_TSBPDSND: u32 = 0x01;
const F_TSBPDRCV: u32 = 0x02;
const F_CRYPT: u32 = 0x04;
const F_TLPKTDROP: u32 = 0x08;
const F_PERIODICNAK: u32 = 0x10;
const F_REXMIT: u32 = 0x20;

fn word(d: &[u8], at: usize) -> u32 {
    d.get(at..at + 4).map_or(0, |b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
}

fn half(d: &[u8], at: usize) -> u16 {
    d.get(at..at + 2).map_or(0, |b| u16::from_be_bytes([b[0], b[1]]))
}

/// `a - b` for 31-bit sequence numbers, signed.
fn seq_off(a: u32, b: u32) -> i32 {
    let d = a.wrapping_sub(b) & SEQ_MASK;
    if d >= 0x4000_0000 {
        (d as i64 - 0x8000_0000i64) as i32
    } else {
        d as i32
    }
}

fn seq_add(a: u32, n: u32) -> u32 {
    a.wrapping_add(n) & SEQ_MASK
}

/// A packet, split into its header fields and what follows.
#[derive(Debug, PartialEq)]
enum Packet<'a> {
    Data { seq: u32, kk: u8, ts: u32, dst: u32, payload: &'a [u8] },
    Control { kind: u16, sub: u16, info: u32, dst: u32, cif: &'a [u8] },
}

fn parse(d: &[u8]) -> Option<Packet<'_>> {
    if d.len() < HEADER {
        return None;
    }
    let (w0, info, ts, dst) = (word(d, 0), word(d, 4), word(d, 8), word(d, 12));
    Some(if w0 & 0x8000_0000 != 0 {
        Packet::Control { kind: ((w0 >> 16) & 0x7fff) as u16, sub: (w0 & 0xffff) as u16, info, dst, cif: &d[HEADER..] }
    } else {
        Packet::Data { seq: w0, kk: ((info >> 27) & 3) as u8, ts, dst, payload: &d[HEADER..] }
    })
}

fn control(kind: u16, sub: u16, info: u32, ts: u32, dst: u32, cif: &[u8]) -> Vec<u8> {
    let mut v = Vec::with_capacity(HEADER + cif.len());
    v.extend_from_slice(&(0x8000_0000 | ((kind as u32) << 16) | sub as u32).to_be_bytes());
    v.extend_from_slice(&info.to_be_bytes());
    v.extend_from_slice(&ts.to_be_bytes());
    v.extend_from_slice(&dst.to_be_bytes());
    v.extend_from_slice(cif);
    v
}

/// The handshake control information (48 bytes) and its extensions.
#[derive(Debug, Clone, PartialEq, Default)]
struct Handshake {
    version: u32,
    /// The encryption field: the key length in 8-byte units (2, 3 or 4), 0 for none.
    enc: u16,
    /// The extension field: `MAGIC` in an induction response, else flags for the extensions.
    ext: u16,
    isn: u32,
    mtu: u32,
    mfw: u32,
    kind: u32,
    sock: u32,
    cookie: u32,
    exts: Vec<(u16, Vec<u8>)>,
}

impl Handshake {
    fn parse(cif: &[u8]) -> Option<Handshake> {
        if cif.len() < 48 {
            return None;
        }
        let mut h = Handshake {
            version: word(cif, 0),
            enc: half(cif, 4),
            ext: half(cif, 6),
            isn: word(cif, 8) & SEQ_MASK,
            mtu: word(cif, 12),
            mfw: word(cif, 16),
            kind: word(cif, 20),
            sock: word(cif, 24),
            cookie: word(cif, 28),
            exts: Vec::new(),
        };
        let mut at = 48;
        while at + 4 <= cif.len() {
            let (t, n) = (half(cif, at), half(cif, at + 2) as usize * 4);
            at += 4;
            if at + n > cif.len() || h.exts.len() >= 16 {
                return None;
            }
            h.exts.push((t, cif[at..at + n].to_vec()));
            at += n;
        }
        Some(h)
    }

    fn encode(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(48 + 128);
        v.extend_from_slice(&self.version.to_be_bytes());
        v.extend_from_slice(&self.enc.to_be_bytes());
        v.extend_from_slice(&self.ext.to_be_bytes());
        for w in [self.isn, self.mtu, self.mfw, self.kind, self.sock, self.cookie] {
            v.extend_from_slice(&w.to_be_bytes());
        }
        v.extend_from_slice(&[0u8; 16]);
        for (t, body) in &self.exts {
            v.extend_from_slice(&t.to_be_bytes());
            v.extend_from_slice(&((body.len() / 4) as u16).to_be_bytes());
            v.extend_from_slice(body);
        }
        v
    }

    fn ext(&self, t: u16) -> Option<&[u8]> {
        self.exts.iter().find(|(k, _)| *k == t).map(|(_, b)| b.as_slice())
    }

    /// The sender delay (milliseconds) the peer's HSREQ or HSRSP asks for, if it has one.
    fn peer_delay(&self) -> Option<u16> {
        self.ext(E_HSREQ).or_else(|| self.ext(E_HSRSP)).filter(|b| b.len() >= 12).map(|b| half(b, 10))
    }
}

fn hs_packet(h: &Handshake, ts: u32, dst: u32) -> Vec<u8> {
    control(C_HANDSHAKE, 0, 0, ts, dst, &h.encode())
}

/// An HSREQ or HSRSP body: version, flags, receiver delay and sender delay (milliseconds).
fn hs_ext(latency: u16, crypt: bool) -> Vec<u8> {
    let flags = F_TSBPDSND | F_TSBPDRCV | F_TLPKTDROP | F_PERIODICNAK | F_REXMIT | if crypt { F_CRYPT } else { 0 };
    let mut v = Vec::with_capacity(12);
    v.extend_from_slice(&SRT_VERSION.to_be_bytes());
    v.extend_from_slice(&flags.to_be_bytes());
    v.extend_from_slice(&latency.to_be_bytes());
    v.extend_from_slice(&latency.to_be_bytes());
    v
}

/// A stream ID as the SID extension carries it: padded to 4 bytes, each group reversed.
fn sid_encode(s: &str) -> Vec<u8> {
    let mut b = s.as_bytes().to_vec();
    while !b.len().is_multiple_of(4) {
        b.push(0);
    }
    for c in b.chunks_mut(4) {
        c.reverse();
    }
    b
}

fn rejection(code: u32) -> &'static str {
    match code {
        1000 => "unknown reason",
        1001 => "system error",
        1002 => "the sender rejected the connection",
        1003 => "the sender is out of resources",
        1004 => "the sender took this for a rogue connection",
        1005 => "the sender's backlog is full",
        1006 => "internal error at the sender",
        1007 => "the sender is closing",
        1008 => "the SRT versions do not match",
        1009 => "bad rendezvous cookie",
        1010 => "bad passphrase (or one was needed)",
        1011 => "the sender requires encryption and no passphrase matched, or encryption is not wanted",
        1012 => "the message API does not match",
        1013 => "the congestion controllers do not match",
        1014 => "the packet filters do not match",
        1015 => "the groups do not match",
        1016 => "the connection timed out",
        _ => "refused",
    }
}

// ---------------------------------------------------------------------------------------------
// Encryption

fn hmac_sha1(key: &[u8], data: &[u8]) -> [u8; 20] {
    let mut k = [0u8; 64];
    if key.len() > 64 {
        k[..20].copy_from_slice(&sha1(key));
    } else {
        k[..key.len()].copy_from_slice(key);
    }
    let mut inner: Vec<u8> = k.iter().map(|b| b ^ 0x36).collect();
    inner.extend_from_slice(data);
    let mut outer: Vec<u8> = k.iter().map(|b| b ^ 0x5c).collect();
    outer.extend_from_slice(&sha1(&inner));
    sha1(&outer)
}

/// PBKDF2 (RFC 8018) with HMAC-SHA1.
fn pbkdf2_sha1(password: &[u8], salt: &[u8], iterations: u32, out: &mut [u8]) {
    for (i, chunk) in out.chunks_mut(20).enumerate() {
        let mut s = salt.to_vec();
        s.extend_from_slice(&(i as u32 + 1).to_be_bytes());
        let mut u = hmac_sha1(password, &s);
        let mut t = u;
        for _ in 1..iterations {
            u = hmac_sha1(password, &u);
            for (a, b) in t.iter_mut().zip(u) {
                *a ^= b;
            }
        }
        chunk.copy_from_slice(&t[..chunk.len()]);
    }
}

/// The key-encrypting key SRT derives from a passphrase: PBKDF2 over the last 64 bits of the salt,
/// 2048 iterations.
fn kek(pass: &[u8], salt: &[u8; 16], klen: usize) -> Vec<u8> {
    let mut k = vec![0u8; klen];
    pbkdf2_sha1(pass, &salt[8..], 2048, &mut k);
    k
}

enum Cipher {
    A128(Aes128),
    A192(Aes192),
    A256(Aes256),
}

impl Cipher {
    fn new(key: &[u8]) -> Option<Cipher> {
        match key.len() {
            16 => Aes128::new(key).ok().map(Cipher::A128),
            24 => Aes192::new(key).ok().map(Cipher::A192),
            32 => Aes256::new(key).ok().map(Cipher::A256),
            _ => None,
        }
    }

    fn ctr(&self, iv: &[u8; 16], data: &mut [u8]) -> bool {
        match self {
            Cipher::A128(c) => ic_cipher::modes::ctr_xor(c, iv, data).is_ok(),
            Cipher::A192(c) => ic_cipher::modes::ctr_xor(c, iv, data).is_ok(),
            Cipher::A256(c) => ic_cipher::modes::ctr_xor(c, iv, data).is_ok(),
        }
    }
}

/// A key material message (KM), parsed and bounds-checked.
struct Km<'a> {
    /// 1 even, 2 odd, 3 both.
    kk: u8,
    salt: [u8; 16],
    klen: usize,
    /// The wrapped key or keys, with the 8-byte integrity check.
    wrap: &'a [u8],
    /// How many bytes of the message these were.
    used: usize,
}

fn parse_km(b: &[u8]) -> Option<Km<'_>> {
    // S=0, V=1, PT=2; the signature 0x2029
    if b.len() < 16 || b[0] != 0x12 || b[1] != 0x20 || b[2] != 0x29 {
        return None;
    }
    let kk = b[3] & 3;
    // AES-CTR only, no authentication, a 16-byte salt and a 128, 192 or 256-bit key
    if kk == 0 || b[8] != 2 || b[9] != 0 || b[14] != 4 || !matches!(b[15], 4 | 6 | 8) {
        return None;
    }
    let klen = b[15] as usize * 4;
    let wrap_len = 8 + klen * if kk == 3 { 2 } else { 1 };
    let used = 32 + wrap_len;
    if b.len() < used {
        return None;
    }
    Some(Km { kk, salt: b[16..32].try_into().ok()?, klen, wrap: &b[32..used], used })
}

fn build_km(pass: &[u8], salt: &[u8; 16], sek: &[u8]) -> Option<Vec<u8>> {
    let klen = sek.len();
    if !matches!(klen, 16 | 24 | 32) {
        return None;
    }
    let mut km = vec![0x12, 0x20, 0x29, 1, 0, 0, 0, 0, 2, 0, 2, 0, 0, 0, 4, (klen / 4) as u8];
    km.extend_from_slice(salt);
    let k = kek(pass, salt, klen);
    let mut wrapped = vec![0u8; klen + 8];
    let ok = match klen {
        16 => Aes128Kw::wrap(&k, sek, &mut wrapped).is_ok(),
        24 => Aes192Kw::wrap(&k, sek, &mut wrapped).is_ok(),
        _ => Aes256Kw::wrap(&k, sek, &mut wrapped).is_ok(),
    };
    km.extend_from_slice(&wrapped);
    ok.then_some(km)
}

struct Sek {
    cipher: Cipher,
    salt: [u8; 16],
}

/// The stream encrypting keys of a connection, as the passphrase unwraps them.
struct Decryptor {
    pass: Vec<u8>,
    /// Even and odd.
    keys: [Option<Sek>; 2],
}

impl Decryptor {
    fn new(pass: &str) -> Decryptor {
        Decryptor { pass: pass.as_bytes().to_vec(), keys: [None, None] }
    }

    /// Takes the keys a KM carries; false when the passphrase does not unwrap them.
    fn install(&mut self, km: &Km) -> bool {
        let k = kek(&self.pass, &km.salt, km.klen);
        let mut plain = vec![0u8; km.wrap.len() - 8];
        let ok = match km.klen {
            16 => Aes128Kw::unwrap(&k, km.wrap, &mut plain).is_ok(),
            24 => Aes192Kw::unwrap(&k, km.wrap, &mut plain).is_ok(),
            _ => Aes256Kw::unwrap(&k, km.wrap, &mut plain).is_ok(),
        };
        if !ok {
            return false;
        }
        let parities: &[usize] = match km.kk {
            1 => &[0],
            2 => &[1],
            _ => &[0, 1],
        };
        for (i, &p) in parities.iter().enumerate() {
            let Some(cipher) = Cipher::new(&plain[i * km.klen..(i + 1) * km.klen]) else { return false };
            self.keys[p] = Some(Sek { cipher, salt: km.salt });
        }
        true
    }

    /// Decrypts (or encrypts: it is a stream cipher) a packet's payload with the key `kk` names.
    fn apply(&self, kk: u8, seq: u32, data: &mut [u8]) -> bool {
        let Some(Some(s)) = kk.checked_sub(1).and_then(|i| self.keys.get(i as usize)) else { return false };
        let mut iv = [0u8; 16];
        iv[..14].copy_from_slice(&s.salt[..14]);
        for (a, b) in iv[10..14].iter_mut().zip(seq.to_be_bytes()) {
            *a ^= b;
        }
        s.cipher.ctr(&iv, data)
    }
}

// ---------------------------------------------------------------------------------------------
// The receiver

enum Slot {
    /// Not here yet; when it was last asked for (microseconds).
    Missing { reported: i64 },
    Have { ts: i64, payload: Vec<u8> },
    /// The sender gave it up.
    Dropped,
}

#[derive(Debug, Clone, PartialEq)]
enum State {
    Open,
    /// The sender said goodbye.
    Shutdown,
    Broken(String),
}

#[derive(Debug, Clone, Default)]
#[cfg_attr(not(test), allow(dead_code))]
struct Stats {
    received: u64,
    /// Arrived after being reported missing.
    recovered: u64,
    /// Never came in time: skipped when a later packet was due.
    late: u64,
    duplicates: u64,
    /// Out of the window, too big for the buffer, or encrypted in a way that could not be read.
    discarded: u64,
}

struct RxConfig {
    my_id: u32,
    peer_id: u32,
    isn: u32,
    latency_ms: u16,
    crypto: Option<Decryptor>,
    /// The listener's conclusion response, sent again if the caller asks again.
    hs_reply: Option<Vec<u8>>,
}

/// The protocol state of a connection from the receiving end, with no socket: datagrams in
/// (`on_datagram`), time passing (`tick`), datagrams out (`take_output`) and payloads out
/// (`ready`).
struct Receiver {
    my_id: u32,
    peer_id: u32,
    latency_us: i64,
    t0: Instant,
    /// The sequence number of `buf[0]`: everything before it has been handed on or given up.
    head: u32,
    buf: VecDeque<Slot>,
    held: usize,
    out: Vec<Vec<u8>>,
    ready: VecDeque<Vec<u8>>,
    crypto: Option<Decryptor>,
    hs_reply: Option<Vec<u8>>,
    last_km: Option<(Vec<u8>, i64)>,
    started: bool,
    have_time: bool,
    ts_ref: u32,
    ts_ext: i64,
    /// The smallest arrival time minus timestamp seen lately: the sender's clock on ours.
    base: i64,
    win_min: i64,
    win_start: i64,
    rtt: i64,
    rttvar: i64,
    ack_no: u32,
    acks: VecDeque<(u32, i64)>,
    last_ack: i64,
    last_ack_try: i64,
    last_ack_point: Option<u32>,
    last_sent: i64,
    last_recv: i64,
    last_nak: i64,
    rate_start: i64,
    rate_pkts: u32,
    rate_bytes: u64,
    pps: u32,
    bps: u32,
    bad_run: u32,
    state: State,
    stats: Stats,
}

impl Receiver {
    fn new(c: RxConfig, now: Instant) -> Receiver {
        Receiver {
            my_id: c.my_id,
            peer_id: c.peer_id,
            latency_us: c.latency_ms as i64 * 1000,
            t0: now,
            head: c.isn & SEQ_MASK,
            buf: VecDeque::new(),
            held: 0,
            out: Vec::new(),
            ready: VecDeque::new(),
            crypto: c.crypto,
            hs_reply: c.hs_reply,
            last_km: None,
            started: false,
            have_time: false,
            ts_ref: 0,
            ts_ext: 0,
            base: 0,
            win_min: 0,
            win_start: 0,
            rtt: 100_000,
            rttvar: 50_000,
            ack_no: 0,
            acks: VecDeque::new(),
            last_ack: 0,
            last_ack_try: 0,
            last_ack_point: None,
            last_sent: 0,
            last_recv: 0,
            last_nak: 0,
            rate_start: 0,
            rate_pkts: 0,
            rate_bytes: 0,
            pps: 0,
            bps: 0,
            bad_run: 0,
            state: State::Open,
            stats: Stats::default(),
        }
    }

    fn us(&self, now: Instant) -> i64 {
        now.saturating_duration_since(self.t0).as_micros() as i64
    }

    fn send(&mut self, kind: u16, sub: u16, info: u32, cif: &[u8], now: i64) {
        self.out.push(control(kind, sub, info, now as u32, self.peer_id, cif));
        self.last_sent = now;
    }

    fn take_output(&mut self) -> Vec<Vec<u8>> {
        std::mem::take(&mut self.out)
    }

    fn shutdown_packet(&self, now: Instant) -> Vec<u8> {
        control(C_SHUTDOWN, 0, 0, self.us(now) as u32, self.peer_id, &[])
    }

    fn on_datagram(&mut self, d: &[u8], now: Instant) {
        let n = self.us(now);
        match parse(d) {
            Some(Packet::Data { seq, kk, ts, dst, payload }) if dst == self.my_id => {
                self.last_recv = n;
                self.on_data(seq, kk, ts, payload, n);
            }
            Some(Packet::Control { kind, sub, info, dst, cif }) if dst == self.my_id => {
                self.last_recv = n;
                self.on_control(kind, sub, info, cif, n);
            }
            _ => {}
        }
    }

    fn on_data(&mut self, seq: u32, kk: u8, ts: u32, payload: &[u8], now: i64) {
        if payload.is_empty() {
            return;
        }
        let mut off = seq_off(seq, self.head);
        // nothing has come yet: the first packet says where the stream is
        if !self.started && !(0..WINDOW as i32).contains(&off) {
            self.head = seq;
            off = 0;
        }
        if off < 0 {
            // a retransmission of what was handed on or given up, or a repeat
            self.stats.duplicates += 1;
            return;
        }
        if off as usize >= WINDOW || self.held + payload.len() > MAX_HELD {
            self.stats.discarded += 1;
            self.bad_run += 1;
            if self.bad_run > 1000 {
                self.state = State::Broken("the sender's sequence numbers are out of range".into());
            }
            return;
        }
        let off = off as usize;
        if off < self.buf.len() && !matches!(self.buf[off], Slot::Missing { .. }) {
            self.stats.duplicates += 1;
            return;
        }
        let mut data = payload.to_vec();
        match (&self.crypto, kk) {
            (Some(c), 1 | 2) if c.apply(kk, seq, &mut data) => {}
            (None, 0) => {}
            // unencrypted data on an encrypted session, or encrypted data we cannot read
            _ => {
                self.stats.discarded += 1;
                return;
            }
        }
        self.bad_run = 0;
        self.started = true;
        let ts = self.note_time(ts, now);
        self.rate_pkts += 1;
        self.rate_bytes += payload.len() as u64;
        self.stats.received += 1;
        self.held += data.len();
        if off < self.buf.len() {
            self.stats.recovered += 1;
            self.buf[off] = Slot::Have { ts, payload: data };
            return;
        }
        let first_gap = self.buf.len();
        while self.buf.len() < off {
            self.buf.push_back(Slot::Missing { reported: now });
        }
        self.buf.push_back(Slot::Have { ts, payload: data });
        if off > first_gap {
            let range = (seq_add(self.head, first_gap as u32), seq_add(self.head, off as u32 - 1));
            self.nak(&[range], now);
        }
    }

    /// The packet's timestamp on a line that does not wrap, and the clock offset kept up to date.
    fn note_time(&mut self, ts: u32, now: i64) -> i64 {
        if !self.have_time {
            self.have_time = true;
            self.ts_ref = ts;
            self.ts_ext = ts as i64;
            self.base = now - ts as i64;
            self.win_min = self.base;
            self.win_start = now;
            return ts as i64;
        }
        let d = ts.wrapping_sub(self.ts_ref) as i32;
        let ext = self.ts_ext + d as i64;
        if d > 0 {
            self.ts_ref = ts;
            self.ts_ext = ext;
        }
        let delay = now - ext;
        self.win_min = self.win_min.min(delay);
        self.base = self.base.min(delay);
        if now - self.win_start >= 1_000_000 {
            // the sender's clock may run slow against ours: follow it up, slowly
            self.base += (self.win_min - self.base) / 4;
            self.win_min = delay;
            self.win_start = now;
        }
        ext
    }

    fn on_control(&mut self, kind: u16, sub: u16, info: u32, cif: &[u8], now: i64) {
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
            C_SHUTDOWN => {
                self.state = State::Shutdown
            }
            C_ACKACK => {
                if let Some(i) = self.acks.iter().position(|(a, _)| *a == info) {
                    let sample = (now - self.acks[i].1).max(1);
                    self.acks.drain(..=i);
                    self.rttvar = (3 * self.rttvar + (self.rtt - sample).abs()) / 4;
                    self.rtt = (7 * self.rtt + sample) / 8;
                }
            }
            C_DROPREQ if cif.len() >= 8 => self.drop_range(word(cif, 0) & SEQ_MASK, word(cif, 4) & SEQ_MASK),
            C_USER if sub == E_KMREQ => self.on_km(cif, now),
            // keep-alives, congestion warnings and the rest need no answer
            _ => {}
        }
    }

    /// The sender will not send these (a too-late packet it dropped): do not wait for them.
    fn drop_range(&mut self, first: u32, last: u32) {
        let (a, b) = (seq_off(first, self.head), seq_off(last, self.head));
        if b < 0 || a > b || b as usize >= WINDOW {
            return;
        }
        let (a, b) = (a.max(0) as usize, b as usize);
        while self.buf.len() <= b {
            self.buf.push_back(Slot::Dropped);
        }
        for i in a..=b {
            if let Slot::Missing { .. } = self.buf[i] {
                self.buf[i] = Slot::Dropped;
            }
        }
    }

    /// A key refresh: the sender's new keys, answered with the same message as the response.
    fn on_km(&mut self, cif: &[u8], now: i64) {
        let status = |s: u32| s.to_be_bytes().to_vec();
        let Some(c) = self.crypto.as_mut() else {
            self.send(C_USER, E_KMRSP, 0, &status(3), now);
            return;
        };
        let Some(km) = parse_km(cif) else {
            self.send(C_USER, E_KMRSP, 0, &status(4), now);
            return;
        };
        let msg = cif[..km.used].to_vec();
        match &self.last_km {
            // the same message again: answer it without the key derivation
            Some((m, _)) if *m == msg => {}
            // key derivation is slow on purpose: not more than a few a second
            Some((_, at)) if now - at < 250_000 => return,
            _ => {
                let ok = c.install(&km);
                self.last_km = Some((msg.clone(), now));
                if !ok {
                    self.send(C_USER, E_KMRSP, 0, &status(4), now);
                    return;
                }
            }
        }
        self.send(C_USER, E_KMRSP, 0, &msg, now);
    }

    fn nak(&mut self, ranges: &[(u32, u32)], now: i64) {
        let mut words: Vec<u8> = Vec::new();
        for &(a, b) in ranges {
            if a == b {
                words.extend_from_slice(&a.to_be_bytes());
            } else {
                words.extend_from_slice(&(a | 0x8000_0000).to_be_bytes());
                words.extend_from_slice(&b.to_be_bytes());
            }
            // a report fits a datagram
            if words.len() >= 1280 {
                self.send(C_NAK, 0, 0, &words, now);
                words.clear();
            }
        }
        if !words.is_empty() {
            self.send(C_NAK, 0, 0, &words, now);
        }
    }

    fn nak_interval(&self) -> i64 {
        (4 * self.rtt + self.rttvar + 10_000).max(20_000)
    }

    /// Asks again for what has been missing a while.
    fn periodic_nak(&mut self, now: i64) {
        let interval = self.nak_interval();
        let mut runs: Vec<(usize, usize)> = Vec::new();
        let mut open: Option<(usize, usize)> = None;
        for i in 0..self.buf.len() {
            let due = matches!(self.buf[i], Slot::Missing { reported } if reported + interval <= now);
            open = match (due, open) {
                (true, None) => Some((i, i)),
                (true, Some((s, _))) => Some((s, i)),
                (false, Some(r)) => {
                    runs.push(r);
                    None
                }
                (false, None) => None,
            };
            if runs.len() >= MAX_NAK_RANGES {
                open = None;
                break;
            }
        }
        runs.extend(open);
        let mut ranges = Vec::with_capacity(runs.len());
        for (s, e) in runs {
            for i in s..=e {
                if let Slot::Missing { reported } = &mut self.buf[i] {
                    *reported = now;
                }
            }
            ranges.push((seq_add(self.head, s as u32), seq_add(self.head, e as u32)));
        }
        self.nak(&ranges, now);
    }

    fn send_ack(&mut self, now: i64) {
        if !self.started {
            return;
        }
        let run = self.buf.iter().take_while(|s| !matches!(s, Slot::Missing { .. })).count();
        let point = seq_add(self.head, run as u32);
        if self.last_ack_point == Some(point) && now - self.last_ack < 100_000 {
            return;
        }
        if now - self.rate_start >= 1_000_000 {
            let secs = (now - self.rate_start) as f64 / 1e6;
            self.pps = (self.rate_pkts as f64 / secs) as u32;
            self.bps = (self.rate_bytes as f64 / secs) as u32;
            (self.rate_start, self.rate_pkts, self.rate_bytes) = (now, 0, 0);
        }
        self.ack_no = self.ack_no.wrapping_add(1).max(1);
        let avail = WINDOW.saturating_sub(self.buf.len()) as u32;
        let mut cif = Vec::with_capacity(28);
        for w in [point, self.rtt as u32, self.rttvar as u32, avail, self.pps, self.pps, self.bps] {
            cif.extend_from_slice(&w.to_be_bytes());
        }
        let no = self.ack_no;
        self.send(C_ACK, 0, no, &cif, now);
        self.acks.push_back((no, now));
        while self.acks.len() > 1024 {
            self.acks.pop_front();
        }
        self.last_ack = now;
        self.last_ack_point = Some(point);
    }

    /// Whether a packet with this (extended) timestamp is due.
    fn due(&self, ts: i64, now: i64) -> bool {
        ts + self.base + self.latency_us <= now
    }

    /// Hands on what is due, in order, and gives up on what a due packet has overtaken.
    fn deliver(&mut self, now: i64) {
        while self.ready.len() < READY_MAX {
            match self.buf.front() {
                None => break,
                Some(Slot::Dropped) => {
                    self.buf.pop_front();
                    self.head = seq_add(self.head, 1);
                }
                Some(Slot::Have { ts, .. }) => {
                    if !self.due(*ts, now) {
                        break;
                    }
                    if let Some(Slot::Have { payload, .. }) = self.buf.pop_front() {
                        self.held -= payload.len();
                        self.ready.push_back(payload);
                    }
                    self.head = seq_add(self.head, 1);
                }
                Some(Slot::Missing { .. }) => {
                    // the first packet after the gap: if it is due, the gap is too late
                    let next = self.buf.iter().position(|s| matches!(s, Slot::Have { .. }));
                    match next {
                        Some(j) if matches!(&self.buf[j], Slot::Have { ts, .. } if self.due(*ts, now)) => {
                            for _ in 0..j {
                                if let Some(Slot::Missing { .. }) = self.buf.pop_front() {
                                    self.stats.late += 1;
                                }
                                self.head = seq_add(self.head, 1);
                            }
                        }
                        _ => break,
                    }
                }
            }
        }
    }

    /// Everything buffered, in order, without waiting for its time (the sender is gone).
    fn flush(&mut self) -> Vec<Vec<u8>> {
        let mut v: Vec<Vec<u8>> = self.ready.drain(..).collect();
        for s in self.buf.drain(..) {
            if let Slot::Have { payload, .. } = s {
                v.push(payload);
            }
        }
        self.held = 0;
        v
    }

    fn tick(&mut self, now: Instant) {
        let n = self.us(now);
        if n - self.last_recv > EXPIRE {
            self.state = State::Broken(format!("the sender sent nothing for {} s", EXPIRE / 1_000_000));
            return;
        }
        self.deliver(n);
        if n - self.last_ack_try >= ACK_PERIOD {
            self.last_ack_try = n;
            self.send_ack(n);
        }
        if n - self.last_nak >= self.nak_interval() / 2 {
            self.last_nak = n;
            self.periodic_nak(n);
        }
        if n - self.last_sent >= KEEPALIVE {
            self.send(C_KEEPALIVE, 0, 0, &[], n);
        }
    }
}

// ---------------------------------------------------------------------------------------------
// Options and the handshake

#[derive(Debug, Clone)]
struct Options {
    listener: bool,
    host: String,
    port: u16,
    latency: u16,
    streamid: Option<String>,
    passphrase: Option<String>,
}

/// Whether a URL waits for the sender (`mode=listener`, or no host) rather than calling it.
pub(crate) fn is_listener(u: &SourceUrl) -> bool {
    u.option("mode").map_or(u.target.starts_with(':'), |m| m.eq_ignore_ascii_case("listener"))
}

fn options(u: &SourceUrl) -> Result<Options> {
    let (host, port) = host_port(&u.target)?;
    let listener = is_listener(u);
    match u.option("mode").map(str::to_ascii_lowercase).as_deref() {
        None | Some("caller" | "listener") => {}
        Some(other) => return Err(Error::invalid_arg(format!("srt mode must be caller or listener, got '{other}'")).hint("rendezvous mode is not supported")),
    }
    if !listener && host.is_empty() {
        return Err(Error::invalid_arg("an srt caller needs the sender's host: srt://host:port"));
    }
    let latency = match u.option("latency") {
        None => DEFAULT_LATENCY,
        Some(s) => s.parse::<u16>().ok().filter(|v| *v <= MAX_LATENCY).ok_or_else(|| Error::invalid_arg(format!("latency must be a number of milliseconds up to {MAX_LATENCY}, got '{s}'")))?,
    };
    let streamid = u.option("streamid").map(percent_decode);
    if streamid.as_ref().is_some_and(|s| s.is_empty() || s.len() > 512) {
        return Err(Error::invalid_arg("an srt streamid is 1 to 512 bytes"));
    }
    let passphrase = u.option("passphrase").map(percent_decode);
    if passphrase.as_ref().is_some_and(|p| !(10..=79).contains(&p.len())) {
        return Err(Error::invalid_arg("an srt passphrase is 10 to 79 characters"));
    }
    Ok(Options { listener, host: host.to_string(), port, latency, streamid, passphrase })
}

fn io_err(kind: ErrorKind, msg: impl Into<String>) -> std::io::Error {
    std::io::Error::new(kind, msg.into())
}

/// The caller's side of the handshake, ending with the receiver for the connection.
fn handshake_caller(sock: &UdpSocket, peer: SocketAddr, o: &Options, shown: &str) -> Result<Receiver> {
    let failed = |what: String| Error::new(ErrorCode::Io, format!("{shown}: {what}")).hint("check the address, that the sender is running in listener mode, and the passphrase and latency");
    let started = Instant::now();
    let my_id = (u32::from_be_bytes(random_bytes::<4>()) & SEQ_MASK).max(1);
    let isn = u32::from_be_bytes(random_bytes::<4>()) & SEQ_MASK;
    let mut crypto = None;
    let mut km = None;
    if let Some(p) = &o.passphrase {
        let (salt, sek) = (random_bytes::<16>(), random_bytes::<16>());
        let msg = build_km(p.as_bytes(), &salt, &sek).ok_or_else(|| failed("could not make the key material".into()))?;
        let mut d = Decryptor::new(p);
        if !parse_km(&msg).is_some_and(|k| d.install(&k)) {
            return Err(failed("could not make the key material".into()));
        }
        (crypto, km) = (Some(d), Some(msg));
    }
    let base = Handshake { version: 5, isn, mtu: MTU, mfw: WINDOW as u32, sock: my_id, ..Handshake::default() };
    let induction = hs_packet(&Handshake { version: 4, ext: 2, kind: HS_INDUCTION, ..base.clone() }, 0, 0);
    sock.set_read_timeout(Some(Duration::from_millis(20)))?;
    let mut buf = vec![0u8; MAX_DATAGRAM];
    // None until the induction response has come
    let mut conclusion: Option<(Vec<u8>, u32)> = None;
    let mut last_sent: Option<Instant> = None;
    while started.elapsed() < HANDSHAKE_TIMEOUT {
        if last_sent.is_none_or(|t| t.elapsed() >= HANDSHAKE_RESEND) {
            sock.send_to(conclusion.as_ref().map_or(&induction, |c| &c.0), peer)?;
            last_sent = Some(Instant::now());
        }
        let n = match sock.recv_from(&mut buf) {
            Ok((n, from)) if from == peer => n,
            Ok(_) => continue,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted) => continue,
            Err(e) if e.kind() == ErrorKind::ConnectionReset => return Err(failed("nothing is listening there (connection refused)".into())),
            Err(e) => return Err(e.into()),
        };
        let Some(Packet::Control { kind: C_HANDSHAKE, dst, cif, .. }) = parse(&buf[..n]) else { continue };
        let Some(h) = Handshake::parse(cif) else { continue };
        if dst != my_id {
            continue;
        }
        if h.kind >= 1000 && h.kind != HS_CONCLUSION {
            return Err(failed(format!("the sender refused the connection: {}", rejection(h.kind))));
        }
        match &conclusion {
            None if h.kind == HS_INDUCTION => {
                if h.version < 5 || h.ext != MAGIC {
                    return Err(failed("the sender speaks SRT handshake version 4, which is not supported (SRT 1.3 or later is needed)".into()));
                }
                let mut exts = vec![(E_HSREQ, hs_ext(o.latency, o.passphrase.is_some()))];
                if let Some(k) = &km {
                    exts.push((E_KMREQ, k.clone()));
                }
                if let Some(s) = &o.streamid {
                    exts.push((E_SID, sid_encode(s)));
                }
                let ext = 1 | if km.is_some() { 2 } else { 0 } | if o.streamid.is_some() { 4 } else { 0 };
                let enc = if km.is_some() { 2 } else { 0 };
                let c = Handshake { enc, ext, kind: HS_CONCLUSION, cookie: h.cookie, exts, ..base.clone() };
                conclusion = Some((hs_packet(&c, started.elapsed().as_micros() as u32, h.sock), h.sock));
                last_sent = None;
            }
            Some((_, peer_id)) if h.kind == HS_CONCLUSION => {
                if h.version < 5 {
                    return Err(failed("the sender speaks SRT handshake version 4, which is not supported".into()));
                }
                if o.passphrase.is_some() {
                    match h.ext(E_KMRSP) {
                        // a status word: the sender could not use our key
                        Some(b) if b.len() == 4 => return Err(failed("the sender could not use the passphrase (is it the same one?)".into())),
                        Some(b) if b.len() >= 16 => {}
                        _ => return Err(failed("the sender does not encrypt, though a passphrase was given".into())),
                    }
                }
                let latency = o.latency.max(h.peer_delay().unwrap_or(0));
                let rx = Receiver::new(RxConfig { my_id, peer_id: *peer_id, isn: h.isn, latency_ms: latency, crypto, hs_reply: None }, Instant::now());
                return Ok(rx);
            }
            _ => {}
        }
    }
    Err(failed("the sender did not answer the handshake".into()))
}

/// The state a listener keeps to tell a caller that has been through an induction.
struct Cookies {
    secret: [u8; 16],
    t0: Instant,
}

impl Cookies {
    fn make(&self, from: &SocketAddr, bucket: u64) -> u32 {
        let mut m = self.secret.to_vec();
        m.extend_from_slice(from.to_string().as_bytes());
        m.extend_from_slice(&bucket.to_be_bytes());
        word(&sha1(&m), 0)
    }

    fn current(&self, from: &SocketAddr) -> u32 {
        self.make(from, self.t0.elapsed().as_secs() / 60)
    }

    fn valid(&self, from: &SocketAddr, cookie: u32) -> bool {
        let b = self.t0.elapsed().as_secs() / 60;
        cookie == self.make(from, b) || (b > 0 && cookie == self.make(from, b - 1))
    }
}

/// Waits for a caller and answers its handshake; `None` when asked to stop first.
fn accept(sock: &UdpSocket, o: &Options, stop: &AtomicBool) -> std::io::Result<Option<(Receiver, SocketAddr)>> {
    let my_id = (u32::from_be_bytes(random_bytes::<4>()) & SEQ_MASK).max(1);
    let cookies = Cookies { secret: random_bytes::<16>(), t0: Instant::now() };
    sock.set_read_timeout(Some(Duration::from_millis(20)))?;
    let mut buf = vec![0u8; MAX_DATAGRAM];
    while !stop.load(Ordering::Relaxed) {
        let (n, from) = match sock.recv_from(&mut buf) {
            Ok(x) => x,
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted | ErrorKind::ConnectionReset) => continue,
            Err(e) => return Err(e),
        };
        let Some(Packet::Control { kind: C_HANDSHAKE, dst, cif, .. }) = parse(&buf[..n]) else { continue };
        let Some(h) = Handshake::parse(cif) else { continue };
        let reply = Handshake { version: 5, mtu: h.mtu.min(MTU), mfw: h.mfw.min(WINDOW as u32), isn: h.isn, sock: my_id, cookie: cookies.current(&from), ..Handshake::default() };
        if h.kind == HS_INDUCTION && dst == 0 && h.version >= 4 {
            let r = Handshake { ext: MAGIC, kind: HS_INDUCTION, ..reply };
            sock.send_to(&hs_packet(&r, 0, h.sock), from)?;
        } else if h.kind == HS_CONCLUSION && dst == my_id && h.version >= 5 && cookies.valid(&from, h.cookie) {
            let reject = |code: u32| hs_packet(&Handshake { kind: code, cookie: h.cookie, ..reply.clone() }, 0, h.sock);
            let mut crypto = None;
            let mut exts = vec![(E_HSRSP, hs_ext(o.latency, o.passphrase.is_some()))];
            let mut enc = 0;
            match (&o.passphrase, h.ext(E_KMREQ)) {
                (None, None) => {}
                (None, Some(_)) => {
                    sock.send_to(&reject(REJ_BADSECRET), from)?;
                    continue;
                }
                (Some(_), None) => {
                    sock.send_to(&reject(REJ_UNSECURE), from)?;
                    continue;
                }
                (Some(p), Some(b)) => {
                    let mut d = Decryptor::new(p);
                    match parse_km(b) {
                        Some(km) if d.install(&km) => {
                            enc = (km.klen / 8) as u16;
                            exts.push((E_KMRSP, b[..km.used].to_vec()));
                            crypto = Some(d);
                        }
                        _ => {
                            sock.send_to(&reject(REJ_BADSECRET), from)?;
                            continue;
                        }
                    }
                }
            }
            let ext = 1 | if enc != 0 { 2 } else { 0 };
            let r = Handshake { enc, ext, kind: HS_CONCLUSION, exts, ..reply };
            let pkt = hs_packet(&r, 0, h.sock);
            sock.send_to(&pkt, from)?;
            let latency = o.latency.max(h.peer_delay().unwrap_or(0));
            let rx = Receiver::new(RxConfig { my_id, peer_id: h.sock, isn: h.isn, latency_ms: latency, crypto, hs_reply: Some(pkt) }, Instant::now());
            return Ok(Some((rx, from)));
        }
    }
    Ok(None)
}

// ---------------------------------------------------------------------------------------------
// The source

/// Runs a connection until it ends: datagrams in, protocol, payloads on to `tx`.
fn pump(sock: &UdpSocket, peer: SocketAddr, mut rx: Receiver, tx: &SyncSender<std::io::Result<Vec<u8>>>, stop: &AtomicBool) {
    let _ = sock.set_read_timeout(Some(POLL));
    let mut buf = vec![0u8; MAX_DATAGRAM];
    loop {
        if stop.load(Ordering::Relaxed) {
            let _ = sock.send_to(&rx.shutdown_packet(Instant::now()), peer);
            return;
        }
        match sock.recv_from(&mut buf) {
            Ok((n, from)) if from == peer => rx.on_datagram(&buf[..n], Instant::now()),
            Ok(_) => {}
            // Windows reports an ICMP "port unreachable" from an earlier send as a reset
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted | ErrorKind::ConnectionReset) => {}
            Err(e) => {
                let _ = tx.send(Err(e));
                return;
            }
        }
        rx.tick(Instant::now());
        for p in rx.take_output() {
            let _ = sock.send_to(&p, peer);
        }
        while let Some(chunk) = rx.ready.pop_front() {
            match tx.try_send(Ok(chunk)) {
                Ok(()) => {}
                Err(TrySendError::Full(item)) => {
                    if let Ok(c) = item {
                        rx.ready.push_front(c);
                    }
                    break;
                }
                Err(TrySendError::Disconnected(_)) => {
                    let _ = sock.send_to(&rx.shutdown_packet(Instant::now()), peer);
                    return;
                }
            }
        }
        match std::mem::replace(&mut rx.state, State::Open) {
            State::Open => {}
            State::Shutdown => {
                // what the sender sent before it left still counts
                for c in rx.flush() {
                    if tx.send(Ok(c)).is_err() {
                        return;
                    }
                }
                return;
            }
            State::Broken(why) => {
                let _ = tx.send(Err(io_err(ErrorKind::TimedOut, why)));
                return;
            }
        }
    }
}

/// An SRT connection read as a source.
pub(crate) struct SrtSource {
    inner: ThreadSource,
    stop: Arc<AtomicBool>,
}

impl Drop for SrtSource {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Source for SrtSource {
    fn info(&self) -> &SourceInfo {
        self.inner.info()
    }
    fn read(&mut self, buf: &mut [u8], wait: Duration) -> Result<Chunk> {
        self.inner.read(buf, wait)
    }
}

/// Opens an SRT source: a caller makes the connection (through the address allow-list) before
/// returning; a listener is bound and waits for the sender.
pub(crate) fn open(u: &SourceUrl, shown: String) -> Result<SrtSource> {
    let o = options(u)?;
    let unreachable = |e: std::io::Error, what: &str| Error::new(ErrorCode::Io, format!("{what} {shown}: {e}")).hint("check the address, and that the sender is running");
    let info = SourceInfo { url: shown.clone(), kind: "srt", live: true, synthetic: false };
    let (sock, first) = if o.listener {
        let host = if o.host.is_empty() { "127.0.0.1" } else { &o.host };
        (UdpSocket::bind((host, o.port)).map_err(|e| unreachable(e, "could not listen on"))?, None)
    } else {
        let peer = net::resolve_udp(&o.host, o.port, &Allow::from_env()?, &shown)?;
        let sock = UdpSocket::bind(if peer.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" }).map_err(|e| unreachable(e, "could not open a socket for"))?;
        let rx = handshake_caller(&sock, peer, &o, &shown)?;
        (sock, Some((rx, peer)))
    };
    let stop = Arc::new(AtomicBool::new(false));
    let (tx, rx) = sync_channel(256);
    let flag = stop.clone();
    std::thread::Builder::new()
        .name("ironav-srt".into())
        .spawn(move || {
            let (receiver, peer) = match first {
                Some(x) => x,
                None => match accept(&sock, &o, &flag) {
                    Ok(Some(x)) => x,
                    Ok(None) => return,
                    Err(e) => {
                        let _ = tx.send(Err(e));
                        return;
                    }
                },
            };
            pump(&sock, peer, receiver, &tx, &flag);
        })
        .expect("spawning a thread");
    Ok(SrtSource { inner: ThreadSource::from_channel(info, rx), stop })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::open_source;
    use std::net::Ipv4Addr;

    // ---- a small deterministic generator, for fuzzing without a dependency
    struct Rng(u64);
    impl Rng {
        fn next(&mut self) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0
        }
        fn junk(&mut self, max: u64) -> Vec<u8> {
            let n = self.next() % max;
            self.bytes(n as usize)
        }
        fn bytes(&mut self, n: usize) -> Vec<u8> {
            (0..n).map(|_| self.next() as u8).collect()
        }
    }

    fn hex(s: &str) -> Vec<u8> {
        (0..s.len() / 2).map(|i| u8::from_str_radix(&s[2 * i..2 * i + 2], 16).unwrap()).collect()
    }

    fn data_packet(seq: u32, ts: u32, dst: u32, kk: u8, payload: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&seq.to_be_bytes());
        // a solo message, in order, the key bits, and a message number
        v.extend_from_slice(&(0xe000_0000u32 | ((kk as u32) << 27) | 1).to_be_bytes());
        v.extend_from_slice(&ts.to_be_bytes());
        v.extend_from_slice(&dst.to_be_bytes());
        v.extend_from_slice(payload);
        v
    }

    #[test]
    fn sequence_numbers_wrap_at_31_bits() {
        assert_eq!(seq_off(5, 3), 2);
        assert_eq!(seq_off(3, 5), -2);
        assert_eq!(seq_off(1, 0x7fff_ffff), 2);
        assert_eq!(seq_off(0x7fff_fffe, 2), -4);
        assert_eq!(seq_add(0x7fff_ffff, 1), 0);
    }

    #[test]
    fn packets_parse_and_short_ones_do_not() {
        let d = data_packet(0x1234, 99, 7, 2, b"abc");
        assert_eq!(parse(&d), Some(Packet::Data { seq: 0x1234, kk: 2, ts: 99, dst: 7, payload: b"abc" }));
        let c = control(C_NAK, 0, 5, 11, 8, &[1, 2, 3, 4]);
        assert_eq!(parse(&c), Some(Packet::Control { kind: C_NAK, sub: 0, info: 5, dst: 8, cif: &[1, 2, 3, 4] }));
        for n in 0..HEADER {
            assert_eq!(parse(&c[..n]), None);
        }
    }

    #[test]
    fn handshakes_round_trip_and_bad_extensions_are_refused() {
        let h = Handshake { version: 5, enc: 2, ext: 7, isn: 0x1111, mtu: 1500, mfw: 8192, kind: HS_CONCLUSION, sock: 42, cookie: 0xdead_beef, exts: vec![(E_HSREQ, hs_ext(120, false)), (E_SID, sid_encode("abc"))] };
        let bytes = h.encode();
        assert_eq!(Handshake::parse(&bytes), Some(h.clone()));
        assert_eq!(h.peer_delay(), Some(120));
        // an extension longer than the packet, and a truncated header
        let mut bad = bytes.clone();
        bad[48 + 2] = 0xff;
        assert_eq!(Handshake::parse(&bad), None);
        assert_eq!(Handshake::parse(&bytes[..47]), None);
        // a stream ID is padded to words, each reversed
        assert_eq!(sid_encode("abcde"), [b'd', b'c', b'b', b'a', 0, 0, 0, b'e']);
    }

    #[test]
    fn pbkdf2_and_hmac_match_their_published_vectors() {
        // RFC 2202, test case 1
        assert_eq!(hmac_sha1(&[0x0b; 20], b"Hi There"), hex("b617318655057264e28bc0b6fb378c8ef146be00")[..]);
        // RFC 6070, 2 iterations; and 4096 iterations, whose 25 bytes span two blocks
        let mut k = [0u8; 20];
        pbkdf2_sha1(b"password", b"salt", 2, &mut k);
        assert_eq!(k, hex("ea6c014dc72d6f8ccd1ed92ace1d41f0d8de8957")[..]);
        let mut k = [0u8; 25];
        pbkdf2_sha1(b"passwordPASSWORDpassword", b"saltSALTsaltSALTsaltSALTsaltSALTsalt", 4096, &mut k);
        assert_eq!(k, hex("3d2eec4fe41c849b80c8d83662c0e44a8b291a964cf2f07038")[..]);
    }

    #[test]
    fn key_material_wraps_and_unwraps_and_a_wrong_passphrase_fails() {
        for klen in [16usize, 24, 32] {
            let (salt, sek) = (random_bytes::<16>(), vec![7u8; klen]);
            let km = build_km(b"correct horse", &salt, &sek).unwrap();
            assert_eq!(km.len(), 32 + 8 + klen);
            let mut d = Decryptor::new("correct horse");
            assert!(d.install(&parse_km(&km).unwrap()));
            let mut bad = Decryptor::new("wrong passphrase");
            assert!(!bad.install(&parse_km(&km).unwrap()));
            // CTR is its own inverse, and the packet number is part of the IV
            let mut x = b"some transport stream bytes".to_vec();
            assert!(d.apply(1, 9, &mut x));
            assert_ne!(x, b"some transport stream bytes");
            let mut y = x.clone();
            assert!(d.apply(1, 10, &mut y));
            assert_ne!(y, b"some transport stream bytes");
            assert!(d.apply(1, 9, &mut x));
            assert_eq!(x, b"some transport stream bytes");
            // no odd key was sent
            assert!(!d.apply(2, 9, &mut x));
        }
        // a key material message that lies about its length
        let km = build_km(b"correct horse", &[1; 16], &[2; 16]).unwrap();
        assert!(parse_km(&km[..km.len() - 1]).is_none());
        let mut v = km.clone();
        v[15] = 5;
        assert!(parse_km(&v).is_none());
    }

    fn receiver(latency: u16, isn: u32) -> (Receiver, Instant) {
        let t0 = Instant::now();
        (Receiver::new(RxConfig { my_id: 1, peer_id: 2, isn, latency_ms: latency, crypto: None, hs_reply: None }, t0), t0)
    }

    fn at(t0: Instant, ms: u64) -> Instant {
        t0 + Duration::from_millis(ms)
    }

    fn naks(out: &[Vec<u8>]) -> Vec<u32> {
        out.iter()
            .filter_map(|p| match parse(p) {
                Some(Packet::Control { kind: C_NAK, cif, .. }) => Some(cif.chunks(4).map(|c| word(c, 0)).collect::<Vec<_>>()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    #[test]
    fn packets_are_handed_on_at_their_time_plus_the_latency_in_order() {
        let (mut r, t0) = receiver(100, 1000);
        // packets 1000..1003 sent 10 ms apart, 1002 arriving before 1001
        for (seq, ms_ts, arrive) in [(1000, 0u32, 5u64), (1002, 20_000, 26), (1001, 10_000, 27)] {
            r.on_datagram(&data_packet(seq, ms_ts, 1, 0, &[(seq - 1000) as u8]), at(t0, arrive));
            r.tick(at(t0, arrive));
        }
        assert!(r.ready.is_empty(), "nothing is due before the latency has passed");
        // the first is due 100 ms after it arrived (5 ms), the others follow at their spacing
        r.tick(at(t0, 104));
        assert!(r.ready.is_empty());
        r.tick(at(t0, 106));
        assert_eq!(r.ready.drain(..).collect::<Vec<_>>(), vec![vec![0u8]]);
        r.tick(at(t0, 140));
        let got: Vec<Vec<u8>> = r.ready.drain(..).collect();
        assert_eq!(got, vec![vec![1u8], vec![2u8]]);
        assert_eq!(r.head, 1003);
    }

    #[test]
    fn a_gap_is_reported_at_once_and_again_until_it_is_filled() {
        let (mut r, t0) = receiver(5000, 0);
        r.on_datagram(&data_packet(0, 0, 1, 0, b"a"), at(t0, 0));
        r.on_datagram(&data_packet(4, 4_000, 1, 0, b"e"), at(t0, 4));
        let out = r.take_output();
        assert_eq!(naks(&out), vec![1 | 0x8000_0000, 3]);
        // nothing new until the report is due again; then the whole gap again, until one comes
        r.tick(at(t0, 6));
        assert!(naks(&r.take_output()).is_empty());
        r.tick(at(t0, 470));
        assert_eq!(naks(&r.take_output()), vec![1 | 0x8000_0000, 3]);
        r.on_datagram(&data_packet(2, 2_000, 1, 0, b"c"), at(t0, 471));
        r.tick(at(t0, 1000));
        assert_eq!(naks(&r.take_output()), vec![1, 3]);
        assert_eq!(r.stats.recovered, 1);
        // a repeat of what is there is not counted twice
        r.on_datagram(&data_packet(2, 2_000, 1, 0, b"c"), at(t0, 1001));
        assert_eq!(r.stats.duplicates, 1);
    }

    #[test]
    fn a_packet_that_misses_its_time_is_given_up_on() {
        let (mut r, t0) = receiver(50, 10);
        r.on_datagram(&data_packet(10, 0, 1, 0, b"a"), at(t0, 0));
        r.on_datagram(&data_packet(12, 20_000, 1, 0, b"c"), at(t0, 20));
        r.tick(at(t0, 55));
        assert_eq!(r.ready.drain(..).collect::<Vec<_>>(), vec![b"a".to_vec()]);
        // 11 is missing and 12 is not yet due
        r.tick(at(t0, 60));
        assert!(r.ready.is_empty());
        r.tick(at(t0, 71));
        assert_eq!(r.ready.drain(..).collect::<Vec<_>>(), vec![b"c".to_vec()]);
        assert_eq!(r.stats.late, 1);
        assert_eq!(r.head, 13);
        // 11 arriving now is of no use
        r.on_datagram(&data_packet(11, 10_000, 1, 0, b"b"), at(t0, 80));
        assert!(r.ready.is_empty() && r.buf.is_empty());
    }

    #[test]
    fn the_sender_can_give_packets_up() {
        let (mut r, t0) = receiver(500, 0);
        r.on_datagram(&data_packet(0, 0, 1, 0, b"a"), at(t0, 0));
        r.on_datagram(&data_packet(3, 3_000, 1, 0, b"d"), at(t0, 3));
        let mut cif = 1u32.to_be_bytes().to_vec();
        cif.extend_from_slice(&2u32.to_be_bytes());
        r.on_datagram(&control(C_DROPREQ, 0, 0, 0, 1, &cif), at(t0, 4));
        r.take_output();
        r.tick(at(t0, 12));
        // the ACK point has passed the dropped packets
        let ack = r.take_output().into_iter().find_map(|p| match parse(&p) {
            Some(Packet::Control { kind: C_ACK, cif, .. }) => Some(word(cif, 0)),
            _ => None,
        });
        assert_eq!(ack, Some(4));
        r.tick(at(t0, 600));
        assert_eq!(r.ready.len(), 2);
        assert_eq!(r.stats.late, 0);
    }

    #[test]
    fn acks_come_every_10_ms_and_an_ackack_measures_the_round_trip() {
        let (mut r, t0) = receiver(100, 0);
        r.on_datagram(&data_packet(0, 0, 1, 0, b"a"), at(t0, 0));
        r.tick(at(t0, 10));
        let out = r.take_output();
        let (no, cif) = out.iter().find_map(|p| match parse(p) {
            Some(Packet::Control { kind: C_ACK, info, cif, .. }) => Some((info, cif.to_vec())),
            _ => None,
        }).expect("a full ACK");
        assert_eq!(cif.len(), 28);
        assert_eq!(word(&cif, 0), 1, "the sequence number after the last one received in order");
        r.on_datagram(&control(C_ACKACK, 0, no, 0, 1, &[]), at(t0, 14));
        assert!(r.rtt < 100_000 && r.rtt > 80_000, "{}", r.rtt);
        // no more ACKs while nothing moves, until a sender hears nothing for a while
        r.tick(at(t0, 30));
        assert!(!r.take_output().iter().any(|p| matches!(parse(p), Some(Packet::Control { kind: C_ACK, .. }))));
    }

    #[test]
    fn keep_alives_shutdown_and_silence() {
        let (mut r, t0) = receiver(100, 0);
        r.tick(at(t0, 1100));
        assert!(r.take_output().iter().any(|p| matches!(parse(p), Some(Packet::Control { kind: C_KEEPALIVE, .. }))));
        r.on_datagram(&control(C_SHUTDOWN, 0, 0, 0, 1, &[]), at(t0, 1200));
        assert_eq!(r.state, State::Shutdown);
        let (mut r, t0) = receiver(100, 0);
        r.tick(at(t0, 4000));
        assert_eq!(r.state, State::Open);
        r.tick(at(t0, 5100));
        assert!(matches!(r.state, State::Broken(_)));
    }

    #[test]
    fn sequence_numbers_and_timestamps_wrap() {
        let (mut r, t0) = receiver(10, 0x7fff_fffe);
        for (i, seq) in [0x7fff_fffeu32, 0x7fff_ffff, 0, 1].into_iter().enumerate() {
            // the 32-bit microsecond timestamp wraps between the second and the third
            let ts = 0xffff_fff0u32.wrapping_add(i as u32 * 10);
            r.on_datagram(&data_packet(seq, ts, 1, 0, &[i as u8]), at(t0, i as u64));
        }
        r.tick(at(t0, 100));
        assert_eq!(r.ready.drain(..).collect::<Vec<_>>(), vec![vec![0u8], vec![1], vec![2], vec![3]]);
        assert_eq!(r.head, 2);
    }

    #[test]
    fn encrypted_packets_are_decrypted_and_the_wrong_kind_is_not_taken() {
        let (salt, sek) = (random_bytes::<16>(), random_bytes::<16>());
        let km = build_km(b"0123456789", &salt, &sek).unwrap();
        let mut tx_side = Decryptor::new("0123456789");
        assert!(tx_side.install(&parse_km(&km).unwrap()));
        let mut rx_side = Decryptor::new("0123456789");
        assert!(rx_side.install(&parse_km(&km).unwrap()));
        let t0 = Instant::now();
        let mut r = Receiver::new(RxConfig { my_id: 1, peer_id: 2, isn: 5, latency_ms: 0, crypto: Some(rx_side), hs_reply: None }, t0);
        let mut p = b"secret ts bytes".to_vec();
        assert!(tx_side.apply(1, 5, &mut p));
        r.on_datagram(&data_packet(5, 0, 1, 1, &p), at(t0, 0));
        // plaintext on an encrypted session, and data under a key never sent
        r.on_datagram(&data_packet(6, 0, 1, 0, b"plain"), at(t0, 0));
        r.on_datagram(&data_packet(6, 0, 1, 2, b"odd"), at(t0, 0));
        r.tick(at(t0, 5));
        assert_eq!(r.ready.drain(..).collect::<Vec<_>>(), vec![b"secret ts bytes".to_vec()]);
        assert_eq!(r.stats.discarded, 2);
        // a key refresh with the same passphrase is answered with the same message; another is not
        r.on_datagram(&control(C_USER, E_KMREQ, 0, 0, 1, &km), at(t0, 10));
        let rsp = r.take_output().into_iter().find_map(|p| match parse(&p) {
            Some(Packet::Control { kind: C_USER, sub: E_KMRSP, cif, .. }) => Some(cif.to_vec()),
            _ => None,
        });
        assert_eq!(rsp.as_deref(), Some(&km[..]));
        let other = build_km(b"another one!", &salt, &sek).unwrap();
        r.on_datagram(&control(C_USER, E_KMREQ, 0, 0, 1, &other), at(t0, 500));
        let rsp = r.take_output().into_iter().find_map(|p| match parse(&p) {
            Some(Packet::Control { kind: C_USER, sub: E_KMRSP, cif, .. }) => Some(cif.to_vec()),
            _ => None,
        });
        assert_eq!(rsp, Some(4u32.to_be_bytes().to_vec()));
    }

    #[test]
    fn random_and_truncated_packets_are_survived_with_bounded_buffers() {
        let mut rng = Rng(0x9e37_79b9_7f4a_7c15);
        let salt = [3u8; 16];
        let km = build_km(b"0123456789", &salt, &[9u8; 16]).unwrap();
        let mut d = Decryptor::new("0123456789");
        d.install(&parse_km(&km).unwrap());
        let t0 = Instant::now();
        let mut r = Receiver::new(RxConfig { my_id: 1, peer_id: 2, isn: 0, latency_ms: 20, crypto: Some(d), hs_reply: Some(vec![1, 2, 3]) }, t0);
        let mut hs = Handshake { version: 5, kind: HS_CONCLUSION, exts: vec![(E_HSREQ, hs_ext(5, false))], ..Handshake::default() }.encode();
        let mut examples: Vec<Vec<u8>> = vec![
            data_packet(0, 0, 1, 1, &[1; 100]),
            control(C_NAK, 0, 0, 0, 1, &[0; 16]),
            control(C_DROPREQ, 0, 0, 0, 1, &[0; 8]),
            control(C_USER, E_KMREQ, 0, 0, 1, &km),
            control(C_HANDSHAKE, 0, 0, 0, 1, &hs),
        ];
        hs.truncate(60);
        examples.push(control(C_HANDSHAKE, 0, 0, 0, 1, &hs));
        for i in 0..20_000u64 {
            let p = match rng.next() % 4 {
                // pure noise, even the destination socket
                0 => rng.junk(200),
                // a valid-looking header for us with a random body, of any control type
                1 => {
                    let mut v = control((rng.next() % 12) as u16, (rng.next() % 6) as u16, rng.next() as u32, rng.next() as u32, 1, &rng.junk(120));
                    if rng.next().is_multiple_of(5) {
                        v[0] = 0xff;
                    }
                    v
                }
                // data with any sequence number, and any key bits
                2 => data_packet(rng.next() as u32 & if rng.next().is_multiple_of(2) { 0x7fff_ffff } else { 0x3ff }, rng.next() as u32, 1, (rng.next() % 4) as u8, &rng.junk(1400)),
                // a good packet cut short or flipped
                _ => {
                    let mut v = examples[(rng.next() % examples.len() as u64) as usize].clone();
                    let cut = (rng.next() % (v.len() as u64 + 1)) as usize;
                    v.truncate(cut);
                    if !v.is_empty() && rng.next().is_multiple_of(2) {
                        let k = (rng.next() % v.len() as u64) as usize;
                        v[k] ^= 1 << (rng.next() % 8);
                    }
                    v
                }
            };
            let now = at(t0, i / 10);
            r.on_datagram(&p, now);
            r.tick(now);
            r.take_output();
            r.ready.clear();
            if matches!(r.state, State::Shutdown | State::Broken(_)) {
                r.state = State::Open;
            }
            assert!(r.buf.len() <= WINDOW && r.held <= MAX_HELD && r.acks.len() <= 1024, "{} {}", r.buf.len(), r.held);
        }
        // and the parsers, on their own
        for _ in 0..5000 {
            let b = rng.junk(300);
            let _ = parse(&b);
            let _ = Handshake::parse(&b);
            let _ = parse_km(&b);
        }
    }

    // ---- an SRT sender, to test against

    #[derive(Clone)]
    struct SenderConfig {
        /// Payloads to send, one packet each.
        payloads: Arc<Vec<Vec<u8>>>,
        /// A packet is not sent the first time when its index modulo `.0` is `.1`.
        lose: Option<(usize, usize)>,
        /// The packet after every this-many is sent first.
        swap: Option<usize>,
        passphrase: Option<String>,
        streamid: Option<String>,
        latency: u16,
        /// Microseconds of timestamp between packets, and the pause between them.
        interval: Duration,
    }

    #[derive(Debug, Default)]
    struct Saw {
        streamid: Option<String>,
        naks: usize,
        resent: usize,
        acks: usize,
        encrypted: bool,
        sent_flags_ok: bool,
        shut: bool,
        acked: usize,
    }

    enum Role {
        /// Waits for the source to call.
        Listen,
        /// Calls the source, which listens.
        Call(SocketAddr),
    }

    fn run_sender(sock: UdpSocket, role: Role, cfg: SenderConfig) -> std::thread::JoinHandle<Saw> {
        std::thread::spawn(move || {
            let mut saw = Saw::default();
            let t_start = Instant::now();
            let ts_now = |t: Instant| t.duration_since(t_start).as_micros() as u32;
            sock.set_read_timeout(Some(Duration::from_millis(5))).unwrap();
            let mut buf = vec![0u8; 65536];
            let my_id = 0x2222u32;
            let mut enc = None;
            // the caller's sequence number starts both directions
            let (peer, peer_id, isn) = match role {
                Role::Listen => {
                    let (mut peer_id, mut cookie) = (0, 0);
                    loop {
                        let (n, from) = match sock.recv_from(&mut buf) {
                            Ok(x) => x,
                            Err(_) => continue,
                        };
                        let Some(Packet::Control { kind: C_HANDSHAKE, dst, cif, .. }) = parse(&buf[..n]) else { continue };
                        let h = Handshake::parse(cif).unwrap();
                        if h.kind == HS_INDUCTION {
                            (peer_id, cookie) = (h.sock, 0xc00c1e);
                            let r = Handshake { version: 5, ext: MAGIC, isn: h.isn, mtu: 1500, mfw: 8192, kind: HS_INDUCTION, sock: my_id, cookie, ..Handshake::default() };
                            sock.send_to(&hs_packet(&r, 0, peer_id), from).unwrap();
                        } else if h.kind == HS_CONCLUSION && dst == my_id {
                            assert_eq!(h.cookie, cookie);
                            saw.streamid = h.ext(E_SID).map(|b| {
                                let mut v = b.to_vec();
                                for c in v.chunks_mut(4) {
                                    c.reverse();
                                }
                                String::from_utf8(v).unwrap().trim_end_matches('\0').to_string()
                            });
                            let mut exts = vec![(E_HSRSP, hs_ext(cfg.latency, false))];
                            if let Some(p) = &cfg.passphrase {
                                let kmreq = h.ext(E_KMREQ).expect("a KMREQ").to_vec();
                                let mut d = Decryptor::new(p);
                                assert!(d.install(&parse_km(&kmreq).unwrap()));
                                enc = Some(d);
                                exts.push((E_KMRSP, kmreq));
                            }
                            let r = Handshake { version: 5, enc: 2, ext: 1, isn: h.isn, mtu: 1500, mfw: 8192, kind: HS_CONCLUSION, sock: my_id, cookie, exts };
                            sock.send_to(&hs_packet(&r, 0, peer_id), from).unwrap();
                            saw.sent_flags_ok = h.ext(E_HSREQ).is_some_and(|b| word(b, 4) & (F_TSBPDSND | F_TSBPDRCV | F_TLPKTDROP) == (F_TSBPDSND | F_TSBPDRCV | F_TLPKTDROP));
                            break (from, peer_id, h.isn);
                        }
                    }
                }
                Role::Call(target) => {
                    // near the wrap of the 31-bit sequence numbers
                    let isn = 0x7fff_fff0u32;
                    let ind =Handshake { version: 4, ext: 2, isn, mtu: 1500, mfw: 8192, kind: HS_INDUCTION, sock: my_id, ..Handshake::default() };
                    let mut cookie = None;
                    let mut peer_id = 0;
                    let mut last = None::<Instant>;
                    let mut sek_km = None;
                    if let Some(p) = &cfg.passphrase {
                        let salt = random_bytes::<16>();
                        let km = build_km(p.as_bytes(), &salt, &random_bytes::<16>()).unwrap();
                        let mut d = Decryptor::new(p);
                        assert!(d.install(&parse_km(&km).unwrap()));
                        enc = Some(d);
                        sek_km = Some(km);
                    }
                    loop {
                        if last.is_none_or(|t| t.elapsed() > Duration::from_millis(100)) {
                            let pkt = match cookie {
                                None => hs_packet(&ind, 0, 0),
                                Some(c) => {
                                    let mut exts = vec![(E_HSREQ, hs_ext(cfg.latency, false))];
                                    if let Some(k) = &sek_km {
                                        exts.push((E_KMREQ, k.clone()));
                                    }
                                    if let Some(s) = &cfg.streamid {
                                        exts.push((E_SID, sid_encode(s)));
                                    }
                                    hs_packet(&Handshake { version: 5, enc: 2, ext: 7, kind: HS_CONCLUSION, cookie: c, exts, ..ind.clone() }, 0, peer_id)
                                }
                            };
                            sock.send_to(&pkt, target).unwrap();
                            last = Some(Instant::now());
                        }
                        let Ok((n, _)) = sock.recv_from(&mut buf) else { continue };
                        let Some(Packet::Control { kind: C_HANDSHAKE, cif, .. }) = parse(&buf[..n]) else { continue };
                        let h = Handshake::parse(cif).unwrap();
                        if h.kind == HS_INDUCTION && cookie.is_none() {
                            cookie = Some(h.cookie);
                            peer_id = h.sock;
                            last = None;
                        } else if h.kind == HS_CONCLUSION {
                            break (target, peer_id, isn);
                        }
                    }
                }
            };
            // the handshake is done: send
            let mut packets: Vec<Vec<u8>> = Vec::new();
            for (i, p) in cfg.payloads.iter().enumerate() {
                let seq = seq_add(isn, i as u32);
                let mut data = p.clone();
                let kk = if let Some(d) = &enc {
                    assert!(d.apply(1, seq, &mut data));
                    saw.encrypted = true;
                    1
                } else {
                    0
                };
                // the timestamp is the sender's clock when the packet is first sent
                packets.push(data_packet(seq, 0, peer_id, kk, &data));
            }
            let first_seq = isn;
            let total = packets.len();
            let mut next = 0usize;
            let mut held: Option<usize> = None;
            let mut last_send = Instant::now();
            let mut acked_to = 0usize;
            let deadline = Instant::now() + Duration::from_secs(20);
            let send_pkt = |i: usize, again: bool, packets: &[Vec<u8>]| {
                let mut p = packets[i].clone();
                if again {
                    p[4] |= 0x04;
                }
                sock.send_to(&p, peer).unwrap();
            };
            while acked_to < total && Instant::now() < deadline {
                if next < total && last_send.elapsed() >= cfg.interval {
                    last_send = Instant::now();
                    let i = next;
                    next += 1;
                    packets[i][8..12].copy_from_slice(&ts_now(Instant::now()).to_be_bytes());
                    let lost = cfg.lose.is_some_and(|(m, k)| i % m == k);
                    let hold = cfg.swap.is_some_and(|m| i % m == m - 1) && held.is_none() && next < total;
                    if hold {
                        held = Some(i);
                    } else {
                        if !lost {
                            send_pkt(i, false, &packets);
                        }
                        if let Some(h) = held.take() {
                            send_pkt(h, false, &packets);
                        }
                    }
                }
                let Ok((n, _)) = sock.recv_from(&mut buf) else { continue };
                match parse(&buf[..n]) {
                    Some(Packet::Control { kind: C_NAK, cif, .. }) => {
                        saw.naks += 1;
                        let mut it = cif.chunks(4).map(|c| word(c, 0));
                        while let Some(w) = it.next() {
                            let (a, b) = if w & 0x8000_0000 != 0 { (w & SEQ_MASK, it.next().unwrap()) } else { (w, w) };
                            let mut s = a;
                            loop {
                                let idx = seq_off(s, first_seq);
                                if idx >= 0 && (idx as usize) < next {
                                    send_pkt(idx as usize, true, &packets);
                                    saw.resent += 1;
                                }
                                if s == b {
                                    break;
                                }
                                s = seq_add(s, 1);
                            }
                        }
                    }
                    Some(Packet::Control { kind: C_ACK, info, cif, .. }) => {
                        saw.acks += 1;
                        sock.send_to(&control(C_ACKACK, 0, info, ts_now(Instant::now()), peer_id, &[]), peer).unwrap();
                        acked_to = acked_to.max(seq_off(word(cif, 0), first_seq).max(0) as usize);
                    }
                    _ => {}
                }
            }
            sock.send_to(&control(C_SHUTDOWN, 0, 0, 0, peer_id, &[]), peer).unwrap();
            saw.shut = true;
            saw.acked = acked_to;
            saw
        })
    }

    fn drain_all(s: &mut dyn Source, limit: Duration) -> (Vec<u8>, Vec<usize>) {
        let (mut out, mut sizes) = (Vec::new(), Vec::new());
        let mut buf = vec![0u8; 64 << 10];
        let start = Instant::now();
        while start.elapsed() < limit {
            match s.read(&mut buf, Duration::from_millis(20)).unwrap() {
                Chunk::Data(n) => {
                    out.extend_from_slice(&buf[..n]);
                    sizes.push(n);
                }
                Chunk::Idle | Chunk::Restart => {}
                Chunk::End => {
                    break;
                }
            }
        }
        (out, sizes)
    }

    fn payloads(n: usize) -> Arc<Vec<Vec<u8>>> {
        let mut rng = Rng(77);
        Arc::new((0..n).map(|_| rng.bytes(1316)).collect())
    }

    fn free_port() -> u16 {
        UdpSocket::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    fn config(n: usize) -> SenderConfig {
        SenderConfig { payloads: payloads(n), lose: None, swap: None, passphrase: None, streamid: None, latency: 120, interval: Duration::ZERO }
    }

    #[cfg_attr(feature = "fips", ignore = "FIPS build: an unencrypted SRT source is refused (tests/fips.rs covers the refusal)")]
    #[test]
    fn as_a_caller_with_lost_and_reordered_packets_every_byte_arrives_in_order() {
        crate::net::allow_loopback_for_tests();
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = sock.local_addr().unwrap().port();
        let mut cfg = config(300);
        cfg.lose = Some((7, 3));
        cfg.swap = Some(11);
        cfg.streamid = Some("#!::r=live/cam1,m=request".into());
        let want: Vec<u8> = cfg.payloads.concat();
        let sender = run_sender(sock, Role::Listen, cfg);
        let mut s = open_source(&format!("srt://127.0.0.1:{port}?reconnect=0&latency=150&streamid=%23!::r=live/cam1,m=request")).unwrap();
        assert!(s.info().live && s.info().kind == "srt");
        let (got, sizes) = drain_all(s.as_mut(), Duration::from_secs(20));
        let saw = sender.join().unwrap();
        assert_eq!(got.len(), want.len(), "{saw:?}");
        assert!(got == want, "bytes differ");
        assert!(sizes.iter().all(|&n| n == 1316), "one payload per read");
        assert!(saw.naks > 0 && saw.resent >= 300 / 7, "{saw:?}");
        assert!(saw.acks > 10 && saw.sent_flags_ok && saw.acked == 300);
        assert_eq!(saw.streamid.as_deref(), Some("#!::r=live/cam1,m=request"));
    }

    #[cfg_attr(feature = "fips", ignore = "FIPS build: an unencrypted SRT source is refused (tests/fips.rs covers the refusal)")]
    #[test]
    fn as_a_listener_the_source_waits_for_the_sender_to_call() {
        crate::net::allow_loopback_for_tests();
        let port = free_port();
        let mut cfg = config(120);
        cfg.lose = Some((10, 0));
        let want: Vec<u8> = cfg.payloads.concat();
        let mut s = open_source(&format!("srt://:{port}?mode=listener&latency=100")).unwrap();
        // nothing has called yet
        let mut buf = vec![0u8; 65536];
        assert_eq!(s.read(&mut buf, Duration::from_millis(30)).unwrap(), Chunk::Idle);
        let sender = run_sender(UdpSocket::bind("127.0.0.1:0").unwrap(), Role::Call(format!("127.0.0.1:{port}").parse().unwrap()), cfg);
        let (got, _) = drain_all(s.as_mut(), Duration::from_secs(20));
        let saw = sender.join().unwrap();
        assert!(got == want, "{} of {} bytes ({saw:?})", got.len(), want.len());
        assert!(saw.naks > 0);
        // the sender's SHUTDOWN is the end of the source
        let mut buf = vec![0u8; 65536];
        assert_eq!(s.read(&mut buf, Duration::from_millis(500)).unwrap(), Chunk::End);
    }

    #[test]
    fn an_encrypted_stream_reads_with_the_passphrase_and_not_without_it() {
        crate::net::allow_loopback_for_tests();
        // the source calls
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = sock.local_addr().unwrap().port();
        let mut cfg = config(150);
        cfg.passphrase = Some("hunter2hunter2".into());
        cfg.lose = Some((13, 5));
        let want: Vec<u8> = cfg.payloads.concat();
        let sender = run_sender(sock, Role::Listen, cfg.clone());
        let mut s = open_source(&format!("srt://127.0.0.1:{port}?passphrase=hunter2hunter2&reconnect=0")).unwrap();
        // the passphrase is not in what the source shows
        assert!(!s.info().url.contains("hunter2"), "{}", s.info().url);
        let (got, _) = drain_all(s.as_mut(), Duration::from_secs(20));
        let saw = sender.join().unwrap();
        assert!(saw.encrypted && got == want, "{} of {} bytes ({saw:?})", got.len(), want.len());

        // the source listens and the sender calls
        let port = free_port();
        let mut s = open_source(&format!("srt://:{port}?mode=listener&passphrase=hunter2hunter2")).unwrap();
        let sender = run_sender(UdpSocket::bind("127.0.0.1:0").unwrap(), Role::Call(format!("127.0.0.1:{port}").parse().unwrap()), cfg);
        let (got, _) = drain_all(s.as_mut(), Duration::from_secs(20));
        sender.join().unwrap();
        assert!(got == want, "{} of {} bytes", got.len(), want.len());

        // another passphrase: the connection is refused rather than read as noise
        let port = free_port();
        let mut s = open_source(&format!("srt://:{port}?mode=listener&passphrase=a-different-one")).unwrap();
        let mut cfg2 = config(10);
        cfg2.passphrase = Some("hunter2hunter2".into());
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let target: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
        let ind = Handshake { version: 4, ext: 2, isn: 5, mtu: 1500, mfw: 8192, kind: HS_INDUCTION, sock: 9, ..Handshake::default() };
        sock.send_to(&hs_packet(&ind, 0, 0), target).unwrap();
        let mut buf = vec![0u8; 2048];
        let (n, _) = sock.recv_from(&mut buf).unwrap();
        let Some(Packet::Control { cif, .. }) = parse(&buf[..n]) else { panic!() };
        let h = Handshake::parse(cif).unwrap();
        let km = build_km(b"hunter2hunter2", &[4; 16], &[5; 16]).unwrap();
        let c = Handshake { version: 5, enc: 2, ext: 3, isn: 5, mtu: 1500, mfw: 8192, kind: HS_CONCLUSION, sock: 9, cookie: h.cookie, exts: vec![(E_HSREQ, hs_ext(100, true)), (E_KMREQ, km)] };
        sock.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
        // a caller says it again if it hears nothing
        let n = (0..10)
            .find_map(|_| {
                sock.send_to(&hs_packet(&c, 0, h.sock), target).unwrap();
                sock.recv_from(&mut buf).ok().map(|(n, _)| n)
            })
            .expect("the rejection");
        let Some(Packet::Control { cif, .. }) = parse(&buf[..n]) else { panic!() };
        assert_eq!(Handshake::parse(cif).unwrap().kind, REJ_BADSECRET);
        let mut b = vec![0u8; 65536];
        assert_eq!(s.read(&mut b, Duration::from_millis(50)).unwrap(), Chunk::Idle);
        drop(cfg2);
    }

    #[test]
    fn a_passphrase_with_an_unencrypted_sender_is_refused() {
        crate::net::allow_loopback_for_tests();
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = sock.local_addr().unwrap().port();
        // this sender answers a conclusion without key material
        let mut cfg = config(5);
        cfg.interval = Duration::from_millis(1);
        let sender = std::thread::spawn(move || {
            let mut buf = vec![0u8; 2048];
            sock.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            for _ in 0..2 {
                let (n, from) = sock.recv_from(&mut buf).unwrap();
                let Some(Packet::Control { cif, .. }) = parse(&buf[..n]) else { panic!() };
                let h = Handshake::parse(cif).unwrap();
                let r = if h.kind == HS_INDUCTION {
                    Handshake { version: 5, ext: MAGIC, isn: h.isn, mtu: 1500, mfw: 8192, kind: HS_INDUCTION, sock: 77, cookie: 5, ..Handshake::default() }
                } else {
                    Handshake { version: 5, ext: 1, isn: h.isn, mtu: 1500, mfw: 8192, kind: HS_CONCLUSION, sock: 77, cookie: 5, exts: vec![(E_HSRSP, hs_ext(120, false))], ..Handshake::default() }
                };
                sock.send_to(&hs_packet(&r, 0, h.sock), from).unwrap();
            }
        });
        let e = open_source(&format!("srt://127.0.0.1:{port}?passphrase=hunter2hunter2&reconnect=0")).err().expect("refused");
        sender.join().unwrap();
        assert!(e.message.contains("does not encrypt") && !e.message.contains("hunter2"), "{}", e.message);
    }

    #[cfg_attr(feature = "fips", ignore = "FIPS build: an unencrypted SRT source is refused (tests/fips.rs covers the refusal)")]
    #[test]
    fn a_caller_is_kept_up_and_calls_again_after_a_shutdown_but_a_listener_ends() {
        crate::net::allow_loopback_for_tests();
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = sock.local_addr().unwrap().port();
        let sender = run_sender(sock, Role::Listen, config(20));
        let mut s = open_source(&format!("srt://127.0.0.1:{port}")).unwrap();
        assert!(s.health().is_some());
        let mut buf = vec![0u8; 65536];
        let mut n = 0;
        let t = Instant::now();
        // the sender says goodbye after 20 packets; the caller notices, and a new call finds nobody
        while t.elapsed() < Duration::from_millis(1500) {
            match s.read(&mut buf, Duration::from_millis(20)).unwrap() {
                Chunk::Data(_) => n += 1,
                Chunk::Restart | Chunk::Idle => {}
                Chunk::End => panic!("a caller is kept up"),
            }
        }
        sender.join().unwrap();
        assert_eq!(n, 20);
        assert_eq!(s.health().map(|h| h.state), Some(crate::HealthState::Reconnecting));
        let l = open_source(&format!("srt://:{}?mode=listener", free_port())).unwrap();
        assert!(l.health().is_none());
    }

    #[cfg_attr(feature = "fips", ignore = "FIPS build: an unencrypted SRT source is refused (tests/fips.rs covers the refusal)")]
    #[test]
    fn a_caller_with_nobody_listening_fails_rather_than_waits() {
        crate::net::allow_loopback_for_tests();
        let port = free_port();
        let t = Instant::now();
        let e = open_source(&format!("srt://127.0.0.1:{port}?reconnect=0")).err().expect("no sender");
        assert_eq!(e.code, ErrorCode::Io);
        assert!(t.elapsed() < Duration::from_secs(6));
    }

    #[test]
    fn urls_are_validated_and_secrets_stay_out_of_what_is_shown() {
        crate::net::allow_loopback_for_tests();
        for bad in ["srt://127.0.0.1:9?mode=rendezvous", "srt://:9?mode=caller", "srt://127.0.0.1:9?latency=70000", "srt://127.0.0.1:9?latency=x", "srt://127.0.0.1:9?passphrase=short", "srt://127.0.0.1:9?streamid=", "srt://127.0.0.1"] {
            let e = open_source(bad).err().unwrap_or_else(|| panic!("{bad} opened"));
            assert_eq!(e.code, ErrorCode::InvalidArgument, "{bad}: {e:?}");
        }
        let port = free_port();
        let s = open_source(&format!("srt://:{port}?passphrase=0123456789&latency=40")).unwrap();
        assert!(!s.info().url.contains("0123456789") && s.info().url.contains("passphrase=***"), "{}", s.info().url);
    }

    #[test]
    fn the_address_allow_list_applies_to_a_caller() {
        // nothing is allowed: no environment is read, so ask the allow-list directly
        let e = net::resolve_udp("127.0.0.1", 9, &Allow::default(), "srt://127.0.0.1:9").unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
        assert!(e.hint.as_deref().is_some_and(|h| h.contains("IRONAV_NET_ALLOW")), "{e:?}");
        let ok = net::resolve_udp("127.0.0.1", 9, &Allow::parse("loopback").unwrap(), "x").unwrap();
        assert_eq!(ok, SocketAddr::from((Ipv4Addr::LOCALHOST, 9)));
    }

    /// With `srt-live-transmit` on the PATH: a real SRT sender, reading a file and sending it as a listener.
    #[ignore = "needs srt-live-transmit on the PATH"]
    #[test]
    fn interop_with_srt_live_transmit() {
        crate::net::allow_loopback_for_tests();
        let src = concat!(env!("CARGO_MANIFEST_DIR"), "/../ironav-formats/tests/data/h264_aac.ts");
        let port = free_port();
        let mut s = open_source(&format!("srt://:{port}?mode=listener")).unwrap();
        let mut child = std::process::Command::new("srt-live-transmit").args(["file://con", &format!("srt://127.0.0.1:{port}?mode=caller")]).stdin(std::fs::File::open(src).unwrap()).spawn().unwrap();
        let (got, _) = drain_all(s.as_mut(), Duration::from_secs(15));
        let _ = child.kill();
        let _ = child.wait();
        assert!(!got.is_empty() && got[0] == 0x47);
    }

    #[cfg_attr(feature = "fips", ignore = "FIPS build: an unencrypted SRT source is refused (tests/fips.rs covers the refusal)")]
    #[test]
    fn what_comes_out_is_what_the_transport_stream_demuxer_reads() {
        use ironav_formats::stream::{stream_demuxer, StreamOptions, SNIFF_BYTES};
        crate::net::allow_loopback_for_tests();
        let ts = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/../ironav-formats/tests/data/h264_aac.ts")).unwrap();
        let demux = |bytes: &[u8]| {
            let mut d = stream_demuxer(&bytes[..SNIFF_BYTES.min(bytes.len())], &StreamOptions::default()).unwrap();
            // fed as it arrives: in the pieces the source gave
            let mut n = 0usize;
            let mut sum = 0u64;
            for c in bytes.chunks(1316) {
                d.feed(c).unwrap();
                while let Some(p) = d.next_packet().unwrap() {
                    n += 1;
                    sum += p.packet.data.iter().map(|b| *b as u64).sum::<u64>() + p.packet.pts as u64;
                }
            }
            d.end();
            while let Some(p) = d.next_packet().unwrap() {
                n += 1;
                sum += p.packet.data.iter().map(|b| *b as u64).sum::<u64>() + p.packet.pts as u64;
            }
            (d.format_name(), n, sum)
        };
        let want = demux(&ts);
        assert!(want.1 > 50, "{want:?}");
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        let port = sock.local_addr().unwrap().port();
        let mut cfg = config(0);
        cfg.payloads = Arc::new(ts.chunks(1316).map(<[u8]>::to_vec).collect());
        cfg.lose = Some((6, 2));
        cfg.swap = Some(9);
        let sender = run_sender(sock, Role::Listen, cfg);
        let mut s = open_source(&format!("srt://127.0.0.1:{port}?reconnect=0")).unwrap();
        let (got, _) = drain_all(s.as_mut(), Duration::from_secs(20));
        sender.join().unwrap();
        assert_eq!(got.len(), ts.len());
        assert_eq!(demux(&got), want);
    }

    #[cfg_attr(feature = "fips", ignore = "FIPS build: an unencrypted SRT source is refused (tests/fips.rs covers the refusal)")]
    #[test]
    fn a_stale_listener_does_not_answer_a_conclusion_it_never_induced() {
        crate::net::allow_loopback_for_tests();
        let port = free_port();
        let _s = open_source(&format!("srt://:{port}?mode=listener")).unwrap();
        let sock = UdpSocket::bind("127.0.0.1:0").unwrap();
        sock.set_read_timeout(Some(Duration::from_millis(150))).unwrap();
        let c = Handshake { version: 5, enc: 0, ext: 1, isn: 5, mtu: 1500, mfw: 8192, kind: HS_CONCLUSION, sock: 9, cookie: 1234, exts: vec![(E_HSREQ, hs_ext(100, false))] };
        for dst in [0u32, 1, 12345] {
            sock.send_to(&hs_packet(&c, 0, dst), ("127.0.0.1", port)).unwrap();
        }
        sock.send_to(&[1, 2, 3], ("127.0.0.1", port)).unwrap();
        let mut buf = [0u8; 2048];
        assert!(sock.recv_from(&mut buf).is_err(), "answered a conclusion with a cookie it never gave");
    }
}
