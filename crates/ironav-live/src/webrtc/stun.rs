//! STUN (RFC 8489) as ICE-lite needs it (RFC 8445 §2.5): binding requests checked against the
//! short-term credentials of the session (MESSAGE-INTEGRITY, HMAC-SHA-1) and its FINGERPRINT (CRC-32),
//! and success responses that carry the requester's address back (XOR-MAPPED-ADDRESS). Nothing is
//! ever sent first, and anything malformed is dropped without an answer.

use crate::sha1::hmac_sha1;
use std::net::SocketAddr;

pub(crate) const MAGIC: u32 = 0x2112_A442;
const BINDING_REQUEST: u16 = 0x0001;
const BINDING_SUCCESS: u16 = 0x0101;
const USERNAME: u16 = 0x0006;
const MESSAGE_INTEGRITY: u16 = 0x0008;
const XOR_MAPPED_ADDRESS: u16 = 0x0020;
const PRIORITY: u16 = 0x0024;
const USE_CANDIDATE: u16 = 0x0025;
const FINGERPRINT: u16 = 0x8028;
const ICE_CONTROLLING: u16 = 0x802A;
const FINGERPRINT_XOR: u32 = 0x5354_554e;
/// The largest message looked at: a binding request is a hundred bytes or so.
const MAX_MESSAGE: usize = 1024;

/// Whether a datagram is STUN (RFC 7983's first-byte ranges, and the magic cookie).
pub(crate) fn is_stun(b: &[u8]) -> bool {
    b.len() >= 20 && b[0] < 4 && b[4..8] == MAGIC.to_be_bytes()
}

/// A binding request whose credentials checked out.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Binding {
    pub txid: [u8; 12],
    pub use_candidate: bool,
}

/// CRC-32 (ISO-HDLC, as Ethernet and zlib use), for FINGERPRINT.
pub(crate) fn crc32(data: &[u8]) -> u32 {
    let mut c = !0u32;
    for &b in data {
        c ^= b as u32;
        for _ in 0..8 {
            c = if c & 1 != 0 { (c >> 1) ^ 0xEDB8_8320 } else { c >> 1 };
        }
    }
    !c
}

/// The attributes of a message, each as (type, value, offset of the attribute in the message).
fn attributes(msg: &[u8]) -> Option<Vec<(u16, &[u8], usize)>> {
    let len = u16::from_be_bytes([msg[2], msg[3]]) as usize;
    if msg.len() != 20 + len || !len.is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::new();
    let mut at = 20;
    while at < msg.len() {
        let head = msg.get(at..at + 4)?;
        let (t, n) = (u16::from_be_bytes([head[0], head[1]]), u16::from_be_bytes([head[2], head[3]]) as usize);
        let value = msg.get(at + 4..at + 4 + n)?;
        out.push((t, value, at));
        if out.len() > 32 {
            return None;
        }
        at += 4 + n.div_ceil(4) * 4;
    }
    (at == msg.len()).then_some(out)
}

/// The HMAC over `msg` up to the attribute at `upto`, with the length field counting that
/// attribute's 24 bytes as RFC 8489 §14.5 has it.
fn integrity(msg: &[u8], upto: usize, key: &[u8]) -> [u8; 20] {
    let mut head = [0u8; 20];
    head.copy_from_slice(&msg[..20]);
    head[2..4].copy_from_slice(&((upto + 24 - 20) as u16).to_be_bytes());
    hmac_sha1(key, &[&head, &msg[20..upto]])
}

