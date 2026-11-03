//! The SDP offer a WHIP client posts (RFC 8829's JSEP, as RFC 9725 §4.2 uses it), and the answer:
//! receive-only, ICE-lite with one host candidate, `rtcp-mux`, everything BUNDLEd on one port, the
//! endpoint's DTLS fingerprint and `setup:passive` (the client is the DTLS client). Per media
//! section one codec is taken, H.264 with `packetization-mode=1` for video and Opus for audio, and
//! every other section is rejected (port 0), as is any further video or audio section.

use std::collections::BTreeMap;
use std::fmt::Write;

/// The longest offer read, and the most media sections and lines taken from it.
pub(crate) const MAX_OFFER: usize = 64 << 10;
const MAX_SECTIONS: usize = 16;
const MAX_LINES: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Video,
    Audio,
}

/// A section of the offer taken: the codec chosen, and how the answer and the RTP stream's
/// description name it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Track {
    pub kind: Kind,
    pub mid: String,
    pub payload: u8,
    /// `H264/90000` or `opus/48000/2`.
    pub rtpmap: String,
    pub fmtp: Option<String>,
    pub clock: u32,
}

/// An offer section: what it says about itself.
#[derive(Debug, Default, Clone)]
struct Section {
    kind: String,
    /// The `m=` line with its port 0, as a rejection repeats it.
    rejected: String,
    port_zero: bool,
    proto: String,
    formats: Vec<u8>,
    mid: Option<String>,
    rtpmap: BTreeMap<u8, String>,
    fmtp: BTreeMap<u8, String>,
    ufrag: Option<String>,
    pwd: Option<String>,
    fingerprint: Option<String>,
    setup: Option<String>,
    inactive: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Offer {
    pub ufrag: String,
    pub pwd: String,
    /// The SHA-256 fingerprint of the client's DTLS certificate.
    pub fingerprint: [u8; 32],
    /// Every section in order: the track taken from it, or `None` (rejected), with its mid and
    /// the `m=` line that rejects it.
    pub sections: Vec<(Option<Track>, String, String)>,
}

impl Offer {
    pub(crate) fn tracks(&self) -> impl Iterator<Item = &Track> {
        self.sections.iter().filter_map(|s| s.0.as_ref())
    }
}

/// `key=value;...` parameters, keys in lower case.
fn params(fmtp: &str) -> BTreeMap<String, String> {
    fmtp.split(';').filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_string())).collect()
}

fn ice_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '+' || c == '/'
}

fn parse_fingerprint(v: &str) -> Option<[u8; 32]> {
    let (alg, hex) = v.trim().split_once(' ')?;
    if !alg.eq_ignore_ascii_case("sha-256") {
        return None;
    }
    let bytes: Vec<u8> = hex.trim().split(':').map(|h| u8::from_str_radix(h, 16).ok().filter(|_| h.len() == 2)).collect::<Option<_>>()?;
    bytes.try_into().ok()
}

