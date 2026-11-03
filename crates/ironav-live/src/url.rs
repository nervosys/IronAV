//! Source URLs: `tcp://host:port`, `udp://:port`, `srt://host:port` or `srt://:port?mode=listener`,
//! `rtsp://[user:password@]host[:port]/path`,
//! `http://host[:port]/path` (HLS or a download; a YouTube video or live stream by its watch URL,
//! through yt-dlp), `device://video[/NAME]` or
//! `device://audio[/NAME]` (a camera or microphone), `whip://[host]:port[/path]` (a WHIP endpoint
//! a WebRTC publisher sends to), `pipe:`, `-`, `file:path` or a bare path, each
//! optionally followed by `?key=value&...` options.

use ironav_core::{Error, Result};
use std::collections::BTreeMap;

/// Options a source URL can carry:
/// - `format`: the stream's format when it cannot be told from its first bytes;
/// - `fps`: the frame rate of a raw H.264/HEVC stream (`30` or `30000/1001`), or to ask a camera for;
/// - `size`: the picture size to ask a camera for (`1280x720`);
/// - `pace=realtime`: read a file no faster than it plays, as if it were live;
/// - `follow`: keep reading a file as it grows;
/// - `idle`: with `follow`, end after this many seconds without new bytes;
/// - `listen`: wait for a TCP connection on the given address instead of making one;
/// - `transport=udp`: have an RTSP server send RTP over UDP rather than on its connection;
/// - `audio`: the HLS audio rendition to play, by language or name;
/// - `height`: the tallest HLS (or YouTube) variant to play, in lines;
/// - `cookies`: for a YouTube URL, the browser whose signed-in session yt-dlp reads (`firefox`,
///   `chrome`, ...), as its `--cookies-from-browser` does;
/// - `captions`: the HLS (or YouTube) subtitles to follow, by language or name, or `off`;
/// - `mode`: for SRT, `caller` (the default with a host) or `listener` (the default with none);
/// - `latency`: for SRT, the milliseconds a packet is held for retransmissions (120);
/// - `streamid`: for SRT, the stream ID to give the sender;
/// - `passphrase`: for SRT, the passphrase of an encrypted stream (redacted from what is shown);
/// - `token`: the bearer token a WHIP publisher must send (redacted wherever the URL is shown);
/// - `cert`, `key`: for `whips://`, the PEM files of the endpoint's TLS certificate chain and private key.
pub const OPTIONS: [&str; 24] = ["format", "fps", "pace", "follow", "idle", "listen", "transport", "audio", "size", "reconnect", "stall", "retries", "rate", "duration", "height", "cookies", "captions", "mode", "latency", "streamid", "passphrase", "token", "cert", "key"];

#[derive(Debug, Clone, PartialEq)]
pub struct SourceUrl {
    /// `file`, `pipe`, `tcp` or `udp`.
    pub scheme: String,
    /// The path, or `host:port` (the host may be empty).
    pub target: String,
    pub options: BTreeMap<String, String>,
}

