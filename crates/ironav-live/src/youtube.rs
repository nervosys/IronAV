//! YouTube URLs (`https://www.youtube.com/watch?v=ID`, `youtu.be/ID`, `/shorts/ID`, `/live/ID`,
//! `/embed/ID`, `music.youtube.com`, `m.youtube.com`): a video or a live stream, played through
//! its HLS master playlist, which IronAV's HLS source fetches as it fetches any other.
//!
//! Finding that playlist takes YouTube's player API, whose answers change often and hide the
//! stream URLs behind signatures; [yt-dlp](https://github.com/yt-dlp/yt-dlp) keeps up with it, so
//! IronAV asks it (`yt-dlp -J`, from `IRONAV_YTDLP` or the `PATH`) and reads only its answer.
//! IronAV does not imitate a browser or work around YouTube's checks. It asks once per video and
//! keeps the answer until its URLs are about to expire (a reconnect, or a second session on the
//! same video, asks nothing), leaves at least two seconds between questions, and fetches the
//! media as the HLS source does, a segment at a time as it is read and under IronAV's own name.
//! When YouTube asks an address to sign in first ("confirm you're not a bot"), the error says so;
//! the `cookies` option then lets yt-dlp use the caller's own signed-in browser session
//! (`?cookies=firefox`, its `--cookies-from-browser`). The `height` option caps the variant, and
//! variants IronAV cannot decode (AV1) are passed over. Fetching from YouTube is subject to its
//! terms of service, which are the caller's to follow.

use crate::source::{Chunk, Source, SourceInfo};
use crate::url::SourceUrl;
use ironav_core::{Error, ErrorCode, Result};
use std::collections::HashMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The longest yt-dlp may take to answer.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(60);
/// The least time between two questions to yt-dlp.
const MIN_INTERVAL: Duration = Duration::from_secs(2);
/// An answer is asked again this long before its URLs expire.
const EXPIRY_MARGIN: u64 = 300;

/// The video ID a YouTube URL names, if it is one.
pub(crate) fn video_id(u: &SourceUrl) -> Option<String> {
    if !matches!(u.scheme.as_str(), "http" | "https") {
        return None;
    }
    let (host, rest) = u.target.split_once(['/', '?']).map_or((u.target.as_str(), ""), |(h, _)| (h, &u.target[h.len()..]));
    let host = host.to_ascii_lowercase();
    let host = host.rsplit_once('@').map_or(host.as_str(), |(_, h)| h).split(':').next().unwrap_or("");
    let id = if host == "youtu.be" {
        rest.trim_start_matches('/').split(['?', '/', '#']).next()
    } else if ["youtube.com", "www.youtube.com", "m.youtube.com", "music.youtube.com", "youtube-nocookie.com", "www.youtube-nocookie.com"].contains(&host) {
        let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
        if path == "/watch" || path == "/watch/" {
            query.split(['&', '#']).find_map(|kv| kv.strip_prefix("v="))
        } else {
            ["/shorts/", "/live/", "/embed/", "/v/"].iter().find_map(|p| path.strip_prefix(p)).and_then(|r| r.split(['/', '?', '#']).next())
        }
    } else {
        None
    }?;
    // IDs are 11 characters of the URL-safe base 64 alphabet
    (id.len() == 11 && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')).then(|| id.to_string())
}

/// What yt-dlp said of a video.
#[derive(Clone, Debug)]
struct Resolved {
    manifest: String,
    live: bool,
    /// Unix seconds after which the URLs stop working.
    expires: u64,
}

fn cache() -> &'static Mutex<HashMap<String, Resolved>> {
    static CACHE: OnceLock<Mutex<HashMap<String, Resolved>>> = OnceLock::new();
    CACHE.get_or_init(Default::default)
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

/// Opens the video `id` that `u` names as an HLS source, labelled `shown`.
pub(crate) fn open(u: &SourceUrl, id: &str, shown: String) -> Result<Box<dyn Source>> {
    let r = resolve(id, u.option("cookies"))?;
    let target = r.manifest.strip_prefix("https://").ok_or_else(|| Error::new(ErrorCode::Io, format!("{shown}: yt-dlp gave a playlist that is not on https")))?;
    let mut options = u.options.clone();
    options.remove("cookies");
    let hls = SourceUrl { scheme: "https".into(), target: target.to_string(), options };
    // the signed playlist URL (which carries the address it was issued to) stays inside
    let inner = crate::hls::open(&hls, shown.clone())?;
    Ok(Box::new(YouTube { info: SourceInfo { url: shown, kind: "youtube", live: true, synthetic: false }, inner, _live: r.live }))
}

/// The HLS source under the YouTube URL's name.
struct YouTube {
    info: SourceInfo,
    inner: Box<dyn Source>,
    _live: bool,
}

impl Source for YouTube {
    fn info(&self) -> &SourceInfo {
        &self.info
    }

    fn read(&mut self, buf: &mut [u8], wait: Duration) -> Result<Chunk> {
        self.inner.read(buf, wait)
    }
}

/// The video's playlist, from the cache or from yt-dlp.
fn resolve(id: &str, cookies: Option<&str>) -> Result<Resolved> {
    if let Some(r) = cache().lock().unwrap().get(id).filter(|r| r.expires > now() + EXPIRY_MARGIN) {
        return Ok(r.clone());
    }
    static LAST: Mutex<Option<Instant>> = Mutex::new(None);
    {
        let mut last = LAST.lock().unwrap();
        if let Some(t) = *last {
            if let Some(wait) = MIN_INTERVAL.checked_sub(t.elapsed()) {
                std::thread::sleep(wait);
            }
        }
        *last = Some(Instant::now());
    }
    let json = ask_ytdlp(id, cookies)?;
    let r = parse(&json, id)?;
    cache().lock().unwrap().insert(id.to_string(), r.clone());
    Ok(r)
}

/// Runs `yt-dlp -J` on the video and returns its standard output.
fn ask_ytdlp(id: &str, cookies: Option<&str>) -> Result<Vec<u8>> {
    let exe = std::env::var("IRONAV_YTDLP").unwrap_or_else(|_| "yt-dlp".to_string());
    let mut cmd = Command::new(&exe);
    cmd.args(["-J", "--no-playlist", "--no-warnings", "--skip-download"]);
    if let Some(browser) = cookies {
        if browser.is_empty() || !browser.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-:+.".contains(&b)) {
            return Err(Error::invalid_arg(format!("cookies names a browser (firefox, chrome, edge, ...), not '{browser}'")));
        }
        cmd.args(["--cookies-from-browser", browser]);
    }
    cmd.arg("--").arg(format!("https://www.youtube.com/watch?v={id}"));
    let mut child = cmd.stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).spawn().map_err(|e| {
        Error::new(ErrorCode::NotFound, format!("could not run yt-dlp ({exe}): {e}")).hint("install yt-dlp (https://github.com/yt-dlp/yt-dlp), or set IRONAV_YTDLP to its path")
    })?;
    let (mut out, mut err) = (child.stdout.take().unwrap(), child.stderr.take().unwrap());
    let reader = std::thread::spawn(move || {
        let mut v = Vec::new();
        out.read_to_end(&mut v).map(|_| v)
    });
    let err_reader = std::thread::spawn(move || {
        let mut s = String::new();
        let _ = err.read_to_string(&mut s);
        s
    });
    let started = Instant::now();
    let status = loop {
        if let Some(s) = child.try_wait()? {
            break s;
        }
        if started.elapsed() > RESOLVE_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            return Err(Error::new(ErrorCode::Io, format!("yt-dlp did not answer for video {id} within {} s", RESOLVE_TIMEOUT.as_secs())));
        }
        std::thread::sleep(Duration::from_millis(50));
    };
    let stdout = reader.join().map_err(|_| Error::new(ErrorCode::Internal, "the yt-dlp reader failed"))??;
    let stderr = err_reader.join().unwrap_or_default();
    if !status.success() {
        return Err(explain(id, &stderr));
    }
    Ok(stdout)
}