/// Checks a message's FINGERPRINT (which must be its last attribute) and MESSAGE-INTEGRITY under
/// `password` (which must come before it), returning its attributes. Short-term credentials: the
/// key is the password itself (RFC 8489 §9.1.2).
fn verify<'a>(msg: &'a [u8], password: &[u8]) -> Option<Vec<(u16, &'a [u8], usize)>> {
    let attrs = attributes(msg)?;
    let &(t, fp, at) = attrs.last()?;
    if t != FINGERPRINT || fp.len() != 4 || u32::from_be_bytes([fp[0], fp[1], fp[2], fp[3]]) != crc32(&msg[..at]) ^ FINGERPRINT_XOR {
        return None;
    }
    let mi = attrs.iter().position(|a| a.0 == MESSAGE_INTEGRITY)?;
    // only FINGERPRINT may follow MESSAGE-INTEGRITY
    if mi + 2 != attrs.len() || attrs[mi].1.len() != 20 {
        return None;
    }
    let want = integrity(msg, attrs[mi].2, password);
    ic_core::ct::verify(&want, attrs[mi].1).then_some(attrs)
}

/// A binding request for this agent: `USERNAME` is `local:remote` (RFC 8445 §7.2.2), and its
/// integrity checks out under the local password. Anything else gives `None`.
pub(crate) fn check_request(msg: &[u8], local_ufrag: &str, remote_ufrag: &str, local_pwd: &str) -> Option<Binding> {
    if !is_stun(msg) || msg.len() > MAX_MESSAGE || u16::from_be_bytes([msg[0], msg[1]]) != BINDING_REQUEST {
        return None;
    }
    let attrs = verify(msg, local_pwd.as_bytes())?;
    let user = attrs.iter().find(|a| a.0 == USERNAME)?.1;
    let want = format!("{local_ufrag}:{remote_ufrag}");
    if !ic_core::ct::verify(want.as_bytes(), user) {
        return None;
    }
    let mut txid = [0u8; 12];
    txid.copy_from_slice(&msg[8..20]);
    Some(Binding { txid, use_candidate: attrs.iter().any(|a| a.0 == USE_CANDIDATE) })
}

fn push_attr(out: &mut Vec<u8>, t: u16, value: &[u8]) {
    out.extend_from_slice(&t.to_be_bytes());
    out.extend_from_slice(&(value.len() as u16).to_be_bytes());
    out.extend_from_slice(value);
    out.resize(out.len().div_ceil(4) * 4, 0);
}

/// Appends MESSAGE-INTEGRITY under `password` and FINGERPRINT, fixing the length as each goes in.
fn seal(mut out: Vec<u8>, password: &[u8]) -> Vec<u8> {
    let at = out.len();
    let mac = integrity(&out, at, password);
    push_attr(&mut out, MESSAGE_INTEGRITY, &mac);
    let len = (out.len() + 8 - 20) as u16;
    out[2..4].copy_from_slice(&len.to_be_bytes());
    let fp = crc32(&out) ^ FINGERPRINT_XOR;
    push_attr(&mut out, FINGERPRINT, &fp.to_be_bytes());
    out
}

fn header(t: u16, txid: &[u8; 12]) -> Vec<u8> {
    let mut out = Vec::with_capacity(80);
    out.extend_from_slice(&t.to_be_bytes());
    out.extend_from_slice(&[0, 0]);
    out.extend_from_slice(&MAGIC.to_be_bytes());
    out.extend_from_slice(txid);
    out
}

/// XOR-MAPPED-ADDRESS's value for `addr` (RFC 8489 §14.2).
fn xor_address(addr: SocketAddr, txid: &[u8; 12]) -> Vec<u8> {
    let port = addr.port() ^ (MAGIC >> 16) as u16;
    let mut v = vec![0];
    match addr {
        SocketAddr::V4(a) => {
            v.push(1);
            v.extend_from_slice(&port.to_be_bytes());
            v.extend(a.ip().octets().iter().zip(MAGIC.to_be_bytes()).map(|(x, m)| x ^ m));
        }
        SocketAddr::V6(a) => {
            v.push(2);
            v.extend_from_slice(&port.to_be_bytes());
            let mut mask = MAGIC.to_be_bytes().to_vec();
            mask.extend_from_slice(txid);
            v.extend(a.ip().octets().iter().zip(mask).map(|(x, m)| x ^ m));
        }
    }
    v
}

