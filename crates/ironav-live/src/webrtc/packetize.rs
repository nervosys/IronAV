//! What a WHIP publisher puts on the wire: H.264 access units as RTP payloads (RFC 6184, single NAL
//! unit packets and FU-A fragments, `packetization-mode=1`), Opus packets as they are (RFC 7587), the
//! RTP header, RTCP sender reports with an SDES CNAME (RFC 3550 §6), and the feedback a server sends
//! back: picture loss indications and full intra requests (RFC 4585, RFC 5104), and generic NACKs.
//!
//! Everything that reads bytes from the network here bounds-checks and returns what it understood.

/// The most payload an RTP packet carries: with the 12-byte header, the SRTP tag and IP and UDP
/// headers it stays well inside any path's MTU.
pub(crate) const MAX_PAYLOAD: usize = 1200;
/// The largest access unit taken, and the most NAL units looked at in one.
pub(crate) const MAX_ACCESS_UNIT: usize = 16 << 20;
/// An Opus packet is at most 1275 bytes for one frame (RFC 6716 §3.2.1); a few frames may be bundled, but
/// not in one packet that would not fit a datagram.
pub(crate) const MAX_OPUS: usize = 1400;
/// Parameter sets kept to put before a keyframe that lacks them.
const MAX_PARAMETER_SET: usize = 4096;

const NAL_IDR: u8 = 5;
const NAL_SPS: u8 = 7;
const NAL_PPS: u8 = 8;
const NAL_AUD: u8 = 9;
const NAL_FILLER: u8 = 12;
const NAL_FU_A: u8 = 28;

