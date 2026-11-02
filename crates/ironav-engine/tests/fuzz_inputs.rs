//! Mutation fuzzing of the demuxers and software decoders.
//!
//! Seeds are small valid files made by `ffmpeg` at run time, one per container and codec the
//! registry reads. Each seed is mutated (bit flips, byte runs of 0x00/0xFF/0x7F/0x80, truncation,
//! large length-like values, chunk duplication and deletion); every mutant is opened with
//! `ironav_formats::open_demuxer`, read to the end, and each stream is decoded with
//! `ironav_codec::new_decoder_for`. A case runs on its own thread under `catch_unwind` with a
//! timeout, so panics and hangs are both reported. The worker writes the current mutant to
//! `current.<ext>` before every case, so a process abort (for example an allocation failure)
//! leaves its reproducer behind.
//!
//! The full run is `#[ignore]`d; the smoke test runs a few mutants per seed with a fixed seed.
//! Output goes to `IRONAV_FUZZ_DIR` (default: the temp dir). Iterations per seed: `IRONAV_FUZZ_ITERS`
//! (default 2000 for the full run); base seed: `IRONAV_FUZZ_SEED`.

use ironav_core::Decoder;
use std::cell::RefCell;
use std::collections::BTreeMap;
use std::panic::{self, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{mpsc, Once};
use std::time::{Duration, Instant};

/// A case that takes longer than this is a hang.
const CASE_TIMEOUT: Duration = Duration::from_secs(10);
/// Packets read from one mutant before the case stops (bounds the work of a mutant that
/// invents packets forever; the stop is not a failure).
const MAX_PACKETS: usize = 100_000;

// ---------------------------------------------------------------- PRNG

/// xorshift64*: enough randomness for mutation, and the same sequence for the same seed.
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Rng(seed.max(1))
    }
    fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }
    /// A value in `0..n` (`0` when `n` is 0).
    fn below(&mut self, n: usize) -> usize {
        if n == 0 {
            0
        } else {
            (self.next_u64() % n as u64) as usize
        }
    }
}

fn hash_name(name: &str) -> u64 {
    name.bytes().fold(0xcbf2_9ce4_8422_2325u64, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3))
}

// ---------------------------------------------------------------- mutation

const BYTE_VALUES: [u8; 4] = [0x00, 0xFF, 0x7F, 0x80];
const LENGTH_VALUES: [u64; 8] = [0xFFFF_FFFF, 0x7FFF_FFFF, 0x8000_0000, 0x0FFF_FFFF, 0x0001_0000, 0xFFFF, 0x7FFF, 0xFFFF_FFFF_FFFF_FFFF];

/// Applies one to four random mutations. Half the positions fall in the first 512 bytes, where
/// the headers and length fields are.
fn mutate(rng: &mut Rng, src: &[u8]) -> Vec<u8> {
    let mut out = src.to_vec();
    for _ in 0..1 + rng.below(4) {
        if out.is_empty() {
            out.push(BYTE_VALUES[rng.below(4)]);
            continue;
        }
        let len = out.len();
        let pos = |rng: &mut Rng| if rng.below(2) == 0 { rng.below(len.min(512)) } else { rng.below(len) };
        match rng.below(7) {
            // bit flips
            0 => {
                for _ in 0..1 + rng.below(8) {
                    let p = pos(rng);
                    out[p] ^= 1 << rng.below(8);
                }
            }
            // byte runs of a boundary value
            1 => {
                let p = pos(rng);
                let v = BYTE_VALUES[rng.below(4)];
                for b in out.iter_mut().skip(p).take(1 + rng.below(4)) {
                    *b = v;
                }
            }
            // truncation
            2 => {
                let keep = rng.below(len);
                out.truncate(keep);
            }
            // a length-like value written over a field, big or little endian
            3 => {
                let v = LENGTH_VALUES[rng.below(LENGTH_VALUES.len())];
                let width = [2usize, 4, 8][rng.below(3)];
                let bytes: Vec<u8> = if rng.below(2) == 0 { v.to_be_bytes()[8 - width..].to_vec() } else { v.to_le_bytes()[..width].to_vec() };
                let p = pos(rng);
                for (i, b) in bytes.iter().enumerate() {
                    if let Some(slot) = out.get_mut(p + i) {
                        *slot = *b;
                    }
                }
            }
            // the same, inserted rather than written over
            4 => {
                let v = LENGTH_VALUES[rng.below(LENGTH_VALUES.len())];
                let bytes = if rng.below(2) == 0 { v.to_be_bytes().to_vec() } else { v.to_le_bytes().to_vec() };
                let p = pos(rng);
                out.splice(p..p, bytes);
            }
            // chunk duplication
            5 => {
                let start = rng.below(len);
                let n = (1 + rng.below(256)).min(len - start);
                let chunk = out[start..start + n].to_vec();
                let at = pos(rng);
                out.splice(at..at, chunk);
            }
            // chunk deletion
            _ => {
                let start = rng.below(len);
                let n = (1 + rng.below(64)).min(len - start);
                out.drain(start..start + n);
            }
        }
    }
    out
}

