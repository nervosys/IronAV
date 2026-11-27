//! Chunking media for agents: `chunk` cuts a file into stable, addressable pieces (fixed windows or
//! scenes) and describes each one cheaply; `chunk_media` then fetches the pictures, sound or a small
//! clip of one piece, inline, with nothing written to disk.
//!
//! The chunk table comes from one run of the analyze pipeline (digests per window, scene cuts,
//! thumbnails, sidecar text), kept in a small in-memory LRU keyed by the file's fingerprint and the
//! parameters, so paging through a table does not analyze again. A chunk's id is derived from the
//! same fingerprint, the mode and its parameters and the chunk's index: the same file and arguments
//! give the same ids on every run, and `ironav://chunks/{id}` reads the cached descriptor back.

use crate::analyze::{analyze, AnalyzeJob};
use crate::tools::Ctx;
use ironav_core::{Demuxer, Error, ErrorCode, Frame, MediaType, Result, StreamInfo, VideoFrame};
use ironav_formats::open_demuxer;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// The URI of a chunk's descriptor is this followed by its id.
pub const URI_PREFIX: &str = "ironav://chunks/";
/// The URI template MCP clients are told about.
pub const URI_TEMPLATE: &str = "ironav://chunks/{id}";

/// Chunk tables kept in memory.
const CACHE_TABLES: usize = 8;
/// Most chunks in one table: a finer grain on a long file is refused, not slowly built.
const MAX_CHUNKS: usize = 100_000;
/// Thumbnails kept in one table; chunks beyond them share the nearest earlier picture.
const MAX_THUMBNAILS: usize = 2000;
/// The window scene mode measures in: a scene's figures are those of the windows it holds.
const SCENE_GRANULE: f64 = 1.0;
/// The bytes of a fingerprint's sample from each end of the file.
const SAMPLE: u64 = 1 << 20;
const MAX_PACKETS_PER_PICTURE: usize = 20_000;
const MAX_FRAMES: usize = 16;
const MAX_AUDIO_SECONDS: f64 = 60.0;
const MAX_INLINE_BYTES: usize = 8 << 20;

// ---- fingerprint and ids -------------------------------------------------------------------------

struct Fnv(u64);

impl Fnv {
    fn new() -> Fnv {
        Fnv(0xcbf2_9ce4_8422_2325)
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= b as u64;
            self.0 = self.0.wrapping_mul(0x0100_0000_01b3);
        }
    }
}

pub(crate) fn hash_str(s: &str) -> u64 {
    let mut h = Fnv::new();
    h.write(s.as_bytes());
    h.0
}

/// A name for the content of the file at `path`: its size and modification time and a hash of its
/// first and last MiB, as 16 hex digits. The same file gives the same fingerprint on every run.
pub fn fingerprint(path: &str) -> Result<String> {
    let mut f = std::fs::File::open(path)?;
    let meta = f.metadata()?;
    if !meta.is_file() {
        return Err(Error::invalid_arg(format!("'{path}' is not a file")));
    }
    let size = meta.len();
    let mtime = meta.modified().ok().and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok()).map_or(0, |d| d.as_nanos());
    let mut h = Fnv::new();
    h.write(&size.to_le_bytes());
    h.write(&mtime.to_le_bytes());
    let mut buf = Vec::new();
    (&mut f).take(SAMPLE).read_to_end(&mut buf)?;
    h.write(&buf);
    if size > SAMPLE {
        buf.clear();
        f.seek(SeekFrom::Start(size.saturating_sub(SAMPLE).max(SAMPLE)))?;
        f.read_to_end(&mut buf)?;
        h.write(&buf);
    }
    Ok(format!("{:016x}", h.0))
}

// ---- arguments -----------------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct ChunkArgs {
    input: String,
    #[serde(default)]
    mode: Option<String>,
    #[serde(default)]
    seconds: Option<f64>,
    #[serde(default)]
    min_seconds: Option<f64>,
    #[serde(default)]
    max_seconds: Option<f64>,
    #[serde(default)]
    from: Option<f64>,
    #[serde(default)]
    to: Option<f64>,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    budget_tokens: Option<usize>,
    #[serde(default)]
    include: Option<Vec<String>>,
    #[serde(default)]
    thumbnail_width: Option<u32>,
    #[serde(default)]
    sidecars: Vec<String>,
    #[serde(default)]
    hwaccel: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Mode {
    Time,
    Scene,
}

impl Mode {
    fn name(self) -> &'static str {
        match self {
            Mode::Time => "time",
            Mode::Scene => "scene",
        }
    }
}

/// The validated parameters that decide where the chunks fall.
#[derive(Debug, Clone)]
struct Spec {
    mode: Mode,
    seconds: f64,
    min: f64,
    max: f64,
    from: f64,
    to: Option<f64>,
    thumb_width: u32,
    sidecars: Vec<String>,
    hwaccel: String,
}

impl Spec {
    // The `!(x > 0.0)` checks reject NaN too, which a plain `<=` would let through.
    #[allow(clippy::neg_cmp_op_on_partial_ord)]
    fn parse(a: &ChunkArgs) -> Result<Spec> {
        let mode = match a.mode.as_deref().unwrap_or("time") {
            "time" => Mode::Time,
            "scene" => Mode::Scene,
            other => return Err(Error::invalid_arg(format!("unknown mode '{other}'")).alternatives(["time", "scene"])),
        };
        let (mut seconds, mut min, mut max) = (0.0, 0.0, 0.0);
        match mode {
            Mode::Time => {
                if a.min_seconds.is_some() || a.max_seconds.is_some() {
                    return Err(Error::invalid_arg("min_seconds and max_seconds are for mode 'scene'").hint("in mode 'time' the chunk length is `seconds`"));
                }
                seconds = a.seconds.unwrap_or(30.0);
                if !(seconds > 0.0) || !seconds.is_finite() {
                    return Err(Error::invalid_arg("seconds must be above 0"));
                }
            }
            Mode::Scene => {
                if a.seconds.is_some() {
                    return Err(Error::invalid_arg("seconds is for mode 'time'").hint("in mode 'scene' the chunks are the scenes; bound them with min_seconds and max_seconds"));
                }
                min = a.min_seconds.unwrap_or(1.0);
                max = a.max_seconds.unwrap_or(120.0);
                if !(min >= 0.0) || !min.is_finite() {
                    return Err(Error::invalid_arg("min_seconds must be 0 or more"));
                }
                if !(max > 0.0) || !max.is_finite() || max < min {
                    return Err(Error::invalid_arg("max_seconds must be above 0 and not below min_seconds"));
                }
            }
        }
        let from = a.from.unwrap_or(0.0);
        if !(from >= 0.0) || !from.is_finite() {
            return Err(Error::invalid_arg("from must be 0 or more"));
        }
        if let Some(to) = a.to {
            if !(to > from) {
                return Err(Error::invalid_arg("'to' must be after 'from'"));
            }
        }
        let mut sidecars = a.sidecars.clone();
        sidecars.sort();
        sidecars.dedup();
        Ok(Spec {
            mode,
            seconds,
            min,
            max,
            from,
            to: a.to.filter(|t| t.is_finite()),
            thumb_width: a.thumbnail_width.unwrap_or(160).clamp(16, 640),
            sidecars,
            hwaccel: a.hwaccel.clone().unwrap_or_else(|| "auto".into()),
        })
    }

