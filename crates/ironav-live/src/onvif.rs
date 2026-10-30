//! ONVIF cameras: finding them on the local network (WS-Discovery) and asking one for its media
//! profiles with the RTSP and snapshot URI of each (Profiles S and T), so an agent can open a camera
//! without knowing its vendor's URL scheme.
//!
//! Requests are SOAP 1.2 over the HTTP client, so every connection goes through the address
//! allow-list. Credentials go as a WS-Security UsernameToken with a password digest, its creation
//! time taken from the camera's own clock (cameras refuse tokens from a clock that disagrees with
//! theirs). Answers are read by element name, without namespaces: enough for the handful of
//! elements these calls need, not a general XML parser.

use crate::http::{self, base64, HttpUrl};
use crate::net::Allow;
use crate::sha1::sha1;
use crate::url::redact;
use ironav_core::{Error, ErrorCode, Result};
use serde::Serialize;
use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Where WS-Discovery probes go.
pub const DISCOVERY: &str = "239.255.255.250:3702";

/// The most of an answer read.
const MAX_ANSWER: usize = 4 << 20;

const DEVICE_NS: &str = "http://www.onvif.org/ver10/device/wsdl";
const MEDIA_NS: &str = "http://www.onvif.org/ver10/media/wsdl";
const MEDIA2_NS: &str = "http://www.onvif.org/ver20/media/wsdl";

/// A device that answered a probe.
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Discovered {
    /// Its endpoint reference, stable across restarts (usually `urn:uuid:...`).
    pub endpoint: String,
    /// Its device service URLs, for [`profiles`].
    pub xaddrs: Vec<String>,
    /// What it says it is: `onvif://www.onvif.org/name/...`, `.../hardware/...`, `.../location/...`.
    pub scopes: Vec<String>,
    pub types: Vec<String>,
    /// The address the answer came from.
    pub from: String,
}

/// What a camera is and what it streams.
#[derive(Debug, Clone, Serialize, PartialEq, Default)]
pub struct Device {
    /// The device service, as asked.
    pub url: String,
    pub manufacturer: Option<String>,
    pub model: Option<String>,
    pub firmware: Option<String>,
    pub serial: Option<String>,
    /// The media service the profiles came from, and its version (1 or 2).
    pub media: String,
    pub media_version: u8,
    pub profiles: Vec<Profile>,
}

