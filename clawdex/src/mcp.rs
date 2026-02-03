use std::io::{self, BufRead, Write};

use anyhow::{Context, Result};
use serde_json::{json, Value};

use codex_protocol::mcp::{CallToolResult, Tool};

use crate::config::{
    resolve_cron_enabled, resolve_heartbeat_enabled, ClawdConfig, ClawdPaths,
};
use crate::cron;
use crate::gateway;
use crate::heartbeat;
use crate::memory;

#[derive(Debug)]
struct JsonRpcError {
    code: i64,
    message: String,
}

pub fn run_mcp_server(
    cfg: ClawdConfig,
    paths: ClawdPaths,
    cron_enabled_override: bool,
    heartbeat_enabled_override: bool,
) -> Result<()> {
    let cron_enabled = cron_enabled_override && resolve_cron_enabled(&cfg);
    let heartbeat_enabled = heartbeat_enabled_override && resolve_heartbeat_enabled(&cfg);

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let line = match line {
            Ok(value) => value,
            Err(err) => {
                eprintln!("[clawdex][mcp] stdin error: {err}");
                continue;
            }
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let payload: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(err) => {
                eprintln!("[clawdex][mcp] invalid json: {err}");
                continue;
            }
        };

        let id = payload.get("id").cloned();
        let method = payload
            .get("method")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let params = payload.get("params").cloned();

        if id.is_none() || id == Some(Value::Null) {
            // Notification (e.g., notifications/initialized). Ignore.
            continue;
        }
        let Some(method) = method else {
            write_jsonrpc_error(&mut stdout, id, JsonRpcError {
                code: -32600,
                message: "missing method".to_string(),
            })?;
            continue;
        };

        let response = handle_request(
            &method,
            params.unwrap_or(Value::Null),
            &cfg,
            &paths,
            cron_enabled,
            heartbeat_enabled,
        );

        match response {
            Ok(result) => write_jsonrpc_result(&mut stdout, id.unwrap(), result)?,
            Err(err) => {
                write_jsonrpc_error(
                    &mut stdout,
                    id,
                    JsonRpcError {
                        code: -32000,
                        message: err.to_string(),
                    },
                )?;
            }
        }
    }

    Ok(())
}

fn handle_request(
    method: &str,
    params: Value,
    cfg: &ClawdConfig,
    paths: &ClawdPaths,
    cron_enabled: bool,
    heartbeat_enabled: bool,
) -> Result<Value> {
    match method {
        "initialize" => Ok(initialize_response(&params)),
        "tools/list" => Ok(tools_list_response()),
        "tools/call" => handle_tool_call(cfg, paths, &params, cron_enabled, heartbeat_enabled),
        "ping" => Ok(json!({ "ok": true })),
        _ => Err(anyhow::anyhow!("unknown method: {method}")),
    }
}

fn initialize_response(params: &Value) -> Value {
    let protocol_version = params
        .get("protocolVersion")
        .cloned()
        .unwrap_or_else(|| Value::String("2025-03-26".to_string()));
    json!({
        "protocolVersion": protocol_version,
        "capabilities": {
            "tools": { "listChanged": true },
        },
        "serverInfo": {
            "name": "clawdex",
            "title": "Clawdex",
            "version": env!("CARGO_PKG_VERSION"),
        }
    })
}

fn tools_list_response() -> Value {
    let tools = tool_definitions();
    json!({
        "tools": tools,
        "nextCursor": Value::Null,
    })
}

