//! WMA 1 and 2 streams, made with FFmpeg's native encoders when the test runs, decode through the ASF
//! reader and IronAV's decoder to what FFmpeg's float decoder makes of the same files: mono and stereo,
//! at 22.05, 44.1 and 48 kHz, at 32, 64, 128 and 192 kbit/s. Each case prints its signal-to-noise
//! ratio against FFmpeg's decode. Skipped without `ffmpeg` on PATH.
//!
//! FFmpeg's encoders write one frame per superframe (no bit reservoir) and fixed-size blocks or short
//! blocks as the signal asks, so the reservoir path is checked by the unit tests of the decoder.

use ironav_core::{Frame, Packet};
use std::path::{Path, PathBuf};
use std::process::Command;

const SIGNAL: &str = "0.4*sin(2*PI*440*t)+0.3*(random(0)-0.5)*(lt(mod(t,0.25),0.02))+0.2*sin(2*PI*3000*t)*(lt(mod(t,0.5),0.1))+0.1*sin(2*PI*9000*t)";

fn ffmpeg(args: &[&str]) -> bool {
    Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).status().map(|s| s.success()).unwrap_or(false)
}

fn have_ffmpeg() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success())
}

fn scratch(tag: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("wma-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Encode `name.wma` with FFmpeg's native encoder `codec`.
fn encode(dir: &Path, name: &str, codec: &str, rate: u32, channels: u32, kbps: u32) -> Option<PathBuf> {
    let out = dir.join(format!("{name}.wma"));
    let source = format!("aevalsrc=exprs='{SIGNAL}':s={rate}:d=2");
    let pan = if channels == 1 { "pan=mono|c0=c0" } else { "pan=stereo|c0=c0|c1=0.7*c0" };
    let kbps = format!("{kbps}k");
    let args = ["-f", "lavfi", "-i", &source, "-af", pan, "-c:a", codec, "-b:a", &kbps, out.to_str().unwrap()];
    ffmpeg(&args).then_some(out)
}

/// The signal-to-noise ratio (dB) of `got` against `want`.
fn snr(got: &[f32], want: &[f32]) -> f64 {
    let (mut sig, mut noise) = (0f64, 0f64);
    for (a, b) in got.iter().zip(want) {
        sig += (*b as f64).powi(2);
        noise += ((a - b) as f64).powi(2);
    }
    10.0 * (sig / noise.max(1e-300)).log10()
}

/// Decode every packet of `file` through the ASF reader and the WMA decoder, then flush: f32 interleaved.
fn decode(file: &Path) -> (Vec<f32>, u32, u16) {
    let mut demux = ironav_formats::open_demuxer(file).unwrap();
    let stream = demux.streams()[0].clone();
    let mut dec = ironav_codec::new_decoder_for(&stream).unwrap();
    let mut out = Vec::new();
    let (mut rate, mut channels) = (0, 0);
    let mut take = |frames: Vec<Frame>| {
        for f in frames {
            let Frame::Audio(a) = f else { panic!("audio expected") };
            (rate, channels) = (a.sample_rate, a.channels);
            out.extend(a.samples.iter().map(|&x| x as f32));
        }
    };
    while let Some(p) = demux.read_packet().unwrap() {
        let p: Packet = p;
        take(dec.decode(&p).unwrap());
    }
    take(dec.flush().unwrap());
    (out, rate, channels)
}

fn reference(dir: &Path, name: &str, file: &Path) -> Vec<f32> {
    let reference = dir.join(format!("{name}.f32"));
    assert!(ffmpeg(&["-i", file.to_str().unwrap(), "-f", "f32le", "-acodec", "pcm_f32le", reference.to_str().unwrap()]), "decoding {name} with FFmpeg");
    let bytes = std::fs::read(&reference).unwrap();
    bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect()
}

#[test]
fn wma_matches_ffmpeg() {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch("cmp");
    let mut worst = f64::MAX;
    let mut cases = 0;
    let mut failures = Vec::new();
    for codec in ["wmav1", "wmav2"] {
        for (rate, channels) in [(22050u32, 1u32), (22050, 2), (44100, 1), (44100, 2), (48000, 1), (48000, 2)] {
            for kbps in [32u32, 64, 128, 192] {
                let name = format!("{codec}_{rate}_{channels}ch_{kbps}k");
                let Some(file) = encode(&dir, &name, codec, rate, channels, kbps) else {
                    eprintln!("{name:>26}: FFmpeg's encoder refused the case");
                    continue;
                };
                let want = reference(&dir, &name, &file);
                let (got, got_rate, got_channels) = decode(&file);
                assert_eq!((got_rate, got_channels), (rate, channels as u16), "{name}: stream parameters");
                assert_eq!(got.len(), want.len(), "{name}: sample count");
                let s = snr(&got, &want);
                let peak = want.iter().fold(0f32, |m, &x| m.max(x.abs()));
                eprintln!("{name:>26}: SNR {s:.1} dB, peak {peak:.3}, {} samples", got.len());
                assert!(peak > 0.1, "{name}: the reference is nearly silent");
                if s <= 90.0 {
                    failures.push(format!("{name}: SNR {s:.1} dB"));
                }
                worst = worst.min(s);
                cases += 1;
            }
        }
    }
    eprintln!("{cases} cases, worst SNR {worst:.1} dB");
    assert!(failures.is_empty(), "below 90 dB against FFmpeg: {failures:?}");
    assert!(cases >= 40, "too few cases ran ({cases})");
    let _ = std::fs::remove_dir_all(&dir);
}