/// One of a camera's media profiles: a stream it offers.
#[derive(Debug, Clone, Serialize, PartialEq, Default)]
pub struct Profile {
    pub token: String,
    pub name: String,
    /// The video encoding (`H264`, `H265`, `JPEG`, `MPEG4`) and its picture.
    pub video: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<f64>,
    pub bitrate_kbps: Option<u32>,
    /// The audio encoding (`G711`, `AAC`, ...) and its rate.
    pub audio: Option<String>,
    pub sample_rate: Option<u32>,
    /// The RTSP URI of the stream, as the camera gives it (without credentials).
    pub stream_uri: Option<String>,
    /// An HTTP URI that answers one JPEG.
    pub snapshot_uri: Option<String>,
    /// Why a URI is missing, when the camera refused to give it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Sends a WS-Discovery probe for video transmitters to `target` (normally [`DISCOVERY`]) and
/// collects the answers that come within `wait`, one per device.
pub fn discover(target: &str, wait: Duration) -> Result<Vec<Discovered>> {
    let target: SocketAddr = target.parse().map_err(|_| Error::invalid_arg(format!("'{target}' is not an address and port")))?;
    let sock = UdpSocket::bind(if target.is_ipv4() { "0.0.0.0:0" } else { "[::]:0" })?;
    let id = format!("uuid:{}", uuid());
    let probe = format!(
        r#"<?xml version="1.0" encoding="UTF-8"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope" xmlns:a="http://schemas.xmlsoap.org/ws/2004/08/addressing" xmlns:d="http://schemas.xmlsoap.org/ws/2005/04/discovery" xmlns:dn="http://www.onvif.org/ver10/network/wsdl"><s:Header><a:MessageID>{id}</a:MessageID><a:To s:mustUnderstand="1">urn:schemas-xmlsoap-org:ws:2005:04:discovery</a:To><a:Action s:mustUnderstand="1">http://schemas.xmlsoap.org/ws/2005/04/discovery/Probe</a:Action></s:Header><s:Body><d:Probe><d:Types>dn:NetworkVideoTransmitter</d:Types></d:Probe></s:Body></s:Envelope>"#
    );
    // UDP may lose one; devices answer each probe, and duplicates are merged
    sock.send_to(probe.as_bytes(), target)?;
    let deadline = Instant::now() + wait;
    let mut resent = false;
    let mut found: Vec<Discovered> = Vec::new();
    let mut buf = vec![0u8; 65536];
    loop {
        let now = Instant::now();
        if now >= deadline {
            break;
        }
        if !resent && deadline - now < wait / 2 {
            resent = true;
            let _ = sock.send_to(probe.as_bytes(), target);
        }
        sock.set_read_timeout(Some((deadline - now).min(wait / 2).max(Duration::from_millis(1))))?;
        let (n, from) = match sock.recv_from(&mut buf) {
            Ok(r) => r,
            Err(e) if matches!(e.kind(), std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut | std::io::ErrorKind::ConnectionReset) => continue,
            Err(e) => return Err(e.into()),
        };
        let xml = String::from_utf8_lossy(&buf[..n]);
        if first(&xml, "RelatesTo").as_deref() != Some(id.as_str()) {
            continue;
        }
        for (_, m) in elements(&xml, "ProbeMatch") {
            let list = |name: &str| first(m, name).map(|s| s.split_whitespace().map(String::from).collect::<Vec<_>>()).unwrap_or_default();
            let d = Discovered { endpoint: first(m, "Address").unwrap_or_default(), xaddrs: list("XAddrs"), scopes: list("Scopes"), types: list("Types"), from: from.ip().to_string() };
            match found.iter_mut().find(|f| f.endpoint == d.endpoint && !d.endpoint.is_empty()) {
                Some(f) => {
                    for x in d.xaddrs {
                        if !f.xaddrs.contains(&x) {
                            f.xaddrs.push(x);
                        }
                    }
                }
                None => found.push(d),
            }
        }
    }
    Ok(found)
}

/// Asks the camera whose device service is at `url` (`http://HOST` alone means the usual
/// `/onvif/device_service`) what it is and what it streams. Credentials come from `user` and
/// `password`, or from the URL.
pub fn profiles(url: &str, user: Option<&str>, password: Option<&str>, allow: &Allow) -> Result<Device> {
    let mut u = HttpUrl::parse(url)?;
    if u.path == "/" {
        u.path = "/onvif/device_service".into();
    }
    let cred = match (user, u.user.take()) {
        (Some(user), _) => Some((user.to_string(), password.unwrap_or("").to_string())),
        (None, from_url) => from_url,
    };
    let base = u.plain();
    let mut c = Client { cred, allow, skew: 0 };
    // the camera's clock, for the token (asked without one, as the standard allows)
    if let Ok(xml) = c.call(&base, DEVICE_NS, "GetSystemDateAndTime", "", false) {
        if let Some(utc) = first_raw(&xml, "UTCDateTime") {
            let num = |name: &str| first(utc, name).and_then(|s| s.parse::<i64>().ok());
            if let (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(s)) = (num("Year"), num("Month"), num("Day"), num("Hour"), num("Minute"), num("Second")) {
                c.skew = unix(y, mo, d, h, mi, s) - now_secs();
            }
        }
    }
    let mut dev = Device { url: base.clone(), ..Device::default() };
    if let Ok(xml) = c.call(&base, DEVICE_NS, "GetDeviceInformation", "", true) {
        dev.manufacturer = first(&xml, "Manufacturer");
        dev.model = first(&xml, "Model");
        dev.firmware = first(&xml, "FirmwareVersion");
        dev.serial = first(&xml, "SerialNumber");
    }
    // the media services: from GetServices, or from GetCapabilities on older devices
    let (mut media1, mut media2) = (None, None);
    match c.call(&base, DEVICE_NS, "GetServices", "<IncludeCapability>false</IncludeCapability>", true) {
        Ok(xml) => {
            for (_, s) in elements(&xml, "Service") {
                let (ns, addr) = (first(s, "Namespace").unwrap_or_default(), first(s, "XAddr"));
                if ns == MEDIA_NS {
                    media1 = addr;
                } else if ns == MEDIA2_NS {
                    media2 = addr;
                }
            }
        }
        Err(e) if e.code == ErrorCode::InvalidArgument => return Err(e),
        Err(_) => {}
    }
    if media1.is_none() && media2.is_none() {
        let xml = c.call(&base, DEVICE_NS, "GetCapabilities", "<Category>Media</Category>", true)?;
        media1 = first_raw(&xml, "Media").and_then(|m| first(m, "XAddr"));
    }
    let (media, version) = match (media1, media2) {
        (Some(m), _) => (m, 1),
        (None, Some(m)) => (m, 2),
        (None, None) => return Err(Error::new(ErrorCode::NotFound, format!("{} has no media service", redact(&base))).hint("it may not be an ONVIF video device")),
    };
    let media = local(&u, &media);
    let ns = if version == 1 { MEDIA_NS } else { MEDIA2_NS };
    let xml = c.call(&media, ns, "GetProfiles", if version == 1 { "" } else { "<Type>All</Type>" }, true)?;
    for (attrs, p) in elements(&xml, "Profiles") {
        let mut prof = Profile { token: attr(attrs, "token").unwrap_or_default(), name: first(p, "Name").unwrap_or_default(), ..Profile::default() };
        if let Some(v) = first_raw(p, "VideoEncoderConfiguration").or_else(|| first_raw(p, "VideoEncoder")) {
            let num = |name: &str| first(v, name).and_then(|s| s.parse::<f64>().ok());
            prof.video = first(v, "Encoding");
            prof.width = num("Width").map(|x| x as u32);
            prof.height = num("Height").map(|x| x as u32);
            prof.fps = num("FrameRateLimit");
            prof.bitrate_kbps = num("BitrateLimit").map(|x| x as u32);
        }
        if let Some(a) = first_raw(p, "AudioEncoderConfiguration").or_else(|| first_raw(p, "AudioEncoder")) {
            prof.audio = first(a, "Encoding");
            // version 1 gives kHz, version 2 Hz
            prof.sample_rate = first(a, "SampleRate").and_then(|s| s.parse::<u32>().ok()).map(|r| if r < 1000 { r * 1000 } else { r });
        }
        let token = xml_escape(&prof.token);
        let stream = if version == 1 {
            format!("<StreamSetup><Stream xmlns=\"http://www.onvif.org/ver10/schema\">RTP-Unicast</Stream><Transport xmlns=\"http://www.onvif.org/ver10/schema\"><Protocol>RTSP</Protocol></Transport></StreamSetup><ProfileToken>{token}</ProfileToken>")
        } else {
            format!("<Protocol>RTSP</Protocol><ProfileToken>{token}</ProfileToken>")
        };
        match c.call(&media, ns, "GetStreamUri", &stream, true) {
            Ok(x) => prof.stream_uri = first(&x, "Uri"),
            Err(e) => prof.error = Some(e.message),
        }
        if prof.video.is_some() {
            if let Ok(x) = c.call(&media, ns, "GetSnapshotUri", &format!("<ProfileToken>{token}</ProfileToken>"), true) {
                prof.snapshot_uri = first(&x, "Uri");
            }
        }
        dev.profiles.push(prof);
    }
    dev.media = media;
    dev.media_version = version;
    Ok(dev)
}

/// A service address the camera gave, pointed at the host it was reached by: a camera behind a
/// gateway often names itself by an address only its own network can reach.
fn local(device: &HttpUrl, xaddr: &str) -> String {
    match HttpUrl::parse(xaddr) {
        Ok(x) if !x.host.eq_ignore_ascii_case(&device.host) => format!("{}://{}{}", if device.tls { "https" } else { "http" }, device.authority, x.path),
        Ok(x) => x.plain(),
        Err(_) => xaddr.to_string(),
    }
}

struct Client<'a> {
    cred: Option<(String, String)>,
    allow: &'a Allow,
    /// The camera's clock less ours, in seconds.
    skew: i64,
}