// ---------------------------------------------------------------- panics

thread_local! {
    static LAST_PANIC: RefCell<Option<String>> = const { RefCell::new(None) };
}

static HOOK: Once = Once::new();

/// Records a panic's location and message on the panicking thread instead of printing it.
fn install_hook() {
    HOOK.call_once(|| {
        panic::set_hook(Box::new(|info| {
            let at = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_else(|| "?".into());
            let msg = payload_text(info.payload());
            LAST_PANIC.with(|p| *p.borrow_mut() = Some(format!("{msg} at {at}")));
        }));
    });
}

fn payload_text(p: &(dyn std::any::Any + Send)) -> String {
    if let Some(s) = p.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = p.downcast_ref::<String>() {
        s.clone()
    } else {
        "non-string panic payload".into()
    }
}

// ---------------------------------------------------------------- one case

/// Opens, reads and decodes one file. Errors are the expected outcome for bad input and are
/// ignored; only panics and hangs are failures.
fn exercise(path: &Path) {
    let Ok(mut dmx) = ironav_formats::open_demuxer(path) else { return };
    let streams: Vec<ironav_core::StreamInfo> = dmx.streams().to_vec();
    let mut decoders: Vec<Option<Box<dyn Decoder>>> = streams.iter().map(|s| ironav_codec::new_decoder_for(s).ok()).collect();
    let mut packets = 0usize;
    while let Ok(Some(p)) = dmx.read_packet() {
        packets += 1;
        if packets > MAX_PACKETS {
            break;
        }
        if let Some(Some(d)) = decoders.get_mut(p.stream) {
            let _ = d.decode(&p);
        }
    }
    for d in decoders.iter_mut().flatten() {
        let _ = d.flush();
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Outcome {
    Ok,
    Panic(String),
    Hang,
}

/// Runs one case on a worker thread with a timeout.
fn run_case(path: &Path) -> (Outcome, Duration) {
    install_hook();
    let (tx, rx) = mpsc::channel();
    let owned = path.to_path_buf();
    let started = Instant::now();
    std::thread::spawn(move || {
        let r = panic::catch_unwind(AssertUnwindSafe(|| exercise(&owned)));
        let msg = match r {
            Ok(()) => None,
            Err(payload) => Some(LAST_PANIC.with(|p| p.borrow_mut().take()).unwrap_or_else(|| payload_text(&*payload))),
        };
        let _ = tx.send(msg);
    });
    let outcome = match rx.recv_timeout(CASE_TIMEOUT) {
        Ok(None) => Outcome::Ok,
        Ok(Some(m)) => Outcome::Panic(m),
        Err(mpsc::RecvTimeoutError::Timeout) => Outcome::Hang,
        Err(mpsc::RecvTimeoutError::Disconnected) => Outcome::Panic("worker thread died without a report".into()),
    };
    (outcome, started.elapsed())
}

// ---------------------------------------------------------------- seeds

const VIDEO_IN: &[&str] = &["-f", "lavfi", "-i", "testsrc2=size=64x48:rate=10:duration=0.5"];
const AUDIO_IN: &[&str] = &["-f", "lavfi", "-i", "sine=frequency=440:sample_rate=48000:duration=0.5"];

/// One seed: which lavfi sources feed it, the output options, and the file extension.
struct Seed {
    name: &'static str,
    video: bool,
    audio: bool,
    out: &'static [&'static str],
    ext: &'static str,
}

const SEEDS: &[Seed] = &[
    Seed { name: "wav-pcm16", video: false, audio: true, out: &["-c:a", "pcm_s16le"], ext: "wav" },
    Seed { name: "flac", video: false, audio: true, out: &["-c:a", "flac"], ext: "flac" },
    Seed { name: "y4m", video: true, audio: false, out: &["-pix_fmt", "yuv420p", "-f", "yuv4mpegpipe"], ext: "y4m" },
    Seed { name: "jpeg", video: true, audio: false, out: &["-frames:v", "1"], ext: "jpg" },
    Seed { name: "mp4-h264-aac", video: true, audio: true, out: &["-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac"], ext: "mp4" },
    Seed { name: "mp4-hevc", video: true, audio: false, out: &["-c:v", "libx265", "-preset", "ultrafast"], ext: "mp4" },
    Seed { name: "mp4-mpeg4", video: true, audio: false, out: &["-c:v", "mpeg4"], ext: "mp4" },
    Seed { name: "mp4-mpeg2", video: true, audio: false, out: &["-c:v", "mpeg2video"], ext: "mp4" },
    Seed { name: "mp4-vp9", video: true, audio: false, out: &["-c:v", "libvpx-vp9", "-deadline", "realtime", "-cpu-used", "8"], ext: "mp4" },
    Seed { name: "mp4-av1", video: true, audio: false, out: &["-c:v", "libaom-av1", "-cpu-used", "8", "-usage", "realtime"], ext: "mp4" },
    Seed { name: "mp4-ac3", video: false, audio: true, out: &["-c:a", "ac3"], ext: "mp4" },
    Seed { name: "mp4-alac", video: false, audio: true, out: &["-c:a", "alac"], ext: "mp4" },
    Seed { name: "mp4-mp2", video: false, audio: true, out: &["-c:a", "mp2"], ext: "mp4" },
    Seed { name: "mov-prores", video: true, audio: false, out: &["-c:v", "prores_ks", "-profile:v", "0"], ext: "mov" },
    Seed { name: "mov-mjpeg", video: true, audio: false, out: &["-c:v", "mjpeg"], ext: "mov" },
    Seed { name: "mov-pcm", video: false, audio: true, out: &["-c:a", "pcm_s16le"], ext: "mov" },
    Seed { name: "mkv-h264", video: true, audio: true, out: &["-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac"], ext: "mkv" },
    Seed { name: "mkv-vp8-opus", video: true, audio: true, out: &["-c:v", "libvpx", "-deadline", "realtime", "-c:a", "libopus"], ext: "mkv" },
    Seed { name: "mkv-av1", video: true, audio: false, out: &["-c:v", "libaom-av1", "-cpu-used", "8", "-usage", "realtime"], ext: "mkv" },
    Seed { name: "mkv-mpeg4", video: true, audio: false, out: &["-c:v", "mpeg4"], ext: "mkv" },
    Seed { name: "mkv-mpeg2", video: true, audio: false, out: &["-c:v", "mpeg2video"], ext: "mkv" },
    Seed { name: "mkv-vorbis", video: false, audio: true, out: &["-c:a", "libvorbis"], ext: "mkv" },
    Seed { name: "mkv-mp3", video: false, audio: true, out: &["-c:a", "libmp3lame"], ext: "mkv" },
    Seed { name: "mkv-flac", video: false, audio: true, out: &["-c:a", "flac"], ext: "mkv" },
    Seed { name: "mkv-pcm-f32", video: false, audio: true, out: &["-c:a", "pcm_f32le"], ext: "mkv" },
    Seed { name: "mkv-ac3", video: false, audio: true, out: &["-c:a", "ac3"], ext: "mkv" },
    Seed { name: "webm-vp9-opus", video: true, audio: true, out: &["-c:v", "libvpx-vp9", "-deadline", "realtime", "-cpu-used", "8", "-c:a", "libopus"], ext: "webm" },
    Seed { name: "webm-av1", video: true, audio: false, out: &["-c:v", "libaom-av1", "-cpu-used", "8", "-usage", "realtime"], ext: "webm" },
    Seed { name: "mp3", video: false, audio: true, out: &["-c:a", "libmp3lame"], ext: "mp3" },
    Seed { name: "mp2", video: false, audio: true, out: &["-c:a", "mp2"], ext: "mp2" },
    Seed { name: "adts-aac", video: false, audio: true, out: &["-c:a", "aac", "-f", "adts"], ext: "aac" },
    Seed { name: "ac3-raw", video: false, audio: true, out: &["-c:a", "ac3", "-f", "ac3"], ext: "ac3" },
    Seed { name: "ogg-opus", video: false, audio: true, out: &["-c:a", "libopus"], ext: "ogg" },
    Seed { name: "ogg-vorbis", video: false, audio: true, out: &["-c:a", "libvorbis"], ext: "ogg" },
    Seed { name: "ogg-flac", video: false, audio: true, out: &["-c:a", "flac"], ext: "ogg" },
    Seed { name: "h264-annexb", video: true, audio: false, out: &["-c:v", "libx264", "-preset", "ultrafast", "-f", "h264"], ext: "h264" },
    Seed { name: "hevc-annexb", video: true, audio: false, out: &["-c:v", "libx265", "-preset", "ultrafast", "-f", "hevc"], ext: "hevc" },
    Seed { name: "m2v", video: true, audio: false, out: &["-c:v", "mpeg2video", "-f", "mpeg2video"], ext: "m2v" },
    Seed { name: "m1v", video: true, audio: false, out: &["-c:v", "mpeg1video", "-r", "25", "-f", "mpeg1video"], ext: "m1v" },
    Seed { name: "mpegps-mpeg2-mp2", video: true, audio: true, out: &["-c:v", "mpeg2video", "-c:a", "mp2", "-f", "mpeg"], ext: "mpg" },
    Seed { name: "mpegps-ac3", video: true, audio: true, out: &["-c:v", "mpeg2video", "-c:a", "ac3", "-f", "mpeg"], ext: "vob" },
    Seed { name: "mpegts-h264-aac", video: true, audio: true, out: &["-c:v", "libx264", "-preset", "ultrafast", "-c:a", "aac", "-f", "mpegts"], ext: "ts" },
    Seed { name: "mpegts-mpeg2-mp2", video: true, audio: true, out: &["-c:v", "mpeg2video", "-c:a", "mp2", "-f", "mpegts"], ext: "ts" },
];

/// Makes the seed file with ffmpeg. `false` when ffmpeg cannot make it (the seed is skipped).
fn make_seed(dir: &Path, seed: &Seed) -> Option<PathBuf> {
    let out = dir.join(format!("{}.{}", seed.name, seed.ext));
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-v", "error", "-y"]);
    if seed.video {
        cmd.args(VIDEO_IN);
    }
    if seed.audio {
        cmd.args(AUDIO_IN);
    }
    if !seed.video {
        cmd.arg("-vn");
    }
    if !seed.audio {
        cmd.arg("-an");
    }
    cmd.args(seed.out).arg(&out);
    let ok = cmd.output().map(|o| o.status.success()).unwrap_or(false);
    (ok && out.metadata().map(|m| m.len() > 0).unwrap_or(false)).then_some(out)
}

fn ffmpeg_available() -> bool {
    Command::new("ffmpeg").arg("-version").output().map(|o| o.status.success()).unwrap_or(false)
}

// ---------------------------------------------------------------- driver

struct Failure {
    seed: &'static str,
    kind: &'static str,
    message: String,
    repro: PathBuf,
}

#[derive(Default)]
struct Tally {
    cases: usize,
    panics: usize,
    hangs: usize,
    slow: usize,
}

fn fuzz_dir() -> PathBuf {
    let d = std::env::var_os("IRONAV_FUZZ_DIR").map(PathBuf::from).unwrap_or_else(|| std::env::temp_dir().join("ironav-fuzz"));
    std::fs::create_dir_all(&d).unwrap();
    d
}

fn env_num(name: &str, default: u64) -> u64 {
    std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Mutates every seed `iters` times and reports the failures. Returns them and the tallies.
fn fuzz(iters: usize, base_seed: u64) -> (Vec<Failure>, BTreeMap<&'static str, Tally>) {
    let dir = fuzz_dir();
    let seed_dir = dir.join("seeds");
    std::fs::create_dir_all(&seed_dir).unwrap();
    let mut failures = Vec::new();
    let mut tallies: BTreeMap<&'static str, Tally> = BTreeMap::new();
    let mut made = 0;
    for seed in SEEDS {
        let Some(path) = make_seed(&seed_dir, seed) else {
            eprintln!("fuzz: ffmpeg could not make seed {}; skipped", seed.name);
            continue;
        };
        made += 1;
        let original = std::fs::read(&path).unwrap();
        let mut rng = Rng::new(base_seed ^ hash_name(seed.name));
        let current = dir.join(format!("current-{}.{}", seed.name, seed.ext));
        let t = tallies.entry(seed.name).or_default();
        for i in 0..iters {
            let mutant = mutate(&mut rng, &original);
            std::fs::write(&current, &mutant).unwrap();
            let (outcome, took) = run_case(&current);
            t.cases += 1;
            if took > Duration::from_secs(2) {
                t.slow += 1;
            }
            let kind = match &outcome {
                Outcome::Ok => continue,
                Outcome::Panic(_) => {
                    t.panics += 1;
                    "panic"
                }
                Outcome::Hang => {
                    t.hangs += 1;
                    "hang"
                }
            };
            let repro = dir.join(format!("crash-{kind}-{}-{i}.{}", seed.name, seed.ext));
            std::fs::write(&repro, &mutant).unwrap();
            let message = match outcome {
                Outcome::Panic(m) => m,
                _ => format!("no result within {} s", CASE_TIMEOUT.as_secs()),
            };
            eprintln!("fuzz: {kind} in {} case {i}: {message} -> {}", seed.name, repro.display());
            failures.push(Failure { seed: seed.name, kind, message, repro });
        }
    }
    assert!(made > 0 || !ffmpeg_available(), "ffmpeg is on the PATH but no seed could be made");
    (failures, tallies)
}

fn report(failures: &[Failure], tallies: &BTreeMap<&'static str, Tally>) {
    eprintln!("\n{:<20} {:>8} {:>7} {:>7} {:>6}", "seed", "cases", "panics", "hangs", "slow");
    let (mut cases, mut panics, mut hangs) = (0, 0, 0);
    for (name, t) in tallies {
        eprintln!("{:<20} {:>8} {:>7} {:>7} {:>6}", name, t.cases, t.panics, t.hangs, t.slow);
        cases += t.cases;
        panics += t.panics;
        hangs += t.hangs;
    }
    eprintln!("total: {cases} cases, {panics} panics, {hangs} hangs");
    // one line per distinct message, with the first reproducer
    let mut unique: BTreeMap<(&str, &str), (usize, &str, &Path)> = BTreeMap::new();
    for f in failures {
        let e = unique.entry((f.kind, f.message.as_str())).or_insert((0, f.seed, f.repro.as_path()));
        e.0 += 1;
    }
    for ((kind, msg), (n, seed, repro)) in &unique {
        eprintln!("  {kind} x{n} ({seed}): {msg}\n    e.g. {}", repro.display());
    }
}

/// Smoke: a fixed seed and a few mutants per seed, so CI runs the harness on every build.
#[test]
fn fuzz_smoke_fixed_seed() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    let (failures, tallies) = fuzz(6, 0x1234_5678);
    report(&failures, &tallies);
    assert!(failures.is_empty(), "{} panics or hangs; reproducers in {}", failures.len(), fuzz_dir().display());
}

/// Full run: `cargo test --release -p ironav-engine --test fuzz_inputs -- --ignored --nocapture`.
#[test]
#[ignore]
fn fuzz_full() {
    if !ffmpeg_available() {
        eprintln!("ffmpeg not on PATH; skipping");
        return;
    }
    let iters = env_num("IRONAV_FUZZ_ITERS", 2000) as usize;
    let seed = env_num("IRONAV_FUZZ_SEED", 0x9E37_79B9_7F4A_7C15);
    let started = Instant::now();
    let (failures, tallies) = fuzz(iters, seed);
    report(&failures, &tallies);
    eprintln!("elapsed: {:.1} s", started.elapsed().as_secs_f64());
    assert!(failures.is_empty(), "{} panics or hangs; reproducers in {}", failures.len(), fuzz_dir().display());
}

// ---------------------------------------------------------------- regressions

fn regression_file(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/data/fuzz").join(name)
}

/// One minimised input per crash class the fuzzer found; each must open, read and decode without
/// a panic or a hang (errors are fine).
#[test]
fn regression_inputs_do_not_panic_or_hang() {
    install_hook();
    for name in [
        // y4m: a lossy conversion left a multi-byte character as the first character of a token
        "y4m-bad-utf8-token.y4m",
        // mpeg4: an escape code read again inside an escape (run-level table index past the end)
        "mpeg4-nested-escape.mp4",
        // hevc: picture sizes that are not a multiple of the minimum coding block
        "hevc-partial-ctb.hevc",
        "hevc-partial-cu.hevc",
    ] {
        let (outcome, _) = run_case(&regression_file(name));
        assert_eq!(outcome, Outcome::Ok, "{name}");
    }
}

/// A y4m picture past the size limit is refused when the file is opened, before any frame is
/// allocated from its header.
#[test]
fn y4m_picture_past_the_size_limit_is_refused_at_open() {
    let path = regression_file("y4m-huge-picture.y4m");
    let err = ironav_formats::open_demuxer(&path).err().expect("refused");
    assert!(err.to_string().contains("larger than the supported size"), "{err}");
}
