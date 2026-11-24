//! Subtitles as timed cues, and the standalone subtitle files that hold them: SubRip (`.srt`), WebVTT
//! (`.vtt`) and Advanced SubStation Alpha (`.ass`, `.ssa`). Each file is one subtitle stream whose
//! packets are the cues: `pts` and `duration` in milliseconds, `data` the cue's raw text as the
//! format holds it (SubRip and WebVTT: the lines with their markup; ASS: Matroska's form of an event,
//! `ReadOrder,Layer,Style,Name,MarginL,MarginR,MarginV,Effect,Text`, so a track copied from one to the
//! other needs no conversion). [`plain_text`] turns a packet's data into text to show.
//!
//! Subtitle text is untrusted input, from the file or from a track in a container. Files are read up
//! to [`MAX_FILE_BYTES`], binary data is refused, a cue's text is cut at [`MAX_CUE_BYTES`] and a file's
//! cues may hold [`MAX_TOTAL_BYTES`] of text and number [`MAX_CUES`] at most.

use ironav_core::{CodecId, Demuxer, Error, ErrorCode, Packet, Rational, Result, StreamInfo, StreamParams};
use serde::Serialize;
use std::io::Read;

/// The longest subtitle file read (bytes).
pub const MAX_FILE_BYTES: usize = 16 << 20;
/// The most text one cue keeps (bytes; the rest is cut off).
pub const MAX_CUE_BYTES: usize = 8 << 10;
/// The most text all the cues of a file may hold (bytes).
pub const MAX_TOTAL_BYTES: usize = 16 << 20;
/// The most cues a file may hold.
pub const MAX_CUES: usize = 100_000;

/// One subtitle cue: when it shows, what it says with its markup stripped, and the text as the file
/// held it (SubRip's and WebVTT's lines with their tags, ASS's event in Matroska's form).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Cue {
    /// Seconds.
    pub start: f64,
    /// Seconds.
    pub end: f64,
    pub text: String,
    pub raw: String,
}

/// Cuts `s` to at most `max` bytes, at a character boundary.
pub fn truncate(mut s: String, max: usize) -> String {
    if s.len() > max {
        let mut n = max;
        while !s.is_char_boundary(n) {
            n -= 1;
        }
        s.truncate(n);
    }
    s
}

/// What a file's cues have used of the limits.
#[derive(Default)]
pub(crate) struct Budget {
    bytes: usize,
    cues: usize,
}

impl Budget {
    pub(crate) fn take(&mut self, cue: &Cue) -> Result<()> {
        self.bytes += cue.text.len() + cue.raw.len();
        self.cues += 1;
        if self.cues > MAX_CUES || self.bytes > MAX_TOTAL_BYTES {
            return Err(Error::invalid_input(format!("the subtitles hold more than {MAX_CUES} cues or {} MiB of text", MAX_TOTAL_BYTES >> 20))
                .hint("subtitle text is capped so a damaged or hostile file cannot use all the memory"));
        }
        Ok(())
    }
}

/// Subtitle text from bytes: UTF-8 (with or without a byte order mark), UTF-16 with one, or else
/// Latin-1. Binary data (NUL bytes, or many control characters) and long input are refused.
pub fn decode_bytes(bytes: &[u8]) -> Result<String> {
    if bytes.len() > MAX_FILE_BYTES {
        return Err(Error::invalid_input(format!("the subtitle file is longer than {} MiB", MAX_FILE_BYTES >> 20)).hint("subtitle files are text; this is not one"));
    }
    let utf16 = |b: &[u8], big: bool| {
        let units = b.as_chunks::<2>().0.iter().map(|c| if big { u16::from_be_bytes([c[0], c[1]]) } else { u16::from_le_bytes([c[0], c[1]]) });
        char::decode_utf16(units).map(|r| r.unwrap_or('\u{FFFD}')).collect::<String>()
    };
    let text = if let Some(b) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        utf16(b, false)
    } else if let Some(b) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        utf16(b, true)
    } else {
        let b = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
        let controls = b.iter().filter(|&&c| c < 0x20 && !matches!(c, b'\t' | b'\n' | b'\r') || c == 0x7F).count();
        if b.contains(&0) || controls * 50 > b.len().max(1) {
            return Err(Error::invalid_input("the file is binary data, not subtitle text").hint("a subtitle file is text: SubRip (.srt), WebVTT (.vtt) or ASS/SSA (.ass, .ssa)"));
        }
        match std::str::from_utf8(b) {
            Ok(s) => s.to_string(),
            // not UTF-8: the legacy single-byte text SubRip files often are
            Err(_) => b.iter().map(|&c| c as char).collect(),
        }
    };
    Ok(text.replace("\r\n", "\n").replace('\r', "\n"))
}

