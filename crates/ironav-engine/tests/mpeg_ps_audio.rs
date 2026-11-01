//! MPEG program streams with every stream in them: the streams IronAV finds are the ones ffprobe
//! lists, each audio stream decodes to the samples FFmpeg decodes from the same file, and a
//! transcode to MP4 keeps audio and video in sync. The files are made by FFmpeg: MPEG-PS with MP2
//! and two video streams, and VOB with AC-3 and DVD linear PCM at 16 and 24 bits. Skipped without
//! `ffmpeg` and `ffprobe` on the PATH.

use ironav_codec::new_decoder_for;
use ironav_core::{Frame, MediaType, Packet, StreamParams};
use ironav_engine::{run, Job, StreamOpts};
use std::path::{Path, PathBuf};
use std::process::Command;

fn tools_available() -> bool {
    ["ffmpeg", "ffprobe"].iter().all(|t| Command::new(t).arg("-version").output().is_ok_and(|o| o.status.success()))
}

/// A directory of the test's own, under the target's temporary directory.
fn scratch(tag: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("mpeg-ps-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn ffmpeg(args: &[&str]) {
    let status = Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).status().expect("running ffmpeg");
    assert!(status.success(), "ffmpeg {args:?} failed");
}

/// Two video streams and two audio streams (sines at 440 and 880 Hz, 48 kHz), two seconds long.
/// `audio` gives the codec options for the two audio streams.
fn make(dir: &Path, name: &str, container: &str, audio: &[&str]) -> PathBuf {
    let out = dir.join(format!("{name}.{container}"));
    let mut args: Vec<&str> = vec![
        "-f", "lavfi", "-i", "testsrc2=size=176x144:rate=25",
        "-f", "lavfi", "-i", "testsrc2=size=176x144:rate=25",
        "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000",
        "-f", "lavfi", "-i", "sine=frequency=880:sample_rate=48000",
        "-t", "2", "-map", "0:v", "-map", "1:v", "-map", "2:a", "-map", "3:a",
        "-c:v", "mpeg2video", "-b:v", "1M", "-b:a", "192k",
    ];
    args.extend_from_slice(audio);
    args.extend_from_slice(&["-f", container]);
    let p = out.to_str().unwrap();
    args.push(p);
    ffmpeg(&args);
    out
}

/// `(media type, codec name)` of each stream, as ffprobe reports them.
fn ffprobe_streams(path: &Path) -> Vec<(String, String)> {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "stream=codec_type,codec_name", "-of", "csv=p=0", path.to_str().unwrap()])
        .output()
        .expect("running ffprobe");
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| {
            let fields: Vec<&str> = l.split(',').filter(|s| !s.is_empty()).collect();
            let kind = ["video", "audio", "subtitle", "data"].into_iter().find(|k| fields.contains(k)).expect("a media type");
            let codec = fields.iter().find(|f| **f != kind).expect("a codec name");
            (kind.to_string(), codec.to_string())
        })
        .collect()
}

fn read_all(path: &Path) -> (Vec<ironav_core::StreamInfo>, Vec<Packet>) {
    let mut d = ironav_formats::open_demuxer(path).unwrap();
    let streams = d.streams().to_vec();
    let mut packets = Vec::new();
    while let Some(p) = d.read_packet().unwrap() {
        packets.push(p);
    }
    (streams, packets)
}

/// IronAV's decoding of one stream, as the interleaved samples of its frames.
fn decode_ours(s: &ironav_core::StreamInfo, packets: &[Packet]) -> Vec<f64> {
    let mut dec = new_decoder_for(s).unwrap();
    let mut out = Vec::new();
    let mut take = |frames: Vec<Frame>| {
        for f in frames {
            if let Frame::Audio(a) = f {
                out.extend(a.samples);
            }
        }
    };
    for p in packets.iter().filter(|p| p.stream == s.index) {
        take(dec.decode(p).unwrap());
    }
    take(dec.flush().unwrap());
    out
}

/// FFmpeg's decoding of the `a`th audio stream of `path`, as 64-bit float samples.
fn decode_ffmpeg(path: &Path, a: usize) -> Vec<f64> {
    let raw = path.with_extension(format!("audio{a}.f64le"));
    ffmpeg(&["-i", path.to_str().unwrap(), "-map", &format!("0:a:{a}"), "-f", "f64le", raw.to_str().unwrap()]);
    std::fs::read(&raw).unwrap().as_chunks::<8>().0.iter().map(|b| f64::from_le_bytes(*b)).collect()
}

/// How close each audio stream's decoding must be to FFmpeg's, as `(tolerance, lsb)` per stream: within
/// `tolerance` of each other, or (`lsb` true) equal once both are rounded to 16-bit samples, up to one
/// step of `tolerance`.
fn check_audio(path: &Path, tols: &[(f64, bool)]) -> usize {
    let (streams, packets) = read_all(path);
    let audio: Vec<_> = streams.iter().filter(|s| s.codec.media_type() == MediaType::Audio).collect();
    assert_eq!(audio.len(), tols.len(), "{}: the audio streams", path.display());
    for (a, s) in audio.iter().enumerate() {
        let (abs, lsb) = tols[a];
        let (ours, theirs) = (decode_ours(s, &packets), decode_ffmpeg(path, a));
        assert_eq!(ours.len(), theirs.len(), "{}: audio {a} ({}) has a different number of samples", path.display(), s.codec.name());
        let worst = if lsb {
            let q = |x: f64| (x * 32768.0).round().clamp(-32768.0, 32767.0);
            ours.iter().zip(&theirs).map(|(x, y)| (q(*x) - q(*y)).abs() / 32768.0).fold(0.0, f64::max)
        } else {
            ours.iter().zip(&theirs).map(|(x, y)| (x - y).abs()).fold(0.0, f64::max)
        };
        assert!(worst <= abs, "{}: audio {a} ({}) differs from FFmpeg by {worst}", path.display(), s.codec.name());
    }
    audio.len()
}

