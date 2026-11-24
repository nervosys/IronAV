//! IronAV playback engine.
//!
//! A [`Player`] opens a media file and decodes it on two background threads, one for audio and
//! one for video, each with its own demuxer so neither can stall the other (the demuxer that
//! probed the file at open goes to the video thread, so the file is indexed once for it). Audio goes to an
//! [`AudioOutput`]; the number of audio frames the device has actually played is the master
//! clock, and video frames are shown when that clock reaches their timestamp. A file without
//! audio (or after its audio ends) is timed by the wall clock instead.
//!
//! The same object serves every front end: the terminal player, the GUI, and the JSON
//! command interface agents use ([`Player::command`]). Nothing here draws or reads input.

mod color;
mod command;
mod output;
mod pipeline;
mod snapshot;

pub use output::{AudioOutput, NullOutput};
#[cfg(feature = "audio")]
pub use output::{device_description, CpalOutput};
pub use pipeline::{open_video_decoder, open_video_decoder_with};
pub use color::{Colorimetry, Matrix, YuvToRgb};
pub use snapshot::{base64, rgb_frame, rgb_frame_reference, to_png};

use ironav_core::{CodecId, Error, ErrorCode, MediaType, Result, StreamInfo, StreamParams, VideoFrame};
use ironav_formats::open_demuxer_lazy;
use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

/// How sound is produced.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AudioMode {
    /// The default output device, or a silent clock if there is none (a warning says so).
    Auto,
    /// Decode the audio and keep time with it, but make no sound.
    Silent,
    /// Ignore audio tracks entirely.
    Off,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields, default)]
pub struct OpenOptions {
    pub audio: AudioMode,
    /// `auto`, `off`, `require` or a backend name, as for conversion jobs.
    pub hwaccel: String,
    /// Start position in seconds.
    pub start: f64,
    /// Begin playing at once. When false the player opens paused on the first frame.
    pub autoplay: bool,
    /// Start again from the beginning when the end is reached.
    pub looping: bool,
    /// Linear gain, 0 to 2.
    pub volume: f32,
}