/// Reads a subtitle file (at most [`MAX_FILE_BYTES`]) and decodes it.
pub(crate) fn read_text<R: Read>(r: R) -> Result<String> {
    let mut bytes = Vec::new();
    r.take(MAX_FILE_BYTES as u64 + 1).read_to_end(&mut bytes)?;
    decode_bytes(&bytes)
}

/// The text of a cue without its markup: HTML-style tags (`<i>`, `</font>`), ASS override blocks
/// (`{\an8}`, `{\pos(1,2)}`) and WebVTT voice and timestamp tags are removed, `\N` and `\n` become line
/// breaks and `\h` a space, the common entities are decoded, and control characters are dropped.
pub fn strip_markup(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let chars: Vec<char> = s.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        match c {
            '{' if chars.get(i + 1) == Some(&'\\') => match chars[i..].iter().take(512).position(|&c| c == '}') {
                Some(n) => i += n,
                None => out.push(c),
            },
            '<' if chars.get(i + 1).is_some_and(|c| c.is_ascii_alphabetic() || *c == '/' || *c == '!') || is_timestamp_tag(&chars[i..]) => match chars[i..].iter().take(256).position(|&c| c == '>') {
                Some(n) => i += n,
                None => out.push(c),
            },
            '\\' if matches!(chars.get(i + 1), Some('N' | 'n')) => {
                out.push('\n');
                i += 1;
            }
            '\\' if chars.get(i + 1) == Some(&'h') => {
                out.push(' ');
                i += 1;
            }
            c => out.push(c),
        }
        i += 1;
    }
    let decoded = decode_entities(&out);
    let lines: Vec<String> = decoded.lines().map(|l| l.chars().filter(|c| !c.is_control() || *c == '\t').collect::<String>().trim().to_string()).collect();
    lines.join("\n").trim().to_string()
}

/// A WebVTT cue timestamp tag, like `<00:01:02.500>`.
fn is_timestamp_tag(c: &[char]) -> bool {
    c.iter().skip(1).take(14).position(|&x| x == '>').is_some_and(|n| n > 0 && c[1..=n].iter().all(|x| x.is_ascii_digit() || *x == ':' || *x == '.'))
}

