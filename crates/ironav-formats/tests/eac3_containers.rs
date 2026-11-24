//! E-AC-3 in its containers: a stream made with FFmpeg's E-AC-3 encoder, read with `open_demuxer`,
//! decodes with the engine's decoder for the stream to what FFmpeg's decoder makes of the same file:
//! raw `.eac3` and `.ec3` files, Matroska, MP4 and MPEG-TS (both stream types FFmpeg writes). Also a
//! raw file with dependent frames (the packets keep them, the decoder leaves them out) and a raw file
//! copied into Matroska. Skipped without `ffmpeg` on PATH.

use ironav_core::{CodecId, Demuxer, Frame, Packet, StreamParams};
use ironav_formats::open_demuxer;
use std::path::{Path, PathBuf};
use std::process::Command;

const SIGNAL: &str = "aevalsrc=exprs='0.4*sin(2*PI*440*t)+0.3*(random(0)-0.5)*(lt(mod(t,0.25),0.02))|0.3*sin(2*PI*660*t)':s=48000:d=2";

fn ffmpeg(args: &[&str]) -> bool {
    Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).status().map(|s| s.success()).unwrap_or(false)
}

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success())
}

fn scratch() -> PathBuf {
    // one directory per test: the tests run in parallel and each removes its own
    let test = std::thread::current().name().unwrap_or("test").replace("::", "-");
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("eac3-container-{}-{test}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// FFmpeg's decode of `file` as f32 samples.
fn reference(dir: &Path, name: &str, file: &Path) -> Vec<f32> {
    let out = dir.join(format!("{name}.f32"));
    assert!(ffmpeg(&["-i", file.to_str().unwrap(), "-f", "f32le", "-acodec", "pcm_f32le", out.to_str().unwrap()]), "decoding {name} with FFmpeg");
    std::fs::read(&out).unwrap().as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect()
}

/// Demux `file` with the formats registry, decode its E-AC-3 packets, and return the samples with the
/// stream's rate and channel count.
fn demux_and_decode(file: &Path) -> (Vec<f32>, u32, u16) {
    let mut d: Box<dyn Demuxer> = open_demuxer(file).expect("the file opens");
    assert_eq!(d.streams().len(), 1);
    let stream = d.streams()[0].clone();
    assert_eq!(stream.codec, CodecId::Eac3);
    let StreamParams::Audio { sample_rate, channels } = stream.params else { panic!("audio stream expected") };
    let mut dec = ironav_codec::new_decoder_for(&stream).expect("an E-AC-3 decoder");
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

/// Encode E-AC-3 into `name` (a container FFmpeg writes it to) and compare what our demuxer and
/// decoder make of it with FFmpeg's decode. Returns the file and the channel count it holds.
fn encode_and_compare(dir: &Path, name: &str, format: &str, channels: &str, extra: &[&str]) -> (PathBuf, u16) {
    let file = dir.join(name);
    let mut args = vec!["-f", "lavfi", "-i", SIGNAL, "-ac", channels, "-c:a", "eac3", "-b:a", "256k"];
    args.extend_from_slice(extra);
    args.extend(["-f", format, file.to_str().unwrap()]);
    assert!(ffmpeg(&args), "encoding {name}");
    let want = reference(dir, name, &file);
    let (got, _, ch) = demux_and_decode(&file);
    assert_eq!(ch.to_string(), channels, "{name}: channels");
    check(name, &got, &want);
    (file, ch)
}

#[test]
fn raw_eac3_matches_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch();
    let (file, _) = encode_and_compare(&dir, "raw_stereo.eac3", "eac3", "2", &[]);
    // the stream: E-AC-3, 48 kHz, 1536 samples a frame
    let d = open_demuxer(&file).unwrap();
    let s = &d.streams()[0];
    assert_eq!((s.codec, s.time_base.den), (CodecId::Eac3, 48000));
    assert_eq!(s.duration, Some(63 * 1536), "63 frames of 1536 samples");
    encode_and_compare(&dir, "raw_surround.ec3", "eac3", "6", &[]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn matroska_eac3_matches_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch();
    encode_and_compare(&dir, "stereo.mkv", "matroska", "2", &[]);
    encode_and_compare(&dir, "surround.mkv", "matroska", "6", &[]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mp4_eac3_matches_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch();
    encode_and_compare(&dir, "stereo.mp4", "mp4", "2", &[]);
    encode_and_compare(&dir, "surround.mp4", "mp4", "6", &[]);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn mpegts_eac3_matches_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch();
    // FFmpeg's default for E-AC-3 in a transport stream is stream type 0x87 (ATSC) ...
    encode_and_compare(&dir, "stereo.ts", "mpegts", "2", &[]);
    encode_and_compare(&dir, "surround.ts", "mpegts", "6", &[]);
    // ... and the DVB form, private data with the enhanced AC-3 descriptor, with system B
    encode_and_compare(&dir, "surround_dvb.ts", "mpegts", "6", &["-mpegts_flags", "+system_b"]);
    let _ = std::fs::remove_dir_all(&dir);
}

/// What the writer does with a raw E-AC-3 file: its packets go into Matroska as `A_EAC3`, and read
/// back to the same samples.
#[test]
fn raw_eac3_copied_to_matroska_matches_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch();
    let raw = dir.join("copy_source.eac3");
    assert!(ffmpeg(&["-f", "lavfi", "-i", SIGNAL, "-ac", "2", "-c:a", "eac3", "-b:a", "192k", "-f", "eac3", raw.to_str().unwrap()]), "encoding the source");
    let want = reference(&dir, "copy_source", &raw);
    let mut input = open_demuxer(&raw).expect("the raw file opens");
    let streams = input.streams().to_vec();
    let out = dir.join("copied.mkv");
    let mut muxer = ironav_formats::create_muxer(&out, "mkv").expect("a Matroska writer");
    muxer.write_header(&streams).expect("the header");
    while let Some(p) = input.read_packet().expect("a packet") {
        muxer.write_packet(&p).expect("the packet");
    }
    muxer.finish().expect("the trailer");
    let (got, _, _) = demux_and_decode(&out);
    check("copied.mkv", &got, &want);
    // FFmpeg reads the file as E-AC-3 too
    let probe = Command::new("ffprobe")
        .args(["-v", "error", "-show_entries", "stream=codec_name,channels", "-of", "default=nw=1"])
        .arg(&out)
        .output()
        .expect("ffprobe runs");
    let text = String::from_utf8_lossy(&probe.stdout).to_string();
    assert!(text.contains("codec_name=eac3") && text.contains("channels=2"), "{text}");
    let _ = std::fs::remove_dir_all(&dir);
}

/// The raw reader gives an independent frame and the dependent frames after it as one packet, and
/// the decoder takes the independent frame's samples from it.
#[test]
fn dependent_frames_ride_with_their_independent_frame() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch();
    let raw = dir.join("plain.eac3");
    assert!(ffmpeg(&["-f", "lavfi", "-i", SIGNAL, "-ac", "6", "-c:a", "eac3", "-b:a", "384k", "-f", "eac3", raw.to_str().unwrap()]), "encoding the source");
    let want = reference(&dir, "plain", &raw);
    let data = std::fs::read(&raw).unwrap();

    // split into frames, and put a dependent frame (strmtyp 1, 40 bytes) behind every other one
    let mut frames = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let info = ironav_codec::ac3::eac3_frame_info(&data[pos..]).unwrap();
        frames.push(&data[pos..pos + info.frame_size]);
        pos += info.frame_size;
    }
    let mut bits = Vec::<bool>::new();
    for (v, n) in [(0x0B77u32, 16), (1, 2), (0, 3), (19, 11), (0, 2), (3, 2), (2, 3), (0, 1), (16, 5)] {
        for i in (0..n).rev() {
            bits.push(v >> i & 1 != 0);
        }
    }
    bits.resize(40 * 8, false);
    let dep: Vec<u8> = (bits.chunks(8).map(|c| c.iter().enumerate().fold(0u8, |b, (i, &x)| b | (x as u8) << (7 - i)))).collect();
    let mut mixed = Vec::new();
    for (i, f) in frames.iter().enumerate() {
        mixed.extend_from_slice(f);
        if i % 2 == 0 {
            mixed.extend_from_slice(&dep);
        }
    }
    let file = dir.join("with_dependent.eac3");
    std::fs::write(&file, &mixed).unwrap();

    let mut d = open_demuxer(&file).unwrap();
    let mut packets = 0;
    let mut sizes = Vec::new();
    while let Some(p) = d.read_packet().unwrap() {
        sizes.push(p.data.len());
        packets += 1;
    }
    assert_eq!(packets, frames.len(), "one packet per independent frame");
    for (i, (&n, f)) in sizes.iter().zip(&frames).enumerate() {
        assert_eq!(n, f.len() + if i % 2 == 0 { dep.len() } else { 0 }, "packet {i}");
    }
    let (got, _, ch) = demux_and_decode(&file);
    assert_eq!(ch, 6);
    check("with_dependent", &got, &want);
    let _ = std::fs::remove_dir_all(&dir);
}
