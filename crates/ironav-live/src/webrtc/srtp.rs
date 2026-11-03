//! SRTP and SRTCP (RFC 3711) with the two protection profiles DTLS-SRTP negotiates here:
//! `SRTP_AEAD_AES_128_GCM` (RFC 7714) and `SRTP_AES128_CM_HMAC_SHA1_80`. Session keys come from the
//! master key and salt through the AES-CM key derivation (RFC 3711 §4.3, no key derivation rate).
//!
//! Packets are authenticated before anything is taken from them, and each SSRC has a 64-packet
//! replay window over its 48-bit packet index (the rollover counter guessed as Appendix A does),
//! which moves only once a packet has authenticated. A context follows at most [`MAX_STREAMS`]
//! SSRCs, so a sender cannot make it grow without bound.

use crate::sha1::hmac_sha1;
use ic_cipher::{ctr_xor, Aes128, Aes128Gcm};
use ic_core::traits::{Aead, BlockCipher};
use std::collections::HashMap;

/// SSRCs followed per direction, at most.
pub(crate) const MAX_STREAMS: usize = 16;
const WINDOW: u64 = 64;
const GCM_TAG: usize = 16;
const CM_TAG: usize = 10;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Profile {
    AeadAes128Gcm,
    Aes128CmHmacSha1_80,
}

impl Profile {
    /// The profile's number in DTLS's `use_srtp` extension (RFC 5764 §4.1.2, RFC 7714 §14.2).
    pub(crate) fn id(self) -> u16 {
        match self {
            Profile::AeadAes128Gcm => 0x0007,
            Profile::Aes128CmHmacSha1_80 => 0x0001,
        }
    }

    #[cfg(test)]
    pub(crate) fn from_id(id: u16) -> Option<Profile> {
        match id {
            0x0007 => Some(Profile::AeadAes128Gcm),
            0x0001 => Some(Profile::Aes128CmHmacSha1_80),
            _ => None,
        }
    }

    pub(crate) fn key_len(self) -> usize {
        16
    }

    pub(crate) fn salt_len(self) -> usize {
        match self {
            Profile::AeadAes128Gcm => 12,
            Profile::Aes128CmHmacSha1_80 => 14,
        }
    }

    pub(crate) fn name(self) -> &'static str {
        match self {
            Profile::AeadAes128Gcm => "SRTP_AEAD_AES_128_GCM",
            Profile::Aes128CmHmacSha1_80 => "SRTP_AES128_CM_HMAC_SHA1_80",
        }
    }
}

/// Why a packet was dropped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reject {
    Malformed,
    Auth,
    Replay,
    TooManyStreams,
}

/// The AES-CM pseudo-random function of RFC 3711 §4.3.3: the keystream for `label` under the
/// master key, the master salt (up to 14 bytes, zero-padded on the right as RFC 7714 §11's 12-byte
/// salts are) XORed with the label in its key-id position.
fn derive(master: &Aes128, salt: &[u8], label: u8, out: &mut [u8]) -> Result<(), Reject> {
    let mut iv = [0u8; 16];
    iv[..salt.len()].copy_from_slice(salt);
    iv[7] ^= label;
    out.fill(0);
    ctr_xor(master, &iv, out).map_err(|_| Reject::Malformed)
}

/// One direction's session keys, for RTP or for RTCP.
struct Keys {
    aes: Aes128,
    gcm: Option<Aes128Gcm>,
    salt: [u8; 14],
    auth: [u8; 20],
}

impl Keys {
    /// Labels 0, 1, 2 for SRTP and 3, 4, 5 for SRTCP.
    fn new(profile: Profile, master_key: &[u8], master_salt: &[u8], first_label: u8) -> Result<Keys, Reject> {
        let master = Aes128::new(master_key).map_err(|_| Reject::Malformed)?;
        let mut key = [0u8; 16];
        let mut salt = [0u8; 14];
        let mut auth = [0u8; 20];
        derive(&master, master_salt, first_label, &mut key)?;
        derive(&master, master_salt, first_label + 2, &mut salt[..profile.salt_len()])?;
        let gcm = match profile {
            Profile::AeadAes128Gcm => Some(Aes128Gcm::new(&key).map_err(|_| Reject::Malformed)?),
            Profile::Aes128CmHmacSha1_80 => {
                derive(&master, master_salt, first_label + 1, &mut auth)?;
                None
            }
        };
        let aes = Aes128::new(&key).map_err(|_| Reject::Malformed)?;
        Ok(Keys { aes, gcm, salt, auth })
    }

