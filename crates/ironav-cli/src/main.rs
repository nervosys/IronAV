//! `ironav` — the command-line front end. Every command prints one JSON envelope on
//! stdout: `{"ok":true,"result":...}` or `{"ok":false,"error":{code,message,hint,...}}`,
//! and exits non-zero on error (see `ErrorCode::exit_code`).

mod mcp;
mod play;

use ironav_core::{Error, Result};
use ironav_engine::{call_tool, ontology, tools, FilterCall, Job, Latency, OpusJob, OpusMode, OpusSignal, ProResProfile};
use serde_json::{json, Value};
use std::io::{IsTerminal, Read, Write};

pub(crate) struct Args {
    pub(crate) rest: Vec<String>,
}

impl Args {
    pub(crate) fn flag(&mut self, names: &[&str]) -> Result<Option<String>> {
        let Some(i) = self.rest.iter().position(|a| names.contains(&a.as_str())) else { return Ok(None) };
        self.rest.remove(i);
        if i >= self.rest.len() {
            return Err(Error::invalid_arg(format!("{} needs a value", names[0])));
        }
        Ok(Some(self.rest.remove(i)))
    }
    pub(crate) fn flags(&mut self, names: &[&str]) -> Result<Vec<String>> {
        let mut v = Vec::new();
        while let Some(x) = self.flag(names)? {
            v.push(x);
        }
        Ok(v)
    }
    pub(crate) fn switch(&mut self, names: &[&str]) -> bool {
        match self.rest.iter().position(|a| names.contains(&a.as_str())) {
            Some(i) => {
                self.rest.remove(i);
                true
            }
            None => false,
        }
    }
    pub(crate) fn positional(&mut self, what: &str) -> Result<String> {
        if self.rest.is_empty() {
            return Err(Error::invalid_arg(format!("missing {what}")).hint("run `ironav help` for usage"));
        }
        Ok(self.rest.remove(0))
    }
}

fn help() -> Value {
    json!({
        "name": "ironav",
        "version": env!("CARGO_PKG_VERSION"),
        "usage": "ironav <command> [args]   (all output is JSON; add --pretty for indentation)",
        "commands": {
            "capabilities": "what this build can do (start here)",
            "devices": "the cameras and microphones to watch (device://video/N, device://audio/NAME)",
            "onvif [URL] [--user U] [--password P] [--timeout-ms N]": "ONVIF cameras: without a URL, those on the local network; with one, its profiles and their RTSP and snapshot URIs",
            "probe <path>": "inspect a media file",
            "convert -i IN [-i MORE]... -o OUT [-f FMT] [--acodec C] [--vcodec C] [--abitrate KBPS] [--vbitrate KBPS | --qp N] [--latency low] [--keyint N] [--refs N] [--bframes N] [--tiles COLSxROWS] [--quarter-mv] [--altref LAG] [--fragment-seconds S] [--prores-profile proxy|lt|standard|hq|4444|4444xq] [--opus-mode M] [--opus-signal S] [--opus-frame-ms N] [--opus-fec PCT] [--opus-dtx] [--filter name[:k=v,..]]... [--hwaccel P] [-y] [--dry-run] [--progress]": "convert, handbrake/ffmpeg style",
            "analyze FILE [--analyzers a,b] [--sidecars a,b] [--level digest|events|raw] [--digest-seconds N] [--from S] [--to S] [--streams 0,1] [--max N] [--thumbnails DIR] [--hwaccel P] [--keyframes-only] [--jsonl]": "what is in a file: loudness, silence, scene cuts, black and frozen pictures, thumbnails, gaps; --jsonl prints each observation as a line as it is found",
            "watch SOURCE [--analyzers a,b] [--sidecars a,b] [--level digest|events|raw] [--digest-seconds N] [--format F] [--fps N] [--pace realtime] [--retain S] [--duration S] [--hwaccel P] [--keyframes-only] [--record OUT.ts|OUT.m3u8 [--segment-seconds S] [--keep-segments N] [-y]] [--transcode OUT.ts|OUT.m3u8 [--width N] [--qp N | --vbitrate KBPS]] [--commands]": "follow a live source (path, path?follow, -, tcp://host:port, tcp://:port, udp://:port, device://video[/N]?size=1280x720&fps=30, device://audio): every observation as a JSON line as it happens, then a summary; with --commands, stdin takes one command per line (status, query kinds=speech_end newest_first=true, configure add=silence, configure stop_recording=true record=next.ts, snapshot, clip from=1 to=5 output=c.ts, close, or the same as JSON) and each reply is a line {\"reply\":...}",
            "plan <job.json|->": "validate a JSON job without writing",
            "run <job.json|->": "execute a JSON job",
            "hw probe | hw select --codec C --direction decode|encode [--policy P]": "hardware acceleration",
            "ontology summary | search [text] [--kind K] [--limit N] | describe ID | convert FROM TO | jsonld": "knowledge graph",
            "tools": "tool definitions with JSON Schemas (function-calling compatible)",
            "mcp": "serve all tools over the Model Context Protocol on stdio",
            "play FILE [--audio auto|silent|off] [--start S] [--volume V] [--loop] [--paused] [--hwaccel P] [--tui|--json] [--until-end]": "play audio/video: a full-screen terminal player on a TTY, otherwise JSON lines on stdin/stdout ({\"cmd\":\"seek\",\"to\":30}); send {\"cmd\":\"help\"} for the commands",
            "gui [FILE] | gui --play [FILE] | gui --web [--port N] [--no-open]": "native desktop window (default), the player window (--play), or the browser-based UI"
        },
        "tools": tools().iter().map(|t| t.name).collect::<Vec<_>>(),
    })
}

