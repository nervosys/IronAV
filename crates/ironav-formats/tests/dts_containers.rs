//! DTS in its containers: a short stereo core stream made with FFmpeg's DTS encoder (the fixture
//! `data/tone_stereo.dts`, 15 KB) is read with `open_demuxer` as a raw `.dts` file, and, muxed by
//! FFmpeg into Matroska, MP4, MPEG-TS and WAV (made at test time, so those are skipped without
//! `ffmpeg` on PATH), decodes with the engine's decoder to what FFmpeg's decoder makes of the same
//! file. Also a raw file with an extension substream after each core frame.

use ironav_core::{CodecId, Demuxer, Frame, Packet, StreamParams};
use ironav_formats::open_demuxer;
use std::path::{Path, PathBuf};
use std::process::Command;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/data/tone_stereo.dts");

fn ffmpeg(args: &[&str]) -> bool {
    Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).status().map(|s| s.success()).unwrap_or(false)
}

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success())
}

fn scratch() -> PathBuf {
    // one directory per test: the tests run in parallel and each removes its own
    let test = std::thread::current().name().unwrap_or("test").replace("::", "-");
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("dts-container-{}-{test}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// FFmpeg's decode of `file` as f32 samples.
fn reference(dir: &Path, name: &str, file: &Path) -> Vec<f32> {
    let out = dir.join(format!("{name}.f32"));
    assert!(ffmpeg(&["-i", file.to_str().unwrap(), "-f", "f32le", "-acodec", "pcm_f32le", out.to_str().unwrap()]), "decoding {name} with FFmpeg");
    std::fs::read(&out).unwrap().as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect()
}

/// Demux `file` with the formats registry, decode its DTS packets, and return the samples with the
/// stream's rate and channel count.
fn demux_and_decode(file: &Path) -> (Vec<f32>, u32, u16) {
    let mut d: Box<dyn Demuxer> = open_demuxer(file).expect("the file opens");
    assert_eq!(d.streams().len(), 1);
    let stream = d.streams()[0].clone();
    assert_eq!(stream.codec, CodecId::Dts);
    let StreamParams::Audio { sample_rate, channels } = stream.params else { panic!("audio stream expected") };
    let mut dec = ironav_codec::new_decoder_for(&stream).expect("a DTS decoder");
    let mut out = Vec::new();
    while let Some(p) = d.read_packet().expect("a packet") {
        let p = Packet { stream: 0, ..p };
        for f in dec.decode(&p).expect("a decoded frame") {
            let Frame::Audio(a) = f else { panic!("audio expected") };
            assert_eq!((a.sample_rate, a.channels), (sample_rate, channels));
            out.extend(a.samples.iter().map(|&x| x as f32));
        }
    }
    (out, sample_rate, channels)
}

/// The signal-to-noise ratio of `got` against `want`, which must have the same length.
fn check(name: &str, got: &[f32], want: &[f32]) {
    assert_eq!(got.len(), want.len(), "{name}: sample count");
    let (mut sig, mut noise) = (0f64, 0f64);
    for (a, b) in got.iter().zip(want) {
        sig += (*b as f64).powi(2);
        noise += ((a - b) as f64).powi(2);
    }
    let snr = 10.0 * (sig / noise.max(1e-300)).log10();
    eprintln!("{name:>20}: SNR {snr:.1} dB over {} samples", got.len());
    assert!(snr > 80.0, "{name}: SNR {snr} dB against FFmpeg");
}

/// Mux the fixture into `name` (a container FFmpeg writes), and compare what our demuxer and decoder
/// make of it with FFmpeg's decode of the same file.
fn mux_and_compare(dir: &Path, name: &str, args: &[&str]) {
    let file = dir.join(name);
    let mut a = vec!["-i", FIXTURE, "-c", "copy"];
    a.extend_from_slice(args);
    a.push(file.to_str().unwrap());
    assert!(ffmpeg(&a), "muxing {name}");
    let want = reference(dir, name, &file);
    let (got, rate, ch) = demux_and_decode(&file);
    assert_eq!((rate, ch), (48000, 2), "{name}: stream");
    check(name, &got, &want);
}

/// The fixture needs no FFmpeg to read: 24 frames of 512 samples, stereo at 48 kHz, decoding to
/// the same count of samples.
#[test]
fn raw_dts_opens_and_decodes() {
    let mut d = open_demuxer(Path::new(FIXTURE)).expect("the fixture opens");
    let s = &d.streams()[0];
    assert_eq!((s.codec, s.time_base.den), (CodecId::Dts, 48000));
    assert!(matches!(s.params, StreamParams::Audio { sample_rate: 48000, channels: 2 }));
    let frames = s.duration.unwrap() / 512;
    assert!(frames >= 20 && s.duration.unwrap() % 512 == 0, "whole frames: {:?}", s.duration);
    let mut n = 0;
    while d.read_packet().unwrap().is_some() {
        n += 1;
    }
    assert_eq!(n, frames);
    let (samples, _, ch) = demux_and_decode(Path::new(FIXTURE));
    assert_eq!(samples.len() as i64, frames * 512 * ch as i64);
}

#[test]
fn raw_dts_matches_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch();
    let want = reference(&dir, "raw", Path::new(FIXTURE));
    let (got, _, _) = demux_and_decode(Path::new(FIXTURE));
    check("raw.dts", &got, &want);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn matroska_dts_matches_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch();
    mux_and_compare(&dir, "tone.mkv", &["-strict", "-2"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mp4_dts_matches_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch();
    mux_and_compare(&dir, "tone.mp4", &["-strict", "-2"]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mpegts_dts_matches_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch();
    // FFmpeg's stream type for DTS is 0x82
    mux_and_compare(&dir, "tone.ts", &[]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn wav_dts_matches_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch();
    mux_and_compare(&dir, "tone.wav", &[]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// A raw file where an extension substream follows each core frame: the packets keep it and the
/// decoder leaves it out, so the samples are those of the plain file.
#[test]
fn extension_substreams_stay_in_the_packets() {
    let core = std::fs::read(FIXTURE).unwrap();
    let frame = ironav_codec::dts::frame_info(&core).unwrap();
    assert_eq!(frame.offset, 0);
    let ext = [0x64, 0x58, 0x20, 0x25, 0, 0x01, 0x60, 0x01, 0x60, 0xAA, 0xBB, 0xCC];
    assert_eq!(frame.frame_size % 4, 0);
    let mut with_ext = Vec::new();
    for f in core.chunks(frame.frame_size) {
        with_ext.extend_from_slice(f);
        if f.len() == frame.frame_size {
            with_ext.extend_from_slice(&ext);
        }
    }
    let dir = scratch();
    let file = dir.join("ext.dts");
    std::fs::write(&file, &with_ext).unwrap();
    let (plain, _, _) = demux_and_decode(Path::new(FIXTURE));
    // the fake substream's size fields say 12 bytes: the packet is the frame and the substream
    let mut d = open_demuxer(&file).unwrap();
    let first = d.read_packet().unwrap().unwrap();
    assert!(first.data.len() >= frame.frame_size);
    let (got, _, _) = demux_and_decode(&file);
    assert_eq!(got, plain);
    let _ = std::fs::remove_dir_all(&dir);
}
