//! Taps: decoded pictures or sound from a live session, handed to code in the same process (a
//! detector, a speech model) as they are decoded, rather than as observations or over a sidecar's
//! pipe. A tap takes pictures at a chosen rate, scaled and converted to RGB, BGR or grey (letterboxed
//! to an exact size if asked), or mono sound at a chosen rate in windows of a chosen length, with
//! its level, spectrum and share of speech if asked.
//!
//! The session never waits for a tap: each keeps a short queue, and when its reader falls behind
//! the oldest item is dropped and counted (`queue` 1 is "latest wins"). Each item carries a
//! sequence number, so a gap is visible, its media time (from the source's timestamps: for RTSP,
//! the RTP clock from the start of the stream) and the wall-clock time it was decoded.

use ironav_codec::voice::{SpectrumMeter, StreamResampler, Vad};
use ironav_core::{AudioFrame, Error, ErrorCode, MediaType, Result, VideoFrame};
use serde::Deserialize;
use std::collections::VecDeque;
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, SystemTime};

/// How a tap's pictures are laid out.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PixelLayout {
    /// Three bytes a pixel, R G B.
    Rgb,
    /// Three bytes a pixel, B G R (OpenCV's order).
    Bgr,
    /// One byte a pixel, the luma (BT.601 weights of the RGB).
    Gray,
}

/// What a tap takes.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TapConfig {
    /// `video` or `audio`.
    pub media: TapMedia,
    /// The stream to tap; the first analyzed one of the kind when not given.
    #[serde(default)]
    pub stream: Option<usize>,
    /// Pictures a second at most; every decoded picture when not given.
    #[serde(default)]
    pub fps: Option<f64>,
    /// The picture fits inside `width` x `height` (aspect kept); either alone scales to it.
    #[serde(default)]
    pub width: Option<u32>,
    #[serde(default)]
    pub height: Option<u32>,
    /// Pad the scaled picture with black to exactly `width` x `height`, centred.
    #[serde(default)]
    pub letterbox: bool,
    #[serde(default = "default_layout")]
    pub layout: PixelLayout,
    /// Sound: the sample rate of the mono windows.
    #[serde(default = "default_rate")]
    pub rate: u32,
    /// Sound: each window's length, and how far apart windows start (overlapping when shorter).
    #[serde(default = "default_window")]
    pub window_ms: u32,
    #[serde(default)]
    pub hop_ms: Option<u32>,
    /// Sound: measure each window's [`AudioFeatures`].
    #[serde(default)]
    pub features: bool,
    /// Items kept for a slow reader before the oldest is dropped.
    #[serde(default = "default_queue")]
    pub queue: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TapMedia {
    Video,
    Audio,
}

fn default_layout() -> PixelLayout {
    PixelLayout::Rgb
}
fn default_rate() -> u32 {
    16_000
}
fn default_window() -> u32 {
    1000
}
fn default_queue() -> usize {
    1
}

impl TapConfig {
    pub fn video() -> Self {
        Self { media: TapMedia::Video, stream: None, fps: None, width: None, height: None, letterbox: false, layout: PixelLayout::Rgb, rate: 16_000, window_ms: 1000, hop_ms: None, features: false, queue: 1 }
    }

    pub fn audio() -> Self {
        Self { media: TapMedia::Audio, ..Self::video() }
    }

    pub fn media_type(&self) -> MediaType {
        match self.media {
            TapMedia::Video => MediaType::Video,
            TapMedia::Audio => MediaType::Audio,
        }
    }

    pub fn check(&self) -> Result<()> {
        if let Some(f) = self.fps {
            if !(f > 0.0 && f <= 1000.0) {
                return Err(Error::invalid_arg(format!("a tap's fps must be above 0 and at most 1000, not {f}")));
            }
        }
        for d in [self.width, self.height].into_iter().flatten() {
            if !(2..=8192).contains(&d) {
                return Err(Error::invalid_arg(format!("a tap's width and height must be 2 to 8192, not {d}")));
            }
        }
        if self.letterbox && (self.width.is_none() || self.height.is_none()) {
            return Err(Error::invalid_arg("letterbox needs both width and height"));
        }
        if !(8000..=192_000).contains(&self.rate) {
            return Err(Error::invalid_arg(format!("a tap's rate must be 8000 to 192000, not {}", self.rate)));
        }
        if !(10..=60_000).contains(&self.window_ms) || self.hop_ms.is_some_and(|h| h == 0 || h > self.window_ms) {
            return Err(Error::invalid_arg("a tap's window_ms must be 10 to 60000, and hop_ms 1 to window_ms"));
        }
        if !(1..=1024).contains(&self.queue) {
            return Err(Error::invalid_arg(format!("a tap's queue must hold 1 to 1024 items, not {}", self.queue)));
        }
        Ok(())
    }
}

