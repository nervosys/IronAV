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
    recvonly: bool,
    sendonly: bool,
}

/// Who posted the offer: a publisher, which sends to this end, or a viewer, which receives from it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Role {
    Ingest,
    Viewer,
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

/// Reads a publisher's offer, choosing what to take. Errors say what is wrong, for the 400 response.
pub(crate) fn parse_offer(text: &str) -> Result<Offer, String> {
    parse_offer_as(text, Role::Ingest)
}

/// The rank of an H.264 `profile-level-id` for a viewer: Constrained Baseline level 3.1 first, then
/// the other Constrained Baseline levels, then the rest.
fn profile_rank(fmtp: Option<&String>) -> u8 {
    let id = fmtp.map(|f| params(f)).and_then(|p| p.get("profile-level-id").cloned()).unwrap_or_default().to_ascii_lowercase();
    match id.as_str() {
        "42e01f" => 0,
        i if i.starts_with("42e0") => 1,
        _ => 2,
    }
}

/// Reads an offer from a publisher or a viewer. A viewer's offer takes H.264 video only, in a section
/// that is `recvonly` or `sendrecv`; its audio is declined.
pub(crate) fn parse_offer_as(text: &str, role: Role) -> Result<Offer, String> {
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
            "inactive" => s.inactive = true,
            "recvonly" => s.recvonly = true,
            "sendonly" => s.sendonly = true,
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
        Some(other) => return Err(format!("the offer has a=setup:{other}; {} must be the DTLS client (actpass or active)", if role == Role::Ingest { "WHIP clients" } else { "WHEP clients" })),
    }
    let mut out = Vec::new();
    let (mut video, mut audio) = (false, false);
    for (i, s) in sections.iter().enumerate() {
        let mid = s.mid.clone().unwrap_or_else(|| i.to_string());
        if mid.is_empty() || mid.len() > 32 || !mid.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c)) {
            return Err("an offer section has a malformed a=mid".into());
        }
        // a publisher's section must not be receive-only, a viewer's must not be send-only
        let direction_ok = !s.inactive && if role == Role::Ingest { !s.recvonly } else { !s.sendonly };
        let usable = !s.port_zero && direction_ok && s.proto.to_ascii_uppercase().contains("SAVP");
        let track = match s.kind.as_str() {
            "video" if usable && !video => s
                .formats
                .iter()
                .filter_map(|&pt| {
                    let map = s.rtpmap.get(&pt)?;
                    let fmtp = s.fmtp.get(&pt);
                    let mode = fmtp.map(|f| params(f)).and_then(|p| p.get("packetization-mode").cloned()).unwrap_or_else(|| "0".into());
                    (map.to_ascii_uppercase().starts_with("H264/90000") && mode == "1").then(|| Track { kind: Kind::Video, mid: mid.clone(), payload: pt, rtpmap: "H264/90000".into(), fmtp: fmtp.cloned(), clock: 90_000 })
                })
                // the first of the best rank (a publisher's offer is taken as it comes)
                .min_by_key(|t| if role == Role::Viewer { profile_rank(t.fmtp.as_ref()) } else { 0 }),
            "audio" if usable && !audio && role == Role::Ingest => s.formats.iter().find_map(|&pt| {
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
        return Err(match role {
            Role::Ingest => "the offer has no H.264 (packetization-mode=1) video or Opus audio",
            Role::Viewer => "the offer has no recvonly or sendrecv H.264 (packetization-mode=1) video",
        }
        .into());
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
    answer_with(offer, l, None)
}

/// The answer to a viewer's offer: the video taken, send-only with its SSRC, and the rest rejected.
pub(crate) fn viewer_answer(offer: &Offer, l: &Local, ssrc: u32) -> String {
    answer_with(offer, l, Some(ssrc))
}

fn answer_with(offer: &Offer, l: &Local, sending: Option<u32>) -> String {
    let mut s = String::new();
    let ip = l.candidate.ip();
    let family = if ip.is_ipv4() { "IP4" } else { "IP6" };
    let _ = write!(s, "v=0\r\no=- {} 2 IN {family} {ip}\r\ns=-\r\nt=0 0\r\n", l.session_id);
    let bundled: Vec<&str> = offer.tracks().map(|t| t.mid.as_str()).collect();
    let _ = write!(s, "a=group:BUNDLE {}\r\na=ice-lite\r\na=msid-semantic: WMS{}\r\n", bundled.join(" "), if sending.is_some() { " ironav" } else { "" });
    for (track, mid, rejected) in &offer.sections {
        let Some(t) = track else {
            let _ = write!(s, "m={rejected}\r\nc=IN {family} {ip}\r\na=mid:{mid}\r\na=inactive\r\n");
            continue;
        };
        let kind = match t.kind {
            Kind::Video => "video",
            Kind::Audio => "audio",
        };
        let direction = if sending.is_some() { "sendonly" } else { "recvonly" };
        let _ = write!(s, "m={kind} {} UDP/TLS/RTP/SAVPF {}\r\nc=IN {family} {ip}\r\na=mid:{}\r\na={direction}\r\na=rtcp-mux\r\n", l.candidate.port(), t.payload, t.mid);
        let _ = write!(s, "a=ice-ufrag:{}\r\na=ice-pwd:{}\r\na=fingerprint:sha-256 {}\r\na=setup:passive\r\n", l.ufrag, l.pwd, super::dtls::fingerprint_text(l.fingerprint));
        let _ = write!(s, "a=rtpmap:{} {}\r\n", t.payload, t.rtpmap);
        if let Some(f) = &t.fmtp {
            let _ = write!(s, "a=fmtp:{} {f}\r\n", t.payload);
        }
        if t.kind == Kind::Video {
            let _ = write!(s, "a=rtcp-fb:{0} nack\r\na=rtcp-fb:{0} nack pli\r\n", t.payload);
        }
        if let Some(ssrc) = sending {
            let _ = write!(s, "a=msid:ironav ironav-video\r\na=ssrc:{ssrc} cname:IronAV\r\n");
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

/// What a publisher's offer says about it: send-only video and audio, BUNDLEd on one transport, the
/// publisher the DTLS client or server as the answer decides (`a=setup:actpass`).
pub(crate) struct PubOffer<'a> {
    pub ufrag: &'a str,
    pub pwd: &'a str,
    pub fingerprint: &'a [u8; 32],
    pub session_id: u64,
    pub cname: &'a str,
    pub profile_level_id: &'a str,
    /// The video's payload type and SSRC, if it is offered.
    pub video: Option<(u8, u32)>,
    /// The audio's payload type and SSRC, if it is offered.
    pub audio: Option<(u8, u32)>,
}

/// The mid of the offer's video section and of its audio section.
pub(crate) const VIDEO_MID: &str = "0";
pub(crate) const AUDIO_MID: &str = "1";

/// The offer a WHIP publisher POSTs (RFC 9725 §4.2): H.264 with `packetization-mode=1` and Opus,
/// send-only, `rtcp-mux`, `nack`, `nack pli` and `ccm fir` for the video, and the publisher's
/// SSRCs, so the server can tell its streams apart.
pub(crate) fn offer(o: &PubOffer) -> String {
    let mut s = String::new();
    let mids: Vec<&str> = [o.video.map(|_| VIDEO_MID), o.audio.map(|_| AUDIO_MID)].into_iter().flatten().collect();
    let _ = write!(s, "v=0\r\no=- {} 2 IN IP4 127.0.0.1\r\ns=IronAV\r\nt=0 0\r\na=group:BUNDLE {}\r\na=msid-semantic: WMS ironav\r\n", o.session_id, mids.join(" "));
    let fp = super::dtls::fingerprint_text(o.fingerprint);
    let transport = |s: &mut String, mid: &str| {
        let _ = write!(s, "c=IN IP4 0.0.0.0\r\na=rtcp:9 IN IP4 0.0.0.0\r\na=ice-ufrag:{}\r\na=ice-pwd:{}\r\na=fingerprint:sha-256 {fp}\r\na=setup:actpass\r\na=mid:{mid}\r\na=sendonly\r\na=rtcp-mux\r\na=rtcp-rsize\r\n", o.ufrag, o.pwd);
    };
    if let Some((pt, ssrc)) = o.video {
        let _ = write!(s, "m=video 9 UDP/TLS/RTP/SAVPF {pt}\r\n");
        transport(&mut s, VIDEO_MID);
        let _ = write!(s, "a=rtpmap:{pt} H264/90000\r\na=rtcp-fb:{pt} nack\r\na=rtcp-fb:{pt} nack pli\r\na=rtcp-fb:{pt} ccm fir\r\n");
        let _ = write!(s, "a=fmtp:{pt} level-asymmetry-allowed=1;packetization-mode=1;profile-level-id={}\r\n", o.profile_level_id);
        let _ = write!(s, "a=ssrc:{ssrc} cname:{}\r\na=msid:ironav ironav-video\r\n", o.cname);
    }
    if let Some((pt, ssrc)) = o.audio {
        let _ = write!(s, "m=audio 9 UDP/TLS/RTP/SAVPF {pt}\r\n");
        transport(&mut s, AUDIO_MID);
        let _ = write!(s, "a=rtpmap:{pt} opus/48000/2\r\na=fmtp:{pt} minptime=10;useinbandfec=1\r\n");
        let _ = write!(s, "a=ssrc:{ssrc} cname:{}\r\na=msid:ironav ironav-audio\r\n", o.cname);
    }
    s
}

/// A media section of an answer.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct AnswerMedia {
    pub kind: Kind,
    pub mid: String,
    /// The payload type to send, or `None` when the section is rejected or took no codec offered.
    pub payload: Option<u8>,
}

/// What a WHIP server's answer says: its ICE credentials, the DTLS fingerprint its certificate must
/// have, where to send, and what it accepted.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Answer {
    pub ufrag: String,
    pub pwd: String,
    pub fingerprint: [u8; 32],
    /// UDP host and server-reflexive candidates (component 1), or the connection address and the media
    /// port if the answer lists none; at most [`MAX_CANDIDATES`], in the order given.
    pub candidates: Vec<std::net::SocketAddr>,
    pub media: Vec<AnswerMedia>,
    /// The DTLS role the answer took (RFC 5763 §5): `passive` makes the publisher the DTLS client,
    /// `active` makes it the DTLS server.
    pub setup: Setup,
}

/// The DTLS role an answer gives itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Setup {
    /// The answerer connects: the publisher is the DTLS server.
    Active,
    /// The answerer waits: the publisher is the DTLS client.
    Passive,
}

impl Answer {
    /// The payload type the answer took for `kind`.
    pub(crate) fn payload(&self, kind: Kind) -> Option<u8> {
        self.media.iter().find(|m| m.kind == kind).and_then(|m| m.payload)
    }
}

pub(crate) const MAX_CANDIDATES: usize = 8;

#[derive(Default)]
struct AnswerSection {
    kind: String,
    port: u16,
    proto: String,
    formats: Vec<u8>,
    mid: Option<String>,
    rtpmap: BTreeMap<u8, String>,
    fmtp: BTreeMap<u8, String>,
    ufrag: Option<String>,
    pwd: Option<String>,
    fingerprint: Option<[u8; 32]>,
    setup: Option<String>,
    connection: Option<std::net::IpAddr>,
    candidates: Vec<std::net::SocketAddr>,
    inactive: bool,
}

/// `a=candidate:` values: a UDP host or server-reflexive candidate of component 1 with an address (not a
/// name) that something could be sent to. Relayed and peer-reflexive ones, TCP ones and the second
/// component (a server that ignores `rtcp-mux` in its candidates, as Pion does) are left out.
/// Anything after the type (`raddr`, `rport`, `generation`, `network-cost`, ...) is not looked at.
fn parse_candidate(v: &str) -> Option<std::net::SocketAddr> {
    let w: Vec<&str> = v.split_whitespace().collect();
    let (component, transport, ip, port, typ) = (w.get(1)?, w.get(2)?, w.get(4)?, w.get(5)?, w.get(7)?);
    let ip: std::net::IpAddr = ip.parse().ok()?;
    let port: u16 = port.parse().ok()?;
    let good = *component == "1" && transport.eq_ignore_ascii_case("udp") && *w.get(6)? == "typ" && (typ.eq_ignore_ascii_case("host") || typ.eq_ignore_ascii_case("srflx"));
    (good && port != 0 && !ip.is_unspecified() && !ip.is_multicast()).then_some(std::net::SocketAddr::new(ip, port))
}

fn credential(v: Option<String>, what: &str, min: usize) -> Result<String, String> {
    let v = v.ok_or_else(|| format!("the answer has no a={what}"))?;
    if !(min..=256).contains(&v.len()) || !v.bytes().all(|b| b.is_ascii_graphic() && b != b':') {
        return Err(format!("the answer's a={what} is malformed"));
    }
    Ok(v)
}

/// Reads a server's answer to [`offer`]: what the server chose for each section, its ICE
/// credentials and fingerprint (SHA-256), its candidates and its DTLS role: `a=setup:active` (what an
/// answerer is told to prefer, and what Pion answers) or `passive`. `actpass` is the offerer's value
/// and is refused in an answer (RFC 5763 §5). An answer without any `a=setup` is taken as `passive`.
/// Attributes about trickle ICE (`ice-options`, `end-of-candidates`) are read past.
pub(crate) fn parse_answer(text: &str) -> Result<Answer, String> {
    if text.len() > MAX_OFFER {
        return Err("the answer is too long".into());
    }
    let mut session = AnswerSection::default();
    let mut sections: Vec<AnswerSection> = Vec::new();
    for (n, line) in text.lines().enumerate() {
        if n >= MAX_LINES {
            return Err("the answer has too many lines".into());
        }
        let line = line.trim_end_matches('\r');
        let Some((k, v)) = line.split_once('=') else { continue };
        match k {
            "m" => {
                if sections.len() == MAX_SECTIONS {
                    return Err(format!("the answer has more than {MAX_SECTIONS} media sections"));
                }
                let words: Vec<&str> = v.split_whitespace().collect();
                sections.push(AnswerSection {
                    kind: words.first().unwrap_or(&"").to_ascii_lowercase(),
                    port: words.get(1).and_then(|p| p.parse().ok()).unwrap_or(0),
                    proto: words.get(2).unwrap_or(&"").to_string(),
                    formats: words.iter().skip(3).filter_map(|p| p.parse::<u8>().ok()).take(64).collect(),
                    ..AnswerSection::default()
                });
            }
            "c" => {
                let s = sections.last_mut().unwrap_or(&mut session);
                s.connection = v.split_whitespace().nth(2).and_then(|a| a.parse().ok());
            }
            "a" => {
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
                    // a sha-256 one is wanted; others may come alongside it
                    "fingerprint" => s.fingerprint = parse_fingerprint(value).or(s.fingerprint),
                    "setup" => s.setup = Some(value.trim().to_ascii_lowercase()),
                    "candidate" => {
                        if let Some(c) = parse_candidate(value) {
                            if s.candidates.len() < MAX_CANDIDATES && !s.candidates.contains(&c) {
                                s.candidates.push(c);
                            }
                        }
                    }
                    "inactive" | "sendonly" => s.inactive = true,
                    _ => {}
                }
            }
            _ => {}
        }
    }
    if sections.is_empty() {
        return Err("the answer has no media sections".into());
    }
    // BUNDLE: the transport attributes of the first section that is in use, or the session's
    let live = sections.iter().find(|s| s.port != 0 && !s.inactive).ok_or("the answer rejects every media section")?;
    let ufrag = credential(live.ufrag.clone().or_else(|| session.ufrag.clone()), "ice-ufrag", 4)?;
    let pwd = credential(live.pwd.clone().or_else(|| session.pwd.clone()), "ice-pwd", 22)?;
    let fingerprint = live.fingerprint.or(session.fingerprint).ok_or("the answer has no sha-256 a=fingerprint")?;
    let setup = match live.setup.clone().or_else(|| session.setup.clone()).as_deref() {
        Some("passive") | None => Setup::Passive,
        Some("active") => Setup::Active,
        Some("actpass") => return Err("the answer has a=setup:actpass, which only an offer may say; an answer chooses active or passive".into()),
        Some(other) => return Err(format!("the answer has a=setup:{other}; it must be active or passive")),
    };
    let mut candidates: Vec<std::net::SocketAddr> = Vec::new();
    for c in sections.iter().chain(std::iter::once(&session)).flat_map(|s| s.candidates.iter()) {
        if candidates.len() < MAX_CANDIDATES && !candidates.contains(c) {
            candidates.push(*c);
        }
    }
    if candidates.is_empty() {
        // no candidates: the connection address and the port of the section in use, as RFC 8839 §5.1 has it
        let ip = live.connection.or(session.connection).filter(|ip| !ip.is_unspecified() && !ip.is_multicast());
        if let Some(ip) = ip.filter(|_| live.port != 0 && live.port != 9) {
            candidates.push(std::net::SocketAddr::new(ip, live.port));
        }
    }
    if candidates.is_empty() {
        return Err("the answer has no usable candidate (a UDP host candidate with an address)".into());
    }
    let mut media = Vec::new();
    for (i, s) in sections.iter().enumerate() {
        let kind = match s.kind.as_str() {
            "video" => Kind::Video,
            "audio" => Kind::Audio,
            _ => continue,
        };
        let mid = s.mid.clone().unwrap_or_else(|| i.to_string());
        let usable = s.port != 0 && !s.inactive && s.proto.to_ascii_uppercase().contains("SAVP");
        let payload = usable
            .then(|| {
                s.formats.iter().copied().find(|pt| {
                    let Some(map) = s.rtpmap.get(pt) else { return false };
                    match kind {
                        // an explicit packetization-mode of 0 would be single NAL units, which FU-A is not
                        Kind::Video => map.to_ascii_uppercase().starts_with("H264/90000") && s.fmtp.get(pt).and_then(|f| params(f).get("packetization-mode").cloned()).is_none_or(|m| m == "1"),
                        Kind::Audio => map.to_ascii_lowercase().starts_with("opus/48000"),
                    }
                })
            })
            .flatten();
        media.push(AnswerMedia { kind, mid, payload });
    }
    Ok(Answer { ufrag, pwd, fingerprint, candidates, media, setup })
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

    /// A viewer's offer: the browser's, turned around to `recvonly`.
    fn viewer_offer() -> String {
        BROWSER_OFFER.replace("a=sendonly", "a=recvonly")
    }

    #[test]
    fn a_viewers_offer_takes_the_video_it_receives_and_declines_audio() {
        let o = parse_offer_as(&viewer_offer(), Role::Viewer).unwrap();
        let t = o.tracks().collect::<Vec<_>>();
        assert_eq!(t.len(), 1);
        assert_eq!((t[0].kind, t[0].payload), (Kind::Video, 102));
        // the same offer, from a publisher, is one that cannot be sent to
        assert!(parse_offer(&viewer_offer()).unwrap_err().contains("H.264"));
        // a send-only section is not one a viewer can have, and send-receive is
        assert!(parse_offer_as(BROWSER_OFFER, Role::Viewer).unwrap_err().contains("recvonly"));
        let both = viewer_offer().replace("a=recvonly", "a=sendrecv");
        assert!(parse_offer_as(&both, Role::Viewer).is_ok());
        let a = viewer_answer(&o, &Local { ufrag: "u", pwd: "p", fingerprint: &[1; 32], candidate: "127.0.0.1:9".parse().unwrap(), session_id: 1 }, 77);
        assert!(a.contains("m=video 9 UDP/TLS/RTP/SAVPF 102\r\n") && a.contains("a=sendonly\r\n") && !a.contains("a=recvonly"), "{a}");
        assert!(a.contains("a=ssrc:77 cname:IronAV\r\n") && a.contains("a=group:BUNDLE 1\r\n"), "{a}");
        assert!(a.contains("m=audio 0 UDP/TLS/RTP/SAVPF 111 63 9 0 8\r\nc=IN IP4 127.0.0.1\r\na=mid:0\r\na=inactive\r\n"), "{a}");
    }

    #[test]
    fn a_viewer_gets_constrained_baseline_level_3_1_when_the_offer_has_several() {
        let offer = |ids: &[(&str, &str)]| {
            let mut s = String::from("v=0\r\na=ice-ufrag:FUZZ\r\na=ice-pwd:aBcDeFgHiJkLmNoPqRsTuVwX\r\na=fingerprint:sha-256 00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF\r\n");
            s += &format!("m=video 9 UDP/TLS/RTP/SAVPF {}\r\na=mid:v\r\na=recvonly\r\n", ids.iter().map(|i| i.0).collect::<Vec<_>>().join(" "));
            for (pt, id) in ids {
                s += &format!("a=rtpmap:{pt} H264/90000\r\na=fmtp:{pt} packetization-mode=1;profile-level-id={id}\r\n");
            }
            s
        };
        let pt = |ids: &[(&str, &str)]| parse_offer_as(&offer(ids), Role::Viewer).unwrap().tracks().next().unwrap().payload;
        assert_eq!(pt(&[("96", "640c1f"), ("97", "42e01f"), ("98", "42e01f")]), 97);
        assert_eq!(pt(&[("96", "640c1f"), ("97", "42e034"), ("98", "42001f")]), 97);
        assert_eq!(pt(&[("96", "640c1f"), ("97", "42001f")]), 96);
    }
}