fn decode_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let end = rest.find(';').filter(|&e| e <= 10);
        let decoded = end.and_then(|e| match &rest[1..e] {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some('\u{a0}'),
            "lrm" => Some('\u{200e}'),
            "rlm" => Some('\u{200f}'),
            n if n.starts_with("#x") || n.starts_with("#X") => u32::from_str_radix(&n[2..], 16).ok().and_then(char::from_u32),
            n if n.starts_with('#') => n[1..].parse().ok().and_then(char::from_u32),
            _ => None,
        });
        match (decoded, end) {
            (Some(c), Some(e)) => {
                out.push(c);
                rest = &rest[e + 1..];
            }
            _ => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// The text to show for a subtitle packet's data: an ASS event loses its leading fields, and every
/// codec's markup is stripped.
pub fn plain_text(codec: CodecId, data: &[u8]) -> String {
    let text = String::from_utf8_lossy(&data[..data.len().min(MAX_CUE_BYTES * 2)]);
    match codec {
        CodecId::Ass => strip_markup(ass_event_text(&text)),
        _ => strip_markup(&text),
    }
}

/// The Text field of an ASS event in Matroska's form (the ninth, which may hold commas).
pub fn ass_event_text(event: &str) -> &str {
    let mut at = 0;
    for _ in 0..8 {
        match event[at..].find(',') {
            Some(i) => at += i + 1,
            None => return "",
        }
    }
    &event[at..]
}

/// `H:MM:SS,mmm`, `HH:MM:SS.mmm`, `MM:SS.mmm` or ASS's `H:MM:SS.cc`, in seconds. The fraction takes the
/// decimal point or comma and any number of digits.
pub(crate) fn parse_time(s: &str) -> Option<f64> {
    let s = s.trim();
    let parts: Vec<&str> = s.split(':').collect();
    let (h, m, sec) = match parts.as_slice() {
        [m, s] => ("0", *m, *s),
        [h, m, s] => (*h, *m, *s),
        _ => return None,
    };
    let (whole, frac) = match sec.split_once(['.', ',']) {
        Some((w, f)) => (w, f),
        None => (sec, ""),
    };
    let num = |s: &str| (!s.is_empty() && s.len() <= 9 && s.bytes().all(|b| b.is_ascii_digit())).then(|| s.parse::<u64>().ok()).flatten();
    let (h, m, w) = (num(h)?, num(m)?, num(whole)?);
    if m >= 60 || w >= 61 || !frac.bytes().all(|b| b.is_ascii_digit()) || frac.len() > 9 {
        return None;
    }
    let f = if frac.is_empty() { 0.0 } else { frac.parse::<f64>().ok()? / 10f64.powi(frac.len() as i32) };
    Some(((h * 60 + m) * 60 + w) as f64 + f)
}

/// The start and end of a SubRip timing line (`00:00:01,000 --> 00:00:02,500`, positions after it ignored).
fn srt_timing(line: &str) -> Option<(f64, f64)> {
    let (a, rest) = line.split_once("-->")?;
    Some((parse_time(a)?, parse_time(rest.split_whitespace().next()?)?))
}

/// What kind of subtitle file starts with `magic` (the first few hundred bytes of a file): `"webvtt"` for a
/// `WEBVTT` header, `"ass"` for a `[Script Info]` section, `"srt"` for a SubRip timing line within the first
/// lines. UTF-8 (with or without a byte order mark), UTF-16 with one, and Latin-1 are told apart; anything
/// with NUL bytes is not text.
pub(crate) fn sniff(magic: &[u8]) -> Option<&'static str> {
    let text: String = if magic.starts_with(&[0xFF, 0xFE]) || magic.starts_with(&[0xFE, 0xFF]) {
        let big = magic[0] == 0xFE;
        char::decode_utf16(magic[2..].as_chunks::<2>().0.iter().map(|c| if big { u16::from_be_bytes([c[0], c[1]]) } else { u16::from_le_bytes([c[0], c[1]]) })).map(|r| r.unwrap_or('\u{FFFD}')).collect()
    } else {
        String::from_utf8_lossy(magic.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(magic)).into_owned()
    };
    if text.contains('\0') {
        return None;
    }
    let head = text.trim_start_matches('\u{feff}');
    if head.starts_with("WEBVTT") && head[6..].chars().next().is_none_or(|c| matches!(c, ' ' | '\t' | '\n' | '\r')) {
        Some("webvtt")
    } else if head.get(..13).is_some_and(|h| h.eq_ignore_ascii_case("[Script Info]")) {
        Some("ass")
    } else if head.lines().take(4).any(|l| srt_timing(l).is_some()) {
        Some("srt")
    } else {
        None
    }
}

/// Parses SubRip. Tolerates a byte order mark, CRLF, a period for the comma before the milliseconds,
/// missing indexes, missing blank lines between cues (the next timing line starts the next cue, taking
/// a bare index line before it off the end of the one before) and trailing positions after the end time.
/// Cues come out sorted by start time; those with no text or an end before the start are dropped.
pub fn parse_srt(text: &str) -> Result<Vec<Cue>> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let lines: Vec<&str> = text.lines().collect();
    let mut cues = Vec::new();
    let mut budget = Budget::default();
    let mut i = 0;
    while i < lines.len() {
        let Some((start, end)) = srt_timing(lines[i]) else {
            i += 1;
            continue;
        };
        i += 1;
        let mut body: Vec<&str> = Vec::new();
        while i < lines.len() && !lines[i].trim().is_empty() {
            if srt_timing(lines[i]).is_some() {
                // the next cue began without a blank line; its index, if it has one, is the line before
                if body.last().is_some_and(|l| l.trim().bytes().all(|b| b.is_ascii_digit()) && !l.trim().is_empty()) {
                    body.pop();
                }
                break;
            }
            body.push(lines[i]);
            i += 1;
        }
        let raw = truncate(body.join("\n").trim().to_string(), MAX_CUE_BYTES);
        let text = strip_markup(&raw);
        if end < start || text.is_empty() {
            continue;
        }
        let cue = Cue { start, end, text, raw };
        budget.take(&cue)?;
        cues.push(cue);
    }
    cues.sort_by(|a, b| a.start.total_cmp(&b.start));
    Ok(cues)
}