impl Client<'_> {
    /// Calls `action` of the service at `url`, whose namespace is `ns`, with `body` inside the
    /// action's element; the answer's body comes back, or a fault as an error.
    fn call(&self, url: &str, ns: &str, action: &str, body: &str, auth: bool) -> Result<String> {
        let header = match (&self.cred, auth) {
            (Some((user, pass)), true) => {
                let nonce = &sha1(format!("{}{}{:?}", uuid(), std::process::id(), SystemTime::now()).as_bytes())[..16];
                let created = iso(now_secs() + self.skew);
                format!(
                    r#"<s:Header><Security s:mustUnderstand="1" xmlns="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-secext-1.0.xsd"><UsernameToken><Username>{}</Username><Password Type="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-username-token-profile-1.0#PasswordDigest">{}</Password><Nonce EncodingType="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-soap-message-security-1.0#Base64Binary">{}</Nonce><Created xmlns="http://docs.oasis-open.org/wss/2004/01/oasis-200401-wss-wssecurity-utility-1.0.xsd">{created}</Created></UsernameToken></Security></s:Header>"#,
                    xml_escape(user),
                    password_digest(nonce, &created, pass),
                    base64(nonce)
                )
            }
            _ => String::new(),
        };
        let envelope = format!(r#"<?xml version="1.0" encoding="UTF-8"?><s:Envelope xmlns:s="http://www.w3.org/2003/05/soap-envelope">{header}<s:Body><{action} xmlns="{ns}">{body}</{action}></s:Body></s:Envelope>"#);
        let ct = format!("application/soap+xml; charset=utf-8; action=\"{ns}/{action}\"");
        let (status, answer) = http::post(url, &ct, envelope.as_bytes(), self.allow, MAX_ANSWER)?;
        let xml = String::from_utf8_lossy(&answer).into_owned();
        if let Some(fault) = first_raw(&xml, "Fault") {
            let reason = first_raw(fault, "Reason").and_then(|r| first(r, "Text")).or_else(|| first(fault, "faultstring")).unwrap_or_default();
            let codes: Vec<String> = elements(fault, "Value").into_iter().map(|(_, v)| text(v)).collect();
            let denied = codes.iter().any(|c| c.ends_with("NotAuthorized") || c.ends_with("FailedAuthentication")) || status == 401;
            let e = Error::new(if denied { ErrorCode::InvalidArgument } else { ErrorCode::Io }, format!("{} refused {action}: {}", redact(url), if reason.is_empty() { codes.join(", ") } else { reason }));
            return Err(if denied { e.hint("check the user and password; the camera must also allow ONVIF users (many keep them apart from web users)") } else { e });
        }
        if status == 401 {
            return Err(Error::new(ErrorCode::InvalidArgument, format!("{} answered 401 to {action}", redact(url))).hint("check the user and password; a camera that wants HTTP digest authentication instead of WS-Security is not supported yet"));
        }
        if status >= 400 {
            return Err(Error::new(ErrorCode::Io, format!("{} answered {status} to {action}", redact(url))));
        }
        match first_raw(&xml, "Body") {
            Some(b) => Ok(b.to_string()),
            None => Err(Error::invalid_input(format!("{} answered {action} without a SOAP body", redact(url)))),
        }
    }
}

