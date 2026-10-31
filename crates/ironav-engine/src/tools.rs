//! The agent tool registry: one definition per capability, rendered as CLI subcommands,
//! MCP tools and ontology `Operation` entities.

use crate::job::Job;
use ironav_codec::{implemented, is_implemented, FILTERS};
use ironav_core::{CodecId, Error, ErrorCode, MediaType, Result};
use ironav_formats::{open_demuxer, FORMATS};
use ironav_hwaccel::{linked_backends, probe_all, provider_codecs, select, Backend, Direction, HwPolicy, Platform, BACKENDS, PROVIDERS};
use ironav_ontology::{entity, Kind, Ontology, Predicate, Status};
use serde_json::{json, Value};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

pub struct Tool {
    pub name: &'static str,
    pub description: &'static str,
    /// The equivalent command line.
    pub cli: &'static str,
    pub input_schema: Value,
    /// Never changes anything on disk.
    pub read_only: bool,
    handler: Handler,
}

enum Handler {
    Plain(fn(&Value) -> Result<Value>),
    /// A long-running tool, which reports progress and can be cancelled.
    WithCtx(fn(&Value, &Ctx) -> Result<Value>),
}

/// What a long-running tool can use while it works: somewhere to report progress, and a flag
/// that says the caller wants it to stop.
#[derive(Default, Clone, Copy)]
pub struct Ctx<'a> {
    pub progress: Option<&'a (dyn Fn(f64, Option<f64>, &str) + Sync)>,
    pub cancel: Option<&'a AtomicBool>,
}

impl Ctx<'_> {
    /// `done` out of `total` (when known), in the tool's own unit, with a short message.
    pub fn progress(&self, done: f64, total: Option<f64>, message: &str) {
        if let Some(p) = self.progress {
            p(done, total, message);
        }
    }
    pub fn cancelled(&self) -> bool {
        self.cancel.is_some_and(|c| c.load(Ordering::Relaxed))
    }
}

impl Tool {
    pub fn call(&self, args: &Value) -> Result<Value> {
        self.call_with(args, &Ctx::default())
    }
    pub fn call_with(&self, args: &Value, ctx: &Ctx) -> Result<Value> {
        match self.handler {
            Handler::Plain(h) => h(args),
            Handler::WithCtx(h) => h(args, ctx),
        }
    }
}

pub fn call_tool(name: &str, args: &Value) -> Result<Value> {
    call_tool_with(name, args, &Ctx::default())
}

pub fn call_tool_with(name: &str, args: &Value, ctx: &Ctx) -> Result<Value> {
    let t = tools().iter().find(|t| t.name == name).ok_or_else(|| {
        Error::new(ErrorCode::NotFound, format!("unknown tool '{name}'")).alternatives(tools().iter().map(|t| t.name))
    })?;
    t.call_with(args, ctx)
}

fn object(props: Value, required: &[&str]) -> Value {
    json!({ "type": "object", "properties": props, "required": required, "additionalProperties": false })
}

/// JSON Schema for [`Job`], with enums filled in from the live registries.
pub fn job_schema() -> Value {
    // Software encoders plus anything a hardware provider can encode.
    let codec_names = |m: MediaType| -> Vec<&'static str> {
        let hw = provider_codecs(Direction::Encode);
        ["copy", "none"]
            .into_iter()
            .chain(implemented().into_iter().filter(|i| i.encode && i.codec.media_type() == m).map(|i| i.codec.name()))
            .chain(hw.into_iter().filter(|c| c.media_type() == m && !is_implemented(*c, true)).map(|c| c.name()))
            .collect()
    };
    let policies: Vec<&str> = ["auto", "off", "require"].into_iter().chain(Backend::ALL.iter().map(|b| b.name())).collect();
    object(
        json!({
            "input": { "type": "string", "description": "Path of the file to read. The format is detected from content, not extension." },
            "extra_inputs": { "type": "array", "items": { "type": "string" },
                        "description": "More files to take streams from, for example a separate audio file for a video. The first stream of each media type wins, so an mp4 can take video from `input` and audio from here." },
            "output": { "type": "string", "description": "Path to write. Written atomically via <output>.partial." },
            "format": { "type": "string", "enum": FORMATS.iter().filter(|f| f.mux).map(|f| f.name).collect::<Vec<_>>(),
                        "description": "Output container. Inferred from the output extension when omitted." },
            "audio": object(json!({ "codec": { "type": "string", "enum": codec_names(MediaType::Audio),
                        "description": "Target audio codec; 'copy' (default) forwards packets untouched; 'none' leaves the audio out." },
                        "bitrate_kbps": { "type": "integer", "minimum": 6, "maximum": 510, "description": "Target bitrate in kbit/s; Opus only (default 64 per channel)." },
                        "opus": object(json!({ "mode": { "type": "string", "enum": ["celt", "silk", "hybrid", "auto"], "description": "Opus coding mode. 'celt' (default) for music at any bit rate; 'silk' for speech at low bit rates, wideband; 'hybrid' for speech with its high frequencies (about 16 to 40 kb/s); 'auto' picks for each packet by the bit rate and the signal." },
                            "signal": { "type": "string", "enum": ["auto", "voice", "music"], "description": "What the input is, for mode 'auto': 'voice' keeps SILK and hybrid up to a higher bit rate, 'music' keeps CELT down to a lower one. Default 'auto' guesses from how the level moves." },
                            "frame_ms": { "type": "number", "enum": [2.5, 5, 10, 20, 40, 60], "description": "Packet length in ms; default 20. Shorter lowers the delay. SILK, hybrid and auto need 10 or more." },
                            "fec_loss_percent": { "type": "integer", "minimum": 0, "maximum": 100, "description": "Expected packet loss in percent. Above 0 turns on in-band forward error correction, which codes each speech frame again, coarser, into the next packet so a lost packet can be rebuilt; it takes bit rate. Default 0 (off)." },
                            "dtx": { "type": "boolean", "description": "Discontinuous transmission: in a pause most frames are not sent, only a packet's TOC byte. Default false." } }), &[]) } ), &[]),
            "video": object(json!({ "codec": { "type": "string", "enum": codec_names(MediaType::Video),
                        "description": "Target video codec; 'copy' (default) forwards packets untouched; 'none' leaves the video out. h264 is encoded on the GPU (NVENC) when one is usable and in software (High profile with CABAC and the 8x8 transform, yuv420p, even dimensions) otherwise; hevc is encoded only on the GPU and needs hwaccel auto or cuda." },
                        "bitrate_kbps": { "type": "integer", "minimum": 1, "description": "Target average bitrate in kbit/s. Mutually exclusive with qp." },
                        "qp": { "type": "integer", "minimum": 0, "maximum": 51, "description": "Constant quantizer: lower is better quality and larger. Default 23 when no bitrate is set. For mjpeg it sets the JPEG quality (100 minus twice qp, so 5 is 90 and 23 is 54); default quality 90." },
                        "latency": { "type": "string", "enum": ["normal", "low"], "description": "'low' encodes for live delivery: no B-frames (each picture is sent as soon as it is coded), a keyframe every second, and a bitrate held to a quarter second of bits (NVENC: low-latency tuning, and with bitrate_kbps CBR with a one-frame buffer). Applies when the video is re-encoded: naming a codec re-encodes even if it matches the source. mjpeg and rawvideo are low latency either way. Default 'normal' favours size." } }), &[]),
            "filters": { "type": "array", "description": "Applied in order to every stream of the matching media type.",
                         "items": object(json!({
                             "name": { "type": "string", "enum": FILTERS.iter().map(|f| f.name).collect::<Vec<_>>() },
                             "args": { "type": "object", "description": "Filter parameters; see each filter's entity in the ontology." }
                         }), &["name"]) },
            "hwaccel": { "type": "string", "enum": policies, "default": "auto",
                         "description": "auto = use hardware if usable else software; off = software only; require = fail unless hardware is usable; or a backend name to require that backend." },
            "overwrite": { "type": "boolean", "default": false, "description": "Allow replacing an existing output file." }
        }),
        &["input", "output"],
    )
}