fn tool_definitions() -> Vec<Tool> {
    vec![
        Tool {
            name: "cron.list".to_string(),
            title: None,
            description: Some("List cron jobs".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "includeDisabled": { "type": "boolean" }
                },
                "additionalProperties": true
            }),
            output_schema: None,
            annotations: None,
            icons: None,
            meta: None,
        },
        Tool {
            name: "cron.status".to_string(),
            title: None,
            description: Some("Cron status summary".to_string()),
            input_schema: json!({
                "type": "object",
                "additionalProperties": true
            }),
            output_schema: None,
            annotations: None,
            icons: None,
            meta: None,
        },
        Tool {
            name: "cron.add".to_string(),
            title: None,
            description: Some("Create a cron job".to_string()),
            input_schema: json!({ "type": "object", "additionalProperties": true }),
            output_schema: None,
            annotations: None,
            icons: None,
            meta: None,
        },
        Tool {
            name: "cron.update".to_string(),
            title: None,
            description: Some("Update a cron job".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "jobId": { "type": "string" },
                    "id": { "type": "string" },
                    "patch": { "type": "object" }
                },
                "additionalProperties": true
            }),
            output_schema: None,
            annotations: None,
            icons: None,
            meta: None,
        },
        Tool {
            name: "cron.remove".to_string(),
            title: None,
            description: Some("Remove a cron job".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "jobId": { "type": "string" },
                    "id": { "type": "string" }
                },
                "additionalProperties": true
            }),
            output_schema: None,
            annotations: None,
            icons: None,
            meta: None,
        },
        Tool {
            name: "cron.run".to_string(),
            title: None,
            description: Some("Run cron jobs".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "jobId": { "type": "string" },
                    "id": { "type": "string" },
                    "mode": { "type": "string" }
                },
                "additionalProperties": true
            }),
            output_schema: None,
            annotations: None,
            icons: None,
            meta: None,
        },
        Tool {
            name: "cron.runs".to_string(),
            title: None,
            description: Some("List cron run history".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "jobId": { "type": "string" },
                    "id": { "type": "string" },
                    "limit": { "type": "number" }
                },
                "additionalProperties": true
            }),
            output_schema: None,
            annotations: None,
            icons: None,
            meta: None,
        },
        Tool {
            name: "memory_search".to_string(),
            title: None,
            description: Some("Search memory markdown files".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "maxResults": { "type": "number" },
                    "minScore": { "type": "number" }
                },
                "required": ["query"],
                "additionalProperties": true
            }),
            output_schema: None,
            annotations: None,
            icons: None,
            meta: None,
        },
        Tool {
            name: "memory_get".to_string(),
            title: None,
            description: Some("Read a memory file".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string" },
                    "from": { "type": "number" },
                    "lines": { "type": "number" }
                },
                "required": ["path"],
                "additionalProperties": true
            }),
            output_schema: None,
            annotations: None,
            icons: None,
            meta: None,
        },
        Tool {
            name: "message.send".to_string(),
            title: None,
            description: Some("Queue a message via the gateway outbox".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "channel": { "type": "string" },
                    "to": { "type": "string" },
                    "text": { "type": "string" },
                    "message": { "type": "string" },
                    "accountId": { "type": "string" },
                    "sessionKey": { "type": "string" },
                    "bestEffort": { "type": "boolean" },
                    "dryRun": { "type": "boolean" }
                },
                "additionalProperties": true
            }),
            output_schema: None,
            annotations: None,
            icons: None,
            meta: None,
        },
        Tool {
            name: "channels.list".to_string(),
            title: None,
            description: Some("List gateway routes".to_string()),
            input_schema: json!({ "type": "object", "additionalProperties": true }),
            output_schema: None,
            annotations: None,
            icons: None,
            meta: None,
        },
        Tool {
            name: "channels.resolve_target".to_string(),
            title: None,
            description: Some("Resolve messaging target from known routes".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "channel": { "type": "string" },
                    "to": { "type": "string" },
                    "accountId": { "type": "string" }
                },
                "additionalProperties": true
            }),
            output_schema: None,
            annotations: None,
            icons: None,
            meta: None,
        },
        Tool {
            name: "heartbeat.wake".to_string(),
            title: None,
            description: Some("Trigger a heartbeat run".to_string()),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "reason": { "type": "string" }
                },
                "additionalProperties": true
            }),
            output_schema: None,
            annotations: None,
            icons: None,
            meta: None,
        },
    ]
}

fn handle_tool_call(
    _cfg: &ClawdConfig,
    paths: &ClawdPaths,
    params: &Value,
    cron_enabled: bool,
    heartbeat_enabled: bool,
) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .context("tools/call requires name")?;
    let arguments = params.get("arguments").cloned().unwrap_or(Value::Null);

    let result = match name {
        "cron.list" => cron::list_jobs(paths, arguments.get("includeDisabled").and_then(|v| v.as_bool()).unwrap_or(false))?,
        "cron.status" => cron::status(paths, cron_enabled)?,
        "cron.add" => cron::add_job(paths, &arguments)?,
        "cron.update" => cron::update_job(paths, &arguments)?,
        "cron.remove" => cron::remove_job(paths, &arguments)?,
        "cron.run" => cron::run_jobs(paths, &arguments)?,
        "cron.runs" => cron::runs(paths, &arguments)?,
        "memory_search" => memory::memory_search(paths, &arguments)?,
        "memory_get" => memory::memory_get(paths, &arguments)?,
        "message.send" => gateway::send_message(paths, &arguments)?,
        "channels.list" => gateway::list_channels(paths)?,
        "channels.resolve_target" => gateway::resolve_target(paths, &arguments)?,
        "heartbeat.wake" => {
            if !heartbeat_enabled {
                json!({ "ok": false, "reason": "heartbeat disabled" })
            } else {
                let reason = arguments.get("reason").and_then(|v| v.as_str()).map(|s| s.to_string());
                heartbeat::wake(paths, reason)?
            }
        }
        _ => return Ok(error_result(format!("Unknown tool: {name}"))),
    };

    Ok(success_result(result))
}

fn success_result(value: Value) -> Value {
    let summary = serde_json::to_string_pretty(&value).unwrap_or_else(|_| value.to_string());
    let result = CallToolResult {
        content: vec![json!({ "type": "text", "text": summary })],
        structured_content: Some(value),
        is_error: None,
        meta: None,
    };
    serde_json::to_value(result).unwrap_or_else(|_| json!({}))
}

fn error_result(message: String) -> Value {
    let result = CallToolResult {
        content: vec![json!({ "type": "text", "text": message.clone() })],
        structured_content: Some(json!({ "error": message })),
        is_error: Some(true),
        meta: None,
    };
    serde_json::to_value(result).unwrap_or_else(|_| json!({}))
}

fn write_jsonrpc_result(stdout: &mut impl Write, id: Value, result: Value) -> Result<()> {
    let payload = json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    });
    writeln!(stdout, "{}", serde_json::to_string(&payload)?)?;
    stdout.flush().ok();
    Ok(())
}

fn write_jsonrpc_error(
    stdout: &mut impl Write,
    id: Option<Value>,
    err: JsonRpcError,
) -> Result<()> {
    let payload = json!({
        "jsonrpc": "2.0",
        "id": id.unwrap_or(Value::Null),
        "error": { "code": err.code, "message": err.message },
    });
    writeln!(stdout, "{}", serde_json::to_string(&payload)?)?;
    stdout.flush().ok();
    Ok(())
}