fn read_job_arg(a: &mut Args) -> Result<Value> {
    let src = a.positional("job file ('-' reads stdin)")?;
    let text = if src == "-" {
        let mut s = String::new();
        std::io::stdin().read_to_string(&mut s)?;
        s
    } else {
        ironav_engine::policy::check_read(&src)?;
        std::fs::read_to_string(&src)?
    };
    Ok(serde_json::from_str(&text)?)
}

fn dispatch(mut a: Args, progress: bool) -> Result<Value> {
    let cmd = if a.rest.is_empty() { "help".to_string() } else { a.rest.remove(0) };
    match cmd.as_str() {
        "help" | "--help" | "-h" => Ok(help()),
        "version" | "--version" | "-V" => Ok(json!({ "name": "ironav", "version": env!("CARGO_PKG_VERSION") })),
        "capabilities" => call_tool("capabilities", &json!({})),
        "devices" => call_tool("devices", &json!({})),
        "onvif" => {
            let (user, password, ms) = (a.flag(&["--user", "-u"])?, a.flag(&["--password", "-p"])?, a.flag(&["--timeout-ms"])?);
            match a.rest.first().cloned() {
                Some(url) => call_tool("onvif_profiles", &json!({ "url": url, "user": user, "password": password })),
                None => call_tool("onvif_discover", &json!({ "timeout_ms": ms.map(|m| m.parse::<u64>()).transpose().map_err(|_| Error::invalid_arg("--timeout-ms must be a whole number"))? })),
            }
        }
        "probe" => call_tool("probe", &json!({ "path": a.positional("path")? })),
        "plan" => call_tool("plan", &read_job_arg(&mut a)?),
        "run" => run_job(read_job_arg(&mut a)?, progress),
        "analyze" => analyze(a, progress),
        "watch" => watch(a),
        "convert" => {
            let dry = a.switch(&["--dry-run", "-n"]);
            let overwrite = a.switch(&["-y", "--overwrite"]);
            let mut inputs = a.flags(&["-i", "--input"])?;
            if inputs.is_empty() {
                return Err(Error::invalid_arg("missing -i <input>"));
            }
            let mut job = Job {
                input: inputs.remove(0),
                extra_inputs: inputs,
                output: a.flag(&["-o", "--output"])?.ok_or_else(|| Error::invalid_arg("missing -o <output>"))?,
                format: a.flag(&["-f", "--format"])?,
                hwaccel: a.flag(&["--hwaccel"])?.unwrap_or_else(|| "auto".into()),
                overwrite,
                ..Job::default()
            };
            job.audio.codec = a.flag(&["--acodec"])?;
            job.video.codec = a.flag(&["--vcodec"])?;
            job.audio.bitrate_kbps = a.flag(&["--abitrate", "-b:a"])?.map(|v| v.parse().map_err(|_| Error::invalid_arg(format!("--abitrate must be a whole number of kbit/s, got '{v}'")))).transpose()?;
            job.video.bitrate_kbps = a.flag(&["--vbitrate", "-b:v"])?.map(|v| v.parse().map_err(|_| Error::invalid_arg(format!("--vbitrate must be a whole number of kbit/s, got '{v}'")))).transpose()?;
            job.video.qp = a.flag(&["--qp"])?.map(|v| v.parse().map_err(|_| Error::invalid_arg(format!("--qp must be a whole number 0-51, got '{v}'")))).transpose()?;
            job.video.latency = a
                .flag(&["--latency"])?
                .map(|v| match v.as_str() {
                    "low" => Ok(Latency::Low),
                    "normal" => Ok(Latency::Normal),
                    _ => Err(Error::invalid_arg(format!("--latency must be low or normal, got '{v}'"))),
                })
                .transpose()?;
            job.fragment_seconds = a
                .flag(&["--fragment-seconds"])?
                .map(|v| v.parse::<f64>().ok().filter(|s| s.is_finite() && *s > 0.0).ok_or_else(|| Error::invalid_arg(format!("--fragment-seconds must be a positive number of seconds, got '{v}'"))))
                .transpose()?;
            job.video.keyint = a.flag(&["--keyint"])?.map(|v| v.parse::<u32>().ok().filter(|&k| k > 0).ok_or_else(|| Error::invalid_arg(format!("--keyint must be a whole number of frames, at least 1, got '{v}'")))).transpose()?;
            job.video.refs = a.flag(&["--refs"])?.map(|v| v.parse::<u32>().ok().filter(|&n| (1..=16).contains(&n)).ok_or_else(|| Error::invalid_arg(format!("--refs must be a number of reference pictures from 1 to 16, got '{v}'")))).transpose()?;
            job.video.bframes = a
                .flag(&["--bframes"])?
                .map(|v| v.parse::<u32>().ok().filter(|&n| n <= 3).ok_or_else(|| Error::invalid_arg(format!("--bframes must be a number of B pictures from 0 to 3, got '{v}'"))))
                .transpose()?;
            job.video.tiles = a
                .flag(&["--tiles"])?
                .map(|v| match ironav_engine::job::parse_tiles(&v) {
                    Some(_) => Ok(v),
                    None => Err(Error::invalid_arg(format!("--tiles must be COLSxROWS, each a power of two from 1 to 64, such as 2x1, got '{v}'")).hint("for example 2x1 for two tile columns and one row")),
                })
                .transpose()?;
            job.video.quarter_mv = a.switch(&["--quarter-mv"]);
            job.video.altref = a
                .flag(&["--altref"])?
                .map(|v| v.parse::<u32>().ok().filter(|&n| (2..=16).contains(&n)).ok_or_else(|| Error::invalid_arg(format!("--altref must be a lag from 2 to 16 frames, got '{v}'"))))
                .transpose()?;
            job.video.prores_profile = a
                .flag(&["--prores-profile"])?
                .map(|v| match v.as_str() {
                    "proxy" => Ok(ProResProfile::Proxy),
                    "lt" => Ok(ProResProfile::Lt),
                    "standard" => Ok(ProResProfile::Standard),
                    "hq" => Ok(ProResProfile::Hq),
                    "4444" => Ok(ProResProfile::Fourfour),
                    "4444xq" => Ok(ProResProfile::FourfourXq),
                    _ => Err(Error::invalid_arg(format!("--prores-profile must be proxy, lt, standard, hq, 4444 or 4444xq, got '{v}'")).alternatives(["proxy", "lt", "standard", "hq", "4444", "4444xq"])),
                })
                .transpose()?;
            let opus = OpusJob {
                mode: a
                    .flag(&["--opus-mode"])?
                    .map(|v| match v.as_str() {
                        "celt" => Ok(OpusMode::Celt),
                        "silk" => Ok(OpusMode::Silk),
                        "hybrid" => Ok(OpusMode::Hybrid),
                        "auto" => Ok(OpusMode::Auto),
                        _ => Err(Error::invalid_arg(format!("--opus-mode must be celt, silk, hybrid or auto, got '{v}'")).alternatives(["celt", "silk", "hybrid", "auto"])),
                    })
                    .transpose()?,
                signal: a
                    .flag(&["--opus-signal"])?
                    .map(|v| match v.as_str() {
                        "auto" => Ok(OpusSignal::Auto),
                        "voice" => Ok(OpusSignal::Voice),
                        "music" => Ok(OpusSignal::Music),
                        _ => Err(Error::invalid_arg(format!("--opus-signal must be auto, voice or music, got '{v}'")).alternatives(["auto", "voice", "music"])),
                    })
                    .transpose()?,
                frame_ms: a.flag(&["--opus-frame-ms"])?.map(|v| v.parse().map_err(|_| Error::invalid_arg(format!("--opus-frame-ms must be a number of milliseconds, got '{v}'")))).transpose()?,
                fec_loss_percent: a.flag(&["--opus-fec"])?.map(|v| v.parse().map_err(|_| Error::invalid_arg(format!("--opus-fec must be a whole number of percent from 0 to 100, got '{v}'")))).transpose()?,
                dtx: a.switch(&["--opus-dtx"]),
            };
            if opus != OpusJob::default() {
                job.audio.opus = Some(opus);
            }
            for f in a.flags(&["--filter"])? {
                job.filters.push(FilterCall::parse_shorthand(&f)?);
            }
            if let Some(extra) = a.rest.first() {
                return Err(Error::invalid_arg(format!("unexpected argument '{extra}'")));
            }
            let v = serde_json::to_value(&job)?;
            if dry { call_tool("plan", &v) } else { run_job(v, progress) }
        }
        "hw" => match a.positional("hw subcommand (probe|select)")?.as_str() {
            "probe" => call_tool("hardware_probe", &json!({})),
            "select" => call_tool(
                "hardware_select",
                &json!({
                    "codec": a.flag(&["--codec"])?,
                    "direction": a.flag(&["--direction", "--dir"])?,
                    "policy": a.flag(&["--policy"])?,
                }),
            ),
            other => Err(Error::invalid_arg(format!("unknown hw subcommand '{other}'")).alternatives(["probe", "select"])),
        },
        "ontology" => match a.positional("ontology subcommand")?.as_str() {
            "summary" => Ok(ontology().summary()),
            "jsonld" => Ok(ontology().to_jsonld()),
            "describe" => call_tool("ontology_describe", &json!({ "id": a.positional("id")? })),
            "convert" => {
                let (from, to) = (a.positional("source codec")?, a.positional("target codec")?);
                call_tool("ontology_explain_conversion", &json!({ "from": from, "to": to }))
            }
            "search" => {
                let kind = a.flag(&["--kind"])?;
                let limit = a.flag(&["--limit"])?.map(|l| l.parse::<u64>()).transpose().map_err(|_| Error::invalid_arg("--limit must be an integer"))?;
                call_tool("ontology_search", &json!({ "query": a.rest.join(" "), "kind": kind, "limit": limit }))
            }
            other => Err(Error::invalid_arg(format!("unknown ontology subcommand '{other}'"))
                .alternatives(["summary", "search", "describe", "convert", "jsonld"])),
        },
        "tools" => Ok(json!(tools()
            .iter()
            .map(|t| json!({ "name": t.name, "description": t.description, "input_schema": t.input_schema }))
            .collect::<Vec<_>>())),
        other => Err(Error::invalid_arg(format!("unknown command '{other}'"))
            .hint("run `ironav help`")
            .alternatives(["capabilities", "devices", "onvif", "probe", "analyze", "watch", "convert", "plan", "run", "hw", "ontology", "tools", "mcp", "play", "gui"])),
    }
}