    /// What decides the chunk boundaries, for the ids.
    fn id_params(&self) -> String {
        match self.mode {
            Mode::Time => format!("time|{:?}|{:?}|{:?}", self.seconds, self.from, self.to),
            Mode::Scene => format!("scene|{:?}|{:?}|{:?}|{:?}", self.min, self.max, self.from, self.to),
        }
    }

    /// What decides the table's content: the cache key.
    fn key(&self, fp: &str) -> String {
        format!("{fp}|{}|{}|{}|{}", self.id_params(), self.thumb_width, self.sidecars.join(","), self.hwaccel)
    }
}

/// What a table was asked to hold, and so what a request can be answered from.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
struct Have {
    thumbnail: bool,
    audio: bool,
    stats: bool,
}

impl Have {
    fn covers(self, want: Have) -> bool {
        (self.thumbnail || !want.thumbnail) && (self.audio || !want.audio) && (self.stats || !want.stats)
    }
    fn union(self, o: Have) -> Have {
        Have { thumbnail: self.thumbnail || o.thumbnail, audio: self.audio || o.audio, stats: self.stats || o.stats }
    }
}

/// What a response carries beyond the chunk's place.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Flags {
    pub(crate) thumbnail: bool,
    pub(crate) audio: bool,
    pub(crate) transcript: bool,
    pub(crate) stats: bool,
}

impl Flags {
    pub(crate) fn parse(include: &Option<Vec<String>>) -> Result<Flags> {
        let mut f = Flags { thumbnail: false, audio: false, transcript: false, stats: false };
        let default = ["audio".to_string(), "transcript".to_string(), "stats".to_string()];
        for name in include.as_deref().unwrap_or(&default) {
            match name.as_str() {
                "thumbnail" => f.thumbnail = true,
                "audio" => f.audio = true,
                "transcript" => f.transcript = true,
                "stats" => f.stats = true,
                other => return Err(Error::invalid_arg(format!("unknown include '{other}'")).alternatives(["thumbnail", "audio", "transcript", "stats"])),
            }
        }
        Ok(f)
    }
    fn have(self) -> Have {
        Have { thumbnail: self.thumbnail, audio: self.audio, stats: self.stats }
    }
    /// Turns off the most expendable field still on: the order a budget trims them in.
    pub(crate) fn drop_next(&mut self) -> Option<&'static str> {
        if std::mem::take(&mut self.thumbnail) {
            Some("thumbnail")
        } else if std::mem::take(&mut self.stats) {
            Some("stats")
        } else if std::mem::take(&mut self.audio) {
            Some("audio")
        } else if std::mem::take(&mut self.transcript) {
            Some("transcript")
        } else {
            None
        }
    }
}

// ---- the table -----------------------------------------------------------------------------------

struct ChunkRec {
    id: String,
    index: usize,
    t: f64,
    t_end: f64,
    scene_id: Option<usize>,
    /// (part, parts) of a scene split because it ran past `max_seconds`.
    part: Option<(usize, usize)>,
    /// The table's picture, the picture's time and whether it lies inside the chunk.
    thumb: Option<(usize, f64, bool)>,
    audio: Option<Value>,
    stats: Option<Value>,
    transcript: Option<Value>,
}

struct Table {
    key: String,
    input: String,
    fingerprint: String,
    spec: Spec,
    have: Have,
    chunks: Vec<ChunkRec>,
    index: HashMap<String, usize>,
    /// PNG thumbnails, base64, by time.
    images: Vec<(f64, String)>,
    start: f64,
    end: f64,
    warnings: Vec<String>,
}

static CACHE: Mutex<Vec<Arc<Table>>> = Mutex::new(Vec::new());

fn cache() -> std::sync::MutexGuard<'static, Vec<Arc<Table>>> {
    CACHE.lock().unwrap_or_else(|e| e.into_inner())
}

/// The cached table for `key`, if there is one (most recently used first).
fn cached(key: &str) -> Option<Arc<Table>> {
    let mut c = cache();
    let i = c.iter().position(|t| t.key == key)?;
    let t = c.remove(i);
    c.insert(0, t.clone());
    Some(t)
}

fn remember(t: Arc<Table>) {
    let mut c = cache();
    c.retain(|o| o.key != t.key);
    c.insert(0, t);
    c.truncate(CACHE_TABLES);
}

/// Drops the cached tables of `input` (a test's, or an embedder's after it changed a file).
pub fn forget(input: &str) {
    cache().retain(|t| t.input != input);
}

/// The cached chunk with this id.
fn find(id: &str) -> Option<(Arc<Table>, usize)> {
    cache().iter().find_map(|t| t.index.get(id).map(|&i| (t.clone(), i)))
}

/// `FINGERPRINT-PARAMS-INDEX-T-TEND`: the file, a hash of the mode and its parameters, the chunk's index,
/// and its range in milliseconds. The range makes an id enough for `chunk_media` even when no table is cached.
fn make_id(fp: &str, params: u32, index: usize, t: f64, t_end: f64) -> String {
    format!("{fp}-{params:08x}-{index}-{}-{}", (t * 1000.0).round() as u64, (t_end * 1000.0).round() as u64)
}

/// The file's fingerprint and the range an id names.
fn parse_id(id: &str) -> Option<(&str, f64, f64)> {
    let mut p = id.split('-');
    let (fp, _params, _index, t, t_end) = (p.next()?, p.next()?, p.next()?, p.next()?, p.next()?);
    if p.next().is_some() || fp.len() != 16 {
        return None;
    }
    Some((fp, t.parse::<u64>().ok()? as f64 / 1000.0, t_end.parse::<u64>().ok()? as f64 / 1000.0))
}

pub(crate) fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// One digest window: when it was, and each stream's figures.
pub(crate) struct Digest<'a> {
    pub(crate) t: f64,
    pub(crate) t_end: f64,
    pub(crate) data: &'a Map<String, Value>,
}

const AUDIO_ANALYZERS: &[&str] = &["levels", "loudness", "silence", "voice_activity"];

fn is_builtin(analyzer: &str) -> bool {
    ironav_analyze::ANALYZERS.iter().any(|a| a.name == analyzer)
}

