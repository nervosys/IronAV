//! Opus in MPEG-TS, against FFmpeg. Two directions: IronAV writes mono, stereo and 6-channel Opus
//! into a `.ts` file, and FFmpeg decodes it to the PCM IronAV decodes from the Ogg file the packets
//! came from. FFmpeg's decoder does not apply the pre-skip that a transport stream's control headers
//! carry, so its decode keeps those samples at the front: the check takes them off. FFmpeg also
//! writes Opus into a `.ts` (`-f mpegts`), and IronAV reads it with the packets of the same audio in
//! an Ogg and a Matroska copy FFmpeg made. The FFmpeg tests skip (and say so) when FFmpeg is not on
//! PATH.

use ironav_core::{CodecId, Packet, Rational, StreamInfo, StreamParams};
use ironav_engine::{run, Job};
use std::path::{Path, PathBuf};
use std::process::Command;

const RATE: u32 = 48_000;

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success())
}

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ironav-opusts-{}-{name}", std::process::id()));
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

/// Writes the tones, `seconds` long and `channels` wide, as a 16-bit PCM WAV file.
fn tones_wav(path: &Path, channels: usize, seconds: f64) -> Vec<i16> {
    let n = (RATE as f64 * seconds) as usize;
    let samples: Vec<i16> = (0..n).flat_map(|i| (0..channels).map(move |c| ((tone(c, i) * 32767.0).round()) as i16)).collect();
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

/// The codec and channel count of the first audio stream of `path`, as ffprobe reads them.
fn ffprobe_codec_channels(path: &Path) -> String {
    let out = Command::new("ffprobe")
        .args(["-v", "error", "-select_streams", "a:0", "-show_entries", "stream=codec_name,channels", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .unwrap();
    assert!(out.status.success(), "ffprobe {}", String::from_utf8_lossy(&out.stderr));
    String::from_utf8_lossy(&out.stdout).lines().next().unwrap_or_default().trim().replace(',', " ")
}

/// IronAV's decode of `path` (an Opus file in any container it reads), as 16-bit samples.
fn ironav_decode(path: &Path, dir: &Path) -> (usize, Vec<i16>) {
    let out = dir.join(format!("{}.decoded.wav", path.file_name().unwrap().to_str().unwrap()));
    let mut j = job(path, &out);
    j.audio.codec = Some("pcm_s16le".into());
    run(&j, &mut |_| {}).unwrap();
    wav_s16(&std::fs::read(&out).unwrap())
}

/// FFmpeg's libopus decode of `path`, as 16-bit samples (interleaved).
fn ffmpeg_libopus_decode(path: &Path) -> Vec<i16> {
    let raw = ffmpeg(&["-c:a", "libopus", "-i", path.to_str().unwrap(), "-f", "s16le", "-"]);
    raw.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect()
}

/// The packets of the first audio stream of `path`, as (pts, duration, data).
fn packets(path: &Path) -> (StreamInfo, Vec<(i64, i64, Vec<u8>)>) {
    let mut d = ironav_formats::open_demuxer(path).unwrap();
    let info = d.streams().iter().find(|s| s.codec == CodecId::Opus).unwrap().clone();
    let mut out = Vec::new();
    while let Some(p) = d.read_packet().unwrap() {
        if p.stream == info.index {
            out.push((p.pts, p.duration, p.data));
        }
    }
    (info, out)
}

/// IronAV writes the encoder's Opus, from a WAV file of the tones, into an Ogg file and then copies
/// it into a `.ts`. Both decode in IronAV to the same samples; FFmpeg decodes the `.ts` to those
/// samples after the pre-skip. (Mono, stereo and 6 channels; 6 is family 1.)
fn ironav_opus_in_ts_decodes_in_ffmpeg(channels: usize) {
    let dir = scratch(&format!("enc{channels}"));
    let wav = dir.join("tones.wav");
    tones_wav(&wav, channels, 1.5);
    let ogg = dir.join("source.opus");
    let mut j = job(&wav, &ogg);
    j.audio.codec = Some("opus".into());
    run(&j, &mut |_| {}).unwrap();
    let ts = dir.join("out.ts");
    let mut j = job(&ogg, &ts);
    j.audio.codec = Some("opus".into());
    run(&j, &mut |_| {}).unwrap();
    assert_eq!(ffprobe_codec_channels(&ts), format!("opus {channels}"), "FFmpeg's view of the transport stream");
    // the source's pre-skip, from its OpusHead (bytes 10 and 11)
    let (info, _) = packets(&ogg);
    let pre = u16::from_le_bytes([info.extradata[10], info.extradata[11]]) as usize;
    let (ch_ogg, from_ogg) = ironav_decode(&ogg, &dir);
    let (ch_ts, from_ts) = ironav_decode(&ts, &dir);
    assert_eq!((ch_ogg, ch_ts), (channels, channels));
    assert!(from_ogg == from_ts, "{channels} channels: IronAV's decode of the transport stream differs from its decode of the Ogg file");
    let got = ffmpeg_libopus_decode(&ts);
    let n = from_ogg.len();
    assert!(got.len() >= pre * channels + n, "{channels} channels: FFmpeg decoded {} samples", got.len());
    let (w, g) = (to_f64(&from_ogg), to_f64(&got[pre * channels..pre * channels + n]));
    for c in 0..channels {
        let (a, b) = (channel(&w, channels, c), channel(&g, channels, c));
        let snr = snr_db(&a, &b);
        let worst = a.iter().zip(&b).map(|(x, y)| (x - y).abs()).fold(0.0, f64::max);
        eprintln!("{channels} channels in TS, channel {c}: {snr:.1} dB against IronAV's decode, worst {worst} LSB");
        assert!(snr > 50.0, "{channels} channels, channel {c}: {snr:.1} dB");
        assert!(worst <= 8.0, "{channels} channels, channel {c}: {worst} LSB");
    }
}

#[test]
fn ironav_opus_ts_mono_decodes_in_ffmpeg_like_the_source() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    ironav_opus_in_ts_decodes_in_ffmpeg(1);
}

#[test]
fn ironav_opus_ts_stereo_decodes_in_ffmpeg_like_the_source() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    ironav_opus_in_ts_decodes_in_ffmpeg(2);
}

#[test]
fn ironav_opus_ts_six_channels_decodes_in_ffmpeg_like_the_source() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    ironav_opus_in_ts_decodes_in_ffmpeg(6);
}