/// The header of an ASS script (everything up to the first event, which Matroska keeps as the codec
/// private data) and its events as cues.
pub struct Script {
    pub header: String,
    pub cues: Vec<Cue>,
}

/// Parses an ASS or SSA script's `Dialogue:` lines (`Comment:` lines are skipped). The fields come by the
/// `Format:` line of the `[Events]` section (SSA's `Marked` takes the place of `Layer`), and the last, the
/// text, takes the rest of the line, commas and all. `raw` is the event in Matroska's form.
pub fn parse_ass(text: &str) -> Result<Script> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut header = String::new();
    let mut in_events = false;
    let mut columns: Vec<String> = ["layer", "start", "end", "style", "name", "marginl", "marginr", "marginv", "effect", "text"].map(String::from).to_vec();
    let mut cues = Vec::new();
    let mut budget = Budget::default();
    let mut events_started = false;
    for line in text.lines() {
        let t = line.trim_start();
        let is = |prefix: &str| t.get(..prefix.len()).is_some_and(|p| p.eq_ignore_ascii_case(prefix));
        if t.starts_with('[') {
            in_events = is("[events]");
        }
        if in_events && is("dialogue:") {
            events_started = true;
            let rest = &t[9..];
            let rest = rest.strip_prefix(' ').unwrap_or(rest);
            let fields: Vec<&str> = rest.splitn(columns.len().max(1), ',').collect();
            if fields.len() < columns.len() {
                continue;
            }
            let field = |name: &str| columns.iter().position(|c| c == name).map(|i| fields[i].trim()).unwrap_or("");
            let (Some(start), Some(end)) = (parse_time(field("start")), parse_time(field("end"))) else { continue };
            if end < start {
                continue;
            }
            let layer = if columns.iter().any(|c| c == "layer") { field("layer") } else { field("marked").trim_start_matches("Marked=") };
            let layer = if layer.is_empty() { "0" } else { layer };
            let body = truncate(fields[columns.len() - 1].to_string(), MAX_CUE_BYTES);
            let plain = strip_markup(&body);
            if plain.is_empty() {
                continue;
            }
            let raw = format!("{},{},{},{},{},{},{},{},{}", cues.len(), layer, field("style"), field("name"), field("marginl"), field("marginr"), field("marginv"), field("effect"), body);
            let cue = Cue { start, end, text: plain, raw };
            budget.take(&cue)?;
            cues.push(cue);
        } else if events_started || is("comment:") && in_events {
            // events after the first are not part of the header
        } else {
            if in_events && is("format:") {
                columns = t[7..].split(',').map(|c| c.trim().to_ascii_lowercase()).collect();
            }
            if header.len() < MAX_CUE_BYTES * 16 {
                header.push_str(line);
                header.push('\n');
            }
        }
    }
    cues.sort_by(|a, b| a.start.total_cmp(&b.start));
    Ok(Script { header, cues })
}

/// A subtitle file: one stream of timed cues.
pub struct SubtitleDemuxer {
    name: &'static str,
    streams: Vec<StreamInfo>,
    packets: Vec<Packet>,
    cues: Vec<Cue>,
    next: usize,
}

fn millis(s: f64) -> i64 {
    (s * 1000.0).round().clamp(0.0, i64::MAX as f64 / 2.0) as i64
}

