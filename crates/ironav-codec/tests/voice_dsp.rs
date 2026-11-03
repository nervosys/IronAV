//! Speech processing before a voice link's encoder: detection, push-to-talk trimming, level
//! control and noise suppression, at 16 kHz in 20 ms frames.

use ironav_codec::voice::{Agc, Denoiser, Gate, SpectrumMeter, Vad};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

/// Counts the allocations a thread makes while its `COUNTING` is set.
struct Counting;
thread_local! {
    static COUNTING: Cell<bool> = const { Cell::new(false) };
    static ALLOCS: Cell<usize> = const { Cell::new(0) };
}

fn count() {
    if COUNTING.with(Cell::get) {
        ALLOCS.with(|a| a.set(a.get() + 1));
    }
}

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, l: Layout) -> *mut u8 {
        count();
        System.alloc(l)
    }
    unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
        System.dealloc(p, l)
    }
    unsafe fn realloc(&self, p: *mut u8, l: Layout, n: usize) -> *mut u8 {
        count();
        System.realloc(p, l, n)
    }
}

#[global_allocator]
static A: Counting = Counting;

const RATE: f64 = 16_000.0;
const FRAME: usize = 320;

/// Speech-like at `gain`: a voiced tone whose pitch glides, in syllables.
fn voice(n: usize, gain: f64) -> Vec<f32> {
    let mut phase = 0f64;
    (0..n)
        .map(|i| {
            let t = i as f64 / RATE;
            let f0 = 140.0 + 40.0 * (2.0 * std::f64::consts::PI * 1.5 * t).sin();
            phase += 2.0 * std::f64::consts::PI * f0 / RATE;
            let env = (2.0 * std::f64::consts::PI * 4.0 * t).sin().abs();
            (gain * env * (0.3 * phase.sin() + 0.12 * (2.0 * phase).sin() + 0.06 * (3.0 * phase).sin() + 0.03 * (5.0 * phase).sin())) as f32
        })
        .collect()
}

/// White noise with peaks at `amp`.
fn noise(n: usize, amp: f32, seed: u32) -> Vec<f32> {
    let mut s = seed;
    (0..n)
        .map(|_| {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            ((s >> 8) as f32 / (1 << 23) as f32 - 1.0) * amp
        })
        .collect()
}

fn energy(x: &[f32]) -> f64 {
    x.iter().map(|v| (*v as f64) * (*v as f64)).sum()
}

fn level_db(x: &[f32]) -> f64 {
    10.0 * (energy(x) / x.len() as f64 + 1e-12).log10()
}

fn snr_db(clean: &[f32], got: &[f32]) -> f64 {
    let err: f64 = clean.iter().zip(got).map(|(a, b)| ((a - b) as f64).powi(2)).sum();
    10.0 * (energy(clean) / (err + 1e-12)).log10()
}

/// Half a second of quiet room noise, a second of speech over it, then a second of the noise.
fn press() -> Vec<f32> {
    let n = 16_000 * 5 / 2;
    let mut x = noise(n, 0.002, 3);
    let v = voice(16_000, 1.0);
    for (o, s) in x[8_000..24_000].iter_mut().zip(&v) {
        *o += s;
    }
    x
}

#[test]
fn vad_follows_speech() {
    let x = press();
    let mut vad = Vad::new(20.0, 200.0);
    let flags: Vec<bool> = x.chunks(FRAME).map(|f| vad.process(f)).collect();
    // frames 0-24 noise, 25-74 speech, 75-124 noise
    assert!(flags[..25].iter().all(|f| !f), "speech in the noise before: {flags:?}");
    let start = flags.iter().position(|f| *f).unwrap();
    assert!((25..=28).contains(&start), "speech found at frame {start}");
    assert!(flags[30..75].iter().filter(|f| **f).count() >= 40, "speech lost in the middle: {flags:?}");
    let end = flags.iter().rposition(|f| *f).unwrap();
    assert!((75..=88).contains(&end), "speech ends at frame {end}");
    let (level, floor) = vad.levels();
    assert!(level < -50.0 && floor < -50.0, "{level} {floor}");
}