fn parse_job(args: &Value) -> Result<Job> {
    serde_json::from_value(args.clone()).map_err(|e| {
        Error::invalid_arg(format!("invalid job: {e}")).hint("the job schema is in `ironav tools` (tool 'plan' → input_schema)")
    })
}

fn str_arg<'a>(args: &'a Value, key: &str) -> Result<Option<&'a str>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s)),
        Some(_) => Err(Error::invalid_arg(format!("'{key}' must be a string"))),
    }
}

fn need_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    str_arg(args, key)?.ok_or_else(|| Error::invalid_arg(format!("missing required argument '{key}'")))
}

fn h_devices(_: &Value) -> Result<Value> {
    let list = ironav_live::capture::devices()?;
    Ok(json!({ "devices": list, "platform_supported": cfg!(any(windows, all(target_os = "linux", target_pointer_width = "64"))) }))
}

fn h_onvif_discover(args: &Value) -> Result<Value> {
    let ms = args.get("timeout_ms").and_then(Value::as_u64).unwrap_or(2000).clamp(100, 10_000);
    let found = ironav_live::onvif::discover(ironav_live::onvif::DISCOVERY, std::time::Duration::from_millis(ms))?;
    Ok(json!({ "cameras": found }))
}

fn h_onvif_profiles(args: &Value) -> Result<Value> {
    let allow = ironav_live::Allow::from_env()?;
    let dev = ironav_live::onvif::profiles(need_str(args, "url")?, str_arg(args, "user")?, str_arg(args, "password")?, &allow)?;
    Ok(serde_json::to_value(dev)?)
}

fn h_capabilities(_: &Value) -> Result<Value> {
    let probes = probe_all();
    Ok(json!({
        "name": "IronAV",
        "version": env!("CARGO_PKG_VERSION"),
        "platform": Platform::current().name(),
        "summary": "Audio/video conversion engine designed for AI agents: JSON in, JSON out, typed errors with recovery hints, a queryable ontology, and an MCP server.",
        "how_to_use": [
            "1. Call `ontology_search` / `ontology_describe` / `ontology_explain_conversion` to learn what exists and what is implemented vs planned.",
            "2. Call `probe` on the input to see its streams.",
            "3. Call `plan` with a job; it validates everything and shows exactly what `run` would do, without writing.",
            "4. Call `run` with the same job. Output is atomic; existing files are never replaced unless overwrite is true.",
            "On error, read error.code, error.hint and error.alternatives; they name valid values to retry with."
        ],
        "formats": FORMATS,
        "codecs": implemented(),
        "filters": FILTERS,
        "hardware": {
            "probes": probes,
            "linked_backends": linked_backends().iter().map(|b| b.name()).collect::<Vec<_>>(),
            "providers": PROVIDERS.iter().map(|p| json!({ "backend": p.backend.name(), "direction": p.direction, "codecs": p.codecs.iter().map(|c| c.name()).collect::<Vec<_>>() })).collect::<Vec<_>>(),
            "note": "A backend is usable only if its driver is detected AND a native provider is linked into this build."
        },
        "tools": tools().iter().map(|t| json!({ "name": t.name, "description": t.description, "cli": t.cli, "read_only": t.read_only })).collect::<Vec<_>>(),
        "analyzers": ironav_analyze::ANALYZERS.iter().map(|a| json!({ "name": a.name, "consumes": a.consumes, "description": a.description, "emits": a.emits })).collect::<Vec<_>>(),
        "sidecars": match crate::sidecar::configured() {
            Ok(list) => json!(list.keys().collect::<Vec<_>>()),
            Err(e) => json!({ "error": e.message }),
        },
        "observation_kinds": ironav_analyze::KINDS,
        "error_codes": ErrorCode::ALL,
        "ontology": ontology().summary(),
        "not_implemented": "Implemented: software decoders for H.264, HEVC (Main, Main 10), VP8, VP9 (profiles 0-3), JPEG, ProRes, AAC-LC (plus HE-AAC v1), MP3, ALAC, Vorbis, Opus, FLAC and PCM; software encoders for H.264 (High profile: CABAC, the 8x8 transform, I, P and B frames, 8-bit yuv420p with even dimensions), JPEG, ProRes (4:2:2 at 10 bits, HQ profile), AAC-LC, MP3, ALAC, Vorbis, Opus, FLAC and PCM. NVIDIA GPUs encode H.264 (preferred when usable) and HEVC (NVENC; HEVC has no software encoder); GPU decoding (NVDEC) is also available for them. AV1 is decoded only on NVIDIA GPUs (NVDEC, no software decoder) and cannot be encoded. Not implemented (planned in the ontology): MPEG-4 Part 2. VP9 and AV1 cannot be written (copied into a file). A video file and an audio file are combined into one output with extra_inputs."
    }))
}

