//! The observation envelope and the catalogue of observation kinds.

use ironav_core::VideoFrame;
use serde::Serialize;
use serde_json::{Map, Value};

/// How much detail a consumer wants. Each kind has a level; a consumer at a level receives
/// every kind at that level or below.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
    /// Rolling digests and the final summary only: the cheapest way to follow a stream.
    Digest,
    /// Discrete events too: cuts, silences, black spans, thumbnails, gaps.
    Events,
    /// Everything, including per-frame and per-second measurements (for tools rather than models).
    Raw,
}

impl Level {
    pub const ALL: [Level; 3] = [Level::Digest, Level::Events, Level::Raw];
    pub fn name(self) -> &'static str {
        match self {
            Level::Digest => "digest",
            Level::Events => "events",
            Level::Raw => "raw",
        }
    }
    pub fn parse(s: &str) -> Option<Level> {
        Self::ALL.into_iter().find(|l| l.name() == s)
    }
}

/// One thing noticed about a stream.
#[derive(Debug, Clone, Serialize)]
pub struct Observation {
    /// Position in the session's log, strictly increasing; set when the observation is logged.
    pub seq: u64,
    pub kind: &'static str,
    pub analyzer: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub stream: Option<usize>,
    /// Media time in seconds; for a span, where it starts.
    pub t: f64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub t_end: Option<f64>,
    /// When it was logged (live sources only), as UTC in RFC 3339 form.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub wall: Option<String>,
    /// From the arrival of the packet behind it to its logging (live sources only).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<f64>,
    pub data: Value,
    /// Where to fetch what it refers to (a thumbnail file, for example).
    #[serde(skip_serializing_if = "Map::is_empty")]
    pub refs: Map<String, Value>,
    /// A picture that goes with it (a thumbnail); stored or sent by whoever logs it.
    #[serde(skip)]
    pub picture: Option<Picture>,
}

/// A picture attached to an observation; its debug form is only its size.
#[derive(Clone)]
pub struct Picture(pub VideoFrame);

impl std::fmt::Debug for Picture {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "Picture({}x{})", self.0.width, self.0.height)
    }
}

impl Observation {
    pub fn new(kind: &'static str, analyzer: &'static str, t: f64, data: Value) -> Self {
        Observation { seq: 0, kind, analyzer, stream: None, t: crate::round(t, 3), t_end: None, wall: None, latency_ms: None, data, refs: Map::new(), picture: None }
    }

    pub fn until(mut self, t_end: f64) -> Self {
        self.t_end = Some(crate::round(t_end, 3));
        self
    }

    pub fn level(&self) -> Level {
        level_of(self.kind)
    }
}

#[derive(Debug, Clone, Copy, Serialize)]
pub struct KindInfo {
    pub name: &'static str,
    pub level: Level,
    pub description: &'static str,
}

/// Every kind of observation, with its level.
pub const KINDS: &[KindInfo] = &[
    KindInfo { name: "digest", level: Level::Digest, description: "Rolling summary of one window: each analyzer's figures for it (`t`..`t_end`)." },
    KindInfo { name: "summary", level: Level::Digest, description: "Summary of everything analyzed, at the end." },
    KindInfo { name: "scene_cut", level: Level::Events, description: "The picture starts a new scene; `data.score` above 0.8, higher is a harder cut." },
    KindInfo { name: "silence_start", level: Level::Events, description: "Audio has been silent from `t` (for at least the minimum duration)." },
    KindInfo { name: "silence_end", level: Level::Events, description: "A silence from `t` to `t_end` ended." },
    KindInfo { name: "black_start", level: Level::Events, description: "The picture has been black from `t`." },
    KindInfo { name: "black_end", level: Level::Events, description: "A black span from `t` to `t_end` ended." },
    KindInfo { name: "freeze_start", level: Level::Events, description: "The picture has not changed since `t`." },
    KindInfo { name: "freeze_end", level: Level::Events, description: "A frozen span from `t` to `t_end` ended." },
    KindInfo { name: "speech_start", level: Level::Events, description: "Speech from `t` (voice activity; the start is placed at the first voiced frame)." },
    KindInfo { name: "speech_end", level: Level::Events, description: "Speech from `t` to `t_end` ended." },
    KindInfo { name: "caption", level: Level::Events, description: "A caption from the source (an HLS or YouTube subtitle track), shown from `t` to `t_end`: `data.text`, and `data.language` when the track says." },
    KindInfo { name: "captions_failed", level: Level::Events, description: "The source's captions stopped coming; `data.message` says why. The media goes on." },
    KindInfo { name: "analyzer_failed", level: Level::Events, description: "A sidecar analyzer crashed, hung or could not be restarted; `data.message` says why (with the end of its error output) and `data.restarting` whether it comes back." },
    KindInfo { name: "analyzer_lagging", level: Level::Events, description: "A sidecar fell behind a live source and `data.dropped` inputs were not sent to it; at most one every 10 s." },
    KindInfo { name: "clipping", level: Level::Events, description: "Audio samples at full scale near `t` (`data.samples`); at most one per second." },
    KindInfo { name: "thumbnail", level: Level::Events, description: "A small picture at `t`, with a 64-bit perceptual hash; `refs` says where it is." },
    KindInfo { name: "timestamp_gap", level: Level::Events, description: "No packets of the stream between `t` and `t_end`." },
    KindInfo { name: "discontinuity", level: Level::Events, description: "Timestamps jumped backwards at `t` by `data.seconds`." },
    KindInfo { name: "decode_error", level: Level::Events, description: "A packet could not be decoded; analysis continues with the next one." },
    KindInfo { name: "source_stalled", level: Level::Events, description: "A network source sent nothing for `data.seconds`; it is still connected." },
    KindInfo { name: "source_lost", level: Level::Events, description: "A network source was lost (`data.reason`); it is being reconnected, with backoff." },
    KindInfo { name: "source_restored", level: Level::Events, description: "A lost source is back after `data.attempts` attempts (`data.reconnects` in all); decoding resumes at the next keyframe and media time goes on from where it stopped." },
    KindInfo { name: "source_failed", level: Level::Events, description: "A lost source could not be reconnected (`data.reason`); the session ends." },
    KindInfo { name: "loudness", level: Level::Raw, description: "Momentary and short-term loudness (LUFS), once a second." },
    KindInfo { name: "change", level: Level::Raw, description: "How much each picture differs from the last (`data.score`: 0 still, above 0.8 a cut)." },
];

pub fn kind_info(name: &str) -> Option<&'static KindInfo> {
    KINDS.iter().find(|k| k.name == name)
}

/// The level of a kind; unknown kinds count as events.
pub fn level_of(kind: &str) -> Level {
    kind_info(kind).map_or(Level::Events, |k| k.level)
}