    /// AES-CM's counter block: the salt, the SSRC and the index in their places (RFC 3711 §4.1.1).
    fn cm_iv(&self, ssrc: u32, index: u64) -> [u8; 16] {
        let mut iv = [0u8; 16];
        iv[..14].copy_from_slice(&self.salt);
        for (d, s) in iv[4..8].iter_mut().zip(ssrc.to_be_bytes()) {
            *d ^= s;
        }
        for (d, s) in iv[8..14].iter_mut().zip(&index.to_be_bytes()[2..]) {
            *d ^= s;
        }
        iv
    }

    /// GCM's nonce: two zero bytes, the SSRC, and the 48-bit index or the SRTCP index, XORed with
    /// the 12-byte salt (RFC 7714 §8.1, §9.1).
    fn gcm_iv(&self, ssrc: u32, index: u64) -> [u8; 12] {
        let mut iv = [0u8; 12];
        iv[2..6].copy_from_slice(&ssrc.to_be_bytes());
        iv[6..12].copy_from_slice(&index.to_be_bytes()[2..]);
        for (d, s) in iv.iter_mut().zip(&self.salt) {
            *d ^= s;
        }
        iv
    }
}

/// A replay window and the rollover counter for one SSRC.
#[derive(Debug, Default, Clone)]
struct Window {
    started: bool,
    /// The highest index authenticated, and which of the 64 before it have been.
    highest: u64,
    bits: u64,
}

impl Window {
    /// The index a sequence number stands for (RFC 3711 Appendix A).
    fn guess(&self, seq: u16) -> u64 {
        if !self.started {
            return seq as u64;
        }
        let roc = (self.highest >> 16) as i64;
        let s_l = (self.highest & 0xFFFF) as i64;
        let seq = seq as i64;
        let v = if s_l < 32768 {
            if seq - s_l > 32768 { roc - 1 } else { roc }
        } else if s_l - 32768 > seq {
            roc + 1
        } else {
            roc
        };
        if v < 0 {
            // before the first packet's rollover: too old to be anything but a replay
            return u64::MAX;
        }
        ((v as u64) << 16) | seq as u64
    }

    fn fresh(&self, index: u64) -> bool {
        if index == u64::MAX || index >= 1 << 48 {
            return false;
        }
        if !self.started || index > self.highest {
            return true;
        }
        let back = self.highest - index;
        back < WINDOW && self.bits & (1 << back) == 0
    }

    fn accept(&mut self, index: u64) {
        if !self.started {
            (self.started, self.highest, self.bits) = (true, index, 1);
        } else if index > self.highest {
            let shift = index - self.highest;
            self.bits = if shift < WINDOW { (self.bits << shift) | 1 } else { 1 };
            self.highest = index;
        } else {
            self.bits |= 1 << (self.highest - index);
        }
    }
}

/// Where an RTP header ends (fixed part, CSRCs, and any extension), if the packet holds a whole one.
pub(crate) fn rtp_header_len(p: &[u8]) -> Option<usize> {
    if p.len() < 12 || p[0] >> 6 != 2 {
        return None;
    }
    let mut n = 12 + 4 * (p[0] & 15) as usize;
    if p[0] & 0x10 != 0 {
        let e = p.get(n..n + 4)?;
        n += 4 + 4 * u16::from_be_bytes([e[2], e[3]]) as usize;
    }
    (n <= p.len()).then_some(n)
}

/// One direction of an SRTP session: what one side sends, protected or unprotected.
pub(crate) struct Context {
    profile: Profile,
    rtp: Keys,
    rtcp: Keys,
    /// Per SSRC: the RTP replay window, and the SRTCP one.
    rtp_in: HashMap<u32, Window>,
    rtcp_in: HashMap<u32, Window>,
    /// Per SSRC sent: the highest index sent (only a test publisher sends RTP).
    #[cfg(test)]
    rtp_out: HashMap<u32, Window>,
    rtcp_index: u32,
}

