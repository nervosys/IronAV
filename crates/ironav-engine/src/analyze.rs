//! Running the analyzers: [`Analysis`] takes packets one at a time, decodes what the chosen
//! analyzers need, feeds them in presentation order and hands out what they observe, with a
//! digest per window and a summary at the end. [`analyze`] drives it over a file; a live session
//! drives the same thing from a socket.

use crate::sidecar::Pending;
use crate::tools::Ctx;
pub use ironav_analyze::Level;
use ironav_analyze::{analyzer, level_of, Analyzer, AnalyzerInfo, Consumes, Input, LowRes, Observation, Settings, ANALYZERS};
use ironav_core::{Decoder, Error, ErrorCode, Frame, MediaType, Packet, PixelFormat, Result, StreamInfo, StreamParams, VideoFrame};
use ironav_formats::open_demuxer;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AnalyzeJob {
    pub input: String,
    #[serde(flatten)]
    pub opts: AnalyzeOptions,
    #[serde(default)]
    pub from: Option<f64>,
    #[serde(default)]
    pub to: Option<f64>,
    /// Events kept in the result beyond this are counted but dropped (digests are always kept).
    #[serde(default = "default_max")]
    pub max_observations: usize,
    /// Write each thumbnail here as a PNG file.
    #[serde(default)]
    pub thumbnail_dir: Option<String>,
    /// Return the first N thumbnails as images.
    #[serde(default)]
    pub inline_thumbnails: usize,
}

/// What to analyze and how, for files and live sessions alike.
#[derive(Debug, Clone, Deserialize)]
pub struct AnalyzeOptions {
    /// Analyzer names; empty means all of them.
    #[serde(default)]
    pub analyzers: Vec<String>,
    #[serde(default = "default_level")]
    pub level: String,
    #[serde(default = "default_digest")]
    pub digest_seconds: f64,
    /// Streams to analyze; empty means every audio and video stream.
    #[serde(default)]
    pub streams: Vec<usize>,
    #[serde(default)]
    pub thumbnail_seconds: Option<f64>,
    #[serde(default)]
    pub thumbnail_width: Option<u32>,
    #[serde(default = "default_hwaccel")]
    pub hwaccel: String,
    /// Model sidecars, by the names the operator gave them in `IRONAV_SIDECARS`.
    #[serde(default)]
    pub sidecars: Vec<String>,
}

impl Default for AnalyzeOptions {
    fn default() -> Self {
        AnalyzeOptions {
            analyzers: Vec::new(),
            level: default_level(),
            digest_seconds: default_digest(),
            streams: Vec::new(),
            thumbnail_seconds: None,
            thumbnail_width: None,
            hwaccel: default_hwaccel(),
            sidecars: Vec::new(),
        }
    }
}

fn default_level() -> String {
    "events".into()
}
fn default_digest() -> f64 {
    10.0
}
fn default_max() -> usize {
    500
}
fn default_hwaccel() -> String {
    "auto".into()
}

