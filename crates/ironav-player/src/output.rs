//! Audio output. The player writes interleaved `f32` frames into a queue that the sound device
//! drains; the number of frames the device has actually taken is the clock that video follows.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::Instant;

pub trait AudioOutput: Send + Sync {
    fn rate(&self) -> u32;
    fn channels(&self) -> u16;
    /// Human-readable device name, for status reports.
    fn name(&self) -> String;
    /// Frames waiting to be played.
    fn queued_frames(&self) -> usize;
    /// Frames the device has played since creation. Only ever grows, and stalls on underrun.
    fn consumed_frames(&self) -> u64;
    /// Queue interleaved samples (already at `rate()` and `channels()`).
    fn write(&self, interleaved: &[f32]);
    /// Throw away everything queued (used when seeking).
    fn clear(&self);
    fn set_playing(&self, on: bool);
    fn set_volume(&self, volume: f32);
    /// Peak amplitude (0..1, after volume) of the audio played in roughly the last 100 ms.
    fn level(&self) -> f32;
}

/// How much audio a ring holds, in seconds: the decoder keeps about half a second ahead, and a packet
/// can add a second or more (a long FLAC block).
const RING_SECONDS: usize = 4;

/// The queue between the decoder thread (one writer) and the thing that plays it (one reader), shared by the
/// real and the silent output. No locks: the sound device's callback never waits for the decoder.
///
/// `head` and `tail` count samples written and read, and only ever grow; a sample lives in slot `count % cap`.
/// The reader owns `tail` and the writer `head`. Clearing (a seek) is a third party's act, so it is
/// recorded in `discard` (everything before it is dead) instead of moving `tail`, which only the reader moves.
pub(crate) struct Ring {
    buf: Box<[AtomicU32]>,
    head: AtomicU64,
    tail: AtomicU64,
    discard: AtomicU64,
    playing: AtomicBool,
    /// An `f32`, as its bits.
    volume: AtomicU32,
    /// Frames played; silence from an underrun is not counted.
    consumed: AtomicU64,
    /// An `f32`, as its bits.
    level: AtomicU32,
    channels: usize,
}

impl Ring {
    pub fn new(rate: u32, channels: usize) -> Self {
        let channels = channels.max(1);
        let cap = (rate.max(1) as usize * channels * RING_SECONDS).max(channels * 1024);
        Self {
            buf: (0..cap / channels * channels).map(|_| AtomicU32::new(0)).collect(),
            head: AtomicU64::new(0),
            tail: AtomicU64::new(0),
            discard: AtomicU64::new(0),
            playing: AtomicBool::new(false),
            volume: AtomicU32::new(1.0f32.to_bits()),
            consumed: AtomicU64::new(0),
            level: AtomicU32::new(0),
            channels,
        }
    }

    /// Where the reader is, counting what `clear` has thrown away.
    fn read_at(&self) -> u64 {
        self.tail.load(Ordering::Acquire).max(self.discard.load(Ordering::Acquire))
    }

    /// Samples waiting to be played.
    fn queued_samples(&self) -> usize {
        self.head.load(Ordering::Acquire).saturating_sub(self.read_at()) as usize
    }

    pub fn queued_frames(&self) -> usize {
        self.queued_samples() / self.channels
    }

    pub fn consumed(&self) -> u64 {
        self.consumed.load(Ordering::Relaxed)
    }

    pub fn level(&self) -> f32 {
        f32::from_bits(self.level.load(Ordering::Relaxed))
    }

    pub fn set_playing(&self, on: bool) {
        self.playing.store(on, Ordering::Release);
    }

    pub fn set_volume(&self, v: f32) {
        self.volume.store(v.to_bits(), Ordering::Relaxed);
    }

    /// Queue interleaved samples (whole frames). What does not fit is dropped.
    pub fn write(&self, data: &[f32]) {
        let head = self.head.load(Ordering::Relaxed);
        let free = self.buf.len() - head.saturating_sub(self.read_at()) as usize;
        let n = data.len().min(free) / self.channels * self.channels;
        let cap = self.buf.len() as u64;
        for (i, s) in data[..n].iter().enumerate() {
            self.buf[((head + i as u64) % cap) as usize].store(s.to_bits(), Ordering::Relaxed);
        }
        self.head.store(head + n as u64, Ordering::Release);
    }

