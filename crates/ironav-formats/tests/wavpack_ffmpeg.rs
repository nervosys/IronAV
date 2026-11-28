//! WavPack streams, made with FFmpeg's native encoder (lossless only) when the test runs, decode
//! through the `.wv` and Matroska readers and IronAV's decoder to exactly what FFmpeg's decoder
//! makes of them: 8, 16, 24 and 32-bit integers and float, mono, stereo, 3 and 5.1 channels,
//! compression levels 0, 3 and 8, joint stereo on and off, tones with noise, runs of digital silence,
//! full-scale squares and plain silence. Also the duration from the block headers, seeking, the tags
//! at the end of the file and damaged files. Skipped without `ffmpeg` on PATH.
//!
//! FFmpeg has no hybrid (lossy) WavPack encoder, so that path is covered by `ironav-codec`'s unit
//! tests (a lossless block marked hybrid) and not by a real lossy file.

use ironav_core::{Demuxer, Frame, Packet, StreamInfo};
use std::path::{Path, PathBuf};
use std::process::Command;

fn ffmpeg(args: &[&str]) -> bool {
    match Command::new("ffmpeg").args(["-v", "error", "-y"]).args(args).status() {
        Ok(s) if s.success() => true,
        Ok(s) => {
            eprintln!("ffmpeg {args:?} exited with {s}");
            false
        }
        Err(e) => {
            eprintln!("ffmpeg could not run: {e}");
            false
        }
    }
}

fn have_ffmpeg() -> bool {
    Command::new("ffmpeg").arg("-version").output().is_ok_and(|o| o.status.success())
}

fn scratch(tag: &str) -> PathBuf {
    let d = Path::new(env!("CARGO_TARGET_TMPDIR")).join(format!("wv-{tag}-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

#[derive(Clone, Copy, Debug)]
enum Sig {
    /// a tone and noise, a different tone on each channel
    ToneNoise,
    /// tone bursts between runs of digital silence
    Bursts,
    /// a full-scale square wave, so samples at the extremes
    Square,
    /// nothing at all
    Silence,
}

/// The lavfi expression of channel `k` of a signal.
fn expr(sig: Sig, k: usize) -> String {
    let f = 220 * (k + 1);
    match sig {
        Sig::ToneNoise => format!("0.45*sin(2*PI*{f}*t)+0.25*(random({k})-0.5)+0.1*sin(2*PI*{}*t)", f * 7),
        Sig::Bursts => format!("if(lt(mod(t,0.2),0.07),0.6*sin(2*PI*{f}*t)+0.1*(random({k})-0.5),0)"),
        Sig::Square => format!("if(gt(sin(2*PI*{}*t),0),1,-1)", 30 + 7 * k),
        Sig::Silence => "0".to_string(),
    }
}

#[derive(Clone, Copy, Debug)]
enum Depth {
    U8,
    S16,
    S24,
    S32,
    Float,
}

impl Depth {
    fn args(self) -> Vec<&'static str> {
        match self {
            Depth::U8 => vec!["-sample_fmt", "u8p"],
            Depth::S16 => vec!["-sample_fmt", "s16p"],
            Depth::S24 => vec!["-sample_fmt", "s32p", "-bits_per_raw_sample", "24"],
            Depth::S32 => vec!["-sample_fmt", "s32p"],
            Depth::Float => vec!["-sample_fmt", "fltp"],
        }
    }
    /// FFmpeg's raw format of the decode, and the bytes per sample.
    fn raw(self) -> &'static str {
        match self {
            Depth::U8 | Depth::S16 => "s16le",
            Depth::S24 | Depth::S32 => "s32le",
            Depth::Float => "f32le",
        }
    }
}

struct Case {
    name: String,
    sig: Sig,
    channels: usize,
    depth: Depth,
    rate: u32,
    level: &'static str,
    joint: Option<bool>,
    block: Option<u32>,
    seconds: f64,
}

fn layout(channels: usize) -> &'static str {
    match channels {
        1 => "mono",
        2 => "stereo",
        3 => "3.0",
        _ => "5.1",
    }
}

