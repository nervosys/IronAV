//! PCM in Matroska: FFmpeg writes each sample format (`A_PCM/INT/LIT`, `A_PCM/INT/BIG` and `A_PCM/FLOAT/IEEE`),
//! and IronAV reads it to the same bytes FFmpeg decodes; IronAV writes PCM, and FFmpeg reads it back to the
//! same bytes too. The FFmpeg checks skip when `ffmpeg` is not on the PATH.

use ironav_core::CodecId;
use ironav_engine::{run, Job};
use std::path::{Path, PathBuf};
use std::process::Command;

fn data(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data").join(name)
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ironav-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn has_ffmpeg() -> bool {
    Command::new("ffmpeg").arg("-version").output().map(|o| o.status.success()).unwrap_or(false)
}

/// The payloads of the first audio stream of `path`, concatenated, with the codec and sample rate.
fn read_pcm(path: &Path) -> (CodecId, u32, u16, Vec<u8>, i64) {
    let mut d = ironav_formats::open_demuxer(path).unwrap();
    let info = d.streams()[0].clone();
    let (rate, channels) = match info.params {
        ironav_core::StreamParams::Audio { sample_rate, channels } => (sample_rate, channels),
        _ => panic!("the first stream is not audio"),
    };
    let mut bytes = Vec::new();
    let mut frames = 0i64;
    while let Some(p) = d.read_packet().unwrap() {
        frames += p.duration;
        bytes.extend_from_slice(&p.data);
    }
    (info.codec, rate, channels, bytes, frames)
}

/// The raw audio FFmpeg decodes from `input`, as little-endian samples of the format `codec`.
fn ffmpeg_pcm(input: &Path, codec: &str, format: &str) -> Vec<u8> {
    let o = Command::new("ffmpeg").args(["-v", "error", "-i"]).arg(input).args(["-map", "0:a:0", "-c:a", codec, "-f", format, "-"]).output().unwrap();
    assert!(o.status.success() && o.stderr.is_empty(), "ffmpeg rejected {}: {}", input.display(), String::from_utf8_lossy(&o.stderr));
    o.stdout
}

/// Each FFmpeg-written fixture: the IronAV reader's samples are the bytes FFmpeg decodes.
#[test]
fn ffmpeg_written_pcm_in_matroska_reads_to_what_ffmpeg_decodes() {
    let cases = [
        ("pcm_s16le", CodecId::PcmS16Le, 2),
        ("pcm_s16be", CodecId::PcmS16Le, 2),
        ("pcm_s24le", CodecId::PcmS24Le, 3),
        ("pcm_s32le", CodecId::PcmS32Le, 4),
        ("pcm_f32le", CodecId::PcmF32Le, 4),
        ("pcm_f64le", CodecId::PcmF64Le, 8),
        ("pcm_u8", CodecId::PcmU8, 1),
    ];
    for (name, codec, width) in cases {
        let (got_codec, rate, channels, bytes, frames) = read_pcm(&data(&format!("{name}.mkv")));
        assert_eq!(got_codec, codec, "{name}: codec");
        assert_eq!((rate, channels), (8000, 1), "{name}: rate and channels");
        assert_eq!(frames, 2000, "{name}: 0.25 s of samples, however the blocks are cut");
        assert_eq!(bytes.len(), 2000 * width, "{name}: payload size");
        // the reference is what FFmpeg decodes, in the little-endian form of the same format
        let want = std::fs::read(data(&format!("{name}.ref"))).unwrap();
        assert!(bytes == want, "{name}: the samples are the ones FFmpeg decodes");
    }
}

/// The WAV source copied into Matroska keeps every sample, and FFmpeg reads the copy to the same bytes.
#[test]
fn pcm_written_to_matroska_reads_back_and_ffmpeg_reads_it() {
    let dir = scratch("pcm-mkv-write");
    let wav = data("pcm_source.wav");
    let (_, _, _, source, _) = read_pcm(&wav);

    let out = dir.join("copy.mkv");
    let job = Job { input: wav.display().to_string(), output: out.display().to_string(), hwaccel: "off".into(), ..Job::default() };
    run(&job, &mut |_| {}).unwrap();
    let (codec, rate, channels, bytes, frames) = read_pcm(&out);
    assert_eq!((codec, rate, channels, frames), (CodecId::PcmS16Le, 8000, 1, 2000));
    assert!(bytes == source, "the copy holds the WAV's samples");

    // a transcode to 24 bits: IronAV's copy reads back to what FFmpeg makes of the same conversion
    let out24 = dir.join("s24.mkv");
    let mut j = Job { input: wav.display().to_string(), output: out24.display().to_string(), hwaccel: "off".into(), ..Job::default() };
    j.audio.codec = Some("pcm_s24le".into());
    run(&j, &mut |_| {}).unwrap();
    let (codec, _, _, bytes24, _) = read_pcm(&out24);
    assert_eq!(codec, CodecId::PcmS24Le);
    if has_ffmpeg() {
        assert!(ffmpeg_pcm(&out, "pcm_s16le", "s16le") == source, "FFmpeg reads the copy to the WAV's samples");
        assert!(ffmpeg_pcm(&out24, "pcm_s24le", "s24le") == bytes24, "FFmpeg reads the 24-bit copy to the samples IronAV wrote");
    } else {
        eprintln!("ffmpeg not on PATH; skipping the FFmpeg reads");
    }
}