impl Default for OpenOptions {
    fn default() -> Self {
        Self { audio: AudioMode::Auto, hwaccel: "auto".into(), start: 0.0, autoplay: true, looping: false, volume: 1.0 }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct VideoInfo {
    pub codec: &'static str,
    pub width: u32,
    pub height: u32,
    pub fps: f64,
    /// `software` or the hardware backend decoding the stream.
    pub decoder: &'static str,
}

#[derive(Debug, Clone, Serialize)]
pub struct AudioInfo {
    pub codec: &'static str,
    pub sample_rate: u32,
    pub channels: u16,
    /// Output device name, or a note that playback is silent.
    pub device: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct MediaInfo {
    pub path: String,
    pub format: &'static str,
    pub duration: Option<f64>,
    pub video: Option<VideoInfo>,
    pub audio: Option<AudioInfo>,
    /// Things that did not work out, such as an audio track that cannot be decoded.
    pub warnings: Vec<String>,
}

/// A decoded picture and the media time at which it should appear.
pub struct TimedFrame {
    pub time: f64,
    pub frame: VideoFrame,
}

/// How exactly a seek lands.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SeekMode {
    /// The first picture shown is the one on screen at the target: the video is decoded from the
    /// keyframe before it up to the target.
    #[default]
    Accurate,
    /// The first picture shown is the keyframe at or before the target, which needs no decoding up
    /// to the target. For scrubbing; follow it with an accurate seek to the same position.
    Keyframe,
}

pub(crate) enum Cmd {
    Seek { to: f64, gen: u64, mode: SeekMode },
    Stop,
}

/// What a decoding thread sleeps on when it has nothing to do: rung by commands and by whatever
/// frees it to work (a picture taken from a full queue, playing again), so no thread polls.
#[derive(Default)]
pub(crate) struct Bell {
    n: Mutex<u64>,
    cv: Condvar,
}

impl Bell {
    /// The ring count; read it before looking at what may have changed, and give it to [`Bell::wait`].
    pub fn count(&self) -> u64 {
        *self.n.lock().unwrap()
    }

    pub fn ring(&self) {
        *self.n.lock().unwrap() += 1;
        self.cv.notify_all();
    }

    /// Sleep until the bell has rung since `seen`, or `timeout` has passed.
    pub fn wait(&self, seen: u64, timeout: Duration) {
        let g = self.n.lock().unwrap();
        let _ = self.cv.wait_timeout_while(g, timeout, |n| *n == seen);
    }
}

#[derive(Default)]
pub(crate) struct AudioState {
    pub gen: u64,
    pub ended: bool,
    pub ready: bool,
    pub end_time: f64,
}

#[derive(Default)]
pub(crate) struct VideoQueue {
    pub gen: u64,
    pub frames: VecDeque<Arc<TimedFrame>>,
    pub bytes: usize,
    pub ended: bool,
    pub ready: bool,
    pub end_time: f64,
    pub front_shown: bool,
    pub shown: u64,
    pub skipped: u64,
    pub last: Option<Arc<TimedFrame>>,
}

impl VideoQueue {
    /// Remove the front picture, keeping the byte count right.
    fn pop(&mut self) {
        if let Some(f) = self.frames.pop_front() {
            self.bytes = self.bytes.saturating_sub(f.frame.planes.iter().map(Vec::len).sum::<usize>());
        }
        self.front_shown = false;
    }

    /// Drop pictures whose successor is already due at `pos`. Pictures nobody looked at count
    /// as skipped.
    pub fn discard_stale(&mut self, pos: f64) {
        while self.frames.len() >= 2 && self.frames[1].time <= pos + 1e-4 {
            if !self.front_shown {
                self.skipped += 1;
            }
            self.pop();
        }
    }
}

pub(crate) struct Clock {
    /// Intent: the user wants it playing.
    pub playing: bool,
    /// Both pipelines have delivered data for the current position.
    pub ready: bool,
    pub running: bool,
    pub anchor_media: f64,
    pub anchor_consumed: u64,
    pub anchor_wall: Instant,
    pub audio_driven: bool,
    pub gen: u64,
    pub volume: f32,
    pub muted: bool,
    pub looping: bool,
}

pub(crate) struct Inner {
    pub info: MediaInfo,
    pub out: Option<Arc<dyn AudioOutput>>,
    pub has_video: bool,
    pub has_audio: bool,
    pub frame_duration: f64,
    pub hwaccel: String,
    pub clock: Mutex<Clock>,
    pub audio: Mutex<AudioState>,
    pub video: Mutex<VideoQueue>,
    pub error: Mutex<Option<Error>>,
    pub shutdown: AtomicBool,
    pub video_bell: Bell,
    pub audio_bell: Bell,
    /// Called when a new picture is queued or playback fails, so a window can repaint at once.
    pub waker: Mutex<Option<Waker>>,
}

/// A callback that wakes whoever is showing the pictures.
pub type Waker = Arc<dyn Fn() + Send + Sync>;

impl Inner {
    pub fn fail(&self, e: Error) {
        {
            let mut g = self.error.lock().unwrap();
            g.get_or_insert(e);
        }
        self.wake();
    }

    pub fn wake(&self) {
        let w = self.waker.lock().unwrap().clone();
        if let Some(w) = w {
            w();
        }
    }

    fn position_locked(&self, c: &Clock) -> f64 {
        if !c.running {
            return c.anchor_media;
        }
        if c.audio_driven {
            if let Some(out) = &self.out {
                let played = out.consumed_frames().saturating_sub(c.anchor_consumed);
                return c.anchor_media + played as f64 / out.rate() as f64;
            }
        }
        c.anchor_media + c.anchor_wall.elapsed().as_secs_f64()
    }

    /// Start (or restart) the clock from `c.anchor_media` right now.
    fn start_clock(&self, c: &mut Clock) {
        c.anchor_wall = Instant::now();
        c.audio_driven = false;
        if let Some(out) = &self.out {
            let drained = {
                let a = self.audio.lock().unwrap();
                a.ended && out.queued_frames() == 0
            };
            if !drained {
                c.anchor_consumed = out.consumed_frames();
                c.audio_driven = true;
                out.set_playing(true);
            }
        }
        c.running = true;
    }

    fn stop_clock(&self, c: &mut Clock) {
        c.anchor_media = self.position_locked(c);
        c.running = false;
        if let Some(out) = &self.out {
            out.set_playing(false);
        }
    }

    /// Move the state machine forward: finish buffering, hand the clock from audio to the wall
    /// clock when the audio runs out, and detect the end. Cheap enough to call on every query.
    pub fn update(&self) -> Option<f64> {
        let mut c = self.clock.lock().unwrap();
        if self.error.lock().unwrap().is_some() {
            if c.running {
                self.stop_clock(&mut c);
            }
            return None;
        }
        let (a_ready, a_ended, a_end) = {
            let a = self.audio.lock().unwrap();
            (a.ready || !self.has_audio, a.ended || !self.has_audio, a.end_time)
        };
        let (v_ready, v_ended, v_end) = {
            let v = self.video.lock().unwrap();
            (v.ready || !self.has_video, v.ended || !self.has_video, v.end_time)
        };
        if !c.ready && a_ready && v_ready {
            c.ready = true;
            if c.playing {
                self.start_clock(&mut c);
            }
        }
        if c.running && c.audio_driven && a_ended {
            if let Some(out) = &self.out {
                if out.queued_frames() == 0 {
                    // The audio has all been played: carry on by the wall clock.
                    c.anchor_media = self.position_locked(&c);
                    c.anchor_wall = Instant::now();
                    c.audio_driven = false;
                }
            }
        }
        let pos = self.position_locked(&c);
        if c.ready && a_ended && v_ended {
            let end = a_end.max(v_end);
            let drained = self.out.as_ref().is_none_or(|o| !c.audio_driven || o.queued_frames() == 0);
            if drained && pos >= end - 1e-3 {
                c.anchor_media = end;
                if c.running {
                    c.running = false;
                    if let Some(out) = &self.out {
                        out.set_playing(false);
                    }
                }
                return Some(end);
            }
        }
        Some(pos)
    }
}

pub struct Player {
    pub(crate) inner: Arc<Inner>,
    tx: Vec<Sender<Cmd>>,
    threads: Mutex<Vec<JoinHandle<()>>>,
}

/// Why playback is where it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    Playing,
    Paused,
    /// Waiting for the first audio and video after opening or seeking.
    Buffering,
    Ended,
    Error,
}

fn pick(streams: &[StreamInfo], media: MediaType) -> Option<&StreamInfo> {
    streams.iter().find(|s| s.params.media_type() == media)
}

impl Player {
    pub fn open(path: &str, opts: &OpenOptions) -> Result<Player> {
        if !(0.0..=2.0).contains(&opts.volume) || !opts.volume.is_finite() {
            return Err(Error::invalid_arg("volume must be between 0 and 2"));
        }
        if !opts.start.is_finite() || opts.start < 0.0 {
            return Err(Error::invalid_arg("start must be zero or more seconds"));
        }
        if !Path::new(path).exists() {
            return Err(Error::new(ErrorCode::NotFound, format!("'{path}' does not exist")).hint("check the path"));
        }
        // The probe is kept: the video thread reads from it, so the index it built is used.
        let probe = open_demuxer_lazy(Path::new(path))?;
        let format = probe.format_name();
        let streams = probe.streams().to_vec();
        let mut probe = Some(probe);

        let mut warnings = Vec::new();
        // Video: needs a decoder (software, or a GPU one); built now so a failure is reported at open.
        let mut video = None;
        if let Some(s) = pick(&streams, MediaType::Video) {
            match pipeline::open_video_decoder(s, &opts.hwaccel) {
                Ok((dec, name)) => video = Some((s.clone(), dec, name)),
                Err(e) => warnings.push(format!("video track ({}) cannot be played: {}", s.codec.name(), e.message)),
            }
        }
        let mut audio = None;
        if opts.audio != AudioMode::Off {
            if let Some(s) = pick(&streams, MediaType::Audio) {
                match ironav_codec::new_decoder_for(s) {
                    Ok(dec) => audio = Some((s.clone(), dec)),
                    Err(_) => warnings.push(format!("audio track ({}) cannot be decoded here, so it is skipped", s.codec.name())),
                }
            }
        }
        if video.is_none() && audio.is_none() {
            return Err(Error::new(ErrorCode::UnsupportedCodec, "nothing in this file can be played")
                .hint(if warnings.is_empty() { "the file has no audio or video stream".to_string() } else { warnings.join("; ") }));
        }

        let out: Option<Arc<dyn AudioOutput>> = match (&audio, opts.audio) {
            (Some((s, _)), mode) => {
                let StreamParams::Audio { sample_rate, channels } = s.params else { unreachable!("audio stream") };
                Some(pipeline::open_output(mode, sample_rate, channels, &mut warnings))
            }
            _ => None,
        };
        if let Some(o) = &out {
            o.set_volume(opts.volume);
        }

        let fps = |s: &StreamInfo| match s.params {
            StreamParams::Video { frame_rate, .. } if frame_rate.num > 0 => frame_rate.as_f64(),
            _ => 25.0,
        };
        let vinfo = video.as_ref().map(|(s, _, name)| {
            let StreamParams::Video { width, height, .. } = s.params else { unreachable!("video stream") };
            VideoInfo { codec: s.codec.name(), width, height, fps: fps(s), decoder: name }
        });
        let ainfo = audio.as_ref().zip(out.as_ref()).map(|((s, _), o)| {
            let StreamParams::Audio { sample_rate, channels } = s.params else { unreachable!("audio stream") };
            AudioInfo { codec: s.codec.name(), sample_rate, channels, device: o.name() }
        });
        let duration = [video.as_ref().map(|v| &v.0), audio.as_ref().map(|a| &a.0)]
            .into_iter()
            .flatten()
            .filter_map(|s| s.duration_secs())
            .fold(None, |m: Option<f64>, d| Some(m.map_or(d, |m| m.max(d))));
        let info = MediaInfo { path: path.to_string(), format, duration, video: vinfo, audio: ainfo, warnings };

        let start = duration.map_or(opts.start, |d| opts.start.min(d));
        let inner = Arc::new(Inner {
            frame_duration: video.as_ref().map_or(0.0, |(s, _, _)| 1.0 / fps(s)),
            has_video: video.is_some(),
            hwaccel: opts.hwaccel.clone(),
            has_audio: audio.is_some(),
            info,
            out,
            clock: Mutex::new(Clock {
                playing: opts.autoplay,
                ready: false,
                running: false,
                anchor_media: start,
                anchor_consumed: 0,
                anchor_wall: Instant::now(),
                audio_driven: false,
                gen: 1,
                volume: opts.volume,
                muted: false,
                looping: opts.looping,
            }),
            audio: Mutex::new(AudioState { gen: 1, ..Default::default() }),
            video: Mutex::new(VideoQueue { gen: 1, ..Default::default() }),
            error: Mutex::new(None),
            shutdown: AtomicBool::new(false),
            video_bell: Bell::default(),
            audio_bell: Bell::default(),
            waker: Mutex::new(None),
        });

        let (mut tx, mut threads) = (Vec::new(), Vec::new());
        if let Some((s, dec, _)) = video {
            let (t, h) = pipeline::spawn_video(inner.clone(), path.to_string(), s, dec, probe.take(), start);
            tx.push(t);
            threads.push(h);
        }
        if let Some((s, dec)) = audio {
            let (t, h) = pipeline::spawn_audio(inner.clone(), path.to_string(), s, dec, probe.take(), start);
            tx.push(t);
            threads.push(h);
        }
        Ok(Player { inner, tx, threads: Mutex::new(threads) })
    }