#[test]
fn gate_trims_a_press_and_keeps_its_preroll() {
    let x = press();
    let mut gate = Gate::new(FRAME, 20.0, 3, 200.0);
    let mut sent = Vec::new();
    let mut first = None;
    for (i, f) in x.chunks(FRAME).enumerate() {
        let n = gate.push(f).unwrap();
        if n > 0 && first.is_none() {
            first = Some((i, n));
        }
        while let Some(out) = gate.next() {
            sent.extend_from_slice(out);
        }
    }
    let (at, n) = first.unwrap();
    assert!((25..=28).contains(&at), "started at frame {at}");
    assert_eq!(n, 4, "the start goes out with three frames before it");
    let frames = sent.len() / FRAME;
    assert!((50..=70).contains(&frames), "{frames} frames sent of {}", x.len() / FRAME);
    // what was sent is the input, in order, from the preroll on
    let from = (at - 3) * FRAME;
    assert_eq!(&sent[..], &x[from..from + sent.len()]);
    assert!(gate.push(&x[..100]).is_err());

    // a new press starts afresh
    gate.reset();
    assert_eq!(gate.push(&x[..FRAME]).unwrap(), 0);
}

#[test]
fn agc_brings_quiet_speech_to_its_target_without_clipping() {
    let x: Vec<f32> = voice(16_000 * 4, 0.03).iter().zip(noise(16_000 * 4, 0.0005, 9)).map(|(v, n)| v + n).collect();
    let mut vad = Vad::new(20.0, 200.0);
    let mut agc = Agc::new(-20.0, 30.0);
    let mut out = x.clone();
    for f in out.chunks_mut(FRAME) {
        let speech = vad.process(f);
        agc.process(f, speech);
    }
    let last = &out[16_000 * 3..];
    let before = level_db(&x[16_000 * 3..]);
    let after = level_db(last);
    // the syllables' pauses pull the long-run level below the speech level
    assert!(after - before > 15.0, "{before:.1} dB to {after:.1} dB, gain {:.1} dB", agc.gain_db());
    assert!((-27.0..-17.0).contains(&after), "{after:.1} dBFS");
    assert!(out.iter().all(|v| v.abs() <= 0.99));

    // too loud comes down at once, and never past full scale
    let loud = voice(16_000, 3.0);
    let mut agc = Agc::new(-20.0, 30.0);
    let mut out = loud.clone();
    for f in out.chunks_mut(FRAME) {
        agc.process(f, true);
    }
    assert!(agc.gain_db() < -5.0, "{}", agc.gain_db());
    assert!(out.iter().all(|v| v.abs() <= 0.99));
}

#[test]
fn denoiser_takes_out_steady_noise_and_passes_clean_speech() {
    let n = 16_000 * 4;
    let clean = voice(n, 1.0);
    let hiss = noise(n, 1.0, 5);
    // noise at 10 dB below the speech
    let k = (energy(&clean) / energy(&hiss) / 10.0).sqrt() as f32;
    let noisy: Vec<f32> = clean.iter().zip(&hiss).map(|(c, h)| c + k * h).collect();

    let mut dn = Denoiser::new(16_000, 80, -12.0).unwrap();
    assert_eq!(dn.delay_ms(), 5.0);
    let mut out = noisy.clone();
    for f in out.chunks_mut(FRAME) {
        dn.process(f).unwrap();
    }
    // a second to learn, then compare with the clean speech, allowing for the delay
    let (from, d) = (16_000, 80);
    let before = snr_db(&clean[from..n - d], &noisy[from..n - d]);
    let after = snr_db(&clean[from..n - d], &out[from + d..]);
    assert!(after > before + 3.0, "SNR {before:.1} dB to {after:.1} dB");

    let mut dn = Denoiser::new(16_000, 80, -12.0).unwrap();
    let mut out = clean.clone();
    for f in out.chunks_mut(FRAME) {
        dn.process(f).unwrap();
    }
    let kept = snr_db(&clean[from..n - d], &out[from + d..]);
    assert!(kept > 22.0, "clean speech comes out at {kept:.1} dB SNR");

    assert!(Denoiser::new(16_000, 77, -12.0).is_err());
    assert!(dn.process(&mut out[..100]).is_err());
}