/// An error for what yt-dlp complained of.
fn explain(id: &str, stderr: &str) -> Error {
    let line = stderr.lines().rev().find(|l| l.contains("ERROR")).unwrap_or(stderr.trim()).trim();
    let lower = stderr.to_ascii_lowercase();
    if lower.contains("not a bot") || lower.contains("sign in to confirm") {
        Error::new(ErrorCode::Io, format!("YouTube asked this address to sign in before giving video {id}: {line}"))
            .hint("pass cookies=BROWSER (firefox, chrome, edge, ...) to use your own signed-in browser session, or try again later; IronAV does not work around the check")
    } else if lower.contains("private video") || lower.contains("video unavailable") || lower.contains("has been removed") || lower.contains("does not exist") {
        Error::new(ErrorCode::NotFound, format!("YouTube video {id} is not available: {line}"))
    } else if lower.contains("429") || lower.contains("too many requests") {
        Error::new(ErrorCode::Io, format!("YouTube is limiting requests from this address: {line}")).hint("wait a while before asking again")
    } else {
        Error::new(ErrorCode::Io, format!("yt-dlp could not resolve video {id}: {line}")).hint("update yt-dlp (yt-dlp -U); YouTube changes often")
    }
}

/// The HLS master playlist (and whether the video is live) from yt-dlp's JSON.
fn parse(json: &[u8], id: &str) -> Result<Resolved> {
    let v: serde_json::Value = serde_json::from_slice(json).map_err(|e| Error::new(ErrorCode::Io, format!("yt-dlp's answer for video {id} is not JSON: {e}")))?;
    let live = v["is_live"].as_bool().unwrap_or(false);
    let formats = v["formats"].as_array().map(Vec::as_slice).unwrap_or(&[]);
    let manifest = formats
        .iter()
        .filter(|f| f["protocol"].as_str().is_some_and(|p| p.starts_with("m3u8")))
        .find_map(|f| f["manifest_url"].as_str())
        .or_else(|| v["manifest_url"].as_str().filter(|m| m.contains(".m3u8") || m.contains("hls")))
        .ok_or_else(|| {
            Error::new(ErrorCode::UnsupportedFormat, format!("YouTube video {id} has no HLS playlist, only separate progressive files, which IronAV cannot stream yet"))
                .hint("download it first (yt-dlp -f 'bv*[vcodec^=avc1]+ba' --remux-video mkv) and open the file")
        })?
        .to_string();
    if !manifest.starts_with("https://") {
        return Err(Error::new(ErrorCode::Io, format!("yt-dlp gave a playlist for video {id} that is not on https")));
    }
    // the URL says when it expires (`/expire/N/` or `expire=N`); else an hour
    let expires = manifest
        .split(['/', '&', '?'])
        .collect::<Vec<_>>()
        .windows(2)
        .find_map(|w| if w[0] == "expire" { w[1].parse().ok() } else { w[0].strip_prefix("expire=").and_then(|n| n.parse().ok()) })
        .unwrap_or(now() + 3600);
    Ok(Resolved { manifest, live, expires })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(url: &str) -> Option<String> {
        video_id(&SourceUrl::parse(url).unwrap())
    }

    #[test]
    fn youtube_urls_name_their_video() {
        for url in [
            "https://www.youtube.com/watch?v=jNQXAC9IVRw",
            "https://youtube.com/watch?feature=share&v=jNQXAC9IVRw&t=4",
            "https://m.youtube.com/watch?v=jNQXAC9IVRw#t=1",
            "https://youtu.be/jNQXAC9IVRw?si=abc",
            "https://www.youtube.com/shorts/jNQXAC9IVRw",
            "https://www.youtube.com/live/jNQXAC9IVRw?feature=shared",
            "https://www.youtube-nocookie.com/embed/jNQXAC9IVRw",
            "http://music.youtube.com/watch?v=jNQXAC9IVRw",
            "https://www.youtube.com/watch?v=jNQXAC9IVRw?height=480",
        ] {
            assert_eq!(id(url).as_deref(), Some("jNQXAC9IVRw"), "{url}");
        }
        for url in ["https://www.youtube.com/", "https://www.youtube.com/watch?v=short", "https://example.com/watch?v=jNQXAC9IVRw", "https://notyoutube.com/watch?v=jNQXAC9IVRw", "rtsp://www.youtube.com/watch?v=jNQXAC9IVRw", "https://youtu.be/bad$id_1234"] {
            assert_eq!(id(url), None, "{url}");
        }
        let u = SourceUrl::parse("https://youtu.be/jNQXAC9IVRw?height=480&cookies=firefox").unwrap();
        assert_eq!((u.option("height"), u.option("cookies")), (Some("480"), Some("firefox")));
    }

    #[test]
    fn the_answer_gives_the_hls_playlist_and_when_it_expires() {
        let json = br#"{"is_live":false,"formats":[
            {"format_id":"140","protocol":"https","url":"https://rr.googlevideo.com/videoplayback?expire=1"},
            {"format_id":"229","protocol":"m3u8_native","url":"https://m/x","manifest_url":"https://manifest.googlevideo.com/api/manifest/hls_variant/expire/1793433920/ei/x/file/index.m3u8"}]}"#;
        let r = parse(json, "jNQXAC9IVRw").unwrap();
        assert_eq!((r.manifest.as_str(), r.live, r.expires), ("https://manifest.googlevideo.com/api/manifest/hls_variant/expire/1793433920/ei/x/file/index.m3u8", false, 1793433920));
        let e = parse(br#"{"formats":[{"protocol":"https","url":"https://x"}]}"#, "jNQXAC9IVRw").unwrap_err();
        assert!(e.to_string().contains("no HLS playlist"), "{e}");
        assert!(explain("x", "ERROR: [youtube] x: Sign in to confirm you\u{2019}re not a bot.").to_string().contains("sign in"));
        assert_eq!(explain("x", "ERROR: [youtube] x: Video unavailable").code, ErrorCode::NotFound);
    }

    /// A real video, through yt-dlp and YouTube: `IRONAV_NET_ALLOW=any IRONAV_YOUTUBE=1`.
    #[test]
    fn a_youtube_video_streams() {
        if std::env::var("IRONAV_YOUTUBE").is_err() {
            return;
        }
        let mut s = crate::open_source("https://www.youtube.com/watch?v=jNQXAC9IVRw?height=240").unwrap();
        assert_eq!(s.info().kind, "youtube");
        let mut buf = vec![0u8; 64 << 10];
        let mut got = 0;
        let started = Instant::now();
        while got < 200_000 && started.elapsed() < Duration::from_secs(60) {
            match s.read(&mut buf, Duration::from_secs(5)).unwrap() {
                Chunk::Data(n) => got += n,
                Chunk::End => break,
                _ => {}
            }
        }
        assert!(got > 100_000, "{got} bytes");
        // a second open asks yt-dlp nothing
        let t = Instant::now();
        crate::open_source("https://youtu.be/jNQXAC9IVRw?height=240").unwrap();
        assert!(t.elapsed() < Duration::from_secs(10));
    }
}
