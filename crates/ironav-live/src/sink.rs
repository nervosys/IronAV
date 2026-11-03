//! Live output: where a session sends what it makes, as `source` is where it reads from. Two kinds of
//! destination: a byte sink, which takes MPEG-TS (`srt://`, see `open_sink`), and a WHIP endpoint
//! (`whip://`, `whips://`), which takes H.264 and Opus through a [`WhipPublisher`](crate::WhipPublisher)
//! and is mapped by [`whip_endpoint`].
//!
//! A sink takes bytes at the pace of the media, and never waits for the network for long: what the
//! line cannot take is dropped and counted, and a peer that is gone is an error the next `write`
//! reports. Whoever writes decides what to do about that (the engine opens the sink again).

use crate::url::{percent_decode, redact, SourceUrl};
use ironav_core::{Error, Result};
use serde::Serialize;

#[derive(Debug, Clone, Serialize)]
pub struct SinkInfo {
    /// The URL with any password or passphrase replaced.
    pub url: String,
    /// `srt`.
    pub kind: &'static str,
    /// Whether the bytes are sent as they arrive rather than kept (a live output).
    pub live: bool,
}

/// What a sink has done.
#[derive(Debug, Clone, Default, Serialize)]
pub struct SinkStats {
    /// Whether a receiver is connected now.
    pub connected: bool,
    /// Payloads sent (not counting those sent again), and their bytes.
    pub packets: u64,
    pub bytes: u64,
    /// Packets sent again at the receiver's request or for want of an acknowledgement.
    pub retransmitted: u64,
    /// Packets given up on: not acknowledged in time to be of use to the receiver.
    pub dropped: u64,
    /// Bytes written with no receiver connected, or faster than the line took them.
    pub discarded_bytes: u64,
    /// The round trip to the receiver, milliseconds.
    pub rtt_ms: f64,
}

pub trait Sink: Send {
    /// MPEG-TS bytes, as they are made. Does not wait for the network; an error means the output
    /// is gone (the receiver left a caller's connection, or the sink was finished).
    fn write(&mut self, bytes: &[u8]) -> Result<()>;
    /// Sends what is held, waits briefly for the receiver to have it, and says goodbye.
    fn finish(&mut self) -> Result<()>;
    fn info(&self) -> &SinkInfo;
    fn stats(&self) -> SinkStats {
        SinkStats::default()
    }
}

/// The schemes a session may publish to.
const OUTPUT_SCHEMES: [&str; 3] = ["srt", "whip", "whips"];

/// A sink URL checked, without connecting to anything. The `reconnect` option says whether the
/// engine opens the sink again after it fails: a caller is kept up unless `reconnect=0`, and a
/// listener's sink takes the next receiver by itself.
#[derive(Debug, Clone)]
pub struct SinkPlan {
    /// The URL with secrets replaced.
    pub shown: String,
    pub scheme: String,
    /// Whether it makes the connection (a caller), rather than waiting for one.
    pub caller: bool,
    /// Whether to open it again when it fails (a caller, unless `reconnect=0`).
    pub reconnect: bool,
}

/// Checks a sink URL: its scheme, its options, and FIPS mode's rule that a transport carry TLS or
/// encryption of its own. The address allow-list applies when the sink connects.
pub fn plan_sink(url: &str) -> Result<SinkPlan> {
    let u = SourceUrl::parse(url)?;
    if !OUTPUT_SCHEMES.contains(&u.scheme.as_str()) {
        return Err(Error::invalid_arg(format!("'{}' outputs are not supported", u.scheme)).alternatives(OUTPUT_SCHEMES));
    }
    // whip:// signals in plaintext, which FIPS mode refuses; whips:// is TLS
    if u.scheme == "whip" && crate::fips::enabled() {
        return Err(Error::invalid_arg("whip:// is a plaintext transport, which FIPS mode refuses").hint("publish to whips://host:port/path, which signals over TLS"));
    }
    // an SRT stream with a passphrase is encrypted; without one it is plaintext
    if !(u.scheme == "srt" && u.option("passphrase").is_some()) {
        crate::fips::check_transport(&u.scheme)?;
    }
    if u.scheme == "srt" {
        crate::srt::check_sender(&u)?;
    }
    if u.scheme == "whip" || u.scheme == "whips" {
        whip_endpoint_of(&u)?;
    }
    let caller = !crate::srt::is_listener(&u);
    let reconnect = match u.option("reconnect") {
        Some(_) => u.flag("reconnect"),
        None => caller,
    };
    Ok(SinkPlan { shown: redact(url), scheme: u.scheme, caller, reconnect })
}

/// Opens the sink a URL names: `srt://host:port` calls a receiver that listens, and
/// `srt://:port?mode=listener` waits for one to call. Options: `latency` (milliseconds, 120),
/// `streamid`, `passphrase` (encrypts the stream; redacted from what is shown), `pbkeylen` (the
/// key's length in bytes: 16, 24 or 32) and `reconnect`. A caller is connected when this returns.
pub fn open_sink(url: &str) -> Result<Box<dyn Sink>> {
    let plan = plan_sink(url)?;
    let u = SourceUrl::parse(url)?;
    match plan.scheme.as_str() {
        "srt" => crate::srt::open_sender(&u, plan.shown),
        "whip" | "whips" => Err(Error::invalid_arg("a WHIP endpoint takes H.264 and Opus, not MPEG-TS bytes").hint("publish to it with ironav_live::WhipPublisher, from whip_endpoint(url)")),
        other => Err(Error::invalid_arg(format!("'{other}' outputs are not supported"))),
    }
}

