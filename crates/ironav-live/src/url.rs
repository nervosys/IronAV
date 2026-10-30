//! Source URLs: `tcp://host:port`, `udp://:port`, `pipe:`, `-`, `file:path` or a bare path, each
//! optionally followed by `?key=value&...` options.

use ironav_core::{Error, Result};
use std::collections::BTreeMap;

/// Options a source URL can carry:
/// - `format`: the stream's format when it cannot be told from its first bytes;
/// - `fps`: the frame rate of a raw H.264/HEVC stream (`30` or `30000/1001`);
/// - `pace=realtime`: read a file no faster than it plays, as if it were live;
/// - `follow`: keep reading a file as it grows;
/// - `idle`: with `follow`, end after this many seconds without new bytes;
/// - `listen`: wait for a TCP connection on the given address instead of making one.
pub const OPTIONS: [&str; 6] = ["format", "fps", "pace", "follow", "idle", "listen"];

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
            "file" | "pipe" | "tcp" | "udp" => {}
            other => {
                return Err(Error::invalid_arg(format!("'{other}' sources are not supported")).alternatives(["file", "pipe", "tcp", "udp"]));
            }
        }
        if matches!(scheme.as_str(), "tcp" | "udp") && !target.contains(':') {
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

/// `url` with any password replaced by `***`, so it can be echoed in observations, statuses and
/// errors. User information with no `:` may itself be a token, so all of it is replaced.
pub fn redact(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else { return url.to_string() };
    let (auth, tail) = match rest.find(['/', '?']) {
        Some(i) => rest.split_at(i),
        None => (rest, ""),
    };
    match auth.rsplit_once('@') {
        Some((cred, host)) => match cred.split_once(':') {
            Some((user, _)) => format!("{scheme}://{user}:***@{host}{tail}"),
            None => format!("{scheme}://***@{host}{tail}"),
        },
        None => url.to_string(),
    }
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
        assert!(SourceUrl::parse("rtsp://cam/stream").is_err());
        assert!(SourceUrl::parse("tcp://localhost").is_err());
    }

    #[test]
    fn passwords_are_redacted() {
        assert_eq!(redact("tcp://bob:hunter2@cam.local:554/live?x=1"), "tcp://bob:***@cam.local:554/live?x=1");
        assert_eq!(redact("udp://:5000"), "udp://:5000");
        assert_eq!(redact("clip.y4m"), "clip.y4m");
        assert_eq!(redact("rtsp://3f9aTOKEN@cam/live"), "rtsp://***@cam/live");
    }
}
