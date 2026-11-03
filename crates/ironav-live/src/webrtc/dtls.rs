//! A DTLS 1.2 server (RFC 6347) for DTLS-SRTP (RFC 5764): just what a WebRTC publisher's handshake
//! needs, over IronCrypto's primitives as IronSocketLayer exposes them.
//!
//! - One cipher suite, `TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256` (RFC 5289), with ECDHE on P-256 and
//!   an ECDSA P-256 certificate made for the endpoint; the client must present a certificate too,
//!   whose SHA-256 fingerprint must be the one its SDP offer gave, and prove it holds the key
//!   (CertificateVerify). The extended master secret (RFC 7627) is used when the client offers it,
//!   and the renegotiation indication (RFC 5746) is answered; renegotiation itself is not done.
//! - `use_srtp` picks `SRTP_AEAD_AES_128_GCM` when offered, else `SRTP_AES128_CM_HMAC_SHA1_80` (not
//!   in FIPS mode, where only AES-GCM is taken), and the SRTP keys come from the exporter (RFC 5705,
//!   label `EXTRACTOR-dtls_srtp`).
//! - A stateless cookie (HelloVerifyRequest) comes first, so an address that cannot receive is never
//!   sent a full flight. Handshake messages are reassembled from fragments within fixed bounds,
//!   the server's flight is sent again with a doubling timer and whenever the client repeats its
//!   own, and the whole handshake must finish within [`HANDSHAKE_TIMEOUT`].
//!
//! Nothing is trusted: a record or message that does not parse is dropped, and one that parses but
//! breaks the protocol ends the handshake.

use super::srtp::Profile;

/// The SRTP protection profiles taken, most preferred first. FIPS mode leaves out the one keyed
/// with HMAC-SHA1, whose SHA-1 is IronAV's own rather than IronCrypto's.
fn profiles(fips: bool) -> &'static [Profile] {
    if fips {
        &[Profile::AeadAes128Gcm]
    } else {
        &[Profile::AeadAes128Gcm, Profile::Aes128CmHmacSha1_80]
    }
}
use ic_cipher::Aes128Gcm;
use ic_core::traits::Aead;
use ironsocketlayer::crypto::kx::KeyShare;
use ironsocketlayer::crypto::sign::{self, KeyKind, PublicKey, SigningKey};
use ironsocketlayer::crypto::{hmac, HashAlg};
use ironsocketlayer::enums::{NamedGroup, SignatureScheme};
use ironsocketlayer::x509;
use std::collections::BTreeMap;
use std::time::{Duration, Instant};

pub(crate) const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);
/// The first wait before the server's flight is sent again; it doubles up to [`MAX_BACKOFF`].
const FIRST_BACKOFF: Duration = Duration::from_millis(500);
const MAX_BACKOFF: Duration = Duration::from_secs(4);
/// The client's repeats answered at most this often.
const ANSWER_EVERY: Duration = Duration::from_millis(100);
/// Datagrams sent stay below this, which fits any path's MTU after IP and UDP headers.
const MTU: usize = 1200;
/// Handshake message bodies are cut into fragments of at most this many bytes.
const MAX_FRAGMENT: usize = 1000;
/// The largest handshake message accepted, the most held for later, and the transcript's limit.
const MAX_MESSAGE: usize = 16 << 10;
const MAX_AHEAD: u16 = 4;
const MAX_TRANSCRIPT: usize = 64 << 10;
const MAX_RECORD: usize = (1 << 14) + 2048;

const CT_CCS: u8 = 20;
const CT_ALERT: u8 = 21;
const CT_HANDSHAKE: u8 = 22;
const HS_CLIENT_HELLO: u8 = 1;
const HS_SERVER_HELLO: u8 = 2;
const HS_HELLO_VERIFY: u8 = 3;
const HS_CERTIFICATE: u8 = 11;
const HS_SERVER_KX: u8 = 12;
const HS_CERT_REQUEST: u8 = 13;
const HS_SERVER_DONE: u8 = 14;
const HS_CERT_VERIFY: u8 = 15;
const HS_CLIENT_KX: u8 = 16;
const HS_FINISHED: u8 = 20;
const DTLS_1_0: u16 = 0xFEFF;
const DTLS_1_2: u16 = 0xFEFD;
const SUITE: u16 = 0xC02B;
const SCSV_RENEGOTIATION: u16 = 0x00FF;
const EXT_GROUPS: u16 = 10;
const EXT_POINT_FORMATS: u16 = 11;
const EXT_SIG_ALGS: u16 = 13;
const EXT_USE_SRTP: u16 = 14;
const EXT_EMS: u16 = 23;
const EXT_RENEGOTIATION: u16 = 0xFF01;
const GROUP_P256: u16 = 23;
const ECDSA_P256_SHA256: u16 = 0x0403;
const ALERT_CLOSE_NOTIFY: u8 = 0;
const ALERT_HANDSHAKE_FAILURE: u8 = 40;

/// The endpoint's certificate and key, made when the endpoint opens and never written anywhere.
pub(crate) struct Identity {
    key: SigningKey,
    pub cert: Vec<u8>,
    pub fingerprint: [u8; 32],
}

impl Identity {
    pub(crate) fn generate(name: &str) -> Result<Identity, String> {
        let mut rng = ic_drbg::Rng::from_os().map_err(|e| e.to_string())?;
        let key = SigningKey::generate(KeyKind::EcdsaP256, &mut rng).map_err(|e| e.to_string())?;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let mut serial: [u8; 16] = rng.random_array().map_err(|e| e.to_string())?;
        serial[0] &= 0x7F;
        let p = x509::CertificateParams::new(name, now.saturating_sub(86_400), now + 30 * 86_400, serial);
        let cert = x509::self_signed(&p, &key, &mut rng).map_err(|e| e.to_string())?;
        let fingerprint = sha256(&cert);
        Ok(Identity { key, cert, fingerprint })
    }

    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, String> {
        let mut rng = ic_drbg::Rng::from_os().map_err(|e| e.to_string())?;
        self.key.sign(SignatureScheme::EcdsaSecp256r1Sha256, message, &mut rng).map_err(|e| e.to_string())
    }
}

pub(crate) fn sha256(data: &[u8]) -> [u8; 32] {
    let mut out = [0u8; 32];
    if let Ok(d) = HashAlg::Sha256.digest(data) {
        out.copy_from_slice(d.as_bytes());
    }
    out
}

/// A fingerprint as SDP writes it: `AB:CD:...`.
pub(crate) fn fingerprint_text(fp: &[u8]) -> String {
    fp.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
}

/// TLS 1.2's PRF with SHA-256 (RFC 5246 §5).
pub(crate) fn prf(secret: &[u8], label: &[u8], seed: &[u8], out: &mut [u8]) -> Result<(), String> {
    let mut a = hmac(HashAlg::Sha256, secret, &[label, seed]).map_err(|e| e.to_string())?;
    let mut at = 0;
    while at < out.len() {
        let block = hmac(HashAlg::Sha256, secret, &[a.as_bytes(), label, seed]).map_err(|e| e.to_string())?;
        let n = (out.len() - at).min(32);
        out[at..at + n].copy_from_slice(&block.as_bytes()[..n]);
        at += n;
        a = hmac(HashAlg::Sha256, secret, &[a.as_bytes()]).map_err(|e| e.to_string())?;
    }
    Ok(())
}

/// The keys DTLS-SRTP exports: each side's master key and salt.
#[derive(Clone, PartialEq)]
pub(crate) struct SrtpKeys {
    pub profile: Profile,
    pub client_key: Vec<u8>,
    pub client_salt: Vec<u8>,
    pub server_key: Vec<u8>,
    pub server_salt: Vec<u8>,
}

impl std::fmt::Debug for SrtpKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "SrtpKeys({})", self.profile.name())
    }
}

fn export_srtp(master: &[u8; 48], client_random: &[u8; 32], server_random: &[u8; 32], profile: Profile) -> Result<SrtpKeys, String> {
    let (k, s) = (profile.key_len(), profile.salt_len());
    let mut m = vec![0u8; 2 * (k + s)];
    prf(master, b"EXTRACTOR-dtls_srtp", &[&client_random[..], &server_random[..]].concat(), &mut m)?;
    Ok(SrtpKeys { profile, client_key: m[..k].to_vec(), server_key: m[k..2 * k].to_vec(), client_salt: m[2 * k..2 * k + s].to_vec(), server_salt: m[2 * k + s..].to_vec() })
}

/// The master secret, and the client's and server's write keys and implicit nonces.
fn key_schedule(pms: &[u8], ems_hash: Option<&[u8; 32]>, client_random: &[u8; 32], server_random: &[u8; 32]) -> Result<([u8; 48], Cipher, Cipher), String> {
    let mut master = [0u8; 48];
    match ems_hash {
        Some(h) => prf(pms, b"extended master secret", h, &mut master)?,
        None => prf(pms, b"master secret", &[&client_random[..], &server_random[..]].concat(), &mut master)?,
    }
    let mut block = [0u8; 40];
    prf(&master, b"key expansion", &[&server_random[..], &client_random[..]].concat(), &mut block)?;
    let client = Cipher::new(&block[..16], &block[32..36])?;
    let server = Cipher::new(&block[16..32], &block[36..40])?;
    Ok((master, client, server))
}