/// The success response to a binding request from `from`, signed with the local password.
pub(crate) fn success(txid: &[u8; 12], from: SocketAddr, local_pwd: &str) -> Vec<u8> {
    let mut out = header(BINDING_SUCCESS, txid);
    push_attr(&mut out, XOR_MAPPED_ADDRESS, &xor_address(from, txid));
    let len = (out.len() - 20) as u16;
    out[2..4].copy_from_slice(&len.to_be_bytes());
    seal(out, local_pwd.as_bytes())
}

/// A binding request as a controlling agent sends it (RFC 8445 §7.2.4, with `tiebreaker` in ICE-CONTROLLING),
/// signed with the peer's password. `username` is `remote:local`. A WHIP publisher sends these as its
/// connectivity checks and its consent checks (RFC 7675).
pub(crate) fn request(txid: &[u8; 12], username: &str, password: &str, use_candidate: bool, tiebreaker: [u8; 8]) -> Vec<u8> {
    let mut out = header(BINDING_REQUEST, txid);
    push_attr(&mut out, USERNAME, username.as_bytes());
    push_attr(&mut out, PRIORITY, &0x6e00_01ffu32.to_be_bytes());
    push_attr(&mut out, ICE_CONTROLLING, &tiebreaker);
    if use_candidate {
        push_attr(&mut out, USE_CANDIDATE, &[]);
    }
    let len = (out.len() - 20) as u16;
    out[2..4].copy_from_slice(&len.to_be_bytes());
    seal(out, password.as_bytes())
}

/// Whether `msg` is the success response to the request `txid` and checks out under `password`
/// (MESSAGE-INTEGRITY and FINGERPRINT), whatever address it carries.
pub(crate) fn is_success(msg: &[u8], txid: &[u8; 12], password: &str) -> bool {
    msg.len() <= MAX_MESSAGE && is_stun(msg) && u16::from_be_bytes([msg[0], msg[1]]) == BINDING_SUCCESS && msg[8..20] == txid[..] && verify(msg, password.as_bytes()).is_some()
}