/// FFmpeg writes Opus into a `.ts` (its PES bundle several packets, each behind its control
/// header); IronAV reads the packets, which are those of an Ogg and a Matroska copy FFmpeg made.
#[test]
fn ffmpeg_opus_ts_reads_as_its_ogg_and_matroska_copies() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch("ffmpeg");
    let wav = dir.join("tones.wav");
    tones_wav(&wav, 2, 1.5);
    let ts = dir.join("ff.ts");
    ffmpeg(&["-i", wav.to_str().unwrap(), "-c:a", "libopus", "-f", "mpegts", ts.to_str().unwrap()]);
    let ogg = dir.join("copy.ogg");
    let mka = dir.join("copy.mka");
    ffmpeg(&["-i", ts.to_str().unwrap(), "-c:a", "copy", ogg.to_str().unwrap()]);
    ffmpeg(&["-i", ts.to_str().unwrap(), "-c:a", "copy", mka.to_str().unwrap()]);
    let (info, from_ts) = packets(&ts);
    assert!(matches!(info.params, StreamParams::Audio { sample_rate: RATE, channels: 2 }), "{:?}", info.params);
    assert!(from_ts.len() > 50, "{} packets", from_ts.len());
    // the trimmed packets add up to the 1.5 s of the tones once the pre-skip is taken off
    let pre = u16::from_le_bytes([info.extradata[10], info.extradata[11]]) as i64;
    let total: i64 = from_ts.iter().map(|p| p.1).sum();
    assert_eq!(total - pre, RATE as i64 * 3 / 2, "the transport stream's duration after the pre-skip");
    for copy in [&ogg, &mka] {
        let (_, from_copy) = packets(copy);
        assert_eq!(from_ts.len(), from_copy.len(), "packet count in {}", copy.display());
        for (i, (a, b)) in from_ts.iter().zip(&from_copy).enumerate() {
            assert_eq!(a.2, b.2, "packet {i} data in {}", copy.display());
        }
        // FFmpeg's transport stream demuxer does not apply the end trim, so its copies play the last
        // packet whole; every other duration is the same
        let (ds, dc): (Vec<i64>, Vec<i64>) = (from_ts.iter().map(|p| p.1).collect(), from_copy.iter().map(|p| p.1).collect());
        let n = ds.len() - 1;
        assert_eq!(ds[..n], dc[..n], "durations in {}", copy.display());
        assert!(ds[n] <= dc[n], "last duration in {}", copy.display());
    }
}
