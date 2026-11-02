//! `http://` and `https://` sources: HLS (RFC 8216) when the URL names a playlist, a progressive download
//! otherwise.
//!
//! A master playlist's highest-bandwidth variant is played, of those whose `CODECS` list nothing
//! IronAV cannot decode (all of them when every one does) and, with the `height` option, of those no taller than that
//! (the shortest when none is). When it takes its audio from a separate
//! rendition (`EXT-X-MEDIA` of `TYPE=AUDIO` in its `AUDIO` group, with a URI), that one is played
//! with it: the one the `audio` option names by language or name, else the default, else the first.
//! The two playlists are then followed side by side, the one behind in media time fetched next, and
//! their bytes go out as a stream of renditions (`ironav_formats::renditions`), which reads each
//! with its own demuxer and merges the packets. Its segments are fetched in order and
//! their bytes given one after another, with the initialization section (`EXT-X-MAP`) in front of
//! the first segment that uses it: concatenated transport stream segments are a transport stream,
//! and fragments after their initialization section are a fragmented MP4, so the stream demuxers
//! read the result as they read any other source. A live playlist (no `EXT-X-ENDLIST`) starts three
//! segments from its end and is reloaded every target duration (half of one when nothing was
//! added); media sequence numbers keep a segment from being given twice, and segments that slid
//! out of the window before they were fetched are skipped. Byte ranges are fetched with Range
//! requests. Encrypted playlists are not supported; they are refused when the source is opened, and
//! so is a playlist reached over HTTPS that lists segments
//! over plain HTTP, which anyone on the way could change.
//!
//! A variant with subtitles (`EXT-X-MEDIA` of `TYPE=SUBTITLES` in its `SUBTITLES` group, WebVTT)
//! has them followed too, on a thread of their own: the one the `captions` option names by language
//! or name, else the default, else the first; `captions=off` takes none. Their cues come out as
//! [`Caption`]s from [`Source::captions`], with rolling captions (each cue repeating the line the
//! one before ended with, as YouTube's automatic captions do) cut to what is new.

use crate::http::{self, join, HttpUrl};
use crate::net::Allow;
use crate::source::{Caption, Chunk, Source, SourceInfo, ThreadSource};
use crate::url::{redact, SourceUrl};
use ironav_core::{Error, ErrorCode, Result};
use std::collections::VecDeque;
use std::io::Read;
use std::time::{Duration, Instant};

const MAX_PLAYLIST: usize = 4 << 20;
const MAX_INIT: usize = 16 << 20;
/// Segments a live playlist starts this far from its end (RFC 8216 §6.3.3 asks for at least three
/// target durations).
const LIVE_START: usize = 3;
/// Reloads of a live playlist that may fail in a row before the source gives up.
const MAX_RELOAD_FAILURES: u32 = 3;
/// Segments a playlist may list, at most.
const MAX_SEGMENTS: usize = 100_000;
/// A live playlist that adds nothing for this many target durations (and at least `MIN_STALL`) has
/// stopped.
const STALL_TARGETS: f64 = 10.0;
const MIN_STALL: Duration = Duration::from_secs(30);

#[derive(Debug, Clone, PartialEq)]
struct Segment {
    seq: u64,
    uri: String,
    /// Its `EXTINF` duration, in seconds.
    duration: f64,
    /// Bytes `first..=last` of the URI, from `EXT-X-BYTERANGE`.
    range: Option<(u64, u64)>,
    /// The initialization section in force: its URI and range.
    map: Option<(String, Option<(u64, u64)>)>,
}

#[derive(Debug, Clone, PartialEq, Default)]
struct Media {
    target: f64,
    segments: Vec<Segment>,
    ended: bool,
    encrypted: Option<String>,
}

/// A variant of a master playlist.
#[derive(Debug, Clone, PartialEq)]
struct Variant {
    bandwidth: u64,
    uri: String,
    /// Its `CODECS` and the height of its `RESOLUTION`, when given.
    codecs: Option<String>,
    height: Option<u32>,
    /// The `GROUP-ID` of the audio renditions it plays with.
    audio: Option<String>,
    /// The `GROUP-ID` of its subtitle renditions.
    subtitles: Option<String>,
}

/// An audio or subtitle rendition (`EXT-X-MEDIA` with `TYPE=AUDIO` or `TYPE=SUBTITLES`).
#[derive(Debug, Clone, PartialEq)]
struct Rendition {
    subtitles: bool,
    group: String,
    /// Absent when the audio is in the variant's own segments.
    uri: Option<String>,
    language: Option<String>,
    name: Option<String>,
    default: bool,
}

#[derive(Debug, Clone, PartialEq)]
enum Playlist {
    Master(Vec<Variant>, Vec<Rendition>),
    Media(Media),
}

/// `KEY=value,KEY="quoted, value"` attribute lists.
fn attributes(s: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let mut rest = s.trim();
    while let Some((k, after)) = rest.split_once('=') {
        let (v, tail) = if let Some(q) = after.strip_prefix('"') {
            match q.find('"') {
                Some(e) => (&q[..e], q[e + 1..].trim_start_matches(',')),
                None => (q, ""),
            }
        } else {
            match after.find(',') {
                Some(e) => (&after[..e], &after[e + 1..]),
                None => (after, ""),
            }
        };
        out.push((k.trim().to_ascii_uppercase(), v.to_string()));
        rest = tail.trim();
    }
    out
}

/// `length[@offset]`; without an offset the range follows `previous` (the end of the last range of
/// the same resource).
fn byte_range(s: &str, previous: Option<u64>) -> Option<(u64, u64)> {
    let (len, off) = match s.split_once('@') {
        Some((l, o)) => (l.trim().parse::<u64>().ok()?, o.trim().parse::<u64>().ok()?),
        None => (s.trim().parse::<u64>().ok()?, previous?),
    };
    (len > 0).then(|| (off, off.checked_add(len - 1)).1.map(|last| (off, last)))?
}