impl Context {
    pub(crate) fn new(profile: Profile, master_key: &[u8], master_salt: &[u8]) -> Result<Context, Reject> {
        if master_key.len() != profile.key_len() || master_salt.len() != profile.salt_len() {
            return Err(Reject::Malformed);
        }
        Ok(Context {
            profile,
            rtp: Keys::new(profile, master_key, master_salt, 0)?,
            rtcp: Keys::new(profile, master_key, master_salt, 3)?,
            rtp_in: HashMap::new(),
            rtcp_in: HashMap::new(),
            #[cfg(test)]
            rtp_out: HashMap::new(),
            rtcp_index: 0,
        })
    }

    fn window(map: &mut HashMap<u32, Window>, ssrc: u32) -> Result<Window, Reject> {
        match map.get(&ssrc) {
            Some(w) => Ok(w.clone()),
            None if map.len() >= MAX_STREAMS => Err(Reject::TooManyStreams),
            None => Ok(Window::default()),
        }
    }

    /// The RTP packet an SRTP packet carries, once it has authenticated and is not a replay.
    pub(crate) fn unprotect_rtp(&mut self, p: &[u8]) -> Result<Vec<u8>, Reject> {
        let h = rtp_header_len(p).ok_or(Reject::Malformed)?;
        let tag = match self.profile {
            Profile::AeadAes128Gcm => GCM_TAG,
            Profile::Aes128CmHmacSha1_80 => CM_TAG,
        };
        if p.len() < h + tag {
            return Err(Reject::Malformed);
        }
        let seq = u16::from_be_bytes([p[2], p[3]]);
        let ssrc = u32::from_be_bytes([p[8], p[9], p[10], p[11]]);
        let mut w = Self::window(&mut self.rtp_in, ssrc)?;
        let index = w.guess(seq);
        if !w.fresh(index) {
            return Err(Reject::Replay);
        }
        let body_end = p.len() - tag;
        let mut body = p[h..body_end].to_vec();
        match &self.rtp.gcm {
            Some(gcm) => gcm.open_detached(&self.rtp.gcm_iv(ssrc, index), &p[..h], &mut body, &p[body_end..]).map_err(|_| Reject::Auth)?,
            None => {
                let roc = ((index >> 16) as u32).to_be_bytes();
                let mac = hmac_sha1(&self.rtp.auth, &[&p[..body_end], &roc]);
                if !ic_core::ct::verify(&mac[..CM_TAG], &p[body_end..]) {
                    return Err(Reject::Auth);
                }
                ctr_xor(&self.rtp.aes, &self.rtp.cm_iv(ssrc, index), &mut body).map_err(|_| Reject::Malformed)?;
            }
        }
        w.accept(index);
        self.rtp_in.insert(ssrc, w);
        let mut out = p[..h].to_vec();
        out.extend_from_slice(&body);
        Ok(out)
    }

    /// The RTCP compound packet an SRTCP packet carries, once it has authenticated and is not a
    /// replay. An unencrypted SRTCP packet (its E flag clear) is refused.
    pub(crate) fn unprotect_rtcp(&mut self, p: &[u8]) -> Result<Vec<u8>, Reject> {
        let tag = match self.profile {
            Profile::AeadAes128Gcm => GCM_TAG,
            Profile::Aes128CmHmacSha1_80 => CM_TAG,
        };
        if p.len() < 8 + 4 + tag || p[0] >> 6 != 2 {
            return Err(Reject::Malformed);
        }
        let ssrc = u32::from_be_bytes([p[4], p[5], p[6], p[7]]);
        // GCM puts the E flag and index after the tag (RFC 7714 §9.1); AES-CM before it
        let trailer_at = match self.profile {
            Profile::AeadAes128Gcm => p.len() - 4,
            Profile::Aes128CmHmacSha1_80 => p.len() - 4 - tag,
        };
        let trailer: [u8; 4] = p[trailer_at..trailer_at + 4].try_into().map_err(|_| Reject::Malformed)?;
        let word = u32::from_be_bytes(trailer);
        if word >> 31 == 0 {
            return Err(Reject::Malformed);
        }
        let index = (word & 0x7FFF_FFFF) as u64;
        let mut w = Self::window(&mut self.rtcp_in, ssrc)?;
        if !w.fresh(index) {
            return Err(Reject::Replay);
        }
        let mut out = p[..8].to_vec();
        match &self.rtcp.gcm {
            Some(gcm) => {
                let mut body = p[8..trailer_at - tag].to_vec();
                let mut aad = p[..8].to_vec();
                aad.extend_from_slice(&trailer);
                gcm.open_detached(&self.rtcp.gcm_iv(ssrc, index), &aad, &mut body, &p[trailer_at - tag..trailer_at]).map_err(|_| Reject::Auth)?;
                out.extend_from_slice(&body);
            }
            None => {
                let mac = hmac_sha1(&self.rtcp.auth, &[&p[..trailer_at + 4]]);
                if !ic_core::ct::verify(&mac[..CM_TAG], &p[trailer_at + 4..]) {
                    return Err(Reject::Auth);
                }
                let mut body = p[8..trailer_at].to_vec();
                ctr_xor(&self.rtcp.aes, &self.rtcp.cm_iv(ssrc, index), &mut body).map_err(|_| Reject::Malformed)?;
                out.extend_from_slice(&body);
            }
        }
        w.accept(index);
        self.rtcp_in.insert(ssrc, w);
        Ok(out)
    }