fn h_probe(args: &Value) -> Result<Value> {
    let path = need_str(args, "path")?;
    let d = open_demuxer(Path::new(path))?;
    let size = std::fs::metadata(path).map(|m| m.len()).ok();
    let duration = d.streams().iter().filter_map(|s| s.duration_secs()).fold(None, |a: Option<f64>, x| Some(a.map_or(x, |a| a.max(x))));
    Ok(json!({ "path": path, "format": d.format_name(), "size_bytes": size, "duration_seconds": duration, "streams": d.streams() }))
}

fn h_plan(args: &Value) -> Result<Value> {
    Ok(serde_json::to_value(crate::plan(&parse_job(args)?)?)?)
}

fn h_run(args: &Value, ctx: &Ctx) -> Result<Value> {
    let mut report = |p: &crate::Progress| ctx.progress(p.bytes_in as f64, None, &format!("{} packets read, {} bytes written", p.packets, p.bytes_out));
    Ok(serde_json::to_value(crate::run::run_with(&parse_job(args)?, &mut report, ctx.cancel)?)?)
}

fn h_analyze(args: &Value, ctx: &Ctx) -> Result<Value> {
    let job: crate::analyze::AnalyzeJob = serde_json::from_value(args.clone())
        .map_err(|e| Error::invalid_arg(format!("invalid analyze arguments: {e}")).hint("the schema is in `ironav tools` (tool 'analyze')"))?;
    crate::analyze::analyze(&job, ctx, &mut |_| {})
}

fn session_arg(args: &Value) -> Result<std::sync::Arc<crate::live::Session>> {
    crate::live::get(need_str(args, "session")?)
}

fn f64_arg(args: &Value, key: &str) -> Result<Option<f64>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_f64().map(Some).ok_or_else(|| Error::invalid_arg(format!("'{key}' must be a number"))),
    }
}

fn h_stream_open(args: &Value) -> Result<Value> {
    let a: crate::live::OpenArgs = serde_json::from_value(args.clone())
        .map_err(|e| Error::invalid_arg(format!("invalid stream_open arguments: {e}")).hint("the schema is in `ironav tools` (tool 'stream_open')"))?;
    let s = crate::live::open(&a)?;
    // give a quick source a moment to show its streams
    let _ = s.events(u64::MAX, &[], ironav_analyze::Level::Digest, 1, std::time::Duration::from_millis(0));
    let t0 = std::time::Instant::now();
    while t0.elapsed() < std::time::Duration::from_millis(1000) && s.status()["state"] == "waiting" {
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    Ok(s.status())
}

fn h_stream_events(args: &Value) -> Result<Value> {
    let s = session_arg(args)?;
    let after = args.get("after").and_then(Value::as_u64).unwrap_or(0);
    let kinds: Vec<String> = match args.get("kinds") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(a)) => a.iter().map(|k| k.as_str().map(str::to_string).ok_or_else(|| Error::invalid_arg("'kinds' must be strings"))).collect::<Result<_>>()?,
        Some(_) => return Err(Error::invalid_arg("'kinds' must be an array of observation kinds")),
    };
    for k in &kinds {
        // a sidecar's kinds are not listed, but have the same form
        if ironav_analyze::kind_info(k).is_none() && !crate::sidecar::is_kind_name(k) {
            return Err(Error::invalid_arg(format!("unknown observation kind '{k}'")).alternatives(ironav_analyze::KINDS.iter().map(|k| k.name)));
        }
    }
    let level = match str_arg(args, "level")? {
        None => ironav_analyze::Level::Events,
        Some(l) => ironav_analyze::Level::parse(l).ok_or_else(|| Error::invalid_arg(format!("unknown level '{l}'")).alternatives(ironav_analyze::Level::ALL.iter().map(|l| l.name())))?,
    };
    let max = args.get("max").and_then(Value::as_u64).unwrap_or(100).clamp(1, 1000) as usize;
    let wait = args.get("wait_ms").and_then(Value::as_u64).unwrap_or(0).min(crate::live::MAX_WAIT_MS);
    Ok(s.events(after, &kinds, level, max, std::time::Duration::from_millis(wait)))
}

fn h_stream_status(args: &Value) -> Result<Value> {
    match str_arg(args, "session")? {
        Some(id) => Ok(crate::live::get(id)?.status()),
        None => Ok(json!({ "sessions": crate::live::list() })),
    }
}

fn h_stream_snapshot(args: &Value) -> Result<Value> {
    let w = args.get("max_width").and_then(Value::as_u64).map(|w| w.clamp(16, 7680) as u32).or(Some(640));
    session_arg(args)?.snapshot(f64_arg(args, "at")?, w)
}

fn h_stream_keyframes(args: &Value) -> Result<Value> {
    let max = args.get("max").and_then(Value::as_u64).unwrap_or(1).clamp(1, 64) as usize;
    let s = session_arg(args)?;
    let found: Vec<Value> = s
        .keyframes(f64_arg(args, "after")?, max)
        .iter()
        .map(|k| {
            let wall = k.decoded_at.duration_since(std::time::UNIX_EPOCH).map_or(0.0, |d| d.as_secs_f64());
            let captured = k.captured_at.and_then(|c| c.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs_f64());
            json!({ "t": (k.t * 1000.0).round() / 1000.0, "decoded_unix": wall, "captured_unix": captured, "width": k.width, "height": k.height, "jpeg_base64": ironav_player::base64(&k.jpeg) })
        })
        .collect();
    Ok(json!({ "session": s.id, "keyframes": found }))
}

fn h_stream_clip(args: &Value) -> Result<Value> {
    let from = f64_arg(args, "from")?.ok_or_else(|| Error::invalid_arg("missing required argument 'from'"))?;
    let to = f64_arg(args, "to")?.ok_or_else(|| Error::invalid_arg("missing required argument 'to'"))?;
    let overwrite = args.get("overwrite").and_then(Value::as_bool).unwrap_or(false);
    session_arg(args)?.clip(from, to, need_str(args, "output")?, str_arg(args, "format")?, overwrite)
}

fn h_stream_query(args: &Value) -> Result<Value> {
    let q: crate::live::Query = serde_json::from_value(args.clone())
        .map_err(|e| Error::invalid_arg(format!("invalid stream_query arguments: {e}")).hint("the schema is in `ironav tools` (tool 'stream_query')"))?;
    Ok(crate::live::get(&q.session)?.query(&q))
}

