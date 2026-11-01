//! Matroska content encodings: a PCM track whose frames were stored with header stripping, and one whose frames
//! were stored zlib-compressed (both written by hand, as FFmpeg's Matroska muxer does not write encodings). The
//! reader gives back the frames whole, and FFmpeg, which reads the same encodings, decodes them to the same bytes.
//! The FFmpeg check skips when `ffmpeg` is not on the PATH.

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

/// The concatenated payloads of the file's first track.
fn payload_bytes(path: &Path) -> Vec<u8> {
    let mut d = ironav_formats::open_demuxer(path).unwrap();
    let mut out = Vec::new();
    while let Some(p) = d.read_packet().unwrap() {
        out.extend_from_slice(&p.data);
    }
    out
}

#[test]
fn header_stripped_and_zlib_pcm_read_to_the_frames_they_were_made_from() {
    let want = std::fs::read(data("mkv_encoded_pcm.ref")).unwrap();
    for name in ["mkv_strip_pcm.mkv", "mkv_zlib_pcm.mkv"] {
        let got = payload_bytes(&data(name));
        assert_eq!(got.len(), want.len(), "{name}: every frame comes back whole");
        assert!(got == want, "{name}: the frames are the originals");
    }
}

#[test]
fn encoded_pcm_decodes_in_ffmpeg_to_what_ironav_reads() {
    let ok = Command::new("ffmpeg").arg("-version").output().map(|o| o.status.success()).unwrap_or(false);
    if !ok {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    let dir = scratch("mkv-enc");
    let want = std::fs::read(data("mkv_encoded_pcm.ref")).unwrap();
    for name in ["mkv_strip_pcm.mkv", "mkv_zlib_pcm.mkv"] {
        let out = dir.join(format!("{name}.s16"));
        let o = Command::new("ffmpeg").args(["-v", "error", "-y", "-i"]).arg(data(name)).args(["-c:a", "pcm_s16le", "-f", "s16le"]).arg(&out).output().unwrap();
        assert!(o.status.success() && o.stderr.is_empty(), "ffmpeg rejected {name}: {}", String::from_utf8_lossy(&o.stderr));
        assert!(std::fs::read(&out).unwrap() == want, "{name}: FFmpeg decodes the same samples");
    }
}
