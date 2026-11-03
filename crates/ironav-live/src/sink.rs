//! Live output: where a session sends the MPEG-TS it makes, as `source` is where it reads from.
//! Today that is `srt://` (an SRT sender, see `srt::send`); other schemes join `open_sink`.
//!
//! A sink takes bytes at the pace of the media, and never waits for the network for long: what the
//! line cannot take is dropped and counted, and a peer that is gone is an error the next `write`
//! reports. Whoever writes decides what to do about that (the engine opens the sink again).

use crate::url::{redact, SourceUrl};
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

/// The schemes `open_sink` knows.
const OUTPUT_SCHEMES: [&str; 1] = ["srt"];

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
    // an SRT stream with a passphrase is encrypted; without one it is plaintext
    if !(u.scheme == "srt" && u.option("passphrase").is_some()) {
        crate::fips::check_transport(&u.scheme)?;
    }
    if u.scheme == "srt" {
        crate::srt::check_sender(&u)?;
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
        other => Err(Error::invalid_arg(format!("'{other}' outputs are not supported"))),
    }
}
