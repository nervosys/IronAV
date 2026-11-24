//! Subtitles in Matroska files made by FFmpeg (text tracks, chapters, attachments), and the standalone
//! subtitle files, read with `open_demuxer`. Skipped without `ffmpeg` on PATH.

use ironav_core::{CodecId, Demuxer, MediaType, StreamParams};
use ironav_formats::open_demuxer;
use std::path::{Path, PathBuf};
use std::process::Command;

const SRT: &str = "1\n00:00:00,500 --> 00:00:01,500\nfirst <i>cue</i>\n\n2\n00:00:02,000 --> 00:00:03,000\nsecond\ncue\n\n";

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success())
}

fn scratch() -> PathBuf {
    let test = std::thread::current().name().unwrap_or("test").replace("::", "-");
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("subs-{}-{test}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run(args: &[&str]) -> bool {
    Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).status().map(|s| s.success()).unwrap_or(false)
}

fn cues(d: &mut dyn Demuxer, stream: usize) -> Vec<(i64, i64, String)> {
    let mut out = Vec::new();
    while let Some(p) = d.read_packet().unwrap() {
        if p.stream == stream {
            out.push((p.pts, p.duration, String::from_utf8_lossy(&p.data).into_owned()));
        }
    }
    out
}

#[test]
fn matroska_text_tracks_are_subtitle_streams() {
    if !ffmpeg_available() {
        return;
    }
    let dir = scratch();
    let srt = dir.join("in.srt");
    std::fs::write(&srt, SRT).unwrap();
    for (codec, expect) in [("srt", CodecId::SubRip), ("ass", CodecId::Ass), ("webvtt", CodecId::WebVtt)] {
        let file = dir.join(format!("{codec}.mkv"));
        let ok = run(&["-f", "lavfi", "-i", "testsrc2=size=64x48:rate=10:duration=4", "-i", srt.to_str().unwrap(), "-c:v", "mpeg4", "-c:s", codec, "-map", "0", "-map", "1", file.to_str().unwrap()]);
        if !ok {
            eprintln!("ffmpeg cannot write {codec} subtitles into Matroska; skipped");
            continue;
        }
        let mut d = open_demuxer(&file).expect("opens");
        let s = d.streams().iter().position(|s| s.params == StreamParams::Subtitle).unwrap_or_else(|| panic!("a subtitle stream: {:?} / {:?}", d.streams(), d.unsupported_streams()));
        assert_eq!(d.streams()[s].params.media_type(), MediaType::Subtitle);
        assert_eq!(d.streams()[s].codec, expect, "{codec}");
        let got = cues(d.as_mut(), s);
        assert_eq!(got.len(), 2, "{codec}: {got:?}");
        let ms = |c: &(i64, i64, String)| (c.0, c.1);
        assert_eq!((ms(&got[0]), ms(&got[1])), ((500, 1000), (2000, 1000)), "{codec}");
        assert!(got[0].2.contains("first"), "{codec}: {}", got[0].2);
    }
}

#[test]
fn standalone_subtitle_files_are_detected_by_content() {
    let dir = scratch();
    // no extension is needed: the first bytes say what the file is
    let cases: [(&str, &[u8], CodecId); 3] = [
        ("a.dat", SRT.as_bytes(), CodecId::SubRip),
        ("b.dat", b"WEBVTT\n\n00:01.000 --> 00:02.000\nhello\n", CodecId::WebVtt),
        ("c.dat", b"[Script Info]\nTitle: t\n\n[Events]\nFormat: Layer, Start, End, Style, Name, MarginL, MarginR, MarginV, Effect, Text\nDialogue: 0,0:00:01.00,0:00:02.00,Default,,0,0,0,,hi\n", CodecId::Ass),
    ];
    for (name, bytes, codec) in cases {
        let f = dir.join(name);
        std::fs::write(&f, bytes).unwrap();
        let mut d = open_demuxer(&f).expect("opens");
        assert_eq!(d.streams().len(), 1);
        assert_eq!(d.streams()[0].codec, codec);
        assert!(!cues(d.as_mut(), 0).is_empty(), "{name}");
    }
}