fn analyze(mut a: Args, progress: bool) -> Result<Value> {
    let input = a.positional("file to analyze")?;
    let jsonl = a.switch(&["--jsonl"]);
    let list = |v: Option<String>| v.map(|s| s.split(',').map(|x| x.trim().to_string()).filter(|x| !x.is_empty()).collect::<Vec<_>>());
    let num = |name: &str, v: Option<String>| -> Result<Option<f64>> {
        v.map(|s| s.parse::<f64>().map_err(|_| Error::invalid_arg(format!("{name} must be a number, got '{s}'")))).transpose()
    };
    let mut args = json!({ "input": input });
    if let Some(v) = list(a.flag(&["--analyzers", "-a"])?) {
        args["analyzers"] = json!(v);
    }
    if let Some(v) = list(a.flag(&["--sidecars"])?) {
        args["sidecars"] = json!(v);
    }
    if let Some(v) = a.flag(&["--level"])? {
        args["level"] = json!(v);
    }
    if let Some(v) = num("--digest-seconds", a.flag(&["--digest-seconds"])?)? {
        args["digest_seconds"] = json!(v);
    }
    if let Some(v) = num("--from", a.flag(&["--from", "--start"])?)? {
        args["from"] = json!(v);
    }
    if let Some(v) = num("--to", a.flag(&["--to", "--end"])?)? {
        args["to"] = json!(v);
    }
    if let Some(v) = list(a.flag(&["--streams"])?) {
        let ids = v.iter().map(|s| s.parse::<usize>().map_err(|_| Error::invalid_arg(format!("--streams takes stream numbers, got '{s}'")))).collect::<Result<Vec<_>>>()?;
        args["streams"] = json!(ids);
    }
    if let Some(v) = num("--max", a.flag(&["--max"])?)? {
        args["max_observations"] = json!(v as usize);
    }
    if let Some(v) = a.flag(&["--thumbnails"])? {
        args["thumbnail_dir"] = json!(v);
    }
    if let Some(v) = a.flag(&["--hwaccel"])? {
        args["hwaccel"] = json!(v);
    }
    if a.switch(&["--keyframes-only"]) {
        args["keyframes_only"] = json!(true);
    }
    if let Some(extra) = a.rest.first() {
        return Err(Error::invalid_arg(format!("unexpected argument '{extra}'")));
    }
    let report = |done: f64, total: Option<f64>, message: &str| eprintln!("{}", json!({ "event": "progress", "done": done, "total": total, "message": message }));
    let ctx = ironav_engine::Ctx { progress: progress.then_some(&report as &(dyn Fn(f64, Option<f64>, &str) + Sync)), cancel: None };
    if !jsonl {
        return ironav_engine::call_tool_with("analyze", &args, &ctx);
    }
    let job: ironav_engine::AnalyzeJob = serde_json::from_value(args).map_err(|e| Error::invalid_arg(format!("invalid arguments: {e}")))?;
    let mut out = std::io::stdout().lock();
    let mut result = ironav_engine::analyze(&job, &ctx, &mut |o| {
        let _ = writeln!(out, "{}", serde_json::to_string(o).unwrap_or_default());
        let _ = out.flush();
    })?;
    // the observations went out one per line; the closing envelope says how many
    if let Some(obj) = result.as_object_mut() {
        obj.remove("observations");
    }
    Ok(result)
}