/// The NAL units of an Annex B byte stream (start codes `00 00 01` or `00 00 00 01`), without the
/// start codes or the zero bytes that trail them. Bytes before the first start code are not a NAL unit.
pub(crate) fn split_annexb(d: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= d.len() {
        if d[i] == 0 && d[i + 1] == 0 && d[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::with_capacity(starts.len());
    for (k, &begin) in starts.iter().enumerate() {
        let end = starts.get(k + 1).map_or(d.len(), |next| next - 3);
        let mut nal = &d[begin..end];
        while let [rest @ .., 0] = nal {
            nal = rest;
        }
        if !nal.is_empty() {
            out.push(nal);
        }
    }
    out
}

/// Cuts H.264 access units into RTP payloads, and remembers the parameter sets so a keyframe that
/// comes without them (an encoder that sends them once) still has them in front of it.
#[derive(Default)]
pub(crate) struct H264Packetizer {
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

impl H264Packetizer {
    /// The payloads of one access unit, in order (the last gets the RTP marker), and whether it is a
    /// keyframe (an IDR slice or a sequence parameter set). Access unit delimiters, filler data,
    /// NAL units with the forbidden bit set and the types that RTP uses for aggregation (24 to 31) are left out.
    pub(crate) fn access_unit(&mut self, annexb: &[u8], max: usize) -> Result<(Vec<Vec<u8>>, bool), String> {
        if annexb.len() > MAX_ACCESS_UNIT {
            return Err(format!("an access unit of {} bytes is larger than {} MiB", annexb.len(), MAX_ACCESS_UNIT >> 20));
        }
        let max = max.clamp(16, MAX_PAYLOAD);
        let mut nals: Vec<&[u8]> = split_annexb(annexb).into_iter().filter(|n| n[0] & 0x80 == 0 && !matches!(n[0] & 31, NAL_AUD | NAL_FILLER | 24..=31)).collect();
        if nals.is_empty() {
            return Err("the access unit has no NAL units (H.264 in Annex B form, with start codes)".into());
        }
        let has = |t: u8, nals: &[&[u8]]| nals.iter().any(|n| n[0] & 31 == t);
        let (sps, pps) = (self.sps.clone(), self.pps.clone());
        for n in &nals {
            let slot = match n[0] & 31 {
                NAL_SPS => &mut self.sps,
                NAL_PPS => &mut self.pps,
                _ => continue,
            };
            if n.len() <= MAX_PARAMETER_SET {
                *slot = Some(n.to_vec());
            }
        }
        let key = has(NAL_IDR, &nals) || has(NAL_SPS, &nals);
        if has(NAL_IDR, &nals) {
            if let (false, Some(s)) = (has(NAL_SPS, &nals), sps.as_deref()) {
                nals.insert(0, s);
            }
            if let (false, Some(p)) = (has(NAL_PPS, &nals), pps.as_deref()) {
                let at = nals.iter().rposition(|n| n[0] & 31 == NAL_SPS).map_or(0, |i| i + 1);
                nals.insert(at, p);
            }
        }
        let mut out = Vec::new();
        for nal in nals {
            if nal.len() <= max {
                out.push(nal.to_vec());
                continue;
            }
            // FU-A (RFC 6184 §5.8): the indicator carries the NAL's F and NRI bits, the header its type
            // and the start and end flags; the NAL's own header byte is not sent
            let indicator = (nal[0] & 0xE0) | NAL_FU_A;
            let chunks = nal[1..].chunks(max - 2);
            let count = chunks.len();
            for (k, c) in chunks.enumerate() {
                let flags = if k == 0 { 0x80 } else { 0 } | if k + 1 == count { 0x40 } else { 0 };
                let mut p = Vec::with_capacity(c.len() + 2);
                p.push(indicator);
                p.push(flags | (nal[0] & 31));
                p.extend_from_slice(c);
                out.push(p);
            }
        }
        Ok((out, key))
    }
}

/// An Opus packet's RTP payload: the packet itself (RFC 7587 §4.2).
pub(crate) fn opus_payload(packet: &[u8]) -> Result<&[u8], String> {
    if packet.is_empty() || packet.len() > MAX_OPUS {
        return Err(format!("an Opus packet is 1 to {MAX_OPUS} bytes, got {}", packet.len()));
    }
    Ok(packet)
}

/// An RTP packet (RFC 3550 §5.1) without padding, extension or contributing sources.
pub(crate) fn rtp_packet(payload_type: u8, marker: bool, seq: u16, timestamp: u32, ssrc: u32, payload: &[u8]) -> Vec<u8> {
    let mut p = Vec::with_capacity(12 + payload.len());
    p.push(0x80);
    p.push((payload_type & 0x7F) | if marker { 0x80 } else { 0 });
    p.extend_from_slice(&seq.to_be_bytes());
    p.extend_from_slice(&timestamp.to_be_bytes());
    p.extend_from_slice(&ssrc.to_be_bytes());
    p.extend_from_slice(payload);
    p
}

/// The 64-bit NTP timestamp (seconds since 1900 and a binary fraction) of a time since the Unix epoch.
pub(crate) fn ntp(since_unix: std::time::Duration) -> u64 {
    let secs = since_unix.as_secs().wrapping_add(2_208_988_800) & 0xFFFF_FFFF;
    let frac = ((since_unix.subsec_nanos() as u64) << 32) / 1_000_000_000;
    secs << 32 | frac
}

/// A compound RTCP packet: a sender report for `ssrc` and an SDES with its CNAME (RFC 3550 §6.1: a
/// compound packet carries the CNAME), with no report blocks.
pub(crate) fn sender_report(ssrc: u32, ntp: u64, rtp_timestamp: u32, packets: u32, octets: u32, cname: &str) -> Vec<u8> {
    let mut p = vec![0x80, 200, 0, 6];
    p.extend_from_slice(&ssrc.to_be_bytes());
    p.extend_from_slice(&ntp.to_be_bytes());
    p.extend_from_slice(&rtp_timestamp.to_be_bytes());
    p.extend_from_slice(&packets.to_be_bytes());
    p.extend_from_slice(&octets.to_be_bytes());
    let cname = &cname.as_bytes()[..cname.len().min(255)];
    let mut chunk = ssrc.to_be_bytes().to_vec();
    chunk.extend_from_slice(&[1, cname.len() as u8]);
    chunk.extend_from_slice(cname);
    // the chunk ends with at least one zero byte, to a 32-bit boundary
    chunk.push(0);
    chunk.resize(chunk.len().div_ceil(4) * 4, 0);
    p.extend_from_slice(&[0x81, 202]);
    p.extend_from_slice(&((chunk.len() / 4) as u16).to_be_bytes());
    p.extend_from_slice(&chunk);
    p
}

/// An RTCP BYE for `ssrc` (RFC 3550 §6.6), as a packet by itself: it follows a sender report in a compound packet.
pub(crate) fn bye(ssrc: u32) -> Vec<u8> {
    let mut p = vec![0x81, 203, 0, 1];
    p.extend_from_slice(&ssrc.to_be_bytes());
    p
}

/// What an RTCP packet from the server asks of the publisher.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Feedback {
    /// A picture loss indication or full intra request for these SSRCs.
    Keyframe(Vec<u32>),
    /// A generic NACK (RFC 4585 §6.2.1): these sequence numbers of `media` were lost.
    Nack { media: u32, seqs: Vec<u16> },
}

/// The most sequence numbers taken from one compound packet.
const MAX_NACKED: usize = 256;

/// The requests in a compound RTCP packet, ignoring what it does not understand and stopping at
/// the first packet that does not fit (RFC 3550 §6.4: length in 32-bit words, minus one).
pub(crate) fn feedback(compound: &[u8]) -> Vec<Feedback> {
    let mut out = Vec::new();
    let mut at = 0;
    let mut nacked = 0;
    while let Some(h) = compound.get(at..at + 4) {
        let len = 4 * (u16::from_be_bytes([h[2], h[3]]) as usize + 1);
        let Some(p) = compound.get(at..at + len) else { break };
        at += len;
        if p[0] >> 6 != 2 {
            break;
        }
        let (fmt, pt) = (p[0] & 31, p[1]);
        let word = |i: usize| p.get(i..i + 4).map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]));
        match (pt, fmt) {
            // payload-specific feedback: PLI (1), FIR (4, the SSRCs are in the FCI entries)
            (206, 1) => out.extend(word(8).map(|m| Feedback::Keyframe(vec![m]))),
            (206, 4) => {
                let ssrcs: Vec<u32> = (0..8).filter_map(|k| word(12 + 8 * k)).collect();
                out.push(Feedback::Keyframe(ssrcs));
            }
            // transport feedback: generic NACK, a packet id and a bitmask of the 16 after it
            (205, 1) => {
                let Some(media) = word(8) else { continue };
                let mut seqs = Vec::new();
                let mut i = 12;
                while let Some(fci) = p.get(i..i + 4) {
                    let pid = u16::from_be_bytes([fci[0], fci[1]]);
                    let blp = u16::from_be_bytes([fci[2], fci[3]]);
                    for k in 0..17u16 {
                        if (k == 0 || blp & (1 << (k - 1)) != 0) && nacked < MAX_NACKED {
                            seqs.push(pid.wrapping_add(k));
                            nacked += 1;
                        }
                    }
                    i += 4;
                }
                out.push(Feedback::Nack { media, seqs });
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn annexb(nals: &[&[u8]]) -> Vec<u8> {
        nals.iter().flat_map(|n| [&[0, 0, 0, 1][..], n].concat()).collect()
    }

    /// The NAL units RFC 6184 §5.8's receiver makes of single NAL unit packets and FU-A fragments.
    fn depacketize(payloads: &[Vec<u8>]) -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        let mut open: Option<Vec<u8>> = None;
        for p in payloads {
            match p[0] & 31 {
                NAL_FU_A => {
                    let (start, end) = (p[1] & 0x80 != 0, p[1] & 0x40 != 0);
                    if start {
                        assert!(open.is_none(), "a fragment starts inside another NAL unit");
                        open = Some(vec![(p[0] & 0xE0) | (p[1] & 31)]);
                    }
                    open.as_mut().expect("a fragment with no start").extend_from_slice(&p[2..]);
                    if end {
                        out.push(open.take().unwrap());
                    }
                }
                t => {
                    assert!((1..=23).contains(&t) && open.is_none());
                    out.push(p.clone());
                }
            }
        }
        assert!(open.is_none(), "a NAL unit never ended");
        out
    }

    fn nal(header: u8, len: usize) -> Vec<u8> {
        let mut n = vec![header];
        n.extend((1..len).map(|i| (i as u8).wrapping_mul(7) | 1));
        n
    }

    #[test]
    fn annex_b_splits_at_either_start_code() {
        let a = nal(0x67, 10);
        let b = nal(0x68, 4);
        let c = nal(0x65, 30);
        let mixed = [&[0u8, 0, 1][..], &a, &[0, 0, 0, 1], &b, &[0, 0, 0, 0, 0, 1], &c, &[0, 0, 0]].concat();
        assert_eq!(split_annexb(&mixed), [&a[..], &b[..], &c[..]]);
        assert_eq!(split_annexb(&annexb(&[&a, &b])), [&a[..], &b[..]]);
        assert!(split_annexb(&[]).is_empty() && split_annexb(&[1, 2, 3, 4]).is_empty() && split_annexb(&[0, 0, 1]).is_empty() && split_annexb(&[0, 0, 0, 1, 0, 0, 1]).is_empty());
        // bytes before the first start code are not a NAL unit
        assert_eq!(split_annexb(&[9, 9, 0, 0, 1, 0x41, 5]), [&[0x41u8, 5][..]]);
    }

    #[test]
    fn fu_a_fragments_at_every_boundary_and_puts_the_nal_back_together() {
        let max = MAX_PAYLOAD;
        let body = max - 2;
        // around the single-packet limit, around each fragment count, and a big NAL unit
        for len in [1, 2, max - 1, max, max + 1, max + 2, 1 + body, 1 + body + 1, 1 + 2 * body, 1 + 2 * body + 1, 1 + 3 * body, 100_000] {
            for header in [0x65u8, 0x41, 0x25, 0x01] {
                let n = nal(header, len);
                let (payloads, _) = H264Packetizer::default().access_unit(&annexb(&[&n]), max).unwrap();
                assert!(payloads.iter().all(|p| !p.is_empty() && p.len() <= max), "len {len}");
                assert_eq!(payloads.len(), if len <= max { 1 } else { (len - 1).div_ceil(body) }, "len {len}");
                assert_eq!(depacketize(&payloads), vec![n.clone()], "len {len} header {header:#x}");
                if len > max {
                    // S on the first fragment only, E on the last only, F and NRI as the NAL's own
                    let flags: Vec<u8> = payloads.iter().map(|p| p[1] & 0xC0).collect();
                    assert_eq!(flags[0], 0x80);
                    assert_eq!(*flags.last().unwrap(), 0x40);
                    assert!(flags[1..flags.len() - 1].iter().all(|f| *f == 0));
                    assert!(payloads.iter().all(|p| p[0] == (header & 0xE0) | 28 && p[1] & 31 == header & 31 && p[1] & 0x20 == 0));
                    // every fragment but the last is full
                    assert!(payloads[..payloads.len() - 1].iter().all(|p| p.len() == max));
                }
            }
        }
        // a small limit works the same
        let n = nal(0x65, 500);
        let (payloads, _) = H264Packetizer::default().access_unit(&annexb(&[&n]), 100).unwrap();
        assert!(payloads.iter().all(|p| p.len() <= 100) && payloads.len() == 6);
        assert_eq!(depacketize(&payloads), [n]);
    }

    #[test]
    fn an_access_unit_keeps_its_nal_units_in_order_and_drops_what_does_not_belong() {
        let (sps, pps, idr) = (nal(0x67, 20), nal(0x68, 6), nal(0x65, 3000));
        let (aud, filler, sei, bad) = (nal(0x09, 2), nal(0x0C, 40), nal(0x06, 9), nal(0xE5, 9));
        let mut p = H264Packetizer::default();
        let (payloads, key) = p.access_unit(&annexb(&[&aud, &sps, &pps, &sei, &idr, &filler, &bad]), MAX_PAYLOAD).unwrap();
        assert!(key);
        assert_eq!(depacketize(&payloads), [sps.clone(), pps.clone(), sei.clone(), idr.clone()]);
        // a P frame is not a keyframe
        let slice = nal(0x41, 800);
        let (payloads, key) = p.access_unit(&annexb(&[&slice]), MAX_PAYLOAD).unwrap();
        assert!(!key);
        assert_eq!(depacketize(&payloads), [slice]);
        // an IDR without parameter sets gets the last ones in front of it; with them, it does not
        let (payloads, key) = p.access_unit(&annexb(&[&idr]), MAX_PAYLOAD).unwrap();
        assert!(key);
        assert_eq!(depacketize(&payloads), [sps.clone(), pps.clone(), idr.clone()]);
        let (payloads, _) = p.access_unit(&annexb(&[&sps, &idr]), MAX_PAYLOAD).unwrap();
        assert_eq!(depacketize(&payloads), [sps.clone(), pps.clone(), idr.clone()]);
        // and a stream that never had any has nothing to add
        let (payloads, _) = H264Packetizer::default().access_unit(&annexb(&[&idr]), MAX_PAYLOAD).unwrap();
        assert_eq!(depacketize(&payloads), [idr]);
        // nothing usable is an error, not an empty access unit
        for bad in [&[][..], &[1, 2, 3], &annexb(&[&aud])[..], &annexb(&[&nal(0xFF, 9)])[..]] {
            assert!(H264Packetizer::default().access_unit(bad, MAX_PAYLOAD).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn opus_goes_out_as_it_is_and_rtp_headers_are_exact() {
        assert_eq!(opus_payload(&[1, 2, 3]).unwrap(), [1, 2, 3]);
        assert!(opus_payload(&[]).is_err() && opus_payload(&[0; MAX_OPUS + 1]).is_err() && opus_payload(&[0; MAX_OPUS]).is_ok());
        let p = rtp_packet(111, true, 0x0102, 0x0A0B0C0D, 0x11223344, &[9, 8]);
        assert_eq!(p, [0x80, 0xEF, 1, 2, 0x0A, 0x0B, 0x0C, 0x0D, 0x11, 0x22, 0x33, 0x44, 9, 8]);
        assert_eq!(rtp_packet(102, false, 0, 0, 0, &[])[1], 102);
        assert_eq!(super::super::srtp::rtp_header_len(&p), Some(12));
    }

    #[test]
    fn sender_reports_are_well_formed_compound_packets() {
        let sr = sender_report(0xAABBCCDD, ntp(std::time::Duration::new(1_000_000_000, 500_000_000)), 90_000, 7, 1234, "ironav");
        // an SR of 28 bytes and an SDES of one chunk: the SSRC, CNAME (type 1), its length, "ironav" and the padding
        assert_eq!(&sr[..8], [0x80, 200, 0, 6, 0xAA, 0xBB, 0xCC, 0xDD]);
        assert_eq!(sr.len(), 28 + 20);
        let sdes = &sr[28..];
        assert_eq!((sdes[0], sdes[1]), (0x81, 202));
        assert_eq!(4 * (u16::from_be_bytes([sdes[2], sdes[3]]) as usize + 1), sdes.len());
        assert_eq!(&sdes[4..8], [0xAA, 0xBB, 0xCC, 0xDD]);
        assert_eq!((sdes[8], sdes[9], &sdes[10..16]), (1, 6, &b"ironav"[..]));
        assert!(sdes[16..].iter().all(|b| *b == 0) && !sdes[16..].is_empty());
        // NTP: 1e9 s after 1970 and half a second
        let t = ntp(std::time::Duration::new(1_000_000_000, 500_000_000));
        assert_eq!((t >> 32) as u32, 1_000_000_000 + 2_208_988_800u64 as u32);
        assert_eq!(t as u32, 0x8000_0000);
        // every CNAME length pads to a word
        for n in 0..12 {
            let sr = sender_report(1, 0, 0, 0, 0, &"x".repeat(n));
            assert_eq!(sr.len() % 4, 0, "{n}");
        }
        assert_eq!(bye(5), [0x81, 203, 0, 1, 0, 0, 0, 5]);
    }

    #[test]
    fn feedback_is_read_from_compound_packets() {
        let ours = 0x1111u32;
        let mut c = vec![0x80, 201, 0, 1, 0, 0, 0, 9]; // a receiver report first
        c.extend_from_slice(&[0x81, 206, 0, 2, 0, 0, 0, 9]);
        c.extend_from_slice(&ours.to_be_bytes());
        // a NACK of 100, 101 and 116 (bits 0 and 15), then of 65535 and 0 across the wrap
        c.extend_from_slice(&[0x81, 205, 0, 4, 0, 0, 0, 9]);
        c.extend_from_slice(&ours.to_be_bytes());
        c.extend_from_slice(&[0, 100, 0x80, 0x01, 0xFF, 0xFF, 0, 1]);
        // a FIR for two SSRCs
        c.extend_from_slice(&[0x84, 206, 0, 6, 0, 0, 0, 9, 0, 0, 0, 0]);
        c.extend_from_slice(&[0, 0, 0, 7, 3, 0, 0, 0]);
        c.extend_from_slice(&[0, 0, 0, 8, 4, 0, 0, 0]);
        let got = feedback(&c);
        assert_eq!(
            got,
            [Feedback::Keyframe(vec![ours]), Feedback::Nack { media: ours, seqs: vec![100, 101, 116, 65535, 0] }, Feedback::Keyframe(vec![7, 8])]
        );
        // a REMB (an application-specific feedback) says nothing to us
        assert!(feedback(&[0x8F, 206, 0, 3, 0, 0, 0, 9, 0, 0, 0, 0, b'R', b'E', b'M', b'B']).is_empty());
    }

    #[test]
    fn hostile_rtcp_never_panics() {
        let mut seed = 0xC0FF_EE11u32;
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            seed
        };
        let nack = [0x81u8, 205, 0, 3, 0, 0, 0, 9, 0, 0, 0, 1, 0, 100, 0xFF, 0xFF];
        for n in 0..nack.len() {
            let _ = feedback(&nack[..n]);
        }
        for _ in 0..20_000 {
            let n = (next() % 80) as usize;
            let mut m: Vec<u8> = (0..n).map(|_| next() as u8).collect();
            if next() % 2 == 0 && m.len() >= 4 {
                m[0] = 0x80 | (m[0] & 0x1F);
                m[1] = [205, 206, 200, 201][next() as usize % 4];
            }
            let f = feedback(&m);
            let total: usize = f.iter().map(|f| if let Feedback::Nack { seqs, .. } = f { seqs.len() } else { 0 }).sum();
            assert!(total <= MAX_NACKED);
            let _ = split_annexb(&m);
        }
    }
}