/// AES-128-GCM on DTLS records (RFC 5288): a 4-byte implicit nonce and the 8-byte explicit one,
/// which here is the record's epoch and sequence number.
struct Cipher {
    aead: Aes128Gcm,
    iv: [u8; 4],
}

impl Cipher {
    fn new(key: &[u8], iv: &[u8]) -> Result<Cipher, String> {
        Ok(Cipher { aead: Aes128Gcm::new(key).map_err(|e| e.to_string())?, iv: iv.try_into().map_err(|_| "iv length")? })
    }

    fn aad(seq: u64, ct: u8, len: usize) -> [u8; 13] {
        let mut a = [0u8; 13];
        a[..8].copy_from_slice(&seq.to_be_bytes());
        a[8] = ct;
        a[9..11].copy_from_slice(&DTLS_1_2.to_be_bytes());
        a[11..13].copy_from_slice(&(len as u16).to_be_bytes());
        a
    }

    fn seal(&self, seq: u64, ct: u8, plain: &[u8]) -> Option<Vec<u8>> {
        let mut nonce = [0u8; 12];
        nonce[..4].copy_from_slice(&self.iv);
        nonce[4..].copy_from_slice(&seq.to_be_bytes());
        let mut body = plain.to_vec();
        let mut tag = [0u8; 16];
        self.aead.seal_detached(&nonce, &Self::aad(seq, ct, plain.len()), &mut body, &mut tag).ok()?;
        let mut out = seq.to_be_bytes().to_vec();
        out.extend_from_slice(&body);
        out.extend_from_slice(&tag);
        Some(out)
    }

    fn open(&self, seq: u64, ct: u8, body: &[u8]) -> Option<Vec<u8>> {
        if body.len() < 8 + 16 {
            return None;
        }
        let mut nonce = [0u8; 12];
        nonce[..4].copy_from_slice(&self.iv);
        nonce[4..].copy_from_slice(&body[..8]);
        let mut plain = body[8..body.len() - 16].to_vec();
        self.aead.open_detached(&nonce, &Self::aad(seq, ct, plain.len()), &mut plain, &body[body.len() - 16..]).ok()?;
        Some(plain)
    }
}

/// A record as it came: content type, epoch and sequence number (together, as the 64-bit
/// `seq_num` AEAD uses), and fragment.
struct Record<'a> {
    ct: u8,
    seq: u64,
    body: &'a [u8],
}

/// A DER element at the start of `b`: its tag, its whole encoding and its contents.
fn der(b: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = b.split_first()?;
    let (&first, rest) = rest.split_first()?;
    let (len, head) = match first {
        n @ 0..=0x7f => (n as usize, 2),
        0x81..=0x84 => {
            let k = (first & 0x7f) as usize;
            let len = rest.get(..k)?.iter().fold(0usize, |a, &d| a << 8 | d as usize);
            (len, 2 + k)
        }
        _ => return None,
    };
    let end = head.checked_add(len)?;
    Some((tag, b.get(..end)?, b.get(head..end)?))
}

/// The SubjectPublicKeyInfo of a DER certificate: the seventh element of its TBSCertificate, or
/// the sixth when the version is left out.
fn spki_of(cert: &[u8]) -> Option<&[u8]> {
    let (0x30, _, body) = der(cert)? else { return None };
    let (0x30, _, mut tbs) = der(body)? else { return None };
    let mut fields = Vec::with_capacity(7);
    while !tbs.is_empty() && fields.len() < 7 {
        let (tag, whole, _) = der(tbs)?;
        fields.push((tag, whole));
        tbs = &tbs[whole.len()..];
    }
    let skip = usize::from(fields.first()?.0 == 0xa0);
    match fields.get(skip + 5) {
        Some(&(0x30, spki)) => Some(spki),
        _ => None,
    }
}

/// The records of a datagram, up to the first that does not parse.
fn records(d: &[u8]) -> Vec<Record<'_>> {
    let mut out = Vec::new();
    let mut at = 0;
    while let Some(h) = d.get(at..at + 13) {
        let len = u16::from_be_bytes([h[11], h[12]]) as usize;
        let version = u16::from_be_bytes([h[1], h[2]]);
        let Some(body) = d.get(at + 13..at + 13 + len) else { break };
        if !(20..=25).contains(&h[0]) || h[1] != 0xFE || version < DTLS_1_2 || len > MAX_RECORD {
            break;
        }
        let mut s = [0u8; 8];
        s.copy_from_slice(&h[3..11]);
        out.push(Record { ct: h[0], seq: u64::from_be_bytes(s), body });
        at += 13 + len;
    }
    out
}

/// Sequence numbers, ciphers and the epoch-1 replay window of one side's records.
#[derive(Default)]
struct RecordLayer {
    next: [u64; 2],
    read: Option<Cipher>,
    write: Option<Cipher>,
    highest: Option<u64>,
    seen: u64,
}

impl RecordLayer {
    /// The record carrying `payload`, in epoch 0 in the clear, or in epoch 1 under the write key.
    fn record(&mut self, ct: u8, epoch: u16, payload: &[u8], version: u16) -> Option<Vec<u8>> {
        let e = epoch.min(1) as usize;
        let seq = ((epoch as u64) << 48) | self.next[e];
        self.next[e] += 1;
        let body = if epoch == 0 { payload.to_vec() } else { self.write.as_ref()?.seal(seq, ct, payload)? };
        // the 64-bit sequence number is the epoch and the 48-bit count, as the header has them
        let mut out = vec![ct];
        out.extend_from_slice(&version.to_be_bytes());
        out.extend_from_slice(&seq.to_be_bytes());
        out.extend_from_slice(&(body.len() as u16).to_be_bytes());
        out.extend_from_slice(&body);
        Some(out)
    }

    /// `(content type, epoch, payload)`s packed into datagrams of at most [`MTU`] bytes.
    fn pack(&mut self, flight: &[(u8, u16, Vec<u8>)], version: u16) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut cur: Vec<u8> = Vec::new();
        for (ct, epoch, payload) in flight {
            let Some(r) = self.record(*ct, *epoch, payload, version) else { continue };
            if !cur.is_empty() && cur.len() + r.len() > MTU {
                out.push(std::mem::take(&mut cur));
            }
            cur.extend_from_slice(&r);
        }
        if !cur.is_empty() {
            out.push(cur);
        }
        out
    }

    /// A record's payload and epoch: in the clear in epoch 0, and in epoch 1 once it decrypts and
    /// is not a replay. Records of other epochs are dropped.
    fn open(&mut self, r: &Record) -> Option<(u16, Vec<u8>)> {
        match r.seq >> 48 {
            0 => Some((0, r.body.to_vec())),
            1 => {
                let n = r.seq & 0xFFFF_FFFF_FFFF;
                if let Some(h) = self.highest {
                    if n <= h && (h - n >= 64 || self.seen & (1 << (h - n)) != 0) {
                        return None;
                    }
                }
                let plain = self.read.as_ref()?.open(r.seq, r.ct, r.body)?;
                match self.highest {
                    Some(h) if n <= h => self.seen |= 1 << (h - n),
                    Some(h) => {
                        let shift = n - h;
                        self.seen = if shift < 64 { (self.seen << shift) | 1 } else { 1 };
                        self.highest = Some(n);
                    }
                    None => (self.highest, self.seen) = (Some(n), 1),
                }
                Some((1, plain))
            }
            _ => None,
        }
    }
}

/// A handshake message with its 12-byte header, unfragmented, as the transcript has it.
fn message(typ: u8, seq: u16, body: &[u8]) -> Vec<u8> {
    let mut m = vec![typ];
    m.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
    m.extend_from_slice(&seq.to_be_bytes());
    m.extend_from_slice(&[0, 0, 0]);
    m.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
    m.extend_from_slice(body);
    m
}

/// A handshake message cut into fragments of at most [`MAX_FRAGMENT`] body bytes.
fn fragments(typ: u8, seq: u16, body: &[u8]) -> Vec<Vec<u8>> {
    if body.is_empty() {
        return vec![message(typ, seq, body)];
    }
    body.chunks(MAX_FRAGMENT)
        .enumerate()
        .map(|(k, c)| {
            let mut m = vec![typ];
            m.extend_from_slice(&(body.len() as u32).to_be_bytes()[1..]);
            m.extend_from_slice(&seq.to_be_bytes());
            m.extend_from_slice(&((k * MAX_FRAGMENT) as u32).to_be_bytes()[1..]);
            m.extend_from_slice(&(c.len() as u32).to_be_bytes()[1..]);
            m.extend_from_slice(c);
            m
        })
        .collect()
}

/// A big-endian reader that fails rather than panics.
struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        if self.0.len() < n {
            return None;
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Some(a)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        let b = self.take(2)?;
        Some(u16::from_be_bytes([b[0], b[1]]))
    }
    fn u24(&mut self) -> Option<usize> {
        let b = self.take(3)?;
        Some(((b[0] as usize) << 16) | ((b[1] as usize) << 8) | b[2] as usize)
    }
    fn vec8(&mut self) -> Option<&'a [u8]> {
        let n = self.u8()? as usize;
        self.take(n)
    }
    fn vec16(&mut self) -> Option<&'a [u8]> {
        let n = self.u16()? as usize;
        self.take(n)
    }
    fn vec24(&mut self) -> Option<&'a [u8]> {
        let n = self.u24()?;
        self.take(n)
    }
}

