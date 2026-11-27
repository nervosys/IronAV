//! TrueHD and MLP streams, made with FFmpeg's encoders when the test runs, decode to exactly what
//! FFmpeg's decoder makes of them, sample for sample (both are lossless): mono, stereo, 2.1, 5.0, 5.1
//! and others, at 44.1, 48 and 96 kHz, from 16- and 24-bit input. Each case also says whether the
//! decoded samples equal the input that was encoded. Skipped without `ffmpeg` on PATH.
//!
//! FFmpeg's TrueHD encoder does not do 7.1 (its layouts end at 5.1), and neither encoder writes
//! the noise-shaped matrices of Dolby's encoders or a third substream, so those paths are checked by
//! the unit tests only.

use ironav_codec::mlp::{access_unit_length, MlpDecoder};
use ironav_core::{Decoder, Frame, Packet};
use std::path::{Path, PathBuf};
use std::process::Command;

fn ffmpeg(args: &[&str]) -> bool {
    Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).status().map(|s| s.success()).unwrap_or(false)
}

fn have_ffmpeg() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success())
}

fn scratch(tag: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("truehd-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// One channel's expression: a tone per channel, a noise burst, a quiet stretch and a click.
fn expr(k: usize) -> String {
    let f = 220.0 * (k as f64 + 1.0);
    format!(
        "0.4*sin(2*PI*{f}*t)+0.25*(random({k})-0.5)*lt(mod(t,0.5),0.08)+0.15*sin(2*PI*{g}*t)*gt(mod(t,1),0.5)+0.5*lt(abs(t-0.3),0.0004)",
        g = 3100.0 + 700.0 * k as f64
    )
}

/// Raw PCM input: `bytes` per sample (2 or 3), little-endian, `layout` channels.
fn make_input(dir: &Path, name: &str, rate: u32, layout: &str, channels: usize, bytes: usize) -> PathBuf {
    let out = dir.join(format!("{name}.pcm"));
    let exprs: Vec<String> = (0..channels).map(expr).collect();
    let source = format!("aevalsrc=exprs='{}':s={rate}:d=2:c={layout}", exprs.join("|"));
    let fmt = if bytes == 2 { "s16le" } else { "s24le" };
    assert!(ffmpeg(&["-f", "lavfi", "-i", &source, "-f", fmt, "-acodec", &format!("pcm_{fmt}"), out.to_str().unwrap()]), "making the input of {name}");
    out
}

/// Encode the raw PCM with `codec` ("truehd" or "mlp") into an elementary stream file.
fn encode(dir: &Path, name: &str, codec: &str, pcm: &Path, rate: u32, layout: &str, bytes: usize) -> PathBuf {
    let out = dir.join(format!("{name}.{}", if codec == "truehd" { "thd" } else { "mlp" }));
    let (fmt, rate) = (if bytes == 2 { "s16le" } else { "s24le" }, rate.to_string());
    assert!(
        ffmpeg(&["-f", fmt, "-ar", &rate, "-ch_layout", layout, "-i", pcm.to_str().unwrap(), "-c:a", codec, "-strict", "-2", "-f", codec, out.to_str().unwrap()]),
        "encoding {name} with FFmpeg"
    );
    out
}

/// What FFmpeg decodes `file` to: little-endian 32-bit (24 bits, shifted up 8) or 16-bit integers.
fn ffmpeg_decode(dir: &Path, name: &str, file: &Path, s16: bool) -> Vec<i32> {
    let out = dir.join(format!("{name}.raw"));
    let (fmt, codec) = if s16 { ("s16le", "pcm_s16le") } else { ("s32le", "pcm_s32le") };
    assert!(ffmpeg(&["-i", file.to_str().unwrap(), "-f", fmt, "-acodec", codec, out.to_str().unwrap()]), "decoding {name} with FFmpeg");
    let bytes = std::fs::read(&out).unwrap();
    if s16 {
        bytes.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b) as i32).collect()
    } else {
        bytes.as_chunks::<4>().0.iter().map(|b| i32::from_le_bytes(*b)).collect()
    }
}

/// The access units of an elementary stream.
fn access_units(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let n = access_unit_length(&data[pos..]).expect("an access unit header");
        assert!(pos + n <= data.len(), "an access unit is cut short");
        out.push(&data[pos..pos + n]);
        pos += n;
    }
    out
}

/// Decode with IronAV: the integers (32-bit scale, or 16-bit when `s16`), the channels and the rate.
fn ironav_decode(data: &[u8], truehd: bool, s16: bool) -> (Vec<i32>, u16, u32) {
    let mut dec = MlpDecoder::new(truehd);
    let mut out = Vec::new();
    let (mut channels, mut rate) = (0, 0);
    let mut pts = 0;
    for au in access_units(data) {
        for f in dec.decode(&Packet { stream: 0, pts, duration: 0, data: au.to_vec() }).expect("decoding an access unit") {
            let Frame::Audio(a) = f else { panic!("audio expected") };
            channels = a.channels;
            rate = a.sample_rate;
            pts += a.frames() as i64;
            let scale = if s16 { 32768.0 } else { 2147483648.0 };
            out.extend(a.samples.iter().map(|&x| (x * scale) as i32));
        }
    }
    assert_eq!(dec.check_failures(), 0, "checks FFmpeg also makes");
    (out, channels, rate)
}