/// Encode a case to `.wv` with FFmpeg, if its encoder takes it.
fn encode(dir: &Path, c: &Case) -> Option<PathBuf> {
    let out = dir.join(format!("{}.wv", c.name));
    let exprs: Vec<String> = (0..c.channels).map(|k| expr(c.sig, k)).collect();
    let source = format!("aevalsrc=exprs='{}':s={}:d={}:c={}", exprs.join("|"), c.rate, c.seconds, layout(c.channels));
    let joint = match c.joint {
        Some(true) => "1",
        Some(false) => "0",
        None => "-1",
    };
    let block = c.block.map(|b| b.to_string()).unwrap_or_default();
    let mut args: Vec<&str> = vec!["-f", "lavfi", "-i", &source, "-c:a", "wavpack", "-compression_level", c.level, "-joint_stereo", joint];
    args.extend(c.depth.args());
    if c.block.is_some() {
        args.extend(["-frame_size", &block]);
    }
    args.push(out.to_str().unwrap());
    ffmpeg(&args).then_some(out)
}

enum Raw {
    Int(Vec<i32>),
    Float(Vec<f32>),
}

/// FFmpeg's decode of `file` to raw samples of the depth's format.
fn reference(dir: &Path, name: &str, file: &Path, depth: Depth) -> Raw {
    let out = dir.join(format!("{name}.{}", depth.raw()));
    assert!(ffmpeg(&["-i", file.to_str().unwrap(), "-f", depth.raw(), out.to_str().unwrap()]), "decoding {name} with FFmpeg");
    let bytes = std::fs::read(&out).unwrap();
    match depth {
        Depth::U8 | Depth::S16 => Raw::Int(bytes.as_chunks::<2>().0.iter().map(|b| i16::from_le_bytes(*b) as i32).collect()),
        Depth::S24 | Depth::S32 => Raw::Int(bytes.as_chunks::<4>().0.iter().map(|b| i32::from_le_bytes(*b)).collect()),
        Depth::Float => Raw::Float(bytes.as_chunks::<4>().0.iter().map(|b| f32::from_le_bytes(*b)).collect()),
    }
}

/// Decode every packet from the demuxer's position: f64 interleaved, with the stream's rate and channels.
fn decode_all(demux: &mut dyn Demuxer, stream: &StreamInfo) -> Vec<f64> {
    let mut dec = ironav_codec::new_decoder_for(stream).unwrap();
    let mut out = Vec::new();
    while let Some(p) = demux.read_packet().unwrap() {
        let p: Packet = p;
        for f in dec.decode(&p).unwrap() {
            let Frame::Audio(a) = f else { panic!("audio expected") };
            out.extend(a.samples);
        }
    }
    out
}

fn decode_file(file: &Path) -> (Vec<f64>, StreamInfo) {
    let mut demux = ironav_formats::open_demuxer(file).unwrap();
    let stream = demux.streams()[0].clone();
    (decode_all(demux.as_mut(), &stream), stream)
}

/// Whether `got` equals `want` sample for sample, where the integers are `scale` times smaller than
/// the f64. Returns the index of the first difference.
fn first_difference(got: &[f64], want: &Raw, depth: Depth) -> Option<usize> {
    match want {
        Raw::Int(w) => {
            let scale = if matches!(depth, Depth::U8 | Depth::S16) { 32768.0 } else { 2147483648.0 };
            (0..got.len().max(w.len())).find(|&i| match (got.get(i), w.get(i)) {
                (Some(&g), Some(&w)) => g * scale != w as f64,
                _ => true,
            })
        }
        Raw::Float(w) => (0..got.len().max(w.len())).find(|&i| match (got.get(i), w.get(i)) {
            (Some(&g), Some(&w)) => (g as f32).to_bits() != w.to_bits(),
            _ => true,
        }),
    }
}