impl SourceUrl {
    pub fn parse(url: &str) -> Result<SourceUrl> {
        // options follow the last '?' only when every one is known, so a path keeps its '?'
        let known = |kv: &str| OPTIONS.contains(&kv.split_once('=').map_or(kv, |(k, _)| k).to_ascii_lowercase().as_str());
        let (base, options) = match url.rsplit_once('?') {
            Some((b, q)) if !q.is_empty() && q.split('&').all(known) => (b, q),
            _ => (url, ""),
        };
        let mut opts = BTreeMap::new();
        for kv in options.split('&').filter(|s| !s.is_empty()) {
            let (k, v) = kv.split_once('=').unwrap_or((kv, "1"));
            opts.insert(k.to_ascii_lowercase(), v.to_string());
        }
        let (scheme, target) = if let Some((s, rest)) = base.split_once("://") {
            (s.to_ascii_lowercase(), rest.to_string())
        } else if base == "-" || base == "pipe:" || base == "stdin" {
            ("pipe".to_string(), String::new())
        } else if let Some(p) = base.strip_prefix("pipe:") {
            ("pipe".to_string(), p.to_string())
        } else if let Some(p) = base.strip_prefix("file:") {
            ("file".to_string(), p.to_string())
        } else {
            ("file".to_string(), base.to_string())
        };
        match scheme.as_str() {
            "file" | "pipe" | "tcp" | "udp" | "srt" | "rtsp" | "rtsps" | "http" | "https" | "device" | "test" | "whip" | "whips" => {}
            other => {
                return Err(Error::invalid_arg(format!("'{other}' sources are not supported")).alternatives(["file", "pipe", "tcp", "udp", "srt", "rtsp", "rtsps", "http", "https", "device", "test", "whip", "whips"]));
            }
        }
        if matches!(scheme.as_str(), "tcp" | "udp" | "srt" | "whip" | "whips") && !target.contains(':') {
            return Err(Error::invalid_arg(format!("{scheme} sources need host:port (or :port), got '{target}'")));
        }
        Ok(SourceUrl { scheme, target, options: opts })
    }

    pub fn flag(&self, key: &str) -> bool {
        self.options.get(key).is_some_and(|v| !matches!(v.as_str(), "0" | "false" | "no" | "off"))
    }

    pub fn option(&self, key: &str) -> Option<&str> {
        self.options.get(key).map(String::as_str)
    }
}