fn watch(mut a: Args) -> Result<Value> {
    let source = a.positional("source to watch")?;
    let num = |name: &str, v: Option<String>| -> Result<Option<f64>> {
        v.map(|s| s.parse::<f64>().map_err(|_| Error::invalid_arg(format!("{name} must be a number, got '{s}'")))).transpose()
    };
    let mut args = json!({ "source": source });
    if let Some(v) = a.flag(&["--analyzers", "-a"])? {
        args["analyzers"] = json!(v.split(',').map(str::trim).filter(|s| !s.is_empty()).collect::<Vec<_>>());
    }
    if let Some(v) = a.flag(&["--sidecars"])? {
        args["sidecars"] = json!(v.split(',').map(str::trim).filter(|s| !s.is_empty()).collect::<Vec<_>>());
    }
    for (flag, key) in [("--level", "level"), ("--format", "format"), ("--pace", "pace"), ("--hwaccel", "hwaccel"), ("--record", "record")] {
        if let Some(v) = a.flag(&[flag])? {
            args[key] = json!(v);
        }
    }
    if let Some(v) = a.flag(&["--keep-segments"])? {
        args["keep_segments"] = json!(v.parse::<usize>().map_err(|_| Error::invalid_arg(format!("--keep-segments must be a whole number, got '{v}'")))?);
    }
    if a.switch(&["-y", "--overwrite"]) {
        args["overwrite"] = json!(true);
    }
    if a.switch(&["--keyframes-only"]) {
        args["keyframes_only"] = json!(true);
    }
    if let Some(out) = a.flag(&["--transcode"])? {
        let mut t = json!({ "output": out });
        for (flag, key) in [("--width", "width"), ("--qp", "qp"), ("--vbitrate", "bitrate_kbps")] {
            if let Some(v) = a.flag(&[flag])? {
                t[key] = json!(v.parse::<u32>().map_err(|_| Error::invalid_arg(format!("{flag} must be a whole number, got '{v}'")))?);
            }
        }
        if args.get("overwrite").is_some() {
            t["overwrite"] = json!(true);
        }
        args["transcode"] = t;
    }
    for (flag, key) in [("--digest-seconds", "digest_seconds"), ("--fps", "fps"), ("--retain", "retain_seconds"), ("--segment-seconds", "segment_seconds")] {
        if let Some(v) = num(flag, a.flag(&[flag])?)? {
            args[key] = json!(v);
        }
    }
    // without --record, the segment options are the transcode's
    if args.get("record").is_none() && args.get("transcode").is_some() {
        for key in ["segment_seconds", "keep_segments"] {
            if let Some(v) = args.as_object_mut().and_then(|o| o.remove(key)) {
                args["transcode"][key] = v;
            }
        }
    }
    let duration = num("--duration", a.flag(&["--duration"])?)?;
    let commands = a.switch(&["--commands"]);
    if let Some(extra) = a.rest.first() {
        return Err(Error::invalid_arg(format!("unexpected argument '{extra}'")));
    }
    let open: ironav_engine::live::OpenArgs = serde_json::from_value(args).map_err(|e| Error::invalid_arg(format!("invalid arguments: {e}")))?;
    let level = ironav_engine::analyze::Level::parse(&open.level).unwrap_or(ironav_engine::analyze::Level::Events);
    if commands && ironav_live::SourceUrl::parse(&open.source).is_ok_and(|u| u.scheme == "pipe") {
        return Err(Error::invalid_arg("--commands reads stdin, which is the source here").hint("send the media over tcp:// or udp:// instead"));
    }
    let s = ironav_engine::live::open(&open)?;
    let replies = commands.then(|| watch_commands(s.id.clone()));
    let t0 = std::time::Instant::now();
    let mut after = 0;
    let mut out = std::io::stdout().lock();
    let wait = std::time::Duration::from_millis(if commands { 100 } else { 250 });
    let mut closing = false;
    loop {
        let r = s.events(after, &[], level, 1000, wait);
        after = r["next_after"].as_u64().unwrap_or(after);
        for e in r["events"].as_array().into_iter().flatten() {
            let _ = writeln!(out, "{e}");
        }
        for (reply, close) in replies.iter().flat_map(|rx| rx.try_iter()) {
            let _ = writeln!(out, "{reply}");
            closing |= close;
        }
        let _ = out.flush();
        let done = matches!(r["state"].as_str(), Some("ended" | "failed" | "closed")) && !r["more"].as_bool().unwrap_or(false) && r["events"].as_array().is_none_or(|e| e.is_empty());
        if done || closing || duration.is_some_and(|d| t0.elapsed().as_secs_f64() >= d) {
            break;
        }
    }
    // answers to commands that were running when the source ended
    for (reply, _) in replies.iter().flat_map(|rx| rx.try_iter()) {
        let _ = writeln!(out, "{reply}");
    }
    let closed = ironav_engine::live::close(&s.id)?;
    if closed["state"] == "failed" {
        let e = &closed["error"];
        let code = ironav_core::ErrorCode::ALL.into_iter().find(|c| json!(c) == e["code"]).unwrap_or(ironav_core::ErrorCode::Internal);
        return Err(Error::new(code, e["message"].as_str().unwrap_or("the source failed")));
    }
    Ok(closed)
}

