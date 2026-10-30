//! A voice link's pieces: Opus in short frames coded one at a time into the caller's buffer, and
//! the streaming resampler.

use ironav_codec::opus::{opus_head, OpusDecoder, OpusEncoder};
use ironav_codec::voice::StreamResampler;
use ironav_core::{Decoder, Frame, Packet, StreamParams};
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

/// Speech-like: a voiced tone whose pitch glides, in syllables, with a little noise.
fn voice(n: usize) -> Vec<f32> {
    let mut s = 7u32;
    let mut phase = 0f64;
    (0..n)
        .map(|i| {
            let t = i as f64 / 48_000.0;
            let f0 = 140.0 + 40.0 * (2.0 * std::f64::consts::PI * 1.5 * t).sin();
            phase += 2.0 * std::f64::consts::PI * f0 / 48_000.0;
            let env = (2.0 * std::f64::consts::PI * 4.0 * t).sin().abs();
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            let noise = ((s >> 8) as f64 / (1 << 23) as f64 - 1.0) * 0.01;
            (env * (0.3 * phase.sin() + 0.12 * (2.0 * phase).sin() + 0.06 * (3.0 * phase).sin()) + noise) as f32
        })
        .collect()
}

fn snr(a: &[f32], b: &[f64]) -> f64 {
    let (mut sig, mut err) = (0f64, 0f64);
    for (x, y) in a.iter().zip(b) {
        sig += (*x as f64) * (*x as f64);
        err += (*x as f64 - y) * (*x as f64 - y);
    }
    10.0 * (sig / err.max(1e-30)).log10()
}

/// Codes `input` frame by frame and decodes it again: the decoded samples, aligned with the input.
fn round_trip(ms: f64, bitrate: u32, input: &[f32]) -> (Vec<f64>, OpusEncoder) {
    let mut enc = OpusEncoder::new(48_000, 1, bitrate).unwrap().with_frame_ms(ms).unwrap();
    let mut dec = OpusDecoder::new(&opus_head(1), &StreamParams::Audio { sample_rate: 48_000, channels: 1 }).unwrap();
    let n = enc.frame_samples();
    let mut buf = vec![0u8; enc.packet_bytes()];
    let mut out = Vec::new();
    for (k, f) in input.chunks_exact(n).enumerate() {
        let len = enc.encode_frame_into(f, &mut buf).unwrap();
        let p = Packet { stream: 0, pts: (k * n) as i64, duration: n as i64, data: buf[..len].to_vec() };
        for f in dec.decode(&p).unwrap() {
            let Frame::Audio(a) = f else { unreachable!() };
            out.extend(a.samples);
        }
    }
    (out, enc)
}

#[test]
fn short_frames_round_trip_and_say_their_delay() {
    let input = voice(48_000);
    // measured: 18.6, 20.1, 20.2 and 11.9 dB
    for (ms, delay, min_snr) in [(20.0, 22.5, 17.0), (10.0, 12.5, 18.0), (5.0, 7.5, 18.0), (2.5, 5.0, 10.0)] {
        let (out, enc) = round_trip(ms, 32_000, &input);
        assert_eq!(enc.delay_ms(), delay);
        assert_eq!(enc.frame_samples() as f64, 48.0 * ms);
        // the decoder drops the look-ahead as the pre-skip, so what comes out lines up with what
        // went in, a look-ahead short of the frames given
        let lost = enc.delay_samples() - enc.frame_samples();
        assert_eq!(out.len(), input.len() - lost, "{ms} ms");
        let s = snr(&input[..out.len()], &out);
        eprintln!("{ms} ms at 32 kb/s: {s:.1} dB, {} bytes a packet", enc.packet_bytes());
        assert!(s > min_snr, "{ms} ms: {s:.1} dB");
    }
}

#[test]
fn a_frame_is_refused_in_the_wrong_size_or_buffer() {
    let mut enc = OpusEncoder::new(48_000, 1, 24_000).unwrap().with_frame_ms(10.0).unwrap();
    assert_eq!(enc.packet_bytes(), 30);
    let mut buf = [0u8; 64];
    assert!(enc.encode_frame_into(&[0.0; 479], &mut buf).is_err());
    assert!(enc.encode_frame_into(&[0.0; 480], &mut buf[..10]).is_err());
    // digital silence is three bytes, its TOC saying CELT, full band, 10 ms
    assert_eq!(enc.encode_frame_into(&[0.0; 480], &mut buf).unwrap(), 3);
    assert_eq!(buf[0] >> 3, 30);
    assert!(OpusEncoder::new(48_000, 1, 24_000).unwrap().with_frame_ms(7.0).is_err());
}

#[test]
fn coding_a_frame_allocates_nothing() {
    let input = voice(48_000);
    for ms in [2.5, 5.0, 10.0, 20.0] {
        for channels in [1usize, 2] {
            let mut enc = OpusEncoder::new(48_000, channels, 24_000 * channels as u32).unwrap().with_frame_ms(ms).unwrap();
            let n = enc.frame_samples();
            let mut buf = vec![0u8; enc.packet_bytes()];
            let pcm: Vec<f32> = input.iter().flat_map(|&v| std::iter::repeat_n(v, channels)).collect();
            let frames: Vec<&[f32]> = pcm.chunks_exact(n * channels).collect();
            // the first frame may size the buffers
            enc.encode_frame_into(frames[0], &mut buf).unwrap();
            ALLOCS.with(|a| a.set(0));
            COUNTING.with(|c| c.set(true));
            for f in &frames[1..] {
                enc.encode_frame_into(f, &mut buf).unwrap();
            }
            COUNTING.with(|c| c.set(false));
            assert_eq!(ALLOCS.with(Cell::get), 0, "{ms} ms, {channels} channels");
        }
    }
}

#[test]
fn a_16_khz_link_allocates_nothing_per_frame() {
    // 16 kHz 16-bit in, to 48 kHz, to 10 ms Opus frames, as a phone would send them
    let input: Vec<i16> = voice(48_000).iter().step_by(3).map(|v| (v * 32767.0) as i16).collect();
    let mut up = StreamResampler::new(16_000, 48_000, 1, 160).unwrap();
    let mut enc = OpusEncoder::new(48_000, 1, 24_000).unwrap().with_frame_ms(10.0).unwrap();
    let mut wide = vec![0i16; 4 * 480];
    let mut pending = Vec::with_capacity(2 * 480);
    let mut packet = vec![0u8; enc.packet_bytes()];
    let mut packets = 0;
    for (k, f) in input.chunks_exact(160).enumerate() {
        if k == 2 {
            ALLOCS.with(|a| a.set(0));
            COUNTING.with(|c| c.set(true));
        }
        up.push_i16(f).unwrap();
        let n = up.pull_i16(&mut wide);
        pending.extend_from_slice(&wide[..n]);
        while pending.len() >= 480 {
            enc.encode_frame_into_i16(&pending[..480], &mut packet).unwrap();
            pending.drain(..480);
            packets += 1;
        }
    }
    COUNTING.with(|c| c.set(false));
    assert_eq!(ALLOCS.with(Cell::get), 0);
    assert!(packets >= 95, "{packets}");
    // the delay these two add
    eprintln!("resampler {} ms + encoder {} ms", up.delay_ms(), enc.delay_ms());
    assert!(up.delay_ms() + enc.delay_ms() <= 13.5);
}