fn cases() -> Vec<Case> {
    let mut v = Vec::new();
    let mut add = |sig: Sig, channels: usize, depth: Depth, rate: u32, level: &'static str, joint: Option<bool>, block: Option<u32>, seconds: f64| {
        let name = format!("{sig:?}_{depth:?}_{channels}ch_{rate}_L{level}_j{}", joint.map_or("auto", |j| if j { "on" } else { "off" }));
        v.push(Case { name, sig, channels, depth, rate, level, joint, block, seconds });
    };
    // every depth, in mono and stereo, at the middle level
    for depth in [Depth::U8, Depth::S16, Depth::S24, Depth::S32, Depth::Float] {
        add(Sig::ToneNoise, 2, depth, 44100, "3", None, Some(8192), 1.0);
        add(Sig::ToneNoise, 1, depth, 48000, "3", None, Some(8192), 0.8);
        add(Sig::Bursts, 2, depth, 22050, "3", None, None, 1.0);
        add(Sig::Square, 2, depth, 32000, "3", None, Some(4096), 0.6);
    }
    // the compression levels and joint stereo
    for level in ["0", "1", "2", "3", "5", "8"] {
        for joint in [None, Some(true), Some(false)] {
            add(Sig::ToneNoise, 2, Depth::S16, 44100, level, joint, Some(8192), 0.3);
        }
        add(Sig::Bursts, 1, Depth::S16, 44100, level, None, Some(4096), 0.3);
    }
    for level in ["0", "8"] {
        add(Sig::ToneNoise, 2, Depth::S24, 96000, level, None, Some(16384), 0.2);
        add(Sig::Bursts, 2, Depth::Float, 44100, level, None, Some(8192), 0.3);
        add(Sig::Square, 1, Depth::S32, 44100, level, None, Some(8192), 0.3);
    }
    // more channels: blocks of one and two channels in a frame
    for depth in [Depth::S16, Depth::S24, Depth::Float] {
        for level in ["0", "3", "8"] {
            add(Sig::ToneNoise, 6, depth, 22050, level, None, Some(4096), 0.3);
        }
        add(Sig::Bursts, 3, depth, 48000, "3", None, Some(4096), 0.5);
    }
    // silence, and an odd sample rate
    add(Sig::Silence, 2, Depth::S16, 44100, "3", None, Some(8192), 0.5);
    add(Sig::Silence, 1, Depth::Float, 44100, "3", None, Some(8192), 0.5);
    add(Sig::ToneNoise, 2, Depth::S16, 11025, "3", None, Some(2048), 0.8);
    add(Sig::ToneNoise, 2, Depth::S16, 8000, "8", None, Some(2000), 0.5);
    v
}

/// Runs one case through both containers: the report lines, the failures and the files compared.
fn run_case(dir: &Path, c: &Case) -> (Vec<String>, Vec<String>, usize) {
    let (mut lines, mut failures, mut ran) = (Vec::new(), Vec::new(), 0);
    let Some(file) = encode(dir, c) else {
        lines.push(format!("{:>46}: FFmpeg's encoder refused the case", c.name));
        return (lines, failures, ran);
    };
    let mka = dir.join(format!("{}.mka", c.name));
    let have_mka = ffmpeg(&["-i", file.to_str().unwrap(), "-c", "copy", mka.to_str().unwrap()]);
    let want = reference(dir, &c.name, &file, c.depth);
    for (kind, path) in [("wv", Some(&file)), ("mka", have_mka.then_some(&mka))] {
        let Some(path) = path else {
            failures.push(format!("{} {kind}: FFmpeg could not remux", c.name));
            continue;
        };
        let (got, stream) = decode_file(path);
        let ironav_core::StreamParams::Audio { sample_rate, channels } = stream.params else { panic!("audio stream") };
        let mut problems = Vec::new();
        if sample_rate != c.rate || channels as usize != c.channels {
            problems.push(format!("parameters {sample_rate} Hz {channels} ch"));
        }
        if let Some(i) = first_difference(&got, &want, c.depth) {
            problems.push(format!("first difference at sample {i} of {}", got.len()));
        }
        let n = got.len() / c.channels;
        if kind == "wv" && stream.duration != Some(n as i64) {
            problems.push(format!("duration {:?}, {n} decoded", stream.duration));
        }
        lines.push(format!("{:>46} {kind:>3}: {} samples {}", c.name, got.len(), if problems.is_empty() { "identical".to_string() } else { problems.join(", ") }));
        if !problems.is_empty() {
            failures.push(format!("{} {kind}: {}", c.name, problems.join(", ")));
        }
        ran += 1;
    }
    (lines, failures, ran)
}

#[test]
fn wavpack_matches_ffmpeg() {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch("cmp");
    let list = cases();
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results = std::sync::Mutex::new(Vec::new());
    std::thread::scope(|s| {
        for _ in 0..4 {
            s.spawn(|| loop {
                let i = next.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let Some(c) = list.get(i) else { break };
                let r = run_case(&dir, c);
                results.lock().unwrap().push((i, r));
            });
        }
    });
    let mut results = results.into_inner().unwrap();
    results.sort_by_key(|r| r.0);
    let (mut ran, mut failures) = (0, Vec::new());
    for (_, (lines, f, n)) in results {
        for l in lines {
            eprintln!("{l}");
        }
        failures.extend(f);
        ran += n;
    }
    eprintln!("{ran} files compared");
    assert!(failures.is_empty(), "differences from FFmpeg: {failures:#?}");
    assert!(ran >= 100, "too few cases ran ({ran})");
    let _ = std::fs::remove_dir_all(&dir);
}