impl SubtitleDemuxer {
    fn build(name: &'static str, codec: CodecId, extradata: Vec<u8>, cues: Vec<Cue>) -> Self {
        let packets: Vec<Packet> = cues.iter().map(|c| Packet { stream: 0, pts: millis(c.start), duration: (millis(c.end) - millis(c.start)).max(0), data: c.raw.clone().into_bytes() }).collect();
        let duration = packets.iter().map(|p| p.pts + p.duration).max().unwrap_or(0);
        let stream = StreamInfo { index: 0, codec, time_base: Rational::new(1, 1000), duration: Some(duration), bits: None, extradata, lead_in: 0, params: StreamParams::Subtitle };
        Self { name, streams: vec![stream], packets, cues, next: 0 }
    }

    /// A SubRip file.
    pub fn srt<R: Read>(r: R) -> Result<Self> {
        let cues = parse_srt(&read_text(r)?)?;
        if cues.is_empty() {
            return Err(Error::new(ErrorCode::InvalidInput, "the SubRip file has no cues").hint("a cue is a timing line such as `00:00:01,000 --> 00:00:02,500` followed by its text"));
        }
        Ok(Self::build("srt", CodecId::SubRip, Vec::new(), cues))
    }

    /// A WebVTT file.
    pub fn vtt<R: Read>(r: R) -> Result<Self> {
        let cues = crate::webvtt::parse_full(&read_text(r)?)?;
        let mut budget = Budget::default();
        for c in &cues {
            budget.take(c)?;
        }
        Ok(Self::build("webvtt", CodecId::WebVtt, Vec::new(), cues))
    }

    /// An ASS or SSA file.
    pub fn ass<R: Read>(r: R) -> Result<Self> {
        let script = parse_ass(&read_text(r)?)?;
        if script.cues.is_empty() {
            return Err(Error::new(ErrorCode::InvalidInput, "the ASS file has no dialogue lines").hint("an event is a `Dialogue:` line in the [Events] section"));
        }
        Ok(Self::build("ass", CodecId::Ass, script.header.into_bytes(), script.cues))
    }

    /// The cues, with their plain text.
    pub fn cues(&self) -> &[Cue] {
        &self.cues
    }
}