fn u16s(b: &[u8]) -> impl Iterator<Item = u16> + '_ {
    b.as_chunks::<2>().0.iter().map(|c| u16::from_be_bytes(*c))
}

/// What the ClientHello asked for.
struct Hello<'a> {
    random: [u8; 32],
    session_id: &'a [u8],
    cookie: &'a [u8],
    suites: &'a [u8],
    profiles: Vec<u16>,
    ems: bool,
    renegotiation: bool,
    point_formats: bool,
    groups: Option<Vec<u16>>,
    schemes: Option<Vec<u16>>,
    version: u16,
    compression: &'a [u8],
}

fn parse_hello(body: &[u8]) -> Option<Hello<'_>> {
    let mut r = Reader(body);
    let version = r.u16()?;
    let random: [u8; 32] = r.take(32)?.try_into().ok()?;
    let session_id = r.vec8()?;
    let cookie = r.vec8()?;
    let suites = r.vec16()?;
    let compression = r.vec8()?;
    let mut h = Hello { random, session_id, cookie, suites, profiles: Vec::new(), ems: false, renegotiation: false, point_formats: false, groups: None, schemes: None, version, compression };
    if session_id.len() > 32 || suites.len() % 2 != 0 {
        return None;
    }
    if r.0.is_empty() {
        return Some(h);
    }
    let mut ext = Reader(r.vec16()?);
    let mut count = 0;
    while !ext.0.is_empty() {
        count += 1;
        if count > 64 {
            return None;
        }
        let t = ext.u16()?;
        let mut v = Reader(ext.vec16()?);
        match t {
            EXT_USE_SRTP => {
                let list = v.vec16()?;
                if list.len() % 2 != 0 {
                    return None;
                }
                h.profiles = u16s(list).collect();
            }
            EXT_EMS => h.ems = true,
            EXT_RENEGOTIATION => h.renegotiation = true,
            EXT_POINT_FORMATS => h.point_formats = true,
            EXT_GROUPS => h.groups = Some(u16s(v.vec16()?).collect()),
            EXT_SIG_ALGS => h.schemes = Some(u16s(v.vec16()?).collect()),
            _ => {}
        }
    }
    Some(h)
}

/// A handshake message being put together from fragments.
struct Partial {
    typ: u8,
    epoch: u16,
    body: Vec<u8>,
    have: Vec<bool>,
    missing: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    Hello,
    Certificate,
    KeyExchange,
    Verify,
    Finished,
    Done,
}

/// A DTLS server for one client.
pub(crate) struct Server {
    id: std::sync::Arc<Identity>,
    peer_fingerprint: [u8; 32],
    cookie_key: [u8; 32],
    state: State,
    recv_seq: u16,
    send_seq: u16,
    pending: BTreeMap<u16, Partial>,
    transcript: Vec<u8>,
    client_random: [u8; 32],
    server_random: [u8; 32],
    ems: bool,
    profile: Option<Profile>,
    share: Option<KeyShare>,
    master: Option<[u8; 48]>,
    peer_key: Vec<u8>,
    layer: RecordLayer,
    flight: Vec<(u8, u16, Vec<u8>)>,
    resend_at: Option<Instant>,
    backoff: Duration,
    answered: Option<Instant>,
    started: Option<Instant>,
    keys: Option<SrtpKeys>,
    closed: bool,
}

impl Server {
    /// A server that will take only a client whose certificate has the SHA-256 `peer_fingerprint`.
    pub(crate) fn new(id: std::sync::Arc<Identity>, peer_fingerprint: [u8; 32]) -> Result<Server, String> {
        let mut rng = ic_drbg::Rng::from_os().map_err(|e| e.to_string())?;
        Ok(Server {
            id,
            peer_fingerprint,
            cookie_key: rng.random_array().map_err(|e| e.to_string())?,
            state: State::Hello,
            recv_seq: 0,
            send_seq: 0,
            pending: BTreeMap::new(),
            transcript: Vec::new(),
            client_random: [0; 32],
            server_random: rng.random_array().map_err(|e| e.to_string())?,
            ems: false,
            profile: None,
            share: None,
            master: None,
            peer_key: Vec::new(),
            layer: RecordLayer::default(),
            flight: Vec::new(),
            resend_at: None,
            backoff: FIRST_BACKOFF,
            answered: None,
            started: None,
            keys: None,
            closed: false,
        })
    }

    /// The SRTP keys, once the handshake is done.
    pub(crate) fn keys(&self) -> Option<&SrtpKeys> {
        self.keys.as_ref()
    }

    /// Whether the client has ended the association (close_notify, or a fatal alert).
    pub(crate) fn closed(&self) -> bool {
        self.closed
    }

    /// A close_notify to send when the endpoint ends the session.
    pub(crate) fn close_notify(&mut self) -> Vec<Vec<u8>> {
        let epoch = if self.layer.write.is_some() && self.state == State::Done { 1 } else { 0 };
        self.layer.pack(&[(CT_ALERT, epoch, vec![1, ALERT_CLOSE_NOTIFY])], DTLS_1_2)
    }

    /// Takes a datagram from the client at `peer` (its address, for the cookie), returning the
    /// datagrams to send back. An error ends the handshake; the datagrams that come with it (an
    /// alert) are best sent anyway.
    pub(crate) fn handle(&mut self, datagram: &[u8], peer: &[u8], now: Instant) -> Result<Vec<Vec<u8>>, (String, Vec<Vec<u8>>)> {
        let mut out = Vec::new();
        for r in records(datagram) {
            let Some((epoch, payload)) = self.layer.open(&r) else { continue };
            match r.ct {
                CT_HANDSHAKE => {
                    let mut at = 0;
                    while let Some(h) = payload.get(at..at + 12) {
                        let flen = ((h[9] as usize) << 16) | ((h[10] as usize) << 8) | h[11] as usize;
                        let Some(frag) = payload.get(at..at + 12 + flen) else { break };
                        at += 12 + flen;
                        if let Err(e) = self.fragment(epoch, frag, peer, now, &mut out) {
                            let alert = self.layer.pack(&[(CT_ALERT, if self.state == State::Done { 1 } else { 0 }, vec![2, ALERT_HANDSHAKE_FAILURE])], DTLS_1_2);
                            return Err((e, alert));
                        }
                    }
                }
                // a fatal alert, or close_notify
                CT_ALERT if payload.len() >= 2 && (payload[0] == 2 || payload[1] == ALERT_CLOSE_NOTIFY) => self.closed = true,
                _ => {} // ChangeCipherSpec says nothing the epoch does not; application data is not used
            }
        }
        Ok(out)
    }

    /// Resends the flight when its timer is up; fails once the handshake has taken too long.
    pub(crate) fn poll(&mut self, now: Instant) -> Result<Vec<Vec<u8>>, String> {
        if self.state != State::Done && self.started.is_some_and(|s| now.duration_since(s) > HANDSHAKE_TIMEOUT) {
            return Err(format!("the DTLS handshake did not finish within {} s", HANDSHAKE_TIMEOUT.as_secs()));
        }
        match self.resend_at {
            Some(t) if now >= t && self.state != State::Done => {
                self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
                self.resend_at = Some(now + self.backoff);
                Ok(self.layer.pack(&self.flight, DTLS_1_2))
            }
            _ => Ok(Vec::new()),
        }
    }

    fn fragment(&mut self, epoch: u16, frag: &[u8], peer: &[u8], now: Instant, out: &mut Vec<Vec<u8>>) -> Result<(), String> {
        let mut r = Reader(frag);
        let (Some(typ), Some(len), Some(seq), Some(off), Some(flen)) = (r.u8(), r.u24(), r.u16(), r.u24(), r.u24()) else { return Ok(()) };
        let data = r.0;
        if len > MAX_MESSAGE || off + flen > len || data.len() != flen {
            return Ok(());
        }
        if seq < self.recv_seq {
            // the client sent its flight again: ours was lost
            if self.answered.is_none_or(|t| now.duration_since(t) >= ANSWER_EVERY) && !self.flight.is_empty() {
                self.answered = Some(now);
                out.extend(self.layer.pack(&self.flight, DTLS_1_2));
            }
            return Ok(());
        }
        if seq - self.recv_seq > MAX_AHEAD {
            return Ok(());
        }
        let p = self.pending.entry(seq).or_insert_with(|| Partial { typ, epoch, body: vec![0; len], have: vec![false; len], missing: len });
        if p.typ != typ || p.body.len() != len {
            return Ok(());
        }
        for (k, b) in data.iter().enumerate() {
            if !p.have[off + k] {
                p.have[off + k] = true;
                p.body[off + k] = *b;
                p.missing -= 1;
            }
        }
        p.epoch = p.epoch.max(epoch);
        while self.pending.get(&self.recv_seq).is_some_and(|p| p.missing == 0) {
            let p = self.pending.remove(&self.recv_seq).expect("present");
            let seq = self.recv_seq;
            self.recv_seq = self.recv_seq.wrapping_add(1);
            self.on_message(p.typ, p.epoch, seq, &p.body, peer, now, out)?;
        }
        Ok(())
    }

