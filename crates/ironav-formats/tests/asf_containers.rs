//! ASF files made with FFmpeg's muxer when the test runs: stream parameters, packet timestamps and
//! sizes against `ffprobe -show_packets`, seeking (with the Simple Index, and with it made unreadable),
//! metadata, a WMV2 or MS MPEG-4 video stream listed as unsupported beside playable audio, and damaged
//! files (never a panic). Skipped without `ffmpeg` on PATH (ffprobe is needed for the packet check).

use ironav_core::{CodecId, Demuxer, ErrorCode, Frame, Packet};
use ironav_formats::asf;
use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;

fn run(tool: &str, args: &[&str]) -> Option<Vec<u8>> {
    let o = Command::new(tool).args(["-v", "error"]).args(args).output().ok()?;
    o.status.success().then_some(o.stdout)
}

fn have(tool: &str) -> bool {
    Command::new(tool).arg("-version").output().is_ok_and(|o| o.status.success())
}

fn scratch(tag: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("asf-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

const SIMPLE_INDEX: [u8; 16] = [0x90, 0x08, 0x00, 0x33, 0xB1, 0xE5, 0xCF, 0x11, 0x89, 0xF4, 0x00, 0xA0, 0xC9, 0x03, 0x49, 0xCB];

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

/// A three-second WMA file from FFmpeg, with `extra` output options.
fn make_wma(dir: &Path, name: &str, codec: &str, rate: u32, channels: u32, extra: &[&str]) -> PathBuf {
    let out = dir.join(name);
    let source = format!("sine=frequency=440:sample_rate={rate}:duration=3");
    let ch = channels.to_string();
    let mut args = vec!["-y", "-f", "lavfi", "-i", &source, "-ac", &ch, "-c:a", codec, "-b:a", "96k"];
    args.extend_from_slice(extra);
    args.push(out.to_str().unwrap());
    run("ffmpeg", &args).expect("encoding with FFmpeg");
    out
}

fn read_all(d: &mut dyn Demuxer) -> Vec<Packet> {
    let mut v = Vec::new();
    while let Some(p) = d.read_packet().unwrap() {
        v.push(p);
    }
    v
}

fn decode_all(d: &mut dyn Demuxer) -> Vec<f32> {
    let stream = d.streams()[0].clone();
    let mut dec = ironav_codec::new_decoder_for(&stream).unwrap();
    let mut out = Vec::new();
    let mut take = |frames: Vec<Frame>| {
        for f in frames {
            let Frame::Audio(a) = f else { panic!("audio expected") };
            out.extend(a.samples.iter().map(|&x| x as f32));
        }
    };
    while let Some(p) = d.read_packet().unwrap() {
        take(dec.decode(&p).unwrap());
    }
    take(dec.flush().unwrap());
    out
}

#[test]
fn stream_parameters_and_packets_match_ffprobe() {
    if !have("ffmpeg") {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch("probe");
    for (codec, rate, ch) in [("wmav1", 22050, 1), ("wmav2", 44100, 2), ("wmav2", 48000, 1)] {
        let file = make_wma(&dir, &format!("{codec}_{rate}_{ch}.wma"), codec, rate, ch, &[]);
        let mut d = ironav_formats::open_demuxer(&file).unwrap();
        assert_eq!(d.format_name(), "asf");
        let s = d.streams()[0].clone();
        assert_eq!(s.codec, if codec == "wmav1" { CodecId::WmaV1 } else { CodecId::WmaV2 });
        assert_eq!((s.time_base.num, s.time_base.den), (1, 1000));
        assert!(matches!(s.params, ironav_core::StreamParams::Audio { sample_rate, channels } if sample_rate == rate && channels == ch as u16));
        assert!(d.unsupported_streams().is_empty());
        let packets = read_all(d.as_mut());
        assert!(packets.len() > 50, "{} packets", packets.len());
        let secs = s.duration_secs().unwrap();
        assert!((2.9..3.3).contains(&secs), "duration {secs}");
        if have("ffprobe") {
            let out = run("ffprobe", &["-select_streams", "a:0", "-show_entries", "packet=pts,size", "-of", "csv=p=0", file.to_str().unwrap()]).unwrap();
            let want: Vec<(i64, usize)> = String::from_utf8(out).unwrap().lines().filter_map(|l| l.split_once(',')).map(|(a, b)| (a.parse().unwrap(), b.parse().unwrap())).collect();
            let got: Vec<(i64, usize)> = packets.iter().map(|p| (p.pts, p.data.len())).collect();
            assert_eq!(got, want, "{codec} {rate} Hz packets against ffprobe");
            eprintln!("{codec} {rate} Hz {ch}ch: {} packets, timestamps and sizes equal ffprobe's", got.len());
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn seeking_with_and_without_the_simple_index() {
    if !have("ffmpeg") {
        return;
    }
    let dir = scratch("seek");
    // FFmpeg's muxer writes a Simple Index only when there is video (whose key frames it lists), so the
    // audio alone seeks by bisecting the packets, and the file with a video stream by its index
    let audio = make_wma(&dir, "seek.wma", "wmav2", 44100, 2, &[]);
    assert!(find(&std::fs::read(&audio).unwrap(), &SIMPLE_INDEX).is_none());
    let video = dir.join("seek.wmv");
    let args = ["-y", "-f", "lavfi", "-i", "testsrc=size=160x120:rate=10:duration=3", "-f", "lavfi", "-i", "sine=frequency=330:sample_rate=44100:duration=3", "-c:v", "wmv2", "-g", "10", "-c:a", "wmav2", "-b:a", "64k", video.to_str().unwrap()];
    run("ffmpeg", &args).expect("encoding with FFmpeg");
    let bytes = std::fs::read(&video).unwrap();
    let at = find(&bytes, &SIMPLE_INDEX).expect("FFmpeg's muxer writes a Simple Index with video");
    let mut plain = bytes.clone();
    plain[at] ^= 0xFF; // no longer the index's GUID, so the reader finds none
    let audio_bytes = std::fs::read(&audio).unwrap();
    for (label, data) in [("by bisection (audio only)", audio_bytes), ("with the index", bytes), ("without the index", plain)] {
        let all = read_all(&mut asf::open(Cursor::new(data.clone())).unwrap());
        for t in [0.0, 0.5, 1.5, 2.9] {
            let mut d = asf::open(Cursor::new(data.clone())).unwrap();
            // read a little first, so the seek has to move back and forth
            d.read_packet().unwrap();
            let landed = d.seek(t).unwrap();
            assert!(landed <= t + 1e-9 && t - landed < 0.3, "{label}: seek to {t} landed on {landed}");
            let rest = read_all(&mut d);
            let first = all.iter().position(|p| p.pts as f64 / 1000.0 >= landed - 1e-9).unwrap();
            assert_eq!(rest.len(), all.len() - first, "{label}: seek to {t}");
            assert!(rest.iter().zip(&all[first..]).all(|(a, b)| a.pts == b.pts && a.data == b.data), "{label}: seek to {t}");
            eprintln!("{label}: seek to {t:.1} s landed on {landed:.3} s");
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn metadata_is_read_cleaned() {
    if !have("ffmpeg") {
        return;
    }
    let dir = scratch("meta");
    let file = make_wma(&dir, "meta.wma", "wmav2", 44100, 1, &["-metadata", "title=Test Title", "-metadata", "artist=Some Artist", "-metadata", "album=An Album"]);
    let meta = asf::metadata(&file).unwrap();
    let get = |k: &str| meta.iter().find(|m| m.0 == k).map(|m| m.1.as_str());
    assert_eq!(get("title"), Some("Test Title"));
    assert_eq!(get("author"), Some("Some Artist"));
    assert!(meta.iter().any(|m| m.1 == "An Album"), "{meta:?}");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn video_streams_are_listed_as_unsupported_and_the_audio_plays() {
    if !have("ffmpeg") {
        return;
    }
    let dir = scratch("video");
    for (vcodec, name) in [("wmv2", "WMV2"), ("msmpeg4", "MPEG-4 version 3")] {
        let file = dir.join(format!("{vcodec}.wmv"));
        let args = ["-y", "-f", "lavfi", "-i", "testsrc=size=160x120:rate=10:duration=2", "-f", "lavfi", "-i", "sine=frequency=330:sample_rate=44100:duration=2", "-c:v", vcodec, "-c:a", "wmav2", "-b:a", "64k", "-shortest", file.to_str().unwrap()];
        if run("ffmpeg", &args).is_none() {
            eprintln!("FFmpeg cannot make {vcodec}: skipped");
            continue;
        }
        let mut d = ironav_formats::open_demuxer(&file).unwrap();
        assert_eq!(d.streams().len(), 1);
        assert_eq!(d.streams()[0].codec, CodecId::WmaV2);
        let un = d.unsupported_streams();
        assert_eq!(un.len(), 1, "{un:?}");
        assert!(un[0].contains(name) && un[0].contains("160x120"), "{un:?}");
        let got = decode_all(d.as_mut());
        let want = dir.join("want.f32");
        run("ffmpeg", &["-y", "-i", file.to_str().unwrap(), "-vn", "-f", "f32le", "-acodec", "pcm_f32le", want.to_str().unwrap()]).unwrap();
        let want = std::fs::read(&want).unwrap();
        assert_eq!(got.len() * 4, want.len(), "{vcodec}: audio sample count");
        let (mut sig, mut noise) = (0f64, 0f64);
        for (g, w) in got.iter().zip(want.as_chunks::<4>().0) {
            let w = f32::from_le_bytes(*w) as f64;
            sig += w * w;
            noise += (*g as f64 - w).powi(2);
        }
        let snr = 10.0 * (sig / noise.max(1e-300)).log10();
        eprintln!("{vcodec}+wmav2: video listed as {:?}, audio SNR {snr:.1} dB", un[0]);
        assert!(snr > 90.0);
        // a file of the video alone has nothing to play
        let only = dir.join(format!("{vcodec}_only.wmv"));
        run("ffmpeg", &["-y", "-i", file.to_str().unwrap(), "-an", "-c:v", "copy", only.to_str().unwrap()]).unwrap();
        let e = ironav_formats::open_demuxer(&only).err().expect("nothing playable");
        assert_eq!(e.code, ErrorCode::UnsupportedCodec);
        assert!(e.message.contains(name), "{}", e.message);
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn damaged_files_are_errors_not_panics() {
    if !have("ffmpeg") {
        return;
    }
    let dir = scratch("fuzz");
    let file = make_wma(&dir, "fuzz.wma", "wmav2", 22050, 2, &[]);
    let bytes = std::fs::read(&file).unwrap();
    let mut state = 12345u64;
    let mut next = move || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (state >> 33) as usize
    };
    let try_file = |data: Vec<u8>| {
        if let Ok(mut d) = asf::open(Cursor::new(data)) {
            let stream = d.streams().first().cloned();
            let mut dec = stream.and_then(|s| ironav_codec::new_decoder_for(&s).ok());
            for _ in 0..2000 {
                match d.read_packet() {
                    Ok(Some(p)) => {
                        if let Some(dec) = dec.as_mut() {
                            let _ = dec.decode(&p);
                        }
                    }
                    Ok(None) => break,
                    Err(_) => {}
                }
            }
            let _ = d.seek(1.0);
            let _ = d.read_packet();
        }
    };
    // truncations
    for cut in (0..bytes.len()).step_by(97) {
        try_file(bytes[..cut].to_vec());
    }
    // flipped bytes: in the header, and in the packets
    for i in 0..400 {
        let mut b = bytes.clone();
        let span = if i % 2 == 0 { 600 } else { b.len() };
        for _ in 0..1 + next() % 4 {
            let at = next() % span.min(b.len());
            b[at] = next() as u8;
        }
        try_file(b);
    }
    // absurd sizes in the header object and the data object
    for off in [16usize, 24, 30 + 16, 30 + 40] {
        let mut b = bytes.clone();
        b[off..off + 8].copy_from_slice(&u64::MAX.to_le_bytes());
        try_file(b);
    }
    let _ = std::fs::remove_dir_all(&dir);
}
