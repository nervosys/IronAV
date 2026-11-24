//! Playlist files: M3U and M3U8 (extended or plain) and PLS. A playlist is a list of places to play, not
//! media, so it has no demuxer: [`parse_file`] gives its entries, and nothing here opens a file or a URL.
//!
//! HLS playlists (an `.m3u8` with `#EXT-X-` tags) are a different thing, read by the live sources as
//! streams; [`parse_m3u`] refuses them, and [`sniff`] reports them as [`Kind::Hls`] so a caller can say so.
//!
//! Relative entries are resolved against the playlist's directory (a path join, with no check that
//! anything is there). Entries are untrusted text: lines longer than [`MAX_LOCATION`] are skipped, titles are cut
//! at [`MAX_TITLE`] characters, and a playlist holds [`MAX_ENTRIES`] entries at most.

use crate::subtitle::decode_bytes;
use ironav_core::{Error, ErrorCode, Result};
use serde::Serialize;
use std::io::Read;
use std::path::Path;

/// The longest playlist file read (bytes).
pub const MAX_FILE_BYTES: usize = 4 << 20;
/// The most entries a playlist holds.
pub const MAX_ENTRIES: usize = 50_000;
/// The longest path or URL kept (bytes).
pub const MAX_LOCATION: usize = 4096;
/// The longest title kept (characters).
pub const MAX_TITLE: usize = 512;

/// One playlist entry.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Entry {
    /// A path (made absolute against the playlist's directory when it was relative) or a URL.
    pub location: String,
    /// Whether `location` is a URL (it has a scheme, such as `http://`), not a file path.
    pub url: bool,
    pub title: Option<String>,
    /// Seconds, when the playlist gives a length.
    pub duration_seconds: Option<f64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    M3u,
    Pls,
    /// An HLS playlist: a stream, not a list of files.
    Hls,
}

impl Kind {
    pub fn name(self) -> &'static str {
        match self {
            Kind::M3u => "m3u",
            Kind::Pls => "pls",
            Kind::Hls => "hls",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Playlist {
    pub format: &'static str,
    pub entries: Vec<Entry>,
}

fn is_url(s: &str) -> bool {
    match s.split_once("://") {
        Some((scheme, rest)) => {
            !scheme.is_empty() && !rest.is_empty() && scheme.starts_with(|c: char| c.is_ascii_alphabetic()) && scheme.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '-' | '.'))
        }
        None => false,
    }
}

/// A path as a playlist wrote it, joined to `base` unless it is already absolute (a drive letter, a root, a UNC path).
fn resolve(location: &str, base: Option<&Path>) -> String {
    let bytes = location.as_bytes();
    let absolute = location.starts_with(['/', '\\']) || (bytes.len() >= 3 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' && matches!(bytes[2], b'/' | b'\\'));
    match base {
        Some(b) if !absolute => {
            // forward slashes everywhere: every platform's file APIs take them, and the result reads the same on all
            let dir = b.to_string_lossy().replace('\\', "/");
            format!("{}/{}", dir.trim_end_matches('/'), location.replace('\\', "/"))
        }
        _ => location.to_string(),
    }
}

fn entry(location: &str, base: Option<&Path>, title: Option<&str>, duration: Option<f64>) -> Option<Entry> {
    let location = location.trim();
    if location.is_empty() || location.len() > MAX_LOCATION || location.chars().any(char::is_control) {
        return None;
    }
    let url = is_url(location);
    let title = title.map(|t| t.chars().filter(|c| !c.is_control()).take(MAX_TITLE).collect::<String>().trim().to_string()).filter(|t| !t.is_empty());
    Some(Entry { location: if url { location.to_string() } else { resolve(location, base) }, url, title, duration_seconds: duration.filter(|d| d.is_finite() && *d >= 0.0) })
}

/// Whether the text is an HLS playlist: it has an `#EXT-X-` tag.
pub fn is_hls(text: &str) -> bool {
    text.lines().any(|l| l.trim_start().starts_with("#EXT-X-"))
}

/// Parses M3U text. `base` is the directory relative entries are resolved against.
pub fn parse_m3u(text: &str, base: Option<&Path>) -> Result<Playlist> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    if is_hls(text) {
        return Err(Error::new(ErrorCode::UnsupportedFormat, "this is an HLS playlist (it has #EXT-X- tags), a stream and not a list of files")
            .hint("play it as a stream: give its URL to stream_open, which reads HLS"));
    }
    let mut entries = Vec::new();
    let mut pending: Option<(Option<f64>, Option<String>)> = None;
    for line in text.lines().map(str::trim).filter(|l| !l.is_empty()) {
        if let Some(info) = line.strip_prefix("#EXTINF:") {
            // `duration [key="value" ...],title`; the title follows the first comma outside quotes
            let (head, title) = split_outside_quotes(info);
            let duration = head.split_whitespace().next().and_then(|d| d.parse::<f64>().ok()).filter(|d| *d >= 0.0);
            pending = Some((duration, Some(title.trim().to_string())));
        } else if line.starts_with('#') {
            continue;
        } else {
            let (duration, title) = pending.take().unwrap_or((None, None));
            if let Some(e) = entry(line, base, title.as_deref(), duration) {
                if entries.len() >= MAX_ENTRIES {
                    return Err(too_many());
                }
                entries.push(e);
            }
        }
    }
    Ok(Playlist { format: "m3u", entries })
}

