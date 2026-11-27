//! DTS streams, made with FFmpeg's DCA encoder when the test runs, decode to what FFmpeg's float
//! decoder makes of them: mono, stereo, quad, 5.0 and 5.1 (with the LFE channel), at 44.1 and 48 kHz,
//! at a few bit rates, with and without ADPCM prediction. Each case prints its signal-to-noise ratio.
//! The same frames in the other three byte orders decode to the same samples, and a frame followed by
//! a DTS-HD extension substream decodes to its core. Skipped without `ffmpeg` on PATH.
//!
//! FFmpeg's encoder writes block codes and raw samples only (no Huffman codes, high-frequency vector
//! quantisation or joint intensity coding), so those paths are checked by the unit tests and by
//! the sample files of the formats tests.

use ironav_codec::dts::{frame_info, ByteOrder, DtsDecoder};
use ironav_core::{Decoder, Frame, Packet};
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
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("dts-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Encode `name.dts` with FFmpeg. `layout` is "mono", "stereo", "5.1" (the signal goes to the first
/// channel and, scaled, to the others) or any other FFmpeg channel layout (the signal goes to all).
fn encode(dir: &Path, name: &str, rate: u32, layout: &str, kbps: u32, extra: &[&str]) -> PathBuf {
    let out = dir.join(format!("{name}.dts"));
    let source = format!("aevalsrc=exprs='{SIGNAL}':s={rate}:d=3");
    let pan = match layout {
        "mono" => "pan=mono|c0=c0".to_string(),
        "stereo" => "pan=stereo|c0=c0|c1=0.7*c0".to_string(),
        "5.1(side)" => "pan=5.1(side)|c0=c0|c1=0.9*c0|c2=0.5*c0|c3=0.3*c0|c4=0.8*c0|c5=0.7*c0".to_string(),
        l => format!("aformat=channel_layouts={l}"),
    };
    let kbps = format!("{kbps}k");
    let mut args: Vec<&str> = vec!["-f", "lavfi", "-i", &source, "-af", &pan, "-c:a", "dca", "-strict", "-2", "-b:a", &kbps];
    args.extend_from_slice(extra);
    args.push(out.to_str().unwrap());
    assert!(ffmpeg(&args), "encoding {name} with FFmpeg");
    out
}

/// The signal-to-noise ratio (dB) of `got` against `want`.
fn snr(got: &[f32], want: &[f32]) -> f64 {
    assert_eq!(got.len(), want.len(), "sample count");
    let (mut sig, mut noise) = (0f64, 0f64);
    for (a, b) in got.iter().zip(want) {
        sig += (*b as f64).powi(2);
        noise += ((a - b) as f64).powi(2);
    }
    10.0 * (sig / noise.max(1e-300)).log10()
}

/// Splits a DTS elementary stream into its frames, as stored.
fn frames_of(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos < data.len() {
        let info = frame_info(&data[pos..]).expect("a DTS frame header");
        assert_eq!(info.offset, 0);
        out.push(&data[pos..pos + info.frame_size]);
        pos += info.frame_size;
    }
    out
}

/// Decode a DTS elementary stream frame by frame, as f32 interleaved samples.
fn decode(data: &[u8], rate: u32, channels: u16) -> Vec<f32> {
    let mut dec = DtsDecoder::new();
    let mut out = Vec::new();
    let mut pts = 0;
    for frame in frames_of(data) {
        let info = frame_info(frame).unwrap();
        assert_eq!((info.sample_rate, info.channels), (rate, channels));
        for f in dec.decode(&Packet { stream: 0, pts, duration: info.samples as i64, data: frame.to_vec() }).unwrap() {
            let Frame::Audio(a) = f else { panic!("audio expected") };
            assert_eq!((a.sample_rate, a.channels, a.pts), (rate, channels, pts));
            out.extend(a.samples.iter().map(|&x| x as f32));
        }
        pts += info.samples as i64;
    }
    out
}

/// Decode `file` with FFmpeg's float decoder and with ours, and compare: the SNR.
fn compare(dir: &Path, name: &str, file: &Path, rate: u32, channels: u16) -> f64 {
    let reference = dir.join(format!("{name}.f32"));
    assert!(ffmpeg(&["-i", file.to_str().unwrap(), "-f", "f32le", "-acodec", "pcm_f32le", reference.to_str().unwrap()]), "decoding {name} with FFmpeg");
    let bytes = std::fs::read(&reference).unwrap();
    let want: Vec<f32> = bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect();
    let got = decode(&std::fs::read(file).unwrap(), rate, channels);
    let s = snr(&got, &want);
    let peak = want.iter().fold(0f32, |m, &x| m.max(x.abs()));
    eprintln!("{name:>20}: SNR {s:.1} dB, peak {peak:.3}");
    assert!(peak > 0.1, "{name}: the reference is nearly silent");
    s
}

#[test]
fn dts_matches_ffmpeg() {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch("cmp");
    let mut results = Vec::new();
    // (name, rate, layout, channels, kbps, extra encoder options)
    let cases: &[(&str, u32, &str, u16, u32, &[&str])] = &[
        ("mono48_192", 48000, "mono", 1, 192, &[]),
        ("mono441_256", 44100, "mono", 1, 256, &[]),
        ("stereo48_384", 48000, "stereo", 2, 384, &[]),
        ("stereo48_768", 48000, "stereo", 2, 768, &[]),
        ("stereo441_448", 44100, "stereo", 2, 448, &[]),
        ("stereo48_adpcm", 48000, "stereo", 2, 384, &["-dca_adpcm", "1"]),
        ("quad48_768", 48000, "quad(side)", 4, 768, &[]),
        ("5.0_48_768", 48000, "5.0(side)", 5, 768, &[]),
        ("5.1_48_768", 48000, "5.1(side)", 6, 768, &[]),
        ("5.1_48_1536", 48000, "5.1(side)", 6, 1536, &[]),
        ("5.1_441_1024", 44100, "5.1(side)", 6, 1024, &[]),
        ("5.1_48_adpcm", 48000, "5.1(side)", 6, 1024, &["-dca_adpcm", "1"]),
        ("stereo32_384", 32000, "stereo", 2, 384, &[]),
        ("stereo24_384", 24000, "stereo", 2, 384, &[]),
    ];
    for &(name, rate, layout, ch, kbps, extra) in cases {
        let file = encode(&dir, name, rate, layout, kbps, extra);
        results.push((name, compare(&dir, name, &file, rate, ch)));
    }
    let _ = std::fs::remove_dir_all(&dir);
    for (name, s) in results {
        assert!(s > 90.0, "{name}: SNR {s} dB against FFmpeg");
    }
}

/// The stored form of a 16-bit big-endian frame in another byte order.
fn reorder(be: &[u8], order: ByteOrder) -> Vec<u8> {
    match order {
        ByteOrder::Be16 => be.to_vec(),
        ByteOrder::Le16 => be.chunks(2).flat_map(|c| [c[1], c[0]]).collect(),
        ByteOrder::Be14 | ByteOrder::Le14 => {
            let mut words = Vec::new();
            let (mut acc, mut n) = (0u64, 0);
            for &b in be {
                acc = (acc << 8) | b as u64;
                n += 8;
                while n >= 14 {
                    words.push(((acc >> (n - 14)) & 0x3FFF) as u16);
                    n -= 14;
                }
                acc &= (1 << n) - 1;
            }
            if n > 0 {
                words.push(((acc << (14 - n)) & 0x3FFF) as u16);
            }
            // each 14-bit word is sign-extended to 16 bits
            let ext = |w: &u16| ((*w << 2) as i16 >> 2) as u16;
            words.iter().flat_map(|w| if order == ByteOrder::Be14 { ext(w).to_be_bytes() } else { ext(w).to_le_bytes() }).collect()
        }
    }
}

/// The four byte orders give the same samples, and a DTS-HD extension substream after the core
/// frame is skipped.
#[test]
fn byte_orders_and_extension_substreams() {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch("order");
    let file = encode(&dir, "orders", 48000, "5.1(side)", 768, &[]);
    let data = std::fs::read(&file).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let frames = frames_of(&data);
    let packet = |d: Vec<u8>, pts: i64| Packet { stream: 0, pts, duration: 512, data: d };
    let run = |make: &dyn Fn(&[u8]) -> Vec<u8>| -> Vec<f64> {
        let mut dec = DtsDecoder::new();
        let mut out = Vec::new();
        for (i, f) in frames.iter().take(20).enumerate() {
            for fr in dec.decode(&packet(make(f), i as i64 * 512)).unwrap() {
                let Frame::Audio(a) = fr else { panic!("audio expected") };
                assert_eq!(a.channels, 6);
                out.extend(a.samples);
            }
        }
        out
    };
    let want = run(&|f| f.to_vec());
    assert!(want.iter().any(|&x| x.abs() > 0.01));
    for order in [ByteOrder::Le16, ByteOrder::Be14, ByteOrder::Le14] {

        let got = run(&|f| reorder(f, order));
        assert_eq!(got.len(), want.len(), "{order:?}");
        assert!(got == want, "{order:?} differs");
        // the info says what order it found
        assert_eq!(frame_info(&reorder(frames[0], order)).unwrap().byte_order, order);
    }
    // an extension substream (sync 0x64582025, header size 16, size 64) after each core frame
    let with_exss = |f: &[u8]| -> Vec<u8> {
        let mut v = f.to_vec();
        while !v.len().is_multiple_of(4) {
            v.push(0);
        }
        let mut exss = vec![0x64, 0x58, 0x20, 0x25];
        // user bits (8), index (2), narrow header (1), header size - 1 (8), size - 1 (16)
        let mut bits = Vec::new();
        for (val, n) in [(0u32, 8), (0, 2), (0, 1), (15, 8), (63, 16)] {
            for i in (0..n).rev() {
                bits.push(val >> i & 1 != 0);
            }
        }
        for c in bits.chunks(8) {
            exss.push(c.iter().enumerate().fold(0u8, |b, (i, &x)| b | (x as u8) << (7 - i)));
        }
        exss.resize(64, 0xA5);
        v.extend(exss);
        v
    };
    let info = frame_info(&with_exss(frames[0])).unwrap();
    assert!(info.has_extension);
    assert_eq!(info.frame_size, info.core_size.div_ceil(4) * 4 + 64);
    let got = run(&with_exss);
    assert!(got == want, "the extension substream changed the output");
}

/// A frame that is cut short, or damaged, is an error or a decode, never a panic.
#[test]
fn damaged_frames_do_not_panic() {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch("dmg");
    let file = encode(&dir, "damaged", 48000, "5.1(side)", 768, &["-dca_adpcm", "1"]);
    let data = std::fs::read(&file).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let frames = frames_of(&data);
    let frame = frames[3];
    let packet = |d: Vec<u8>| Packet { stream: 0, pts: 0, duration: 512, data: d };

    // cut short: an error for the first half, and never a panic for any length
    for n in 0..frame.len() {
        let r = DtsDecoder::new().decode(&packet(frame[..n].to_vec()));
        if n < frame.len() / 2 {
            assert!(r.is_err(), "a frame cut to {n} bytes decoded");
        }
    }
    // every single-bit flip of the header and side information
    for byte in 0..frame.len().min(200) {
        for bit in 0..8 {
            let mut f = frame.to_vec();
            f[byte] ^= 1 << bit;
            let _ = DtsDecoder::new().decode(&packet(f));
        }
    }
    // runs of fill bytes from every seventh offset on
    for fill in [0x00u8, 0xFF, 0x55, 0xAA] {
        for from in (8..frame.len()).step_by(7) {
            let mut f = frame.to_vec();
            f[from..].fill(fill);
            let _ = DtsDecoder::new().decode(&packet(f));
        }
    }
}