    fn add_transcript(&mut self, m: &[u8]) -> Result<(), String> {
        if self.transcript.len() + m.len() > MAX_TRANSCRIPT {
            return Err("the DTLS handshake is too long".into());
        }
        self.transcript.extend_from_slice(m);
        Ok(())
    }

    fn cookie(&self, peer: &[u8], h: &Hello) -> Result<[u8; 20], String> {
        let mac = hmac(HashAlg::Sha256, &self.cookie_key, &[peer, &h.random, h.session_id, h.suites]).map_err(|e| e.to_string())?;
        let mut c = [0u8; 20];
        c.copy_from_slice(&mac.as_bytes()[..20]);
        Ok(c)
    }

    fn send_flight(&mut self, flight: Vec<(u8, u16, Vec<u8>)>, timer: Option<Instant>, out: &mut Vec<Vec<u8>>) {
        self.flight = flight;
        self.backoff = FIRST_BACKOFF;
        self.resend_at = timer.map(|now| now + FIRST_BACKOFF);
        out.extend(self.layer.pack(&self.flight, DTLS_1_2));
    }

    fn on_message(&mut self, typ: u8, epoch: u16, seq: u16, body: &[u8], peer: &[u8], now: Instant, out: &mut Vec<Vec<u8>>) -> Result<(), String> {
        let raw = message(typ, seq, body);
        // only Finished comes under the new keys, and nothing else may
        if (typ == HS_FINISHED) != (epoch == 1) {
            return Err("a DTLS handshake message came in the wrong epoch".into());
        }
        match (self.state, typ) {
            (State::Hello, HS_CLIENT_HELLO) => self.client_hello(seq, body, &raw, peer, now, out),
            (State::Certificate, HS_CERTIFICATE) => {
                let mut r = Reader(body);
                let mut list = Reader(r.vec24().ok_or("malformed Certificate")?);
                let first = list.vec24().ok_or("the client sent no certificate")?;
                if !ic_core::ct::verify(&sha256(first), &self.peer_fingerprint) {
                    return Err("the client's certificate is not the one its offer's fingerprint names".into());
                }
                // its bytes are vouched for by the fingerprint; only its key is wanted, so it is not held
                // to RFC 5280 (OBS's, from libdatachannel, has an empty extensions list)
                let spki = spki_of(first).ok_or("the client's certificate does not parse")?.to_vec();
                let key = PublicKey::from_spki(&spki).map_err(|e| format!("the client's key: {e}"))?;
                if !key.matches(SignatureScheme::EcdsaSecp256r1Sha256) {
                    return Err("the client's certificate key is not ECDSA P-256".into());
                }
                self.peer_key = spki;
                self.add_transcript(&raw)?;
                self.state = State::KeyExchange;
                Ok(())
            }
            (State::KeyExchange, HS_CLIENT_KX) => {
                let point = Reader(body).vec8().ok_or("malformed ClientKeyExchange")?;
                let share = self.share.take().ok_or("no key share")?;
                let pms = share.complete(point).map_err(|e| format!("the client's key share: {e}"))?;
                self.add_transcript(&raw)?;
                let ems = self.ems.then(|| sha256(&self.transcript));
                let (master, client, server) = key_schedule(pms.get(), ems.as_ref(), &self.client_random, &self.server_random)?;
                self.master = Some(master);
                self.layer.read = Some(client);
                self.layer.write = Some(server);
                self.state = State::Verify;
                Ok(())
            }
            (State::Verify, HS_CERT_VERIFY) => {
                let mut r = Reader(body);
                let scheme = r.u16().ok_or("malformed CertificateVerify")?;
                let sig = r.vec16().ok_or("malformed CertificateVerify")?;
                if scheme != ECDSA_P256_SHA256 {
                    return Err("the client signed with a scheme other than ECDSA P-256 SHA-256".into());
                }
                let key = PublicKey::from_spki(&self.peer_key).map_err(|e| e.to_string())?;
                sign::verify(SignatureScheme::EcdsaSecp256r1Sha256, &key, &self.transcript, sig).map_err(|_| "the client's CertificateVerify signature does not check out")?;
                self.add_transcript(&raw)?;
                self.state = State::Finished;
                Ok(())
            }
            (State::Finished, HS_FINISHED) => {
                let master = self.master.ok_or("no master secret")?;
                let mut want = [0u8; 12];
                prf(&master, b"client finished", &sha256(&self.transcript), &mut want)?;
                if !ic_core::ct::verify(&want, body) {
                    return Err("the client's Finished does not check out".into());
                }
                self.add_transcript(&raw)?;
                let mut verify = [0u8; 12];
                prf(&master, b"server finished", &sha256(&self.transcript), &mut verify)?;
                let fin = message(HS_FINISHED, self.send_seq, &verify);
                self.send_seq += 1;
                self.send_flight(vec![(CT_CCS, 0, vec![1]), (CT_HANDSHAKE, 1, fin)], None, out);
                self.keys = Some(export_srtp(&master, &self.client_random, &self.server_random, self.profile.ok_or("no SRTP profile")?)?);
                self.state = State::Done;
                Ok(())
            }
            (State::Done, _) => Ok(()),
            _ => Err(format!("unexpected DTLS handshake message {typ}")),
        }
    }

