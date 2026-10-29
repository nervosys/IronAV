//! Running the analyzers over a file: demux, decode what the chosen analyzers need, feed them in
//! presentation order, and log what they observe, with a digest per window and a summary at the end.

use crate::tools::Ctx;
use ironav_analyze::{analyzer, level_of, Analyzer, Consumes, Input, Level, LowRes, Observation, Settings, ANALYZERS};
use ironav_core::{Decoder, Error, ErrorCode, Frame, MediaType, PixelFormat, Result, StreamInfo, StreamParams, VideoFrame};
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
    /// Analyzer names; empty means all of them.
    #[serde(default)]
    pub analyzers: Vec<String>,
    #[serde(default = "default_level")]
    pub level: String,
    #[serde(default = "default_digest")]
    pub digest_seconds: f64,
    #[serde(default)]
    pub from: Option<f64>,
    #[serde(default)]
    pub to: Option<f64>,
    /// Streams to analyze; empty means every audio and video stream.
    #[serde(default)]
    pub streams: Vec<usize>,
    /// Events kept in the result beyond this are counted but dropped (digests are always kept).
    #[serde(default = "default_max")]
    pub max_observations: usize,
    /// Write each thumbnail here as a PNG file.
    #[serde(default)]
    pub thumbnail_dir: Option<String>,
    /// Return the first N thumbnails as images.
    #[serde(default)]
    pub inline_thumbnails: usize,
    #[serde(default)]
    pub thumbnail_seconds: Option<f64>,
    #[serde(default)]
    pub thumbnail_width: Option<u32>,
    #[serde(default = "default_hwaccel")]
    pub hwaccel: String,
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
}

impl Stream {
    fn analyzers(&mut self) -> impl Iterator<Item = &mut (&'static str, Box<dyn Analyzer>)> {
        self.packet_analyzers.iter_mut().chain(self.frame_analyzers.iter_mut())
    }
}