/// [`is_success`], with the address the response says the request came from (XOR-MAPPED-ADDRESS); for tests.
#[cfg(test)]
pub(crate) fn check_success(msg: &[u8], txid: &[u8; 12], password: &str) -> Option<SocketAddr> {
    if !is_success(msg, txid, password) {
        return None;
    }
    let attrs = attributes(msg)?;
    let v = attrs.iter().find(|a| a.0 == XOR_MAPPED_ADDRESS)?.1;
    let port = u16::from_be_bytes([*v.get(2)?, *v.get(3)?]) ^ (MAGIC >> 16) as u16;
    match *v.get(1)? {
        1 => {
            let a = v.get(4..8)?;
            let ip: [u8; 4] = std::array::from_fn(|i| a[i] ^ MAGIC.to_be_bytes()[i]);
            Some(SocketAddr::from((ip, port)))
        }
        2 => {
            let a = v.get(4..20)?;
            let mut mask = MAGIC.to_be_bytes().to_vec();
            mask.extend_from_slice(txid);
            let ip: [u8; 16] = std::array::from_fn(|i| a[i] ^ mask[i]);
            Some(SocketAddr::from((ip, port)))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        let s: String = s.split_whitespace().collect();
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    /// RFC 5769 §2.1: the sample request, with the password "VOkJxbRl1RmTxUk/WvJxBt".
    const REQUEST: &str = "0001 0058 2112a442 b7e7a701 bc34d686 fa87dfae
        8022 0010 5354554e 20746573 7420636c 69656e74
        0024 0004 6e0001ff
        8029 0008 932ff9b1 51263b36
        0006 0009 6576746a 3a683676 59202020
        0008 0014 9aeaa70c bfd8cb56 781ef2b5 b2d3f249 c1b571a2
        8028 0004 e57a3bcf";

    /// RFC 5769 §2.2: the sample IPv4 response, with the same password.
    const RESPONSE: &str = "0101 003c 2112a442 b7e7a701 bc34d686 fa87dfae
        8022 000b 74657374 20766563 746f7220
        0020 0008 0001a147 e112a643
        0008 0014 2b91f599 fd9e90c3 8c7489f9 2af9ba53 f06be7d7
        8028 0004 c07d4c96";

    #[test]
    fn rfc_5769_vectors_check_out() {
        let req = unhex(REQUEST);
        assert!(is_stun(&req));
        // the vector's USERNAME is "evtj:h6vY"
        let b = check_request(&req, "evtj", "h6vY", "VOkJxbRl1RmTxUk/WvJxBt").expect("the sample request verifies");
        assert_eq!(b.txid.to_vec(), unhex("b7e7a701 bc34d686 fa87dfae"));
        assert!(!b.use_candidate);
        // a wrong password, a wrong user name, and any flipped bit are refused
        assert!(check_request(&req, "evtj", "h6vY", "VOkJxbRl1RmTxUk/WvJxBu").is_none());
        assert!(check_request(&req, "evtj", "h6vZ", "VOkJxbRl1RmTxUk/WvJxBt").is_none());
        for i in 0..req.len() * 8 {
            let mut bad = req.clone();
            bad[i / 8] ^= 1 << (i % 8);
            assert!(check_request(&bad, "evtj", "h6vY", "VOkJxbRl1RmTxUk/WvJxBt").is_none(), "bit {i}");
        }

        let resp = unhex(RESPONSE);
        assert!(verify(&resp, b"VOkJxbRl1RmTxUk/WvJxBt").is_some());
        let txid: [u8; 12] = resp[8..20].try_into().unwrap();
        assert_eq!(check_success(&resp, &txid, "VOkJxbRl1RmTxUk/WvJxBt"), Some("192.0.2.1:32853".parse().unwrap()));
        // and the response this module makes for that address carries the vector's XOR-MAPPED-ADDRESS
        let ours = success(&txid, "192.0.2.1:32853".parse().unwrap(), "VOkJxbRl1RmTxUk/WvJxBt");
        assert!(ours.windows(12).any(|w| w == &resp[36..48]));
        assert_eq!(check_success(&ours, &txid, "VOkJxbRl1RmTxUk/WvJxBt"), Some("192.0.2.1:32853".parse().unwrap()));
    }

    #[test]
    fn requests_round_trip_and_ipv6_maps() {
        let txid = [7u8; 12];
        let req = request(&txid, "local:remote", "pw-of-the-local-agent!!", true, [9; 8]);
        let b = check_request(&req, "local", "remote", "pw-of-the-local-agent!!").unwrap();
        assert!(b.use_candidate);
        let v6 = xor_address("[2001:db8::1]:5000".parse().unwrap(), &txid);
        assert_eq!(v6.len(), 20);
        assert_eq!(crc32(b"123456789"), 0xCBF4_3926);
    }

    #[test]
    fn garbage_is_dropped_without_panicking() {
        let req = unhex(REQUEST);
        let mut seed = 0x1234_5678u32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for n in 0..req.len() {
            assert!(check_request(&req[..n], "evtj", "h6vY", "VOkJxbRl1RmTxUk/WvJxBt").is_none());
        }
        for _ in 0..20_000 {
            let n = (next() % 200) as usize;
            let mut m: Vec<u8> = (0..n).map(|_| next() as u8).collect();
            if m.len() >= 20 && next() % 2 == 0 {
                m[..8].copy_from_slice(&[0, 1, 0, (n as u8).wrapping_sub(20) & !3, 0x21, 0x12, 0xa4, 0x42]);
            }
            let _ = check_request(&m, "a", "b", "c");
            if m.len() >= 20 {
                let _ = attributes(&m);
                let _ = verify(&m, b"c");
            }
        }
    }
}
