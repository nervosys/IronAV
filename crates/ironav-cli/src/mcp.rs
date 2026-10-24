//! Minimal Model Context Protocol server over stdio (newline-delimited JSON-RPC 2.0).
//! Tools come from the shared registry; the ontology and tool schemas are also exposed as
//! resources so clients can load them into context without a tool call.
//!
//! The process stays resident, so an agent pays process-start and registry-build costs once
//! instead of on every conversion.

use ironav_core::{Error, ErrorCode, Result};
use ironav_engine::{call_tool, ontology, tools};
use serde_json::{json, Value};
use std::io::{BufRead, Write};

const PROTOCOL_VERSION: &str = "2025-06-18";

pub fn serve() -> Result<()> {
    let stdin = std::io::stdin();
    let mut out = std::io::stdout().lock();
    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let reply = match serde_json::from_str::<Value>(&line) {
            Err(e) => Some(rpc_error(Value::Null, -32700, &format!("parse error: {e}"))),
            Ok(msg) => handle(&msg),
        };
        if let Some(r) = reply {
            writeln!(out, "{r}")?;
            out.flush()?;
        }
    }
    Ok(())
}

fn rpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
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
            "instructions": "IronAV converts audio/video. Call `capabilities` first, use `ontology_*` tools to discover what exists, `plan` before `run`."
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
        "tools/call" => {
            let name = params.get("name").and_then(Value::as_str).unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            Ok(match call_tool(name, &args) {
                Ok(mut v) => {
                    // A snapshot goes out as a real image block, not as base64 text in JSON.
                    let image = v.get_mut("snapshot").and_then(|s| s.as_object_mut()).and_then(|s| s.remove("png_base64"));
                    let mut content = vec![json!({ "type": "text", "text": v.to_string() })];
                    if let Some(data) = image {
                        content.push(json!({ "type": "image", "data": data, "mimeType": "image/png" }));
                    }
                    json!({ "content": content, "structuredContent": v, "isError": false })
                }
                // Tool failures are results, not protocol errors: the model must see the hint.
                Err(e) => json!({ "content": [{ "type": "text", "text": error_text(&e) }], "isError": true }),
            })
        }
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
