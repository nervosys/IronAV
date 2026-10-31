//! WebVTT subtitles, as an HLS subtitle rendition carries them: one document per segment.
//!
//! Cues come out with their times in seconds and their text without tags or entities. NOTE,
//! STYLE and REGION blocks are skipped, and so are cue blocks whose timing line does not parse.
//! An `X-TIMESTAMP-MAP` header moves the cue times onto the MPEG-TS clock the video uses.

use ironav_core::{Error, Result};

/// One cue: its start and end in seconds and its text, tags removed, lines joined by '\n'.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct Cue {
    pub start: f64,
    pub end: f64,
    pub text: String,
}

/// Parses a WebVTT document (one HLS subtitle segment). An `X-TIMESTAMP-MAP` header
/// (`MPEGTS:<90 kHz ticks>,LOCAL:<cue time>`) moves every cue by MPEGTS / 90000 - LOCAL seconds.
pub fn parse(text: &str) -> Result<Vec<Cue>> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let first = text.lines().next().unwrap_or("").trim_end();
    let header_ok = first.strip_prefix("WEBVTT").is_some_and(|rest| {
        rest.is_empty() || rest.starts_with([' ', '\t'])
    });
    if !header_ok {
        return Err(Error::invalid_input("the subtitles do not start with WEBVTT"));
    }
    let blocks = blocks(text);
    let offset = header_offset(&blocks[0][1..])?;
    let mut cues = Vec::new();
    for block in &blocks[1..] {
        let head = block[0];
        if ["NOTE", "STYLE", "REGION"].iter().any(|kind| is_block(head, kind)) {
            continue;
        }
        // an identifier line may come before the timing line, and nothing else
        let Some(t) = block.iter().position(|line| line.contains("-->")) else {
            continue;
        };
        if t > 1 {
            continue;
        }
        let Some((start, end)) = timing(block[t]) else {
            continue;
        };
        let lines: Vec<String> = block[t + 1..]
            .iter()
            .map(|line| clean(line))
            .filter(|line| !line.is_empty())
            .collect();
        if lines.is_empty() {
            continue;
        }
        cues.push(Cue { start: start + offset, end: end + offset, text: lines.join("\n") });
    }
    Ok(cues)
}

/// Removes, from each cue, the leading lines that repeat the previous cue's last line, and drops
/// the cues left empty. YouTube's automatic captions roll like this: each cue starts with the line
/// the one before it ended with.
pub fn dedupe_rolling(cues: Vec<Cue>) -> Vec<Cue> {
    let mut out = Vec::with_capacity(cues.len());
    let mut previous: Option<String> = None;
    for cue in cues {
        let lines: Vec<&str> = cue.text.lines().collect();
        let skip = lines.iter().take_while(|line| Some(**line) == previous.as_deref()).count();
        previous = lines.last().map(|line| line.to_string());
        if skip == lines.len() {
            continue;
        }
        let text = lines[skip..].join("\n");
        out.push(Cue { text, ..cue });
    }
    out
}