/// `watch --commands`: reads stdin on its own thread and answers each line by calling the
/// session's tool, so a slow one (configure waits for the session) does not hold up the
/// observations. Replies come back with whether the command was `close`.
fn watch_commands(session: String) -> std::sync::mpsc::Receiver<(Value, bool)> {
    use std::io::BufRead;
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        for line in std::io::stdin().lock().lines() {
            let Ok(line) = line else { break };
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let (reply, close) = watch_command(&session, line);
            if tx.send((reply, close)).is_err() || close {
                break;
            }
        }
    });
    rx
}

const WATCH_COMMANDS: [&str; 7] = ["status", "query", "configure", "snapshot", "clip", "close", "help"];
const WATCH_LISTS: [&str; 3] = ["kinds", "add", "remove"];

/// One command line: a JSON object with `cmd`, or the player's style, `query kinds=a,b max=5`.
/// A value is text for the text fields, a list (split at commas) for the list fields, and
/// otherwise JSON when it parses as JSON (numbers, true) or else text. Values have no spaces.
fn watch_command(session: &str, line: &str) -> (Value, bool) {
    let mut req = if line.starts_with('{') {
        match serde_json::from_str::<Value>(line) {
            Ok(v) if v.is_object() => v,
            _ => return (json!({ "reply": null, "ok": false, "error": Error::invalid_arg("not a JSON object").hint("send {\"cmd\":\"status\"} or a bare word such as status") }), false),
        }
    } else {
        let mut words = line.split_whitespace();
        let mut v = json!({ "cmd": words.next().unwrap_or_default() });
        for w in words {
            let Some((k, val)) = w.split_once('=').filter(|(k, _)| !matches!(*k, "cmd" | "id")) else {
                return (json!({ "reply": v["cmd"], "ok": false, "error": Error::invalid_arg(format!("'{w}' is not key=value")) }), false);
            };
            let text = matches!(k, "text" | "analyzer" | "output" | "format" | "level" | "record");
            v[k] = match serde_json::from_str::<Value>(val) {
                _ if text => json!(val),
                Ok(j) if !WATCH_LISTS.contains(&k) || j.is_array() => j,
                _ => json!(val),
            };
        }
        v
    };
    // a single name for a list field, in either form
    for k in WATCH_LISTS {
        if let Some(s) = req[k].as_str() {
            req[k] = json!(s.split(',').filter(|s| !s.is_empty()).collect::<Vec<_>>());
        }
    }
    let id = req.as_object_mut().and_then(|o| o.remove("id"));
    let cmd = req.as_object_mut().and_then(|o| o.remove("cmd")).and_then(|c| c.as_str().map(str::to_string)).unwrap_or_default();
    let cmd = if matches!(cmd.as_str(), "quit" | "stop" | "exit") { "close".to_string() } else { cmd };
    let result = match cmd.as_str() {
        "help" => Ok(json!({ "commands": WATCH_COMMANDS, "example": "query kinds=speech_end newest_first=true max=1", "tools": "each runs the stream_<command> tool with this session; see `ironav tools`" })),
        // the main loop closes the session once the observations before it are out
        "close" => Ok(json!({ "closing": true })),
        c if WATCH_COMMANDS.contains(&c) => {
            req["session"] = json!(session);
            call_tool(&format!("stream_{c}"), &req)
        }
        c => Err(Error::invalid_arg(format!("unknown command '{c}'")).alternatives(WATCH_COMMANDS)),
    };
    let mut reply = match result {
        Ok(v) => json!({ "reply": cmd, "ok": true, "result": v }),
        Err(e) => json!({ "reply": cmd, "ok": false, "error": e }),
    };
    if let Some(id) = id {
        reply["id"] = id;
    }
    (reply, cmd == "close")
}

