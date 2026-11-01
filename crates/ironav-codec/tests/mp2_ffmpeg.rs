//! MPEG-1/2 Audio Layer II streams, made with FFmpeg's native MP2 encoder when the test runs, decode
//! to what FFmpeg's float decoder makes of them: 32, 44.1 and 48 kHz (MPEG-1) and 16, 22.05 and 24 kHz
//! (MPEG-2 LSF), mono and stereo, at bit rates that select each of the four MPEG-1 allocation tables,
//! and a full-scale noise stream. Skipped without `ffmpeg` on PATH.
//!
//! Not covered: joint stereo. FFmpeg's encoder writes none, and streams whose headers are patched to
//! say joint stereo (or whose codes are scrambled) do not decode to FFmpeg's samples, so the
//! intensity-stereo path has no reference here.

use ironav_codec::mp3::{parse_frame, Mp2Decoder, Mp3Decoder};
use ironav_core::{Decoder, Frame, Packet, StreamParams};
use std::path::{Path, PathBuf};
use std::process::Command;

const SOURCE: &str = "aevalsrc=0.5*sin(2*PI*330*t)+0.1*sin(2*PI*2100*t)*sin(2*PI*3*t)|0.4*sin(2*PI*495*t+0.4)+0.2*random(0)-0.1:s=44100:d=2";

fn ffmpeg(args: &[&str]) -> bool {
    Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).status().map(|s| s.success()).unwrap_or(false)
}

fn scratch() -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("mp2-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Decode a Layer II elementary stream frame by frame, as f32 interleaved samples. The decoder is
/// `Mp2Decoder`, or the Layer III one, which decodes Layer II frames too (MP4 and TS label them MP3).
fn decode(mut d: Box<dyn Decoder>, data: &[u8], rate: u32, channels: u16) -> Vec<f32> {
    let mut out = Vec::new();
    let mut pos = 0;
    let mut pts = 0;
    while pos + 4 <= data.len() {
        let h = parse_frame(&data[pos..]).expect("a Layer II frame header");
        assert_eq!(h.layer, 2);
        let frame = &data[pos..pos + h.frame_len];
        for f in d.decode(&Packet { stream: 0, pts, duration: 1152, data: frame.to_vec() }).unwrap() {
            let Frame::Audio(a) = f else { panic!("audio expected") };
            assert_eq!((a.sample_rate, a.channels), (rate, channels));
            out.extend(a.samples.iter().map(|&x| x as f32));
        }
        pos += h.frame_len;
        pts += 1152;
    }
    out
}

/// Encode `name.mp2` with FFmpeg from `source`.
fn encode(dir: &Path, name: &str, source: &str, rate: u32, channels: u16, bitrate: &str) -> PathBuf {
    let mp2 = dir.join(format!("{name}.mp2"));
    let rate_s = rate.to_string();
    let ch_s = channels.to_string();
    assert!(
        ffmpeg(&["-f", "lavfi", "-i", source, "-ac", &ch_s, "-ar", &rate_s, "-b:a", bitrate, "-c:a", "mp2", mp2.to_str().unwrap()]),
        "encoding {name}"
    );
    mp2
}

/// Decode `mp2` with FFmpeg's float decoder and with ours, and compare.
fn compare(dir: &Path, name: &str, mp2: &Path, rate: u32, channels: u16) {
    let reference = dir.join(format!("{name}.f32"));
    assert!(
        ffmpeg(&["-c:a", "mp2float", "-i", mp2.to_str().unwrap(), "-f", "f32le", "-acodec", "pcm_f32le", reference.to_str().unwrap()]),
        "decoding {name} with FFmpeg"
    );
    let data = std::fs::read(mp2).unwrap();
    let bytes = std::fs::read(&reference).unwrap();
    let want: Vec<f32> = bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
    let params = StreamParams::Audio { sample_rate: rate, channels };
    let got = decode(Box::new(Mp2Decoder::new(&params).unwrap()), &data, rate, channels);
    let as_layer_iii = decode(Box::new(Mp3Decoder::new(&params).unwrap()), &data, rate, channels);
    assert!(got == as_layer_iii, "{name}: the Layer III decoder does not decode Layer II frames as the Layer II decoder does");
    assert_eq!(got.len(), want.len(), "{name}: sample count");
    let mut worst = 0f32;
    let mut at = 0;
    for (i, (a, b)) in got.iter().zip(&want).enumerate() {
        let e = (a - b).abs();
        if e > worst {
            worst = e;
            at = i;
        }
    }
    let peak = want.iter().fold(0f32, |m, &x| m.max(x.abs()));
    assert!(peak > 0.1, "{name}: the reference is nearly silent");
    // float32 rounding: the worst difference measured is about 1e-6
    assert!(worst < 1e-4, "{name}: worst difference {worst} at sample {at} (got {}, FFmpeg {})", got[at], want[at]);
}

#[test]
fn layer_ii_matches_ffmpeg() {
    if !Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success()) {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch();
    // MPEG-1 stereo at each allocation table's bit rates (table 0 at 48 kHz 256k, 1 at 44.1k 192k,
    // 2 at 44.1k 64k, 3 at 32 kHz 96k), then mono, then MPEG-2 LSF
    for (name, rate, ch, br) in [
        ("s44_192", 44100, 2, "192k"),
        ("s32_128", 32000, 2, "128k"),
        ("s32_96", 32000, 2, "96k"),
        ("s48_256", 48000, 2, "256k"),
        ("s48_96", 48000, 2, "96k"),
        ("s44_64", 44100, 2, "64k"),
        ("m48_64", 48000, 1, "64k"),
        ("m44_128", 44100, 1, "128k"),
        ("l22_64", 22050, 2, "64k"),
        ("l16_32", 16000, 1, "32k"),
        ("l24_96", 24000, 2, "96k"),
    ] {
        let mp2 = encode(&dir, name, SOURCE, rate, ch, br);
        compare(&dir, name, &mp2, rate, ch);
    }
    // full-scale noise: the largest subband values a valid stream carries
    let mp2 = encode(&dir, "loud44_224", "anoisesrc=d=2:c=pink:a=1:r=44100", 44100, 2, "224k");
    compare(&dir, "loud44_224", &mp2, 44100, 2);
    let _ = std::fs::remove_dir_all(&dir);
}
