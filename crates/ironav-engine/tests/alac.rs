//! ALAC in MP4, both ways against FFmpeg (`ffmpeg -c:a alac` writes it, `ffmpeg` decodes it), and
//! IronAV to IronAV. The FFmpeg tests skip when `ffmpeg` is not on the PATH.

use ironav_engine::{run, Job};
use std::path::{Path, PathBuf};
use std::process::Command;

fn scratch(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("ironav-alac-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn job(input: &Path, output: &Path) -> Job {
    Job { input: input.display().to_string(), output: output.display().to_string(), hwaccel: "auto".into(), ..Job::default() }
}

/// A canonical PCM WAV of interleaved integer `samples` at `bits` (16 or 24).
fn write_wav(path: &Path, samples: &[i32], channels: u16, rate: u32, bits: u16) {
    let bytes = (bits / 8) as usize;
    let mut data = Vec::with_capacity(samples.len() * bytes);
    for s in samples {
        data.extend_from_slice(&s.to_le_bytes()[..bytes]);
    }
    let mut f = Vec::new();
    f.extend_from_slice(b"RIFF");
    f.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    f.extend_from_slice(b"WAVEfmt ");
    f.extend_from_slice(&16u32.to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&channels.to_le_bytes());
    f.extend_from_slice(&rate.to_le_bytes());
    f.extend_from_slice(&(rate * channels as u32 * bytes as u32).to_le_bytes());
    f.extend_from_slice(&(channels * bytes as u16).to_le_bytes());
    f.extend_from_slice(&bits.to_le_bytes());
    f.extend_from_slice(b"data");
    f.extend_from_slice(&(data.len() as u32).to_le_bytes());
    f.extend_from_slice(&data);
    std::fs::write(path, f).unwrap();
}

/// Raw bytes of the `data` chunk of a canonical WAV file.
fn wav_data(path: &Path) -> Vec<u8> {
    let b = std::fs::read(path).unwrap();
    let mut p = 12;
    while p + 8 <= b.len() {
        let len = u32::from_le_bytes([b[p + 4], b[p + 5], b[p + 6], b[p + 7]]) as usize;
        if &b[p..p + 4] == b"data" {
            return b[p + 8..p + 8 + len].to_vec();
        }
        p += 8 + len + (len & 1);
    }
    panic!("no data chunk");
}

/// A tone with uniform noise of `noise` (fraction of full scale), at `bits`, interleaved.
fn signal(frames: usize, ch: usize, bits: u32, noise: f64) -> Vec<i32> {
    let scale = (1i64 << (bits - 1)) as f64;
    let mut x = 0x2545_F491_4F6C_DD1Du64;
    let mut out = Vec::with_capacity(frames * ch);
    for i in 0..frames {
        for c in 0..ch {
            x = x.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let r = ((x >> 40) as f64 / (1u64 << 24) as f64 - 0.5) * 2.0 * noise;
            let tone = (i as f64 * (0.031 + 0.017 * c as f64)).sin() * 0.6 + (i as f64 * 0.004).cos() * 0.25;
            out.push(((tone + r) * scale).round().clamp(-scale, scale - 1.0) as i32);
        }
    }
    out
}

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").output().map(|o| o.status.success()).unwrap_or(false)
}

fn ffmpeg(args: &[&str]) {
    let st = Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).status().unwrap();
    assert!(st.success(), "ffmpeg {args:?} failed");
}

#[test]
fn ffmpeg_alac_decodes_to_the_source_pcm() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    let d = scratch("ffmpeg-to-ironav");
    // eight channels are left out: FFmpeg 9.0 writes an 8-channel ALAC that says it has seven, so there is no
    // 8-channel file of its own to read here (IronAV's eight-channel file is checked in FFmpeg below)
    for &(bits, ch) in &[(16u16, 1u16), (16, 2), (24, 1), (24, 2), (16, 6), (24, 6)] {
        let tag = format!("{bits}b{ch}ch");
        let (wav, m4a, back) = (d.join(format!("{tag}.wav")), d.join(format!("{tag}.m4a")), d.join(format!("{tag}-back.wav")));
        // a tone with noise, so the frames mix predicted and near-raw coding; 20000 frames ends mid-frame
        write_wav(&wav, &signal(20_000, ch as usize, bits as u32, 0.02), ch, 44_100, bits);
        ffmpeg(&["-i", wav.to_str().unwrap(), "-c:a", "alac", m4a.to_str().unwrap()]);

        let b = std::fs::read(&m4a).unwrap();
        assert!(b.windows(4).any(|w| w == b"alac"), "{tag}: FFmpeg did not write an alac sample entry");
        assert!(std::fs::metadata(&m4a).unwrap().len() < std::fs::metadata(&wav).unwrap().len(), "{tag}: ALAC should be smaller than the PCM");

        let r = run(&job(&m4a, &back), &mut |_| {}).unwrap();
        assert_eq!(r.format, "wav", "{tag}");
        assert!(wav_data(&back) == wav_data(&wav), "{tag}: IronAV decodes FFmpeg's ALAC differently from the source");
    }
}