/// Reads an offer, choosing what to take. Errors say what is wrong, for the 400 response.
pub(crate) fn parse_offer(text: &str) -> Result<Offer, String> {
    if text.len() > MAX_OFFER {
        return Err("the offer is too long".into());
    }
    let mut session = Section::default();
    let mut sections: Vec<Section> = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if n >= MAX_LINES {
            return Err("the offer has too many lines".into());
        }
        let line = line.trim_end_matches('\r');
        let Some((k, v)) = line.split_once('=') else { continue };
        if k == "m" {
            if sections.len() == MAX_SECTIONS {
                return Err(format!("the offer has more than {MAX_SECTIONS} media sections"));
            }
            let words: Vec<&str> = v.split_whitespace().collect();
            let kind = words.first().unwrap_or(&"").to_ascii_lowercase();
            let port_zero = words.get(1).is_some_and(|p| *p == "0");
            let proto = words.get(2).unwrap_or(&"").to_string();
            let formats = words.iter().skip(3).filter_map(|p| p.parse::<u8>().ok()).take(64).collect();
            // a rejected section keeps its media, protocol and formats (RFC 8829 §5.3.1)
            let rejected = if words.len() >= 4 && v.len() <= 256 && v.bytes().all(|b| b.is_ascii_graphic() || b == b' ') {
                format!("{} 0 {}", words[0], words[2..].join(" "))
            } else {
                format!("{} 0 UDP/TLS/RTP/SAVPF 0", if !kind.is_empty() && kind.bytes().all(|b| b.is_ascii_alphanumeric()) { kind.as_str() } else { "application" })
            };
            sections.push(Section { kind, rejected, port_zero, proto, formats, ..Section::default() });
            continue;
        }
        if k != "a" {
            continue;
        }
        let s = sections.last_mut().unwrap_or(&mut session);
        let (name, value) = v.split_once(':').unwrap_or((v, ""));
        let for_pt = |value: &str| -> Option<(u8, String)> {
            let (pt, rest) = value.split_once(char::is_whitespace)?;
            Some((pt.parse().ok()?, rest.trim().to_string()))
        };
        match name {
            "mid" => s.mid = Some(value.trim().to_string()),
            "rtpmap" => {
                if let Some((pt, m)) = for_pt(value) {
                    s.rtpmap.insert(pt, m);
                }
            }
            "fmtp" => {
                if let Some((pt, m)) = for_pt(value) {
                    s.fmtp.insert(pt, m);
                }
            }
            "ice-ufrag" => s.ufrag = Some(value.trim().to_string()),
            "ice-pwd" => s.pwd = Some(value.trim().to_string()),
            "fingerprint" => {
                // a sha-256 one is wanted; others (sha-384, ...) may come alongside it
                if parse_fingerprint(value).is_some() || s.fingerprint.is_none() {
                    s.fingerprint = Some(value.trim().to_string());
                }
            }
            "setup" => s.setup = Some(value.trim().to_ascii_lowercase()),
            "inactive" | "recvonly" => s.inactive = true,
            _ => {}
        }
    }
    if sections.is_empty() {
        return Err("the offer has no media sections".into());
    }
    // the transport attributes of the first section, or the session's: BUNDLE gives every section one transport
    let first = &sections[0];
    let pick = |f: fn(&Section) -> &Option<String>| f(first).clone().or_else(|| f(&session).clone());
    let ufrag = pick(|s| &s.ufrag).ok_or("the offer has no a=ice-ufrag")?;
    let pwd = pick(|s| &s.pwd).ok_or("the offer has no a=ice-pwd")?;
    if !(4..=256).contains(&ufrag.len()) || !ufrag.chars().all(ice_char) || !(22..=256).contains(&pwd.len()) || !pwd.chars().all(ice_char) {
        return Err("the offer's ICE credentials are malformed".into());
    }
    let fingerprint = pick(|s| &s.fingerprint).and_then(|f| parse_fingerprint(&f)).ok_or("the offer has no sha-256 a=fingerprint")?;
    match pick(|s| &s.setup).as_deref() {
        Some("actpass") | Some("active") | None => {}
        Some(other) => return Err(format!("the offer has a=setup:{other}; WHIP clients must be the DTLS client (actpass or active)")),
    }
    let mut out = Vec::new();
    let (mut video, mut audio) = (false, false);
    for (i, s) in sections.iter().enumerate() {
        let mid = s.mid.clone().unwrap_or_else(|| i.to_string());
        if mid.is_empty() || mid.len() > 32 || !mid.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) {
            return Err("an offer section has a malformed a=mid".into());
        }
        let usable = !s.port_zero && !s.inactive && s.proto.to_ascii_uppercase().contains("SAVP");
        let track = match s.kind.as_str() {
            "video" if usable && !video => s.formats.iter().find_map(|&pt| {
                let map = s.rtpmap.get(&pt)?;
                let fmtp = s.fmtp.get(&pt);
                let mode = fmtp.map(|f| params(f)).and_then(|p| p.get("packetization-mode").cloned()).unwrap_or_else(|| "0".into());
                (map.to_ascii_uppercase().starts_with("H264/90000") && mode == "1").then(|| Track { kind: Kind::Video, mid: mid.clone(), payload: pt, rtpmap: "H264/90000".into(), fmtp: fmtp.cloned(), clock: 90_000 })
            }),
            "audio" if usable && !audio => s.formats.iter().find_map(|&pt| {
                let map = s.rtpmap.get(&pt)?;
                map.to_ascii_lowercase().starts_with("opus/48000").then(|| Track { kind: Kind::Audio, mid: mid.clone(), payload: pt, rtpmap: "opus/48000/2".into(), fmtp: s.fmtp.get(&pt).cloned(), clock: 48_000 })
            }),
            _ => None,
        };
        match track.as_ref().map(|t| t.kind) {
            Some(Kind::Video) => video = true,
            Some(Kind::Audio) => audio = true,
            None => {}
        }
        out.push((track, mid, s.rejected.clone()));
    }
    // fmtp values pass on into the answer, so nothing that could break a line
    for t in out.iter().filter_map(|s| s.0.as_ref()) {
        if t.fmtp.as_ref().is_some_and(|f| f.len() > 512 || f.chars().any(|c| c.is_control())) {
            return Err("an offer fmtp line is malformed".into());
        }
    }
    let offer = Offer { ufrag, pwd, fingerprint, sections: out };
    if offer.tracks().next().is_none() {
        return Err("the offer has no H.264 (packetization-mode=1) video or Opus audio".into());
    }
    if offer.tracks().any(|t| offer.tracks().filter(|u| u.mid == t.mid).count() > 1) {
        return Err("two offer sections share a mid".into());
    }
    Ok(offer)
}