/// What a tap gives.
#[derive(Debug, Clone)]
pub enum TapData {
    /// Rows top to bottom, no padding, in the layout asked for.
    Video { width: u32, height: u32, layout: PixelLayout, pixels: Vec<u8> },
    /// Mono samples in -1 to 1, and what they are like when the tap measures it.
    Audio { rate: u32, samples: Vec<f32>, features: Option<AudioFeatures> },
}

/// What a window of sound is like.
#[derive(Debug, Clone, Copy, PartialEq, Default, serde::Serialize)]
pub struct AudioFeatures {
    pub rms_dbfs: f32,
    pub peak_dbfs: f32,
    /// The power-weighted mean frequency: low for a hum or a voice, high for hiss or breaking glass.
    pub centroid_hz: f32,
    /// Below this frequency lies 85% of the power.
    pub rolloff_hz: f32,
    /// Near 1 for noise, near 0 for a tone.
    pub flatness: f32,
    /// The share of the window's 20 ms frames a speech detector (energy over an adaptive noise
    /// floor, not hiss) took for speech, from 0 to 1.
    pub speech: f32,
}

#[derive(Debug, Clone)]
pub struct TapFrame {
    /// Counts every item the tap made, given or dropped, from 0.
    pub seq: u64,
    pub stream: usize,
    /// The media time of the picture, or of a window's first sample, in seconds.
    pub t: f64,
    /// When it was decoded, as Unix time.
    pub decoded_at: SystemTime,
    pub data: TapData,
}

/// A tap's counts.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct TapStats {
    pub made: u64,
    pub taken: u64,
    pub dropped: u64,
    pub queued: usize,
}

#[derive(Default)]
struct State {
    queue: VecDeque<TapFrame>,
    cap: usize,
    stats: TapStats,
    /// The reader let go of the tap.
    closed: bool,
    /// The session stopped feeding it, and why if not at the end of the stream.
    ended: Option<Option<String>>,
    /// The stream it is fed from, once attached.
    stream: Option<usize>,
}

struct Shared {
    state: Mutex<State>,
    ready: Condvar,
}

fn lock(s: &Shared) -> std::sync::MutexGuard<'_, State> {
    s.state.lock().unwrap_or_else(|e| e.into_inner())
}

/// The reading end of a tap. Dropping it stops the tap.
pub struct Tap {
    shared: Arc<Shared>,
}

impl Tap {
    /// The next item, waiting up to `wait` for one. `Ok(None)` when none came in time; an error
    /// once the session has stopped feeding the tap and everything it made has been taken.
    pub fn recv_timeout(&self, wait: Duration) -> Result<Option<TapFrame>> {
        let deadline = std::time::Instant::now() + wait;
        let mut st = lock(&self.shared);
        loop {
            if let Some(f) = st.queue.pop_front() {
                st.stats.taken += 1;
                return Ok(Some(f));
            }
            if let Some(why) = &st.ended {
                return Err(match why {
                    None => Error::new(ErrorCode::NotFound, "the tapped stream has ended"),
                    Some(w) => Error::new(ErrorCode::InvalidArgument, w.clone()),
                });
            }
            let now = std::time::Instant::now();
            if now >= deadline {
                return Ok(None);
            }
            st = self.shared.ready.wait_timeout(st, deadline - now).unwrap_or_else(|e| e.into_inner()).0;
        }
    }

    /// The next item if one is waiting.
    pub fn try_recv(&self) -> Result<Option<TapFrame>> {
        self.recv_timeout(Duration::ZERO)
    }

    pub fn stats(&self) -> TapStats {
        let st = lock(&self.shared);
        TapStats { queued: st.queue.len(), ..st.stats }
    }

    /// The stream it is fed from, once the session has attached it.
    pub fn stream(&self) -> Option<usize> {
        lock(&self.shared).stream
    }
}