fn h_stream_configure(args: &Value) -> Result<Value> {
    let s = session_arg(args)?;
    let list = |key: &str| -> Result<Vec<String>> {
        match args.get(key) {
            None | Some(Value::Null) => Ok(Vec::new()),
            Some(Value::Array(a)) => a.iter().map(|k| k.as_str().map(str::to_string).ok_or_else(|| Error::invalid_arg(format!("'{key}' must be analyzer names")))).collect(),
            Some(_) => Err(Error::invalid_arg(format!("'{key}' must be an array of analyzer names"))),
        }
    };
    let c = crate::analyze::Reconfigure { level: str_arg(args, "level")?.map(str::to_string), digest_seconds: f64_arg(args, "digest_seconds")?, add: list("add")?, remove: list("remove")? };
    let flag = |key: &str| -> Result<bool> {
        match args.get(key) {
            None | Some(Value::Null) => Ok(false),
            Some(Value::Bool(b)) => Ok(*b),
            Some(_) => Err(Error::invalid_arg(format!("'{key}' must be true or false"))),
        }
    };
    let keep = match args.get("keep_segments") {
        None | Some(Value::Null) => None,
        Some(v) => Some(v.as_u64().ok_or_else(|| Error::invalid_arg("'keep_segments' must be a whole number"))? as usize),
    };
    let start = match str_arg(args, "record")? {
        Some(path) => Some(crate::record::RecordOptions { path: path.to_string(), segment_seconds: f64_arg(args, "segment_seconds")?, keep_segments: keep, overwrite: flag("overwrite")? }),
        None => None,
    };
    let given = |key: &str| args.get(key).is_some_and(|v| !v.is_null());
    if start.is_none() && (given("segment_seconds") || given("keep_segments") || given("overwrite")) {
        return Err(Error::invalid_arg("segment_seconds, keep_segments and overwrite go with record"));
    }
    let transcode = match args.get("transcode") {
        None | Some(Value::Null) => None,
        Some(v) => Some(serde_json::from_value::<crate::transcode::TranscodeOptions>(v.clone()).map_err(|e| Error::invalid_arg(format!("transcode: {e}")))?),
    };
    s.configure(c, crate::live::RecordChange { stop: flag("stop_recording")?, start, stop_transcode: flag("stop_transcode")?, transcode })
}

fn h_stream_close(args: &Value) -> Result<Value> {
    crate::live::close(need_str(args, "session")?)
}

fn stream_open_schema() -> Value {
    object(
        json!({
            "source": { "type": "string", "description": "What to read: a path (add ?pace=realtime to play a file as if live, ?follow to read it as it grows), '-' or pipe:NAME, tcp://host:port (connect), tcp://:port (listen on loopback), udp://:port or udp://group:port (multicast), or rtsp://[user:password@]host[:port]/path (a camera or server; credentials are never echoed; add ?transport=udp for RTP over UDP), or http://host/path (an HLS playlist, or any other stream downloaded as it arrives; ?height=N caps the variant; its subtitles come as 'caption' events, ?captions=LANG picks the track and ?captions=off takes none), or a YouTube video or live stream by its URL (youtube.com/watch?v=ID, youtu.be/ID, /shorts/, /live/; resolved by yt-dlp, which must be installed; ?cookies=BROWSER uses that browser's signed-in session when YouTube asks for one), or test://video / test://audio (made up for testing, marked synthetic in the status; size, fps, rate, duration); https:// and rtsps:// are the same over TLS 1.3, with the server's certificate checked against the platform's roots and IRONAV_CA_FILE. Network addresses must be loopback, private or link-local unless the operator set IRONAV_NET_ALLOW. Options can follow '?': format, fps, pace, follow, idle, listen, reconnect (RTSP is reconnected after a drop or a stall by default; reconnect=0 turns it off, tcp://host:port?reconnect turns it on), stall (seconds without data counted as a stall, twice that as a loss; 5), retries (attempts before giving up; unlimited)." },
            "analyzers": { "type": "array", "items": { "type": "string", "enum": ironav_analyze::ANALYZERS.iter().map(|a| a.name).collect::<Vec<_>>() }, "description": "All of them when absent." },
            "level": { "type": "string", "enum": ironav_analyze::Level::ALL.iter().map(|l| l.name()).collect::<Vec<_>>(), "default": "events", "description": "The most detail the session keeps; stream_events can ask for less." },
            "digest_seconds": { "type": "number", "exclusiveMinimum": 0, "default": 10 },
            "retain_seconds": { "type": "number", "minimum": 0, "default": 60, "description": "Media kept for stream_snapshot and stream_clip (at most 256 MiB of it)." },
            "format": { "type": "string", "enum": ironav_formats::stream::STREAM_FORMATS, "description": "The stream's format, when its first bytes do not say (raw H.264 and HEVC usually do)." },
            "fps": { "type": "number", "exclusiveMinimum": 0, "description": "Frame rate of a raw H.264/HEVC stream, which carries none (25 when absent)." },
            "pace": { "type": "string", "enum": ["realtime", "none"], "description": "realtime: read no faster than the media plays." },
            "thumbnail_seconds": { "type": "number", "minimum": 2, "default": 10 },
            "hwaccel": { "type": "string", "default": "auto" },
            "keyframes_only": { "type": "boolean", "default": false, "description": "Decode only keyframes (IDR, I and key frames): a fraction of the work, for thumbnails, cuts and black spans at keyframe resolution (a cut is reported at the first keyframe after it). The freeze analyzer does not run." },
            "sidecars": { "type": "array", "items": { "type": "string" }, "description": "Model sidecars to run as well (speech-to-text, detection, ...), by the names the operator configured in IRONAV_SIDECARS; `capabilities` lists them. Each takes the first analyzed stream of the kind it wants." },
            "keyframe_jpegs": { "type": "integer", "minimum": 0, "maximum": 256, "default": 0, "description": "Keep the pictures of this many of the newest keyframes of the first video stream as JPEG, for stream_keyframes." },
            "keyframe_width": { "type": "integer", "minimum": 16, "maximum": 8192, "description": "Scale the kept keyframe pictures to this width." },
            "record": { "type": "string", "description": "Also write the media as it arrives, without re-encoding: a file (.ts stays playable even if cut off; .mp4 is complete when the session closes), or an .m3u8 path for HLS: MPEG-TS segments cut at keyframes next to it, and the playlist rewritten after each one." },
            "segment_seconds": { "type": "number", "minimum": 1, "maximum": 600, "default": 6, "description": "HLS segment length (cut at the first keyframe after it)." },
            "transcode": { "type": "object", "required": ["output"], "additionalProperties": false,
                           "description": "Also encode the first video stream again as low-latency H.264 (software: no B pictures, a keyframe a second), to a file or an .m3u8 path for HLS, with the audio copied when the output holds it. Runs on its own thread; when it falls behind, video is dropped up to the next keyframe (status: transcode.dropped_packets). The software encoder suits reduced sizes: give width.",
                           "properties": {
                               "output": { "type": "string" },
                               "width": { "type": "integer", "minimum": 16, "maximum": 16384, "description": "Even; the height keeps the aspect ratio. The source's size when absent." },
                               "bitrate_kbps": { "type": "integer", "minimum": 1, "maximum": 1000000 },
                               "qp": { "type": "integer", "minimum": 0, "maximum": 51, "description": "Constant quantiser when no bitrate is given (23 by default)." },
                               "audio": { "type": "string", "enum": ["copy", "none"], "default": "copy" },
                               "segment_seconds": { "type": "number", "minimum": 1, "maximum": 600 },
                               "keep_segments": { "type": "integer", "minimum": 1 },
                               "overwrite": { "type": "boolean" } } },
            "keep_segments": { "type": "integer", "minimum": 1, "description": "HLS: keep only the newest N segments (a sliding live playlist); all of them when absent." },
            "overwrite": { "type": "boolean", "default": false },
            "idle_timeout_seconds": { "type": "number", "minimum": 0, "default": 300, "description": "Close the session after this long without a call about it (0: never), so a forgotten one does not hold one of the 8 places." }
        }),
        &["source"],
    )
}

