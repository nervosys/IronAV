//! Matroska output with several tracks of one kind: two audio tracks with a video track, and two video tracks with
//! an audio track. The sources are made by FFmpeg, IronAV copies them into `.mkv` through the engine, FFprobe lists
//! every track with its codec, and IronAV's own Matroska reader returns each track's packets as the source had them.
//! The checks skip when `ffmpeg` or `ffprobe` is not on the PATH.

use ironav_core::CodecId;
use ironav_engine::{run, Job};
use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ironav-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn has_tool(name: &str) -> bool {
    Command::new(name).arg("-version").output().map(|o| o.status.success()).unwrap_or(false)
}

/// Runs FFmpeg with `args` (the output path last), failing the test if it does.
fn ffmpeg(args: &[&str]) {
    let o = Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).output().unwrap();
    assert!(o.status.success(), "ffmpeg failed: {}", String::from_utf8_lossy(&o.stderr));
}

/// The (codec type, codec name) of each stream FFprobe lists in `path`, in order.
fn ffprobe_streams(path: &Path) -> Vec<(String, String)> {
    let o = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "stream=codec_type,codec_name", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .unwrap();
    assert!(o.status.success(), "ffprobe rejected {}: {}", path.display(), String::from_utf8_lossy(&o.stderr));
    String::from_utf8(o.stdout)
        .unwrap()
        .lines()
        .map(|l| {
            // FFprobe prints the codec name before the codec type
            let mut f = l.split(',');
            let (name, kind) = (f.next().unwrap(), f.next().unwrap());
            (kind.to_string(), name.to_string())
        })
        .collect()
}

/// The codec of each stream IronAV's reader finds in `path`, and the payloads of each stream's packets.
fn read_tracks(path: &Path) -> (Vec<CodecId>, Vec<Vec<Vec<u8>>>) {
    let mut d = ironav_formats::open_demuxer(path).unwrap();
    let codecs: Vec<CodecId> = d.streams().iter().map(|s| s.codec).collect();
    let mut tracks = vec![Vec::new(); codecs.len()];
    while let Some(p) = d.read_packet().unwrap() {
        tracks[p.stream].push(p.data);
    }
    (codecs, tracks)
}

/// Two audio tracks (AAC and FLAC) and one MPEG-4 video track copied into one Matroska file.
#[test]
fn two_audio_tracks_and_a_video_track_are_all_written() {
    if !has_tool("ffmpeg") || !has_tool("ffprobe") {
        eprintln!("ffmpeg or ffprobe not on PATH; skipping");
        return;
    }
    let dir = scratch("mkv-two-audio");
    let src = dir.join("src.mkv");
    ffmpeg(&[
        "-f", "lavfi", "-i", "testsrc=size=64x48:rate=25",
        "-f", "lavfi", "-i", "sine=frequency=440",
        "-f", "lavfi", "-i", "sine=frequency=880:sample_rate=48000",
        "-map", "0:v", "-map", "1:a", "-map", "2:a",
        "-c:v", "mpeg4", "-c:a:0", "aac", "-c:a:1", "flac", "-t", "1",
        src.to_str().unwrap(),
    ]);
    let out = dir.join("out.mkv");
    let job = Job { input: src.display().to_string(), output: out.display().to_string(), hwaccel: "off".into(), ..Job::default() };
    run(&job, &mut |_| {}).unwrap();

    let want = vec![("video".to_string(), "mpeg4".to_string()), ("audio".to_string(), "aac".to_string()), ("audio".to_string(), "flac".to_string())];
    assert_eq!(ffprobe_streams(&out), want, "FFprobe lists all three tracks");

    let (src_codecs, src_tracks) = read_tracks(&src);
    let (codecs, tracks) = read_tracks(&out);
    assert_eq!(codecs, src_codecs, "IronAV reads the same codecs back");
    assert_eq!(codecs, vec![CodecId::Mpeg4, CodecId::Aac, CodecId::Flac]);
    for (i, (got, want)) in tracks.iter().zip(&src_tracks).enumerate() {
        assert!(!want.is_empty(), "track {i} has packets in the source");
        assert_eq!(got.len(), want.len(), "track {i}: packet count");
        assert!(got == want, "track {i}: the packets are the source's");
    }
}

/// Two MPEG-4 video tracks and an AAC audio track copied into one Matroska file.
#[test]
fn two_video_tracks_and_an_audio_track_are_all_written() {
    if !has_tool("ffmpeg") || !has_tool("ffprobe") {
        eprintln!("ffmpeg or ffprobe not on PATH; skipping");
        return;
    }
    let dir = scratch("mkv-two-video");
    let src = dir.join("src.mkv");
    ffmpeg(&[
        "-f", "lavfi", "-i", "testsrc=size=64x48:rate=25",
        "-f", "lavfi", "-i", "testsrc2=size=32x24:rate=25",
        "-f", "lavfi", "-i", "sine=frequency=440",
        "-map", "0:v", "-map", "1:v", "-map", "2:a",
        "-c:v", "mpeg4", "-c:a", "aac", "-t", "1",
        src.to_str().unwrap(),
    ]);
    let out = dir.join("out.mkv");
    let job = Job { input: src.display().to_string(), output: out.display().to_string(), hwaccel: "off".into(), ..Job::default() };
    run(&job, &mut |_| {}).unwrap();

    let want = vec![("video".to_string(), "mpeg4".to_string()), ("video".to_string(), "mpeg4".to_string()), ("audio".to_string(), "aac".to_string())];
    assert_eq!(ffprobe_streams(&out), want, "FFprobe lists all three tracks");

    let (src_codecs, src_tracks) = read_tracks(&src);
    let (codecs, tracks) = read_tracks(&out);
    assert_eq!(codecs, src_codecs, "IronAV reads the same codecs back");
    assert_eq!(codecs, vec![CodecId::Mpeg4, CodecId::Mpeg4, CodecId::Aac]);
    for (i, (got, want)) in tracks.iter().zip(&src_tracks).enumerate() {
        assert!(!want.is_empty(), "track {i} has packets in the source");
        assert_eq!(got.len(), want.len(), "track {i}: packet count");
        assert!(got == want, "track {i}: the packets are the source's");
    }
}