fn parse(text: &str, base: &str) -> Result<Playlist> {
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty());
    if lines.next() != Some("#EXTM3U") {
        return Err(Error::invalid_input("the playlist does not start with #EXTM3U"));
    }
    let mut variants = Vec::new();
    let mut renditions = Vec::new();
    let mut media = Media { target: 6.0, ..Media::default() };
    let mut seq = 0u64;
    let mut duration = 0.0;
    let mut pending_variant: Option<(u64, Option<String>, Option<String>, Option<u32>, Option<String>)> = None;
    let mut range: Option<String> = None;
    let mut map: Option<(String, Option<(u64, u64)>)> = None;
    // where the last range of each resource ended, for ranges without an offset
    let mut last_end: Option<(String, u64)> = None;
    let mut is_media = false;
    // the key in force: once any segment is encrypted, the playlist is
    let mut key: Option<String> = None;
    for line in lines {
        if let Some(tag) = line.strip_prefix('#') {
            let (name, value) = tag.split_once(':').unwrap_or((tag, ""));
            match name {
                "EXT-X-STREAM-INF" => {
                    let a = attributes(value);
                    let get = |key: &str| a.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
                    let height = get("RESOLUTION").and_then(|r| r.split_once(['x', 'X']).and_then(|(_, h)| h.parse().ok()));
                    pending_variant = Some((get("BANDWIDTH").and_then(|v| v.parse().ok()).unwrap_or(0), get("AUDIO"), get("CODECS"), height, get("SUBTITLES")));
                }
                "EXT-X-MEDIA" => {
                    let a = attributes(value);
                    let get = |key: &str| a.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone());
                    let kind = get("TYPE").unwrap_or_default();
                    let subtitles = kind.eq_ignore_ascii_case("SUBTITLES");
                    if subtitles || kind.eq_ignore_ascii_case("AUDIO") {
                        if let Some(group) = get("GROUP-ID") {
                            renditions.push(Rendition {
                                subtitles,
                                group,
                                uri: get("URI").map(|u| join(base, &u)),
                                language: get("LANGUAGE"),
                                name: get("NAME"),
                                default: get("DEFAULT").is_some_and(|d| d.eq_ignore_ascii_case("YES")),
                            });
                        }
                    }
                }
                "EXT-X-TARGETDURATION" => {
                    is_media = true;
                    media.target = value.trim().parse::<f64>().ok().filter(|t| *t > 0.0 && *t < 3600.0).unwrap_or(6.0);
                }
                "EXT-X-MEDIA-SEQUENCE" => seq = value.trim().parse().unwrap_or(0),
                "EXTINF" => {
                    is_media = true;
                    duration = value.split(',').next().and_then(|d| d.trim().parse::<f64>().ok()).filter(|d| d.is_finite() && *d >= 0.0).unwrap_or(0.0);
                }
                "EXT-X-BYTERANGE" => range = Some(value.to_string()),
                "EXT-X-ENDLIST" => media.ended = true,
                "EXT-X-KEY" => {
                    let a = attributes(value);
                    let method = a.iter().find(|(k, _)| k == "METHOD").map_or("NONE".to_string(), |(_, v)| v.clone());
                    key = (!method.eq_ignore_ascii_case("NONE")).then_some(method);
                }
                "EXT-X-MAP" => {
                    let a = attributes(value);
                    let uri = a.iter().find(|(k, _)| k == "URI").map(|(_, v)| join(base, v)).ok_or_else(|| Error::invalid_input("an EXT-X-MAP has no URI"))?;
                    let r = a.iter().find(|(k, _)| k == "BYTERANGE").and_then(|(_, v)| byte_range(v, Some(0)));
                    map = Some((uri, r));
                }
                _ => {}
            }
            continue;
        }
        let uri = join(base, line);
        if let Some((bandwidth, audio, codecs, height, subtitles)) = pending_variant.take() {
            variants.push(Variant { bandwidth, uri, audio, codecs, height, subtitles });
            continue;
        }
        let prev = last_end.as_ref().filter(|(u, _)| *u == uri).and_then(|(_, e)| e.checked_add(1));
        let r = match range.take() {
            Some(s) => Some(byte_range(&s, prev).ok_or_else(|| Error::invalid_input(format!("EXT-X-BYTERANGE '{s}' cannot be placed")))?),
            None => None,
        };
        if let Some((_, last)) = r {
            last_end = Some((uri.clone(), last));
        }
        if key.is_some() {
            media.encrypted = key.clone();
        }
        if media.segments.len() == MAX_SEGMENTS {
            return Err(Error::invalid_input(format!("the playlist lists more than {MAX_SEGMENTS} segments")));
        }
        media.segments.push(Segment { seq, uri, duration: std::mem::take(&mut duration), range: r, map: map.clone() });
        seq = seq.saturating_add(1);
    }
    if !variants.is_empty() && !is_media {
        return Ok(Playlist::Master(variants, renditions));
    }
    Ok(Playlist::Media(media))
}

fn fetch_playlist(url: &str, allow: &Allow) -> Result<(String, Playlist)> {
    let r = http::get(url, None, allow)?;
    let at = r.url.clone();
    let text = String::from_utf8_lossy(&r.bytes(MAX_PLAYLIST)?).into_owned();
    let p = parse(&text, &at)?;
    Ok((at, p))
}

fn check(media: &Media, shown: &str) -> Result<()> {
    if let Some(m) = &media.encrypted {
        return Err(Error::new(ErrorCode::UnsupportedFormat, format!("{shown} is encrypted ({m}), which IronAV does not decrypt")));
    }
    // a source opened over TLS keeps every segment under TLS
    let tls = shown.starts_with("https://");
    let web = |u: &str| u.starts_with("https://") || (!tls && u.starts_with("http://"));
    if let Some(s) = media.segments.iter().find(|s| !web(&s.uri) || s.map.as_ref().is_some_and(|(u, _)| !web(u))) {
        return Err(Error::new(ErrorCode::UnsupportedFormat, format!("{shown} has a segment at {}, which is not {}", redact(&s.uri), if tls { "HTTPS" } else { "HTTP" })));
    }
    Ok(())
}

/// Opens an `http://` or `https://` source: HLS when the response is a playlist, the body itself otherwise.
pub(crate) fn open(u: &SourceUrl, shown: String) -> Result<Box<dyn Source>> {
    let allow = Allow::from_env()?;
    let url = format!("{}://{}", u.scheme, u.target);
    HttpUrl::parse(&url)?;
    let mut r = http::get(&url, None, &allow)?;
    let mut head = vec![0u8; 0];
    let mut chunk = [0u8; 4096];
    while head.len() < 7 {
        let n = r.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        head.extend_from_slice(&chunk[..n]);
    }
    // the body decides: a server may call anything a playlist
    if !head.starts_with(b"#EXTM3U") {
        let info = SourceInfo { url: shown, kind: "http", live: true, synthetic: false };
        return Ok(Box::new(ThreadSource::new(info, Box::new(std::io::Cursor::new(head).chain(r)))));
    }
    let at = r.url.clone();
    let mut text = head;
    text.extend(r.bytes(MAX_PLAYLIST)?);
    let mut playlist = parse(&String::from_utf8_lossy(&text), &at)?;
    let mut playlist_url = at;
    let mut audio = None;
    let mut captions = None;
    if let Playlist::Master(variants, renditions) = &playlist {
        let height = u.option("height").map(|h| h.parse::<u32>().map_err(|_| Error::invalid_arg(format!("height must be a number of lines, got '{h}'")))).transpose()?;
        let best = pick_variant(variants, height);
        audio = pick_audio(&best, renditions, u.option("audio"), &shown)?;
        captions = pick_captions(&best, renditions, u.option("captions"), &shown)?;
        let (at, p) = fetch_playlist(&best.uri, &allow)?;
        if matches!(p, Playlist::Master(..)) {
            return Err(Error::invalid_input(format!("{shown}: a variant of the master playlist is itself a master playlist")));
        }
        playlist = p;
        playlist_url = at;
    } else if u.option("audio").is_some() {
        return Err(Error::invalid_arg(format!("{shown} is a media playlist: it has no audio renditions to choose from")));
    }
    let source = open_media(playlist, playlist_url, audio, &allow, &shown)?;
    let Some((uri, language)) = captions else { return Ok(source) };
    let (tx, rx) = std::sync::mpsc::channel();
    let follow = (allow, shown);
    std::thread::Builder::new()
        .name("ironav-captions".into())
        .spawn(move || {
            if let Err(e) = follow_captions(&uri, language, &follow.0, &tx) {
                let _ = tx.send(Err(format!("{}: captions: {}", follow.1, e.message)));
            }
        })
        .map_err(|e| Error::new(ErrorCode::Io, format!("cannot start the captions thread: {e}")))?;
    Ok(Box::new(Captioned { inner: source, rx }))
}