#[cfg(test)]
mod publisher_tests {
    use super::*;

    fn pub_offer(video: bool, audio: bool) -> String {
        offer(&PubOffer { ufrag: "uF12", pwd: "0123456789abcdefghijkl", fingerprint: &[0xAB; 32], session_id: 5, cname: "ironav-test", profile_level_id: "42e01f", video: video.then_some((102, 0x1111)), audio: audio.then_some((111, 0x2222)) })
    }

    #[test]
    fn the_offer_is_what_a_whip_server_takes() {
        let text = pub_offer(true, true);
        for want in [
            "a=group:BUNDLE 0 1\r\n",
            "m=video 9 UDP/TLS/RTP/SAVPF 102\r\n",
            "m=audio 9 UDP/TLS/RTP/SAVPF 111\r\n",
            "a=setup:actpass\r\n",
            "a=sendonly\r\n",
            "a=rtcp-mux\r\n",
            "a=rtpmap:102 H264/90000\r\n",
            "a=fmtp:102 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r\n",
            "a=rtpmap:111 opus/48000/2\r\n",
            "a=ssrc:4369 cname:ironav-test\r\n",
        ] {
            assert!(text.contains(want), "{want:?} in {text}");
        }
        assert!(text.contains(&format!("a=fingerprint:sha-256 {}\r\n", "AB:".repeat(31) + "AB")));
        assert!(text.lines().all(|l| !l.contains('\n')) && text.ends_with("\r\n"));
        // this endpoint's own parser accepts it, and takes both tracks
        let parsed = parse_offer(&text).unwrap();
        let tracks: Vec<_> = parsed.tracks().map(|t| (t.kind, t.payload, t.mid.as_str())).collect();
        assert_eq!(tracks, [(Kind::Video, 102, VIDEO_MID), (Kind::Audio, 111, AUDIO_MID)]);
        assert_eq!(parsed.fingerprint, [0xAB; 32]);
        // one track alone is bundled alone
        let video = pub_offer(true, false);
        assert!(video.contains("a=group:BUNDLE 0\r\n") && !video.contains("m=audio"), "{video}");
        let audio = pub_offer(false, true);
        assert!(audio.contains("a=group:BUNDLE 1\r\n") && !audio.contains("m=video"), "{audio}");
    }