/// JSON Schema for the `analyze` tool.
pub fn analyze_schema() -> Value {
    object(
        json!({
            "input": { "type": "string", "description": "Path of the media file to analyze." },
            "analyzers": { "type": "array", "items": { "type": "string", "enum": ironav_analyze::ANALYZERS.iter().map(|a| a.name).collect::<Vec<_>>() },
                           "description": "Which analyzers to run; all of them when empty or absent." },
            "level": { "type": "string", "enum": ironav_analyze::Level::ALL.iter().map(|l| l.name()).collect::<Vec<_>>(), "default": "events",
                       "description": "digest: rolling digests and a summary only (fewest tokens); events: plus cuts, silences, black and frozen spans, thumbnails, gaps; raw: plus per-frame and per-second measurements." },
            "digest_seconds": { "type": "number", "exclusiveMinimum": 0, "default": 10, "description": "Length of each digest window, in seconds of media." },
            "from": { "type": "number", "minimum": 0, "description": "Start of the analyzed range, in seconds." },
            "to": { "type": "number", "description": "End of the analyzed range, in seconds." },
            "streams": { "type": "array", "items": { "type": "integer", "minimum": 0 }, "description": "Stream indices (from `probe`); every audio and video stream when absent." },
            "max_observations": { "type": "integer", "minimum": 0, "default": 500, "description": "Events beyond this many are counted in `counts` and `dropped` but not returned; digests always are." },
            "thumbnail_dir": { "type": "string", "description": "Write each thumbnail there as PNG; observations then carry refs.path." },
            "inline_thumbnails": { "type": "integer", "minimum": 0, "default": 0, "description": "Return the first N thumbnails as images." },
            "thumbnail_seconds": { "type": "number", "minimum": 2, "default": 10, "description": "Longest time between thumbnails; new content brings one sooner (at most every 2 s)." },
            "thumbnail_width": { "type": "integer", "minimum": 16, "default": 320 },
            "hwaccel": { "type": "string", "default": "auto", "description": "Video decoding: auto, off, require or a backend name, as for jobs." },
            "keyframes_only": { "type": "boolean", "default": false, "description": "Decode only keyframes (IDR, I and key frames): a fraction of the work, for thumbnails, cuts and black spans at keyframe resolution (a cut is reported at the first keyframe after it). The freeze analyzer does not run." },
            "sidecars": { "type": "array", "items": { "type": "string" }, "description": "Model sidecars to run as well (speech-to-text, detection, ...), by the names the operator configured in IRONAV_SIDECARS; `capabilities` lists them. Each takes the first analyzed stream of the kind it wants." }
        }),
        &["input"],
    )
}

fn h_search(args: &Value) -> Result<Value> {
    let query = str_arg(args, "query")?.unwrap_or("");
    let kind = match str_arg(args, "kind")? {
        None => None,
        Some(k) => Some(Kind::parse(k).ok_or_else(|| {
            Error::invalid_arg(format!("unknown kind '{k}'")).alternatives(Kind::ALL.iter().map(|k| k.prefix()))
        })?),
    };
    let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;
    let hits = ontology().search(query, kind, limit);
    Ok(json!({
        "count": hits.len(),
        "results": hits.iter().map(|e| json!({ "id": e.id, "kind": e.kind, "label": e.label, "status": e.status, "description": e.description })).collect::<Vec<_>>()
    }))
}

fn h_describe(args: &Value) -> Result<Value> {
    ontology().describe(need_str(args, "id")?)
}

fn h_explain(args: &Value) -> Result<Value> {
    ontology().explain_conversion(need_str(args, "from")?, need_str(args, "to")?)
}

fn h_hw_probe(_: &Value) -> Result<Value> {
    Ok(json!({
        "platform": Platform::current().name(),
        "probes": probe_all(),
        "linked_backends": linked_backends().iter().map(|b| b.name()).collect::<Vec<_>>(),
        "matrix": BACKENDS,
    }))
}

fn h_hw_select(args: &Value) -> Result<Value> {
    let codec = CodecId::parse(need_str(args, "codec")?)?;
    let dir = match need_str(args, "direction")? {
        "decode" => Direction::Decode,
        "encode" => Direction::Encode,
        other => return Err(Error::invalid_arg(format!("direction must be decode or encode, got '{other}'"))),
    };
    let policy = HwPolicy::parse(str_arg(args, "policy")?.unwrap_or("auto"))?;
    Ok(serde_json::to_value(select(codec, dir, policy, &probe_all())?)?)
}