/// The media of a variant (or a media playlist), with its audio rendition when it has one.
fn open_media(playlist: Playlist, playlist_url: String, audio: Option<String>, allow: &Allow, shown: &str) -> Result<Box<dyn Source>> {
    let shown = shown.to_string();
    let allow = allow.clone();
    let Playlist::Media(media) = playlist else { unreachable!() };
    let video = HlsReader::new(allow.clone(), playlist_url, shown.clone(), media)?;
    let info = SourceInfo { url: shown.clone(), kind: "hls", live: true, synthetic: false };
    let Some(audio_url) = audio else {
        return Ok(Box::new(ThreadSource::new(info, Box::new(video))));
    };
    let media = match fetch_playlist(&audio_url, &allow)? {
        (at, Playlist::Media(m)) => (at, m),
        _ => return Err(Error::invalid_input(format!("{shown}: an audio rendition is a master playlist"))),
    };
    let audio = HlsReader::new(allow, media.0, shown, media.1)?;
    Ok(Box::new(ThreadSource::new(info, Box::new(Renditions { parts: [video, audio], done: [false; 2], out: std::io::Cursor::new(ironav_formats::renditions::MAGIC.to_vec()) }))))
}

/// The URI and language of the subtitle rendition to follow with `variant`: the one `wanted` names
/// by language or name, else the default, else the first; none for `off`.
fn pick_captions(variant: &Variant, renditions: &[Rendition], wanted: Option<&str>, shown: &str) -> Result<Option<(String, Option<String>)>> {
    if wanted.is_some_and(|w| ["off", "none", "0", "no", "false"].iter().any(|o| w.eq_ignore_ascii_case(o))) {
        return Ok(None);
    }
    let group: Vec<&Rendition> = variant.subtitles.as_ref().map_or(Vec::new(), |g| renditions.iter().filter(|r| r.subtitles && &r.group == g && r.uri.is_some()).collect());
    let found = match wanted {
        Some(w) => {
            let is = |s: &Option<String>| s.as_ref().is_some_and(|s| s.eq_ignore_ascii_case(w));
            let found = group.iter().find(|r| is(&r.language)).or_else(|| group.iter().find(|r| is(&r.name)));
            if found.is_none() {
                let names = group.iter().filter_map(|r| r.language.clone().or_else(|| r.name.clone()));
                return Err(Error::invalid_arg(format!("{shown} has no captions '{w}'")).alternatives(names.chain(["off".to_string()])));
            }
            found
        }
        None => group.iter().find(|r| r.default).or(group.first()),
    };
    Ok(found.map(|r| (r.uri.clone().unwrap(), r.language.clone())))
}

/// The largest WebVTT segment taken.
const MAX_CAPTION_SEGMENT: usize = 4 << 20;

/// Follows a subtitle playlist, sending its cues as their segments come, until it ends or no one
/// takes them.
fn follow_captions(url: &str, language: Option<String>, allow: &Allow, tx: &std::sync::mpsc::Sender<std::result::Result<Caption, String>>) -> Result<()> {
    let mut next_seq = None;
    let mut last: Option<crate::webvtt::Cue> = None;
    loop {
        let (_, p) = fetch_playlist(url, allow)?;
        let Playlist::Media(media) = p else {
            return Err(Error::invalid_input("a subtitle rendition is a master playlist"));
        };
        if media.encrypted.is_some() {
            return Err(Error::new(ErrorCode::UnsupportedFormat, "encrypted subtitles are not supported"));
        }
        // a live playlist starts near its end, as its media does
        let start = next_seq.unwrap_or_else(|| if media.ended { 0 } else { media.segments.iter().rev().nth(2).map_or(0, |s| s.seq) });
        for seg in media.segments.iter().filter(|s| s.seq >= start) {
            let body = http::get(&seg.uri, seg.range, allow)?.bytes(MAX_CAPTION_SEGMENT)?;
            let cues = crate::webvtt::parse(&String::from_utf8_lossy(&body))?;
            // the last cue of the segment before goes first, so a cue rolling over from it is cut too
            let had = last.is_some() as usize;
            let cues: Vec<_> = last.take().into_iter().chain(cues).collect();
            last = cues.last().cloned();
            for c in crate::webvtt::dedupe_rolling(cues).into_iter().skip(had) {
                if tx.send(Ok(Caption { start: c.start, end: c.end, text: c.text, language: language.clone() })).is_err() {
                    return Ok(());
                }
            }
            next_seq = Some(seg.seq + 1);
        }
        if media.ended {
            return Ok(());
        }
        std::thread::sleep(Duration::from_secs_f64(media.target / 2.0));
        // nobody reads them any more
        if tx.send(Err(String::new())).is_err() {
            return Ok(());
        }
    }
}

/// A source with captions alongside.
struct Captioned {
    inner: Box<dyn Source>,
    rx: std::sync::mpsc::Receiver<std::result::Result<Caption, String>>,
}

impl Source for Captioned {
    fn info(&self) -> &SourceInfo {
        self.inner.info()
    }

    fn read(&mut self, buf: &mut [u8], wait: Duration) -> Result<Chunk> {
        self.inner.read(buf, wait)
    }

    fn health(&self) -> Option<crate::reconnect::Health> {
        self.inner.health()
    }

    fn captions(&mut self) -> Vec<std::result::Result<Caption, String>> {
        // empty errors are the follower checking that someone still listens
        self.rx.try_iter().filter(|c| !matches!(c, Err(e) if e.is_empty())).collect()
    }
}

/// Codecs (by their `CODECS` names' first part) that IronAV decodes.
const DECODED: [&str; 8] = ["avc1", "avc3", "hvc1", "hev1", "vp09", "vp8", "mp4a", "opus"];