/// A digest figure's way of combining over windows.
fn fold(key: &str, v: &[(f64, f64)]) -> Option<f64> {
    if v.is_empty() {
        return None;
    }
    let weight: f64 = v.iter().map(|p| p.0).sum::<f64>().max(1e-9);
    Some(match key {
        "peak_dbfs" | "momentary_max_lufs" | "max_change" => v.iter().map(|p| p.1).fold(f64::NEG_INFINITY, f64::max),
        "short_term_lufs" | "integrated_lufs" => v.last()?.1,
        "mean_change" | "kbit_s" => v.iter().map(|p| p.0 * p.1).sum::<f64>() / weight,
        "rms_dbfs" => 10.0 * (v.iter().map(|p| p.0 * 10f64.powf(p.1 / 10.0)).sum::<f64>() / weight).max(1e-12).log10(),
        _ => v.iter().map(|p| p.1).sum(),
    })
}

/// The figures of the windows in `group` as one: audio levels and the rest, and the text a sidecar
/// made of them.
pub(crate) fn merge(group: &[&Digest], len: f64) -> (Option<Value>, Option<Value>, Option<Value>) {
    // label -> key -> (weight, value) in time order; a key present with no number (a null) is kept as null
    let mut audio: BTreeMap<String, BTreeMap<String, Vec<(f64, f64)>>> = BTreeMap::new();
    let mut stats: BTreeMap<String, BTreeMap<String, Vec<(f64, f64)>>> = BTreeMap::new();
    let mut nulls: BTreeSet<(String, String)> = BTreeSet::new();
    let mut text = String::new();
    for d in group {
        let w = (d.t_end - d.t).max(0.0);
        for (label, parts) in d.data {
            let Some(parts) = parts.as_object() else { continue };
            for (analyzer, part) in parts {
                if !is_builtin(analyzer) {
                    if let Some(s) = part.get("text").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty()) {
                        if !text.is_empty() {
                            text.push(' ');
                        }
                        text.push_str(s);
                    }
                    continue;
                }
                let Some(part) = part.as_object() else { continue };
                let into = if AUDIO_ANALYZERS.contains(&analyzer.as_str()) { &mut audio } else { &mut stats };
                let m = into.entry(label.clone()).or_default();
                for (k, v) in part {
                    match v.as_f64() {
                        Some(x) => m.entry(k.clone()).or_default().push((w, x)),
                        None if v.is_null() => {
                            nulls.insert((label.clone(), k.clone()));
                        }
                        None => {}
                    }
                }
            }
        }
    }
    let finish = |m: BTreeMap<String, BTreeMap<String, Vec<(f64, f64)>>>, nulls: &BTreeSet<(String, String)>| -> Option<Value> {
        let mut out = Map::new();
        for (label, keys) in m {
            let mut o = Map::new();
            for (k, v) in &keys {
                if k == "speech_fraction" {
                    continue;
                }
                if let Some(x) = fold(k, v) {
                    o.insert(k.clone(), json!(if k == "kbit_s" { (x * 10.0).round() / 10.0 } else { round3(x) }));
                }
            }
            for (l, k) in nulls {
                if *l == label && !o.contains_key(k) {
                    o.insert(k.clone(), Value::Null);
                }
            }
            if let Some(s) = o.get("speech_seconds").and_then(Value::as_f64) {
                o.insert("speech_fraction".into(), json!(if len > 0.0 { round3((s / len).min(1.0)) } else { 0.0 }));
            }
            if !o.is_empty() {
                out.insert(label, Value::Object(o));
            }
        }
        (!out.is_empty()).then_some(Value::Object(out))
    };
    let transcript = (!text.is_empty()).then(|| {
        let mut v = json!({ "text": text });
        crate::untrusted::mark(&mut v);
        v
    });
    (finish(audio, &nulls), finish(stats, &nulls), transcript)
}

/// The boundaries of the chunks (one more than there are chunks), for a time-mode table.
fn time_bounds(start: f64, end: f64, seconds: f64) -> Result<Vec<f64>> {
    if end <= start {
        return Ok(vec![start]);
    }
    let n = ((end - start) / seconds - 1e-9).ceil().max(1.0);
    if n > MAX_CHUNKS as f64 {
        return Err(Error::invalid_arg(format!("{n} chunks of {seconds} s is more than the {MAX_CHUNKS} a table holds"))
            .hint("use a longer `seconds`, or chunk a part of the file with from and to"));
    }
    let n = n as usize;
    let mut b: Vec<f64> = (0..n).map(|i| round3(start + i as f64 * seconds)).collect();
    b.push(round3(end));
    Ok(b)
}

/// The boundaries of the scenes between the cuts, merged and split to the bounds asked for.
fn scene_bounds(start: f64, end: f64, cuts: &[f64], min: f64, max: f64) -> Result<(Vec<f64>, Vec<usize>)> {
    if end <= start {
        return Ok((vec![start], Vec::new()));
    }
    let mut cuts: Vec<f64> = cuts.iter().copied().filter(|c| *c > start && *c < end).collect();
    cuts.sort_by(f64::total_cmp);
    cuts.dedup_by(|a, b| (*a - *b).abs() < 1e-3);
    // a cut that would leave a scene shorter than `min` is not one
    let mut kept = vec![start];
    for c in cuts {
        if c - kept[kept.len() - 1] >= min - 1e-6 {
            kept.push(c);
        }
    }
    if kept.len() > 1 && end - kept[kept.len() - 1] < min - 1e-6 {
        kept.pop();
    }
    kept.push(end);
    // a scene longer than `max` is cut into equal parts
    let mut bounds = vec![kept[0]];
    let mut scene_of = Vec::new();
    for (s, w) in kept.windows(2).enumerate() {
        let len = w[1] - w[0];
        let parts = ((len / max - 1e-9).ceil().max(1.0)) as usize;
        if bounds.len() + parts > MAX_CHUNKS {
            return Err(Error::invalid_arg(format!("more than {MAX_CHUNKS} chunks")).hint("use a larger min_seconds or max_seconds, or chunk a part of the file with from and to"));
        }
        for p in 1..=parts {
            bounds.push(if p == parts { w[1] } else { w[0] + len * p as f64 / parts as f64 });
            scene_of.push(s);
        }
    }
    let bounds = bounds.into_iter().map(round3).collect();
    Ok((bounds, scene_of))
}

