//! The MP2 encoder against FFmpeg: a sine sweep is encoded by IronAV and by FFmpeg's `mp2` encoder
//! from the same 16-bit samples. The frames should match byte for byte (IronAV's are a port of
//! FFmpeg's), FFmpeg should decode IronAV's stream to the input within a sensible SNR, and IronAV's
//! own decoder should decode it to the same samples FFmpeg does. Skipped without `ffmpeg` on PATH.

use ironav_codec::mp3::{parse_frame, Mp2Decoder, Mp2Encoder, MP2_LEAD_IN};
use ironav_core::{AudioFrame, Decoder, Encoder, Frame, Packet, StreamParams};
use ironav_engine::{run, Job};
use std::path::{Path, PathBuf};
use std::process::Command;

const SECONDS: f64 = 3.0;

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success())
}

/// A directory of the test's own: the tests run in parallel, so each has one of its own.
fn scratch(tag: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("mp2-enc-{}-{tag}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn ffmpeg(args: &[&str]) {
    let status = Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).status().expect("running ffmpeg");
    assert!(status.success(), "ffmpeg {args:?} failed");
}

/// A logarithmic sine sweep from 60 Hz to 9 kHz, interleaved, as 16-bit samples: a sweep passes
/// through every subband, so the whole filterbank and the allocation tables get some use.
fn sweep(rate: u32, channels: usize, amplitude: f64) -> Vec<i16> {
    let n = (rate as f64 * SECONDS) as usize;
    let mut out = Vec::with_capacity(n * channels);
    let mut phase = [0f64; 2];
    for i in 0..n {
        let t = i as f64 / rate as f64;
        let f = 60.0 * (9000.0f64 / 60.0).powf(t / SECONDS);
        for (c, p) in phase.iter_mut().enumerate().take(channels) {
            *p += 2.0 * std::f64::consts::PI * f * (1.0 + 0.01 * c as f64) / rate as f64;
            out.push((amplitude * 32767.0 * p.sin()).round().clamp(-32768.0, 32767.0) as i16);
        }
    }
    out
}