fn too_many() -> Error {
    Error::invalid_input(format!("the playlist has more than {MAX_ENTRIES} entries")).hint("playlists are capped so a damaged or hostile file cannot use all the memory")
}

/// Splits at the first comma that is not inside double quotes.
fn split_outside_quotes(s: &str) -> (&str, &str) {
    let mut quoted = false;
    for (i, c) in s.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ',' if !quoted => return (&s[..i], &s[i + 1..]),
            _ => {}
        }
    }
    (s, "")
}

/// Parses PLS text: `FileN=`, `TitleN=` and `LengthN=` lines of a `[playlist]` section, in order of N.
pub fn parse_pls(text: &str, base: Option<&Path>) -> Result<Playlist> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines().map(str::trim).filter(|l| !l.is_empty() && !l.starts_with([';', '#']));
    if !lines.next().is_some_and(|l| l.eq_ignore_ascii_case("[playlist]")) {
        return Err(Error::invalid_input("a PLS playlist starts with [playlist]"));
    }
    #[derive(Default)]
    struct Slot {
        file: Option<String>,
        title: Option<String>,
        length: Option<f64>,
    }
    let mut slots: std::collections::BTreeMap<u32, Slot> = std::collections::BTreeMap::new();
    for line in lines {
        if line.starts_with('[') {
            break;
        }
        let Some((key, value)) = line.split_once('=') else { continue };
        let key = key.trim().to_ascii_lowercase();
        let split = key.find(|c: char| c.is_ascii_digit()).unwrap_or(key.len());
        let (name, n) = key.split_at(split);
        let Ok(n) = n.parse::<u32>() else { continue };
        if slots.len() >= MAX_ENTRIES && !slots.contains_key(&n) {
            return Err(too_many());
        }
        let slot = slots.entry(n).or_default();
        match name {
            "file" => slot.file = Some(value.trim().to_string()),
            "title" => slot.title = Some(value.trim().to_string()),
            "length" => slot.length = value.trim().parse::<f64>().ok().filter(|l| *l >= 0.0),
            _ => {}
        }
    }
    let entries = slots.into_values().filter_map(|s| entry(&s.file?, base, s.title.as_deref(), s.length)).collect();
    Ok(Playlist { format: "pls", entries })
}

/// What kind of playlist a file's first bytes (and, when they say nothing, its extension) make it, if it is one.
pub fn sniff(magic: &[u8], extension: &str) -> Option<Kind> {
    let m = magic.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(magic);
    let text = String::from_utf8_lossy(&m[..m.len().min(2048)]);
    let head = text.trim_start();
    let by_content = if head.get(..7).is_some_and(|h| h.eq_ignore_ascii_case("#EXTM3U")) {
        Some(Kind::M3u)
    } else if head.get(..10).is_some_and(|h| h.eq_ignore_ascii_case("[playlist]")) {
        Some(Kind::Pls)
    } else {
        None
    };
    let kind = by_content.or(match extension.to_ascii_lowercase().as_str() {
        "m3u" | "m3u8" if !m.contains(&0) => Some(Kind::M3u),
        "pls" if !m.contains(&0) => Some(Kind::Pls),
        _ => None,
    })?;
    // the tags of an HLS playlist may come after the 2 KiB read, but the usual ones are the first lines
    Some(if kind == Kind::M3u && is_hls(&text) { Kind::Hls } else { kind })
}

