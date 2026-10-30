//! Media understanding: analyzers turn packets and decoded frames into small typed
//! [`Observation`]s (a scene cut, the start of a silence, a loudness reading) and contribute to
//! rolling digests, so an agent learns what is in a stream without being handed its pixels.
//!
//! Analyzers know nothing about files, sockets or sessions: whoever reads the media feeds them
//! [`Input`]s in presentation order per stream and asks for digests at window boundaries. The
//! same analyzers therefore serve a file read as fast as possible and a live source.

mod audio;
mod observation;
mod speech;
mod structure;
mod video;

pub use ironav_codec::change::LowRes;
pub use observation::{kind_info, level_of, KindInfo, Level, Observation, Picture, KINDS};
pub use speech::VoiceActivity;

use ironav_core::{AudioFrame, MediaType, Packet, StreamInfo, VideoFrame};
use serde_json::Value;

/// One thing an analyzer is shown, with its presentation time and duration in seconds.
pub enum Input<'a> {
    Packet { packet: &'a Packet, t: f64 },
    Audio { frame: &'a AudioFrame, t: f64 },
    /// `low` is the picture's luma averaged over 4x4 blocks, worked out once for every analyzer.
    Video { frame: &'a VideoFrame, low: &'a LowRes, t: f64, dt: f64 },
}

pub trait Analyzer: Send {
    fn observe(&mut self, input: &Input, out: &mut Vec<Observation>);
    /// This analyzer's part of the digest for the `seconds` since the last call, after which it
    /// starts a new window.
    fn digest(&mut self, _seconds: f64) -> Option<Value> {
        None
    }
    /// The end of the stream at `t`: intervals still open are closed. Returns this analyzer's
    /// part of the summary of the whole stream.
    fn finish(&mut self, _t: f64, _out: &mut Vec<Observation>) -> Option<Value> {
        None
    }
}

/// What an analyzer is fed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Consumes {
    /// Compressed packets of any stream; no decoding needed.
    Packets,
    Audio,
    Video,
}

impl Consumes {
    pub fn applies_to(self, m: MediaType) -> bool {
        match self {
            Consumes::Packets => true,
            Consumes::Audio => m == MediaType::Audio,
            Consumes::Video => m == MediaType::Video,
        }
    }
}

/// Settings shared by the analyzers.
#[derive(Debug, Clone)]
pub struct Settings {
    /// The longest gap between thumbnails, in seconds.
    pub thumbnail_seconds: f64,
    /// Thumbnails are scaled to at most this width.
    pub thumbnail_width: u32,
}

impl Default for Settings {
    fn default() -> Self {
        Settings { thumbnail_seconds: 10.0, thumbnail_width: 320 }
    }
}

pub struct AnalyzerInfo {
    pub name: &'static str,
    pub consumes: Consumes,
    pub description: &'static str,
    /// Observation kinds it can produce.
    pub emits: &'static [&'static str],
    make: fn(&StreamInfo, &Settings) -> Box<dyn Analyzer>,
}

impl AnalyzerInfo {
    pub fn make(&self, s: &StreamInfo, settings: &Settings) -> Box<dyn Analyzer> {
        (self.make)(s, settings)
    }
}

pub const ANALYZERS: &[AnalyzerInfo] = &[
    AnalyzerInfo {
        name: "structure",
        consumes: Consumes::Packets,
        description: "Packet timing and size: bit rate per window, gaps in the timestamps and jumps backwards.",
        emits: &["timestamp_gap", "discontinuity"],
        make: |s, _| Box::new(structure::Structure::new(s)),
    },
    AnalyzerInfo {
        name: "levels",
        consumes: Consumes::Audio,
        description: "Sample peak and RMS level in dBFS, and clipped samples.",
        emits: &["clipping"],
        make: |_, _| Box::new(audio::Levels::default()),
    },
    AnalyzerInfo {
        name: "loudness",
        consumes: Consumes::Audio,
        description: "EBU R128 / ITU-R BS.1770 loudness: momentary, short-term and integrated (LUFS) and loudness range (LU).",
        emits: &["loudness"],
        make: |s, _| Box::new(audio::Loudness::new(s)),
    },
    AnalyzerInfo {
        name: "silence",
        consumes: Consumes::Audio,
        description: "Spans of at least 0.5 s below -50 dBFS RMS.",
        emits: &["silence_start", "silence_end"],
        make: |_, _| Box::new(audio::Silence::new()),
    },
    AnalyzerInfo {
        name: "voice_activity",
        consumes: Consumes::Audio,
        description: "Spans of speech, from energy above the noise floor, zero crossings and syllabic modulation: a cheap gate for a transcription model (steady tones, hum and hiss do not count).",
        emits: &["speech_start", "speech_end"],
        make: |_, _| Box::new(speech::VoiceActivity::default()),
    },
    AnalyzerInfo {
        name: "scene",
        consumes: Consumes::Video,
        description: "Scene cuts (the same measure the H.264 encoder uses to start an IDR picture) and how much the picture changes per window.",
        emits: &["scene_cut", "change"],
        make: |_, _| Box::new(video::Scene::default()),
    },
    AnalyzerInfo {
        name: "black",
        consumes: Consumes::Video,
        description: "Spans of at least 0.5 s where 98 % of the picture is black.",
        emits: &["black_start", "black_end"],
        make: |_, _| Box::new(video::Black::new()),
    },
    AnalyzerInfo {
        name: "freeze",
        consumes: Consumes::Video,
        description: "Spans of at least 1 s where the picture does not change (a frozen feed or a still).",
        emits: &["freeze_start", "freeze_end"],
        make: |_, _| Box::new(video::Freeze::new()),
    },
    AnalyzerInfo {
        name: "thumbnails",
        consumes: Consumes::Video,
        description: "A small picture whenever the content has changed (perceptual hash) or the thumbnail interval has passed.",
        emits: &["thumbnail"],
        make: |_, s| Box::new(video::Thumbnails::new(s)),
    },
];