fn to_bytes(samples: &[i16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

/// IronAV's encoding of `samples`, as one Layer II elementary stream.
fn encode_ours(samples: &[i16], rate: u32, channels: usize, bitrate: u32) -> Vec<u8> {
    let mut enc = Mp2Encoder::new(rate, channels, bitrate).unwrap();
    let input: Vec<f64> = samples.iter().map(|&s| s as f64 / 32768.0).collect();
    let frame = Frame::Audio(AudioFrame { sample_rate: rate, channels: channels as u16, pts: 0, samples: input });
    let mut packets = enc.encode(&frame).unwrap();
    packets.extend(enc.flush().unwrap());
    packets.iter().flat_map(|p| p.data.clone()).collect()
}

/// The frames of a Layer II stream, one slice each.
fn frames(stream: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut pos = 0;
    while pos + 4 <= stream.len() {
        let h = parse_frame(&stream[pos..]).expect("a frame header");
        assert_eq!(h.layer, 2, "every frame is Layer II");
        out.push(&stream[pos..pos + h.frame_len]);
        pos += h.frame_len;
    }
    assert_eq!(pos, stream.len(), "the stream ends on a frame boundary");
    out
}

/// FFmpeg's decoding of an MP2 file, as 16-bit interleaved samples.
fn ffmpeg_decode(dir: &Path, name: &str, mp2: &Path) -> Vec<i16> {
    let raw = dir.join(format!("{name}.s16le"));
    ffmpeg(&["-i", mp2.to_str().unwrap(), "-f", "s16le", "-acodec", "pcm_s16le", raw.to_str().unwrap()]);
    std::fs::read(&raw).unwrap().as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect()
}

/// IronAV's decoding of a Layer II stream, as 16-bit interleaved samples.
fn ours_decode(stream: &[u8], rate: u32, channels: usize) -> Vec<i16> {
    let mut dec = Mp2Decoder::new(&StreamParams::Audio { sample_rate: rate, channels: channels as u16 }).unwrap();
    let mut out = Vec::new();
    let mut pts = 0;
    for f in frames(stream) {
        for frame in dec.decode(&Packet { stream: 0, pts, duration: 1152, data: f.to_vec() }).unwrap() {
            let Frame::Audio(a) = frame else { panic!("audio expected") };
            out.extend(a.samples.iter().map(|&x| (x * 32768.0).round().clamp(-32768.0, 32767.0) as i16));
        }
        pts += 1152;
    }
    out
}

/// The signal-to-noise ratio of `got` against `want` in dB, with `got` delayed by `lag` samples
/// (the decoder's delay), over interleaved samples.
fn snr(want: &[i16], got: &[i16], lag: usize) -> f64 {
    let n = want.len().min(got.len().saturating_sub(lag));
    let (mut signal, mut noise) = (0f64, 0f64);
    for i in 0..n {
        let w = want[i] as f64;
        let e = w - got[i + lag] as f64;
        signal += w * w;
        noise += e * e;
    }
    10.0 * (signal / noise).log10()
}

/// The lag, in frames, that lines `got` up best with `want` over its first 20000 frames: the delay a
/// container may or may not trim, and its SNR there.
fn best_lag(want: &[i16], got: &[i16], channels: usize) -> (usize, f64) {
    let w = want.len().min(20_000 * channels);
    (0..=1500usize)
        .map(|f| (f, snr(&want[..w], got, f * channels)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .unwrap()
}

/// Encodes a sweep with IronAV and FFmpeg at one configuration and checks what the module says:
/// the frames match, FFmpeg's decoding is close to the input, and IronAV's decoder agrees.
fn check(rate: u32, channels: usize, kbps: u32) {
    check_signal(&format!("{rate}_{channels}ch_{kbps}k"), rate, channels, kbps, sweep(rate, channels, 0.4));
}

fn check_signal(name: &str, rate: u32, channels: usize, kbps: u32, input: Vec<i16>) {
    let dir = scratch(name);
    let raw = dir.join(format!("{name}.s16le"));
    std::fs::write(&raw, to_bytes(&input)).unwrap();

    let reference = dir.join(format!("{name}_ffmpeg.mp2"));
    ffmpeg(&["-f", "s16le", "-ar", &rate.to_string(), "-ac", &channels.to_string(), "-i", raw.to_str().unwrap(), "-b:a", &format!("{kbps}k"), "-c:a", "mp2", reference.to_str().unwrap()]);
    let want = std::fs::read(&reference).unwrap();

    let ours = encode_ours(&input, rate, channels, kbps * 1000);
    let ours_path = dir.join(format!("{name}_ours.mp2"));
    std::fs::write(&ours_path, &ours).unwrap();

    // the frames, byte for byte, over those both streams have
    let (fo, fr) = (frames(&ours), frames(&want));
    let common = fo.len().min(fr.len());
    let same = (0..common).filter(|&i| fo[i] == fr[i]).count();
    eprintln!("{name}: {} frames, {} bytes (FFmpeg: {} frames, {} bytes); {same}/{common} frames identical", fo.len(), ours.len(), fr.len(), want.len());
    assert_eq!(same, common, "{name}: frames differ from FFmpeg's");

    // FFmpeg decodes the stream to the input, after the decoder's delay. The SNR is that of Layer II
    // at this bit rate with a fixed masking model, and FFmpeg's own stream scores the same.
    let decoded = ffmpeg_decode(&dir, &format!("{name}_ours"), &ours_path);
    let lag = MP2_LEAD_IN as usize * channels;
    let snr_db = snr(&input, &decoded, lag);
    let snr_ref = snr(&input, &ffmpeg_decode(&dir, &format!("{name}_ffmpeg"), &reference), lag);
    eprintln!("{name}: FFmpeg decodes IronAV's stream to {snr_db:.2} dB SNR (its own stream: {snr_ref:.2} dB)");
    assert!(snr_db > 8.0, "{name}: SNR {snr_db:.2} dB");

    // IronAV's decoder reads it to what FFmpeg does, within rounding
    let ours_dec = ours_decode(&ours, rate, channels);
    let n = ours_dec.len().min(decoded.len());
    let worst = (0..n).map(|i| (ours_dec[i] as i32 - decoded[i] as i32).abs()).max().unwrap_or(0);
    eprintln!("{name}: IronAV decodes it to {:.2} dB SNR; worst difference from FFmpeg's decoding {worst} LSB", snr(&input, &ours_dec, lag));
    assert!(worst <= 2, "{name}: IronAV's decoding differs from FFmpeg's by {worst}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn encoder_matches_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    // stereo and mono, at the rates and bit rates that select each allocation table
    check(44100, 2, 192);
    check(48000, 2, 128);
    check(32000, 2, 64);
    check(44100, 1, 64);
    check(48000, 1, 96);
}

#[test]
fn full_scale_input_matches_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    // a sweep at full scale: the largest subband values, which take the lowest scale factors
    check_signal("loud_44100_2ch_224k", 44100, 2, 224, sweep(44100, 2, 1.0));
}

/// The job's output in each container that holds MP2 (`.mp2`, `.mka`, `.mp4` and `.ts`), decoded by
/// FFmpeg to the input, and read back by IronAV as the same codec.
#[test]
fn containers_hold_mp2() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let d = scratch("containers");
    let input = sweep(44100, 2, 0.4);
    let wav = d.join("in.wav");
    write_wav(&wav, &input, 2, 44100);
    for ext in ["mp2", "mka", "mp4", "ts"] {
        let out = d.join(format!("out.{ext}"));
        let mut j = Job { input: wav.display().to_string(), output: out.display().to_string(), hwaccel: "auto".into(), ..Job::default() };
        j.audio.codec = Some("mp2".into());
        let r = run(&j, &mut |_| {}).unwrap_or_else(|e| panic!("{ext}: {e:?}"));
        eprintln!("{ext}: format {}", r.format);

        let raw = d.join(format!("out_{ext}.s16le"));
        ffmpeg(&["-i", out.to_str().unwrap(), "-map", "0:a:0", "-f", "s16le", "-acodec", "pcm_s16le", raw.to_str().unwrap()]);
        let decoded: Vec<i16> = std::fs::read(&raw).unwrap().as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect();
        let (lag, _) = best_lag(&input, &decoded, 2);
        let db = snr(&input, &decoded, lag * 2);
        eprintln!("{ext}: FFmpeg decodes it to {db:.2} dB SNR at a lag of {lag} frames");
        assert!(db > 8.0, "{ext}: SNR {db:.2} dB");

        // IronAV reads its own output back to the same signal
        let back = d.join(format!("back_{ext}.wav"));
        run(&job(&out, &back), &mut |_| {}).unwrap();
        let ours = wav_data_i16(&back);
        let (lag, _) = best_lag(&input, &ours, 2);
        assert!(snr(&input, &ours, lag * 2) > 8.0, "{ext}: IronAV reads it back to a different signal");
    }
    let _ = std::fs::remove_dir_all(&d);
}

/// MPEG audio IronAV writes into Matroska carries its encoder's lead-in as CodecDelay, which FFmpeg
/// trims: its decoding then lines up with the input at lag 0 (without the delay it is 481 or 1057
/// samples late). IronAV reads its own file the same way.
#[test]
fn mpeg_audio_in_matroska_has_its_codec_delay_trimmed() {
    if !ffmpeg_available() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let d = scratch("mkv-delay");
    let input = sweep(44100, 2, 0.4);
    let wav = d.join("in.wav");
    write_wav(&wav, &input, 2, 44100);
    for codec in ["mp2", "mp3"] {
        let out = d.join(format!("out_{codec}.mkv"));
        let mut j = Job { input: wav.display().to_string(), output: out.display().to_string(), hwaccel: "auto".into(), ..Job::default() };
        j.audio.codec = Some(codec.into());
        run(&j, &mut |_| {}).unwrap_or_else(|e| panic!("{codec}: {e:?}"));

        let raw = d.join(format!("out_{codec}.s16le"));
        ffmpeg(&["-i", out.to_str().unwrap(), "-map", "0:a:0", "-f", "s16le", "-acodec", "pcm_s16le", raw.to_str().unwrap()]);
        let decoded: Vec<i16> = std::fs::read(&raw).unwrap().as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b)).collect();
        let (lag, _) = best_lag(&input, &decoded, 2);
        assert_eq!(lag, 0, "{codec}: FFmpeg's decoding of the Matroska file starts {lag} frames after the input");

        let back = d.join(format!("back_{codec}.wav"));
        run(&job(&out, &back), &mut |_| {}).unwrap();
        let (lag, _) = best_lag(&input, &wav_data_i16(&back), 2);
        assert_eq!(lag, 0, "{codec}: IronAV's reading of the Matroska file starts {lag} frames after the input");
    }
    let _ = std::fs::remove_dir_all(&d);
}

fn job(input: &Path, output: &Path) -> Job {
    Job { input: input.display().to_string(), output: output.display().to_string(), hwaccel: "auto".into(), ..Job::default() }
}

/// A canonical 16-bit PCM WAV of interleaved samples.
fn write_wav(path: &Path, samples: &[i16], channels: u16, rate: u32) {
    let data = to_bytes(samples);
    let mut f = Vec::new();
    f.extend_from_slice(b"RIFF");
    f.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
    f.extend_from_slice(b"WAVEfmt ");
    f.extend_from_slice(&16u32.to_le_bytes());
    f.extend_from_slice(&1u16.to_le_bytes());
    f.extend_from_slice(&channels.to_le_bytes());
    f.extend_from_slice(&rate.to_le_bytes());
    f.extend_from_slice(&(rate * channels as u32 * 2).to_le_bytes());
    f.extend_from_slice(&(channels * 2).to_le_bytes());
    f.extend_from_slice(&16u16.to_le_bytes());
    f.extend_from_slice(b"data");
    f.extend_from_slice(&(data.len() as u32).to_le_bytes());
    f.extend_from_slice(&data);
    std::fs::write(path, f).unwrap();
}

/// The 16-bit samples of a canonical WAV file's `data` chunk.
fn wav_data_i16(path: &Path) -> Vec<i16> {
    let b = std::fs::read(path).unwrap();
    let mut p = 12;
    while p + 8 <= b.len() {
        let len = u32::from_le_bytes([b[p + 4], b[p + 5], b[p + 6], b[p + 7]]) as usize;
        if &b[p..p + 4] == b"data" {
            return b[p + 8..p + 8 + len].as_chunks::<2>().0.iter().map(|c| i16::from_le_bytes(*c)).collect();
        }
        p += 8 + len + (len & 1);
    }
    panic!("no data chunk");
}