impl Drop for Tap {
    fn drop(&mut self) {
        let mut st = lock(&self.shared);
        st.closed = true;
        st.queue.clear();
    }
}

/// The session's end of a tap: shown each decoded frame of its stream.
pub struct TapFeeder {
    pub(crate) config: TapConfig,
    shared: Arc<Shared>,
    stream: usize,
    seq: u64,
    /// Video: when the next picture is due.
    next_t: Option<f64>,
    /// Audio: rate conversion to mono at the tap's rate, and the samples not yet in a window.
    resampler: Option<StreamResampler>,
    pending: VecDeque<f32>,
    /// The media time of `pending`'s first sample.
    pending_t: Option<f64>,
    mono: Vec<f32>,
    pulled: Vec<f32>,
    /// Audio features: the spectrum, and the speech detector run over 20 ms frames as they come,
    /// with each frame's start time and verdict.
    meter: Option<SpectrumMeter>,
    vad: Option<Vad>,
    vad_buf: Vec<f32>,
    vad_t: f64,
    flags: VecDeque<(f64, bool)>,
}

/// A tap and its feeder, unattached.
pub fn tap(config: TapConfig) -> Result<(Tap, TapFeeder)> {
    config.check()?;
    let shared = Arc::new(Shared { state: Mutex::new(State { cap: config.queue, ..State::default() }), ready: Condvar::new() });
    let feeder = TapFeeder { config, shared: shared.clone(), stream: 0, seq: 0, next_t: None, resampler: None, pending: VecDeque::new(), pending_t: None, mono: Vec::new(), pulled: Vec::new(), meter: None, vad: None, vad_buf: Vec::new(), vad_t: 0.0, flags: VecDeque::new() };
    Ok((Tap { shared }, feeder))
}

impl TapFeeder {
    /// Whether the reader still holds the tap.
    pub fn open(&self) -> bool {
        !lock(&self.shared).closed
    }

    pub(crate) fn attach(&mut self, stream: usize) {
        self.stream = stream;
        lock(&self.shared).stream = Some(stream);
    }

    /// Stops feeding the tap, saying why when it is not the end of the stream.
    pub(crate) fn fail(self, why: String) {
        lock(&self.shared).ended = Some(Some(why));
        // dropping it wakes the reader
    }

    fn give(&mut self, t: f64, data: TapData) {
        let f = TapFrame { seq: self.seq, stream: self.stream, t, decoded_at: SystemTime::now(), data };
        self.seq += 1;
        let mut st = lock(&self.shared);
        st.stats.made += 1;
        if st.queue.len() >= st.cap {
            st.queue.pop_front();
            st.stats.dropped += 1;
        }
        st.queue.push_back(f);
        drop(st);
        self.shared.ready.notify_all();
    }

    /// A decoded picture at `t`.
    pub(crate) fn video(&mut self, v: &VideoFrame, t: f64) {
        if let Some(fps) = self.config.fps {
            let step = 1.0 / fps;
            if let Some(next) = self.next_t {
                // a picture a little early counts as on time, so 30 into 15 takes every other one
                if t + 0.25 * step < next {
                    return;
                }
                // far behind (a gap in the stream): start again from here
                self.next_t = Some(if t - next > step { t + step } else { next + step });
            } else {
                self.next_t = Some(t + step);
            }
        }
        let c = &self.config;
        let (w, h, rgb) = ironav_player::rgb_frame(v, c.width, c.height);
        let (mut w, mut h, mut px) = (w, h, rgb);
        if c.letterbox {
            let (bw, bh) = (c.width.unwrap(), c.height.unwrap());
            if (w, h) != (bw, bh) {
                let mut boxed = vec![0u8; bw as usize * bh as usize * 3];
                let (x0, y0) = (((bw - w.min(bw)) / 2) as usize, ((bh - h.min(bh)) / 2) as usize);
                let row = w.min(bw) as usize * 3;
                for y in 0..h.min(bh) as usize {
                    let to = ((y0 + y) * bw as usize + x0) * 3;
                    boxed[to..to + row].copy_from_slice(&px[y * w as usize * 3..y * w as usize * 3 + row]);
                }
                (w, h, px) = (bw, bh, boxed);
            }
        }
        let pixels = match c.layout {
            PixelLayout::Rgb => px,
            PixelLayout::Bgr => {
                for p in px.chunks_exact_mut(3) {
                    p.swap(0, 2);
                }
                px
            }
            PixelLayout::Gray => px.chunks_exact(3).map(|p| ((p[0] as u32 * 77 + p[1] as u32 * 150 + p[2] as u32 * 29 + 128) >> 8) as u8).collect(),
        };
        let layout = c.layout;
        self.give(t, TapData::Video { width: w, height: h, layout, pixels });
    }