    /// An RTP packet protected as SRTP (the sender keeps the rollover counter by watching its own
    /// sequence numbers wrap), as a publisher sends it; for tests.
    #[cfg(test)]
    pub(crate) fn protect_rtp(&mut self, p: &[u8]) -> Result<Vec<u8>, Reject> {
        let h = rtp_header_len(p).ok_or(Reject::Malformed)?;
        let seq = u16::from_be_bytes([p[2], p[3]]);
        let ssrc = u32::from_be_bytes([p[8], p[9], p[10], p[11]]);
        let mut w = Self::window(&mut self.rtp_out, ssrc)?;
        let index = w.guess(seq);
        if index >= 1 << 48 {
            return Err(Reject::Malformed);
        }
        let mut body = p[h..].to_vec();
        let mut out = p[..h].to_vec();
        match &self.rtp.gcm {
            Some(gcm) => {
                let mut tag = [0u8; GCM_TAG];
                gcm.seal_detached(&self.rtp.gcm_iv(ssrc, index), &p[..h], &mut body, &mut tag).map_err(|_| Reject::Malformed)?;
                out.extend_from_slice(&body);
                out.extend_from_slice(&tag);
            }
            None => {
                ctr_xor(&self.rtp.aes, &self.rtp.cm_iv(ssrc, index), &mut body).map_err(|_| Reject::Malformed)?;
                out.extend_from_slice(&body);
                let roc = ((index >> 16) as u32).to_be_bytes();
                let mac = hmac_sha1(&self.rtp.auth, &[&out, &roc]);
                out.extend_from_slice(&mac[..CM_TAG]);
            }
        }
        if !w.started || index > w.highest {
            w.accept(index);
        }
        self.rtp_out.insert(ssrc, w);
        Ok(out)
    }