/// WS-Security's password digest: Base64(SHA-1(nonce + created + password)).
pub fn password_digest(nonce: &[u8], created: &str, password: &str) -> String {
    let mut d = nonce.to_vec();
    d.extend_from_slice(created.as_bytes());
    d.extend_from_slice(password.as_bytes());
    base64(&sha1(&d))
}

/// A random-enough UUID (version 4 form) for message IDs: unique, not secret.
fn uuid() -> String {
    static N: AtomicU64 = AtomicU64::new(0);
    let t = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let h = sha1(format!("{t:?}{}{}", std::process::id(), N.fetch_add(1, Ordering::Relaxed)).as_bytes());
    let x: String = h[..16].iter().enumerate().map(|(i, b)| format!("{:02x}", if i == 6 { b & 0x0f | 0x40 } else if i == 8 { b & 0x3f | 0x80 } else { *b })).collect();
    format!("{}-{}-{}-{}-{}", &x[..8], &x[8..12], &x[12..16], &x[16..20], &x[20..])
}

fn now_secs() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs() as i64)
}

/// Seconds since 1970 of a UTC date and time (Howard Hinnant's days-from-civil).
fn unix(y: i64, m: i64, d: i64, h: i64, mi: i64, s: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    (era * 146_097 + doe - 719_468) * 86_400 + h * 3600 + mi * 60 + s
}