#[test]
fn ironav_alac_decodes_bit_exactly_in_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    let d = scratch("ironav-to-ffmpeg");
    // frames of every kind: tonal (predicted), full-scale noise (raw fallback) and silence (zero runs)
    let mut cases: Vec<(String, u16, u16, Vec<i32>)> = Vec::new();
    for &(bits, ch) in &[(16u16, 1u16), (16, 2), (24, 1), (24, 2), (16, 6), (24, 6), (16, 8)] {
        cases.push((format!("tone{bits}b{ch}"), bits, ch, signal(20_000, ch as usize, bits as u32, 0.02)));
    }
    cases.push(("noise16b2".into(), 16, 2, signal(9_000, 2, 16, 1.0)));
    cases.push(("noise16b6".into(), 16, 6, signal(9_000, 6, 16, 1.0)));
    cases.push(("silence24b1".into(), 24, 1, vec![0; 10_000]));
    for (tag, bits, ch, samples) in cases {
        let (wav, m4a, raw) = (d.join(format!("{tag}.wav")), d.join(format!("{tag}.m4a")), d.join(format!("{tag}.pcm")));
        write_wav(&wav, &samples, ch, 44_100, bits);
        let mut j = job(&wav, &m4a);
        j.audio.codec = Some("alac".into());
        let r = run(&j, &mut |_| {}).unwrap();
        assert_eq!(r.format, "mp4", "{tag}");
        let fmt = if bits == 16 { "s16le" } else { "s24le" };
        ffmpeg(&["-i", m4a.to_str().unwrap(), "-c:a", &format!("pcm_{fmt}"), "-f", fmt, raw.to_str().unwrap()]);
        assert!(std::fs::read(&raw).unwrap() == wav_data(&wav), "{tag}: FFmpeg decodes IronAV's ALAC differently from the source");
    }
}

#[test]
fn ironav_alac_round_trips_bit_exactly() {
    let d = scratch("roundtrip");
    for &(bits, ch) in &[(16u16, 1u16), (16, 2), (24, 1), (24, 2), (16, 6), (24, 6), (16, 8)] {
        let tag = format!("{bits}b{ch}ch");
        let (wav, m4a, back) = (d.join(format!("{tag}.wav")), d.join(format!("{tag}.m4a")), d.join(format!("{tag}-back.wav")));
        write_wav(&wav, &signal(12_345, ch as usize, bits as u32, 0.01), ch, 48_000, bits);
        let mut j = job(&wav, &m4a);
        j.audio.codec = Some("alac".into());
        let r = run(&j, &mut |_| {}).unwrap();
        assert!(r.bytes_out < r.bytes_in, "{bits}-bit {ch}ch: ALAC should be smaller than the PCM");
        run(&job(&m4a, &back), &mut |_| {}).unwrap();
        assert!(wav_data(&back) == wav_data(&wav), "{bits}-bit {ch}ch: not bit-exact after ALAC");
    }
}

#[test]
fn alac_is_refused_where_it_cannot_be_made() {
    let d = scratch("refused");
    let (wav, out) = (d.join("in.wav"), d.join("out.m4a"));
    write_wav(&wav, &signal(100, 1, 16, 0.0), 1, 44_100, 16);
    let mut j = job(&wav, &out);
    j.audio.codec = Some("alac".into());
    run(&j, &mut |_| {}).unwrap();
    // nine channels are more than ALAC holds: the encoder says so before anything is written
    let (wav9, out9) = (d.join("in9.wav"), d.join("out9.m4a"));
    write_wav(&wav9, &signal(100, 9, 16, 0.0), 9, 44_100, 16);
    let mut j = job(&wav9, &out9);
    j.audio.codec = Some("alac".into());
    assert!(run(&j, &mut |_| {}).is_err());
    assert!(!out9.exists());
}
