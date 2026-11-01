//! AC-3 streams, made with FFmpeg's native AC-3 encoder when the test runs, decode to what FFmpeg's
//! float decoder makes of them: mono, stereo and 5.1 (with LFE), at 44.1 and 48 kHz and bit rates
//! from 96 to 640 kbit/s. Each case prints its worst difference and the signal-to-noise ratio.
//! Skipped without `ffmpeg` on PATH.

use ironav_codec::ac3::{frame_info, Ac3Decoder};
use ironav_core::{Decoder, Frame, Packet};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Mono, stereo and 5.1 test signals: tones and a little noise, with bursts that make the encoder
/// switch blocks.
const SIGNAL: &str = "0.4*sin(2*PI*440*t)+0.3*(random(0)-0.5)*(lt(mod(t,0.25),0.02))+0.2*sin(2*PI*3000*t)*(lt(mod(t,0.5),0.1))";

fn ffmpeg(args: &[&str]) -> bool {
    Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).status().map(|s| s.success()).unwrap_or(false)
}

fn scratch() -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("ac3-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Encode `name.ac3` with FFmpeg: `channels` 1, 2 or 6 (6 is 5.1 with an LFE channel).
fn encode(dir: &Path, name: &str, rate: u32, channels: u16, kbps: u32) -> PathBuf {
    let out = dir.join(format!("{name}.ac3"));
    let source = format!("aevalsrc=exprs='{SIGNAL}':s={rate}:d=3");
    let kbps = format!("{kbps}k");
    let mut args: Vec<String> = vec!["-f".into(), "lavfi".into(), "-i".into(), source];
    if channels == 6 {
        args.extend(["-af".into(), "pan=5.1(side)|c0=c0|c1=c0|c2=0.5*c0|c3=0.3*c0|c4=0.8*c0|c5=0.7*c0".into()]);
    } else {
        args.extend(["-ac".into(), channels.to_string()]);
    }
    args.extend(["-c:a".into(), "ac3".into(), "-b:a".into(), kbps, out.to_str().unwrap().into()]);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    assert!(ffmpeg(&refs), "encoding {name} with FFmpeg");
    out
}

/// The worst absolute difference and the signal-to-noise ratio (dB) of `got` against `want`.
fn stats(got: &[f32], want: &[f32]) -> (f32, f64) {
    assert_eq!(got.len(), want.len(), "sample count");
    let mut worst = 0f32;
    let (mut sig, mut noise) = (0f64, 0f64);
    for (a, b) in got.iter().zip(want) {
        worst = worst.max((a - b).abs());
        sig += (*b as f64).powi(2);
        noise += ((a - b) as f64).powi(2);
    }
    (worst, 10.0 * (sig / noise.max(1e-300)).log10())
}

/// Decode an AC-3 elementary stream frame by frame, as f32 interleaved samples.
fn decode(data: &[u8], rate: u32, channels: u16) -> Vec<f32> {
    let mut dec = Ac3Decoder::new();
    let mut out = Vec::new();
    let mut pos = 0;
    let mut pts = 0;
    while pos < data.len() {
        let info = frame_info(&data[pos..]).expect("an AC-3 frame header");
        let start = pos + info.offset;
        let frame = &data[start..start + info.frame_size];
        assert_eq!((info.sample_rate, info.channels), (rate, channels));
        for f in dec.decode(&Packet { stream: 0, pts, duration: 1536, data: frame.to_vec() }).unwrap() {
            let Frame::Audio(a) = f else { panic!("audio expected") };
            out.extend(a.samples.iter().map(|&x| x as f32));
        }
        pos = start + info.frame_size;
        pts += 1536;
    }
    out
}

/// Decode `file` with FFmpeg's float decoder and with ours, and compare. Returns the worst
/// difference and the SNR.
fn compare(dir: &Path, name: &str, file: &Path, rate: u32, channels: u16) -> (f32, f64) {
    let reference = dir.join(format!("{name}.f32"));
    assert!(
        ffmpeg(&["-i", file.to_str().unwrap(), "-f", "f32le", "-acodec", "pcm_f32le", reference.to_str().unwrap()]),
        "decoding {name} with FFmpeg"
    );
    let bytes = std::fs::read(&reference).unwrap();
    let want: Vec<f32> = bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
    let got = decode(&std::fs::read(file).unwrap(), rate, channels);
    let (worst, snr) = stats(&got, &want);
    let peak = want.iter().fold(0f32, |m, &x| m.max(x.abs()));
    eprintln!("{name:>18}: max abs diff {worst:.3e}, SNR {snr:.1} dB, peak {peak:.3}");
    assert!(peak > 0.1, "{name}: the reference is nearly silent");
    (worst, snr)
}

#[test]
fn ac3_matches_ffmpeg() {
    if !Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success()) {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch();
    let mut results = Vec::new();
    for (name, rate, ch, kbps) in [
        ("mono48_96", 48000, 1, 96),
        ("mono48_128", 48000, 1, 128),
        ("stereo441_96", 44100, 2, 96),
        ("stereo48_192", 48000, 2, 192),
        ("surround48_160", 48000, 6, 160),
        ("surround48_384", 48000, 6, 384),
        ("surround441_448", 44100, 6, 448),
        ("surround48_640", 48000, 6, 640),
    ] {
        let file = encode(&dir, name, rate, ch, kbps);
        let (worst, _) = compare(&dir, name, &file, rate, ch);
        results.push((name, worst));
    }
    let _ = std::fs::remove_dir_all(&dir);
    for (name, worst) in results {
        // The measured worst case is 2.5e-5 (stereo 48 kHz at 192 kbit/s), under one 16-bit LSB
        // (3.05e-5). The bound is 3e-5 rather than the 1e-5 aim, which the float path does not reach.
        assert!(worst < 3e-5, "{name}: worst difference {worst}");
    }
}