fn build_table(a: &ChunkArgs, spec: &Spec, fp: String, key: String, have: Have, ctx: &Ctx) -> Result<Table> {
    let d = open_demuxer(Path::new(&a.input))?;
    let infos: Vec<StreamInfo> = d.streams().to_vec();
    let has_video = infos.iter().any(|s| s.params.media_type() == MediaType::Video);
    let has_audio = infos.iter().any(|s| s.params.media_type() == MediaType::Audio);
    drop(d);
    let mut warnings = Vec::new();
    if spec.mode == Mode::Scene && !has_video {
        return Err(Error::invalid_arg("scene mode needs a video stream").hint("this file has none; use mode 'time'"));
    }
    for s in infos.iter().filter(|s| matches!(s.params.media_type(), MediaType::Video | MediaType::Audio)) {
        crate::policy::current().check_decode(s)?;
    }
    if have.thumbnail && !has_video {
        warnings.push("thumbnail: the file has no video stream".into());
    }
    if have.audio && !has_audio {
        warnings.push("audio: the file has no audio stream".into());
    }
    let mut analyzers = vec!["structure"];
    if has_video {
        if have.thumbnail {
            analyzers.push("thumbnails");
        }
        if spec.mode == Mode::Scene || have.stats {
            analyzers.push("scene");
        }
        if have.stats {
            analyzers.extend(["black", "freeze"]);
        }
    }
    if has_audio && have.audio {
        analyzers.extend(AUDIO_ANALYZERS);
    }
    let scene = spec.mode == Mode::Scene;
    let job: AnalyzeJob = serde_json::from_value(json!({
        "input": a.input,
        "analyzers": analyzers,
        "level": if scene { "events" } else { "digest" },
        "digest_seconds": if scene { SCENE_GRANULE } else { spec.seconds },
        "from": spec.from,
        "to": spec.to,
        "max_observations": 200_000,
        "inline_thumbnails": if have.thumbnail && has_video { MAX_THUMBNAILS } else { 0 },
        "thumbnail_seconds": if scene { 10.0 } else { spec.seconds.clamp(2.0, 60.0) },
        "thumbnail_width": spec.thumb_width,
        "hwaccel": spec.hwaccel,
        "sidecars": spec.sidecars,
    }))
    .map_err(|e| Error::invalid_arg(format!("invalid chunk arguments: {e}")))?;
    let r = analyze(&job, ctx, &mut |_| {})?;
    if let Some(w) = r["warnings"].as_array() {
        warnings.extend(w.iter().filter_map(|w| w.as_str().map(str::to_string)));
    }
    let obs: &[Value] = r["observations"].as_array().map_or(&[], |v| v.as_slice());
    let start = spec.from;
    let mut end = r["media_seconds"].as_f64().map_or(start, |m| start + m);
    if let Some(s) = obs.iter().rev().find(|o| o["kind"] == "summary") {
        if let Some(e) = s["t_end"].as_f64() {
            end = e;
        }
    }
    if let Some(to) = spec.to {
        end = end.min(to);
    }
    let (bounds, scene_of) = match spec.mode {
        Mode::Time => (time_bounds(start, end, spec.seconds)?, Vec::new()),
        Mode::Scene => {
            let cuts: Vec<f64> = obs.iter().filter(|o| o["kind"] == "scene_cut").filter_map(|o| o["t"].as_f64()).collect();
            scene_bounds(start, end, &cuts, spec.min, spec.max)?
        }
    };
    let n = bounds.len().saturating_sub(1);
    // the digests, each given to the chunk holding its middle
    let digests: Vec<Digest> = obs
        .iter()
        .filter(|o| o["kind"] == "digest")
        .filter_map(|o| Some(Digest { t: o["t"].as_f64()?, t_end: o["t_end"].as_f64()?, data: o["data"].as_object()? }))
        .collect();
    let mut groups: Vec<Vec<&Digest>> = vec![Vec::new(); n];
    for d in &digests {
        if n > 0 {
            let mid = (d.t + d.t_end) / 2.0;
            let i = bounds.partition_point(|b| *b <= mid).saturating_sub(1).min(n - 1);
            groups[i].push(d);
        }
    }
    let mut images: Vec<(f64, String)> = r["images"]
        .as_array()
        .map(|l| l.iter().filter_map(|i| Some((i["t"].as_f64()?, i["png_base64"].as_str()?.to_string()))).collect())
        .unwrap_or_default();
    images.sort_by(|a, b| a.0.total_cmp(&b.0));
    // the parts of a split scene: how many, and which
    let mut parts_of: HashMap<usize, usize> = HashMap::new();
    for s in &scene_of {
        *parts_of.entry(*s).or_default() += 1;
    }
    let ph = hash_str(&spec.id_params()) as u32;
    let mut chunks = Vec::with_capacity(n);
    let mut seen_parts: HashMap<usize, usize> = HashMap::new();
    for i in 0..n {
        let (t, t_end) = (bounds[i], bounds[i + 1]);
        let (audio, stats, transcript) = merge(&groups[i], t_end - t);
        let k = images.partition_point(|im| im.0 < t - 1e-6);
        let thumb = if images.is_empty() {
            None
        } else if k < images.len() && images[k].0 < t_end - 1e-6 {
            Some((k, images[k].0, true))
        } else if k > 0 {
            Some((k - 1, images[k - 1].0, false))
        } else {
            Some((k, images[k].0, false))
        };
        let (scene_id, part) = match scene_of.get(i) {
            Some(&s) => {
                let seen = seen_parts.entry(s).or_default();
                *seen += 1;
                let parts = parts_of[&s];
                (Some(s), (parts > 1).then_some((*seen - 1, parts)))
            }
            None => (None, None),
        };
        chunks.push(ChunkRec { id: make_id(&fp, ph, i, t, t_end), index: i, t, t_end, scene_id, part, thumb, audio, stats, transcript });
    }
    if !spec.sidecars.is_empty() && chunks.iter().all(|c| c.transcript.is_none()) {
        warnings.push("transcript: the sidecars produced no text for this range".into());
    }
    let index = chunks.iter().map(|c| (c.id.clone(), c.index)).collect();
    Ok(Table { key, input: a.input.clone(), fingerprint: fp, spec: spec.clone(), have, chunks, index, images, start, end, warnings })
}

// ---- descriptors and pages -----------------------------------------------------------------------

/// The pictures of a response, each once, in the order the descriptors refer to them.
#[derive(Default)]
struct PageImages {
    slots: HashMap<usize, usize>,
    list: Vec<Value>,
    chars: usize,
}

fn descriptor(t: &Table, c: &ChunkRec, f: Flags, pics: Option<&mut PageImages>) -> Value {
    let mut d = Map::new();
    d.insert("id".into(), json!(c.id));
    d.insert("index".into(), json!(c.index));
    d.insert("t".into(), json!(c.t));
    d.insert("t_end".into(), json!(c.t_end));
    if let Some(s) = c.scene_id {
        d.insert("scene_id".into(), json!(s));
    }
    if let Some((p, n)) = c.part {
        d.insert("part".into(), json!(p));
        d.insert("parts".into(), json!(n));
    }
    d.insert("uri".into(), json!(format!("{URI_PREFIX}{}", c.id)));
    if f.thumbnail {
        if let Some((k, at, exact)) = c.thumb {
            let mut th = Map::new();
            th.insert("t".into(), json!(round3(at)));
            if !exact {
                th.insert("exact".into(), json!(false));
            }
            if let Some(p) = pics {
                let slot = *p.slots.entry(k).or_insert_with(|| {
                    p.chars += t.images[k].1.len();
                    p.list.push(json!({ "chunk": c.id, "t": round3(at), "png_base64": t.images[k].1 }));
                    p.list.len() - 1
                });
                th.insert("image".into(), json!(slot));
            }
            d.insert("thumbnail".into(), Value::Object(th));
        }
    }
    if f.audio {
        if let Some(a) = &c.audio {
            d.insert("audio".into(), a.clone());
        }
    }
    if f.stats {
        if let Some(s) = &c.stats {
            d.insert("stats".into(), s.clone());
        }
    }
    if f.transcript {
        if let Some(s) = &c.transcript {
            d.insert("transcript".into(), s.clone());
        }
    }
    Value::Object(d)
}