fn run_job(v: Value, progress: bool) -> Result<Value> {
    if !progress {
        return call_tool("run", &v);
    }
    let job: Job = serde_json::from_value(v).map_err(|e| Error::invalid_arg(format!("invalid job: {e}")))?;
    let report = ironav_engine::run(&job, &mut |p| eprintln!("{}", json!({ "event": "progress", "progress": p })))?;
    Ok(serde_json::to_value(report)?)
}

/// Run the sibling `ironav-gui` program and wait for its window to close. It is a separate
/// executable so that this one does not load the graphics stack on every call.
fn native_gui(file: Option<String>, player: bool) -> Result<()> {
    let name = if cfg!(windows) { "ironav-gui.exe" } else { "ironav-gui" };
    let exe = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join(name))).filter(|p| p.exists());
    let Some(exe) = exe else {
        return Err(Error::new(ironav_core::ErrorCode::UnsupportedFormat, "the native GUI program (ironav-gui) is not installed next to ironav")
            .hint("build with default features (cargo build --release), or use `ironav gui --web`"));
    };
    let mut cmd = std::process::Command::new(exe);
    if player {
        cmd.arg("--play");
    }
    cmd.args(file);
    let status = cmd.status().map_err(|e| Error::new(ironav_core::ErrorCode::Io, format!("could not start ironav-gui: {e}")))?;
    if status.success() { Ok(()) } else { std::process::exit(status.code().unwrap_or(1)) }
}