/// `secs` since 1970 as `YYYY-MM-DDTHH:MM:SSZ`.
fn iso(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + (month <= 2) as i64;
    format!("{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z", rem / 3600, rem / 60 % 60, rem % 60)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

/// Element text with its entities and character references replaced, trimmed.
fn text(s: &str) -> String {
    let s = s.trim();
    if let Some(c) = s.strip_prefix("<![CDATA[").and_then(|c| c.strip_suffix("]]>")) {
        return c.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(amp) = rest.find('&') {
        out.push_str(&rest[..amp]);
        rest = &rest[amp..];
        let Some(semi) = rest.find(';').filter(|&k| k <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..semi];
        let c = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")).map_or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()), |h| u32::from_str_radix(h, 16).ok()).and_then(char::from_u32),
        };
        match c {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Every element named `name` (any namespace prefix) in `xml` that is not inside another of that
/// name: its attributes and its content.
fn elements<'a>(xml: &'a str, name: &str) -> Vec<(&'a str, &'a str)> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(tag) = next_tag(xml, i) {
        i = tag.end;
        if tag.close || tag.local != name {
            continue;
        }
        if tag.empty {
            out.push((tag.attrs, ""));
            continue;
        }
        // to the matching close, counting nested ones of the same name
        let mut depth = 1;
        let start = tag.end;
        while let Some(t) = next_tag(xml, i) {
            i = t.end;
            if t.local == name && !t.empty {
                depth += if t.close { -1 } else { 1 };
                if depth == 0 {
                    out.push((tag.attrs, &xml[start..t.start]));
                    break;
                }
            }
        }
        if depth != 0 {
            break;
        }
    }
    out
}

/// The content of the first element named `name`, as it is.
fn first_raw<'a>(xml: &'a str, name: &str) -> Option<&'a str> {
    elements(xml, name).into_iter().next().map(|(_, c)| c)
}

/// The text of the first element named `name`.
fn first(xml: &str, name: &str) -> Option<String> {
    first_raw(xml, name).map(text)
}

/// The value of attribute `name` (any namespace prefix).
fn attr(attrs: &str, name: &str) -> Option<String> {
    let mut rest = attrs;
    while let Some(eq) = rest.find('=') {
        let key = rest[..eq].trim().rsplit(|c: char| c.is_whitespace()).next().unwrap_or("");
        let after = rest[eq + 1..].trim_start();
        let q = after.chars().next()?;
        if q != '"' && q != '\'' {
            return None;
        }
        let end = after[1..].find(q)? + 1;
        if key.rsplit(':').next() == Some(name) {
            return Some(text(&after[1..end]));
        }
        rest = &after[end + 1..];
    }
    None
}

struct Tag<'a> {
    start: usize,
    end: usize,
    local: &'a str,
    attrs: &'a str,
    close: bool,
    empty: bool,
}

/// The next start or end tag at or after `from`, skipping declarations, comments and CDATA.
fn next_tag(xml: &str, from: usize) -> Option<Tag<'_>> {
    let mut i = from;
    loop {
        let start = i + xml.get(i..)?.find('<')?;
        let rest = &xml[start..];
        let skip = if rest.starts_with("<!--") {
            Some("-->")
        } else if rest.starts_with("<![CDATA[") {
            Some("]]>")
        } else if rest.starts_with("<?") {
            Some("?>")
        } else if rest.starts_with("<!") {
            Some(">")
        } else {
            None
        };
        if let Some(endmark) = skip {
            i = start + rest.find(endmark)? + endmark.len();
            continue;
        }
        let gt = start + rest.find('>')?;
        let inner = &xml[start + 1..gt];
        let close = inner.starts_with('/');
        let inner = inner.trim_start_matches('/');
        let empty = inner.ends_with('/');
        let inner = inner.trim_end_matches('/');
        let name_end = inner.find(|c: char| c.is_whitespace()).unwrap_or(inner.len());
        let qname = &inner[..name_end];
        return Some(Tag { start, end: gt + 1, local: qname.rsplit(':').next().unwrap_or(qname), attrs: &inner[name_end..], close, empty });
    }
}