    /// Decoded sound starting at `t`.
    pub(crate) fn audio(&mut self, a: &AudioFrame, t: f64) {
        let ch = a.channels.max(1) as usize;
        let n = a.samples.len() / ch;
        if n == 0 {
            return;
        }
        let rate = self.config.rate;
        if self.resampler.as_ref().is_none_or(|r| r.rates().0 != a.sample_rate) {
            match StreamResampler::new(a.sample_rate, rate, 1, 4096) {
                Ok(r) => self.resampler = Some(r),
                Err(_) => return,
            }
            self.pending.clear();
            self.pending_t = None;
        }
        self.mono.clear();
        self.mono.extend(a.samples.chunks_exact(ch).map(|s| (s.iter().sum::<f64>() / ch as f64) as f32));
        let r = self.resampler.as_mut().unwrap();
        let delay = r.delay_ms() / 1000.0;
        // the resampler's output is not offset: what is waiting, what it holds ready and its
        // look-ahead should end where this frame starts; if not, the stream jumped
        let expect = self.pending_t.map(|p| p + (self.pending.len() + r.ready()) as f64 / rate as f64);
        if expect.is_none_or(|e| (e - (t - delay)).abs() > 0.1) {
            self.pending.clear();
            self.pending_t = Some(t);
            self.vad_buf.clear();
            self.flags.clear();
            self.vad_t = t;
        }
        let features = self.config.features;
        let vad_frame = rate as usize / 50;
        for chunk in self.mono.chunks(4096) {
            if r.push(chunk).is_err() {
                return;
            }
            self.pulled.resize(r.ready(), 0.0);
            let got = r.pull(&mut self.pulled);
            self.pending.extend(&self.pulled[..got]);
            if features {
                let vad = self.vad.get_or_insert_with(|| Vad::new(20.0, 200.0));
                for &v in &self.pulled[..got] {
                    self.vad_buf.push(v);
                    if self.vad_buf.len() == vad_frame {
                        self.flags.push_back((self.vad_t, vad.process(&self.vad_buf)));
                        self.vad_buf.clear();
                        self.vad_t += 0.02;
                    }
                }
            }
        }
        let window = (rate as u64 * self.config.window_ms as u64 / 1000) as usize;
        let hop = (rate as u64 * self.config.hop_ms.unwrap_or(self.config.window_ms) as u64 / 1000).max(1) as usize;
        while self.pending.len() >= window {
            let start = self.pending_t.unwrap_or(t);
            let samples: Vec<f32> = self.pending.iter().take(window).copied().collect();
            let drop = hop.min(self.pending.len());
            self.pending.drain(..drop);
            self.pending_t = Some(start + drop as f64 / rate as f64);
            let features = features.then(|| {
                let s = self.meter.get_or_insert_with(SpectrumMeter::default).measure(&samples, rate);
                let end = start + window as f64 / rate as f64;
                let inside: Vec<bool> = self.flags.iter().filter(|(ft, _)| *ft >= start - 0.001 && *ft + 0.02 <= end + 0.001).map(|f| f.1).collect();
                let speech = if inside.is_empty() { 0.0 } else { inside.iter().filter(|s| **s).count() as f32 / inside.len() as f32 };
                let next = start + drop as f64 / rate as f64;
                while self.flags.front().is_some_and(|(ft, _)| *ft < next - 0.001) {
                    self.flags.pop_front();
                }
                AudioFeatures { rms_dbfs: s.rms_dbfs, peak_dbfs: s.peak_dbfs, centroid_hz: s.centroid_hz, rolloff_hz: s.rolloff_hz, flatness: s.flatness, speech }
            });
            self.give(start.max(0.0), TapData::Audio { rate, samples, features });
        }
    }
}

impl Drop for TapFeeder {
    fn drop(&mut self) {
        let mut st = lock(&self.shared);
        if st.ended.is_none() {
            st.ended = Some(None);
        }
        drop(st);
        self.shared.ready.notify_all();
    }
}