    /// An RTCP compound packet protected as SRTCP, encrypted, with the next SRTCP index.
    pub(crate) fn protect_rtcp(&mut self, p: &[u8]) -> Result<Vec<u8>, Reject> {
        if p.len() < 8 {
            return Err(Reject::Malformed);
        }
        let ssrc = u32::from_be_bytes([p[4], p[5], p[6], p[7]]);
        let index = self.rtcp_index;
        self.rtcp_index = (self.rtcp_index + 1) & 0x7FFF_FFFF;
        let trailer = (index | 0x8000_0000).to_be_bytes();
        let mut body = p[8..].to_vec();
        let mut out = p[..8].to_vec();
        match &self.rtcp.gcm {
            Some(gcm) => {
                let mut aad = p[..8].to_vec();
                aad.extend_from_slice(&trailer);
                let mut tag = [0u8; GCM_TAG];
                gcm.seal_detached(&self.rtcp.gcm_iv(ssrc, index as u64), &aad, &mut body, &mut tag).map_err(|_| Reject::Malformed)?;
                out.extend_from_slice(&body);
                out.extend_from_slice(&tag);
                out.extend_from_slice(&trailer);
            }
            None => {
                ctr_xor(&self.rtcp.aes, &self.rtcp.cm_iv(ssrc, index as u64), &mut body).map_err(|_| Reject::Malformed)?;
                out.extend_from_slice(&body);
                out.extend_from_slice(&trailer);
                let mac = hmac_sha1(&self.rtcp.auth, &[&out]);
                out.extend_from_slice(&mac[..CM_TAG]);
            }
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(s: &str) -> Vec<u8> {
        let s: String = s.split_whitespace().collect();
        (0..s.len()).step_by(2).map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap()).collect()
    }

    #[test]
    fn rfc_3711_key_derivation_vector() {
        // RFC 3711 Appendix B.3
        let master = Aes128::new(&unhex("E1F97A0D3E018BE0D64FA32C06DE4139")).unwrap();
        let salt = unhex("0EC675AD498AFEEBB6960B3AABE6");
        let mut key = [0u8; 16];
        derive(&master, &salt, 0, &mut key).unwrap();
        assert_eq!(key.to_vec(), unhex("C61E7A93744F39EE10734AFE3FF7A087"));
        let mut s = [0u8; 14];
        derive(&master, &salt, 2, &mut s).unwrap();
        assert_eq!(s.to_vec(), unhex("30CBBC08863D8C85D49DB34A9AE1"));
        let mut auth = [0u8; 20];
        derive(&master, &salt, 1, &mut auth).unwrap();
        assert_eq!(auth.to_vec(), unhex("CEBE321F6FF7716B6FD4AB49AF256A156D38BAA4"));
    }

    #[test]
    fn rfc_3711_keystream_vector() {
        // RFC 3711 Appendix B.2: the AES-CM keystream for a session key and salt, SSRC 0, index 0
        let keys = Keys { aes: Aes128::new(&unhex("2B7E151628AED2A6ABF7158809CF4F3C")).unwrap(), gcm: None, salt: unhex("F0F1F2F3F4F5F6F7F8F9FAFBFCFD").try_into().unwrap(), auth: [0; 20] };
        let mut ks = [0u8; 48];
        ctr_xor(&keys.aes, &keys.cm_iv(0, 0), &mut ks).unwrap();
        assert_eq!(ks.to_vec(), unhex("E03EAD0935C95E80E166B16DD92B4EB4 D23513162B02D0F72A43A2FE4A5F97AB 41E95B3BB0A2E8DD477901E4FCA894C0"));
    }

    #[test]
    fn rfc_7714_gcm_vector() {
        // RFC 7714 §16.1.1: AEAD_AES_128_GCM, with the session key and salt given directly
        let key = unhex("000102030405060708090a0b0c0d0e0f");
        let salt = unhex("517569642070726f2071756f");
        let mut padded = [0u8; 14];
        padded[..12].copy_from_slice(&salt);
        let keys = Keys { aes: Aes128::new(&key).unwrap(), gcm: Some(Aes128Gcm::new(&key).unwrap()), salt: padded, auth: [0; 20] };
        let plain = unhex("8040f17b 8041f8d3 5501a0b2 47616c6c 69612065 7374206f 6d6e6973 20646976 69736120 696e2070 61727465 73207472 6573");
        let want = unhex(
            "8040f17b 8041f8d3 5501a0b2 f24de3a3 fb34de6c acba861c 9d7e4bca be633bd5 0d294e6f 42a5f47a 51c7d19b 36de3adf 8833899d 7f27beb1 6a9152cf 765ee439 0cce",
        );
        let mut body = plain[12..].to_vec();
        let mut tag = [0u8; 16];
        keys.gcm.as_ref().unwrap().seal_detached(&keys.gcm_iv(0x5501a0b2, 0xf17b), &plain[..12], &mut body, &mut tag).unwrap();
        let mut got = plain[..12].to_vec();
        got.extend_from_slice(&body);
        got.extend_from_slice(&tag);
        assert_eq!(got, want);
    }

    fn rtp(seq: u16, ssrc: u32, payload: &[u8]) -> Vec<u8> {
        let mut p = vec![0x80, 96];
        p.extend_from_slice(&seq.to_be_bytes());
        p.extend_from_slice(&1234u32.to_be_bytes());
        p.extend_from_slice(&ssrc.to_be_bytes());
        p.extend_from_slice(payload);
        p
    }

    #[test]
    fn both_profiles_round_trip_and_refuse_replays_and_forgeries() {
        for profile in [Profile::AeadAes128Gcm, Profile::Aes128CmHmacSha1_80] {
            let (key, salt) = ([3u8; 16], vec![5u8; profile.salt_len()]);
            let mut tx = Context::new(profile, &key, &salt).unwrap();
            let mut rx = Context::new(profile, &key, &salt).unwrap();
            // across a sequence number wrap, so the rollover counter is used
            let mut sent = Vec::new();
            for k in 0..200u32 {
                let seq = 65500u16.wrapping_add(k as u16);
                let p = rtp(seq, 42, &k.to_be_bytes());
                let s = tx.protect_rtp(&p).unwrap();
                assert_ne!(&s[12..16], &p[12..16]);
                sent.push((p, s));
            }
            // out of order within the window is fine
            sent.swap(10, 12);
            for (p, s) in &sent {
                assert_eq!(&rx.unprotect_rtp(s).unwrap(), p, "{profile:?}");
            }
            assert_eq!(rx.unprotect_rtp(&sent[150].1), Err(Reject::Replay));
            let next = tx.protect_rtp(&rtp(65500u16.wrapping_add(200), 42, b"next")).unwrap();
            let mut forged = next.clone();
            *forged.last_mut().unwrap() ^= 1;
            assert_eq!(rx.unprotect_rtp(&forged), Err(Reject::Auth));
            forged[13] ^= 1;
            assert_eq!(rx.unprotect_rtp(&forged), Err(Reject::Auth));
            assert!(rx.unprotect_rtp(&next).is_ok(), "a forgery does not move the window");

            let rr = [0x80u8, 201, 0, 1, 0, 0, 0, 9];
            let s = tx.protect_rtcp(&rr).unwrap();
            assert_eq!(rx.unprotect_rtcp(&s).unwrap(), rr);
            assert_eq!(rx.unprotect_rtcp(&s), Err(Reject::Replay));
            let s = tx.protect_rtcp(&[0x81, 206, 0, 2, 0, 0, 0, 9, 0, 0, 0, 7]).unwrap();
            let mut bad = s.clone();
            bad[9] ^= 0x40;
            assert_eq!(rx.unprotect_rtcp(&bad), Err(Reject::Auth));
            assert_eq!(rx.unprotect_rtcp(&s).unwrap(), [0x81, 206, 0, 2, 0, 0, 0, 9, 0, 0, 0, 7]);
        }
    }

    #[test]
    fn streams_are_bounded_and_garbage_never_panics() {
        let mut tx = Context::new(Profile::AeadAes128Gcm, &[1; 16], &[2; 12]).unwrap();
        let mut rx = Context::new(Profile::AeadAes128Gcm, &[1; 16], &[2; 12]).unwrap();
        for ssrc in 0..MAX_STREAMS as u32 {
            rx.unprotect_rtp(&tx.protect_rtp(&rtp(1, ssrc, b"x")).unwrap()).unwrap();
        }
        let extra = Context::new(Profile::AeadAes128Gcm, &[1; 16], &[2; 12]).unwrap().protect_rtp(&rtp(1, 999, b"x")).unwrap();
        assert_eq!(tx.protect_rtp(&rtp(1, 999, b"x")), Err(Reject::TooManyStreams));
        assert_eq!(rx.unprotect_rtp(&extra), Err(Reject::TooManyStreams));

        let good = tx.protect_rtp(&rtp(5, 1, &[9u8; 100])).unwrap();
        let mut seed = 99u32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        for profile in [Profile::AeadAes128Gcm, Profile::Aes128CmHmacSha1_80] {
            let mut rx = Context::new(profile, &[1; 16], &vec![2; profile.salt_len()]).unwrap();
            for n in 0..good.len() {
                assert!(rx.unprotect_rtp(&good[..n]).is_err());
                assert!(rx.unprotect_rtcp(&good[..n]).is_err());
            }
            for _ in 0..20_000 {
                let n = (next() % 120) as usize;
                let mut m: Vec<u8> = (0..n).map(|_| next() as u8).collect();
                if let Some(b) = m.first_mut() {
                    *b = 0x80 | (*b & 0x3F);
                }
                let _ = rx.unprotect_rtp(&m);
                let _ = rx.unprotect_rtcp(&m);
            }
        }
    }
}