#[test]
fn nothing_allocates_per_frame() {
    let x = press();
    let mut vad = Vad::new(20.0, 200.0);
    let mut gate = Gate::new(FRAME, 20.0, 3, 200.0);
    let mut agc = Agc::new(-20.0, 30.0);
    let mut dn = Denoiser::new(16_000, 80, -12.0).unwrap();
    let mut buf = vec![0f32; FRAME];
    let mut sent = 0;
    COUNTING.with(|c| c.set(true));
    for f in x.chunks(FRAME) {
        buf.copy_from_slice(f);
        dn.process(&mut buf).unwrap();
        let speech = vad.process(&buf);
        agc.process(&mut buf, speech);
        gate.push(&buf).unwrap();
        while gate.next().is_some() {
            sent += 1;
        }
    }
    COUNTING.with(|c| c.set(false));
    assert_eq!(ALLOCS.with(Cell::get), 0);
    assert!(sent > 0);
}

#[test]
fn the_spectrum_tells_a_tone_from_hiss() {
    let mut m = SpectrumMeter::default();
    let tone: Vec<f32> = (0..4000).map(|i| 0.5 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / 16_000.0).sin()).collect();
    let s = m.measure(&tone, 16_000);
    assert!((s.rms_dbfs - -9.0).abs() < 0.2 && (s.peak_dbfs - -6.0).abs() < 0.1, "{s:?}");
    assert!((s.centroid_hz - 1000.0).abs() < 60.0 && s.flatness < 0.05 && s.rolloff_hz < 1100.0, "{s:?}");
    let hiss = noise(4000, 0.5, 11);
    let s = m.measure(&hiss, 16_000);
    assert!((s.centroid_hz - 4000.0).abs() < 400.0 && s.flatness > 0.5 && s.rolloff_hz > 6000.0, "{s:?}");
    let s = m.measure(&[0.0; 100], 16_000);
    assert!(s.rms_dbfs < -100.0 && s.centroid_hz == 0.0, "{s:?}");
}

/// A fan: low-passed noise (so it is not hiss by its zero crossings) at `dbfs`.
fn fan(n: usize, dbfs: f64) -> Vec<f32> {
    let mut y = 0f32;
    let x: Vec<f32> = noise(n, 1.0, 13)
        .into_iter()
        .map(|v| {
            y = 0.9 * y + 0.1 * v;
            y
        })
        .collect();
    let k = 10f64.powf((dbfs - level_db(&x)) / 20.0) as f32;
    x.into_iter().map(|v| v * k).collect()
}

#[test]
fn vad_learns_steady_noise_at_the_start() {
    // three seconds of a fan at -31.5 dBFS, then two of speech over it, from a fresh detector
    let mut x = fan(16_000 * 5, -31.5);
    let zcr = x.windows(2).filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0)).count() as f64 / x.len() as f64;
    assert!(zcr < 0.35, "{zcr}");
    for (o, s) in x[16_000 * 3..].iter_mut().zip(voice(16_000 * 2, 1.0)) {
        *o += s;
    }
    let mut vad = Vad::new(20.0, 200.0);
    let flags: Vec<bool> = x.chunks(FRAME).map(|f| vad.process(f)).collect();
    // at most the steadiness window and the hangover read as speech
    let early = flags[..150].iter().filter(|f| **f).count();
    assert!(early <= 32, "{early} frames of the fan read as speech: {flags:?}");
    assert!(flags[35..150].iter().all(|f| !f), "the fan read as speech: {flags:?}");
    let (_, floor) = vad.levels();
    assert!(flags[155..250].iter().filter(|f| **f).count() >= 80, "speech over the fan lost: {flags:?}");
    assert!(floor > -40.0, "floor {floor}");

    // starting from the floor the last press ended with, the fan never reads as speech
    let mut vad = Vad::with_floor(20.0, 200.0, floor);
    assert!(x[..16_000 * 3].chunks(FRAME).all(|f| !vad.process(f)));
}

#[test]
fn vad_learns_a_fan_through_the_denoiser_at_48k() {
    // a voice link's chain: 48 kHz, 10 ms frames, the suppressor before the detector
    let mut x = fan(48_000 * 3, -31.5);
    let mut dn = Denoiser::new(48_000, 240, -12.0).unwrap();
    let mut vad = Vad::new(10.0, 200.0);
    let mut active = 0;
    for (i, f) in x.chunks_mut(480).enumerate() {
        dn.process(f).unwrap();
        if vad.process(f) {
            active += 1;
            assert!(i < 70, "the fan reads as speech at frame {i}");
        }
    }
    assert!(active <= 70, "{active}");
}