/// The variant to play: the highest bandwidth of those that list no codec IronAV does not decode
/// (or of all, when every one does), and with `height`, of those no taller (the shortest when none is).
fn pick_variant(variants: &[Variant], height: Option<u32>) -> Variant {
    let decoded = |v: &&Variant| v.codecs.as_ref().is_none_or(|c| c.split(',').all(|c| DECODED.iter().any(|d| c.trim().starts_with(d))));
    let mut pool: Vec<&Variant> = variants.iter().filter(decoded).collect();
    if pool.is_empty() {
        pool = variants.iter().collect();
    }
    if let Some(h) = height {
        let fits: Vec<&Variant> = pool.iter().copied().filter(|v| v.height.is_none_or(|vh| vh <= h)).collect();
        pool = if fits.is_empty() {
            let least = pool.iter().filter_map(|v| v.height).min();
            pool.into_iter().filter(|v| v.height == least).collect()
        } else {
            fits
        };
    }
    pool.into_iter().max_by_key(|v| v.bandwidth).cloned().expect("a master playlist has variants")
}

/// The URI of the audio rendition to play with `variant`: the one `wanted` names (by language or
/// name), else the default, else the first; `None` when its audio is in its own segments.
fn pick_audio(variant: &Variant, renditions: &[Rendition], wanted: Option<&str>, shown: &str) -> Result<Option<String>> {
    let group: Vec<&Rendition> = variant.audio.as_ref().map_or(Vec::new(), |g| renditions.iter().filter(|r| !r.subtitles && &r.group == g).collect());
    if let Some(w) = wanted {
        let is = |s: &Option<String>| s.as_ref().is_some_and(|s| s.eq_ignore_ascii_case(w));
        let found = group.iter().find(|r| is(&r.language)).or_else(|| group.iter().find(|r| is(&r.name)));
        return match found {
            Some(r) => Ok(r.uri.clone()),
            None => {
                let names = group.iter().filter_map(|r| r.language.clone().or_else(|| r.name.clone()));
                Err(Error::invalid_arg(format!("{shown} has no audio rendition '{w}'")).alternatives(names))
            }
        };
    }
    Ok(group.iter().find(|r| r.default).or(group.first()).and_then(|r| r.uri.clone()))
}

/// Two playlists followed side by side (a variant and its audio rendition), the one behind in media
/// time read next, their bytes given as pieces of a stream of renditions.
struct Renditions {
    parts: [HlsReader; 2],
    done: [bool; 2],
    out: std::io::Cursor<Vec<u8>>,
}

impl Read for Renditions {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let n = self.out.read(buf)?;
            if n > 0 {
                return Ok(n);
            }
            let Some(k) = (0..2).filter(|&k| !self.done[k]).min_by(|&a, &b| self.parts[a].fed.total_cmp(&self.parts[b].fed)) else {
                return Ok(0);
            };
            let mut chunk = vec![0u8; 64 << 10];
            let n = match self.parts[k].read(&mut chunk) {
                Ok(n) => n,
                // the audio rendition failing leaves the video playing
                Err(_) if k == 1 => 0,
                Err(e) => return Err(e),
            };
            if n == 0 {
                // an empty piece: the part has ended
                self.done[k] = true;
                self.out = std::io::Cursor::new(ironav_formats::renditions::piece(k as u8, &[]));
                continue;
            }
            self.out = std::io::Cursor::new(ironav_formats::renditions::piece(k as u8, &chunk[..n]));
        }
    }
}

/// Gives the segments' bytes in order, reloading a live playlist as it goes (on the source's thread).
struct HlsReader {
    /// Seconds of media in the segments started so far.
    fed: f64,
    allow: Allow,
    playlist_url: String,
    shown: String,
    target: f64,
    ended: bool,
    next_seq: u64,
    queue: VecDeque<Segment>,
    current: Option<http::Response>,
    init: std::io::Cursor<Vec<u8>>,
    map: Option<(String, Option<(u64, u64)>)>,
    last_load: Instant,
    /// Whether the last reload added segments.
    grew: bool,
    failures: u32,
    /// When a reload last added a segment.
    progress: Instant,
}

impl HlsReader {
    fn new(allow: Allow, playlist_url: String, shown: String, media: Media) -> Result<HlsReader> {
        check(&media, &shown)?;
        let start = if media.ended { 0 } else { media.segments.len().saturating_sub(LIVE_START) };
        Ok(HlsReader {
            fed: 0.0,
            allow,
            playlist_url,
            shown,
            target: media.target,
            ended: media.ended,
            // the queue holds everything from `start` on, so only later segments are new
            next_seq: media.segments.last().map_or(0, |s| s.seq.saturating_add(1)),
            queue: media.segments[start..].iter().cloned().collect(),
            current: None,
            init: std::io::Cursor::new(Vec::new()),
            map: None,
            last_load: Instant::now(),
            grew: true,
            failures: 0,
            progress: Instant::now(),
        })
    }
}

fn io_err(e: Error) -> std::io::Error {
    std::io::Error::other(e.to_string())
}

impl HlsReader {
    fn reload(&mut self) -> Result<()> {
        let wait = Duration::from_secs_f64(if self.grew { self.target } else { self.target / 2.0 }.max(0.5));
        let due = self.last_load + wait;
        let now = Instant::now();
        if due > now {
            std::thread::sleep(due - now);
        }
        self.last_load = Instant::now();
        let media = match fetch_playlist(&self.playlist_url, &self.allow) {
            Ok((_, Playlist::Media(m))) => m,
            Ok(_) => return Err(Error::invalid_input(format!("{} became a master playlist", self.shown))),
            Err(e) => {
                self.failures += 1;
                return if self.failures >= MAX_RELOAD_FAILURES { Err(e) } else { Ok(()) };
            }
        };
        self.failures = 0;
        check(&media, &self.shown)?;
        self.target = media.target;
        self.ended = media.ended;
        let mut segments = media.segments;
        if segments.last().is_some_and(|s| s.seq.saturating_add(1) < self.next_seq) {
            // the numbering went back: the packager restarted, so start again near the end
            let start = segments.len().saturating_sub(LIVE_START);
            segments.drain(..start);
            self.next_seq = segments[0].seq;
        }
        let fresh: Vec<Segment> = segments.into_iter().filter(|s| s.seq >= self.next_seq).collect();
        self.grew = !fresh.is_empty();
        if let Some(last) = fresh.last() {
            self.next_seq = last.seq.saturating_add(1);
            self.progress = Instant::now();
        } else if !self.ended && self.progress.elapsed() > Duration::from_secs_f64(self.target * STALL_TARGETS).max(MIN_STALL) {
            return Err(Error::new(ErrorCode::Io, format!("{} stopped adding segments", self.shown)));
        }
        self.queue.extend(fresh);
        Ok(())
    }
}