/// Where the blocks of a `.wv` file end (FFmpeg's muxer writes an APEv2 tag after them).
fn audio_end(file: &[u8]) -> usize {
    let mut p = 0;
    while let Some(h) = ironav_codec::wavpack::BlockHeader::parse(&file[p..]) {
        p += 8 + h.size as usize;
    }
    p
}

/// A stereo 16-bit file of 3 seconds in blocks of 4096 samples.
fn seekable(dir: &Path) -> Option<PathBuf> {
    let c = Case { name: "seek".into(), sig: Sig::ToneNoise, channels: 2, depth: Depth::S16, rate: 44100, level: "3", joint: None, block: Some(4096), seconds: 3.0 };
    encode(dir, &c)
}

#[test]
fn seeking_lands_on_a_block_and_matches_a_full_read() {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch("seek");
    let file = seekable(&dir).expect("FFmpeg encodes the file");
    let mka = dir.join("seek.mka");
    assert!(ffmpeg(&["-i", file.to_str().unwrap(), "-c", "copy", mka.to_str().unwrap()]));
    let (full, stream) = decode_file(&file);
    let ironav_core::StreamParams::Audio { sample_rate, channels } = stream.params else { panic!("audio") };
    let total = full.len() / channels as usize;
    assert_eq!(total, 3 * 44100);
    assert_eq!(stream.duration, Some(total as i64), "duration from the block headers");
    for path in [&file, &mka] {
        let is_wv = path == &file;
        for want in [0.0, 1.5, 0.2, 2.0, 2.9, 2.99, 10.0] {
            let mut demux = ironav_formats::open_demuxer(path).unwrap();
            let stream = demux.streams()[0].clone();
            let landed = demux.seek(want).unwrap();
            let landed_at = (landed * sample_rate as f64).round() as usize;
            let rest = decode_all(demux.as_mut(), &stream);
            // where the packets after the seek start, by what is left of the whole
            let at = total - rest.len() / channels as usize;
            let wanted = (want * sample_rate as f64).round() as usize;
            if is_wv {
                // the blocks hold 4096 samples each, so a seek lands on a multiple of that
                assert_eq!(at, landed_at, "seek to {want}: the reported time is where the samples start");
                assert_eq!(at % 4096, 0, "seek to {want} landed at sample {at}");
                if want <= 3.0 {
                    assert!(at <= wanted && wanted < at + 4096, "seek to {want} landed at sample {at}, not on the block that holds sample {wanted}");
                }
            } else {
                // Matroska times are in milliseconds
                assert!(at.abs_diff(landed_at) <= 45, "seek to {want}: landed at {landed}, samples start at {at}");
                assert!(at <= wanted + 45 || want > 3.0, "seek to {want} landed after it, at sample {at}");
            }
            if want <= 3.0 {
                assert_eq!(rest.as_slice(), &full[at * channels as usize..], "{path:?}: the samples after a seek to {want} (landed at {at}) equal a full read from there");
            }
            eprintln!("{} seek {want:>5}: landed at sample {at} ({landed:.4} s), {} samples follow, equal to the full read", if is_wv { "wv " } else { "mka" }, rest.len() / channels as usize);
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// An APEv2 tag (with header and footer) of text items.
fn ape_tag(items: &[(&str, &str)]) -> Vec<u8> {
    let mut body = Vec::new();
    for (k, v) in items {
        body.extend((v.len() as u32).to_le_bytes());
        body.extend(0u32.to_le_bytes());
        body.extend(k.as_bytes());
        body.push(0);
        body.extend(v.as_bytes());
    }
    let size = (body.len() + 32) as u32;
    let part = |flags: u32| {
        let mut h = b"APETAGEX".to_vec();
        h.extend(2000u32.to_le_bytes());
        h.extend(size.to_le_bytes());
        h.extend((items.len() as u32).to_le_bytes());
        h.extend(flags.to_le_bytes());
        h.extend([0u8; 8]);
        h
    };
    let mut tag = part(0xA000_0000);
    tag.extend(&body);
    tag.extend(part(0x8000_0000));
    tag
}

fn id3v1(title: &str, artist: &str, album: &str) -> Vec<u8> {
    let mut t = b"TAG".to_vec();
    for (s, n) in [(title, 30), (artist, 30), (album, 30), ("1999", 4), ("a comment", 30)] {
        let mut f = s.as_bytes().to_vec();
        f.resize(n, 0);
        t.extend(f);
    }
    t.push(17);
    t
}

#[test]
fn tags_at_the_end_are_skipped_and_read() {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch("tags");
    let file = seekable(&dir).expect("FFmpeg encodes the file");
    let written = std::fs::read(&file).unwrap();
    let (want, _) = decode_file(&file);
    // FFmpeg's muxer tags its files with the encoder's name
    let own = ironav_formats::wv::metadata(&file).unwrap();
    eprintln!("FFmpeg's own tag: {own:?}");
    assert!(own.iter().any(|(k, v)| k == "encoder" && v.starts_with("Lavf")), "{own:?}");
    let plain = written[..audio_end(&written)].to_vec();
    let path = dir.join("untagged.wv");
    std::fs::write(&path, &plain).unwrap();
    assert!(ironav_formats::wv::metadata(&path).unwrap().is_empty());
    assert_eq!(decode_file(&path).0, want);

    let variants: Vec<(&str, Vec<u8>, Vec<(String, String)>)> = vec![
        (
            "ape",
            [plain.clone(), ape_tag(&[("Title", "Tone"), ("Artist", "Test\u{1b}[31m"), ("Album", "Lab")])].concat(),
            vec![("title".into(), "Tone".into()), ("artist".into(), "Test[31m".into()), ("album".into(), "Lab".into())],
        ),
        (
            "ape+id3v1",
            [plain.clone(), ape_tag(&[("Title", "Tone")]), id3v1("Old", "Older", "Oldest")].concat(),
            vec![("title".into(), "Tone".into())],
        ),
        (
            "id3v1",
            [plain.clone(), id3v1("Old", "Older", "Oldest")].concat(),
            vec![("title".into(), "Old".into()), ("artist".into(), "Older".into()), ("album".into(), "Oldest".into()), ("year".into(), "1999".into()), ("comment".into(), "a comment".into())],
        ),
    ];
    for (name, bytes, tags) in variants {
        let path = dir.join(format!("tagged_{name}.wv"));
        std::fs::write(&path, bytes).unwrap();
        let (got, stream) = decode_file(&path);
        assert_eq!(got, want, "{name}: the audio is unchanged by the tag");
        assert_eq!(stream.duration, Some(3 * 44100), "{name}: duration");
        assert_eq!(ironav_formats::wv::metadata(&path).unwrap(), tags, "{name}: tags");
        eprintln!("{name}: tags read as {tags:?}, audio unchanged");
    }
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn damaged_files_give_errors_not_panics() {
    if !have_ffmpeg() {
        eprintln!("skipped: ffmpeg is not on PATH");
        return;
    }
    let dir = scratch("bad");
    let c = Case { name: "bad".into(), sig: Sig::ToneNoise, channels: 6, depth: Depth::S16, rate: 22050, level: "3", joint: None, block: Some(1024), seconds: 0.2 };
    let file = encode(&dir, &c).expect("FFmpeg encodes the file");
    let written = std::fs::read(&file).unwrap();
    let good = written[..audio_end(&written)].to_vec();
    let (full, _) = decode_file(&file);
    assert!(!full.is_empty());

    // outcomes of reading and decoding a file: Ok(samples) or Err(message); never a panic
    let run = |name: &str, bytes: &[u8]| -> Result<Vec<f64>, String> {
        let path = dir.join(format!("{name}.wv"));
        std::fs::write(&path, bytes).unwrap();
        let mut demux = ironav_formats::open_demuxer(&path).map_err(|e| format!("open: {e}"))?;
        let stream = demux.streams()[0].clone();
        let mut dec = ironav_codec::new_decoder_for(&stream).map_err(|e| format!("decoder: {e}"))?;
        let mut out = Vec::new();
        while let Some(p) = demux.read_packet().map_err(|e| format!("read: {e}"))? {
            for f in dec.decode(&p).map_err(|e| format!("decode: {e}"))? {
                let Frame::Audio(a) = f else { panic!("audio expected") };
                out.extend(a.samples);
            }
        }
        Ok(out)
    };

    // the blocks of the first frame
    let h = |at: usize| ironav_codec::wavpack::BlockHeader::parse(&good[at..]).unwrap();
    let first = h(0);
    let first_len = first.size as usize + 8;

    // a bad CRC in a block header: the frame is refused
    let mut b = good.clone();
    b[28] ^= 0x01;
    let r = run("crc", &b);
    eprintln!("bad CRC: {r:?}", r = r.as_ref().map(|v| v.len()));
    assert!(r.as_ref().is_err_and(|e| e.contains("CRC")), "a bad CRC is an error: {:?}", r.map(|v| v.len()));

    // a flipped bit in the middle of the entropy-coded data: an error (or, rarely, identical samples)
    let mut b = good.clone();
    b[first_len / 2] ^= 0x10;
    let r = run("flip", &b);
    eprintln!("flipped bit: {:?}", r.as_ref().map(|v| v.len()));
    assert!(r.is_err() || r.as_ref().is_ok_and(|v| *v != full || v.len() == full.len()));

    // truncated in the middle of the first block: nothing playable, so refused
    let r = run("trunc_first", &good[..first_len / 2]);
    eprintln!("cut in the first block: {r:?}", r = r.as_ref().map(|v| v.len()));
    assert!(r.is_err(), "no whole frame");

    // truncated in the middle of a later block: the frames before it are read, the cut one dropped
    let r = run("trunc_late", &good[..good.len() - 40]).expect("the whole frames are read");
    assert!(!r.is_empty() && r.len() < full.len() && r == full[..r.len()], "the frames before the cut are intact");
    eprintln!("cut near the end: {} of {} samples, intact", r.len(), full.len());

    // a size that runs far past the file: the block is not trusted, the reader moves on
    let mut b = good.clone();
    b[4..8].copy_from_slice(&0x000F_FFF0u32.to_le_bytes());
    let r = run("bigsize", &b);
    eprintln!("a huge first block size: {r:?}", r = r.as_ref().map(|v| v.len()));
    if let Ok(v) = &r {
        assert!(v.len() < full.len());
    }
    // sizes under the header's 24 bytes and over the 1 MiB limit are not blocks at all
    for (name, size) in [("size0", 0u32), ("size23", 23), ("size_limit", (1 << 20) + 1), ("size_max", u32::MAX)] {
        let mut b = good.clone();
        b[4..8].copy_from_slice(&size.to_le_bytes());
        let r = run(name, &b);
        eprintln!("block size {size}: {:?}", r.as_ref().map(|v| v.len()));
    }

    // garbage before the first block, between blocks and after the last
    let mut b = vec![0x55u8; 1000];
    b.extend(&good);
    // (a file is recognised by a block header at its very start, as FFmpeg's probe does)
    let r = run("junk_front", &b);
    assert!(r.as_ref().is_err_and(|e| e.contains("unrecognised")), "{:?}", r.map(|v| v.len()));
    let mut b = good[..first_len].to_vec();
    b.extend(vec![0xAA; 333]);
    b.extend(&good[first_len..]);
    let r = run("junk_between", &b).expect("blocks are found after junk");
    assert!(!r.is_empty());
    eprintln!("junk between the first block and the next: {} of {} samples", r.len(), full.len());

    // a mutation sweep: single bytes changed all over the file; any outcome but a panic
    let (mut errors, mut oks) = (0, 0);
    let step = (good.len() / 250).max(1);
    for at in (0..good.len()).step_by(step) {
        for flip in [0x01u8, 0xFF] {
            let mut b = good.clone();
            b[at] ^= flip;
            match run("mut", &b) {
                Ok(_) => oks += 1,
                Err(_) => errors += 1,
            }
        }
    }
    eprintln!("{} single-byte mutations: {errors} errors, {oks} decodes, no panic", errors + oks);

    // packets cut short at every length: errors, not panics
    let mut demux = ironav_formats::open_demuxer(&file).unwrap();
    let stream = demux.streams()[0].clone();
    let p = demux.read_packet().unwrap().unwrap();
    let mut dec = ironav_codec::new_decoder_for(&stream).unwrap();
    let step = (p.data.len() / 400).max(1);
    let mut cut_errors = 0;
    for n in (0..p.data.len()).step_by(step) {
        let cut = Packet { data: p.data[..n].to_vec(), ..p.clone() };
        if dec.decode(&cut).is_err() {
            cut_errors += 1;
        }
    }
    eprintln!("a packet cut at {} lengths: {cut_errors} errors", p.data.len() / step);
    assert!(cut_errors > 0);
    assert!(dec.decode(&p).is_ok(), "the decoder is unharmed by the damaged packets");
    let _ = std::fs::remove_dir_all(&dir);
}