/// What the answer says about this end.
pub(crate) struct Local<'a> {
    pub ufrag: &'a str,
    pub pwd: &'a str,
    pub fingerprint: &'a [u8; 32],
    pub candidate: std::net::SocketAddr,
    pub session_id: u64,
}

/// The answer to `offer`: the tracks taken, receive-only, and the rest rejected.
pub(crate) fn answer(offer: &Offer, l: &Local) -> String {
    let mut s = String::new();
    let ip = l.candidate.ip();
    let family = if ip.is_ipv4() { "IP4" } else { "IP6" };
    let _ = write!(s, "v=0\r\no=- {} 2 IN {family} {ip}\r\ns=-\r\nt=0 0\r\n", l.session_id);
    let bundled: Vec<&str> = offer.tracks().map(|t| t.mid.as_str()).collect();
    let _ = write!(s, "a=group:BUNDLE {}\r\na=ice-lite\r\na=msid-semantic: WMS\r\n", bundled.join(" "));
    for (track, mid, rejected) in &offer.sections {
        let Some(t) = track else {
            let _ = write!(s, "m={rejected}\r\nc=IN {family} {ip}\r\na=mid:{mid}\r\na=inactive\r\n");
            continue;
        };
        let kind = match t.kind {
            Kind::Video => "video",
            Kind::Audio => "audio",
        };
        let _ = write!(s, "m={kind} {} UDP/TLS/RTP/SAVPF {}\r\nc=IN {family} {ip}\r\na=mid:{}\r\na=recvonly\r\na=rtcp-mux\r\n", l.candidate.port(), t.payload, t.mid);
        let _ = write!(s, "a=ice-ufrag:{}\r\na=ice-pwd:{}\r\na=fingerprint:sha-256 {}\r\na=setup:passive\r\n", l.ufrag, l.pwd, super::dtls::fingerprint_text(l.fingerprint));
        let _ = write!(s, "a=rtpmap:{} {}\r\n", t.payload, t.rtpmap);
        if let Some(f) = &t.fmtp {
            let _ = write!(s, "a=fmtp:{} {f}\r\n", t.payload);
        }
        if t.kind == Kind::Video {
            let _ = write!(s, "a=rtcp-fb:{0} nack\r\na=rtcp-fb:{0} nack pli\r\n", t.payload);
        }
        let _ = write!(s, "a=candidate:1 1 udp 2130706431 {ip} {} typ host\r\na=end-of-candidates\r\n", l.candidate.port());
    }
    s
}

/// The session description the RTP stream reader gets: one section per track, in the answer's
/// order, each with its one payload type.
pub(crate) fn description(offer: &Offer) -> String {
    let mut s = String::from("v=0\r\no=- 0 0 IN IP4 0.0.0.0\r\ns=WHIP\r\nt=0 0\r\n");
    for t in offer.tracks() {
        let kind = if t.kind == Kind::Video { "video" } else { "audio" };
        let _ = write!(s, "m={kind} 0 RTP/AVP {0}\r\na=rtpmap:{0} {1}\r\n", t.payload, t.rtpmap);
        if let Some(f) = &t.fmtp {
            let _ = write!(s, "a=fmtp:{} {f}\r\n", t.payload);
        }
    }
    s
}

