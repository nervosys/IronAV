//! Minimal Model Context Protocol server over stdio (newline-delimited JSON-RPC 2.0).
//! Tools come from the shared registry; the ontology and tool schemas are also exposed as
//! resources so clients can load them into context without a tool call.
//!
//! The process stays resident, so an agent pays process-start and registry-build costs once
//! instead of on every conversion. Each tool call runs on its own thread, so a long `run` or
//! `analyze` does not hold up other requests; it reports progress when the request carries a
//! `progressToken` and stops when the client sends `notifications/cancelled`.

use ironav_core::{Error, ErrorCode, Result};
use ironav_engine::{call_tool_with, ontology, tools, Ctx};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::io::{BufRead, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const PROTOCOL_VERSION: &str = "2025-06-18";
/// Progress notifications for one request are sent at most this often.
const PROGRESS_EVERY: Duration = Duration::from_millis(250);

/// Writes whole lines, one at a time, from any thread.
#[derive(Clone)]
struct Out(Arc<Mutex<std::io::Stdout>>);

impl Out {
    fn send(&self, v: &Value) {
        let mut o = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let _ = writeln!(o, "{v}");
        let _ = o.flush();
    }
}

pub fn serve() -> Result<()> {
    let out = Out(Arc::new(Mutex::new(std::io::stdout())));
    // requests still running, by their JSON-RPC id, and the flag that cancels each
    let running: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>> = Arc::default();
    let mut workers = Vec::new();
    for line in std::io::stdin().lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let msg = match serde_json::from_str::<Value>(&line) {
            Err(e) => {
                out.send(&rpc_error(Value::Null, -32700, &format!("parse error: {e}")));
                continue;
            }
            Ok(m) => m,
        };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
        if method == "notifications/cancelled" {
            if let Some(id) = msg.pointer("/params/requestId") {
                if let Some(flag) = running.lock().unwrap_or_else(|e| e.into_inner()).get(&id.to_string()) {
                    flag.store(true, Ordering::Relaxed);
                }
            }
            continue;
        }
        if let (Some(id), "tools/call") = (msg.get("id").cloned(), method) {
            let key = id.to_string();
            let cancel = Arc::new(AtomicBool::new(false));
            running.lock().unwrap_or_else(|e| e.into_inner()).insert(key.clone(), cancel.clone());
            let (out, running) = (out.clone(), running.clone());
            workers.push(std::thread::spawn(move || {
                let params = msg.get("params").cloned().unwrap_or(Value::Null);
                let result = call(&params, &out, &cancel);
                running.lock().unwrap_or_else(|e| e.into_inner()).remove(&key);
                // a cancelled request gets no response
                if !cancel.load(Ordering::Relaxed) {
                    out.send(&json!({ "jsonrpc": "2.0", "id": id, "result": result }));
                }
            }));
            workers.retain(|w| !w.is_finished());
            continue;
        }
        if let Some(reply) = handle(&msg) {
            out.send(&reply);
        }
    }
    // the client closed its end: let calls in flight finish and answer
    for w in workers {
        let _ = w.join();
    }
    Ok(())
}

fn rpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}

/// Runs one `tools/call` and builds its result.
fn call(params: &Value, out: &Out, cancel: &AtomicBool) -> Value {
    let name = params.get("name").and_then(Value::as_str).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    let token = params.pointer("/_meta/progressToken").cloned();
    let last = Mutex::new(None::<Instant>);
    let progress = |done: f64, total: Option<f64>, message: &str| {
        let Some(token) = &token else { return };
        let mut last = last.lock().unwrap_or_else(|e| e.into_inner());
        if last.is_some_and(|l| l.elapsed() < PROGRESS_EVERY) {
            return;
        }
        *last = Some(Instant::now());
        let mut p = json!({ "progressToken": token, "progress": done, "message": message });
        if let Some(t) = total {
            p["total"] = json!(t);
        }
        out.send(&json!({ "jsonrpc": "2.0", "method": "notifications/progress", "params": p }));
    };
    let ctx = Ctx { progress: Some(&progress), cancel: Some(cancel) };
    match call_tool_with(name, &args, &ctx) {
        Ok(mut v) => {
            // Pictures go out as real image blocks, not as base64 text in JSON: a player
            // snapshot, and thumbnails from `analyze`.
            let mut images = Vec::new();
            if let Some(data) = v.get_mut("snapshot").and_then(|s| s.as_object_mut()).and_then(|s| s.remove("png_base64")) {
                images.push(data);
            }
            if let Some(Value::Array(list)) = v.as_object_mut().and_then(|o| o.remove("images")) {
                v["image_count"] = json!(list.len());
                images.extend(list.into_iter().filter_map(|mut i| i.get_mut("png_base64").map(Value::take)));
            }
            let mut content = vec![json!({ "type": "text", "text": v.to_string() })];
            content.extend(images.into_iter().map(|data| json!({ "type": "image", "data": data, "mimeType": "image/png" })));
            json!({ "content": content, "structuredContent": v, "isError": false })
        }
        // Tool failures are results, not protocol errors: the model must see the hint.
        Err(e) => json!({ "content": [{ "type": "text", "text": error_text(&e) }], "isError": true }),
    }
}

fn handle(msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned();
    let method = msg.get("method").and_then(Value::as_str).unwrap_or("");
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    // Notifications (no id) never get a reply.
    let id = id?;
    let result: std::result::Result<Value, (i32, String)> = match method {
        "initialize" => Ok(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "capabilities": { "tools": {}, "resources": {} },
            "serverInfo": { "name": "ironav", "version": env!("CARGO_PKG_VERSION") },
            "instructions": "IronAV converts and understands audio/video. Call `capabilities` first, use `ontology_*` tools to discover what exists, `analyze` to learn what is in a file (level 'digest' costs the fewest tokens), and `plan` before `run`. Long calls report progress when given a progressToken and stop on notifications/cancelled."
        })),
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({
            "tools": tools().iter().map(|t| json!({
                "name": t.name,
                "description": t.description,
                "inputSchema": t.input_schema,
                "annotations": { "readOnlyHint": t.read_only, "destructiveHint": false, "idempotentHint": t.read_only, "openWorldHint": false }
            })).collect::<Vec<_>>()
        })),
        "resources/list" => Ok(json!({
            "resources": [
                { "uri": "ironav://ontology.jsonld", "name": "IronAV ontology (JSON-LD)", "mimeType": "application/ld+json" },
                { "uri": "ironav://tools.json", "name": "Tool definitions", "mimeType": "application/json" }
            ]
        })),
        "resources/read" => {
            let uri = params.get("uri").and_then(Value::as_str).unwrap_or("");
            let (mime, body) = match uri {
                "ironav://ontology.jsonld" => ("application/ld+json", ontology().to_jsonld().to_string()),
                "ironav://tools.json" => (
                    "application/json",
                    json!(tools().iter().map(|t| json!({ "name": t.name, "description": t.description, "input_schema": t.input_schema })).collect::<Vec<_>>())
                        .to_string(),
                ),
                other => return Some(rpc_error(id, -32002, &format!("unknown resource '{other}'"))),
            };
            Ok(json!({ "contents": [{ "uri": uri, "mimeType": mime, "text": body }] }))
        }
        other => Err((-32601, format!("method not found: {other}"))),
    };
    Some(match result {
        Ok(r) => json!({ "jsonrpc": "2.0", "id": id, "result": r }),
        Err((c, m)) => rpc_error(id, c, &m),
    })
}

fn error_text(e: &Error) -> String {
    serde_json::to_string(&json!({ "error": e })).unwrap_or_else(|_| format!("{:?}", ErrorCode::Internal))
}