fn main() {
    let mut rest: Vec<String> = std::env::args().skip(1).collect();
    let mut take = |name: &str| match rest.iter().position(|a| a == name) {
        Some(i) => {
            rest.remove(i);
            true
        }
        None => false,
    };
    let pretty = take("--pretty") || std::io::stdout().is_terminal();
    let progress = take("--progress");

    match rest.first().map(String::as_str) {
        Some("mcp") => {
            if let Err(e) = mcp::serve() {
                eprintln!("{}", serde_json::to_string(&e).unwrap_or_default());
                std::process::exit(e.code.exit_code());
            }
            return;
        }
        Some("play") => {
            let mut a = Args { rest: rest.split_off(1) };
            if let Err(e) = play::parse(&mut a).and_then(play::run) {
                println!("{}", serde_json::to_string(&json!({ "ok": false, "error": e })).unwrap_or_default());
                std::process::exit(e.code.exit_code());
            }
            return;
        }
        Some("gui") => {
            let mut a = Args { rest: rest.split_off(1) };
            let result = (|| -> Result<()> {
                if a.switch(&["--web"]) {
                    let port = a.flag(&["--port"])?.map(|p| p.parse::<u16>()).transpose().map_err(|_| Error::invalid_arg("--port must be 0-65535"))?;
                    ironav_engine::audit::set_caller(ironav_engine::audit::Caller::WebUi);
                    return ironav_webui::serve(ironav_webui::Options { port: port.unwrap_or(0), open_browser: !a.switch(&["--no-open"]) });
                }
                let player = a.switch(&["--play", "--player"]);
                native_gui(a.rest.first().cloned(), player)
            })();
            if let Err(e) = result {
                println!("{}", serde_json::to_string(&json!({ "ok": false, "error": e })).unwrap_or_default());
                std::process::exit(e.code.exit_code());
            }
            return;
        }
        _ => {}
    }

    let (envelope, code) = match dispatch(Args { rest }, progress) {
        Ok(v) => (json!({ "ok": true, "result": v }), 0),
        Err(e) => (json!({ "ok": false, "error": e }), e.code.exit_code()),
    };
    let text = if pretty { serde_json::to_string_pretty(&envelope) } else { serde_json::to_string(&envelope) };
    println!("{}", text.unwrap_or_default());
    std::process::exit(code);
}
