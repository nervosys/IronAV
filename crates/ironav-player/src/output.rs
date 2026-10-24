//! Audio output. The player writes interleaved `f32` frames into a queue that the sound device
//! drains; the number of frames the device has actually taken is the clock that video follows.

use std::collections::VecDeque;
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

/// Queue state shared by the real and the silent output.
pub(crate) struct Ring {
    pub queue: VecDeque<f32>,
    pub playing: bool,
    pub volume: f32,
    pub consumed: u64,
    pub level: f32,
    pub channels: usize,
}

impl Ring {
    pub fn new(channels: usize) -> Self {
        Self { queue: VecDeque::new(), playing: false, volume: 1.0, consumed: 0, level: 0.0, channels }
    }

    /// Fill `out` with queued audio (volume applied), zero-filling on underrun. Returns frames played.
    pub fn pull(&mut self, out: &mut [f32]) -> usize {
        if !self.playing {
            out.fill(0.0);
            self.level *= 0.9;
            return 0;
        }
        let n = out.len().min(self.queue.len()) / self.channels * self.channels;
        let mut peak = 0f32;
        for (o, s) in out.iter_mut().zip(self.queue.drain(..n)) {
            *o = s * self.volume;
            peak = peak.max(o.abs());
        }
        out[n..].fill(0.0);
        // Hold the peak briefly so a poll between buffers still sees it.
        self.level = if n > 0 { peak.max(self.level * 0.8) } else { self.level * 0.8 };
        let frames = n / self.channels;
        self.consumed += frames as u64;
        frames
    }
}

/// An output with no sound device: it drains its queue against the wall clock, so playback
/// behaves exactly as with a real device, just silently. Used for headless machines, tests
/// and agents that only need the timeline.
pub struct NullOutput {
    rate: u32,
    channels: u16,
    state: Mutex<(Ring, Instant)>,
}

impl NullOutput {
    pub fn new(rate: u32, channels: u16) -> Self {
        Self { rate, channels, state: Mutex::new((Ring::new(channels as usize), Instant::now())) }
    }

    /// Consume what the wall clock says a device would have played since the last call.
    fn pump<R>(&self, f: impl FnOnce(&mut Ring) -> R) -> R {
        let mut g = self.state.lock().unwrap();
        let now = Instant::now();
        let elapsed = now.duration_since(g.1).as_secs_f64();
        g.1 = now;
        if g.0.playing {
            let want = (elapsed * self.rate as f64) as usize * self.channels as usize;
            let mut sink = vec![0f32; want.min(self.rate as usize * self.channels as usize)];
            if !sink.is_empty() {
                g.0.pull(&mut sink);
            }
        } else if elapsed > 0.02 {
            g.0.level *= 0.8;
        }
        f(&mut g.0)
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
        self.pump(|r| r.queue.len() / r.channels)
    }
    fn consumed_frames(&self) -> u64 {
        self.pump(|r| r.consumed)
    }
    fn write(&self, interleaved: &[f32]) {
        self.pump(|r| r.queue.extend(interleaved.iter().copied()));
    }
    fn clear(&self) {
        self.pump(|r| r.queue.clear());
    }
    fn set_playing(&self, on: bool) {
        self.pump(|r| r.playing = on);
    }
    fn set_volume(&self, volume: f32) {
        self.pump(|r| r.volume = volume);
    }
    fn level(&self) -> f32 {
        self.pump(|r| r.level)
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
    use std::sync::{mpsc, Arc, Mutex};
    use std::thread::JoinHandle;

    /// Name of the default output device, if there is one.
    pub fn device_description() -> Option<String> {
        cpal::default_host().default_output_device().and_then(|d| d.name().ok())
    }

    /// Sound through the operating system's default output device. The stream lives on its own
    /// thread because some platforms' streams cannot be moved between threads.
    pub struct CpalOutput {
        ring: Arc<Mutex<Ring>>,
        rate: u32,
        channels: u16,
        name: String,
        stop: Arc<AtomicBool>,
        thread: Option<JoinHandle<()>>,
    }

    fn fill<T: SizedSample + FromSample<f32>>(ring: &Arc<Mutex<Ring>>, data: &mut [T], scratch: &mut Vec<f32>) {
        scratch.resize(data.len(), 0.0);
        if let Ok(mut r) = ring.lock() {
            r.pull(scratch);
        } else {
            scratch.fill(0.0);
        }
        for (d, s) in data.iter_mut().zip(scratch.iter()) {
            *d = T::from_sample(*s);
        }
    }

    impl CpalOutput {
        pub fn open() -> Result<Self> {
            let unavailable = |m: String| Error::new(ErrorCode::HardwareUnavailable, m).hint("check that a sound device is connected, or play with audio set to \"none\"");
            let (tx, rx) = mpsc::channel::<std::result::Result<(u32, u16, String, Arc<Mutex<Ring>>), String>>();
            let stop = Arc::new(AtomicBool::new(false));
            let stop2 = stop.clone();
            let thread = std::thread::spawn(move || {
                let setup = || -> std::result::Result<(cpal::Stream, u32, u16, String, Arc<Mutex<Ring>>), String> {
                    let host = cpal::default_host();
                    let dev = host.default_output_device().ok_or("no default audio output device")?;
                    let name = dev.name().unwrap_or_else(|_| "unknown device".into());
                    let cfg = dev.default_output_config().map_err(|e| format!("{name}: {e}"))?;
                    let (rate, channels, format) = (cfg.sample_rate().0, cfg.channels(), cfg.sample_format());
                    let ring = Arc::new(Mutex::new(Ring::new(channels as usize)));
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

        fn ring<R>(&self, f: impl FnOnce(&mut Ring) -> R) -> R {
            f(&mut self.ring.lock().unwrap())
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
            self.ring(|r| r.queue.len() / r.channels)
        }
        fn consumed_frames(&self) -> u64 {
            self.ring(|r| r.consumed)
        }
        fn write(&self, interleaved: &[f32]) {
            self.ring(|r| r.queue.extend(interleaved.iter().copied()));
        }
        fn clear(&self) {
            self.ring(|r| r.queue.clear());
        }
        fn set_playing(&self, on: bool) {
            self.ring(|r| r.playing = on);
        }
        fn set_volume(&self, volume: f32) {
            self.ring(|r| r.volume = volume);
        }
        fn level(&self) -> f32 {
            self.ring(|r| r.level)
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
        let mut r = Ring::new(1);
        r.playing = true;
        r.volume = 0.5;
        r.queue.extend([1.0, -1.0]);
        let mut out = [9.0; 4];
        assert_eq!(r.pull(&mut out), 2);
        assert_eq!(out, [0.5, -0.5, 0.0, 0.0], "gain applied, remainder is silence");
        assert_eq!(r.consumed, 2, "silence from an underrun is not counted as played");
    }
}
