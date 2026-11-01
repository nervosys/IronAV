//! Multichannel Opus (RFC 7845 channel-mapping families 1 and 255) against FFmpeg, which has
//! libopus. Two directions: FFmpeg writes 5.1, 7.1, quad and family 255 streams into Ogg,
//! Matroska, WebM and MP4, and IronAV decodes them; IronAV encodes 3 to 8 channels into Ogg,
//! Matroska and WebM, and FFmpeg decodes them with each tone in its own channel. The FFmpeg tests
//! skip (and say so) when FFmpeg is not on PATH.

use ironav_core::{CodecId, Packet, Rational, StreamInfo, StreamParams};
use ironav_engine::{run, Job};
use std::path::{Path, PathBuf};
use std::process::Command;

const RATE: u32 = 48_000;

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success())
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ironav-opusmc-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn job(input: &Path, output: &Path) -> Job {
    Job { input: input.display().to_string(), output: output.display().to_string(), hwaccel: "auto".into(), ..Job::default() }
}

/// The tone on channel `c`: 300 + 170 c Hz at a quarter of full scale, so each channel is its own.
fn tone(c: usize, i: usize) -> f64 {
    0.25 * (2.0 * std::f64::consts::PI * (300.0 + 170.0 * c as f64) * i as f64 / RATE as f64).sin()
}

/// The tones, `seconds` long, `channels` wide, as 16-bit samples (interleaved).
fn tone_samples(channels: usize, seconds: f64) -> Vec<i16> {
    let n = (RATE as f64 * seconds) as usize;
    (0..n).flat_map(|i| (0..channels).map(move |c| ((tone(c, i) * 32767.0).round()) as i16)).collect()
}

/// Writes the tones as a 16-bit PCM WAV file and returns the samples written.
fn tones_wav(path: &Path, channels: usize, seconds: f64) -> Vec<i16> {
    let samples = tone_samples(channels, seconds);
    let n = samples.len() / channels;
    let pcm = StreamInfo {
        index: 0,
        codec: CodecId::PcmS16Le,
        time_base: Rational::new(1, RATE as i64),
        duration: None,
        bits: Some(16),
        extradata: Vec::new(),
        lead_in: 0,
        params: StreamParams::Audio { sample_rate: RATE, channels: channels as u16 },
    };
    let mut wav = ironav_formats::create_muxer(path, "wav").unwrap();
    wav.write_header(std::slice::from_ref(&pcm)).unwrap();
    let data = samples.iter().flat_map(|v| v.to_le_bytes()).collect();
    wav.write_packet(&Packet { stream: 0, pts: 0, duration: n as i64, data }).unwrap();
    wav.finish().unwrap();
    samples
}

/// The 16-bit samples and channel count of a WAV file, from its `fmt ` and `data` chunks.
fn wav_s16(bytes: &[u8]) -> (usize, Vec<i16>) {
    let mut at = 12;
    let (mut channels, mut data) = (0, &[][..]);
    while at + 8 <= bytes.len() {
        let len = u32::from_le_bytes(bytes[at + 4..at + 8].try_into().unwrap()) as usize;
        let body = &bytes[at + 8..(at + 8 + len).min(bytes.len())];
        match &bytes[at..at + 4] {
            b"fmt " => channels = u16::from_le_bytes([body[2], body[3]]) as usize,
            b"data" => data = body,
            _ => {}
        }
        at += 8 + len + (len & 1);
    }
    assert!(channels > 0 && !data.is_empty(), "no fmt or data chunk");
    (channels, data.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect())
}

/// The samples of channel `c` of interleaved `s` with `channels` channels.
fn channel<T: Copy>(s: &[T], channels: usize, c: usize) -> Vec<T> {
    s.iter().skip(c).step_by(channels).copied().collect()
}

/// The signal-to-noise ratio in dB of `got` against `want`, over the samples both have.
fn snr_db(want: &[f64], got: &[f64]) -> f64 {
    let (mut sig, mut err) = (0.0, 0.0);
    for (x, y) in want.iter().zip(got) {
        sig += x * x;
        err += (x - y) * (x - y);
    }
    10.0 * (sig / err.max(1e-12)).log10()
}

