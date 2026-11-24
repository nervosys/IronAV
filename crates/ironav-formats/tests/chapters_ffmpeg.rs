//! Matroska chapters and attachments in files made by FFmpeg, read with `open_demuxer`. Skipped without
//! `ffmpeg` on PATH.

use ironav_core::Demuxer;
use ironav_formats::open_demuxer;
use std::path::{Path, PathBuf};
use std::process::Command;

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success())
}

fn scratch() -> PathBuf {
    let test = std::thread::current().name().unwrap_or("test").replace("::", "-");
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("chap-{}-{test}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn run(args: &[&str]) -> bool {
    Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).status().map(|s| s.success()).unwrap_or(false)
}

#[test]
fn matroska_chapters_and_attachments_are_listed() {
    if !ffmpeg_available() {
        return;
    }
    let dir = scratch();
    let meta = dir.join("meta.txt");
    std::fs::write(&meta, ";FFMETADATA1\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=0\nEND=1000\ntitle=Intro\n[CHAPTER]\nTIMEBASE=1/1000\nSTART=1000\nEND=2000\ntitle=Main\n").unwrap();
    let font = dir.join("font.ttf");
    std::fs::write(&font, b"not really a font").unwrap();
    let file = dir.join("ch.mkv");
    let ok = run(&[
        "-f", "lavfi", "-i", "testsrc2=size=64x48:rate=10:duration=2", "-i", meta.to_str().unwrap(), "-map", "0", "-map_chapters", "1", "-c:v", "mpeg4", "-attach", font.to_str().unwrap(),
        "-metadata:s:t", "mimetype=font/ttf", file.to_str().unwrap(),
    ]);
    if !ok {
        eprintln!("ffmpeg could not write the chapters; skipped");
        return;
    }
    let d = open_demuxer(&file).expect("opens");
    let ch = d.chapters();
    assert_eq!(ch.iter().map(|c| c.title.as_str()).collect::<Vec<_>>(), ["Intro", "Main"]);
    assert!((ch[1].start - 1.0).abs() < 1e-6 && ch[1].end.is_some_and(|e| (e - 2.0).abs() < 1e-6), "{ch:?}");
    let at = d.attachments();
    assert_eq!(at.len(), 1, "{at:?}");
    // FFmpeg stores the path it was given as the name
    assert!(at[0].name.ends_with("font.ttf"), "{at:?}");
    assert_eq!((at[0].mime_type.as_str(), at[0].size), ("font/ttf", 17));
}