impl Demuxer for SubtitleDemuxer {
    fn format_name(&self) -> &'static str {
        self.name
    }
    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }
    fn read_packet(&mut self) -> Result<Option<Packet>> {
        let p = self.packets.get(self.next).cloned();
        if p.is_some() {
            self.next += 1;
        }
        Ok(p)
    }
    /// Lands on the first cue that is still showing at `seconds` or starts after it.
    fn seek(&mut self, seconds: f64) -> Result<f64> {
        let t = millis(seconds.max(0.0));
        self.next = self.packets.partition_point(|p| p.pts + p.duration <= t);
        Ok(self.packets.get(self.next).map_or(seconds.max(0.0), |p| (p.pts as f64 / 1000.0).min(seconds.max(0.0))))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn srt_with_every_irregularity() {
        let text = "\u{feff}1\r\n00:00:01,000 --> 00:00:02,500\r\n<i>Hello</i>\r\nworld\r\n\r\n2\r\n00:00:03.000 --> 00:00:04.000 X1:10 X2:20\r\nsecond\r\n3\r\n00:00:05,5 --> 00:00:06,25\r\nno blank line before the index\r\n\r\n00:01:00,000 --> 01:00:00,000\r\nno index &amp; &lt;b&gt; {\\an8}positioned\r\n";
        let cues = parse_srt(&decode_bytes(text.as_bytes()).unwrap()).unwrap();
        assert_eq!(cues.len(), 4);
        assert_eq!((cues[0].start, cues[0].end), (1.0, 2.5));
        assert_eq!(cues[0].text, "Hello\nworld");
        assert_eq!(cues[0].raw, "<i>Hello</i>\nworld");
        assert_eq!((cues[1].start, cues[1].end, cues[1].text.as_str()), (3.0, 4.0, "second"));
        assert_eq!((cues[2].start, cues[2].end), (5.5, 6.25), "short fractions are fractions of a second");
        assert_eq!(cues[2].text, "no blank line before the index");
        assert_eq!(cues[3].text, "no index & <b> positioned");
        assert_eq!(cues[3].end, 3600.0);
    }

    #[test]
    fn srt_cues_with_no_blank_line_between_them_split_and_lose_the_index() {
        let cues = parse_srt("1\n00:00:01,000 --> 00:00:02,000\nfirst\n2\n00:00:03,000 --> 00:00:04,000\nsecond\n").unwrap();
        assert_eq!(cues.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(), vec!["first", "second"]);
    }

    #[test]
    fn srt_drops_empty_and_backwards_cues_and_sorts() {
        let cues = parse_srt("00:00:05,000 --> 00:00:06,000\nlate\n\n00:00:01,000 --> 00:00:02,000\nearly\n\n00:00:03,000 --> 00:00:02,000\nbackwards\n\n00:00:07,000 --> 00:00:08,000\n\n").unwrap();
        assert_eq!(cues.iter().map(|c| c.text.as_str()).collect::<Vec<_>>(), vec!["early", "late"]);
    }

    #[test]
    fn markup_is_stripped_with_its_entities_and_ass_breaks() {
        assert_eq!(strip_markup("<b>bold</b> <font color=\"#fff\">c</font> a < b"), "bold c a < b");
        assert_eq!(strip_markup("{\\an8\\pos(1,2)}Line one\\NLine two\\hhere"), "Line one\nLine two here");
        assert_eq!(strip_markup("&lt;i&gt; &amp;amp; &#65;&#x42; &bogus; &"), "<i> &amp; AB &bogus; &");
        assert_eq!(strip_markup("{not an override}"), "{not an override}");
        assert_eq!(strip_markup("a\u{0}b\u{7}c"), "abc");
        assert_eq!(strip_markup("<c.yellow>cue</c> <00:00:01.000>text <v Bob>hi"), "cue text hi");
        // an unterminated tag or override is text
        assert_eq!(strip_markup("x <b y"), "x <b y");
    }

    #[test]
    fn ass_events_come_with_their_header_and_raw_text() {
        let text = "[Script Info]\nTitle: t\n\n[V4+ Styles]\nFormat: Name, Fontname\nStyle: Default,Arial\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:01.00,0:00:03.50,Default,Bob,0,0,0,,{\\i1}Hello, world{\\i0}\\Nsecond\nComment: 0,0:00:01.00,0:00:03.50,Default,,0,0,0,,not shown\nDialogue: 1,0:00:05.00,0:00:06.00,Sign,,10,20,30,Banner,Sign text\nDialogue: broken\n";
        let s = parse_ass(text).unwrap();
        assert!(s.header.contains("[Script Info]") && s.header.contains("Format: Layer"), "{}", s.header);
        assert!(!s.header.contains("Dialogue"));
        assert_eq!(s.cues.len(), 2);
        assert_eq!((s.cues[0].start, s.cues[0].end), (1.0, 3.5));
        assert_eq!(s.cues[0].text, "Hello, world\nsecond");
        assert_eq!(s.cues[0].raw, "0,0,Default,Bob,0,0,0,,{\\i1}Hello, world{\\i0}\\Nsecond");
        assert_eq!(s.cues[1].raw, "1,1,Sign,,10,20,30,Banner,Sign text");
        // a packet's data gives its text back
        assert_eq!(plain_text(CodecId::Ass, s.cues[0].raw.as_bytes()), "Hello, world\nsecond");
        assert_eq!(plain_text(CodecId::SubRip, b"<i>x</i>"), "x");
    }

    #[test]
    fn ssa_marked_column_stands_for_the_layer() {
        let text = "[Events]\nFormat: Marked, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: Marked=0,0:00:01.00,0:00:02.00,Default,,0,0,0,,text\n";
        let s = parse_ass(text).unwrap();
        assert_eq!(s.cues[0].raw, "0,0,Default,,0,0,0,,text");
    }

    #[test]
    fn binary_and_oversize_input_is_refused() {
        assert!(decode_bytes(&[0, 1, 2, 3, 0xFF, 0xFE, 0, 0]).is_err());
        assert!(decode_bytes(b"\x00\x00\x00\x18ftypmp42").is_err());
        assert!(decode_bytes(&vec![b'a'; MAX_FILE_BYTES + 1]).is_err());
        let ctl: Vec<u8> = (0..400).map(|i| if i % 5 == 0 { 0x01 } else { b'a' }).collect();
        assert!(decode_bytes(&ctl).is_err());
        // Latin-1 and UTF-16 decode
        assert_eq!(decode_bytes(b"caf\xE9").unwrap(), "café");
        assert_eq!(decode_bytes(&[0xFF, 0xFE, b'h', 0, b'i', 0]).unwrap(), "hi");
        assert_eq!(decode_bytes(&[0xFE, 0xFF, 0, b'h', 0, b'i']).unwrap(), "hi");
    }

    #[test]
    fn cues_and_files_are_capped() {
        let long = "x".repeat(MAX_CUE_BYTES * 3);
        let cues = parse_srt(&format!("00:00:01,000 --> 00:00:02,000\n{long}\n")).unwrap();
        assert_eq!(cues[0].raw.len(), MAX_CUE_BYTES);
        // a file of too many cues is an error, not a gigabyte of memory
        let mut big = String::new();
        for i in 0..=MAX_CUES {
            big.push_str(&format!("00:00:{:02},000 --> 00:00:59,000\nc\n\n", i % 60));
        }
        assert!(big.len() < MAX_FILE_BYTES);
        let e = parse_srt(&big).err().unwrap();
        assert!(e.message.contains("more than"), "{}", e.message);
        // total text: each cue within its cap, the sum over the budget
        let body = "y".repeat(MAX_CUE_BYTES);
        let mut total = String::new();
        for i in 0..(MAX_TOTAL_BYTES / MAX_CUE_BYTES / 2 + 10) {
            total.push_str(&format!("00:00:01,{:03} --> 00:00:59,000\n{body}\n\n", i % 1000));
        }
        assert!(parse_srt(&total).is_err());
    }

    #[test]
    fn the_demuxer_gives_one_packet_per_cue_and_seeks() {
        let mut d = SubtitleDemuxer::srt("1\n00:00:01,000 --> 00:00:02,000\none\n\n2\n00:00:05,000 --> 00:00:06,000\ntwo\n".as_bytes()).unwrap();
        assert_eq!(d.format_name(), "srt");
        let s = d.streams()[0].clone();
        assert_eq!((s.codec, s.params, s.time_base, s.duration), (CodecId::SubRip, StreamParams::Subtitle, Rational::new(1, 1000), Some(6000)));
        let p = d.read_packet().unwrap().unwrap();
        assert_eq!((p.pts, p.duration, p.data.as_slice()), (1000, 1000, b"one".as_slice()));
        assert_eq!(d.seek(5.5).unwrap(), 5.0);
        assert_eq!(d.read_packet().unwrap().unwrap().data, b"two");
        assert!(d.read_packet().unwrap().is_none());
        assert_eq!(d.seek(1.5).unwrap(), 1.0, "a cue still showing is read again");
        assert_eq!(d.read_packet().unwrap().unwrap().data, b"one");
        assert!(SubtitleDemuxer::srt(b"just text".as_slice()).is_err());
    }

    #[test]
    fn formats_are_recognised_by_their_first_bytes() {
        assert_eq!(sniff(b"1\r\n00:00:01,000 --> 00:00:02,000\r\nhi"), Some("srt"));
        assert_eq!(sniff("\u{feff}00:00:01.000 --> 00:00:02.000\nhi".as_bytes()), Some("srt"));
        assert_eq!(sniff(b"WEBVTT\n\n00:00:01.000 --> 00:00:02.000\nhi"), Some("webvtt"), "WebVTT's timing lines do not make it SubRip");
        assert_eq!(sniff(b"hello world"), None);
        assert_eq!(sniff(b"WEBVTT - title"), Some("webvtt"));
        assert_eq!(sniff(b"WEBVTTX"), None);
        assert_eq!(sniff(b"[script info]\nTitle: x"), Some("ass"));
        assert_eq!(sniff(b"[Events]"), None);
        assert_eq!(sniff(b"\x00\x00\x00\x18ftypmp42 00:00:01,000 --> 00:00:02,000"), None);
        let mut utf16 = vec![0xFF, 0xFE];
        utf16.extend("1\n00:00:01,000 --> 00:00:02,000\nhi".encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(sniff(&utf16), Some("srt"));
    }
}