    #[test]
    fn this_endpoints_answer_is_read_back() {
        let parsed = parse_offer(&pub_offer(true, true)).unwrap();
        let a = answer(&parsed, &Local { ufrag: "srvU", pwd: "serverpasswordserverpw1", fingerprint: &[0xCD; 32], candidate: "192.0.2.7:4000".parse().unwrap(), session_id: 1 });
        let got = parse_answer(&a).unwrap();
        assert_eq!((got.ufrag.as_str(), got.pwd.as_str(), got.fingerprint), ("srvU", "serverpasswordserverpw1", [0xCD; 32]));
        assert_eq!(got.candidates, ["192.0.2.7:4000".parse::<std::net::SocketAddr>().unwrap()]);
        assert_eq!(got.payload(Kind::Video), Some(102));
        assert_eq!(got.payload(Kind::Audio), Some(111));
    }

    const ANSWER: &str = "v=0\r
o=- 1 2 IN IP4 203.0.113.9\r
s=-\r
t=0 0\r
a=group:BUNDLE 0 1\r
a=ice-ufrag:abcd\r
a=ice-pwd:abcdefghijklmnopqrstuv\r
a=fingerprint:sha-1 00:11\r
a=fingerprint:sha-256 00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF:00:11:22:33:44:55:66:77:88:99:AA:BB:CC:DD:EE:FF\r
a=setup:passive\r
m=video 50000 UDP/TLS/RTP/SAVPF 96 97\r
c=IN IP4 203.0.113.9\r
a=mid:0\r
a=recvonly\r
a=rtpmap:96 H264/90000\r
a=fmtp:96 packetization-mode=0\r
a=rtpmap:97 H264/90000\r
a=fmtp:97 packetization-mode=1;profile-level-id=42e01f\r
a=candidate:1 1 tcp 1 198.51.100.1 9 typ host tcptype passive\r
a=candidate:2 1 udp 2130706431 198.51.100.2 50001 typ host\r
a=candidate:3 1 udp 2130706431 abc.local 50002 typ host\r
a=candidate:4 1 udp 1 198.51.100.3 50003 typ relay raddr 0.0.0.0 rport 0\r
a=candidate:5 1 udp 2130706431 2001:db8::5 50004 typ host\r
m=audio 0 UDP/TLS/RTP/SAVPF 111\r
a=mid:1\r
a=inactive\r
";