    pub fn info(&self) -> &MediaInfo {
        &self.inner.info
    }

    /// Current media position in seconds.
    pub fn position(&self) -> f64 {
        self.inner.update().unwrap_or_else(|| self.inner.clock.lock().unwrap().anchor_media)
    }

    pub fn state(&self) -> State {
        let pos = self.inner.update();
        if self.inner.error.lock().unwrap().is_some() {
            return State::Error;
        }
        let c = self.inner.clock.lock().unwrap();
        if !c.ready {
            return State::Buffering;
        }
        let (a_ended, v_ended) = (self.inner.audio.lock().unwrap().ended, self.inner.video.lock().unwrap().ended);
        let all_ended = (a_ended || !self.inner.has_audio) && (v_ended || !self.inner.has_video);
        let end = self.inner.audio.lock().unwrap().end_time.max(self.inner.video.lock().unwrap().end_time);
        if all_ended && !c.running && pos.is_some_and(|p| p >= end - 1e-3) {
            return State::Ended;
        }
        if c.playing { State::Playing } else { State::Paused }
    }

    pub fn play(&self) {
        if self.state() == State::Ended {
            self.seek(0.0);
        }
        let mut c = self.inner.clock.lock().unwrap();
        c.playing = true;
        if c.ready && !c.running {
            self.inner.start_clock(&mut c);
        }
        drop(c);
        // a decoder that stopped after a keyframe seek carries on
        self.inner.video_bell.ring();
    }