pub fn tools() -> &'static [Tool] {
    static TOOLS: OnceLock<Vec<Tool>> = OnceLock::new();
    TOOLS.get_or_init(|| {
        vec![
            Tool {
                name: "capabilities",
                description: "Start here. Returns what this IronAV build can do: formats, codecs, filters, hardware, tools, error codes and usage workflow.",
                cli: "ironav capabilities",
                input_schema: object(json!({}), &[]),
                read_only: true,
                handler: Handler::Plain(h_capabilities),
            },
            Tool {
                name: "devices",
                description: "The cameras and microphones this machine has, each with the index and name a live source names it by: stream_open with device://video/INDEX or device://audio/NAME (a part of the name, any case; without one, the first device). Listing does not open any device. Capture is supported on Windows (Media Foundation) and 64-bit Linux (Video4Linux2, ALSA).",
                cli: "ironav devices",
                input_schema: object(json!({}), &[]),
                read_only: true,
                handler: Handler::Plain(h_devices),
            },
            Tool {
                name: "onvif_discover",
                description: "Find ONVIF cameras on the local network (a WS-Discovery probe for video transmitters on the machine's default interface), each with its endpoint, device service URLs (xaddrs, for onvif_profiles) and scopes (name, hardware, location). Waits timeout_ms for answers.",
                cli: "ironav onvif [--timeout-ms N]",
                input_schema: object(json!({ "timeout_ms": { "type": "integer", "minimum": 100, "maximum": 10000, "default": 2000 } }), &[]),
                read_only: true,
                handler: Handler::Plain(h_onvif_discover),
            },
            Tool {
                name: "onvif_profiles",
                description: "Ask an ONVIF camera (its device service URL, from onvif_discover, or http://HOST) what it is and what it streams: manufacturer, model, firmware, and each media profile's encoding, size, frame rate and bitrate, with its RTSP stream URI and JPEG snapshot URI. Open a stream with stream_open on the stream URI, adding the credentials (rtsp://USER:PASSWORD@...). Credentials go as a WS-Security password digest; connections are limited like any source's (IRONAV_NET_ALLOW).",
                cli: "ironav onvif URL [--user U] [--password P]",
                input_schema: object(json!({ "url": { "type": "string" }, "user": { "type": "string" }, "password": { "type": "string" } }), &["url"]),
                read_only: true,
                handler: Handler::Plain(h_onvif_profiles),
            },
            Tool {
                name: "probe",
                description: "Inspect a media file: container format, duration and per-stream codec/parameters.",
                cli: "ironav probe <path>",
                input_schema: object(json!({ "path": { "type": "string" } }), &["path"]),
                read_only: true,
                handler: Handler::Plain(h_probe),
            },
            Tool {
                name: "plan",
                description: "Validate a conversion job and return the exact steps (copy vs transcode, filters, hardware decisions, warnings) without writing anything. Always safe to call.",
                cli: "ironav plan <job.json|->  or  ironav convert --dry-run ...",
                input_schema: job_schema(),
                read_only: true,
                handler: Handler::Plain(h_plan),
            },
            Tool {
                name: "run",
                description: "Execute a conversion job. Writes atomically; refuses to replace an existing output unless overwrite is true. Returns timing and throughput.",
                cli: "ironav run <job.json|->  or  ironav convert ...",
                input_schema: job_schema(),
                read_only: false,
                handler: Handler::WithCtx(h_run),
            },
            Tool {
                name: "analyze",
                description: "Understand a media file without looking at it: runs analyzers (stream structure, audio levels, EBU R128 loudness, silence, speech, scene cuts, black and frozen pictures, thumbnails, and any model sidecars the operator configured) and returns typed observations with a digest per window and a summary. Use level 'digest' for the fewest tokens. Writes files only when thumbnail_dir is given.",
                cli: "ironav analyze <file> [--analyzers a,b] [--sidecars a,b] [--level digest|events|raw] [--digest-seconds N] [--from S] [--to S] [--thumbnails DIR] [--keyframes-only] [--jsonl]",
                input_schema: analyze_schema(),
                read_only: false,
                handler: Handler::WithCtx(h_analyze),
            },
            Tool {
                name: "stream_open",
                description: "Start watching a live source (a socket, a pipe, a growing file, or a file played as if live). It is read, demuxed and analyzed on its own thread as it arrives; follow it with stream_events. Readable as it arrives: MPEG-TS, Matroska/WebM (as MediaRecorder or OBS send it), fragmented MP4 (CMAF), Y4M, WAV, ADTS AAC, MP3, Ogg (Opus, Vorbis, FLAC; internet radio) and raw H.264/HEVC.",
                cli: "ironav watch <source> [--analyzers a,b] [--sidecars a,b] [--level L] [--format F] [--fps N] [--pace realtime]",
                input_schema: stream_open_schema(),
                read_only: true,
                handler: Handler::Plain(h_stream_open),
            },
            Tool {
                name: "stream_events",
                description: "What a live session has observed since `after` (a seq from the last call's next_after; 0 for everything kept). With wait_ms it waits up to that long (30 s at most) for something new, so polling costs nothing while the stream is quiet. `missed` counts events dropped from the log before they were read.",
                cli: "ironav watch <source>  (prints every event)",
                input_schema: object(
                    json!({
                        "session": { "type": "string" },
                        "after": { "type": "integer", "minimum": 0, "default": 0 },
                        "kinds": { "type": "array", "items": { "type": "string" }, "description": "Only these kinds (all when absent): the ones `capabilities` lists, or a sidecar's." },
                        "level": { "type": "string", "enum": ironav_analyze::Level::ALL.iter().map(|l| l.name()).collect::<Vec<_>>(), "default": "events" },
                        "max": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 100 },
                        "wait_ms": { "type": "integer", "minimum": 0, "maximum": 30000, "default": 0 }
                    }),
                    &["session"],
                ),
                read_only: true,
                handler: Handler::Plain(h_stream_events),
            },
            Tool {
                name: "stream_status",
                description: "A live session's state (waiting, running, ended, failed, closed), streams (with the decoder each uses: software or GPU), bytes and packets in, media time, latency percentiles, per-stage times (stage_ms: demux, decode, analyze), source health, log and retained range. Without a session, every open session.",
                cli: "",
                input_schema: object(json!({ "session": { "type": "string" } }), &[]),
                read_only: true,
                handler: Handler::Plain(h_stream_status),
            },
            Tool {
                name: "stream_snapshot",
                description: "A picture from a live session as an image: the newest one, or the one shown at `at` seconds (decoded from the retained media; a thumbnail's `t` works).",
                cli: "",
                input_schema: object(json!({ "session": { "type": "string" }, "at": { "type": "number", "minimum": 0 }, "max_width": { "type": "integer", "minimum": 16, "default": 640 } }), &["session"]),
                read_only: true,
                handler: Handler::Plain(h_stream_snapshot),
            },
            Tool {
                name: "stream_keyframes",
                description: "The pictures of the newest keyframes of a live session opened with keyframe_jpegs, as JPEG: the newest `max` (1 by default), or those after media time `after`, oldest first, each with its media time and when it was decoded. Cheap evidence for an alert: the session encodes each keyframe as it arrives.",
                cli: "",
                input_schema: object(json!({ "session": { "type": "string" }, "after": { "type": "number" }, "max": { "type": "integer", "minimum": 1, "maximum": 64, "default": 1 } }), &["session"]),
                read_only: true,
                handler: Handler::Plain(h_stream_keyframes),
            },
            Tool {
                name: "stream_clip",
                description: "Write the retained media from `from` to `to` seconds (from the keyframe at or before `from`) to a file, without re-encoding. Written atomically; refuses to replace a file unless overwrite is true. The container must hold the stream's codec as received (raw H.264 to .h264 or .mp4, Y4M to .y4m, WAV to .wav, AAC to .aac).",
                cli: "",
                input_schema: object(
                    json!({
                        "session": { "type": "string" }, "from": { "type": "number" }, "to": { "type": "number" },
                        "output": { "type": "string" }, "format": { "type": "string" }, "overwrite": { "type": "boolean", "default": false }
                    }),
                    &["session", "from", "to", "output"],
                ),
                read_only: false,
                handler: Handler::Plain(h_stream_clip),
            },
            Tool {
                name: "stream_query",
                description: "Search a live session's log (the newest 20000 observations): by kind, analyzer, stream, media time overlapping from..to, and text in the data (a transcript's words, say). newest_first answers 'when did X last happen'. Unlike stream_events it does not wait, and it sees every level.",
                cli: "",
                input_schema: object(
                    json!({
                        "session": { "type": "string" },
                        "kinds": { "type": "array", "items": { "type": "string" } },
                        "analyzer": { "type": "string" },
                        "stream": { "type": "integer", "minimum": 0 },
                        "from": { "type": "number", "description": "Media seconds." },
                        "to": { "type": "number", "description": "Media seconds." },
                        "text": { "type": "string", "description": "Case-insensitive, anywhere in the data." },
                        "max": { "type": "integer", "minimum": 1, "maximum": 1000, "default": 100 },
                        "newest_first": { "type": "boolean", "default": false }
                    }),
                    &["session"],
                ),
                read_only: true,
                handler: Handler::Plain(h_stream_query),
            },
            Tool {
                name: "stream_configure",
                description: "Change a running session: the most detail it keeps (level), the digest interval (from the next window), and which analyzers and sidecars run (add, remove: built-in analyzer names, or the operator's sidecar names from `capabilities`). An added video analyzer starts at the next keyframe. An added sidecar starts in the background and joins the first stream of its kind once ready (its name then shows in the status; a failure is an analyzer_failed observation). A removed one finishes in the background, and its closing observations follow. It can also start a recording (record), finish the running one (stop_recording), or both to go on in a new file; the same for a transcode (transcode, stop_transcode). Returns the status with `applied` once the session has taken the change (within a second).",
                cli: "",
                input_schema: object(
                    json!({
                        "session": { "type": "string" },
                        "level": { "type": "string", "enum": ironav_analyze::Level::ALL.iter().map(|l| l.name()).collect::<Vec<_>>() },
                        "digest_seconds": { "type": "number", "exclusiveMinimum": 0 },
                        "add": { "type": "array", "items": { "type": "string" }, "description": format!("Analyzers ({}) or configured sidecars.", ironav_analyze::ANALYZERS.iter().map(|a| a.name).collect::<Vec<_>>().join(", ")) },
                        "remove": { "type": "array", "items": { "type": "string" } },
                        "stop_recording": { "type": "boolean", "description": "Finish the recording running now (its file or playlist is then complete)." },
                        "record": { "type": "string", "description": "Start recording, as stream_open's record: a file begins at the next video keyframe with its timestamps from zero; an .m3u8 path writes HLS segments. With stop_recording, the old recording is finished first, so this goes on in a new file." },
                        "segment_seconds": { "type": "number", "minimum": 1, "maximum": 600 },
                        "keep_segments": { "type": "integer", "minimum": 1 },
                        "overwrite": { "type": "boolean", "description": "Replace an existing recording at that path." },
                        "stop_transcode": { "type": "boolean", "description": "Finish the transcode running now." },
                        "transcode": { "type": "object", "description": "Start a transcode, as stream_open's: it begins at the next video keyframe with its timestamps from zero. With stop_transcode, the old one is finished first." }
                    }),
                    &["session"],
                ),
                read_only: false,
                handler: Handler::Plain(h_stream_configure),
            },
            Tool {
                name: "stream_close",
                description: "Stop a live session and return its final status and summary.",
                cli: "",
                input_schema: object(json!({ "session": { "type": "string" } }), &["session"]),
                read_only: false,
                handler: Handler::Plain(h_stream_close),
            },
            Tool {
                name: "ontology_search",
                description: "Search the ontology of codecs, containers, filters, pixel/sample formats, hardware backends and platforms. Results include status: implemented or planned.",
                cli: "ironav ontology search [text] [--kind K]",
                input_schema: object(
                    json!({
                        "query": { "type": "string", "description": "Space-separated words; empty lists everything of the given kind." },
                        "kind": { "type": "string", "enum": Kind::ALL.iter().map(|k| k.prefix()).collect::<Vec<_>>() },
                        "limit": { "type": "integer", "default": 20 }
                    }),
                    &[],
                ),
                read_only: true,
                handler: Handler::Plain(h_search),
            },
            Tool {
                name: "ontology_describe",
                description: "Full description of one ontology entity (id like 'codec/h264', or a bare name) with all incoming and outgoing relations.",
                cli: "ironav ontology describe <id>",
                input_schema: object(json!({ "id": { "type": "string" } }), &["id"]),
                read_only: true,
                handler: Handler::Plain(h_describe),
            },
            Tool {
                name: "ontology_explain_conversion",
                description: "Can this build convert codec A to codec B? Returns blockers, hardware candidates on this platform, and containers that can hold the result.",
                cli: "ironav ontology convert <from> <to>",
                input_schema: object(json!({ "from": { "type": "string" }, "to": { "type": "string" } }), &["from", "to"]),
                read_only: true,
                handler: Handler::Plain(h_explain),
            },
            Tool {
                name: "hardware_probe",
                description: "Detect hardware acceleration backends on this machine and show the full backend × codec capability matrix.",
                cli: "ironav hw probe",
                input_schema: object(json!({}), &[]),
                read_only: true,
                handler: Handler::Plain(h_hw_probe),
            },
            Tool {
                name: "hardware_select",
                description: "Ask which hardware backend would handle a codec in a direction under a policy, with the reason each candidate was accepted or rejected.",
                cli: "ironav hw select --codec C --direction decode|encode [--policy P]",
                input_schema: object(
                    json!({
                        "codec": { "type": "string" },
                        "direction": { "type": "string", "enum": ["decode", "encode"] },
                        "policy": { "type": "string", "default": "auto" }
                    }),
                    &["codec", "direction"],
                ),
                read_only: true,
                handler: Handler::Plain(h_hw_select),
            },
            Tool {
                name: "play_open",
                description: "Open a media file for playback and return a session id and the player status. Plays from the start unless autoplay is false. Sessions last until play_close or the end of the process. hevc video needs the GPU decoder (h264 falls back to software); audio is silent when there is no sound device.",
                cli: "ironav play <file> --json   (JSON lines on stdin/stdout)",
                input_schema: object(
                    json!({
                        "path": { "type": "string" },
                        "audio": { "type": "string", "enum": ["auto", "silent", "off"], "default": "auto", "description": "auto = the default sound device; silent = keep time with the audio but make no sound; off = ignore audio tracks." },
                        "hwaccel": { "type": "string", "default": "auto" },
                        "start": { "type": "number", "minimum": 0, "description": "Start position in seconds." },
                        "autoplay": { "type": "boolean", "default": true },
                        "loop": { "type": "boolean", "default": false },
                        "volume": { "type": "number", "minimum": 0, "maximum": 2, "default": 1 }
                    }),
                    &["path"],
                ),
                read_only: true,
                handler: Handler::Plain(crate::sessions::open),
            },
            Tool {
                name: "play_control",
                description: "Send one command to a playback session and get the status back. Commands: play, pause, toggle, seek (to | by | fraction), volume (value), mute (on), loop (on), snapshot (the current picture as PNG; inline base64 unless path is given), wait (until position or state=ended), status, help. The status reports state, position, stream info, frames shown and skipped, and the audio level, which is above zero while sound is actually being played.",
                cli: "ironav play <file> --json  then {\"cmd\":\"seek\",\"to\":30}",
                input_schema: object(
                    json!({
                        "session": { "type": "string" },
                        "cmd": { "type": "string", "enum": ["status", "play", "pause", "toggle", "seek", "volume", "mute", "loop", "snapshot", "wait", "help"] },
                        "to": { "type": "number", "description": "seek: absolute position in seconds" },
                        "by": { "type": "number", "description": "seek: relative offset in seconds, may be negative" },
                        "fraction": { "type": "number", "minimum": 0, "maximum": 1, "description": "seek: position as a fraction of the duration" },
                        "value": { "type": "number", "minimum": 0, "maximum": 2, "description": "volume: linear gain" },
                        "on": { "type": "boolean", "description": "mute / loop: the new setting; toggles when omitted" },
                        "max_width": { "type": "integer", "description": "snapshot: shrink to fit this width" },
                        "max_height": { "type": "integer", "description": "snapshot: shrink to fit this height" },
                        "path": { "type": "string", "description": "snapshot: write the PNG here instead of returning it inline" },
                        "position": { "type": "number", "description": "wait: seconds into the media to wait for" },
                        "state": { "type": "string", "enum": ["ended"], "description": "wait: state to wait for" },
                        "timeout": { "type": "number", "default": 30, "description": "wait: give up after this many seconds" }
                    }),
                    &["session", "cmd"],
                ),
                read_only: false,
                handler: Handler::Plain(crate::sessions::control),
            },
            Tool {
                name: "play_status",
                description: "The current status of a playback session, without changing anything.",
                cli: "ironav play <file> --json  then {\"cmd\":\"status\"}",
                input_schema: object(json!({ "session": { "type": "string" } }), &["session"]),
                read_only: true,
                handler: Handler::Plain(crate::sessions::status),
            },
            Tool {
                name: "play_close",
                description: "Stop a playback session and release its decoder and sound device.",
                cli: "(quit the ironav play process)",
                input_schema: object(json!({ "session": { "type": "string" } }), &["session"]),
                read_only: false,
                handler: Handler::Plain(crate::sessions::close),
            },
        ]
    })
}

