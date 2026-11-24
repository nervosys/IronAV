//! E-AC-3 streams, made with FFmpeg's E-AC-3 encoder when the test runs, decode to what FFmpeg's
//! float decoder makes of them: mono to 5.1 (with and without the LFE channel), at 32, 44.1 and 48
//! kHz, bit rates from 96 to 640 kbit/s, with and without channel coupling. Each case prints its
//! signal-to-noise ratio. Skipped without `ffmpeg` on PATH.
//!
//! FFmpeg's encoder writes neither spectral extension nor the adaptive hybrid transform; those are
//! checked against FFmpeg's decoder by the synthetic streams in the codec's own unit tests.

use ironav_codec::ac3::{any_frame_info, eac3_frame_info, frame_info, Ac3Decoder};
use ironav_core::{Decoder, ErrorCode, Frame, Packet};
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
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("eac3-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// Encode `name.eac3` with FFmpeg. `layout` is "mono", "stereo", "5.1" (the signal goes to the first
/// channel and, scaled, to the others) or any other FFmpeg channel layout (the signal goes to all).
fn encode(dir: &Path, name: &str, rate: u32, layout: &str, kbps: u32, extra: &[&str]) -> PathBuf {
    let out = dir.join(format!("{name}.eac3"));
    let source = format!("aevalsrc=exprs='{SIGNAL}':s={rate}:d=3");
    let pan = match layout {
        "mono" => "pan=mono|c0=c0".to_string(),
        "stereo" => "pan=stereo|c0=c0|c1=0.7*c0".to_string(),
        "5.1" => "pan=5.1|c0=c0|c1=0.9*c0|c2=0.5*c0|c3=0.3*c0|c4=0.8*c0|c5=0.7*c0".to_string(),
        l => format!("aformat=channel_layouts={l}"),
    };
    let kbps = format!("{kbps}k");
    let mut args: Vec<&str> = vec!["-f", "lavfi", "-i", &source, "-af", &pan, "-c:a", "eac3", "-b:a", &kbps];
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

/// Decode an E-AC-3 elementary stream frame by frame, as f32 interleaved samples.
fn decode(data: &[u8], rate: u32, channels: u16) -> Vec<f32> {
    let mut dec = Ac3Decoder::new();
    let mut out = Vec::new();
    let mut pos = 0;
    let mut pts = 0;
    while pos < data.len() {
        let info = eac3_frame_info(&data[pos..]).expect("an E-AC-3 frame header");
        let start = pos + info.offset;
        let frame = &data[start..start + info.frame_size];
        assert_eq!((info.sample_rate, info.channels), (rate, channels));
        assert_eq!(info.samples, 1536);
        for f in dec.decode(&Packet { stream: 0, pts, duration: 1536, data: frame.to_vec() }).unwrap() {
            let Frame::Audio(a) = f else { panic!("audio expected") };
            assert_eq!((a.sample_rate, a.channels, a.pts), (rate, channels, pts));
            out.extend(a.samples.iter().map(|&x| x as f32));
        }
        pos = start + info.frame_size;
        pts += 1536;
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
fn eac3_matches_ffmpeg() {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch("cmp");
    let mut results = Vec::new();
    // (name, rate, layout, channels, kbps, extra encoder options)
    let cases: &[(&str, u32, &str, u16, u32, &[&str])] = &[
        ("mono48_96", 48000, "mono", 1, 96, &[]),
        ("stereo48_192", 48000, "stereo", 2, 192, &[]),
        ("stereo441_128", 44100, "stereo", 2, 128, &[]),
        ("stereo32_96", 32000, "stereo", 2, 96, &[]),
        ("stereo48_cpl", 48000, "stereo", 2, 96, &["-channel_coupling", "1"]),
        ("stereo48_nocpl", 48000, "stereo", 2, 96, &["-channel_coupling", "0", "-stereo_rematrixing", "0"]),
        ("surround48_192", 48000, "5.1", 6, 192, &[]),
        ("surround48_384", 48000, "5.1", 6, 384, &[]),
        ("surround48_640", 48000, "5.1", 6, 640, &[]),
        ("surround48_cpl", 48000, "5.1", 6, 192, &["-channel_coupling", "1", "-cpl_start_band", "3"]),
        ("surround441_448", 44100, "5.1", 6, 448, &[]),
        (
            "surround48_meta",
            48000,
            "5.1",
            6,
            256,
            &["-per_frame_metadata", "1", "-mixing_level", "100", "-room_type", "1", "-dmix_mode", "2", "-ltrt_cmixlev", "1.0", "-loro_surmixlev", "0.5", "-dsurex_mode", "2"],
        ),
        ("5.0_320", 48000, "5.0", 5, 320, &[]),
        ("quad_256", 48000, "quad", 4, 256, &[]),
        ("2.1_192", 48000, "2.1", 3, 192, &[]),
        ("3.0_192", 48000, "3.0", 3, 192, &[]),
        ("4.0_256", 48000, "4.0", 4, 256, &[]),
    ];
    for &(name, rate, layout, ch, kbps, extra) in cases {
        let file = encode(&dir, name, rate, layout, kbps, extra);
        results.push((name, compare(&dir, name, &file, rate, ch)));
    }
    let _ = std::fs::remove_dir_all(&dir);
    for (name, s) in results {
        assert!(s > 60.0, "{name}: SNR {s} dB against FFmpeg");
    }
}

/// A frame that is cut short, or damaged, is an error or a decode, never a panic.
#[test]
fn damaged_frames_do_not_panic() {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch("dmg");
    let file = encode(&dir, "damaged", 48000, "5.1", 192, &["-channel_coupling", "1"]);
    let data = std::fs::read(&file).unwrap();
    let _ = std::fs::remove_dir_all(&dir);
    let info = eac3_frame_info(&data).unwrap();
    let frame = &data[..info.frame_size];
    let packet = |d: Vec<u8>| Packet { stream: 0, pts: 0, duration: 1536, data: d };

    // cut short at every length: an error
    for n in 0..frame.len() {
        assert!(Ac3Decoder::new().decode(&packet(frame[..n].to_vec())).is_err(), "a frame cut to {n} bytes decoded");
    }
    // every single-bit flip of the header, side information and the start of the mantissas
    for byte in 0..frame.len().min(300) {
        for bit in 0..8 {
            let mut f = frame.to_vec();
            f[byte] ^= 1 << bit;
            let _ = Ac3Decoder::new().decode(&packet(f));
        }
    }
    // runs of fill bytes from every seventh offset on
    for fill in [0x00u8, 0xFF, 0x55, 0xAA] {
        for from in (8..frame.len()).step_by(7) {
            let mut f = frame.to_vec();
            f[from..].fill(fill);
            let _ = Ac3Decoder::new().decode(&packet(f));
        }
    }
}

/// The header of a frame that promises more bytes than there are, or less than a header, or that has
/// a reserved field, is an error.
#[test]
fn bad_headers_are_errors() {
    // sync word, then strmtyp 0, substreamid 0, frmsiz 999 (2000 bytes), fscod 0, numblkscod 3, acmod 7,
    // lfeon 1, bsid 16, and zeros
    let mut f = vec![0x0B, 0x77];
    let mut bits = Vec::<bool>::new();
    for (v, n) in [(0u32, 2), (0, 3), (999, 11), (0, 2), (3, 2), (7, 3), (1, 1), (16, 5), (0, 200)] {
        for i in (0..n).rev() {
            bits.push(i < 32 && v >> i & 1 != 0);
        }
    }
    for c in bits.chunks(8) {
        f.push(c.iter().enumerate().fold(0u8, |b, (i, &x)| b | (x as u8) << (7 - i)));
    }
    let packet = |d: Vec<u8>| Packet { stream: 0, pts: 0, duration: 0, data: d };

    assert_eq!(eac3_frame_info(&f).unwrap().frame_size, 2000);
    assert!(any_frame_info(&f).unwrap().eac3);
    // the data is far shorter than the header says
    assert!(Ac3Decoder::new().decode(&packet(f.clone())).is_err());
    // the AC-3 reader refuses it as another codec
    assert_eq!(frame_info(&f).unwrap_err().code, ErrorCode::UnsupportedCodec);
    // a frame size under the header's own length
    let mut g = f.clone();
    g[2] = 0;
    g[3] = 0;
    assert!(Ac3Decoder::new().decode(&packet(g)).is_err());
    // the reserved stream type
    let mut h = f.clone();
    h[2] |= 0xC0;
    assert!(Ac3Decoder::new().decode(&packet(h)).is_err());
    // a bit stream id past 16
    let mut k = f;
    k[5] = (k[5] & 0x07) | (17 << 3);
    assert!(Ac3Decoder::new().decode(&packet(k)).is_err());
}