fn cursor_for(t: &Table, at: usize) -> String {
    format!("{:08x}.{at}", (hash_str(&t.key) >> 32) as u32)
}

fn parse_cursor(t: &Table, c: &str) -> Result<usize> {
    let bad = || Error::invalid_arg(format!("cursor '{c}' is not one of these arguments' cursors")).hint("use the next_cursor of a response to the same arguments (same file, mode, seconds, from and to)");
    let (h, at) = c.split_once('.').ok_or_else(bad)?;
    if h != format!("{:08x}", (hash_str(&t.key) >> 32) as u32) {
        return Err(bad());
    }
    at.parse::<usize>().ok().filter(|a| *a <= t.chunks.len()).ok_or_else(bad)
}

/// The `chunk` tool.
pub fn chunk(args: &Value, ctx: &Ctx) -> Result<Value> {
    let a: ChunkArgs = serde_json::from_value(args.clone()).map_err(|e| Error::invalid_arg(format!("invalid chunk arguments: {e}")).hint("the schema is in `ironav tools` (tool 'chunk')"))?;
    crate::policy::check_read(&a.input)?;
    let spec = Spec::parse(&a)?;
    let flags = Flags::parse(&a.include)?;
    let fp = fingerprint(&a.input)?;
    let key = spec.key(&fp);
    let want = flags.have();
    let (table, was_cached) = match cached(&key).filter(|t| t.have.covers(want)) {
        Some(t) => (t, true),
        None => {
            // a table that lacks what is wanted is built again with what it had too
            let have = cached(&key).map_or(want, |t| t.have.union(want));
            let t = Arc::new(build_table(&a, &spec, fp, key, have, ctx)?);
            remember(t.clone());
            (t, false)
        }
    };
    page(&table, &a, flags, was_cached)
}

fn page(t: &Table, a: &ChunkArgs, mut flags: Flags, was_cached: bool) -> Result<Value> {
    let total = t.chunks.len();
    let start = match &a.cursor {
        Some(c) => parse_cursor(t, c)?,
        None => 0,
    };
    let asked = a.limit.unwrap_or(20).clamp(1, 1000);
    let n0 = asked.min(total - start);
    let mut n = n0;
    let budget = a.budget_tokens.map(|b| b.saturating_mul(4));
    let mut dropped = Vec::new();
    let (pics, chunks) = loop {
        let mut pics = PageImages::default();
        let chunks = t.chunks[start..start + n].iter().map(|c| descriptor(t, c, flags, Some(&mut pics))).collect::<Vec<_>>();
        let size = chunks.iter().map(|c| c.to_string().len()).sum::<usize>() + pics.chars;
        if budget.is_none_or(|b| size <= b) {
            break (pics, chunks);
        }
        if let Some(name) = flags.drop_next() {
            dropped.push(name);
        } else if n > 1 {
            n = n.div_ceil(2);
        } else {
            // one chunk with nothing left to drop: over budget, but not empty
            break (pics, chunks);
        }
    };
    let mut warnings = t.warnings.clone();
    if flags.transcript && t.spec.sidecars.is_empty() {
        warnings.push("transcript: no sidecars were named, so there is no text; pass `sidecars` (the names in IRONAV_SIDECARS)".into());
    }
    let next = (start + n < total).then(|| cursor_for(t, start + n));
    let mut r = json!({
        "input": t.input,
        "fingerprint": t.fingerprint,
        "mode": t.spec.mode.name(),
        "params": match t.spec.mode {
            Mode::Time => json!({ "seconds": t.spec.seconds, "from": t.spec.from, "to": t.spec.to }),
            Mode::Scene => json!({ "min_seconds": t.spec.min, "max_seconds": t.spec.max, "from": t.spec.from, "to": t.spec.to }),
        },
        "range": [t.start, t.end],
        "total_chunks": total,
        "chunks": chunks,
        "next_cursor": next,
        "cached": was_cached,
    });
    if !pics.list.is_empty() {
        r["images"] = Value::Array(pics.list);
    }
    if !dropped.is_empty() || n < n0 {
        let mut tr = Map::new();
        if !dropped.is_empty() {
            tr.insert("dropped".into(), json!(dropped));
        }
        if n < n0 {
            tr.insert("chunks_on_page".into(), json!(n));
        }
        r["trimmed"] = Value::Object(tr);
    }
    if !warnings.is_empty() {
        r["warnings"] = json!(warnings);
    }
    if let Some(b) = a.budget_tokens {
        let est = r["chunks"].to_string().len().div_ceil(4);
        r["budget"] = json!({ "tokens": b, "estimated_tokens": est });
    }
    Ok(r)
}

/// What `resources/read` of `ironav://chunks/{id}` returns: the descriptor of a chunk of a table still
/// in memory, with everything the table holds and no pixels.
pub fn resource(uri: &str) -> Result<Value> {
    let id = uri.strip_prefix(URI_PREFIX).filter(|i| !i.is_empty() && !i.contains('/')).ok_or_else(|| Error::invalid_arg(format!("'{uri}' is not a chunk URI")).hint(format!("a chunk's URI is {URI_TEMPLATE}")))?;
    if id.starts_with(crate::live::chunks::ID_PREFIX) {
        return crate::live::chunks::resource(id);
    }
    let Some((t, i)) = find(id) else {
        return Err(Error::new(ErrorCode::NotFound, format!("chunk '{id}' is not in memory"))
            .hint("call the `chunk` tool on the file first (chunk tables are kept in memory, a few at a time, and are lost when the server restarts); then read the uri it returns"));
    };
    crate::policy::check_read(&t.input)?;
    let all = Flags { thumbnail: true, audio: true, transcript: true, stats: true };
    let mut d = descriptor(&t, &t.chunks[i], all, None);
    d["input"] = json!(t.input);
    d["fingerprint"] = json!(t.fingerprint);
    d["mode"] = json!(t.spec.mode.name());
    d["total_chunks"] = json!(t.chunks.len());
    d["hint"] = json!("pictures and sound of this chunk: the `chunk_media` tool with this id");
    Ok(d)
}