#[test]
fn program_streams_list_the_streams_ffprobe_lists() {
    if !tools_available() {
        eprintln!("ffmpeg/ffprobe not on PATH; skipping");
        return;
    }
    let d = scratch("list");
    let files = [
        make(&d, "mp2", "mpeg", &["-c:a", "mp2"]),
        make(&d, "ac3", "vob", &["-c:a", "ac3"]),
        make(&d, "ac3_lpcm", "vob", &["-c:a:0", "ac3", "-c:a:1", "pcm_dvd"]),
    ];
    for f in &files {
        let (streams, _) = read_all(f);
        let ours: Vec<(String, String)> = streams
            .iter()
            .map(|s| {
                let kind = match s.params {
                    StreamParams::Video { .. } => "video",
                    StreamParams::Audio { .. } => "audio",
                };
                // IronAV carries DVD linear PCM as the little-endian PCM it converts to
                let codec = if s.codec.name().starts_with("pcm_") { "pcm_dvd".to_string() } else { s.codec.name().to_string() };
                (kind.to_string(), codec)
            })
            .collect();
        assert_eq!(ours, ffprobe_streams(f), "{}", f.display());
    }
}

#[test]
fn mp2_audio_decodes_like_ffmpeg() {
    if !tools_available() {
        eprintln!("ffmpeg/ffprobe not on PATH; skipping");
        return;
    }
    let d = scratch("mp2");
    let f = make(&d, "mp2", "mpeg", &["-c:a", "mp2"]);
    // one LSB of 16-bit samples
    assert_eq!(check_audio(&f, &[(1.0, true), (1.0, true)]), 2);
}

#[test]
fn ac3_audio_decodes_like_ffmpeg() {
    if !tools_available() {
        eprintln!("ffmpeg/ffprobe not on PATH; skipping");
        return;
    }
    let d = scratch("ac3");
    let f = make(&d, "ac3", "vob", &["-c:a", "ac3"]);
    assert_eq!(check_audio(&f, &[(5e-5, false), (5e-5, false)]), 2);
}

#[test]
fn dvd_lpcm_16_bit_decodes_like_ffmpeg() {
    if !tools_available() {
        eprintln!("ffmpeg/ffprobe not on PATH; skipping");
        return;
    }
    let d = scratch("lpcm16");
    let f = make(&d, "ac3_lpcm16", "vob", &["-c:a:0", "ac3", "-c:a:1", "pcm_dvd", "-ac:a:1", "2"]);
    // the AC-3 stream as above, the PCM exact: the same samples, not merely close
    assert_eq!(check_audio(&f, &[(5e-5, false), (1e-9, false)]), 2);
}

#[test]
fn dvd_lpcm_24_bit_decodes_like_ffmpeg() {
    if !tools_available() {
        eprintln!("ffmpeg/ffprobe not on PATH; skipping");
        return;
    }
    let d = scratch("lpcm24");
    let f = make(&d, "ac3_lpcm24", "vob", &["-c:a:0", "ac3", "-c:a:1", "pcm_dvd", "-ac:a:1", "2", "-sample_fmt:a:1", "s32", "-bits_per_raw_sample:a:1", "24"]);
    assert_eq!(check_audio(&f, &[(5e-5, false), (1e-9, false)]), 2);
}

#[test]
fn transcode_to_mp4_keeps_audio_and_video_in_sync() {
    if !tools_available() {
        eprintln!("ffmpeg/ffprobe not on PATH; skipping");
        return;
    }
    let d = scratch("sync");
    let src = make(&d, "mp2", "mpeg", &["-c:a", "mp2"]);
    let out = d.join("out.mp4");
    let job = Job {
        input: src.display().to_string(),
        output: out.display().to_string(),
        hwaccel: "off".into(),
        video: StreamOpts { codec: Some("h264".into()), qp: Some(28), ..StreamOpts::default() },
        audio: StreamOpts { codec: Some("aac".into()), ..StreamOpts::default() },
        ..Job::default()
    };
    run(&job, &mut |_| {}).unwrap();
    // the first presentation time of each stream, from FFmpeg's reading of the MP4
    let times = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "stream=codec_type,start_time", "-of", "csv=p=0", out.to_str().unwrap()])
        .output()
        .unwrap();
    let text = String::from_utf8(times.stdout).unwrap();
    let start = |kind: &str| -> f64 {
        text.lines().find(|l| l.starts_with(kind)).and_then(|l| l.split(',').nth(1)).and_then(|t| t.parse().ok()).unwrap_or_else(|| panic!("no {kind} start in {text}"))
    };
    let (v, a) = (start("video"), start("audio"));
    let frame = 1.0 / 25.0;
    assert!((v - a).abs() <= frame, "video starts at {v} s and audio at {a} s");
}