    #[test]
    fn an_answer_is_read_for_what_it_chose() {
        let a = parse_answer(ANSWER).unwrap();
        // the UDP host candidates with addresses, in order; no tcp, name, or relay
        assert_eq!(a.candidates, ["198.51.100.2:50001".parse::<std::net::SocketAddr>().unwrap(), "[2001:db8::5]:50004".parse().unwrap()]);
        // the H.264 with packetization-mode=1 is chosen over the other; the audio section is rejected
        assert_eq!(a.media, [AnswerMedia { kind: Kind::Video, mid: "0".into(), payload: Some(97) }, AnswerMedia { kind: Kind::Audio, mid: "1".into(), payload: None }]);
        assert_eq!(a.fingerprint[1], 0x11);
        // without candidates, the connection address and port stand in
        let bare: String = ANSWER.lines().filter(|l| !l.starts_with("a=candidate")).map(|l| format!("{l}\r\n")).collect();
        assert_eq!(parse_answer(&bare).unwrap().candidates, ["203.0.113.9:50000".parse::<std::net::SocketAddr>().unwrap()]);
    }

    #[test]
    fn an_answer_may_be_active_or_passive_and_never_actpass() {
        assert_eq!(parse_answer(ANSWER).unwrap().setup, Setup::Passive);
        assert_eq!(parse_answer(&ANSWER.replace("a=setup:passive", "a=setup:active")).unwrap().setup, Setup::Active);
        // the section's value wins over the session's, and none at all is passive
        let in_section = ANSWER.replace("a=mid:0\r\n", "a=mid:0\r\na=setup:active\r\n");
        assert_eq!(parse_answer(&in_section).unwrap().setup, Setup::Active);
        let none: String = ANSWER.lines().filter(|l| !l.starts_with("a=setup")).map(|l| format!("{l}\r\n")).collect();
        assert_eq!(parse_answer(&none).unwrap().setup, Setup::Passive);
        let e = parse_answer(&ANSWER.replace("a=setup:passive", "a=setup:actpass")).unwrap_err();
        assert!(e.contains("actpass") && e.contains("offer"), "{e}");
    }