// ---- chunk_media ---------------------------------------------------------------------------------

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
struct MediaArgs {
    #[serde(default)]
    input: Option<String>,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    t: Option<f64>,
    #[serde(default)]
    t_end: Option<f64>,
    #[serde(default)]
    frames: Option<usize>,
    #[serde(default)]
    audio: bool,
    #[serde(default)]
    clip: bool,
    #[serde(default)]
    max_side: Option<u32>,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    audio_seconds: Option<f64>,
    #[serde(default)]
    max_bytes: Option<usize>,
    #[serde(default)]
    hwaccel: Option<String>,
}

/// Opens the media a chunk's pictures, sound and clip are taken from, afresh on each call: a file, or a
/// live session's retained packets.
pub(crate) type Opener<'a> = &'a dyn Fn() -> Result<Box<dyn Demuxer>>;

fn check_range(t: f64, t_end: f64) -> Result<()> {
    // NaN and infinities fail this too
    if !(t >= 0.0 && t_end > t && t_end.is_finite()) {
        return Err(Error::invalid_arg("the range must have 0 <= t < t_end"));
    }
    Ok(())
}

/// The `chunk_media` tool.
pub fn chunk_media(args: &Value, ctx: &Ctx) -> Result<Value> {
    let a: MediaArgs = serde_json::from_value(args.clone()).map_err(|e| Error::invalid_arg(format!("invalid chunk_media arguments: {e}")).hint("the schema is in `ironav tools` (tool 'chunk_media')"))?;
    // a live chunk: the session is in the id, and the media is what it still retains
    if let Some(id) = a.id.as_deref().filter(|i| i.starts_with(crate::live::chunks::ID_PREFIX)) {
        if a.input.is_some() || a.t.is_some() || a.t_end.is_some() {
            return Err(Error::invalid_arg("a live chunk id is given alone").hint("drop `input`, `t` and `t_end`: the id names the session and the range"));
        }
        let (session, t, t_end) = crate::live::chunks::parse_id(id).ok_or_else(|| Error::invalid_arg(format!("'{id}' is not a chunk id")).hint("live chunk ids come from `stream_chunks`"))?;
        check_range(t, t_end)?;
        let (streams, packets) = crate::live::get(session)?.retained(t, t_end)?;
        let open = move || -> Result<Box<dyn Demuxer>> { Ok(Box::new(crate::live::Retained::new(streams.clone(), packets.clone()))) };
        let mut r = json!({ "session": session, "id": id, "uri": format!("{URI_PREFIX}{id}"), "t": t, "t_end": t_end });
        fetch(&open, &a, t, t_end, ctx, &mut r)?;
        return Ok(r);
    }
    let input = a.input.clone().ok_or_else(|| Error::invalid_arg("missing required argument 'input'").hint("give the file; a live chunk id from `stream_chunks` needs no input"))?;
    crate::policy::check_read(&input)?;
    let (t, t_end) = match (&a.id, a.t, a.t_end) {
        (Some(id), None, None) => {
            match find(id) {
                Some((table, i)) => {
                    if table.input != input {
                        return Err(Error::invalid_arg(format!("chunk '{id}' belongs to '{}', not '{input}'", table.input)));
                    }
                    (table.chunks[i].t, table.chunks[i].t_end)
                }
                // not cached: the id carries its range, and says which file content it is for
                None => match parse_id(id) {
                    Some((fp, t, t_end)) if fp == fingerprint(&input)? => (t, t_end),
                    Some(_) => {
                        return Err(Error::new(ErrorCode::NotFound, format!("chunk '{id}' is not of this file as it is now"))
                            .hint("the id names another file, or this one has changed since; call `chunk` again, or give t and t_end"))
                    }
                    None => return Err(Error::invalid_arg(format!("'{id}' is not a chunk id")).hint("ids come from the `chunk` tool; or give t and t_end")),
                },
            }
        }
        (None, Some(t), Some(t_end)) => (t, t_end),
        _ => return Err(Error::invalid_arg("give `id` (from `chunk`), or both `t` and `t_end`")),
    };
    check_range(t, t_end)?;
    let mut r = json!({ "input": input, "t": t, "t_end": t_end });
    if let Some(id) = &a.id {
        r["id"] = json!(id);
        r["uri"] = json!(format!("{URI_PREFIX}{id}"));
    }
    let open = || open_demuxer(Path::new(&input));
    fetch(&open, &a, t, t_end, ctx, &mut r)?;
    Ok(r)
}

/// The pictures, sound and clip of `t..t_end` that `a` asks for, added to `r`.
fn fetch(open: Opener, a: &MediaArgs, t: f64, t_end: f64, ctx: &Ctx, r: &mut Value) -> Result<()> {
    check_range(t, t_end)?;
    let png = match a.format.as_deref().unwrap_or("jpeg") {
        "jpeg" => false,
        "png" => true,
        other => return Err(Error::invalid_arg(format!("unknown format '{other}'")).alternatives(["jpeg", "png"])),
    };
    let side_cap = crate::policy::current().max_side.min(1024);
    let max_side = a.max_side.unwrap_or(320).clamp(16, side_cap.max(16));
    let max_bytes = a.max_bytes.unwrap_or(1 << 20).clamp(4096, MAX_INLINE_BYTES);
    let frames = a.frames.unwrap_or(if a.audio || a.clip { 0 } else { 4 });
    if frames > MAX_FRAMES {
        return Err(Error::invalid_arg(format!("frames is at most {MAX_FRAMES}")));
    }
    let hwaccel = a.hwaccel.clone().unwrap_or_else(|| "auto".into());
    let mut warnings = Vec::new();
    if frames > 0 {
        r["frames"] = Value::Array(grab_frames(open, t, t_end, frames, max_side, png, &hwaccel, ctx)?);
    }
    if a.audio {
        let secs = a.audio_seconds.unwrap_or(10.0).clamp(0.1, MAX_AUDIO_SECONDS);
        match grab_audio(open, t, t_end, secs, max_bytes, ctx)? {
            Some(v) => r["audio"] = v,
            None => warnings.push("audio: the file has no audio stream".into()),
        }
    }
    if a.clip {
        match grab_clip(open, t, t_end, max_bytes, ctx) {
            Ok(v) => r["clip"] = v,
            Err(e) if e.code == ErrorCode::Cancelled => return Err(e),
            Err(e) => warnings.push(format!("clip: {}", e.message)),
        }
    }
    if !warnings.is_empty() {
        r["warnings"] = json!(warnings);
    }
    Ok(())
}

fn video_stream(d: &dyn Demuxer) -> Result<StreamInfo> {
    d.streams().iter().find(|s| s.params.media_type() == MediaType::Video).cloned().ok_or_else(|| Error::invalid_arg("the file has no video stream").hint("ask for audio instead, or use another file"))
}

fn cancelled(ctx: &Ctx) -> Result<()> {
    if ctx.cancelled() {
        return Err(Error::new(ErrorCode::Cancelled, "chunk_media cancelled"));
    }
    Ok(())
}