    fn client_hello(&mut self, seq: u16, body: &[u8], raw: &[u8], peer: &[u8], now: Instant, out: &mut Vec<Vec<u8>>) -> Result<(), String> {
        let h = parse_hello(body).ok_or("malformed ClientHello")?;
        let cookie = self.cookie(peer, &h)?;
        if !ic_core::ct::verify(&cookie, h.cookie) {
            // stateless: nothing is kept but the sequence numbers
            let mut v = DTLS_1_0.to_be_bytes().to_vec();
            v.push(cookie.len() as u8);
            v.extend_from_slice(&cookie);
            self.send_seq = seq.wrapping_add(1);
            self.flight = vec![(CT_HANDSHAKE, 0, message(HS_HELLO_VERIFY, seq, &v))];
            out.extend(self.layer.pack(&self.flight, DTLS_1_0));
            return Ok(());
        }
        self.started.get_or_insert(now);
        if h.version > DTLS_1_2 {
            return Err("the client does not offer DTLS 1.2".into());
        }
        if !u16s(h.suites).any(|s| s == SUITE) {
            return Err("the client does not offer TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256".into());
        }
        if !h.compression.contains(&0) {
            return Err("the client offers no null compression".into());
        }
        if h.groups.as_ref().is_some_and(|g| !g.contains(&GROUP_P256)) {
            return Err("the client does not offer the P-256 group".into());
        }
        if h.schemes.as_ref().is_some_and(|s| !s.contains(&ECDSA_P256_SHA256)) {
            return Err("the client does not accept ECDSA P-256 SHA-256 signatures".into());
        }
        let fips = crate::fips::enabled();
        let profile = profiles(fips).iter().copied().find(|p| h.profiles.contains(&p.id())).ok_or(if fips {
            "the client does not offer the SRTP protection profile FIPS mode allows (AEAD_AES_128_GCM)"
        } else {
            "the client offers no SRTP protection profile in common (AEAD_AES_128_GCM or AES128_CM_HMAC_SHA1_80)"
        })?;
        self.profile = Some(profile);
        self.client_random = h.random;
        self.ems = h.ems;
        self.send_seq = seq;
        self.add_transcript(raw)?;

        // ServerHello
        let mut sh = DTLS_1_2.to_be_bytes().to_vec();
        sh.extend_from_slice(&self.server_random);
        sh.push(0); // no session id: no resumption
        sh.extend_from_slice(&SUITE.to_be_bytes());
        sh.push(0);
        let mut ext = Vec::new();
        let mut push_ext = |t: u16, v: &[u8]| {
            ext.extend_from_slice(&t.to_be_bytes());
            ext.extend_from_slice(&(v.len() as u16).to_be_bytes());
            ext.extend_from_slice(v);
        };
        let mut srtp = vec![0, 2];
        srtp.extend_from_slice(&profile.id().to_be_bytes());
        srtp.push(0);
        push_ext(EXT_USE_SRTP, &srtp);
        if h.ems {
            push_ext(EXT_EMS, &[]);
        }
        if h.renegotiation || u16s(h.suites).any(|s| s == SCSV_RENEGOTIATION) {
            push_ext(EXT_RENEGOTIATION, &[0]);
        }
        if h.point_formats {
            push_ext(EXT_POINT_FORMATS, &[1, 0]);
        }
        sh.extend_from_slice(&(ext.len() as u16).to_be_bytes());
        sh.extend_from_slice(&ext);

        // Certificate
        let cert = &self.id.cert;
        let mut c = ((cert.len() + 3) as u32).to_be_bytes()[1..].to_vec();
        c.extend_from_slice(&(cert.len() as u32).to_be_bytes()[1..]);
        c.extend_from_slice(cert);

        // ServerKeyExchange: an ephemeral P-256 share, signed with the certificate's key
        let mut rng = ic_drbg::Rng::from_os().map_err(|e| e.to_string())?;
        let share = KeyShare::generate(NamedGroup::Secp256r1, &mut rng).map_err(|e| e.to_string())?;
        let mut params = vec![3];
        params.extend_from_slice(&GROUP_P256.to_be_bytes());
        params.push(share.public().len() as u8);
        params.extend_from_slice(share.public());
        let sig = self.id.sign(&[&self.client_random[..], &self.server_random[..], &params].concat())?;
        let mut ske = params;
        ske.extend_from_slice(&ECDSA_P256_SHA256.to_be_bytes());
        ske.extend_from_slice(&(sig.len() as u16).to_be_bytes());
        ske.extend_from_slice(&sig);
        self.share = Some(share);

        // CertificateRequest: an ECDSA certificate, signing with P-256 SHA-256, from any issuer
        let cr = vec![1, 64, 0, 2, 4, 3, 0, 0];

        let mut flight = Vec::new();
        for (typ, b) in [(HS_SERVER_HELLO, sh), (HS_CERTIFICATE, c), (HS_SERVER_KX, ske), (HS_CERT_REQUEST, cr), (HS_SERVER_DONE, Vec::new())] {
            let seq = self.send_seq;
            self.send_seq += 1;
            self.add_transcript(&message(typ, seq, &b))?;
            flight.extend(fragments(typ, seq, &b).into_iter().map(|f| (CT_HANDSHAKE, 0u16, f)));
        }
        self.send_flight(flight, Some(now), out);
        self.state = State::Certificate;
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ClientState {
    /// The first ClientHello is out: a HelloVerifyRequest or the ServerHello comes.
    Hello,
    Certificate,
    KeyExchange,
    /// A CertificateRequest, or the ServerHelloDone.
    RequestOrDone,
    Finished,
    Done,
}

/// A DTLS 1.2 client (RFC 6347) for DTLS-SRTP, the role a WHIP publisher takes: it offers
/// `TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256` and the SRTP profiles [`profiles`] allows (so only
/// AEAD_AES_128_GCM in FIPS mode), takes the server's certificate only if its SHA-256 fingerprint is
/// the one the SDP answer gave, answers the server's CertificateRequest with its own certificate, and
/// exports the SRTP keys. Like [`Server`] it holds no socket: datagrams go in and come out, the flight
/// is sent again with a doubling timer, and the whole handshake must finish within [`HANDSHAKE_TIMEOUT`].
/// Nothing from the network is trusted: a record or message that does not parse is dropped, and one
/// that parses but breaks the protocol ends the handshake.
pub(crate) struct Client {
    pub id: std::sync::Arc<Identity>,
    pub server_fingerprint: [u8; 32],
    pub keys: Option<SrtpKeys>,
    offered: Vec<u16>,
    offer_ems: bool,
    state: ClientState,
    layer: RecordLayer,
    random: [u8; 32],
    server_random: [u8; 32],
    transcript: Vec<u8>,
    cookie: Vec<u8>,
    send_seq: u16,
    recv_seq: u16,
    pending: BTreeMap<u16, Partial>,
    ems: bool,
    profile: Option<Profile>,
    server_key: Vec<u8>,
    server_share: Vec<u8>,
    cert_requested: bool,
    master: Option<[u8; 48]>,
    flight: Vec<(u8, u16, Vec<u8>)>,
    flight_version: u16,
    resend_at: Option<Instant>,
    backoff: Duration,
    answered: Option<Instant>,
    started: Option<Instant>,
    closed: bool,
}

impl Client {
    /// A client that offers what this build allows and will take only a server whose certificate
    /// has the SHA-256 `server_fingerprint`.
    pub(crate) fn new(id: std::sync::Arc<Identity>, server_fingerprint: [u8; 32]) -> Result<Client, String> {
        let offered = profiles(crate::fips::enabled()).iter().map(|p| p.id()).collect();
        Client::with_options(id, server_fingerprint, offered, true)
    }

    /// [`Client::new`] offering the SRTP profiles numbered `offered` and, or not, the extended master secret.
    pub(crate) fn with_options(id: std::sync::Arc<Identity>, server_fingerprint: [u8; 32], offered: Vec<u16>, offer_ems: bool) -> Result<Client, String> {
        let mut rng = ic_drbg::Rng::from_os().map_err(|e| e.to_string())?;
        Ok(Client {
            id,
            server_fingerprint,
            keys: None,
            offered,
            offer_ems,
            state: ClientState::Hello,
            layer: RecordLayer::default(),
            random: rng.random_array().map_err(|e| e.to_string())?,
            server_random: [0; 32],
            transcript: Vec::new(),
            cookie: Vec::new(),
            send_seq: 0,
            recv_seq: 0,
            pending: BTreeMap::new(),
            ems: false,
            profile: None,
            server_key: Vec::new(),
            server_share: Vec::new(),
            cert_requested: false,
            master: None,
            flight: Vec::new(),
            flight_version: DTLS_1_0,
            resend_at: None,
            backoff: FIRST_BACKOFF,
            answered: None,
            started: None,
            closed: false,
        })
    }

    /// The SRTP keys, once the handshake is done.
    pub(crate) fn keys(&self) -> Option<&SrtpKeys> {
        self.keys.as_ref()
    }

    /// Whether the server has ended the association (close_notify, or a fatal alert after the handshake).
    pub(crate) fn closed(&self) -> bool {
        self.closed
    }

    /// A close_notify to send when the publisher ends the session.
    pub(crate) fn close_notify(&mut self) -> Vec<Vec<u8>> {
        let epoch = if self.layer.write.is_some() && self.state == ClientState::Done { 1 } else { 0 };
        self.layer.pack(&[(CT_ALERT, epoch, vec![1, ALERT_CLOSE_NOTIFY])], DTLS_1_2)
    }

    /// The first flight: a ClientHello without a cookie.
    pub(crate) fn start(&mut self, now: Instant) -> Result<Vec<Vec<u8>>, String> {
        self.started = Some(now);
        let mut out = Vec::new();
        self.hello(now, &mut out)?;
        Ok(out)
    }

    fn hello(&mut self, now: Instant, out: &mut Vec<Vec<u8>>) -> Result<(), String> {
        let mut b = DTLS_1_2.to_be_bytes().to_vec();
        b.extend_from_slice(&self.random);
        b.push(0); // no session id
        b.push(self.cookie.len() as u8);
        b.extend_from_slice(&self.cookie);
        b.extend_from_slice(&[0, 2]);
        b.extend_from_slice(&SUITE.to_be_bytes());
        b.extend_from_slice(&[1, 0]);
        let mut ext = Vec::new();
        let mut push = |t: u16, v: &[u8]| {
            ext.extend_from_slice(&t.to_be_bytes());
            ext.extend_from_slice(&(v.len() as u16).to_be_bytes());
            ext.extend_from_slice(v);
        };
        let mut srtp = ((self.offered.len() * 2) as u16).to_be_bytes().to_vec();
        for p in &self.offered {
            srtp.extend_from_slice(&p.to_be_bytes());
        }
        srtp.push(0);
        push(EXT_USE_SRTP, &srtp);
        push(EXT_GROUPS, &[0, 2, 0, GROUP_P256 as u8]);
        push(EXT_POINT_FORMATS, &[1, 0]);
        push(EXT_SIG_ALGS, &[0, 2, 4, 3]);
        push(EXT_RENEGOTIATION, &[0]);
        if self.offer_ems {
            push(EXT_EMS, &[]);
        }
        b.extend_from_slice(&(ext.len() as u16).to_be_bytes());
        b.extend_from_slice(&ext);
        let seq = self.send_seq;
        self.send_seq += 1;
        let m = message(HS_CLIENT_HELLO, seq, &b);
        // the handshake hash starts at the ClientHello that carries the cookie
        self.transcript = m.clone();
        self.send_flight(vec![(CT_HANDSHAKE, 0, m)], DTLS_1_0, now, out);
        Ok(())
    }

    fn send_flight(&mut self, flight: Vec<(u8, u16, Vec<u8>)>, version: u16, now: Instant, out: &mut Vec<Vec<u8>>) {
        self.flight = flight;
        self.flight_version = version;
        self.backoff = FIRST_BACKOFF;
        self.resend_at = Some(now + FIRST_BACKOFF);
        out.extend(self.layer.pack(&self.flight, version));
    }

    /// Resends the flight when its timer is up; fails once the handshake has taken too long.
    pub(crate) fn poll(&mut self, now: Instant) -> Result<Vec<Vec<u8>>, String> {
        if self.state != ClientState::Done && self.started.is_some_and(|s| now.duration_since(s) > HANDSHAKE_TIMEOUT) {
            return Err(format!("the DTLS handshake did not finish within {} s", HANDSHAKE_TIMEOUT.as_secs()));
        }
        match self.resend_at {
            Some(t) if now >= t && self.state != ClientState::Done => {
                self.backoff = (self.backoff * 2).min(MAX_BACKOFF);
                self.resend_at = Some(now + self.backoff);
                Ok(self.layer.pack(&self.flight, self.flight_version))
            }
            _ => Ok(Vec::new()),
        }
    }

    /// Takes a datagram from the server, returning the datagrams to send back. An error ends the
    /// handshake (or, once it is done, the association).
    pub(crate) fn handle(&mut self, datagram: &[u8], now: Instant) -> Result<Vec<Vec<u8>>, String> {
        let mut out = Vec::new();
        for r in records(datagram) {
            let Some((epoch, payload)) = self.layer.open(&r) else { continue };
            match r.ct {
                CT_HANDSHAKE => {
                    let mut at = 0;
                    while let Some(h) = payload.get(at..at + 12) {
                        let flen = ((h[9] as usize) << 16) | ((h[10] as usize) << 8) | h[11] as usize;
                        let Some(frag) = payload.get(at..at + 12 + flen) else { break };
                        at += 12 + flen;
                        self.fragment(epoch, frag, now, &mut out)?;
                    }
                }
                // an alert in the clear means nothing once the keys are in use
                CT_ALERT if payload.len() >= 2 && (self.state != ClientState::Done || epoch == 1) => {
                    if payload[1] == ALERT_CLOSE_NOTIFY {
                        self.closed = true;
                    } else if payload[0] == 2 {
                        if self.state == ClientState::Done {
                            self.closed = true;
                        } else {
                            return Err(format!("the server ended the DTLS handshake with alert {}", payload[1]));
                        }
                    }
                }
                _ => {}
            }
        }
        Ok(out)
    }

    fn fragment(&mut self, epoch: u16, frag: &[u8], now: Instant, out: &mut Vec<Vec<u8>>) -> Result<(), String> {
        let mut r = Reader(frag);
        let (Some(typ), Some(len), Some(seq), Some(off), Some(flen)) = (r.u8(), r.u24(), r.u16(), r.u24(), r.u24()) else { return Ok(()) };
        let data = r.0;
        if len > MAX_MESSAGE || off + flen > len || data.len() != flen {
            return Ok(());
        }
        if self.state == ClientState::Done {
            return Ok(());
        }
        if seq < self.recv_seq {
            // the server sent its flight again: ours was lost
            if self.answered.is_none_or(|t| now.duration_since(t) >= ANSWER_EVERY) && !self.flight.is_empty() {
                self.answered = Some(now);
                out.extend(self.layer.pack(&self.flight, self.flight_version));
            }
            return Ok(());
        }
        if seq - self.recv_seq > MAX_AHEAD {
            return Ok(());
        }
        let p = self.pending.entry(seq).or_insert_with(|| Partial { typ, epoch, body: vec![0; len], have: vec![false; len], missing: len });
        if p.typ != typ || p.body.len() != len {
            return Ok(());
        }
        for (k, b) in data.iter().enumerate() {
            if !p.have[off + k] {
                p.have[off + k] = true;
                p.body[off + k] = *b;
                p.missing -= 1;
            }
        }
        p.epoch = p.epoch.max(epoch);
        while self.pending.get(&self.recv_seq).is_some_and(|p| p.missing == 0) {
            let Some(p) = self.pending.remove(&self.recv_seq) else { break };
            let seq = self.recv_seq;
            self.recv_seq = self.recv_seq.wrapping_add(1);
            self.on_message(p.typ, p.epoch, seq, &p.body, now, out)?;
        }
        Ok(())
    }

    fn add_transcript(&mut self, m: &[u8]) -> Result<(), String> {
        if self.transcript.len() + m.len() > MAX_TRANSCRIPT {
            return Err("the DTLS handshake is too long".into());
        }
        self.transcript.extend_from_slice(m);
        Ok(())
    }

    fn on_message(&mut self, typ: u8, epoch: u16, seq: u16, body: &[u8], now: Instant, out: &mut Vec<Vec<u8>>) -> Result<(), String> {
        let raw = message(typ, seq, body);
        // only Finished comes under the new keys, and nothing else may
        if (typ == HS_FINISHED) != (epoch == 1) {
            return Err("a DTLS handshake message came in the wrong epoch".into());
        }
        match (self.state, typ) {
            (ClientState::Hello, HS_HELLO_VERIFY) => {
                let mut r = Reader(body);
                r.u16().ok_or("malformed HelloVerifyRequest")?;
                let cookie = r.vec8().ok_or("malformed HelloVerifyRequest")?;
                // one cookie only: a server that asks again is not to keep this client sending
                if !self.cookie.is_empty() || cookie.is_empty() {
                    return Err("the server's HelloVerifyRequest is not acceptable".into());
                }
                self.cookie = cookie.to_vec();
                self.hello(now, out)
            }
            (ClientState::Hello, HS_SERVER_HELLO) => {
                let mut r = Reader(body);
                if r.u16() != Some(DTLS_1_2) {
                    return Err("the server does not speak DTLS 1.2".into());
                }
                self.server_random = r.take(32).ok_or("malformed ServerHello")?.try_into().map_err(|_| "malformed ServerHello")?;
                let session = r.vec8().ok_or("malformed ServerHello")?;
                if session.len() > 32 {
                    return Err("malformed ServerHello".into());
                }
                if r.u16() != Some(SUITE) {
                    return Err("the server chose a cipher suite other than TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256".into());
                }
                if r.u8() != Some(0) {
                    return Err("the server chose compression".into());
                }
                let (mut srtp, mut ems) = (None, false);
                if !r.0.is_empty() {
                    let mut ext = Reader(r.vec16().ok_or("malformed ServerHello")?);
                    let mut count = 0;
                    while !ext.0.is_empty() {
                        count += 1;
                        if count > 32 {
                            return Err("malformed ServerHello".into());
                        }
                        let t = ext.u16().ok_or("malformed ServerHello")?;
                        let mut v = Reader(ext.vec16().ok_or("malformed ServerHello")?);
                        match t {
                            EXT_USE_SRTP => {
                                let list = v.vec16().ok_or("malformed use_srtp")?;
                                if list.len() != 2 || v.vec8().is_none() {
                                    return Err("malformed use_srtp".into());
                                }
                                srtp = Some(u16::from_be_bytes([list[0], list[1]]));
                            }
                            EXT_EMS => ems = true,
                            _ => {}
                        }
                    }
                }
                let id = srtp.ok_or("the server did not negotiate DTLS-SRTP (no use_srtp)")?;
                let profile = Profile::from_id(id).filter(|p| self.offered.contains(&p.id())).ok_or("the server chose an SRTP protection profile that was not offered")?;
                if ems && !self.offer_ems {
                    return Err("the server answered an extension that was not offered".into());
                }
                self.profile = Some(profile);
                self.ems = ems;
                self.add_transcript(&raw)?;
                self.state = ClientState::Certificate;
                Ok(())
            }
            (ClientState::Certificate, HS_CERTIFICATE) => {
                let mut r = Reader(body);
                let mut list = Reader(r.vec24().ok_or("malformed Certificate")?);
                let first = list.vec24().ok_or("the server sent no certificate")?;
                // its bytes are vouched for by the fingerprint the answer gave, so only its key is wanted
                if !ic_core::ct::verify(&sha256(first), &self.server_fingerprint) {
                    return Err("the server's certificate is not the one its answer's fingerprint names".into());
                }
                let spki = spki_of(first).ok_or("the server's certificate does not parse")?.to_vec();
                let key = PublicKey::from_spki(&spki).map_err(|e| format!("the server's key: {e}"))?;
                if !key.matches(SignatureScheme::EcdsaSecp256r1Sha256) {
                    return Err("the server's certificate key is not ECDSA P-256".into());
                }
                self.server_key = spki;
                self.add_transcript(&raw)?;
                self.state = ClientState::KeyExchange;
                Ok(())
            }
            (ClientState::KeyExchange, HS_SERVER_KX) => {
                let mut r = Reader(body);
                if r.u8() != Some(3) || r.u16() != Some(GROUP_P256) {
                    return Err("the server's key exchange is not on P-256".into());
                }
                let point = r.vec8().ok_or("malformed ServerKeyExchange")?;
                let params = body.get(..4 + point.len()).ok_or("malformed ServerKeyExchange")?;
                if r.u16() != Some(ECDSA_P256_SHA256) {
                    return Err("the server signed with a scheme other than ECDSA P-256 SHA-256".into());
                }
                let sig = r.vec16().ok_or("malformed ServerKeyExchange")?;
                let key = PublicKey::from_spki(&self.server_key).map_err(|e| e.to_string())?;
                sign::verify(SignatureScheme::EcdsaSecp256r1Sha256, &key, &[&self.random[..], &self.server_random[..], params].concat(), sig).map_err(|_| "the server's ServerKeyExchange signature does not check out")?;
                self.server_share = point.to_vec();
                self.add_transcript(&raw)?;
                self.state = ClientState::RequestOrDone;
                Ok(())
            }
            (ClientState::RequestOrDone, HS_CERT_REQUEST) if !self.cert_requested => {
                self.cert_requested = true;
                self.add_transcript(&raw)
            }
            (ClientState::RequestOrDone, HS_SERVER_DONE) => {
                if !body.is_empty() {
                    return Err("malformed ServerHelloDone".into());
                }
                self.add_transcript(&raw)?;
                self.final_flight(now, out)
            }
            (ClientState::Finished, HS_FINISHED) => {
                let master = self.master.ok_or("no master secret")?;
                let mut want = [0u8; 12];
                prf(&master, b"server finished", &sha256(&self.transcript), &mut want)?;
                if !ic_core::ct::verify(&want, body) {
                    return Err("the server's Finished does not check out".into());
                }
                self.keys = Some(export_srtp(&master, &self.random, &self.server_random, self.profile.ok_or("no SRTP profile")?)?);
                self.state = ClientState::Done;
                self.resend_at = None;
                Ok(())
            }
            _ => Err(format!("unexpected DTLS handshake message {typ}")),
        }
    }

    /// Adds a handshake message to the transcript and to `flight`, cut into fragments.
    fn queue(&mut self, flight: &mut Vec<(u8, u16, Vec<u8>)>, typ: u8, body: Vec<u8>) -> Result<(), String> {
        let seq = self.send_seq;
        self.send_seq += 1;
        self.add_transcript(&message(typ, seq, &body))?;
        flight.extend(fragments(typ, seq, &body).into_iter().map(|f| (CT_HANDSHAKE, 0u16, f)));
        Ok(())
    }

    /// Certificate (when asked for), ClientKeyExchange, CertificateVerify, ChangeCipherSpec, Finished.
    fn final_flight(&mut self, now: Instant, out: &mut Vec<Vec<u8>>) -> Result<(), String> {
        let mut flight: Vec<(u8, u16, Vec<u8>)> = Vec::new();
        if self.cert_requested {
            let cert = self.id.cert.clone();
            let mut c = ((cert.len() + 3) as u32).to_be_bytes()[1..].to_vec();
            c.extend_from_slice(&(cert.len() as u32).to_be_bytes()[1..]);
            c.extend_from_slice(&cert);
            self.queue(&mut flight, HS_CERTIFICATE, c)?;
        }
        let mut rng = ic_drbg::Rng::from_os().map_err(|e| e.to_string())?;
        let share = KeyShare::generate(NamedGroup::Secp256r1, &mut rng).map_err(|e| e.to_string())?;
        let mut kx = vec![share.public().len() as u8];
        kx.extend_from_slice(share.public());
        self.queue(&mut flight, HS_CLIENT_KX, kx)?;
        let pms = share.complete(&self.server_share).map_err(|e| format!("the server's key share: {e}"))?;
        let ems = self.ems.then(|| sha256(&self.transcript));
        let (master, client, server) = key_schedule(pms.get(), ems.as_ref(), &self.random, &self.server_random)?;
        self.master = Some(master);
        self.layer.write = Some(client);
        self.layer.read = Some(server);
        if self.cert_requested {
            let sig = self.id.sign(&self.transcript)?;
            let mut cv = ECDSA_P256_SHA256.to_be_bytes().to_vec();
            cv.extend_from_slice(&(sig.len() as u16).to_be_bytes());
            cv.extend_from_slice(&sig);
            self.queue(&mut flight, HS_CERT_VERIFY, cv)?;
        }
        flight.push((CT_CCS, 0, vec![1]));
        let mut verify = [0u8; 12];
        prf(&master, b"client finished", &sha256(&self.transcript), &mut verify)?;
        let seq = self.send_seq;
        self.send_seq += 1;
        let fin = message(HS_FINISHED, seq, &verify);
        self.add_transcript(&fin)?;
        flight.push((CT_HANDSHAKE, 1, fin));
        self.send_flight(flight, DTLS_1_2, now, out);
        self.pending.clear();
        self.state = ClientState::Finished;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn unhex(s: &str) -> Vec<u8> {
        let s: String = s.split_whitespace().collect();
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn the_client_key_is_found_in_certificates_rfc_5280_would_refuse() {
        fn tlv(tag: u8, body: &[u8]) -> Vec<u8> {
            let mut v = vec![tag];
            if body.len() < 0x80 {
                v.push(body.len() as u8);
            } else {
                v.extend_from_slice(&[0x82, (body.len() >> 8) as u8, body.len() as u8]);
            }
            v.extend_from_slice(body);
            v
        }
        let id = Identity::generate("test").unwrap();
        let spki = x509::Certificate::parse(&id.cert).unwrap().spki_der().to_vec();
        assert_eq!(spki_of(&id.cert), Some(&spki[..]));
        // as OBS (libdatachannel) makes them: version 3 with an empty extensions list
        let alg = tlv(0x30, &tlv(0x06, &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02]));
        let name = tlv(0x30, &tlv(0x31, &tlv(0x30, &[tlv(0x06, &[0x55, 0x04, 0x03]), tlv(0x0c, b"x")].concat())));
        let validity = tlv(0x30, &[tlv(0x17, b"260101000000Z"), tlv(0x17, b"270101000000Z")].concat());
        for (version, exts) in [(tlv(0xa0, &tlv(0x02, &[2])), tlv(0xa3, &tlv(0x30, &[]))), (Vec::new(), Vec::new())] {
            let tbs = tlv(0x30, &[version, tlv(0x02, &[1]), alg.clone(), name.clone(), validity.clone(), name.clone(), spki.clone(), exts].concat());
            let cert = tlv(0x30, &[tbs, alg.clone(), tlv(0x03, &[0, 1, 2])].concat());
            assert_eq!(spki_of(&cert), Some(&spki[..]));
        }
        // and nothing else is taken for one
        let mut rng = 7u32;
        for n in 0..2000 {
            let mut b = id.cert[..n % id.cert.len()].to_vec();
            if n % 3 == 0 {
                rng = rng.wrapping_mul(1664525).wrapping_add(1013904223);
                let at = rng as usize % id.cert.len();
                b = id.cert.clone();
                b[at] ^= (rng >> 24) as u8 | 1;
            }
            let _ = spki_of(&b);
        }
        assert_eq!(spki_of(&[]), None);
        assert_eq!(spki_of(&[0x30, 0x84, 0xff, 0xff, 0xff, 0xff]), None);
    }

    #[test]
    fn tls12_prf_sha256_vector() {
        // the widely used TLS 1.2 PRF (SHA-256) test vector from the IETF TLS working group list
        let mut out = [0u8; 100];
        prf(&unhex("9bbe436ba940f017b17652849a71db35"), b"test label", &unhex("a0ba9f936cda311827a6f796ffd5198c"), &mut out).unwrap();
        assert_eq!(
            out.to_vec(),
            unhex(
                "e3f229ba727be17b8d122620557cd453c2aab21d07c3d495329b52d4e61edb5a6b301791e90d35c9c9a46b4e14baf9af0fa022f7077def17abfd3797c0564bab4fbc91666e9def9b97fce34f796789baa48082d122ee42c5a72e5a5110fff70187347b66"
            )
        );
    }

    /// Runs the handshake between a server and the test client, passing each datagram through
    /// `wire` (which may drop, repeat or reorder), until both have keys or `rounds` pass.
    fn run(server: &mut Server, client: &mut Client, mut wire: impl FnMut(usize, bool, Vec<Vec<u8>>) -> Vec<Vec<u8>>) -> Result<(), String> {
        let peer = b"127.0.0.1:5000";
        let mut now = Instant::now();
        let mut to_server = client.start(now)?;
        for round in 0..40 {
            let mut to_client = Vec::new();
            for d in wire(round, true, std::mem::take(&mut to_server)) {
                to_client.extend(server.handle(&d, peer, now).map_err(|e| e.0)?);
            }
            now += Duration::from_millis(600);
            to_client.extend(server.poll(now)?);
            for d in wire(round, false, to_client) {
                to_server.extend(client.handle(&d, now)?);
            }
            if server.keys().is_some() && client.keys.is_some() {
                return Ok(());
            }
        }
        Err("the handshake did not finish".into())
    }

    fn test_client(server_fingerprint: [u8; 32], profiles: &[u16], ems: bool) -> Client {
        Client::with_options(Arc::new(Identity::generate("IronAV test client").unwrap()), server_fingerprint, profiles.to_vec(), ems).unwrap()
    }

    fn pair(profiles: &[u16], ems: bool) -> (Server, Client) {
        let id = Arc::new(Identity::generate("IronAV test").unwrap());
        let client = test_client(id.fingerprint, profiles, ems);
        let server = Server::new(id, client.id.fingerprint).unwrap();
        (server, client)
    }

    #[test]
    fn a_handshake_gives_both_sides_the_same_srtp_keys() {
        for (profiles, ems, want) in [(&[0x0001u16, 0x0007][..], true, Profile::AeadAes128Gcm), (&[0x0001][..], false, Profile::Aes128CmHmacSha1_80)] {
            let (mut s, mut c) = pair(profiles, ems);
            let r = run(&mut s, &mut c, |_, _, d| d);
            if crate::fips::enabled() && want != Profile::AeadAes128Gcm {
                assert!(r.unwrap_err().contains("FIPS"));
                continue;
            }
            r.unwrap();
            assert_eq!(s.keys(), c.keys.as_ref());
            assert_eq!(s.keys().unwrap().profile, want);
            assert_eq!(s.keys().unwrap().client_salt.len(), want.salt_len());
        }
    }

    #[test]
    fn fips_mode_takes_only_aes_gcm() {
        assert_eq!(profiles(true), [Profile::AeadAes128Gcm]);
        assert_eq!(profiles(false), [Profile::AeadAes128Gcm, Profile::Aes128CmHmacSha1_80]);
    }

    #[test]
    fn lost_and_repeated_datagrams_are_recovered() {
        // the server's first full flight is lost: its timer sends it again
        let (mut s, mut c) = pair(&[0x0007], true);
        run(&mut s, &mut c, |round, to_server, d| if round == 1 && !to_server { Vec::new() } else { d }).unwrap();
        assert_eq!(s.keys(), c.keys.as_ref());
        // every datagram arrives twice
        let (mut s, mut c) = pair(&[0x0007], true);
        run(&mut s, &mut c, |_, _, d| d.iter().flat_map(|x| [x.clone(), x.clone()]).collect()).unwrap();
        assert_eq!(s.keys(), c.keys.as_ref());
    }

    #[test]
    fn a_wrong_fingerprint_or_no_common_profile_fails() {
        let id = Arc::new(Identity::generate("IronAV test").unwrap());
        let mut c = test_client(id.fingerprint, &[0x0007], true);
        let mut s = Server::new(id, [9u8; 32]).unwrap();
        let e = run(&mut s, &mut c, |_, _, d| d).unwrap_err();
        assert!(e.contains("fingerprint"), "{e}");

        let (mut s, mut c) = pair(&[0x0002], true);
        let e = run(&mut s, &mut c, |_, _, d| d).unwrap_err();
        assert!(e.contains("SRTP"), "{e}");
    }

    #[test]
    fn a_handshake_that_stalls_times_out() {
        let (mut s, mut c) = pair(&[0x0007], true);
        let now = Instant::now();
        let peer = b"x";
        let hvr = s.handle(&c.start(now).unwrap()[0], peer, now).unwrap();
        let ch2 = c.handle(&hvr[0], now).unwrap();
        s.handle(&ch2[0], peer, now).unwrap();
        assert!(!s.poll(now + Duration::from_secs(1)).unwrap().is_empty(), "the flight is sent again");
        assert!(s.poll(now + HANDSHAKE_TIMEOUT + Duration::from_secs(1)).is_err());
    }

    #[test]
    fn fragments_are_put_back_together_in_any_order() {
        let (mut s, mut c) = pair(&[0x0007], true);
        let peer = b"p";
        let now = Instant::now();
        let hvr = s.handle(&c.start(now).unwrap()[0], peer, now).unwrap();
        let ch2 = c.handle(&hvr[0], now).unwrap();
        // the ClientHello cut into 3 fragments, sent last first, each in its own record
        let recs = records(&ch2[0]);
        let payload = recs[0].body;
        let body = &payload[12..];
        let n = body.len();
        let mut frags = Vec::new();
        for (off, end) in [(0, n / 3), (n / 3, 2 * n / 3), (2 * n / 3, n)] {
            let mut m = payload[..6].to_vec();
            m.extend_from_slice(&(off as u32).to_be_bytes()[1..]);
            m.extend_from_slice(&((end - off) as u32).to_be_bytes()[1..]);
            m.extend_from_slice(&body[off..end]);
            frags.push(m);
        }
        let mut layer = RecordLayer { next: [5, 0], ..Default::default() };
        let mut got = Vec::new();
        for f in frags.iter().rev() {
            got.extend(s.handle(&layer.pack(&[(CT_HANDSHAKE, 0, f.clone())], DTLS_1_2)[0], peer, now).unwrap());
        }
        assert!(!got.is_empty(), "the reassembled ClientHello is answered");
        let mut to_server = Vec::new();
        for d in got {
            to_server.extend(c.handle(&d, now).unwrap());
        }
        let mut back = Vec::new();
        for d in to_server {
            back.extend(s.handle(&d, peer, now).unwrap());
        }
        for d in back {
            c.handle(&d, now).unwrap();
        }
        assert_eq!(s.keys(), c.keys.as_ref());
    }

    #[test]
    fn the_client_takes_only_the_server_its_answer_named() {
        let id = Arc::new(Identity::generate("IronAV test").unwrap());
        let mut c = test_client([9u8; 32], &[0x0007], true);
        let mut s = Server::new(id, c.id.fingerprint).unwrap();
        let e = run(&mut s, &mut c, |_, _, d| d).unwrap_err();
        assert!(e.contains("fingerprint"), "{e}");
        assert!(c.keys().is_none());
    }

    #[test]
    fn the_client_offers_what_this_build_allows_and_takes_nothing_else() {
        let id = Arc::new(Identity::generate("IronAV test").unwrap());
        let c = Client::new(id, [0; 32]).unwrap();
        let want: Vec<u16> = profiles(crate::fips::enabled()).iter().map(|p| p.id()).collect();
        assert_eq!(c.offered, want);
        assert!(!crate::fips::enabled() || want == [0x0007], "FIPS mode offers only AEAD_AES_128_GCM");
        // a server that picks a profile that was not offered is refused
        let (mut s, mut c) = pair(&[0x0007], true);
        let (peer, now) = (b"p", Instant::now());
        let hvr = s.handle(&c.start(now).unwrap()[0], peer, now).unwrap();
        let ch2 = c.handle(&hvr[0], now).unwrap();
        c.offered = vec![0x0001];
        let mut err = String::new();
        for d in s.handle(&ch2[0], peer, now).unwrap() {
            if let Err(e) = c.handle(&d, now) {
                err = e;
            }
        }
        assert!(err.contains("not offered"), "{err}");
    }
    #[test]
    fn a_hostile_server_never_panics_the_client() {
        let (mut s, mut c) = pair(&[0x0007], true);
        let peer = b"x";
        let now = Instant::now();
        let first = c.start(now).unwrap();
        let hvr = s.handle(&first[0], peer, now).unwrap();
        let ch2 = c.handle(&hvr[0], now).unwrap();
        let flight = s.handle(&ch2[0], peer, now).unwrap();
        assert!(!flight.is_empty());
        let mut seed = 0x51ED_u32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for k in 0..4000 {
            let mut d = flight[k % flight.len()].clone();
            for _ in 0..1 + next() % 4 {
                let i = next() as usize % d.len();
                d[i] = next() as u8;
            }
            if next() % 3 == 0 {
                d.truncate(next() as usize % d.len());
            }
            let _ = c.handle(&d, now);
            let random: Vec<u8> = (0..next() % 300).map(|_| next() as u8).collect();
            let _ = c.handle(&random, now);
            let _ = c.poll(now + Duration::from_millis(next() as u64 % 20_000));
        }
        // and the real flight still works on a client that has not been wrecked: truncations only
        let (mut s, mut c) = pair(&[0x0007], true);
        let hvr = s.handle(&c.start(now).unwrap()[0], peer, now).unwrap();
        let ch2 = c.handle(&hvr[0], now).unwrap();
        let flight = s.handle(&ch2[0], peer, now).unwrap();
        for d in &flight {
            for n in 0..d.len() {
                let _ = c.handle(&d[..n], now);
            }
        }
        let mut back = Vec::new();
        for d in &flight {
            back.extend(c.handle(d, now).unwrap());
        }
        assert!(!back.is_empty(), "the whole flight still gets its answer");
    }

    #[test]
    fn the_client_asks_for_nothing_twice_and_ignores_a_server_that_does() {
        // two HelloVerifyRequests in a row end the handshake rather than loop
        let (mut s, mut c) = pair(&[0x0007], true);
        let now = Instant::now();
        let first = c.start(now).unwrap();
        let hvr = s.handle(&first[0], b"p", now).unwrap();
        c.handle(&hvr[0], now).unwrap();
        let e = c.handle(&hvr[0], now);
        assert!(e.is_ok(), "the same one again is an old message, not a second request");
        let mut layer = RecordLayer { next: [9, 0], ..Default::default() };
        let mut v = DTLS_1_0.to_be_bytes().to_vec();
        v.extend_from_slice(&[4, 1, 2, 3, 4]);
        let again = layer.pack(&[(CT_HANDSHAKE, 0, message(HS_HELLO_VERIFY, 1, &v))], DTLS_1_0);
        assert!(c.handle(&again[0], now).unwrap_err().contains("HelloVerifyRequest"));
    }
    #[test]
    fn garbage_never_panics() {
        let (mut s, mut c) = pair(&[0x0007], true);
        let peer = b"g";
        let now = Instant::now();
        let first = c.start(now).unwrap()[0].clone();
        let hvr = s.handle(&first, peer, now).unwrap();
        let ch2 = c.handle(&hvr[0], now).unwrap()[0].clone();
        let mut seed = 7u32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for n in 0..ch2.len() {
            let (mut s, _) = pair(&[0x0007], true);
            let _ = s.handle(&ch2[..n], peer, now);
        }
        for _ in 0..3000 {
            let mut d = if next() % 2 == 0 { ch2.clone() } else { first.clone() };
            for _ in 0..1 + next() % 4 {
                let i = next() as usize % d.len();
                d[i] = next() as u8;
            }
            if next() % 3 == 0 {
                d.truncate(next() as usize % d.len());
            }
            let _ = s.handle(&d, peer, now);
            let random: Vec<u8> = (0..next() % 300).map(|_| next() as u8).collect();
            let _ = s.handle(&random, peer, now);
            let _ = parse_hello(&random);
        }
    }
}