    /// What MediaMTX v1.21.3 (Pion) answered a publisher's offer with, as it came: active, one host
    /// candidate listed for both components, `end-of-candidates`, no ICE-lite.
    const MEDIAMTX_ANSWER: &str = "v=0\r
o=- 2030033224607173941 1793807081 IN IP4 0.0.0.0\r
s=-\r
t=0 0\r
a=msid-semantic:WMS *\r
a=fingerprint:sha-256 BB:18:A8:BF:5D:0A:BB:D8:AE:88:6A:CE:34:92:E0:44:E6:23:1B:8A:FA:B2:4C:6F:89:D2:08:07:DD:C8:18:5F\r
a=group:BUNDLE 0 1\r
m=video 9 UDP/TLS/RTP/SAVPF 102\r
c=IN IP4 0.0.0.0\r
a=setup:active\r
a=mid:0\r
a=ice-ufrag:vRcVOBnQdslKPfhL\r
a=ice-pwd:rYbCsQmMdtQjQOgRSEMjjTqanYeYUGuI\r
a=rtcp-mux\r
a=rtcp-rsize\r
a=rtpmap:102 H264/90000\r
a=fmtp:102 level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f\r
a=rtcp-fb:102 nack\r
a=rtcp-fb:102 nack pli\r
a=recvonly\r
a=candidate:1502218671 1 udp 2130706431 127.0.0.1 8189 typ host\r
a=candidate:1502218671 2 udp 2130706431 127.0.0.1 8189 typ host\r
a=end-of-candidates\r
m=audio 9 UDP/TLS/RTP/SAVPF 111\r
c=IN IP4 0.0.0.0\r
a=setup:active\r
a=mid:1\r
a=ice-ufrag:vRcVOBnQdslKPfhL\r
a=ice-pwd:rYbCsQmMdtQjQOgRSEMjjTqanYeYUGuI\r
a=rtcp-mux\r
a=rtcp-rsize\r
a=rtpmap:111 opus/48000/2\r
a=fmtp:111 minptime=10;useinbandfec=1\r
a=recvonly\r
";