    /// Throw away everything queued. The writer must not be writing at the same time (a seek takes its
    /// lock first); the reader may be reading.
    pub fn clear(&self) {
        self.discard.fetch_max(self.head.load(Ordering::Acquire), Ordering::AcqRel);
    }

    /// Fill `out` with queued audio (volume applied), zero-filling on underrun. Returns frames played.
    /// Only one thread at a time may be the reader.
    pub fn pull(&self, out: &mut [f32]) -> usize {
        if !self.playing.load(Ordering::Acquire) {
            out.fill(0.0);
            self.set_level(self.level() * 0.9);
            return 0;
        }
        let tail = self.read_at();
        let avail = self.head.load(Ordering::Acquire).saturating_sub(tail) as usize;
        let n = out.len().min(avail) / self.channels * self.channels;
        let volume = f32::from_bits(self.volume.load(Ordering::Relaxed));
        let cap = self.buf.len() as u64;
        let mut peak = 0f32;
        for (i, o) in out[..n].iter_mut().enumerate() {
            *o = f32::from_bits(self.buf[((tail + i as u64) % cap) as usize].load(Ordering::Relaxed)) * volume;
            peak = peak.max(o.abs());
        }
        out[n..].fill(0.0);
        self.tail.store(tail + n as u64, Ordering::Release);
        // Hold the peak briefly so a poll between buffers still sees it.
        self.set_level(if n > 0 { peak.max(self.level() * 0.8) } else { self.level() * 0.8 });
        let frames = n / self.channels;
        self.consumed.fetch_add(frames as u64, Ordering::Relaxed);
        frames
    }

    fn set_level(&self, v: f32) {
        self.level.store(v.to_bits(), Ordering::Relaxed);
    }
}

/// An output with no sound device: it drains its queue against the wall clock, so playback
/// behaves exactly as with a real device, just silently. Used for headless machines, tests
/// and agents that only need the timeline.
pub struct NullOutput {
    rate: u32,
    channels: u16,
    ring: Ring,
    /// When the ring was last drained; also what makes one thread at a time the reader.
    last: Mutex<Instant>,
}

impl NullOutput {
    pub fn new(rate: u32, channels: u16) -> Self {
        Self { rate, channels, ring: Ring::new(rate, channels as usize), last: Mutex::new(Instant::now()) }
    }

    /// Consume what the wall clock says a device would have played since the last call.
    fn pump(&self) -> &Ring {
        let mut last = self.last.lock().unwrap();
        let now = Instant::now();
        let elapsed = now.duration_since(*last).as_secs_f64();
        *last = now;
        if self.ring.playing.load(Ordering::Acquire) {
            let want = (elapsed * self.rate as f64) as usize * self.channels as usize;
            let mut sink = vec![0f32; want.min(self.rate as usize * self.channels as usize)];
            if !sink.is_empty() {
                self.ring.pull(&mut sink);
            }
        } else if elapsed > 0.02 {
            self.ring.set_level(self.ring.level() * 0.8);
        }
        &self.ring
    }
}

impl AudioOutput for NullOutput {
    fn rate(&self) -> u32 {
        self.rate
    }
    fn channels(&self) -> u16 {
        self.channels
    }
    fn name(&self) -> String {
        "none (silent clock)".into()
    }
    fn queued_frames(&self) -> usize {
        self.pump().queued_frames()
    }
    fn consumed_frames(&self) -> u64 {
        self.pump().consumed()
    }
    fn write(&self, interleaved: &[f32]) {
        self.ring.write(interleaved);
    }
    fn clear(&self) {
        self.ring.clear();
    }
    fn set_playing(&self, on: bool) {
        self.pump();
        self.ring.set_playing(on);
    }
    fn set_volume(&self, volume: f32) {
        self.ring.set_volume(volume);
    }
    fn level(&self) -> f32 {
        self.pump().level()
    }
}

#[cfg(feature = "audio")]
pub use device::{CpalOutput, device_description};

#[cfg(feature = "audio")]
mod device {
    use super::{AudioOutput, Ring};
    use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
    use cpal::{FromSample, SampleFormat, SizedSample};
    use ironav_core::{Error, ErrorCode, Result};
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{mpsc, Arc};
    use std::thread::JoinHandle;

    /// Name of the default output device, if there is one.
    pub fn device_description() -> Option<String> {
        cpal::default_host().default_output_device().and_then(|d| d.name().ok())
    }