struct Case {
    name: &'static str,
    codec: &'static str,
    rate: u32,
    layout: &'static str,
    channels: u16,
    /// Bytes per sample of the input: 2 (16-bit) or 3 (24-bit).
    bytes: usize,
}

const fn case(name: &'static str, codec: &'static str, rate: u32, layout: &'static str, channels: u16, bytes: usize) -> Case {
    Case { name, codec, rate, layout, channels, bytes }
}

#[test]
fn truehd_and_mlp_match_ffmpeg_bit_for_bit() {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch("cmp");
    let cases = [
        case("thd_mono48_24", "truehd", 48000, "mono", 1, 3),
        case("thd_mono48_16", "truehd", 48000, "mono", 1, 2),
        case("thd_stereo48_24", "truehd", 48000, "stereo", 2, 3),
        case("thd_stereo48_16", "truehd", 48000, "stereo", 2, 2),
        case("thd_stereo441_24", "truehd", 44100, "stereo", 2, 3),
        case("thd_stereo96_24", "truehd", 96000, "stereo", 2, 3),
        case("thd_2.1_48_24", "truehd", 48000, "2.1", 3, 3),
        case("thd_4.0_48_24", "truehd", 48000, "4.0", 4, 3),
        case("thd_5.0_48_24", "truehd", 48000, "5.0(side)", 5, 3),
        case("thd_5.1_48_24", "truehd", 48000, "5.1(side)", 6, 3),
        case("thd_5.1_48_16", "truehd", 48000, "5.1(side)", 6, 2),
        case("thd_5.1_96_24", "truehd", 96000, "5.1(side)", 6, 3),
        case("mlp_mono48_16", "mlp", 48000, "mono", 1, 2),
        case("mlp_stereo48_16", "mlp", 48000, "stereo", 2, 2),
        case("mlp_stereo48_24", "mlp", 48000, "stereo", 2, 3),
        case("mlp_stereo441_24", "mlp", 44100, "stereo", 2, 3),
        case("mlp_stereo96_24", "mlp", 96000, "stereo", 2, 3),
        case("mlp_quad48_24", "mlp", 48000, "quad", 4, 3),
        case("mlp_5.0_48_24", "mlp", 48000, "5.0", 5, 3),
        case("mlp_5.1_48_24", "mlp", 48000, "5.1", 6, 3),
        case("mlp_5.1_48_16", "mlp", 48000, "5.1", 6, 2),
        case("mlp_5.1_96_24", "mlp", 96000, "5.1", 6, 3),
    ];
    let mut failures = Vec::new();
    for c in &cases {
        let pcm = make_input(&dir, c.name, c.rate, c.layout, c.channels as usize, c.bytes);
        let file = encode(&dir, c.name, c.codec, &pcm, c.rate, c.layout, c.bytes);
        let stream = std::fs::read(&file).unwrap();
        // the stream's own sample size: MLP keeps 16-bit input at 16 bits, TrueHD is always 24
        let s16 = c.codec == "mlp" && c.bytes == 2;
        let want = ffmpeg_decode(&dir, c.name, &file, s16);
        let (got, channels, rate) = ironav_decode(&stream, c.codec == "truehd", s16);
        let same = got == want;
        // the input, in the scale of the decoded samples
        let input = std::fs::read(&pcm).unwrap();
        let orig: Vec<i32> = if c.bytes == 2 {
            input.as_chunks::<2>().0.iter().map(|b| (i16::from_le_bytes(*b) as i32) << if s16 { 0 } else { 16 }).collect()
        } else {
            input.as_chunks::<3>().0.iter().map(|b| i32::from_le_bytes([0, b[0], b[1], b[2]])).collect()
        };
        let lossless = got.len() >= orig.len() && got[..orig.len()] == orig[..];
        let ffmpeg_lossless = want.len() >= orig.len() && want[..orig.len()] == orig[..];
        let first_diff = got.iter().zip(&want).position(|(a, b)| a != b);
        eprintln!(
            "{:>18}: {} samples, {channels} ch, {rate} Hz, IronAV == FFmpeg: {same}, equals the input: {lossless} (FFmpeg: {ffmpeg_lossless}){}",
            c.name,
            got.len(),
            first_diff.map(|i| format!(", first difference at {i}")).unwrap_or_default()
        );
        if !same || channels != c.channels || rate != c.rate || (ffmpeg_lossless && !lossless) {
            failures.push(c.name);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    assert!(failures.is_empty(), "cases that differ from FFmpeg: {failures:?}");
}