#[cfg(test)]
pub(crate) const BROWSER_OFFER: &str = "v=0\r
o=- 4611731400430051336 2 IN IP4 127.0.0.1\r
s=-\r
t=0 0\r
a=group:BUNDLE 0 1 2\r
a=msid-semantic: WMS\r
m=audio 9 UDP/TLS/RTP/SAVPF 111 63 9 0 8\r
c=IN IP4 0.0.0.0\r
a=ice-ufrag:FUZZ\r
a=ice-pwd:aBcDeFgHiJkLmNoPqRsTuVwX\r
a=ice-options:trickle\r
a=fingerprint:sha-256 00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF\r
a=setup:actpass\r
a=mid:0\r
a=sendonly\r
a=rtcp-mux\r
a=rtpmap:111 opus/48000/2\r
a=fmtp:111 minptime=10;useinbandfec=1\r
a=rtpmap:63 red/48000/2\r
a=rtpmap:9 G722/8000\r
m=video 9 UDP/TLS/RTP/SAVPF 96 97 102 103\r
c=IN IP4 0.0.0.0\r
a=mid:1\r
a=sendonly\r
a=rtcp-mux\r
a=rtpmap:96 VP8/90000\r
a=rtpmap:97 rtx/90000\r
a=fmtp:97 apt=96\r
a=rtpmap:102 H264/90000\r
a=fmtp:102 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f\r
a=rtpmap:103 H264/90000\r
a=fmtp:103 level-asymmetry-allowed=1;packetization-mode=0;profile-level-id=42001f\r
m=application 9 UDP/DTLS/SCTP webrtc-datachannel\r
c=IN IP4 0.0.0.0\r
a=mid:2\r
";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_browser_offer_is_answered() {
        let o = parse_offer(BROWSER_OFFER).unwrap();
        assert_eq!((o.ufrag.as_str(), o.pwd.as_str()), ("FUZZ", "aBcDeFgHiJkLmNoPqRsTuVwX"));
        assert_eq!(o.fingerprint[1], 0x11);
        let tracks: Vec<_> = o.tracks().map(|t| (t.kind, t.payload, t.mid.as_str())).collect();
        // VP8 is passed over for H.264 in packetization mode 1; the data channel is rejected
        assert_eq!(tracks, [(Kind::Audio, 111, "0"), (Kind::Video, 102, "1")]);
        assert!(o.sections[2].0.is_none());
        let a = answer(&o, &Local { ufrag: "abcd", pwd: "0123456789abcdefghijklmn", fingerprint: &[0xAB; 32], candidate: "127.0.0.1:8080".parse().unwrap(), session_id: 7 });
        assert!(a.contains("a=group:BUNDLE 0 1\r\n") && a.contains("a=ice-lite") && a.contains("a=setup:passive"), "{a}");
        assert!(a.contains("m=audio 8080 UDP/TLS/RTP/SAVPF 111\r\n") && a.contains("m=video 8080 UDP/TLS/RTP/SAVPF 102\r\n"), "{a}");
        assert!(a.contains("m=application 0 UDP/DTLS/SCTP webrtc-datachannel\r\n") && a.contains("a=mid:2\r\n"), "{a}");
        assert!(a.contains("a=candidate:1 1 udp 2130706431 127.0.0.1 8080 typ host"), "{a}");
        assert!(a.contains("a=fmtp:102 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42001f"));
        let d = description(&o);
        let sdp = ironav_formats::rtp::Sdp::parse(&d).unwrap();
        assert_eq!(sdp.media.len(), 2);
        assert_eq!(sdp.media[0].codec(), Ok(ironav_core::CodecId::Opus));
        assert_eq!(sdp.media[1].codec(), Ok(ironav_core::CodecId::H264));
    }

    #[test]
    fn unusable_offers_are_refused_with_a_reason() {
        let no_codec = BROWSER_OFFER.replace("opus/48000/2", "ISAC/16000").replace("packetization-mode=1", "packetization-mode=0");
        assert!(parse_offer(&no_codec).unwrap_err().contains("H.264"));
        assert!(parse_offer(&BROWSER_OFFER.replace("a=setup:actpass", "a=setup:passive")).unwrap_err().contains("setup"));
        assert!(parse_offer(&BROWSER_OFFER.replace("sha-256", "sha-1")).unwrap_err().contains("fingerprint"));
        assert!(parse_offer(&BROWSER_OFFER.replace("a=ice-pwd:aBcDeFgHiJkLmNoPqRsTuVwX", "a=ice-pwd:short")).is_err());
        assert!(parse_offer(&BROWSER_OFFER.replace("a=mid:1", "a=mid:0")).is_err());
        assert!(parse_offer(&BROWSER_OFFER.replace("minptime=10", "minptime=10\u{1}")).is_err());
        assert!(parse_offer("v=0\r\n").is_err());
        let many: String = BROWSER_OFFER.to_string() + &"m=audio 9 UDP/TLS/RTP/SAVPF 0\r\n".repeat(20);
        assert!(parse_offer(&many).is_err());
        // truncated anywhere, it fails or parses, but never panics
        for n in 0..BROWSER_OFFER.len() {
            if BROWSER_OFFER.is_char_boundary(n) {
                let _ = parse_offer(&BROWSER_OFFER[..n]);
            }
        }
    }
}