/// One analyzed stream.
struct Stream {
    info: StreamInfo,
    /// `video0`, `audio1`: how digests and summaries name it.
    label: String,
    decoder: Option<Box<dyn Decoder>>,
    decoder_name: Option<&'static str>,
    packet_analyzers: Vec<(&'static str, Box<dyn Analyzer>)>,
    frame_analyzers: Vec<(&'static str, Box<dyn Analyzer>)>,
    /// Where the next audio frame starts, counted in samples, so rounding in timestamps cannot drift.
    audio_next: Option<f64>,
    last_video: Option<f64>,
    end: f64,
    decode_errors: u64,
    /// A decoder started mid-stream waits for a keyframe.
    wait_key: bool,
}

impl Stream {
    fn analyzers(&mut self) -> impl Iterator<Item = &mut (&'static str, Box<dyn Analyzer>)> {
        self.packet_analyzers.iter_mut().chain(self.frame_analyzers.iter_mut())
    }
    fn names(&self) -> Vec<&'static str> {
        self.packet_analyzers.iter().chain(&self.frame_analyzers).map(|(n, _)| *n).collect()
    }
}

/// The luma of `v` averaged over 4x4 blocks, in 8-bit levels.
fn low_res(v: &VideoFrame) -> LowRes {
    let (w, h) = (v.width as usize, v.height as usize);
    match v.pix_fmt {
        PixelFormat::Rgb24 => {
            let p = &v.planes[0];
            LowRes::new(w / 4, h / 4, |x, y| {
                let o = (y * w + x) * 3;
                (p[o] as i32 * 66 + p[o + 1] as i32 * 129 + p[o + 2] as i32 * 25 + 128 >> 8) + 16
            })
        }
        f if f.bit_depth() > 8 => LowRes::of_plane(&v.planes[0], w, 2, (f.bit_depth() - 8) as u32, w, h),
        _ => LowRes::of_plane(&v.planes[0], w, 1, 0, w, h),
    }
}

fn round3(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// Analyzers over a set of streams, fed one packet at a time.
pub struct Analysis {
    level: Level,
    digest_seconds: f64,
    from: f64,
    to: f64,
    streams: Vec<Option<Stream>>,
    window: Option<f64>,
    latest: f64,
    warnings: Vec<String>,
    /// The newest decoded picture and its time, kept for snapshots.
    pub last_picture: Option<(f64, VideoFrame)>,
    out: Vec<Observation>,
    /// What `configure` needs to add analyzers later.
    settings: Settings,
    hwaccel: String,
}

/// A change to a running analysis.
#[derive(Debug, Clone, Default)]
pub struct Reconfigure {
    pub level: Option<String>,
    pub digest_seconds: Option<f64>,
    pub add: Vec<String>,
    pub remove: Vec<String>,
}

impl Reconfigure {
    /// Checks names and values, so a request learns of a mistake at once.
    pub fn check(&self) -> Result<()> {
        if let Some(l) = &self.level {
            Level::parse(l).ok_or_else(|| Error::invalid_arg(format!("unknown level '{l}'")).alternatives(Level::ALL.iter().map(|l| l.name())))?;
        }
        if self.digest_seconds.is_some_and(|d| !(d > 0.0)) {
            return Err(Error::invalid_arg("digest_seconds must be above 0"));
        }
        for n in self.add.iter().chain(&self.remove) {
            analyzer(n).ok_or_else(|| Error::invalid_arg(format!("unknown analyzer '{n}'")).alternatives(ANALYZERS.iter().map(|a| a.name)))?;
        }
        if let Some(n) = self.add.iter().find(|n| self.remove.contains(n)) {
            return Err(Error::invalid_arg(format!("'{n}' is both added and removed")));
        }
        Ok(())
    }
}

impl Analysis {
    /// Analysis of `infos` (a file's streams, or a live source's) over `from..to` seconds. Sidecars
    /// start once there are streams; a `live` source drops what a slow one cannot take.
    pub fn new(infos: &[StreamInfo], opts: &AnalyzeOptions, from: f64, to: f64, live: bool) -> Result<Analysis> {
        let level = Level::parse(&opts.level)
            .ok_or_else(|| Error::invalid_arg(format!("unknown level '{}'", opts.level)).alternatives(Level::ALL.iter().map(|l| l.name())))?;
        if !(opts.digest_seconds > 0.0) {
            return Err(Error::invalid_arg("digest_seconds must be above 0"));
        }
        let chosen: Vec<&AnalyzerInfo> = if opts.analyzers.is_empty() {
            ANALYZERS.iter().collect()
        } else {
            opts.analyzers
                .iter()
                .map(|n| analyzer(n).ok_or_else(|| Error::invalid_arg(format!("unknown analyzer '{n}'")).alternatives(ANALYZERS.iter().map(|a| a.name))))
                .collect::<Result<_>>()?
        };
        let settings = Settings {
            thumbnail_seconds: opts.thumbnail_seconds.unwrap_or(Settings::default().thumbnail_seconds),
            thumbnail_width: opts.thumbnail_width.unwrap_or(Settings::default().thumbnail_width),
        };
        let wanted: Vec<usize> = if opts.streams.is_empty() {
            infos.iter().filter(|s| matches!(s.params.media_type(), MediaType::Audio | MediaType::Video)).map(|s| s.index).collect()
        } else {
            for &i in &opts.streams {
                if i >= infos.len() {
                    return Err(Error::invalid_arg(format!("stream {i} does not exist; there are {}", infos.len())));
                }
            }
            opts.streams.clone()
        };
        let sidecars = crate::sidecar::resolve(&opts.sidecars)?;
        let mut pending = Vec::new();
        if !infos.is_empty() {
            for (name, cfg) in &sidecars {
                pending.push(Pending::start(name, cfg, live)?);
            }
        }
        let mut warnings = Vec::new();
        let mut streams = Vec::new();
        for info in infos {
            if !wanted.contains(&info.index) {
                streams.push(None);
                continue;
            }
            let media = info.params.media_type();
            let label = format!("{}{}", if media == MediaType::Video { "video" } else { "audio" }, info.index);
            let mine: Vec<&AnalyzerInfo> = chosen.iter().copied().filter(|a| a.consumes.applies_to(media)).collect();
            let (pa, fa): (Vec<_>, Vec<_>) = mine.into_iter().partition(|a| a.consumes == Consumes::Packets);
            // each sidecar takes the first analyzed stream of the kind it wants
            let mut cars = Vec::new();
            let mut k = 0;
            while k < pending.len() {
                if pending[k].media() == media {
                    cars.push(pending.remove(k));
                } else {
                    k += 1;
                }
            }
            let (mut decoder, mut decoder_name) = (None, None);
            if !fa.is_empty() || !cars.is_empty() {
                let made = if media == MediaType::Video {
                    ironav_player::open_video_decoder(info, &opts.hwaccel)
                } else {
                    ironav_codec::new_decoder_for(info).map(|d| (d, "software"))
                };
                match made {
                    Ok((dec, name)) => (decoder, decoder_name) = (Some(dec), Some(name)),
                    Err(e) => warnings.push(format!("stream {}: {} not decoded, so only packet analyzers run on it: {}", info.index, info.codec.name(), e.message)),
                }
            }
            let fa = if decoder.is_some() { fa } else { Vec::new() };
            let mut frame_analyzers: Vec<(&'static str, Box<dyn Analyzer>)> = fa.iter().map(|a| (a.name, a.make(info, &settings))).collect();
            for car in cars {
                if decoder.is_none() {
                    warnings.push(format!("sidecar '{}' not run: stream {} is not decoded", car.name(), info.index));
                    continue;
                }
                let name = car.name();
                frame_analyzers.push((name, Box::new(car.attach(info)?)));
            }
            streams.push(Some(Stream {
                info: info.clone(),
                label,
                decoder,
                decoder_name,
                packet_analyzers: pa.iter().map(|a| (a.name, a.make(info, &settings))).collect(),
                frame_analyzers,
                audio_next: None,
                last_video: None,
                end: 0.0,
                decode_errors: 0,
                wait_key: false,
            }));
        }
        for car in pending {
            let media = if car.media() == MediaType::Video { "video" } else { "audio" };
            warnings.push(format!("sidecar '{}' not run: it wants {media} and no {media} stream is analyzed", car.name()));
        }
        if to <= from {
            return Err(Error::invalid_arg("'to' must be after 'from'"));
        }
        Ok(Analysis {
            level,
            digest_seconds: opts.digest_seconds,
            from,
            to,
            streams,
            window: None,
            latest: from,
            warnings,
            last_picture: None,
            out: Vec::new(),
            settings,
            hwaccel: opts.hwaccel.clone(),
        })
    }

    pub fn level(&self) -> Level {
        self.level
    }

    /// Changes a running analysis: its level, its digest interval (from the next window), and
    /// which built-in analyzers run. An added analyzer starts with what arrives next (a video
    /// decoder started now waits for a keyframe); a removed one is dropped without a summary, and
    /// a stream left with no frame analyzer stops being decoded. Returns warnings.
    pub fn configure(&mut self, c: &Reconfigure) -> Result<Vec<String>> {
        c.check()?;
        if let Some(l) = &c.level {
            self.level = Level::parse(l).unwrap();
        }
        if let Some(d) = c.digest_seconds {
            self.digest_seconds = d;
        }
        let mut warnings = Vec::new();
        for s in self.streams.iter_mut().flatten() {
            let keep = |(n, _): &(&'static str, Box<dyn Analyzer>)| !c.remove.iter().any(|r| r == n);
            s.packet_analyzers.retain(keep);
            s.frame_analyzers.retain(keep);
            let media = s.info.params.media_type();
            for name in &c.add {
                let info = analyzer(name).unwrap();
                if !info.consumes.applies_to(media) || s.names().contains(&info.name) {
                    continue;
                }
                if info.consumes == Consumes::Packets {
                    s.packet_analyzers.push((info.name, info.make(&s.info, &self.settings)));
                    continue;
                }
                if s.decoder.is_none() {
                    let made = if media == MediaType::Video {
                        ironav_player::open_video_decoder(&s.info, &self.hwaccel)
                    } else {
                        ironav_codec::new_decoder_for(&s.info).map(|d| (d, "software"))
                    };
                    match made {
                        Ok((dec, n)) => (s.decoder, s.decoder_name, s.wait_key) = (Some(dec), Some(n), true),
                        Err(e) => {
                            warnings.push(format!("stream {}: '{name}' not added: {} is not decoded: {}", s.info.index, s.info.codec.name(), e.message));
                            continue;
                        }
                    }
                }
                s.frame_analyzers.push((info.name, info.make(&s.info, &self.settings)));
            }
            if s.frame_analyzers.is_empty() && s.decoder.is_some() {
                (s.decoder, s.decoder_name) = (None, None);
                (s.audio_next, s.last_video) = (None, None);
            }
        }
        self.warnings.extend(warnings.iter().cloned());
        Ok(warnings)
    }

    /// Whether stream `i` is analyzed.
    pub fn wants(&self, i: usize) -> bool {
        matches!(self.streams.get(i), Some(Some(_)))
    }

    /// The latest media time shown to the analyzers.
    pub fn position(&self) -> f64 {
        self.latest
    }

    /// Whether every analyzed stream has gone past `to`.
    pub fn past_end(&self) -> bool {
        self.to.is_finite() && self.streams.iter().flatten().all(|s| s.end > self.to)
    }

    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// What is analyzed, per stream.
    pub fn describe(&self) -> Vec<Value> {
        self.streams
            .iter()
            .flatten()
            .map(|s| json!({ "index": s.info.index, "label": s.label, "codec": s.info.codec.name(), "decoder": s.decoder_name, "analyzers": s.names() }))
            .collect()
    }

    /// Shows one packet (and the frames it decodes to) to the analyzers; what they observe goes
    /// to `sink`, in order, already filtered by the level.
    pub fn packet(&mut self, p: &Packet, sink: &mut dyn FnMut(Observation) -> Result<()>) -> Result<()> {
        self.packet_keyed(p, true, sink)
    }

    /// [`packet`](Self::packet), saying whether a decoder can start at it (as a live source knows).
    pub fn packet_keyed(&mut self, p: &Packet, key: bool, sink: &mut dyn FnMut(Observation) -> Result<()>) -> Result<()> {
        let Some(Some(s)) = self.streams.get_mut(p.stream) else { return Ok(()) };
        if s.wait_key && key {
            s.wait_key = false;
        }
        let (from, to) = (self.from, self.to);
        let tb = s.info.time_base.as_f64();
        let t = p.pts as f64 * tb;
        let inside = t >= from && t <= to;
        if inside {
            for (_, a) in s.packet_analyzers.iter_mut() {
                a.observe(&Input::Packet { packet: p, t }, &mut self.out);
            }
            s.end = s.end.max(t + p.duration as f64 * tb);
        }
        let frames = match s.decoder.as_mut().filter(|_| !s.wait_key).map(|dec| dec.decode(p)) {
            None => Vec::new(),
            Some(Ok(f)) => f,
            Some(Err(e)) => {
                s.decode_errors += 1;
                if s.decode_errors <= 3 {
                    self.out.push(Observation::new("decode_error", "decoder", t, json!({ "message": e.message })));
                }
                Vec::new()
            }
        };
        // Windows close on what the analyzers have been shown: decoded frames, which a decoder
        // can hold back behind its packets, or the packets themselves when nothing is decoded.
        let mut clock = (s.decoder.is_none() && inside).then_some(t);
        for f in frames {
            if let Some(ft) = feed_frame(s, f, from, to, &mut self.out, &mut self.last_picture) {
                clock = Some(clock.map_or(ft, |c: f64| c.max(ft)));
            }
        }
        self.emit(p.stream, sink)?;
        if let Some(c) = clock {
            self.latest = self.latest.max(c);
            self.close_windows(c, sink)?;
        }
        Ok(())
    }

    fn emit(&mut self, stream: usize, sink: &mut dyn FnMut(Observation) -> Result<()>) -> Result<()> {
        for mut o in std::mem::take(&mut self.out) {
            o.stream.get_or_insert(stream);
            // pictures go to the sink whatever the level: asking for thumbnails is asking for them
            if o.level() <= self.level || o.picture.is_some() {
                sink(o)?;
            }
        }
        Ok(())
    }

    fn digest(&mut self, ws: f64, end: f64) -> Observation {
        let mut data = Map::new();
        for s in self.streams.iter_mut().flatten() {
            let part: Map<String, Value> = s.analyzers().filter_map(|(n, a)| a.digest(end - ws).map(|v| ((*n).to_string(), v))).collect();
            data.insert(s.label.clone(), Value::Object(part));
        }
        Observation::new("digest", "digest", ws, Value::Object(data)).until(end)
    }

    /// Closes every window that ends at or before `t`. A long stretch with nothing in it becomes one digest.
    fn close_windows(&mut self, t: f64, sink: &mut dyn FnMut(Observation) -> Result<()>) -> Result<()> {
        let ws = *self.window.get_or_insert(t);
        let ds = self.digest_seconds;
        if t < ws + ds {
            return Ok(());
        }
        let end = ws + ((t - ws) / ds).floor() * ds;
        self.window = Some(end);
        let d = self.digest(ws, end);
        sink(d)
    }

    /// The end of the input: decoders are flushed, the last window closes and a summary comes last.
    /// Returns the end time.
    pub fn finish(&mut self, sink: &mut dyn FnMut(Observation) -> Result<()>) -> Result<f64> {
        let (from, to) = (self.from, self.to);
        for i in 0..self.streams.len() {
            let Some(s) = self.streams[i].as_mut() else { continue };
            if let Some(dec) = s.decoder.as_mut() {
                for f in dec.flush().unwrap_or_default() {
                    feed_frame(s, f, from, to, &mut self.out, &mut self.last_picture);
                }
            }
            self.emit(i, sink)?;
        }
        let end = self.streams.iter().flatten().map(|s| s.end).fold(self.latest, f64::max).min(to);
        if let Some(ws) = self.window {
            if end > ws {
                let d = self.digest(ws, end);
                sink(d)?;
            }
        }
        let mut summary = Map::new();
        for i in 0..self.streams.len() {
            let Some(s) = self.streams[i].as_mut() else { continue };
            let mut part = Map::new();
            for (name, a) in s.analyzers() {
                if let Some(v) = a.finish(end, &mut self.out) {
                    part.insert((*name).into(), v);
                }
            }
            if s.decode_errors > 0 {
                part.insert("decode_errors".into(), json!(s.decode_errors));
            }
            summary.insert(s.label.clone(), Value::Object(part));
            self.emit(i, sink)?;
        }
        sink(Observation::new("summary", "summary", from, Value::Object(summary)).until(end))?;
        Ok(end)
    }
}

/// Shows `f` to the stream's frame analyzers; returns its time when it is inside `from..to`.
/// The newest picture is kept in `last`.
fn feed_frame(s: &mut Stream, f: Frame, from: f64, to: f64, out: &mut Vec<Observation>, last: &mut Option<(f64, VideoFrame)>) -> Option<f64> {
    let tb = s.info.time_base.as_f64();
    match f {
        Frame::Audio(a) => {
            let stamped = a.pts as f64 * tb;
            let t = match s.audio_next {
                Some(next) if (next - stamped).abs() < 0.1 => next,
                _ => stamped,
            };
            let dur = a.frames() as f64 / a.sample_rate.max(1) as f64;
            s.audio_next = Some(t + dur);
            if t + dur <= from || t > to {
                return None;
            }
            s.end = s.end.max(t + dur);
            for (_, an) in s.frame_analyzers.iter_mut() {
                an.observe(&Input::Audio { frame: &a, t }, out);
            }
            Some(t)
        }
        Frame::Video(v) => {
            let t = v.pts as f64 * tb;
            let rate = match s.info.params {
                StreamParams::Video { frame_rate, .. } if frame_rate.num > 0 && frame_rate.den > 0 => frame_rate.as_f64(),
                _ => 0.0,
            };
            let dt = if rate > 0.0 { 1.0 / rate } else { s.last_video.map_or(0.04, |l| (t - l).clamp(0.001, 1.0)) };
            s.last_video = Some(t);
            if t < from || t > to {
                return None;
            }
            s.end = s.end.max(t + dt);
            let low = low_res(&v);
            for (_, an) in s.frame_analyzers.iter_mut() {
                an.observe(&Input::Video { frame: &v, low: &low, t, dt }, out);
            }
            *last = Some((t, v));
            Some(t)
        }
    }
}

/// A PNG of `v`, scaled to fit `max_width` when given.
pub fn png_of(v: &VideoFrame, max_width: Option<u32>) -> Result<(u32, u32, Vec<u8>)> {
    let (w, h, rgb) = ironav_player::rgb_frame(v, max_width, None);
    Ok((w, h, ironav_player::to_png(w, h, &rgb)?))
}

/// Where a file analysis's observations go: the result list (capped), counts, thumbnails and a
/// callback that sees each one as it is made.
struct Log<'a> {
    level: Level,
    seq: u64,
    max: usize,
    list: Vec<Observation>,
    counts: BTreeMap<&'static str, u64>,
    dropped: u64,
    thumb_dir: Option<PathBuf>,
    inline_left: usize,
    images: Vec<Value>,
    on_observation: &'a mut dyn FnMut(&Observation),
}

impl Log<'_> {
    fn push(&mut self, mut o: Observation) -> Result<()> {
        if let Some(pic) = o.picture.take() {
            if self.thumb_dir.is_some() || self.inline_left > 0 {
                let (_, _, png) = png_of(&pic.0, None)?;
                if let Some(dir) = &self.thumb_dir {
                    let path = dir.join(format!("thumb_s{}_{:09.3}.png", o.stream.unwrap_or(0), o.t));
                    std::fs::write(&path, &png)?;
                    o.refs.insert("path".into(), json!(path.to_string_lossy()));
                }
                if self.inline_left > 0 {
                    self.inline_left -= 1;
                    o.refs.insert("image".into(), json!(self.images.len()));
                    self.images.push(json!({ "t": o.t, "png_base64": ironav_player::base64(&png) }));
                }
            }
        }
        if level_of(o.kind) > self.level {
            return Ok(());
        }
        self.seq += 1;
        o.seq = self.seq;
        *self.counts.entry(o.kind).or_default() += 1;
        (self.on_observation)(&o);
        if self.list.len() < self.max || o.level() == Level::Digest {
            self.list.push(o);
        } else {
            self.dropped += 1;
        }
        Ok(())
    }
}

/// Analyze `job.input`. Every observation that passes the level is also handed to
/// `on_observation` as it is made, so a caller can stream them.
pub fn analyze(job: &AnalyzeJob, ctx: &Ctx, on_observation: &mut dyn FnMut(&Observation)) -> Result<Value> {
    let t0 = Instant::now();
    let mut d = open_demuxer(Path::new(&job.input))?;
    let infos = d.streams().to_vec();
    let from = job.from.unwrap_or(0.0).max(0.0);
    let mut a = Analysis::new(&infos, &job.opts, from, job.to.unwrap_or(f64::INFINITY), false)?;
    for i in 0..infos.len() {
        if !a.wants(i) {
            d.discard(i);
        }
    }
    if from > 0.0 {
        if let Err(e) = d.seek(from) {
            a.warnings.push(format!("could not seek, so the file is read from the start: {}", e.message));
        }
    }
    let duration = infos.iter().filter_map(|s| s.duration_secs()).fold(None, |a: Option<f64>, x| Some(a.map_or(x, |a| a.max(x))));
    let thumb_dir = job.thumbnail_dir.as_ref().map(PathBuf::from);
    if let Some(dir) = &thumb_dir {
        std::fs::create_dir_all(dir)?;
    }
    let mut log = Log {
        level: a.level(),
        seq: 0,
        max: job.max_observations,
        list: Vec::new(),
        counts: BTreeMap::new(),
        dropped: 0,
        thumb_dir,
        inline_left: job.inline_thumbnails,
        images: Vec::new(),
        on_observation,
    };
    let mut last_progress = Instant::now();
    let to = job.to.unwrap_or(f64::INFINITY);
    while let Some(p) = d.read_packet()? {
        if ctx.cancelled() {
            return Err(Error::new(ErrorCode::Cancelled, "analysis cancelled"));
        }
        if a.past_end() {
            break;
        }
        a.packet(&p, &mut |o| log.push(o))?;
        if last_progress.elapsed().as_millis() >= 500 {
            last_progress = Instant::now();
            ctx.progress(a.position() - from, duration.map(|d| d.min(to) - from), "analyzing");
        }
    }
    let end = a.finish(&mut |o| log.push(o))?;
    let wall = t0.elapsed().as_secs_f64();
    let mut result = json!({
        "input": job.input,
        "format": d.format_name(),
        "duration_seconds": duration,
        "level": a.level(),
        "streams": a.describe(),
        "observations": log.list,
        "counts": log.counts,
        "dropped": log.dropped,
        "warnings": a.warnings(),
        "media_seconds": round3(end - from),
        "wall_ms": (wall * 1000.0).round(),
        "realtime_factor": if wall > 0.0 { round3((end - from) / wall) } else { 0.0 },
    });
    if !log.images.is_empty() {
        result["images"] = Value::Array(log.images);
    }
    Ok(result)
}