    #[test]
    fn mediamtxs_answer_is_read() {
        let a = parse_answer(MEDIAMTX_ANSWER).unwrap();
        assert_eq!(a.setup, Setup::Active);
        assert_eq!(a.candidates, ["127.0.0.1:8189".parse::<std::net::SocketAddr>().unwrap()], "the component 2 duplicate is left out");
        assert_eq!((a.payload(Kind::Video), a.payload(Kind::Audio)), (Some(102), Some(111)));
        assert_eq!(a.ufrag, "vRcVOBnQdslKPfhL");
        assert_eq!(a.fingerprint[0], 0xBB);
    }

    #[test]
    fn several_candidates_of_several_types_are_sorted_out() {
        let text = ANSWER.replace("a=candidate:4 1 udp 1 198.51.100.3 50003 typ relay raddr 0.0.0.0 rport 0\r\n", "a=candidate:4 1 udp 1 198.51.100.3 50003 typ relay raddr 0.0.0.0 rport 0\r\na=candidate:6 1 udp 1694498815 203.0.113.50 50006 typ srflx raddr 10.0.0.2 rport 50006 generation 0 network-cost 10\r\na=candidate:7 1 udp 1 203.0.113.51 50007 typ prflx\r\na=ice-options:trickle\r\na=end-of-candidates\r\n");
        let a = parse_answer(&text).unwrap();
        assert_eq!(a.candidates, ["198.51.100.2:50001".parse::<std::net::SocketAddr>().unwrap(), "203.0.113.50:50006".parse().unwrap(), "[2001:db8::5]:50004".parse().unwrap()]);
    }