/// The blocks of a document: runs of non-blank lines, trimmed, separated by blank lines.
fn blocks(text: &str) -> Vec<Vec<&str>> {
    let mut out = Vec::new();
    let mut current = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() {
            if !current.is_empty() {
                out.push(std::mem::take(&mut current));
            }
        } else {
            current.push(line);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

/// Whether a block starts with `kind`, as a word of its own (`NOTE` or `NOTE text`).
fn is_block(head: &str, kind: &str) -> bool {
    head.strip_prefix(kind)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with([' ', '\t']))
}

/// The offset from the header's `X-TIMESTAMP-MAP`, or zero when it has none. The pairs take ':'
/// or '=' between key and value.
fn header_offset(lines: &[&str]) -> Result<f64> {
    let Some(map) = lines.iter().find_map(|line| line.strip_prefix("X-TIMESTAMP-MAP")) else {
        return Ok(0.0);
    };
    let map = map.trim_start();
    let Some(map) = map.strip_prefix(['=', ':']) else {
        return Err(Error::invalid_input("X-TIMESTAMP-MAP has no value"));
    };
    let mut mpegts = None;
    let mut local = None;
    for pair in map.split(',') {
        let Some(split) = pair.find([':', '=']) else {
            return Err(Error::invalid_input(format!("bad X-TIMESTAMP-MAP entry '{pair}'")));
        };
        let (key, value) = (pair[..split].trim(), pair[split + 1..].trim());
        if key.eq_ignore_ascii_case("MPEGTS") {
            mpegts = number(value);
        } else if key.eq_ignore_ascii_case("LOCAL") {
            local = timestamp(value);
        }
    }
    match (mpegts, local) {
        (Some(ticks), Some(local)) => Ok(ticks as f64 / 90_000.0 - local),
        _ => Err(Error::invalid_input("X-TIMESTAMP-MAP needs MPEGTS and LOCAL times")),
    }
}

/// The start and end of a timing line; settings after the end time are ignored.
fn timing(line: &str) -> Option<(f64, f64)> {
    let (start, rest) = line.split_once("-->")?;
    let start = timestamp(start.trim())?;
    let end = timestamp(rest.split_whitespace().next()?)?;
    Some((start, end))
}

/// `HH:MM:SS.mmm`, or `MM:SS.mmm` with the hours left out.
fn timestamp(s: &str) -> Option<f64> {
    let parts: Vec<&str> = s.split(':').collect();
    let (hours, minutes, seconds) = match parts.as_slice() {
        [m, sec] => (0, *m, *sec),
        [h, m, sec] => (number(h)?, *m, *sec),
        _ => return None,
    };
    let minutes = number(minutes)?;
    let (whole, fraction) = seconds.split_once('.')?;
    let whole = number(whole)?;
    if fraction.len() != 3 || !fraction.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let millis = number(fraction)?;
    if minutes >= 60 || whole >= 60 {
        return None;
    }
    let seconds = (hours * 60 + minutes) * 60 + whole;
    Some(seconds as f64 + millis as f64 / 1000.0)
}

fn number(s: &str) -> Option<u64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// A cue line without its tags, entities decoded and surrounding whitespace removed. Tags are
/// removed first, so an escaped `&lt;` stays text. A '<' without a '>' ends the line.
fn clean(line: &str) -> String {
    let mut plain = String::with_capacity(line.len());
    let mut in_tag = false;
    for c in line.chars() {
        match c {
            '<' => in_tag = true,
            '>' if in_tag => in_tag = false,
            _ if !in_tag => plain.push(c),
            _ => {}
        }
    }
    decode(&plain).trim().to_string()
}

/// Decodes the entities WebVTT cues use, in one pass.
fn decode(s: &str) -> String {
    const ENTITIES: [(&str, &str); 6] = [
        ("&amp;", "&"),
        ("&lt;", "<"),
        ("&gt;", ">"),
        ("&nbsp;", "\u{a0}"),
        ("&lrm;", "\u{200e}"),
        ("&rlm;", "\u{200f}"),
    ];
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        match ENTITIES.iter().find(|(entity, _)| rest.starts_with(entity)) {
            Some((entity, text)) => {
                out.push_str(text);
                rest = &rest[entity.len()..];
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cue(start: f64, end: f64, text: &str) -> Cue {
        Cue { start, end, text: text.to_string() }
    }

    #[test]
    fn basic_cues_with_id_and_settings() {
        let text = "WEBVTT\n\n1\n00:00:01.000 --> 00:00:02.500 align:start position:0%\nHello there\n\n\
                    00:00:03.000 --> 00:00:04.000\nSecond cue\ntwo lines\n";
        let cues = parse(text).unwrap();
        assert_eq!(cues, vec![cue(1.0, 2.5, "Hello there"), cue(3.0, 4.0, "Second cue\ntwo lines")]);
    }

    #[test]
    fn hours_are_optional() {
        let text = "WEBVTT\n\n00:01.500 --> 00:02.250\nshort\n\n01:00:00.000 --> 01:00:01.000\nlong\n";
        let cues = parse(text).unwrap();
        assert_eq!(cues, vec![cue(1.5, 2.25, "short"), cue(3600.0, 3601.0, "long")]);
    }

    #[test]
    fn tags_and_entities_are_stripped() {
        let text = "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\n\
                    <v Roger>&lt;b&gt; &amp; <c.yellow>we&nbsp;are</c></v>\n\
                    a&lrm;b&rlm;\n\n\
                    00:00:01.000 --> 00:00:02.000\n<00:00:01.500><c>x</c>\n\n\
                    00:00:02.000 --> 00:00:03.000\n<c></c>\n";
        let cues = parse(text).unwrap();
        assert_eq!(
            cues,
            vec![
                cue(0.0, 1.0, "<b> & we\u{a0}are\na\u{200e}b\u{200f}"),
                cue(1.0, 2.0, "x"),
            ]
        );
    }

    #[test]
    fn note_style_and_region_blocks_are_skipped() {
        let text = "WEBVTT\n\nNOTE This is a comment\nwith --> no cue\n\n\
                    STYLE\n::cue { color: red }\n\nREGION\nid:r1\n\n\
                    00:00:00.000 --> 00:00:01.000\nkept\n";
        let cues = parse(text).unwrap();
        assert_eq!(cues, vec![cue(0.0, 1.0, "kept")]);
    }

    #[test]
    fn crlf_and_bom() {
        let text = "\u{feff}WEBVTT\r\n\r\n00:00:00.000 --> 00:00:01.000\r\nhi\r\n\r\n\
                    00:00:01.000 --> 00:00:02.000\r\nthere\r\n";
        let cues = parse(text).unwrap();
        assert_eq!(cues, vec![cue(0.0, 1.0, "hi"), cue(1.0, 2.0, "there")]);
    }

    #[test]
    fn x_timestamp_map_shifts_cues() {
        let text = "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:900000,LOCAL:00:00:00.000\n\n\
                    00:00:01.000 --> 00:00:02.000\nshifted\n";
        let cues = parse(text).unwrap();
        assert_eq!(cues, vec![cue(11.0, 12.0, "shifted")]);

        // the local time is subtracted, and '=' works as the separator too
        let text = "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS=900000,LOCAL=00:00:01.000\n\n\
                    00:00:01.000 --> 00:00:02.000\nshifted\n";
        let cues = parse(text).unwrap();
        assert_eq!(cues, vec![cue(10.0, 11.0, "shifted")]);
    }

    #[test]
    fn missing_header_is_an_error() {
        assert!(parse("00:00:00.000 --> 00:00:01.000\nhi\n").is_err());
        assert!(parse("").is_err());
        assert!(parse("WEBVTTX\n\n00:00:00.000 --> 00:00:01.000\nhi\n").is_err());
    }

    #[test]
    fn rolling_captions_are_deduplicated() {
        let cues = vec![
            cue(0.0, 1.0, "Hello"),
            cue(1.0, 2.0, "Hello\nworld"),
            cue(2.0, 3.0, "world\nnext"),
            cue(3.0, 4.0, "next"),
        ];
        assert_eq!(
            dedupe_rolling(cues),
            vec![cue(0.0, 1.0, "Hello"), cue(1.0, 2.0, "world"), cue(2.0, 3.0, "next")]
        );
    }
}