/// Where a WHIP publisher connects: the endpoint's HTTP(S) URL, without the token, and the bearer token.
#[derive(Debug, Clone, PartialEq)]
pub struct WhipEndpoint {
    /// `http://host:port/path` for `whip://`, `https://host:port/path` for `whips://`.
    pub url: String,
    /// The `token` option, percent-decoded: sent as `Authorization: Bearer` to this endpoint only.
    pub token: Option<String>,
}

/// Maps a publish URL, `whip://host:port[/path][?token=T]` or `whips://...`, to the endpoint a
/// [`WhipPublisher`](crate::WhipPublisher) connects to. The host and port are required (a listening
/// endpoint's host may be left out, a destination's may not). Only `token` and `reconnect` are taken;
/// `cert` and `key` belong to a listening `whips://` endpoint.
pub fn whip_endpoint(url: &str) -> Result<WhipEndpoint> {
    whip_endpoint_of(&SourceUrl::parse(url)?)
}

fn whip_endpoint_of(u: &SourceUrl) -> Result<WhipEndpoint> {
    if let Some(k) = u.options.keys().find(|k| !matches!(k.as_str(), "token" | "reconnect")) {
        return Err(Error::invalid_arg(format!("'{k}' is not an option of a WHIP destination")).hint("a WHIP destination takes token= and reconnect= only; cert= and key= are for a listening whips:// endpoint"));
    }
    let hostport = u.target.split('/').next().unwrap_or("");
    let (host, port) = hostport.rsplit_once(':').unwrap_or(("", ""));
    if host.is_empty() {
        return Err(Error::invalid_arg(format!("'{}://{}' has no host", u.scheme, u.target)).hint("name the endpoint: whip://host:port/path"));
    }
    if port.parse::<u16>().is_err() {
        return Err(Error::invalid_arg(format!("'{}://{}' needs a port number after the host", u.scheme, u.target)).hint("whip://host:port/path"));
    }
    if u.target.contains(['@', '?', ' ']) {
        return Err(Error::invalid_arg("a WHIP destination's address holds only host, port and path; credentials go in token=").hint("whip://host:port/path?token=T"));
    }
    let scheme = if u.scheme == "whips" { "https" } else { "http" };
    let url = format!("{scheme}://{}", u.target);
    crate::http::HttpUrl::parse(&url)?;
    let token = u.option("token").map(percent_decode);
    if let Some(t) = &token {
        if t.is_empty() || t.len() > 512 || !t.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(Error::invalid_arg("the WHIP token must be 1 to 512 printable characters, without spaces"));
        }
    }
    Ok(WhipEndpoint { url, token })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_whip_destination_maps_to_its_http_url_and_the_token_apart() {
        let e = whip_endpoint("whip://127.0.0.1:8080/live?token=s3cret").unwrap();
        assert_eq!(e, WhipEndpoint { url: "http://127.0.0.1:8080/live".into(), token: Some("s3cret".into()) });
        let e = whip_endpoint("whips://ingest.example:443/a/b?token=tok%2Bn").unwrap();
        assert_eq!(e, WhipEndpoint { url: "https://ingest.example:443/a/b".into(), token: Some("tok+n".into()) });
        assert_eq!(whip_endpoint("whip://[::1]:8080").unwrap(), WhipEndpoint { url: "http://[::1]:8080".into(), token: None });
        assert_eq!(whip_endpoint("whip://10.0.0.5:9/x?reconnect=0").unwrap().url, "http://10.0.0.5:9/x");
    }

    #[test]
    fn a_whip_destination_that_cannot_be_connected_to_is_refused_before_anything_is_made() {
        for bad in [
            "whip://:8080/live",
            "whip://127.0.0.1/live",
            "whip://127.0.0.1:port/live",
            "whip://127.0.0.1:8080/live?cert=a.pem&key=b.pem",
            "whip://127.0.0.1:8080/live?latency=5",
            "whip://127.0.0.1:8080/live?token=",
            "whip://127.0.0.1:8080/live?token=a%20b",
            "whip://user:pw@127.0.0.1:8080/live",
            "whip://127.0.0.1:8080/live?mode=listener",
        ] {
            let e = whip_endpoint(bad).err().unwrap_or_else(|| panic!("{bad} was accepted"));
            assert_eq!(e.code, ironav_core::ErrorCode::InvalidArgument, "{bad}: {e:?}");
            assert!(plan_sink(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn the_plan_shows_the_token_redacted_and_a_destination_calls() {
        let p = plan_sink("whips://127.0.0.1:8443/live?token=s3cret").unwrap();
        assert_eq!((p.scheme.as_str(), p.caller, p.reconnect), ("whips", true, true));
        assert_eq!(p.shown, "whips://127.0.0.1:8443/live?token=***");
        assert!(!format!("{p:?}").contains("s3cret"));
        assert!(!plan_sink("whips://127.0.0.1:8080/live?reconnect=0").unwrap().reconnect);
    }
}