/// `n` pictures at the middles of `n` equal parts of `t..t_end`.
#[allow(clippy::too_many_arguments)]
fn grab_frames(open: Opener, t: f64, t_end: f64, n: usize, max_side: u32, png: bool, hwaccel: &str, ctx: &Ctx) -> Result<Vec<Value>> {
    let info = video_stream(open()?.as_ref())?;
    crate::policy::current().check_decode(&info)?;
    let mut out = Vec::new();
    for k in 0..n {
        cancelled(ctx)?;
        let target = t + (k as f64 + 0.5) * (t_end - t) / n as f64;
        let Some((at, pic)) = picture_at(open, &info, target, hwaccel, ctx)? else { continue };
        let (w, h, bytes) = if png {
            let (w, h, rgb) = ironav_player::rgb_frame(&pic, Some(max_side), Some(max_side));
            (w, h, ironav_player::to_png(w, h, &rgb)?)
        } else {
            jpeg_of(&pic, max_side)?
        };
        let key = if png { "png_base64" } else { "jpeg_base64" };
        let mut f = json!({ "t": round3(at), "target": round3(target), "width": w, "height": h });
        f[key] = json!(ironav_player::base64(&bytes));
        out.push(f);
    }
    Ok(out)
}

pub(crate) fn jpeg_of(v: &VideoFrame, max_side: u32) -> Result<(u32, u32, Vec<u8>)> {
    use ironav_core::PixelFormat as P;
    let s = (max_side as f64 / v.width.max(v.height) as f64).min(1.0);
    let mut pic = if s < 1.0 {
        let (w, h) = (((v.width as f64 * s).round() as u32).max(2) & !1, ((v.height as f64 * s).round() as u32).max(2) & !1);
        ironav_codec::resize_frame(v, w, h)
    } else {
        v.clone()
    };
    if !matches!(pic.pix_fmt, P::Yuv420p | P::Yuv422p | P::Yuv444p | P::Gray8) {
        pic = ironav_codec::convert_pixel_format(pic, P::Yuv420p, ironav_codec::Matrix::Bt709);
    }
    let jpeg = ironav_codec::jpeg::encode_jpeg(&pic, 80)?;
    Ok((pic.width, pic.height, jpeg))
}

/// The decoded picture shown at `target` (the nearer of the two around it), and its time.
fn picture_at(open: Opener, info: &StreamInfo, target: f64, hwaccel: &str, ctx: &Ctx) -> Result<Option<(f64, VideoFrame)>> {
    let mut d = open()?;
    for s in d.streams().to_vec() {
        if s.index != info.index {
            d.discard(s.index);
        }
    }
    // a file that cannot seek is read from the start
    let _ = d.seek(target);
    let (mut dec, _) = ironav_player::open_video_decoder_with(info, hwaccel, &ironav_codec::DecoderOptions { low_delay: true })?;
    let tb = info.time_base.as_f64();
    let mut before: Option<(f64, VideoFrame)> = None;
    let mut packets = 0;
    let pick = |frames: Vec<Frame>, before: &mut Option<(f64, VideoFrame)>| -> Option<(f64, VideoFrame)> {
        for f in frames {
            let Frame::Video(v) = f else { continue };
            let ft = v.pts as f64 * tb;
            if ft >= target {
                return Some(match before.take() {
                    Some((bt, b)) if target - bt < ft - target => (bt, b),
                    _ => (ft, v),
                });
            }
            *before = Some((ft, v));
        }
        None
    };
    while let Some(p) = d.read_packet()? {
        if p.stream != info.index {
            continue;
        }
        packets += 1;
        if packets > MAX_PACKETS_PER_PICTURE {
            break;
        }
        if packets % 256 == 0 {
            cancelled(ctx)?;
        }
        // a decoder started mid-stream may miss references for a few pictures: keep going
        let frames = dec.decode(&p).unwrap_or_default();
        if let Some(found) = pick(frames, &mut before) {
            return Ok(Some(found));
        }
    }
    let rest = dec.flush().unwrap_or_default();
    Ok(pick(rest, &mut before).or(before))
}

/// A WAV of `t..t_end` (at most `seconds` and `max_bytes` of it), mixed down to mono 16-bit.
fn grab_audio(open: Opener, t: f64, t_end: f64, seconds: f64, max_bytes: usize, ctx: &Ctx) -> Result<Option<Value>> {
    let mut d = open()?;
    let Some(info) = d.streams().iter().find(|s| s.params.media_type() == MediaType::Audio).cloned() else { return Ok(None) };
    for s in d.streams().to_vec() {
        if s.index != info.index {
            d.discard(s.index);
        }
    }
    let mut dec = crate::policy::new_decoder_for(&info)?;
    let _ = d.seek(t);
    let tb = info.time_base.as_f64();
    let want_end = t_end.min(t + seconds);
    let cap_samples = (max_bytes.saturating_sub(44)) / 2;
    let mut samples: Vec<i16> = Vec::new();
    let (mut rate, mut first_at, mut next) = (0u32, None::<f64>, None::<f64>);
    let mut packets = 0;
    let mut done = false;
    let take = |frames: Vec<Frame>, samples: &mut Vec<i16>, rate: &mut u32, first_at: &mut Option<f64>, next: &mut Option<f64>| -> bool {
        for f in frames {
            let Frame::Audio(a) = f else { continue };
            let stamped = a.pts as f64 * tb;
            let fs = match *next {
                Some(n) if (n - stamped).abs() < 0.1 => n,
                _ => stamped,
            };
            let (sr, ch) = (a.sample_rate.max(1), a.channels.max(1) as usize);
            let nf = a.frames();
            *next = Some(fs + nf as f64 / sr as f64);
            if fs >= want_end {
                return true;
            }
            let i0 = (((t - fs) * sr as f64).round().max(0.0) as usize).min(nf);
            let i1 = (((want_end - fs) * sr as f64).round().max(0.0) as usize).min(nf);
            if i1 <= i0 {
                continue;
            }
            *rate = sr;
            first_at.get_or_insert(fs + i0 as f64 / sr as f64);
            for i in i0..i1 {
                if samples.len() >= cap_samples {
                    return true;
                }
                let s: f64 = a.samples[i * ch..(i + 1) * ch].iter().sum::<f64>() / ch as f64;
                samples.push((s.clamp(-1.0, 1.0) * 32767.0).round() as i16);
            }
            if i1 < nf {
                return true;
            }
        }
        false
    };
    while !done {
        let Some(p) = d.read_packet()? else { break };
        if p.stream != info.index {
            continue;
        }
        packets += 1;
        if packets % 256 == 0 {
            cancelled(ctx)?;
        }
        done = take(dec.decode(&p).unwrap_or_default(), &mut samples, &mut rate, &mut first_at, &mut next);
    }
    if !done {
        take(dec.flush().unwrap_or_default(), &mut samples, &mut rate, &mut first_at, &mut next);
    }
    if samples.is_empty() {
        return Ok(Some(json!({ "t": t, "t_end": t, "sample_rate": rate, "channels": 1, "seconds": 0.0, "bytes": 0, "truncated": false, "note": "no sound in this range" })));
    }
    let secs = samples.len() as f64 / rate as f64;
    let at = first_at.unwrap_or(t);
    let wav = wav_bytes(&samples, rate);
    Ok(Some(json!({
        "t": round3(at),
        "t_end": round3(at + secs),
        "sample_rate": rate,
        "channels": 1,
        "seconds": round3(secs),
        "bytes": wav.len(),
        "truncated": at + secs < t_end - 0.05,
        "mime": "audio/wav",
        "wav_base64": ironav_player::base64(&wav),
    })))
}