fn to_f64(s: &[i16]) -> Vec<f64> {
    s.iter().map(|&v| v as f64).collect()
}

/// Runs FFmpeg with `args` (after a quiet `-hide_banner -loglevel error -y`) and returns its stdout.
fn ffmpeg(args: &[&str]) -> Vec<u8> {
    let out = Command::new("ffmpeg").args(["-hide_banner", "-loglevel", "error", "-y"]).args(args).output().unwrap();
    assert!(out.status.success(), "ffmpeg {args:?}: {}", String::from_utf8_lossy(&out.stderr));
    out.stdout
}

/// The channel count of the first audio stream of `path`, as ffprobe reads it.
fn ffprobe_channels(path: &Path) -> usize {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "a:0", "-show_entries", "stream=channels", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .unwrap();
    assert!(out.status.success(), "ffprobe {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).trim().parse().unwrap()
}

/// FFmpeg's libopus decode of `path`, as 16-bit samples (interleaved).
fn ffmpeg_libopus_decode(path: &Path) -> Vec<i16> {
    let p = path.to_str().unwrap();
    let raw = ffmpeg(&["-c:a", "libopus", "-i", p, "-f", "s16le", "-"]);
    raw.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect()
}

/// IronAV's decode of `path` (an Opus file in any container it reads), as 16-bit samples.
fn ironav_decode(path: &Path, dir: &Path) -> (usize, Vec<i16>) {
    let out = dir.join(format!("{}.decoded.wav", path.file_name().unwrap().to_str().unwrap()));
    let mut j = job(path, &out);
    j.audio.codec = Some("pcm_s16le".into());
    run(&j, &mut |_| {}).unwrap();
    wav_s16(&std::fs::read(&out).unwrap())
}

/// FFmpeg writes `channels` tones with libopus at mapping family `family` into `name`, and IronAV
/// decodes it.
///
/// FFmpeg is asked for CELT only (`lowdelay`): IronAV's CELT decode of a multichannel stream matches
/// libopus to a least significant bit, and this checks the channel layout and the mapping. (In
/// SILK and hybrid frames, IronAV's output runs about a sample ahead of libopus's, which costs a
/// tone about 15 dB: a separate matter from the mapping.)
///
/// Family 1 is compared with FFmpeg's libopus decode, channel by channel, which must agree to a
/// few LSB. Family 255 is compared with the tones themselves: FFmpeg's libopus wrapper reorders the
/// channels of any 3 to 8 channel stream from Vorbis order, which RFC 7845 does not ask of family
/// 255 (unassigned channels, in table order), so its channels are not compared one to one here.
fn ffmpeg_file_decodes(channels: usize, family: u8, name: &str) {
    let dir = scratch(name);
    let wav = dir.join("tones.wav");
    let source = tones_wav(&wav, channels, 1.5);
    let src = wav.to_str().unwrap();
    let enc = dir.join(format!("ff.{name}"));
    let mapping = family.to_string();
    let bitrate = format!("{}k", 48 * channels);
    // four channels of a WAV are read as 4.0, which libopus refuses at family 1: they are quad
    // (the same order of samples, relabelled)
    let quad = ["-ch_layout", "quad"];
    let layout: &[&str] = if channels == 4 { &quad } else { &[] };
    let mut args: Vec<&str> = layout.to_vec();
    args.extend(["-i", src, "-c:a", "libopus", "-application", "lowdelay", "-mapping_family", &mapping, "-b:a", &bitrate, enc.to_str().unwrap()]);
    ffmpeg(&args);
    assert_eq!(ffprobe_channels(&enc), channels, "FFmpeg's {name} file");
    let (got_ch, got) = ironav_decode(&enc, &dir);
    assert_eq!(got_ch, channels, "{name}: IronAV's output channels");
    let g = to_f64(&got);
    if family == 1 {
        let w = to_f64(&ffmpeg_libopus_decode(&enc));
        let n = w.len().min(g.len());
        assert!(n > RATE as usize, "{name}: {n} samples");
        for c in 0..channels {
            let (a, b) = (channel(&w[..n], channels, c), channel(&g[..n], channels, c));
            let snr = snr_db(&a, &b);
            let worst = a.iter().zip(&b).map(|(x, y)| (x - y).abs()).fold(0.0, f64::max);
            eprintln!("{name}, channel {c}: {snr:.1} dB against FFmpeg's libopus, worst {worst} LSB");
            assert!(snr > 50.0, "{name} channel {c}: {snr:.1} dB against FFmpeg's libopus decode");
            assert!(worst <= 8.0, "{name} channel {c}: {worst} LSB from FFmpeg's libopus decode");
        }
    } else {
        let s = to_f64(&source);
        let n = s.len().min(g.len()) / channels;
        assert!(n > RATE as usize, "{name}: {n} samples per channel");
        for c in 0..channels {
            let snr = snr_db(&channel(&s[..n * channels], channels, c), &channel(&g[..n * channels], channels, c));
            eprintln!("{name}, channel {c}: {snr:.1} dB against its tone");
            assert!(snr > 40.0, "{name} channel {c}: {snr:.1} dB against its tone");
        }
    }
}

#[test]
fn ffmpeg_surround_opus_decodes_in_ironav() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    for (channels, family) in [(6, 1), (8, 1), (4, 1), (6, 255)] {
        for ext in ["ogg", "mkv", "webm", "mp4"] {
            ffmpeg_file_decodes(channels, family, &format!("{channels}ch{family}.{ext}"));
        }
    }
}