    /// Sound through the operating system's default output device. The stream lives on its own
    /// thread because some platforms' streams cannot be moved between threads.
    pub struct CpalOutput {
        ring: Arc<Ring>,
        rate: u32,
        channels: u16,
        name: String,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    /// The device's callback: no lock, no allocation once the scratch buffer has grown to the device's size.
    fn fill<T: SizedSample + FromSample<f32>>(ring: &Ring, data: &mut [T], scratch: &mut Vec<f32>) {
        scratch.resize(data.len(), 0.0);
        ring.pull(scratch);
        for (d, s) in data.iter_mut().zip(scratch.iter()) {
            *d = T::from_sample(*s);
        }
    }

    impl CpalOutput {
        pub fn open() -> Result<Self> {
            let unavailable = |m: String| Error::new(ErrorCode::HardwareUnavailable, m).hint("check that a sound device is connected, or play with audio set to \"none\"");
            let (tx, rx) = mpsc::channel::<std::result::Result<(u32, u16, String, Arc<Ring>), String>>();
            let stop = Arc::new(AtomicBool::new(false));
            let stop2 = stop.clone();
            let thread = std::thread::spawn(move || {
                let setup = || -> std::result::Result<(cpal::Stream, u32, u16, String, Arc<Ring>), String> {
                    let host = cpal::default_host();
                    let dev = host.default_output_device().ok_or("no default audio output device")?;
                    let name = dev.name().unwrap_or_else(|_| "unknown device".into());
                    let cfg = dev.default_output_config().map_err(|e| format!("{name}: {e}"))?;
                    let (rate, channels, format) = (cfg.sample_rate().0, cfg.channels(), cfg.sample_format());
                    let ring = Arc::new(Ring::new(rate, channels as usize));
                    let config: cpal::StreamConfig = cfg.into();
                    let err = |e| eprintln!("{}", serde_json::json!({ "event": "audio-error", "message": format!("{e}") }));
                    let r = ring.clone();
                    let mut scratch = Vec::new();
                    let stream = match format {
                        SampleFormat::F32 => dev.build_output_stream(&config, move |d: &mut [f32], _| fill(&r, d, &mut scratch), err, None),
                        SampleFormat::I16 => dev.build_output_stream(&config, move |d: &mut [i16], _| fill(&r, d, &mut scratch), err, None),
                        SampleFormat::U16 => dev.build_output_stream(&config, move |d: &mut [u16], _| fill(&r, d, &mut scratch), err, None),
                        SampleFormat::I32 => dev.build_output_stream(&config, move |d: &mut [i32], _| fill(&r, d, &mut scratch), err, None),
                        other => return Err(format!("{name}: sample format {other:?} is not supported")),
                    }
                    .map_err(|e| format!("{name}: {e}"))?;
                    stream.play().map_err(|e| format!("{name}: {e}"))?;
                    Ok((stream, rate, channels, name, ring))
                };
                match setup() {
                    Ok((stream, rate, channels, name, ring)) => {
                        let _ = tx.send(Ok((rate, channels, name, ring)));
                        while !stop2.load(Ordering::Relaxed) {
                            std::thread::park_timeout(std::time::Duration::from_millis(100));
                        }
                        drop(stream);
                    }
                    Err(e) => {
                        let _ = tx.send(Err(e));
                    }
                }
            });
            match rx.recv() {
                Ok(Ok((rate, channels, name, ring))) => Ok(Self { ring, rate, channels, name, stop, thread: Some(thread) }),
                Ok(Err(m)) => Err(unavailable(m)),
                Err(_) => Err(unavailable("the audio thread exited while opening the device".into())),
            }
        }
    }

    impl Drop for CpalOutput {
        fn drop(&mut self) {
            self.stop.store(true, Ordering::Relaxed);
            if let Some(t) = self.thread.take() {
                t.thread().unpark();
                let _ = t.join();
            }
        }
    }