fn wav_bytes(samples: &[i16], rate: u32) -> Vec<u8> {
    let data = samples.len() * 2;
    let mut w = Vec::with_capacity(44 + data);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data as u32).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&rate.to_le_bytes());
    w.extend_from_slice(&(rate * 2).to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&16u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(data as u32).to_le_bytes());
    w.extend(samples.iter().flat_map(|s| s.to_le_bytes()));
    w
}

/// The packets of `t..t_end` (from the keyframe at or before `t`), copied into a fragmented MP4 in memory.
fn grab_clip(open: Opener, t: f64, t_end: f64, max_bytes: usize, ctx: &Ctx) -> Result<Value> {
    let mut d = open()?;
    let streams = d.streams().to_vec();
    let video = streams.iter().find(|s| s.params.media_type() == MediaType::Video).cloned();
    let audio = streams.iter().find(|s| s.params.media_type() == MediaType::Audio).cloned();
    // the writer takes one of each, numbered from 0 in the order given
    let mut chosen: Vec<StreamInfo> = video.iter().chain(audio.iter()).cloned().collect();
    let mut writer = None;
    while writer.is_none() && !chosen.is_empty() {
        let renumbered: Vec<StreamInfo> = chosen.iter().enumerate().map(|(i, s)| StreamInfo { index: i, ..s.clone() }).collect();
        match ironav_formats::FragmentedMp4Writer::new(Vec::new(), &renumbered, 2.0) {
            Ok(w) => writer = Some(w),
            Err(e) if chosen.len() > 1 => {
                // maybe one of the two cannot go in an MP4
                let _ = e;
                chosen.pop();
            }
            Err(e) => return Err(Error::new(ErrorCode::UnsupportedFormat, format!("these streams cannot be copied into an MP4: {}", e.message))),
        }
    }
    let mut writer = writer.ok_or_else(|| Error::invalid_arg("the file has no video or audio stream"))?;
    let map: HashMap<usize, usize> = chosen.iter().enumerate().map(|(i, s)| (s.index, i)).collect();
    for s in &streams {
        if !map.contains_key(&s.index) {
            d.discard(s.index);
        }
    }
    let landed = d.seek(t).unwrap_or(0.0);
    let mut done: Vec<bool> = vec![false; chosen.len()];
    let (mut bytes, mut truncated, mut packets, mut last_end) = (0usize, false, 0usize, t);
    while let Some(mut p) = d.read_packet()? {
        let Some(&i) = map.get(&p.stream) else { continue };
        packets += 1;
        if packets % 256 == 0 {
            cancelled(ctx)?;
        }
        let at = p.pts as f64 * chosen[i].time_base.as_f64();
        if at > t_end {
            done[i] = true;
            if done.iter().all(|d| *d) {
                break;
            }
            continue;
        }
        if bytes + p.data.len() > max_bytes / 2 {
            truncated = true;
            break;
        }
        bytes += p.data.len();
        last_end = last_end.max(at);
        p.stream = i;
        writer.write_packet(&p)?;
    }
    let mp4 = writer.finish()?;
    if mp4.len() > max_bytes {
        return Err(Error::invalid_arg(format!("the clip is {} bytes, over the {max_bytes} allowed", mp4.len())).hint("ask for a shorter range, or raise max_bytes"));
    }
    Ok(json!({
        "from": round3(landed.min(t)),
        "t_end": round3(last_end),
        "streams": chosen.iter().map(|s| s.codec.name()).collect::<Vec<_>>(),
        "bytes": mp4.len(),
        "truncated": truncated,
        "copied": true,
        "mime": "video/mp4",
        "mp4_base64": ironav_player::base64(&mp4),
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn time_bounds_tile_the_range() {
        let b = time_bounds(0.0, 10.0, 3.0).unwrap();
        assert_eq!(b, [0.0, 3.0, 6.0, 9.0, 10.0]);
        assert_eq!(time_bounds(2.0, 8.0, 3.0).unwrap(), [2.0, 5.0, 8.0]);
        assert_eq!(time_bounds(0.0, 0.0, 3.0).unwrap(), [0.0]);
    }

    #[test]
    fn scenes_merge_below_min_and_split_above_max() {
        // cuts at 1, 1.5, 6; min 1 drops 1.5; max 3 splits 1..6 in two
        let (b, s) = scene_bounds(0.0, 7.0, &[1.0, 1.5, 6.0], 1.0, 3.0).unwrap();
        assert_eq!(b, [0.0, 1.0, 3.5, 6.0, 7.0]);
        assert_eq!(s, [0, 1, 1, 2]);
        // a short last scene joins the one before it
        let (b, _) = scene_bounds(0.0, 6.5, &[3.0, 6.0], 1.0, 60.0).unwrap();
        assert_eq!(b, [0.0, 3.0, 6.5]);
    }

    #[test]
    fn figures_combine_by_what_they_are() {
        let d1 = json!({ "audio1": { "levels": { "peak_dbfs": -20.0, "rms_dbfs": -30.0 }, "silence": { "silent_seconds": 1.0 } } });
        let d2 = json!({ "audio1": { "levels": { "peak_dbfs": -10.0, "rms_dbfs": -30.0 }, "silence": { "silent_seconds": 0.5 } } });
        let (a, b) = (Digest { t: 0.0, t_end: 1.0, data: d1.as_object().unwrap() }, Digest { t: 1.0, t_end: 2.0, data: d2.as_object().unwrap() });
        let (audio, stats, text) = merge(&[&a, &b], 2.0);
        let m = &audio.unwrap()["audio1"];
        assert_eq!(m["peak_dbfs"], -10.0);
        assert_eq!(m["rms_dbfs"], -30.0);
        assert_eq!(m["silent_seconds"], 1.5);
        assert!(stats.is_none() && text.is_none());
    }

    #[test]
    fn a_wav_has_a_header_and_the_samples() {
        let w = wav_bytes(&[1, -1, 2], 8000);
        assert_eq!(&w[..4], b"RIFF");
        assert_eq!(w.len(), 44 + 6);
        assert_eq!(u32::from_le_bytes(w[24..28].try_into().unwrap()), 8000);
    }
}