/// Reads and parses a playlist file (M3U or PLS, found by its start or extension). Relative entries are
/// resolved against the file's directory. Nothing in it is opened.
pub fn parse_file(path: &Path) -> Result<Playlist> {
    let mut bytes = Vec::new();
    std::fs::File::open(path)?.take(MAX_FILE_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(Error::invalid_input(format!("the playlist is longer than {} MiB", MAX_FILE_BYTES >> 20)));
    }
    let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("");
    let kind = sniff(&bytes, ext).ok_or_else(|| Error::new(ErrorCode::UnsupportedFormat, "this is not an M3U or PLS playlist"))?;
    let text = decode_bytes(&bytes)?;
    let base = path.parent().filter(|p| !p.as_os_str().is_empty());
    match kind {
        Kind::M3u | Kind::Hls => parse_m3u(&text, base),
        Kind::Pls => parse_pls(&text, base),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extended_m3u_with_titles_durations_and_relative_paths() {
        let text = "\u{feff}#EXTM3U\r\n#EXTINF:123,Artist - Song\r\nmusic/song.mp3\r\n#EXTINF:-1 tvg-id=\"a,b\" group=\"x\",Live, with comma\r\nhttp://example.com/stream\r\n# a comment\r\n\r\nC:\\Music\\abs.mp3\r\n/abs/path.flac\r\nrel\\back.ogg\r\n#EXTINF:5,\r\nnotitle.wav\r\n";
        let p = parse_m3u(text, Some(Path::new("/lists"))).unwrap();
        assert_eq!(p.format, "m3u");
        let e = &p.entries;
        assert_eq!(e.len(), 6);
        assert_eq!((e[0].location.as_str(), e[0].title.as_deref(), e[0].duration_seconds, e[0].url), ("/lists/music/song.mp3", Some("Artist - Song"), Some(123.0), false));
        assert_eq!((e[1].location.as_str(), e[1].title.as_deref(), e[1].duration_seconds, e[1].url), ("http://example.com/stream", Some("Live, with comma"), None, true));
        assert_eq!(e[2].location, "C:\\Music\\abs.mp3");
        assert_eq!(e[3].location, "/abs/path.flac");
        assert_eq!(e[4].location, "/lists/rel/back.ogg");
        assert_eq!((e[5].title.as_deref(), e[5].duration_seconds), (None, Some(5.0)));
    }

    #[test]
    fn plain_m3u_is_a_list_of_lines() {
        let p = parse_m3u("a.mp3\nb.mp3\n", None).unwrap();
        assert_eq!(p.entries.iter().map(|e| e.location.as_str()).collect::<Vec<_>>(), vec!["a.mp3", "b.mp3"]);
    }

    #[test]
    fn hls_is_refused_and_recognised() {
        let hls = "#EXTM3U\n#EXT-X-VERSION:3\n#EXTINF:9.0,\nseg0.ts\n";
        let e = parse_m3u(hls, None).err().unwrap();
        assert_eq!(e.code, ErrorCode::UnsupportedFormat);
        assert!(e.hint.unwrap().contains("stream_open"));
        assert_eq!(sniff(hls.as_bytes(), "m3u8"), Some(Kind::Hls));
        assert_eq!(sniff(b"#EXTM3U\n#EXTINF:1,a\na.mp3\n", "m3u8"), Some(Kind::M3u));
        assert_eq!(sniff(b"a.mp3\nb.mp3\n", "m3u"), Some(Kind::M3u), "a plain list has no header");
        assert_eq!(sniff(b"a.mp3\n", "txt"), None);
        assert_eq!(sniff(&[0, 1, 2], "m3u"), None);
    }

    #[test]
    fn pls_entries_come_in_order_of_their_numbers() {
        let text = "[playlist]\r\nNumberOfEntries=3\r\nFile2=http://x/y\r\nTitle2=Two\r\nLength2=-1\r\nFile1=one.mp3\r\nTitle1=One\r\nLength1=61\r\nFile3=\r\nVersion=2\r\n";
        let p = parse_pls(text, Some(Path::new("/d"))).unwrap();
        assert_eq!(p.format, "pls");
        assert_eq!(p.entries.len(), 2, "an empty file is no entry");
        assert_eq!((p.entries[0].location.as_str(), p.entries[0].title.as_deref(), p.entries[0].duration_seconds), ("/d/one.mp3", Some("One"), Some(61.0)));
        assert_eq!((p.entries[1].location.as_str(), p.entries[1].url, p.entries[1].duration_seconds), ("http://x/y", true, None));
        assert!(parse_pls("File1=a\n", None).is_err());
    }

    #[test]
    fn entries_are_bounded_and_clean() {
        let long = "a".repeat(MAX_LOCATION + 1);
        let p = parse_m3u(&format!("{long}\nok.mp3\nbad\u{7}.mp3\n#EXTINF:1,{}\nt.mp3\n", "T".repeat(MAX_TITLE * 2)), None).unwrap();
        assert_eq!(p.entries.len(), 2, "a long line and one with control characters are skipped");
        assert_eq!(p.entries[1].title.as_ref().unwrap().chars().count(), MAX_TITLE);
        let many: String = (0..=MAX_ENTRIES).map(|i| format!("f{i}.mp3\n")).collect();
        assert!(parse_m3u(&many, None).is_err());
    }

    #[test]
    fn a_file_is_read_and_resolved_against_its_directory() {
        let dir = std::env::temp_dir().join(format!("ironav-playlist-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let f = dir.join("list.m3u");
        std::fs::write(&f, "#EXTM3U\n#EXTINF:3,Three\nsub/a.wav\n").unwrap();
        let p = parse_file(&f).unwrap();
        assert_eq!(Path::new(&p.entries[0].location), dir.join("sub").join("a.wav"));
        let bin = dir.join("bin.m3u");
        std::fs::write(&bin, [0u8, 1, 2, 3]).unwrap();
        assert!(parse_file(&bin).is_err());
        std::fs::remove_dir_all(dir).ok();
    }
}