impl Read for HlsReader {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        loop {
            let n = self.init.read(buf)?;
            if n > 0 {
                return Ok(n);
            }
            if let Some(r) = self.current.as_mut() {
                match r.read(buf) {
                    Ok(0) => self.current = None,
                    Ok(n) => return Ok(n),
                    // a live segment that broke off: go on with the next one
                    Err(_) if !self.ended => self.current = None,
                    Err(e) => return Err(e),
                }
                continue;
            }
            if let Some(seg) = self.queue.pop_front() {
                // a segment without a usable duration counts as a target duration, so the other
                // playlist is not starved
                self.fed += if seg.duration > 0.0 { seg.duration } else { self.target };
                if seg.map.is_some() && seg.map != self.map {
                    let (uri, range) = seg.map.clone().unwrap();
                    match http::get(&uri, range, &self.allow).and_then(|r| r.bytes(MAX_INIT)) {
                        Ok(bytes) => {
                            self.init = std::io::Cursor::new(bytes);
                            self.map = seg.map.clone();
                        }
                        // live: the segment cannot be read without it, so it is skipped too
                        Err(_) if !self.ended => continue,
                        Err(e) => return Err(io_err(e)),
                    }
                }
                match http::get(&seg.uri, seg.range, &self.allow) {
                    Ok(r) => self.current = Some(r),
                    // a live segment that is gone already: skip it
                    Err(_) if !self.ended => {}
                    Err(e) => return Err(io_err(e)),
                }
                continue;
            }
            if self.ended {
                return Ok(0);
            }
            self.reload().map_err(io_err)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::Chunk;
    use crate::md5::md5_hex;
    use std::collections::HashMap;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::sync::{Arc, Mutex};

    type Routes = Arc<Mutex<HashMap<String, Vec<u8>>>>;