    pub fn pause(&self) {
        let mut c = self.inner.clock.lock().unwrap();
        c.playing = false;
        if c.running {
            self.inner.stop_clock(&mut c);
        }
    }

    pub fn toggle(&self) {
        let playing = self.inner.clock.lock().unwrap().playing;
        if playing && self.state() != State::Ended { self.pause() } else { self.play() }
    }

    /// Jump to `seconds` (clamped to the media), landing exactly. Playback continues or stays paused as it was.
    pub fn seek(&self, seconds: f64) {
        self.seek_mode(seconds, SeekMode::Accurate);
    }

    /// [`Player::seek`] with a choice of how exactly to land: [`SeekMode::Keyframe`] shows the
    /// keyframe at or before `seconds` at once, without decoding up to it.
    pub fn seek_mode(&self, seconds: f64, mode: SeekMode) {
        let inner = &self.inner;
        let mut target = if seconds.is_finite() { seconds.max(0.0) } else { 0.0 };
        if let Some(d) = inner.info.duration {
            target = target.min(d);
        }
        let mut c = inner.clock.lock().unwrap();
        if c.running {
            if let Some(out) = &inner.out {
                out.set_playing(false);
            }
        }
        c.gen += 1;
        let gen = c.gen;
        c.ready = false;
        c.running = false;
        c.anchor_media = target;
        {
            let mut a = inner.audio.lock().unwrap();
            *a = AudioState { gen, ..Default::default() };
            if let Some(out) = &inner.out {
                out.clear();
            }
        }
        {
            let mut v = inner.video.lock().unwrap();
            let (shown, skipped) = (v.shown, v.skipped);
            *v = VideoQueue { gen, shown, skipped, ..Default::default() };
        }
        for t in &self.tx {
            let _ = t.send(Cmd::Seek { to: target, gen, mode });
        }
        inner.video_bell.ring();
        inner.audio_bell.ring();
    }

