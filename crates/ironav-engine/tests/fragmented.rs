//! Fragmented MP4 output (`fragment_seconds`): the Matroska clip with H.264 B-frames and AAC is copied
//! into a fragmented file, which reads back packet for packet like the copy made with the index first,
//! and which FFmpeg decodes to the same samples. The FFmpeg checks skip when `ffmpeg` is not on the PATH.

use ironav_engine::{plan, run, Job};
use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ironav-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn job(input: &Path, output: &Path) -> Job {
    Job { input: input.display().to_string(), output: output.display().to_string(), hwaccel: "auto".into(), ..Job::default() }
}

fn clip() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/bframes_aac.mkv")
}

/// Every packet as (stream, time shown in seconds, payload), with the edit list's lead-in taken off.
fn packets(path: &Path) -> Vec<(usize, f64, Vec<u8>)> {
    let mut d = ironav_formats::open_demuxer(path).unwrap();
    let streams = d.streams().to_vec();
    let mut out = Vec::new();
    while let Some(pk) = d.read_packet().unwrap() {
        let secs = (pk.pts - streams[pk.stream].lead_in) as f64 * streams[pk.stream].time_base.as_f64();
        out.push((pk.stream, secs, pk.data));
    }
    out
}

fn has_ffmpeg() -> bool {
    Command::new("ffmpeg").arg("-version").output().map(|o| o.status.success()).unwrap_or(false)
}

/// The decoded output of the first video or audio stream of `input`, as FFmpeg writes it raw.
fn ffmpeg_decode(input: &Path, map: &str, format: &str) -> Vec<u8> {
    let o = Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(input).args(["-map", map, "-f", format, "-"]).output().unwrap();
    assert!(o.status.success() && o.stderr.is_empty(), "ffmpeg rejected {}: {}", input.display(), String::from_utf8_lossy(&o.stderr));
    o.stdout
}

#[test]
fn fragmented_copy_reads_back_packet_for_packet() {
    let dir = scratch("frag-read");
    let (frag, plain) = (dir.join("frag.mp4"), dir.join("plain.mp4"));
    let mut j = job(&clip(), &frag);
    j.fragment_seconds = Some(0.25);
    run(&j, &mut |_| {}).unwrap();
    run(&job(&clip(), &plain), &mut |_| {}).unwrap();

    // several fragments, each with its own moof, and no index samples (the moov has an mvex)
    let raw = std::fs::read(&frag).unwrap();
    assert!(raw.windows(4).filter(|w| *w == b"moof").count() >= 2, "the clip is cut into more than one fragment");
    assert!(raw.windows(4).any(|w| w == b"mvex"));

    let (from, back) = (packets(&clip()), packets(&frag));
    assert_eq!(from.len(), 20 + 17, "20 video frames and 17 AAC packets");
    assert_eq!(back.len(), from.len());
    for stream in 0..2 {
        let a: Vec<_> = from.iter().filter(|p| p.0 == stream).collect();
        let b: Vec<_> = back.iter().filter(|p| p.0 == stream).collect();
        assert_eq!(a.len(), b.len(), "stream {stream}");
        for (i, (x, y)) in a.iter().zip(&b).enumerate() {
            assert_eq!(x.2, y.2, "stream {stream} packet {i}: payload");
            assert!((x.1 - y.1).abs() < 1e-3, "stream {stream} packet {i}: shown at {} s in the Matroska, {} s in the fragmented copy", x.1, y.1);
        }
    }
    assert_eq!(packets(&plain).len(), from.len());
}

#[test]
fn fragmented_copy_decodes_in_ffmpeg_like_the_plain_copy() {
    if !has_ffmpeg() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    let dir = scratch("frag-ffmpeg");
    let (frag, plain) = (dir.join("frag.mp4"), dir.join("plain.mp4"));
    let mut j = job(&clip(), &frag);
    j.fragment_seconds = Some(0.25);
    run(&j, &mut |_| {}).unwrap();
    run(&job(&clip(), &plain), &mut |_| {}).unwrap();

    let video = ffmpeg_decode(&frag, "0:v:0", "rawvideo");
    assert!(!video.is_empty());
    assert!(video == ffmpeg_decode(&plain, "0:v:0", "rawvideo"), "FFmpeg decodes the same pictures from both files");
    let audio = ffmpeg_decode(&frag, "0:a:0", "s16le");
    assert!(!audio.is_empty());
    assert!(audio == ffmpeg_decode(&plain, "0:a:0", "s16le"), "FFmpeg decodes the same audio from both files");
}

/// Audio alone (a FLAC-coded WAV, as the MP4 writer holds it) cuts a fragment at any packet once it is long
/// enough, and every sample reads back.
#[test]
fn fragmented_audio_only_file_reads_back_every_sample() {
    let dir = scratch("frag-audio");
    let (wav, out) = (dir.join("in.wav"), dir.join("out.mp4"));
    let samples: Vec<u8> = (0..3 * 44100i32).flat_map(|i| (((i * 37) % 2000 - 1000) as i16).to_le_bytes()).collect();
    let mut f = Vec::new();
    f.extend_from_slice(b"RIFF");
    f.extend_from_slice(&(36 + samples.len() as u32).to_le_bytes());
    f.extend_from_slice(b"WAVEfmt ");
    f.extend_from_slice(&16u32.to_le_bytes());
    f.extend_from_slice(&[1, 0, 1, 0]); // PCM, mono
    f.extend_from_slice(&44100u32.to_le_bytes());
    f.extend_from_slice(&(44100u32 * 2).to_le_bytes());
    f.extend_from_slice(&[2, 0, 16, 0]);
    f.extend_from_slice(b"data");
    f.extend_from_slice(&(samples.len() as u32).to_le_bytes());
    f.extend_from_slice(&samples);
    std::fs::write(&wav, f).unwrap();

    let mut j = job(&wav, &out);
    j.audio.codec = Some("flac".into());
    j.fragment_seconds = Some(1.0);
    run(&j, &mut |_| {}).unwrap();
    let raw = std::fs::read(&out).unwrap();
    assert!(raw.windows(4).filter(|w| *w == b"moof").count() >= 2, "three seconds make more than one fragment");
    let back = packets(&out);
    let last = back.last().expect("packets were read");
    assert!(last.1 > 2.0 && last.1 < 3.0, "the last packet is shown in the third second, at {} s", last.1);
}

#[test]
fn fragment_seconds_is_for_mp4_and_mov_only_and_must_be_positive() {
    let dir = scratch("frag-refuse");
    let mut j = job(&clip(), &dir.join("out.wav"));
    j.fragment_seconds = Some(1.0);
    let err = plan(&j).unwrap_err();
    assert!(err.message.contains("mp4 and mov"), "{}", err.message);

    j.output = dir.join("out.mp4").display().to_string();
    j.fragment_seconds = Some(0.0);
    assert!(plan(&j).is_err(), "a zero fragment length is refused");
    j.fragment_seconds = Some(f64::NAN);
    assert!(plan(&j).is_err(), "a fragment length that is not a number is refused");
    j.fragment_seconds = Some(2.0);
    assert!(plan(&j).is_ok());
}