    /// Serves `routes` (path to body), with Range requests and redirects (a body of `>target`);
    /// `on_get` may change the routes before each answer, as a live packager does.
    fn server(routes: Routes, on_get: impl Fn(&str, &Routes) + Send + 'static) -> u16 {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        std::thread::spawn(move || {
            for c in l.incoming() {
                let Ok(c) = c else { return };
                let mut r = BufReader::new(c.try_clone().unwrap());
                let mut first = String::new();
                if r.read_line(&mut first).is_err() {
                    continue;
                }
                let path = first.split_whitespace().nth(1).unwrap_or("/").to_string();
                let mut range = None;
                let mut authorized = false;
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                        break;
                    }
                    // u:p, answered with Digest: Basic would send the password in the clear over this plain HTTP
                    authorized |= h.trim_start().starts_with("Authorization: Digest ") && h.contains("username=\"u\"");
                    if let Some(v) = h.strip_prefix("Range: bytes=") {
                        let (a, b) = v.trim().split_once('-').unwrap();
                        range = Some((a.parse::<usize>().unwrap(), b.parse::<usize>().unwrap()));
                    }
                }
                let mut c = c;
                // everything under /r/ needs credentials, asked for by a challenge
                if path.starts_with("/r/") && !authorized {
                    let _ = write!(c, "HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Digest realm=\"r\", qop=\"auth\", nonce=\"n\"\r\nContent-Length: 0\r\n\r\n");
                    continue;
                }
                on_get(&path, &routes);
                let body = routes.lock().unwrap().get(&path).cloned();
                match body {
                    Some(b) if b.starts_with(b">") => {
                        let _ = write!(c, "HTTP/1.1 302 Found\r\nLocation: {}\r\nContent-Length: 0\r\n\r\n", String::from_utf8_lossy(&b[1..]));
                    }
                    Some(b) => {
                        let (status, part, extra) = match range {
                            Some((a, z)) => ("206 Partial Content", b[a..=z.min(b.len() - 1)].to_vec(), format!("Content-Range: bytes {a}-{}/{}\r\n", z.min(b.len() - 1), b.len())),
                            None => ("200 OK", b, String::new()),
                        };
                        // playlists chunked, media by length, as servers mix them
                        if path.ends_with(".m3u8") {
                            let _ = write!(c, "HTTP/1.1 {status}\r\nTransfer-Encoding: chunked\r\n\r\n");
                            for ch in part.chunks(100) {
                                let _ = write!(c, "{:x}\r\n", ch.len());
                                let _ = c.write_all(ch);
                                let _ = c.write_all(b"\r\n");
                            }
                            let _ = c.write_all(b"0\r\n\r\n");
                        } else {
                            // an interim answer first, which the client must look past
                            let _ = write!(c, "HTTP/1.1 103 Early Hints\r\nLink: </x>; rel=preload\r\n\r\n");
                            let _ = write!(c, "HTTP/1.1 {status}\r\n{extra}Content-Length: {}\r\n\r\n", part.len());
                            let _ = c.write_all(&part);
                        }
                    }
                    None => {
                        let _ = write!(c, "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n");
                    }
                }
            }
        });
        port
    }

    fn drain(url: &str, limit: Duration) -> Result<Vec<u8>> {
        let mut s = crate::open_source(url)?;
        let mut got = Vec::new();
        let mut buf = vec![0u8; 65536];
        let start = Instant::now();
        while start.elapsed() < limit {
            match s.read(&mut buf, Duration::from_millis(50))? {
                Chunk::Data(n) => got.extend_from_slice(&buf[..n]),
                Chunk::Idle | Chunk::Restart => {}
                Chunk::End => break,
            }
        }
        Ok(got)
    }

    fn fixture(name: &str) -> Vec<u8> {
        std::fs::read(format!("{}/../ironav-formats/tests/data/{name}", env!("CARGO_MANIFEST_DIR"))).unwrap()
    }

    #[test]
    fn a_master_playlist_plays_its_best_variant_through_a_redirect() {
        let ts = fixture("h264_aac.ts");
        let packets = ts.len() / 188;
        let cut = [0, packets / 3 * 188, packets * 2 / 3 * 188, ts.len()];
        let routes: Routes = Default::default();
        {
            let mut r = routes.lock().unwrap();
            r.insert("/master.m3u8".into(), b"#EXTM3U\n#EXT-X-STREAM-INF:BANDWIDTH=100000,CODECS=\"avc1,mp4a\"\nlow/index.m3u8\n#EXT-X-STREAM-INF:BANDWIDTH=900000\nhigh/index.m3u8\n".to_vec());
            r.insert("/low/index.m3u8".into(), b"#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\n/nothing.ts\n#EXT-X-ENDLIST\n".to_vec());
            r.insert("/high/index.m3u8".into(), b">/moved/index.m3u8".to_vec());
            r.insert("/moved/index.m3u8".into(), b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:7\n#EXTINF:1.0,\ns0.ts\n#EXTINF:1.0,\ns1.ts\n#EXT-X-DISCONTINUITY\n#EXTINF:1.0,\n../moved/s2.ts\n#EXT-X-ENDLIST\n".to_vec());
            for k in 0..3 {
                r.insert(format!("/moved/s{k}.ts"), ts[cut[k]..cut[k + 1]].to_vec());
            }
        }
        let port = server(routes, |_, _| {});
        let got = drain(&format!("http://127.0.0.1:{port}/master.m3u8"), Duration::from_secs(10)).unwrap();
        assert!(got == ts, "the segments, in order, are the stream");
    }

    #[test]
    fn the_variant_is_one_that_decodes_and_fits_the_height() {
        let text = "#EXTM3U\n\
#EXT-X-STREAM-INF:BANDWIDTH=300000,CODECS=\"avc1.4D400C,mp4a.40.2\",RESOLUTION=320x240\nlow.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=2000000,CODECS=\"avc1.640028,mp4a.40.2\",RESOLUTION=1280x720\nhd.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=900000,CODECS=\"vp09.00.20.08,mp4a.40.2\",RESOLUTION=640x360\nvp9.m3u8\n\
#EXT-X-STREAM-INF:BANDWIDTH=5000000,CODECS=\"av01.0.08M.08,mp4a.40.2\",RESOLUTION=1920x1080\nav1.m3u8\n";
        let Playlist::Master(v, _) = parse(text, "https://h/m.m3u8").unwrap() else { panic!("a master playlist") };
        let uri = |h| pick_variant(&v, h).uri;
        // AV1 is not decoded, so the 720p H.264 one
        assert_eq!(uri(None), "https://h/hd.m3u8");
        assert_eq!(uri(Some(480)), "https://h/vp9.m3u8");
        assert_eq!(uri(Some(100)), "https://h/low.m3u8");
    }

    #[test]
    fn a_variant_plays_with_its_audio_rendition() {
        let ts = fixture("h264_aac.ts");
        let aac = std::fs::read(format!("{}/../ironav-engine/tests/data/aac_stereo_tools.aac", env!("CARGO_MANIFEST_DIR"))).unwrap();
        let routes: Routes = Default::default();
        {
            let mut r = routes.lock().unwrap();
            r.insert(
                "/master.m3u8".into(),
                b"#EXTM3U
#EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=YES,URI=\"en/a.m3u8\"
                  #EXT-X-MEDIA:TYPE=AUDIO,GROUP-ID=\"aud\",NAME=\"Francais\",LANGUAGE=\"fr\",URI=\"fr/a.m3u8\"
                  #EXT-X-STREAM-INF:BANDWIDTH=500000,AUDIO=\"aud\"
v.m3u8
"
                    .to_vec(),
            );
            r.insert("/v.m3u8".into(), b"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXTINF:2.0,
v0.ts
#EXT-X-ENDLIST
".to_vec());
            r.insert("/v0.ts".into(), ts.clone());
            for l in ["en", "fr"] {
                r.insert(format!("/{l}/a.m3u8"), b"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXTINF:2.0,
a0.aac
#EXT-X-ENDLIST
".to_vec());
                r.insert(format!("/{l}/a0.aac"), aac.clone());
            }
        }
        let asked: Arc<Mutex<Vec<String>>> = Default::default();
        let log = asked.clone();
        let port = server(routes, move |p, _| log.lock().unwrap().push(p.to_string()));
        let url = format!("http://127.0.0.1:{port}/master.m3u8");
        let got = drain(&url, Duration::from_secs(10)).unwrap();
        // one stream of both: the transport stream's video and audio, then the rendition's audio
        let mut d = ironav_formats::stream::stream_demuxer(&got[..200], &Default::default()).unwrap();
        d.feed(&got).unwrap();
        d.end();
        let mut n = [0; 3];
        while let Some(p) = d.next_packet().unwrap() {
            n[p.packet.stream] += 1;
        }
        assert_eq!(d.streams().len(), 3);
        assert!(n.iter().all(|&k| k > 0), "{n:?}");
        assert!(asked.lock().unwrap().contains(&"/en/a0.aac".to_string()), "the default rendition");
        // another, by language or name, and one that is not there
        asked.lock().unwrap().clear();
        drain(&format!("{url}?audio=FR"), Duration::from_secs(10)).unwrap();
        assert!(asked.lock().unwrap().contains(&"/fr/a0.aac".to_string()));
        let e = drain(&format!("{url}?audio=de"), Duration::from_secs(10)).unwrap_err();
        assert_eq!(e.code, ErrorCode::InvalidArgument);
        assert!(e.alternatives.contains(&"en".to_string()), "{e:?}");
    }

    #[test]
    fn a_variant_brings_its_captions() -> Result<()> {
        let routes: Routes = Default::default();
        {
            let mut r = routes.lock().unwrap();
            r.insert("/master.m3u8".into(), b"#EXTM3U
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"vtt\",NAME=\"English\",LANGUAGE=\"en\",DEFAULT=NO,URI=\"en/s.m3u8\"
#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"vtt\",NAME=\"Deutsch\",LANGUAGE=\"de\",DEFAULT=NO,URI=\"de/s.m3u8\"
#EXT-X-STREAM-INF:BANDWIDTH=500000,SUBTITLES=\"vtt\"
v.m3u8
".to_vec());
            r.insert("/v.m3u8".into(), b"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXTINF:2.0,
v0.ts
#EXT-X-ENDLIST
".to_vec());
            r.insert("/v0.ts".into(), fixture("h264_aac.ts"));
            for l in ["en", "de"] {
                r.insert(format!("/{l}/s.m3u8"), b"#EXTM3U
#EXT-X-TARGETDURATION:2
#EXTINF:1.0,
s0.vtt
#EXTINF:1.0,
s1.vtt
#EXT-X-ENDLIST
".to_vec());
                // the second segment's cue rolls over from the first's
                r.insert(format!("/{l}/s0.vtt"), format!("WEBVTT

00:00.100 --> 00:00.900
{l} one
").into_bytes());
                r.insert(format!("/{l}/s1.vtt"), format!("WEBVTT

00:01.000 --> 00:01.800
{l} one
{l} <c>two</c>
").into_bytes());
            }
        }
        let port = server(routes, |_, _| {});
        let url = format!("http://127.0.0.1:{port}/master.m3u8");
        let captions = |q: &str| -> Result<Vec<Caption>> {
            let mut s = crate::open_source(&format!("{url}{q}"))?;
            let mut buf = vec![0u8; 65536];
            let mut got = Vec::new();
            // until both cues are in, or a second after the media ends
            let (start, mut ended) = (Instant::now(), None::<Instant>);
            while start.elapsed() < Duration::from_secs(10) && got.len() < 2 && ended.is_none_or(|t| t.elapsed() < Duration::from_secs(1)) {
                got.extend(s.captions().into_iter().map(|c| c.unwrap()));
                if ended.is_none() && matches!(s.read(&mut buf, Duration::from_millis(50))?, Chunk::End) {
                    ended = Some(Instant::now());
                }
                std::thread::sleep(Duration::from_millis(5));
            }
            Ok(got)
        };
        // none is the default, so the first; each cue says only what is new
        let c = captions("")?;
        assert_eq!(c.iter().map(|c| (c.start, c.text.as_str())).collect::<Vec<_>>(), [(0.1, "en one"), (1.0, "en two")]);
        assert_eq!(c[0].language.as_deref(), Some("en"));
        assert_eq!(captions("?captions=Deutsch")?[1].text, "de two");
        assert!(captions("?captions=off")?.is_empty());
        let e = captions("?captions=fr").unwrap_err();
        assert!(e.alternatives.contains(&"de".to_string()) && e.alternatives.contains(&"off".to_string()), "{e:?}");
        Ok(())
    }

    #[test]
    fn byte_ranges_and_an_initialization_section_make_a_fragmented_mp4() {
        let mp4 = fixture("h264_aac_frag.mp4");
        let moofs: Vec<usize> = mp4.windows(4).enumerate().filter(|(_, w)| *w == b"moof").map(|(i, _)| i - 4).collect();
        let mut pl = format!("#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-TARGETDURATION:2\n#EXT-X-MAP:URI=\"all.mp4\",BYTERANGE=\"{}@0\"\n", moofs[0]);
        let ends: Vec<usize> = moofs[1..].iter().copied().chain([mp4.len()]).collect();
        for (k, (&a, &z)) in moofs.iter().zip(&ends).enumerate() {
            // the first range has an offset, the rest follow on
            if k == 0 {
                pl += &format!("#EXTINF:1,\n#EXT-X-BYTERANGE:{}@{a}\nall.mp4\n", z - a);
            } else {
                pl += &format!("#EXTINF:1,\n#EXT-X-BYTERANGE:{}\nall.mp4\n", z - a);
            }
        }
        pl += "#EXT-X-ENDLIST\n";
        let routes: Routes = Default::default();
        routes.lock().unwrap().insert("/v/index.m3u8".into(), pl.into_bytes());
        routes.lock().unwrap().insert("/v/all.mp4".into(), mp4.clone());
        let port = server(routes, |_, _| {});
        let got = drain(&format!("http://127.0.0.1:{port}/v/index.m3u8"), Duration::from_secs(10)).unwrap();
        assert!(got == mp4);
    }

    #[test]
    fn a_live_playlist_is_followed_without_repeats() {
        // a window of three segments that slides by one every time the playlist is fetched
        let routes: Routes = Default::default();
        for k in 0..12 {
            routes.lock().unwrap().insert(format!("/live/s{k}.ts"), format!("[segment {k}]").into_bytes());
        }
        let fetches = Arc::new(Mutex::new(0u64));
        let f = fetches.clone();
        let port = server(routes, move |path, routes| {
            if path == "/live/index.m3u8" {
                let mut n = f.lock().unwrap();
                let first = 2 + *n;
                *n += 1;
                let mut pl = format!("#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:{first}\n");
                for k in first..first + 3 {
                    pl += &format!("#EXTINF:0.5,\ns{k}.ts\n");
                }
                if first + 3 >= 10 {
                    pl += "#EXT-X-ENDLIST\n";
                }
                routes.lock().unwrap().insert(path.into(), pl.into_bytes());
            }
        });
        let got = drain(&format!("http://127.0.0.1:{port}/live/index.m3u8"), Duration::from_secs(20)).unwrap();
        let want: String = (2..10).map(|k| format!("[segment {k}]")).collect();
        assert_eq!(String::from_utf8_lossy(&got), want);
    }

    #[test]
    fn a_packager_restart_is_followed_and_credentials_reach_every_request() {
        // numbering 100.. for two fetches, then back to 0 (the packager restarted)
        let routes: Routes = Default::default();
        for k in (0..4).chain(100..104) {
            routes.lock().unwrap().insert(format!("/r/s{k}.ts"), format!("[{k}]").into_bytes());
        }
        let fetches = Arc::new(Mutex::new(0u64));
        let f = fetches.clone();
        let port = server(routes, move |path, routes| {
            if path == "/r/index.m3u8" {
                let mut n = f.lock().unwrap();
                let (first, end) = match *n { 0 => (100, false), 1 => (101, false), _ => (0, true) };
                *n += 1;
                let mut pl = format!("#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXT-X-MEDIA-SEQUENCE:{first}\n");
                for k in first..first + 3 {
                    pl += &format!("#EXTINF:0.5,\ns{k}.ts\n");
                }
                if end {
                    pl += "#EXT-X-ENDLIST\n";
                }
                routes.lock().unwrap().insert(path.into(), pl.into_bytes());
            }
        });
        let got = drain(&format!("http://u:p@127.0.0.1:{port}/r/index.m3u8"), Duration::from_secs(20)).unwrap();
        assert_eq!(String::from_utf8_lossy(&got), "[100][101][102][103][0][1][2]");
    }

    /// A Digest server for user "u" (realm "r", MD5, qop auth) with password `pass`, serving `routes`.
    /// The request for `rotate`, if any, first moves it to nonce "n2", which makes the client's nonce
    /// stale. Each request is logged as "path status nonce nc", status being challenge, stale, ok or bad.
    fn digest_server(routes: Routes, pass: &'static str, rotate: Option<&'static str>) -> (u16, Arc<Mutex<Vec<String>>>) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = l.local_addr().unwrap().port();
        let log: Arc<Mutex<Vec<String>>> = Default::default();
        let seen = log.clone();
        std::thread::spawn(move || {
            let mut nonce = "n1";
            for c in l.incoming() {
                let Ok(mut c) = c else { return };
                let mut r = BufReader::new(c.try_clone().unwrap());
                let mut first = String::new();
                if r.read_line(&mut first).is_err() {
                    continue;
                }
                let path = first.split_whitespace().nth(1).unwrap_or("/").to_string();
                if rotate == Some(path.as_str()) {
                    nonce = "n2";
                }
                let mut auth = String::new();
                loop {
                    let mut h = String::new();
                    if r.read_line(&mut h).unwrap_or(0) == 0 || h.trim().is_empty() {
                        break;
                    }
                    if let Some(v) = h.trim().strip_prefix("Authorization: Digest ") {
                        auth = v.to_string();
                    }
                }
                let p: HashMap<String, String> = auth.split(", ").filter_map(|kv| kv.split_once('=')).map(|(k, v)| (k.to_string(), v.trim_matches('"').to_string())).collect();
                let get = |k: &str| p.get(k).cloned().unwrap_or_default();
                let (client_nonce, nc, cnonce) = (get("nonce"), get("nc"), get("cnonce"));
                let ha1 = md5_hex(format!("u:r:{pass}").as_bytes());
                let ha2 = md5_hex(format!("GET:{path}").as_bytes());
                let want = md5_hex(format!("{ha1}:{client_nonce}:{nc}:{cnonce}:auth:{ha2}").as_bytes());
                let status = if auth.is_empty() {
                    "challenge"
                } else if client_nonce != nonce {
                    "stale"
                } else if get("username") == "u" && get("response") == want && get("uri") == path {
                    "ok"
                } else {
                    "bad"
                };
                seen.lock().unwrap().push(format!("{path} {status} {client_nonce} {nc}"));
                let challenge = |extra: &str| format!("HTTP/1.1 401 Unauthorized\r\nWWW-Authenticate: Digest realm=\"r\", qop=\"auth\", algorithm=MD5, opaque=\"o\", nonce=\"{nonce}\"{extra}\r\nContent-Length: 0\r\n\r\n");
                let _ = match status {
                    "ok" => match routes.lock().unwrap().get(&path).cloned() {
                        Some(b) => {
                            let mut resp = format!("HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\r\n", b.len()).into_bytes();
                            resp.extend(b);
                            c.write_all(&resp)
                        }
                        None => c.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n"),
                    },
                    "stale" => c.write_all(challenge(", stale=true").as_bytes()),
                    _ => c.write_all(challenge("").as_bytes()),
                };
            }
        });
        (port, log)
    }

    #[test]
    fn digest_is_answered_once_and_the_nonce_is_reused() {
        let routes: Routes = Default::default();
        {
            let mut r = routes.lock().unwrap();
            r.insert("/d/index.m3u8".into(), b"#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\ns0.ts\n#EXTINF:1,\ns1.ts\n#EXT-X-ENDLIST\n".to_vec());
            r.insert("/d/s0.ts".into(), b"A".to_vec());
            r.insert("/d/s1.ts".into(), b"B".to_vec());
        }
        let (port, log) = digest_server(routes, "p", None);
        let got = drain(&format!("http://u:p@127.0.0.1:{port}/d/index.m3u8"), Duration::from_secs(10)).unwrap();
        assert_eq!(got, b"AB");
        let log = log.lock().unwrap().clone();
        assert_eq!(log.iter().filter(|l| l.contains(" challenge ")).count(), 1, "{log:?}");
        // the requests after the challenge all carry its nonce, with the count going up by one each time
        let oks: Vec<Vec<&str>> = log.iter().map(|l| l.split_whitespace().collect::<Vec<&str>>()).filter(|f| f[1] == "ok").collect();
        assert!(oks.len() >= 3, "{log:?}");
        assert!(oks.iter().all(|f| f[2] == "n1"), "{log:?}");
        let ncs: Vec<String> = oks.iter().map(|f| f[3].to_string()).collect();
        assert_eq!(ncs, (1..=ncs.len()).map(|k| format!("{k:08x}")).collect::<Vec<_>>(), "{log:?}");
    }

    #[test]
    fn a_stale_nonce_is_answered_with_the_new_one() {
        let routes: Routes = Default::default();
        {
            let mut r = routes.lock().unwrap();
            r.insert("/d/index.m3u8".into(), b"#EXTM3U\n#EXT-X-TARGETDURATION:1\n#EXTINF:1,\ns0.ts\n#EXTINF:1,\ns1.ts\n#EXT-X-ENDLIST\n".to_vec());
            r.insert("/d/s0.ts".into(), b"A".to_vec());
            r.insert("/d/s1.ts".into(), b"B".to_vec());
        }
        let (port, log) = digest_server(routes, "p", Some("/d/s1.ts"));
        let got = drain(&format!("http://u:p@127.0.0.1:{port}/d/index.m3u8"), Duration::from_secs(10)).unwrap();
        assert_eq!(got, b"AB");
        let log = log.lock().unwrap().clone();
        assert_eq!(log.iter().filter(|l| l.starts_with("/d/s1.ts stale n1 ")).count(), 1, "{log:?}");
        assert!(log.iter().any(|l| l.starts_with("/d/s1.ts ok n2 00000001")), "{log:?}");
    }

    #[test]
    fn a_wrong_password_is_refused_without_showing_it() {
        let routes: Routes = Default::default();
        routes.lock().unwrap().insert("/d/index.m3u8".into(), b"#EXTM3U\n#EXT-X-ENDLIST\n".to_vec());
        let (port, log) = digest_server(routes, "secret-pass", None);
        let e = drain(&format!("http://u:wrong-pass@127.0.0.1:{port}/d/index.m3u8"), Duration::from_secs(10)).unwrap_err();
        let msg = e.to_string();
        assert!(msg.contains("refused the username and password"), "{msg}");
        assert!(!msg.contains("wrong-pass") && !msg.contains("secret-pass"), "{msg}");
        assert!(log.lock().unwrap().iter().all(|l| !l.contains(" ok ")));
    }

    #[test]
    fn encrypted_and_ftp_playlists_are_refused_and_plain_files_download() {
        let routes: Routes = Default::default();
        {
            let mut r = routes.lock().unwrap();
            r.insert("/enc.m3u8".into(), b"#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\"\n#EXTINF:2,\na.ts\n#EXT-X-ENDLIST\n".to_vec());
            r.insert("/ftp.m3u8".into(), b"#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2,\nftp://cdn.example/a.ts\n#EXT-X-ENDLIST\n".to_vec());
            r.insert("/clip.ts".into(), fixture("h264_aac.ts"));
        }
        let port = server(routes, |_, _| {});
        for p in ["enc", "ftp"] {
            let e = crate::open_source(&format!("http://127.0.0.1:{port}/{p}.m3u8")).err().unwrap();
            assert_eq!(e.code, ErrorCode::UnsupportedFormat, "{p}: {e:?}");
        }
        assert_eq!(crate::open_source(&format!("http://127.0.0.1:{port}/missing.m3u8")).err().unwrap().code, ErrorCode::NotFound);
        let got = drain(&format!("http://127.0.0.1:{port}/clip.ts"), Duration::from_secs(10)).unwrap();
        assert!(got == fixture("h264_aac.ts"));
    }

    #[test]
    fn playlist_details() {
        assert_eq!(attributes(r#"BANDWIDTH=1280000,CODECS="avc1.4d401f,mp4a.40.2",RESOLUTION=1280x720"#)[1], ("CODECS".into(), "avc1.4d401f,mp4a.40.2".into()));
        assert_eq!(byte_range("100@50", None), Some((50, 149)));
        assert_eq!(byte_range("100", Some(150)), Some((150, 249)));
        assert_eq!(byte_range("100", None), None);
        assert_eq!(byte_range("0@5", None), None);
        assert_eq!(byte_range(&format!("2@{}", u64::MAX), None), None);
        assert!(parse("#EXTINF:1,\na.ts", "http://h/").is_err());
        // a key turned off later does not hide segments encrypted before it
        let Playlist::Media(m) = parse("#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-KEY:METHOD=AES-128,URI=\"k\"\n#EXTINF:2,\na.ts\n#EXT-X-KEY:METHOD=NONE\n#EXTINF:2,\nb.ts\n", "http://h/").unwrap() else { panic!() };
        assert_eq!(m.encrypted.as_deref(), Some("AES-128"));
        // sequence numbers at the top of the range do not overflow
        assert!(parse("#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXT-X-MEDIA-SEQUENCE:18446744073709551615\n#EXTINF:2,\na.ts\n#EXTINF:2,\nb.ts\n", "http://h/").is_ok());
        assert!(parse("#EXTM3U\n#EXT-X-TARGETDURATION:2\n#EXTINF:2,\n#EXT-X-BYTERANGE:2@18446744073709551613\na.ts\n#EXTINF:2,\n#EXT-X-BYTERANGE:2\na.ts\n", "http://h/").is_err());
    }
}