/// IronAV encodes `channels` tones as Opus in `ext`, and FFmpeg decodes it: the channel count is
/// `channels`, and each tone is in its own channel.
fn ironav_encoded_decodes_in_ffmpeg(channels: usize, ext: &str) {
    let dir = scratch(&format!("enc{channels}.{ext}"));
    let wav = dir.join("tones.wav");
    let seconds = 2.0;
    let source = tones_wav(&wav, channels, seconds);
    let out = dir.join(format!("ours.{ext}"));
    let mut j = job(&wav, &out);
    j.audio.codec = Some("opus".into());
    run(&j, &mut |_| {}).unwrap();
    assert_eq!(ffprobe_channels(&out), channels, "{ext}: channels as ffprobe reads them");
    let got = ffmpeg_libopus_decode(&out);
    assert_eq!(got.len() % channels, 0);
    let (w, g) = (to_f64(&source), to_f64(&got));
    let n = (w.len().min(g.len())) / channels;
    // the source and the decode from the start: the pre-skip is out of both
    assert!(n as f64 > seconds * RATE as f64 * 0.99, "{channels} channels in {ext}: {n} samples per channel");
    for c in 0..channels {
        let snr = snr_db(&channel(&w[..n * channels], channels, c), &channel(&g[..n * channels], channels, c));
        eprintln!("IronAV {channels} channels in {ext}, channel {c}: {snr:.1} dB in FFmpeg's decode");
        assert!(snr > 15.0, "{channels} channels in {ext}, channel {c}: {snr:.1} dB");
    }
}

#[test]
fn ironav_surround_opus_decodes_in_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    // mono is the control: the same encoder, one stream
    for channels in [1, 3, 6, 8] {
        for ext in ["ogg", "mka", "webm"] {
            ironav_encoded_decodes_in_ffmpeg(channels, ext);
        }
    }
}

#[test]
fn ironav_surround_opus_decodes_in_ironav() {
    // our own decoder reads our own multichannel Ogg, Matroska and WebM, each tone in its channel
    let dir = scratch("ours-roundtrip");
    let wav = dir.join("tones.wav");
    let source = tones_wav(&wav, 6, 1.0);
    for ext in ["ogg", "mka", "webm"] {
        let out = dir.join(format!("ours.{ext}"));
        let mut j = job(&wav, &out);
        j.audio.codec = Some("opus".into());
        run(&j, &mut |_| {}).unwrap();
        let (channels, got) = ironav_decode(&out, &dir);
        assert_eq!(channels, 6, "{ext}");
        let (w, g) = (to_f64(&source), to_f64(&got));
        let n = w.len().min(g.len()) / 6;
        for c in 0..6 {
            let snr = snr_db(&channel(&w[..n * 6], 6, c), &channel(&g[..n * 6], 6, c));
            assert!(snr > 15.0, "{ext} channel {c}: {snr:.1} dB");
        }
    }
}