/// The full ontology: registry-derived entities plus one `Operation` per tool, so an
/// agent walking the graph also discovers how to act on it.
pub fn ontology() -> &'static Ontology {
    static ONTOLOGY: OnceLock<Ontology> = OnceLock::new();
    ONTOLOGY.get_or_init(|| {
        let mut o = ironav_ontology::build();
        for t in tools() {
            o.add_entity(
                entity(Kind::Operation, t.name, t.name, t.description, Status::Implemented)
                    .with_prop("cli", t.cli)
                    .with_prop("read_only", t.read_only)
                    .with_prop("mcp_tool", t.name)
                    .with_prop("input_schema", t.input_schema.clone()),
            );
        }
        for k in ironav_analyze::KINDS {
            o.add_entity(entity(Kind::Observation, k.name, k.name, k.description, Status::Implemented).with_prop("level", k.level.name()));
        }
        for a in ironav_analyze::ANALYZERS {
            o.add_entity(
                entity(Kind::Analyzer, a.name, a.name, a.description, Status::Implemented)
                    .with_prop("consumes", serde_json::to_value(a.consumes).unwrap_or_default())
                    .with_prop("mcp_tool", "analyze"),
            );
            for e in a.emits {
                o.add_relation(&format!("analyzer/{}", a.name), Predicate::Emits, &format!("observation/{e}"), None);
            }
        }
        o
    })
}
