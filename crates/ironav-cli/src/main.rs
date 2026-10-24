//! `ironav` — the command-line front end. Every command prints one JSON envelope on
//! stdout: `{"ok":true,"result":...}` or `{"ok":false,"error":{code,message,hint,...}}`,
//! and exits non-zero on error (see `ErrorCode::exit_code`).

mod mcp;

use ironav_core::{Error, Result};
use ironav_engine::{call_tool, ontology, tools, FilterCall, Job};
use serde_json::{json, Value};
use std::io::{IsTerminal, Read};

struct Args {
    rest: Vec<String>,
}

impl Args {
    fn flag(&mut self, names: &[&str]) -> Result<Option<String>> {
        let Some(i) = self.rest.iter().position(|a| names.contains(&a.as_str())) else { return Ok(None) };
        self.rest.remove(i);
        if i >= self.rest.len() {
            return Err(Error::invalid_arg(format!("{} needs a value", names[0])));
        }
        Ok(Some(self.rest.remove(i)))
    }
    fn flags(&mut self, names: &[&str]) -> Result<Vec<String>> {
        let mut v = Vec::new();
        while let Some(x) = self.flag(names)? {
            v.push(x);
        }
        Ok(v)
    }
    fn switch(&mut self, names: &[&str]) -> bool {
        match self.rest.iter().position(|a| names.contains(&a.as_str())) {
            Some(i) => {
                self.rest.remove(i);
                true
            }
            None => false,
        }
    }
    fn positional(&mut self, what: &str) -> Result<String> {
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
            "convert -i IN -o OUT [-f FMT] [--acodec C] [--vcodec C] [--filter name[:k=v,..]]... [--hwaccel P] [-y] [--dry-run] [--progress]": "convert, handbrake/ffmpeg style",
            "plan <job.json|->": "validate a JSON job without writing",
            "run <job.json|->": "execute a JSON job",
            "hw probe | hw select --codec C --direction decode|encode [--policy P]": "hardware acceleration",
            "ontology summary | search [text] [--kind K] [--limit N] | describe ID | convert FROM TO | jsonld": "knowledge graph",
            "tools": "tool definitions with JSON Schemas (function-calling compatible)",
            "mcp": "serve all tools over the Model Context Protocol on stdio",
            "gui [--port N] [--no-open]": "open the graphical interface in your browser"
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
        "convert" => {
            let dry = a.switch(&["--dry-run", "-n"]);
            let overwrite = a.switch(&["-y", "--overwrite"]);
            let mut job = Job {
                input: a.flag(&["-i", "--input"])?.ok_or_else(|| Error::invalid_arg("missing -i <input>"))?,
                output: a.flag(&["-o", "--output"])?.ok_or_else(|| Error::invalid_arg("missing -o <output>"))?,
                format: a.flag(&["-f", "--format"])?,
                hwaccel: a.flag(&["--hwaccel"])?.unwrap_or_else(|| "auto".into()),
                overwrite,
                ..Job::default()
            };
            job.audio.codec = a.flag(&["--acodec"])?;
            job.video.codec = a.flag(&["--vcodec"])?;
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
            .alternatives(["capabilities", "probe", "convert", "plan", "run", "hw", "ontology", "tools", "mcp", "gui"])),
    }
}

fn run_job(v: Value, progress: bool) -> Result<Value> {
    if !progress {
        return call_tool("run", &v);
    }
    let job: Job = serde_json::from_value(v).map_err(|e| Error::invalid_arg(format!("invalid job: {e}")))?;
    let report = ironav_engine::run(&job, &mut |p| eprintln!("{}", json!({ "event": "progress", "progress": p })))?;
    Ok(serde_json::to_value(report)?)
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
        Some("gui") => {
            let mut a = Args { rest: rest.split_off(1) };
            let result = (|| -> Result<()> {
                let port = a.flag(&["--port"])?.map(|p| p.parse::<u16>()).transpose().map_err(|_| Error::invalid_arg("--port must be 0-65535"))?;
                ironav_gui::serve(ironav_gui::Options { port: port.unwrap_or(0), open_browser: !a.switch(&["--no-open"]) })
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