/// `%XX` escapes decoded, as user names and passwords in URLs are written.
pub(crate) fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        match (b[i], b.get(i + 1..i + 3).and_then(|h| u8::from_str_radix(std::str::from_utf8(h).ok()?, 16).ok())) {
            (b'%', Some(v)) => {
                out.push(v);
                i += 3;
            }
            (c, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// What follows `scheme://`, split into user information, host and port, and the rest (path and
/// query). The user information ends at the last `@` before any `?`, so a password holding an
/// unescaped `/` is still taken whole (and redacted whole) rather than read as a path.
pub(crate) fn split_authority(rest: &str) -> (Option<&str>, &str, &str) {
    let query = rest.find('?').unwrap_or(rest.len());
    let (cred, after) = match rest[..query].rfind('@') {
        Some(at) => (Some(&rest[..at]), &rest[at + 1..]),
        None => (None, rest),
    };
    let end = after.find(['/', '?']).unwrap_or(after.len());
    (cred, &after[..end], &after[end..])
}

/// `url` with the value of an SRT `passphrase` option replaced by `***`.
fn redact_passphrase(url: &str) -> String {
    let secret = |kv: &str| kv.split_once('=').is_some_and(|(k, _)| k.eq_ignore_ascii_case("passphrase"));
    match url.rsplit_once('?') {
        Some((base, q)) if q.split('&').any(secret) => {
            let q: Vec<String> = q.split('&').map(|kv| if secret(kv) { format!("{}=***", kv.split_once('=').map_or(kv, |(k, _)| k)) } else { kv.to_string() }).collect();
            format!("{base}?{}", q.join("&"))
        }
        _ => url.to_string(),
    }
}

/// `url` with any password replaced by `***`, so it can be echoed in observations, statuses and
/// errors. User information with no `:` may itself be a token, so all of it is replaced, and so is
/// the value of a `token=` option.
pub fn redact(url: &str) -> String {
    let url = &redact_token(&redact_passphrase(url));
    let Some((scheme, rest)) = url.split_once("://") else { return url.to_string() };
    match split_authority(rest) {
        (Some(cred), host, tail) => match cred.split_once(':') {
            Some((user, _)) => format!("{scheme}://{user}:***@{host}{tail}"),
            None => format!("{scheme}://***@{host}{tail}"),
        },
        (None, ..) => url.to_string(),
    }
}

/// `url` with the value of each `token=` in its query replaced by `***`.
fn redact_token(url: &str) -> String {
    let Some((base, query)) = url.split_once('?') else { return url.to_string() };
    let parts: Vec<String> = query
        .split('&')
        .map(|kv| match kv.split_once('=') {
            Some((k, _)) if k.eq_ignore_ascii_case("token") => format!("{k}=***"),
            _ => kv.to_string(),
        })
        .collect();
    format!("{base}?{}", parts.join("&"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_and_their_options() {
        let u = SourceUrl::parse("udp://:5000?format=h264&fps=30").unwrap();
        assert_eq!((u.scheme.as_str(), u.target.as_str()), ("udp", ":5000"));
        assert_eq!(u.option("format"), Some("h264"));
        let u = SourceUrl::parse("clip.y4m?pace=realtime&follow").unwrap();
        assert_eq!((u.scheme.as_str(), u.target.as_str()), ("file", "clip.y4m"));
        assert!(u.flag("follow") && u.option("pace") == Some("realtime"));
        assert_eq!(SourceUrl::parse("-").unwrap().scheme, "pipe");
        assert_eq!(SourceUrl::parse("C:/media/a?b.wav").unwrap().target, "C:/media/a?b.wav");
        assert_eq!(SourceUrl::parse("rtsp://cam/stream").unwrap().scheme, "rtsp");
        let u = SourceUrl::parse("srt://cam:9000?latency=200&streamid=a&mode=caller").unwrap();
        assert_eq!((u.scheme.as_str(), u.target.as_str(), u.option("latency"), u.option("streamid")), ("srt", "cam:9000", Some("200"), Some("a")));
        assert!(SourceUrl::parse("srt://cam").is_err());
        assert!(SourceUrl::parse("srtp://cam:9000").is_err());
        assert_eq!(percent_decode("p%40ss%3a%zz"), "p@ss:%zz");
        assert!(SourceUrl::parse("tcp://localhost").is_err());
    }

    #[test]
    fn passwords_are_redacted() {
        assert_eq!(redact("tcp://bob:hunter2@cam.local:554/live?x=1"), "tcp://bob:***@cam.local:554/live?x=1");
        assert_eq!(redact("udp://:5000"), "udp://:5000");
        assert_eq!(redact("clip.y4m"), "clip.y4m");
        assert_eq!(redact("srt://cam:9000?latency=200&passphrase=hunter2hunter2&streamid=a"), "srt://cam:9000?latency=200&passphrase=***&streamid=a");
        assert_eq!(redact("srt://:9000?PassPhrase=x"), "srt://:9000?PassPhrase=***");
        assert_eq!(redact("rtsp://3f9aTOKEN@cam/live"), "rtsp://***@cam/live");
        // an unescaped '/' in the password must not end the user information early
        assert_eq!(redact("rtsp://bob:pa/ss@cam:554/live?x=a@b"), "rtsp://bob:***@cam:554/live?x=a@b");
        assert_eq!(redact("whip://0.0.0.0:8080/live?token=s3cret&format=rtp"), "whip://0.0.0.0:8080/live?token=***&format=rtp");
        let u = SourceUrl::parse("whip://:8080/live?token=abc").unwrap();
        assert_eq!((u.scheme.as_str(), u.target.as_str(), u.option("token")), ("whip", ":8080/live", Some("abc")));
        assert!(SourceUrl::parse("whip://live").is_err());
        let u = SourceUrl::parse("whips://:8443/live?cert=C:/tls/chain.pem&key=C:/tls/key.pem&token=abc").unwrap();
        assert_eq!((u.scheme.as_str(), u.option("cert"), u.option("key")), ("whips", Some("C:/tls/chain.pem"), Some("C:/tls/key.pem")));
        assert_eq!(redact("whips://:8443/live?cert=a.pem&key=k.pem&token=s3cret"), "whips://:8443/live?cert=a.pem&key=k.pem&token=***");
    }
}