    pub fn seek_by(&self, delta: f64) {
        self.seek(self.position() + delta);
    }

    pub fn set_volume(&self, volume: f32) {
        let v = volume.clamp(0.0, 2.0);
        let mut c = self.inner.clock.lock().unwrap();
        c.volume = v;
        if let Some(out) = &self.inner.out {
            out.set_volume(if c.muted { 0.0 } else { v });
        }
    }

    pub fn set_muted(&self, muted: bool) {
        let mut c = self.inner.clock.lock().unwrap();
        c.muted = muted;
        if let Some(out) = &self.inner.out {
            out.set_volume(if muted { 0.0 } else { c.volume });
        }
    }

    pub fn set_looping(&self, on: bool) {
        self.inner.clock.lock().unwrap().looping = on;
    }

    /// The picture that should be on screen now, if any. Frames that are already late when
    /// this is called are skipped, so a slow caller drops frames instead of falling behind.
    pub fn frame(&self) -> Option<Arc<TimedFrame>> {
        let pos = self.inner.update()?;
        if self.inner.clock.lock().unwrap().looping && self.state() == State::Ended {
            self.seek(0.0);
        }
        let mut q = self.inner.video.lock().unwrap();
        if !q.ready {
            return None;
        }
        let before = q.frames.len();
        q.discard_stale(pos);
        if q.frames.len() < before {
            // room for the decoder to queue more
            self.inner.video_bell.ring();
        }
        let front = q.frames.front().cloned();
        match front {
            Some(f) if f.time <= pos + 1e-4 || q.last.is_none() => {
                if !q.front_shown {
                    q.front_shown = true;
                    q.shown += 1;
                }
                q.last = Some(f.clone());
                Some(f)
            }
            _ => q.last.clone(),
        }
    }

    /// Call `wake` (from a decoder thread) whenever a new picture is queued or playback fails.
    /// A window uses it to repaint exactly when there is something new, instead of polling.
    pub fn set_waker(&self, wake: Waker) {
        *self.inner.waker.lock().unwrap() = Some(wake);
    }

    /// Seconds of media time until the next queued picture is due, or None if none is queued
    /// beyond the one on show. At normal speed this is also wall time.
    pub fn next_frame_due(&self) -> Option<f64> {
        let pos = self.inner.update()?;
        let q = self.inner.video.lock().unwrap();
        q.frames.iter().find(|f| f.time > pos + 1e-4).map(|f| f.time - pos)
    }

    /// The volume (0..2), whether it is muted, and whether it repeats; cheaper than [`Player::status`].
    pub fn volume(&self) -> f32 {
        self.inner.clock.lock().unwrap().volume
    }

    pub fn muted(&self) -> bool {
        self.inner.clock.lock().unwrap().muted
    }

    pub fn looping(&self) -> bool {
        self.inner.clock.lock().unwrap().looping
    }

    pub fn error(&self) -> Option<Error> {
        self.inner.error.lock().unwrap().clone()
    }

    pub fn has_video(&self) -> bool {
        self.inner.has_video
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        self.inner.shutdown.store(true, Ordering::SeqCst);
        for t in &self.tx {
            let _ = t.send(Cmd::Stop);
        }
        self.inner.video_bell.ring();
        self.inner.audio_bell.ring();
        if let Some(out) = &self.inner.out {
            out.set_playing(false);
        }
        for h in self.threads.lock().unwrap().drain(..) {
            let _ = h.join();
        }
    }
}

/// Codec names that cannot be decoded by this build, for building helpful messages.
pub fn codec_label(c: CodecId) -> &'static str {
    c.name()
}