    impl AudioOutput for CpalOutput {
        fn rate(&self) -> u32 {
            self.rate
        }
        fn channels(&self) -> u16 {
            self.channels
        }
        fn name(&self) -> String {
            self.name.clone()
        }
        fn queued_frames(&self) -> usize {
            self.ring.queued_frames()
        }
        fn consumed_frames(&self) -> u64 {
            self.ring.consumed()
        }
        fn write(&self, interleaved: &[f32]) {
            self.ring.write(interleaved);
        }
        fn clear(&self) {
            self.ring.clear();
        }
        fn set_playing(&self, on: bool) {
            self.ring.set_playing(on);
        }
        fn set_volume(&self, volume: f32) {
            self.ring.set_volume(volume);
        }
        fn level(&self) -> f32 {
            self.ring.level()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_silent_output_drains_in_real_time_and_only_while_playing() {
        let o = NullOutput::new(1000, 2);
        o.write(&vec![0.5; 2000]); // one second of stereo
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert_eq!(o.consumed_frames(), 0, "nothing plays until started");
        o.set_playing(true);
        std::thread::sleep(std::time::Duration::from_millis(200));
        let c = o.consumed_frames();
        assert!((120..=400).contains(&c), "about 200 frames in 200 ms, got {c}");
        assert!(o.level() > 0.4, "the peak of what played is visible");
        o.set_playing(false);
        let at = o.consumed_frames();
        std::thread::sleep(std::time::Duration::from_millis(100));
        assert_eq!(o.consumed_frames(), at, "paused output does not advance");
        o.clear();
        assert_eq!(o.queued_frames(), 0);
    }

    #[test]
    fn volume_scales_what_is_played_and_underrun_stalls_the_clock() {
        let r = Ring::new(1000, 1);
        r.set_playing(true);
        r.set_volume(0.5);
        r.write(&[1.0, -1.0]);
        let mut out = [9.0; 4];
        assert_eq!(r.pull(&mut out), 2);
        assert_eq!(out, [0.5, -0.5, 0.0, 0.0], "gain applied, remainder is silence");
        assert_eq!(r.consumed(), 2, "silence from an underrun is not counted as played");
    }

    #[test]
    fn the_ring_wraps_keeps_whole_frames_drops_what_does_not_fit_and_clears() {
        let r = Ring::new(1000, 2);
        r.set_playing(true);
        let mut out = vec![0.0f32; 1500];
        // many times around the ring, in odd-sized pieces, and every sample comes out in order
        let (mut next_in, mut next_out) = (0u32, 0u32);
        for round in 0..200 {
            let n = 2 * (1 + round % 37);
            let chunk: Vec<f32> = (0..n).map(|i| (next_in + i as u32) as f32).collect();
            r.write(&chunk);
            next_in += n as u32;
            let got = r.pull(&mut out[..2 * (1 + (round * 7) % 41)]) * 2;
            for s in &out[..got] {
                assert_eq!(*s, next_out as f32);
                next_out += 1;
            }
        }
        // a write that is too big is cut at a whole frame; the part that fits is kept
        let big = vec![1.0f32; 2 * 1000 * RING_SECONDS + 10];
        let before = r.queued_frames();
        r.write(&big);
        assert_eq!(r.queued_frames(), 1000 * RING_SECONDS, "full, not past it (had {before} queued)");
        // clearing empties it at once, even though the reader has not moved, and it fills again
        r.clear();
        assert_eq!(r.queued_frames(), 0);
        r.write(&[3.0, 4.0]);
        let mut one = [0.0; 2];
        assert_eq!(r.pull(&mut one), 1);
        assert_eq!(one, [3.0, 4.0], "what was cleared is not played");
    }

    #[test]
    fn a_writer_and_a_reader_on_two_threads_lose_and_reorder_nothing() {
        let r = std::sync::Arc::new(Ring::new(48000, 2));
        r.set_playing(true);
        let w = r.clone();
        let total = 2 * 48000 * 3;
        let writer = std::thread::spawn(move || {
            let mut n = 0u32;
            while (n as usize) < total {
                let k = 2 * 100;
                let chunk: Vec<f32> = (0..k).map(|i| (n + i as u32) as f32).collect();
                if w.queued_frames() * 2 + k <= 2 * 48000 {
                    w.write(&chunk);
                    n += k as u32;
                } else {
                    std::thread::yield_now();
                }
            }
        });
        let (mut expect, mut buf) = (0u32, vec![0.0f32; 480]);
        while (expect as usize) < total {
            let got = r.pull(&mut buf) * 2;
            for s in &buf[..got] {
                assert_eq!(*s, expect as f32);
                expect += 1;
            }
            if got == 0 {
                std::thread::yield_now();
            }
        }
        writer.join().unwrap();
    }
}