pub fn analyzer(name: &str) -> Option<&'static AnalyzerInfo> {
    ANALYZERS.iter().find(|a| a.name == name)
}

/// Distinct names [`intern`] keeps for the life of the process.
pub const MAX_INTERNED: usize = 512;

/// A `&'static str` for a name made at run time (a sidecar's name, or a kind of observation it
/// reports), so it fits the observation envelope. Each distinct name is stored once; past
/// [`MAX_INTERNED`] of them, new names get `None`.
pub fn intern(name: &str) -> Option<&'static str> {
    static NAMES: std::sync::Mutex<Vec<&'static str>> = std::sync::Mutex::new(Vec::new());
    let mut names = NAMES.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(n) = names.iter().find(|n| **n == name) {
        return Some(n);
    }
    if names.len() >= MAX_INTERNED {
        return None;
    }
    let n: &'static str = Box::leak(name.to_string().into_boxed_str());
    names.push(n);
    Some(n)
}

/// `x` rounded to `digits` decimals, so observations stay short.
pub(crate) fn round(x: f64, digits: i32) -> f64 {
    let m = 10f64.powi(digits);
    (x * m).round() / m
}

/// A power ratio in decibels, rounded to 0.01 dB; `null` for silence.
pub(crate) fn db_power(p: f64) -> Value {
    if p > 0.0 { serde_json::json!(round(10.0 * p.log10(), 2)) } else { Value::Null }
}

/// Opens and closes spans of time where a condition holds for at least `min` seconds.
pub(crate) struct Spans {
    start_kind: &'static str,
    end_kind: &'static str,
    analyzer: &'static str,
    min: f64,
    start: Option<f64>,
    confirmed: bool,
    /// Seconds inside confirmed spans, this window and in all.
    window: f64,
    total: f64,
    count: u64,
}

impl Spans {
    pub(crate) fn new(analyzer: &'static str, start_kind: &'static str, end_kind: &'static str, min: f64) -> Self {
        Spans { start_kind, end_kind, analyzer, min, start: None, confirmed: false, window: 0.0, total: 0.0, count: 0 }
    }

    /// The condition over `[t, t + dt)`.
    pub(crate) fn update(&mut self, t: f64, dt: f64, active: bool, out: &mut Vec<Observation>) {
        if active {
            let start = *self.start.get_or_insert(t);
            if self.confirmed {
                self.window += dt;
                self.total += dt;
            } else if t + dt - start >= self.min {
                self.confirmed = true;
                self.count += 1;
                self.window += t + dt - start;
                self.total += t + dt - start;
                out.push(Observation::new(self.start_kind, self.analyzer, start, Value::Object(Default::default())));
            }
        } else {
            self.close(t, out);
        }
    }

    pub(crate) fn close(&mut self, t: f64, out: &mut Vec<Observation>) {
        if let Some(start) = self.start.take() {
            if self.confirmed {
                out.push(Observation::new(self.end_kind, self.analyzer, start, serde_json::json!({ "duration": round(t - start, 3) })).until(t));
            }
        }
        self.confirmed = false;
    }

    pub(crate) fn is_open(&self) -> bool {
        self.start.is_some()
    }

    /// Seconds inside spans since the last call.
    pub(crate) fn take_window(&mut self) -> f64 {
        std::mem::take(&mut self.window)
    }

    pub(crate) fn summary(&self) -> Value {
        serde_json::json!({ "spans": self.count, "seconds": round(self.total, 3) })
    }
}