/// Where observations go: the result list (capped), counts, thumbnails and a live callback.
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
        // Pictures are stored even when their observation is below the level: asking for
        // thumbnails is asking for them at any level.
        if let Some(pic) = o.picture.take() {
            if self.thumb_dir.is_some() || self.inline_left > 0 {
                let (w, h, rgb) = ironav_player::rgb_frame(&pic.0, None, None);
                let png = ironav_player::to_png(w, h, &rgb)?;
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

/// Analyze `job.input`. Every observation that passes the level is also handed to
/// `on_observation` as it is made, so a caller can stream them.
pub fn analyze(job: &AnalyzeJob, ctx: &Ctx, on_observation: &mut dyn FnMut(&Observation)) -> Result<Value> {
    let t0 = Instant::now();
    let level = Level::parse(&job.level)
        .ok_or_else(|| Error::invalid_arg(format!("unknown level '{}'", job.level)).alternatives(Level::ALL.iter().map(|l| l.name())))?;
    if !(job.digest_seconds > 0.0) {
        return Err(Error::invalid_arg("digest_seconds must be above 0"));
    }
    let chosen: Vec<&ironav_analyze::AnalyzerInfo> = if job.analyzers.is_empty() {
        ANALYZERS.iter().collect()
    } else {
        job.analyzers
            .iter()
            .map(|n| analyzer(n).ok_or_else(|| Error::invalid_arg(format!("unknown analyzer '{n}'")).alternatives(ANALYZERS.iter().map(|a| a.name))))
            .collect::<Result<_>>()?
    };
    let settings = Settings {
        thumbnail_seconds: job.thumbnail_seconds.unwrap_or(Settings::default().thumbnail_seconds),
        thumbnail_width: job.thumbnail_width.unwrap_or(Settings::default().thumbnail_width),
    };
    let mut d = open_demuxer(Path::new(&job.input))?;
    let infos = d.streams().to_vec();
    let wanted: Vec<usize> = if job.streams.is_empty() {
        infos.iter().filter(|s| matches!(s.params.media_type(), MediaType::Audio | MediaType::Video)).map(|s| s.index).collect()
    } else {
        for &i in &job.streams {
            if i >= infos.len() {
                return Err(Error::invalid_arg(format!("stream {i} does not exist; the file has {}", infos.len())));
            }
        }
        job.streams.clone()
    };
    let mut warnings = Vec::new();
    let mut streams: Vec<Option<Stream>> = Vec::new();
    for info in &infos {
        if !wanted.contains(&info.index) {
            d.discard(info.index);
            streams.push(None);
            continue;
        }
        let media = info.params.media_type();
        let label = format!("{}{}", if media == MediaType::Video { "video" } else { "audio" }, info.index);
        let mine: Vec<&ironav_analyze::AnalyzerInfo> = chosen.iter().copied().filter(|a| a.consumes.applies_to(media)).collect();
        let (pa, fa): (Vec<_>, Vec<_>) = mine.into_iter().partition(|a| a.consumes == Consumes::Packets);
        let (mut decoder, mut decoder_name) = (None, None);
        if !fa.is_empty() {
            let made = if media == MediaType::Video {
                ironav_player::open_video_decoder(info, &job.hwaccel)
            } else {
                ironav_codec::new_decoder_for(info).map(|d| (d, "software"))
            };
            match made {
                Ok((dec, name)) => (decoder, decoder_name) = (Some(dec), Some(name)),
                Err(e) => warnings.push(format!("stream {}: {} not decoded, so only packet analyzers run on it: {}", info.index, info.codec.name(), e.message)),
            }
        }
        let fa = if decoder.is_some() { fa } else { Vec::new() };
        streams.push(Some(Stream {
            info: info.clone(),
            label,
            decoder,
            decoder_name,
            packet_analyzers: pa.iter().map(|a| (a.name, a.make(info, &settings))).collect(),
            frame_analyzers: fa.iter().map(|a| (a.name, a.make(info, &settings))).collect(),
            audio_next: None,
            last_video: None,
            end: 0.0,
            decode_errors: 0,
        }));
    }
    let from = job.from.unwrap_or(0.0).max(0.0);
    if from > 0.0 {
        if let Err(e) = d.seek(from) {
            warnings.push(format!("could not seek, so the file is read from the start: {}", e.message));
        }
    }
    let to = job.to.unwrap_or(f64::INFINITY);
    if to <= from {
        return Err(Error::invalid_arg("'to' must be after 'from'"));
    }
    let duration = infos.iter().filter_map(|s| s.duration_secs()).fold(None, |a: Option<f64>, x| Some(a.map_or(x, |a| a.max(x))));
    let thumb_dir = job.thumbnail_dir.as_ref().map(PathBuf::from);
    if let Some(dir) = &thumb_dir {
        std::fs::create_dir_all(dir)?;
    }
    let mut log = Log {
        level,
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
    let mut window: Option<f64> = None;
    let mut last_progress = Instant::now();
    let mut latest = from;
    let mut out = Vec::new();

    // Closes every window that ends at or before `t`. A long stretch with nothing in it becomes one digest.
    let close_windows = |t: f64, window: &mut Option<f64>, streams: &mut [Option<Stream>], log: &mut Log| -> Result<()> {
        let ws = *window.get_or_insert(t);
        let ds = job.digest_seconds;
        if t < ws + ds {
            return Ok(());
        }
        let end = ws + ((t - ws) / ds).floor() * ds;
        let mut data = Map::new();
        for s in streams.iter_mut().flatten() {
            let mut part = Map::new();
            for (name, a) in s.analyzers() {
                if let Some(v) = a.digest(end - ws) {
                    part.insert((*name).into(), v);
                }
            }
            data.insert(s.label.clone(), Value::Object(part));
        }
        *window = Some(end);
        log.push(Observation::new("digest", "digest", ws, Value::Object(data)).until(end))
    };

    while let Some(p) = d.read_packet()? {
        if ctx.cancelled() {
            return Err(Error::new(ErrorCode::Cancelled, "analysis cancelled"));
        }
        let Some(Some(s)) = streams.get_mut(p.stream) else { continue };
        let tb = s.info.time_base.as_f64();
        let t = p.pts as f64 * tb;
        if t > to + 1.0 && streams.iter().flatten().all(|s| s.end > to) {
            break;
        }
        let s = streams[p.stream].as_mut().unwrap();
        if t >= from && t <= to {
            for (_, a) in s.packet_analyzers.iter_mut() {
                a.observe(&Input::Packet { packet: &p, t }, &mut out);
            }
            s.end = s.end.max(t + p.duration as f64 * tb);
        }
        let frames = match s.decoder.as_mut().map(|dec| dec.decode(&p)) {
            None => Vec::new(),
            Some(Ok(f)) => f,
            Some(Err(e)) => {
                s.decode_errors += 1;
                if s.decode_errors <= 3 {
                    out.push(Observation::new("decode_error", "decoder", t, json!({ "message": e.message })));
                }
                Vec::new()
            }
        };
        // Windows close on what the analyzers have been shown: decoded frames, which a decoder
        // can hold back behind its packets, or the packets themselves when nothing is decoded.
        let mut clock = (s.decoder.is_none() && t >= from && t <= to).then_some(t);
        for f in frames {
            if let Some(ft) = feed_frame(s, &f, from, to, &mut out) {
                clock = Some(clock.map_or(ft, |c: f64| c.max(ft)));
            }
        }
        let idx = p.stream;
        for mut o in out.drain(..) {
            o.stream.get_or_insert(idx);
            log.push(o)?;
        }
        if let Some(c) = clock {
            latest = latest.max(c);
            close_windows(c, &mut window, &mut streams, &mut log)?;
        }
        if last_progress.elapsed().as_millis() >= 500 {
            last_progress = Instant::now();
            ctx.progress(latest - from, duration.map(|d| d.min(to) - from), "analyzing");
        }
    }
    for i in 0..streams.len() {
        let Some(s) = streams[i].as_mut() else { continue };
        if let Some(dec) = s.decoder.as_mut() {
            for f in dec.flush().unwrap_or_default() {
                feed_frame(s, &f, from, to, &mut out);
            }
        }
        for mut o in out.drain(..) {
            o.stream.get_or_insert(i);
            log.push(o)?;
        }
    }
    let end = streams.iter().flatten().map(|s| s.end).fold(latest, f64::max).min(to);
    // the last, partial window
    if let Some(ws) = window {
        if end > ws {
            let mut data = Map::new();
            for s in streams.iter_mut().flatten() {
                let part: Map<String, Value> = s.analyzers().filter_map(|(n, a)| a.digest(end - ws).map(|v| ((*n).to_string(), v))).collect();
                data.insert(s.label.clone(), Value::Object(part));
            }
            log.push(Observation::new("digest", "digest", ws, Value::Object(data)).until(end))?;
        }
    }
    let mut summary = Map::new();
    for i in 0..streams.len() {
        let Some(s) = streams[i].as_mut() else { continue };
        let mut part = Map::new();
        for (name, a) in s.analyzers() {
            if let Some(v) = a.finish(end, &mut out) {
                part.insert((*name).into(), v);
            }
        }
        if s.decode_errors > 0 {
            part.insert("decode_errors".into(), json!(s.decode_errors));
        }
        summary.insert(s.label.clone(), Value::Object(part));
        for mut o in out.drain(..) {
            o.stream.get_or_insert(i);
            log.push(o)?;
        }
    }
    log.push(Observation::new("summary", "summary", from, Value::Object(summary)).until(end))?;
    let wall = t0.elapsed().as_secs_f64();
    let analyzed: Vec<Value> = streams
        .iter()
        .flatten()
        .map(|s| {
            json!({
                "index": s.info.index,
                "label": s.label,
                "codec": s.info.codec.name(),
                "decoder": s.decoder_name,
                "analyzers": s.packet_analyzers.iter().chain(&s.frame_analyzers).map(|(n, _)| *n).collect::<Vec<_>>(),
            })
        })
        .collect();
    let mut result = json!({
        "input": job.input,
        "format": d.format_name(),
        "duration_seconds": duration,
        "level": level,
        "streams": analyzed,
        "observations": log.list,
        "counts": log.counts,
        "dropped": log.dropped,
        "warnings": warnings,
        "media_seconds": crate_round(end - from),
        "wall_ms": (wall * 1000.0).round(),
        "realtime_factor": if wall > 0.0 { crate_round((end - from) / wall) } else { 0.0 },
    });
    if !log.images.is_empty() {
        result["images"] = Value::Array(log.images);
    }
    Ok(result)
}

fn crate_round(x: f64) -> f64 {
    (x * 1000.0).round() / 1000.0
}

/// Shows `f` to the stream's frame analyzers; returns its time when it is inside `from..to`.
fn feed_frame(s: &mut Stream, f: &Frame, from: f64, to: f64, out: &mut Vec<Observation>) -> Option<f64> {
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
                an.observe(&Input::Audio { frame: a, t }, out);
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
            let low = low_res(v);
            for (_, an) in s.frame_analyzers.iter_mut() {
                an.observe(&Input::Video { frame: v, low: &low, t, dt }, out);
            }
            Some(t)
        }
    }
}
