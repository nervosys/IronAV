//! AVI files made by FFmpeg, read with `open_demuxer`: the streams, their times, a decode of the
//! pictures and of the sound, and seeking, with and without the `idx1` index. Skipped without `ffmpeg`
//! on PATH.

use ironav_core::{CodecId, Demuxer, Frame, StreamParams};
use ironav_formats::open_demuxer;
use std::path::{Path, PathBuf};
use std::process::Command;

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success())
}

fn scratch() -> PathBuf {
    let test = std::thread::current().name().unwrap_or("test").replace("::", "-");
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("avi-{}-{test}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A half-second 10 fps, 64x48 test pattern and a sine, written with `out` as AVI.
fn make(dir: &Path, name: &str, out: &[&str]) -> Option<PathBuf> {
    let file = dir.join(name);
    let ok = Command::new("ffmpeg")
        .args(["-v", "error", "-y", "-f", "lavfi", "-i", "testsrc2=size=64x48:rate=10:duration=1", "-f", "lavfi", "-i", "sine=frequency=440:sample_rate=44100:duration=1"])
        .args(out)
        .arg(&file)
        .status()
        .map(|s| s.success())
        .unwrap_or(false);
    ok.then_some(file)
}

struct Read {
    streams: Vec<ironav_core::StreamInfo>,
    video_pts: Vec<i64>,
    audio_samples: i64,
    pictures: usize,
    sound_frames: usize,
}

fn read(file: &Path) -> Read {
    let mut d: Box<dyn Demuxer> = open_demuxer(file).expect("the file opens");
    assert_eq!(d.format_name(), "avi");
    let streams = d.streams().to_vec();
    let mut decoders: Vec<_> = streams.iter().map(|s| ironav_codec::new_decoder_for(s).expect("a decoder")).collect();
    let mut r = Read { streams, video_pts: Vec::new(), audio_samples: 0, pictures: 0, sound_frames: 0 };
    while let Some(p) = d.read_packet().unwrap() {
        if r.streams[p.stream].params.media_type() == ironav_core::MediaType::Video {
            r.video_pts.push(p.pts);
        } else {
            r.audio_samples += p.duration;
        }
        for f in decoders[p.stream].decode(&p).unwrap() {
            match f {
                Frame::Video(_) => r.pictures += 1,
                Frame::Audio(a) => r.sound_frames += a.frames(),
            }
        }
    }
    for (i, dec) in decoders.iter_mut().enumerate() {
        for f in dec.flush().unwrap() {
            match f {
                Frame::Video(_) => r.pictures += 1,
                Frame::Audio(a) => r.sound_frames += a.frames(),
            }
        }
        let _ = i;
    }
    r
}

#[test]
fn mjpeg_and_pcm() {
    if !ffmpeg_available() {
        return;
    }
    let dir = scratch();
    let f = make(&dir, "a.avi", &["-c:v", "mjpeg", "-c:a", "pcm_s16le"]).expect("ffmpeg writes the file");
    let r = read(&f);
    assert_eq!((r.streams[0].codec, r.streams[1].codec), (CodecId::Mjpeg, CodecId::PcmS16Le));
    assert!(matches!(r.streams[0].params, StreamParams::Video { width: 64, height: 48, .. }));
    assert_eq!(r.video_pts, (0..10).collect::<Vec<_>>());
    assert_eq!(r.audio_samples, 44100);
    assert_eq!(r.pictures, 10);
    assert_eq!(r.sound_frames, 44100);
    // no idx1: the same file, found by scanning
    let g = make(&dir, "b.avi", &["-c:v", "mjpeg", "-c:a", "pcm_s16le", "-write_index", "0"]).unwrap();
    let r2 = read(&g);
    assert_eq!(r2.video_pts, r.video_pts);
    assert_eq!(r2.audio_samples, r.audio_samples);
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn mpeg4_with_b_frames_and_mp3() {
    if !ffmpeg_available() {
        return;
    }
    let dir = scratch();
    let f = make(&dir, "a.avi", &["-c:v", "mpeg4", "-bf", "2", "-vtag", "XVID", "-c:a", "libmp3lame"]).expect("ffmpeg writes the file");
    let r = read(&f);
    assert_eq!((r.streams[0].codec, r.streams[1].codec), (CodecId::Mpeg4, CodecId::Mp3));
    assert!(!r.streams[0].extradata.is_empty(), "the VOL header is found in the first frame");
    assert_eq!(r.pictures, 10);
    assert!(r.sound_frames >= 44100, "{}", r.sound_frames);
    let mut d = open_demuxer(&f).unwrap();
    let landed = d.seek(0.55).unwrap();
    assert!((0.0..=0.55).contains(&landed), "{landed}");
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn h264_and_ac3_and_mp2() {
    if !ffmpeg_available() {
        return;
    }
    let dir = scratch();
    let f = make(&dir, "a.avi", &["-c:v", "libx264", "-preset", "ultrafast", "-c:a", "ac3"]).expect("ffmpeg writes the file");
    let r = read(&f);
    assert_eq!((r.streams[0].codec, r.streams[1].codec), (CodecId::H264, CodecId::Ac3));
    assert_eq!(r.pictures, 10);
    let f = make(&dir, "b.avi", &["-c:v", "mjpeg", "-c:a", "mp2"]).unwrap();
    let r = read(&f);
    assert_eq!(r.streams[1].codec, CodecId::Mp2);
    assert!(r.sound_frames >= 44100);
    std::fs::remove_dir_all(dir).ok();
}

#[test]
fn unsupported_codecs_are_reported() {
    if !ffmpeg_available() {
        return;
    }
    let dir = scratch();
    let f = make(&dir, "a.avi", &["-c:v", "mjpeg", "-c:a", "adpcm_ms"]).expect("ffmpeg writes the file");
    let d = open_demuxer(&f).unwrap();
    assert_eq!(d.streams().len(), 1);
    assert_eq!(d.unsupported_streams().len(), 1, "{:?}", d.unsupported_streams());
    std::fs::remove_dir_all(dir).ok();
}
