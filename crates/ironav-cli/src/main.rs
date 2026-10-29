//! `ironav` — the command-line front end. Every command prints one JSON envelope on
//! stdout: `{"ok":true,"result":...}` or `{"ok":false,"error":{code,message,hint,...}}`,
//! and exits non-zero on error (see `ErrorCode::exit_code`).

mod mcp;
mod play;

use ironav_core::{Error, Result};
use ironav_engine::{call_tool, ontology, tools, FilterCall, Job};
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
            "probe <path>": "inspect a media file",
            "convert -i IN [-i MORE]... -o OUT [-f FMT] [--acodec C] [--vcodec C] [--abitrate KBPS] [--vbitrate KBPS | --qp N] [--filter name[:k=v,..]]... [--hwaccel P] [-y] [--dry-run] [--progress]": "convert, handbrake/ffmpeg style",
            "analyze FILE [--analyzers a,b] [--level digest|events|raw] [--digest-seconds N] [--from S] [--to S] [--streams 0,1] [--max N] [--thumbnails DIR] [--hwaccel P] [--jsonl]": "what is in a file: loudness, silence, scene cuts, black and frozen pictures, thumbnails, gaps; --jsonl prints each observation as a line as it is found",
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
        "probe" => call_tool("probe", &json!({ "path": a.positional("path")? })),
        "plan" => call_tool("plan", &read_job_arg(&mut a)?),
        "run" => run_job(read_job_arg(&mut a)?, progress),
        "analyze" => analyze(a, progress),
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
            .alternatives(["capabilities", "probe", "analyze", "convert", "plan", "run", "hw", "ontology", "tools", "mcp", "play", "gui"])),
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