    #[test]
    fn an_unusable_answer_is_refused_with_a_reason() {
        for (from, to, why) in [
            ("a=setup:passive", "a=setup:actpass", "actpass"),
            ("a=setup:passive", "a=setup:holdconn", "setup"),
            ("a=ice-pwd:abcdefghijklmnopqrstuv", "a=ice-pwd:short", "ice-pwd"),
            ("a=ice-ufrag:abcd", "a=ice-ufrag:ab:cd", "ice-ufrag"),
            ("sha-256 00:11", "sha-512 00:11", "fingerprint"),
            ("m=video 50000", "m=video 0", "rejects"),
        ] {
            let e = parse_answer(&ANSWER.replace(from, to)).unwrap_err();
            assert!(e.contains(why), "{e}");
        }
        let no_media = "v=0\r\na=ice-ufrag:abcd\r\n";
        assert!(parse_answer(no_media).is_err());
        assert!(parse_answer(&"a".repeat(MAX_OFFER + 1)).is_err());
        // truncated anywhere, or with a byte changed, it fails or parses, but never panics
        for n in 0..ANSWER.len() {
            if ANSWER.is_char_boundary(n) {
                let _ = parse_answer(&ANSWER[..n]);
            }
            let mut b = ANSWER.as_bytes().to_vec();
            b[n] = b'=';
            let _ = parse_answer(&String::from_utf8_lossy(&b));
        }
    }
}
