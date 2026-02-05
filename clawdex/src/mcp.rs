use std::io::{self, BufRead, Write};

use anyhow::Result;
use serde_json::{json, Value};

use codex_protocol::mcp::{CallToolResult, Tool};

use crate::config::{
    resolve_cron_enabled, resolve_heartbeat_enabled, ClawdConfig, ClawdPaths,
};
use crate::cron;
use crate::daemon_client;
use crate::gateway;
use crate::heartbeat;
use crate::memory;

const CRON_ADD_REQUEST_SCHEMA: &str =
    include_str!("../../compat/tool-schemas/cron.add.request.schema.json");
const CRON_ADD_RESPONSE_SCHEMA: &str =
    include_str!("../../compat/tool-schemas/cron.add.response.schema.json");
const CRON_UPDATE_REQUEST_SCHEMA: &str =
    include_str!("../../compat/tool-schemas/cron.update.request.schema.json");
const CRON_UPDATE_RESPONSE_SCHEMA: &str =
    include_str!("../../compat/tool-schemas/cron.update.response.schema.json");
const CRON_LIST_REQUEST_SCHEMA: &str =
    include_str!("../../compat/tool-schemas/cron.list.request.schema.json");
const CRON_LIST_RESPONSE_SCHEMA: &str =
    include_str!("../../compat/tool-schemas/cron.list.response.schema.json");
const CRON_REMOVE_REQUEST_SCHEMA: &str =
    include_str!("../../compat/tool-schemas/cron.remove.request.schema.json");
const CRON_REMOVE_RESPONSE_SCHEMA: &str =
    include_str!("../../compat/tool-schemas/cron.remove.response.schema.json");
const CRON_RUN_REQUEST_SCHEMA: &str =
    include_str!("../../compat/tool-schemas/cron.run.request.schema.json");
const CRON_RUN_RESPONSE_SCHEMA: &str =
    include_str!("../../compat/tool-schemas/cron.run.response.schema.json");
const CRON_RUNS_REQUEST_SCHEMA: &str =
    include_str!("../../compat/tool-schemas/cron.runs.request.schema.json");
const CRON_RUNS_RESPONSE_SCHEMA: &str =
    include_str!("../../compat/tool-schemas/cron.runs.response.schema.json");
const CRON_STATUS_REQUEST_SCHEMA: &str =
    include_str!("../../compat/tool-schemas/cron.status.request.schema.json");
const CRON_STATUS_RESPONSE_SCHEMA: &str =
    include_str!("../../compat/tool-schemas/cron.status.response.schema.json");
const MEMORY_SEARCH_REQUEST_SCHEMA: &str =
    include_str!("../../compat/tool-schemas/memory_search.request.schema.json");
const MEMORY_SEARCH_RESPONSE_SCHEMA: &str =
    include_str!("../../compat/tool-schemas/memory_search.response.schema.json");
const MEMORY_GET_REQUEST_SCHEMA: &str =
    include_str!("../../compat/tool-schemas/memory_get.request.schema.json");
const MEMORY_GET_RESPONSE_SCHEMA: &str =
    include_str!("../../compat/tool-schemas/memory_get.response.schema.json");
const MESSAGE_SEND_REQUEST_SCHEMA: &str =
    include_str!("../../compat/tool-schemas/message.send.request.schema.json");
const MESSAGE_SEND_RESPONSE_SCHEMA: &str =
    include_str!("../../compat/tool-schemas/message.send.response.schema.json");
const CHANNELS_LIST_REQUEST_SCHEMA: &str =
    include_str!("../../compat/tool-schemas/channels.list.request.schema.json");
const CHANNELS_LIST_RESPONSE_SCHEMA: &str =
    include_str!("../../compat/tool-schemas/channels.list.response.schema.json");
const CHANNELS_RESOLVE_REQUEST_SCHEMA: &str =
    include_str!("../../compat/tool-schemas/channels.resolve_target.request.schema.json");
const CHANNELS_RESOLVE_RESPONSE_SCHEMA: &str =
    include_str!("../../compat/tool-schemas/channels.resolve_target.response.schema.json");
const HEARTBEAT_WAKE_REQUEST_SCHEMA: &str =
    include_str!("../../compat/tool-schemas/heartbeat.wake.request.schema.json");
const HEARTBEAT_WAKE_RESPONSE_SCHEMA: &str =
    include_str!("../../compat/tool-schemas/heartbeat.wake.response.schema.json");

#[derive(Debug)]
struct JsonRpcError {
    code: i64,
    message: String,
}

impl JsonRpcError {
    fn invalid_request(message: impl Into<String>) -> Self {
        Self {
            code: -32600,
            message: message.into(),
        }
    }

    fn method_not_found(message: impl Into<String>) -> Self {
        Self {
            code: -32601,
            message: message.into(),
        }
    }

    fn invalid_params(message: impl Into<String>) -> Self {
        Self {
            code: -32602,
            message: message.into(),
        }
    }

    fn internal(message: impl Into<String>) -> Self {
        Self {
            code: -32000,
            message: message.into(),
        }
    }
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
            write_jsonrpc_error(&mut stdout, id, JsonRpcError::invalid_request("missing method"))?;
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
                write_jsonrpc_error(&mut stdout, id, err)?;
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
) -> std::result::Result<Value, JsonRpcError> {
    match method {
        "initialize" => Ok(initialize_response(&params)),
        "tools/list" => Ok(tools_list_response()),
        "tools/call" => handle_tool_call(cfg, paths, &params, cron_enabled, heartbeat_enabled),
        "ping" => Ok(json!({ "ok": true })),
        _ => Err(JsonRpcError::method_not_found(format!(
            "unknown method: {method}"
        ))),
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
            input_schema: schema_value(CRON_LIST_REQUEST_SCHEMA),
            output_schema: Some(schema_value(CRON_LIST_RESPONSE_SCHEMA)),
            annotations: None,
            icons: None,
            meta: None,
        },
        Tool {
            name: "cron.status".to_string(),
            title: None,
            description: Some("Cron status summary".to_string()),
            input_schema: schema_value(CRON_STATUS_REQUEST_SCHEMA),
            output_schema: Some(schema_value(CRON_STATUS_RESPONSE_SCHEMA)),
            annotations: None,
            icons: None,
            meta: None,
        },
        Tool {
            name: "cron.add".to_string(),
            title: None,
            description: Some("Create a cron job".to_string()),
            input_schema: schema_value(CRON_ADD_REQUEST_SCHEMA),
            output_schema: Some(schema_value(CRON_ADD_RESPONSE_SCHEMA)),
            annotations: None,
            icons: None,
            meta: None,
        },
        Tool {
            name: "cron.update".to_string(),
            title: None,
            description: Some("Update a cron job".to_string()),
            input_schema: schema_value(CRON_UPDATE_REQUEST_SCHEMA),
            output_schema: Some(schema_value(CRON_UPDATE_RESPONSE_SCHEMA)),
            annotations: None,
            icons: None,
            meta: None,
        },
        Tool {
            name: "cron.remove".to_string(),
            title: None,
            description: Some("Remove a cron job".to_string()),
            input_schema: schema_value(CRON_REMOVE_REQUEST_SCHEMA),
            output_schema: Some(schema_value(CRON_REMOVE_RESPONSE_SCHEMA)),
            annotations: None,
            icons: None,
            meta: None,
        },
        Tool {
            name: "cron.run".to_string(),
            title: None,
            description: Some("Run cron jobs".to_string()),
            input_schema: schema_value(CRON_RUN_REQUEST_SCHEMA),
            output_schema: Some(schema_value(CRON_RUN_RESPONSE_SCHEMA)),
            annotations: None,
            icons: None,
            meta: None,
        },
        Tool {
            name: "cron.runs".to_string(),
            title: None,
            description: Some("List cron run history".to_string()),
            input_schema: schema_value(CRON_RUNS_REQUEST_SCHEMA),
            output_schema: Some(schema_value(CRON_RUNS_RESPONSE_SCHEMA)),
            annotations: None,
            icons: None,
            meta: None,
        },
        Tool {
            name: "memory_search".to_string(),
            title: None,
            description: Some("Search memory markdown files".to_string()),
            input_schema: schema_value(MEMORY_SEARCH_REQUEST_SCHEMA),
            output_schema: Some(schema_value(MEMORY_SEARCH_RESPONSE_SCHEMA)),
            annotations: None,
            icons: None,
            meta: None,
        },
        Tool {
            name: "memory_get".to_string(),
            title: None,
            description: Some("Read a memory file".to_string()),
            input_schema: schema_value(MEMORY_GET_REQUEST_SCHEMA),
            output_schema: Some(schema_value(MEMORY_GET_RESPONSE_SCHEMA)),
            annotations: None,
            icons: None,
            meta: None,
        },
        Tool {
            name: "message.send".to_string(),
            title: None,
            description: Some("Send a message via the gateway".to_string()),
            input_schema: schema_value(MESSAGE_SEND_REQUEST_SCHEMA),
            output_schema: Some(schema_value(MESSAGE_SEND_RESPONSE_SCHEMA)),
            annotations: None,
            icons: None,
            meta: None,
        },
        Tool {
            name: "channels.list".to_string(),
            title: None,
            description: Some("List gateway routes".to_string()),
            input_schema: schema_value(CHANNELS_LIST_REQUEST_SCHEMA),
            output_schema: Some(schema_value(CHANNELS_LIST_RESPONSE_SCHEMA)),
            annotations: None,
            icons: None,
            meta: None,
        },
        Tool {
            name: "channels.resolve_target".to_string(),
            title: None,
            description: Some("Resolve messaging target from known routes".to_string()),
            input_schema: schema_value(CHANNELS_RESOLVE_REQUEST_SCHEMA),
            output_schema: Some(schema_value(CHANNELS_RESOLVE_RESPONSE_SCHEMA)),
            annotations: None,
            icons: None,
            meta: None,
        },
        Tool {
            name: "heartbeat.wake".to_string(),
            title: None,
            description: Some("Trigger a heartbeat run".to_string()),
            input_schema: schema_value(HEARTBEAT_WAKE_REQUEST_SCHEMA),
            output_schema: Some(schema_value(HEARTBEAT_WAKE_RESPONSE_SCHEMA)),
            annotations: None,
            icons: None,
            meta: None,
        },
    ]
}

fn schema_value(schema: &str) -> Value {
    serde_json::from_str(schema).unwrap_or_else(|_| json!({}))
}

fn handle_tool_call(
    cfg: &ClawdConfig,
    paths: &ClawdPaths,
    params: &Value,
    cron_enabled: bool,
    heartbeat_enabled: bool,
) -> std::result::Result<Value, JsonRpcError> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| JsonRpcError::invalid_params("tools/call requires name"))?;
    let arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    if !arguments.is_object() {
        return Err(JsonRpcError::invalid_params("arguments must be an object"));
    }

    let result = match name {
        "cron.list" => cron::list_jobs(
            paths,
            arguments
                .get("includeDisabled")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
        )
        .map_err(|err| JsonRpcError::internal(err.to_string()))?,
        "cron.status" => cron::status(paths, cron_enabled)
            .map_err(|err| JsonRpcError::internal(err.to_string()))?,
        "cron.add" => cron::add_job(paths, &arguments)
            .map_err(|err| JsonRpcError::internal(err.to_string()))?,
        "cron.update" => cron::update_job(paths, &arguments)
            .map_err(|err| JsonRpcError::internal(err.to_string()))?,
        "cron.remove" => {
            let job_id = arguments
                .get("jobId")
                .or_else(|| arguments.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if job_id.is_empty() {
                return Err(JsonRpcError::invalid_params("missing jobId"));
            }
            cron::remove_job(paths, &arguments)
                .map_err(|err| JsonRpcError::internal(err.to_string()))?
        }
        "cron.run" => {
            let job_id = arguments
                .get("jobId")
                .or_else(|| arguments.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if job_id.is_empty() {
                return Err(JsonRpcError::invalid_params("missing jobId"));
            }
            let mode = arguments
                .get("mode")
                .and_then(|v| v.as_str())
                .unwrap_or("due");
            if !job_id.is_empty() {
                if let Some(result) = daemon_client::cron_run(job_id, mode) {
                    result
                } else {
                    cron::run_jobs(paths, &arguments)
                        .map_err(|err| JsonRpcError::internal(err.to_string()))?
                }
            } else {
                cron::run_jobs(paths, &arguments)
                    .map_err(|err| JsonRpcError::internal(err.to_string()))?
            }
        }
        "cron.runs" => {
            let job_id = arguments
                .get("jobId")
                .or_else(|| arguments.get("id"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if job_id.is_empty() {
                return Err(JsonRpcError::invalid_params("missing jobId"));
            }
            cron::runs(paths, &arguments)
                .map_err(|err| JsonRpcError::internal(err.to_string()))?
        }
        "memory_search" => {
            let query = arguments.get("query").and_then(|v| v.as_str()).unwrap_or("");
            if query.trim().is_empty() {
                return Err(JsonRpcError::invalid_params("missing query"));
            }
            memory::memory_search(paths, &arguments)
                .map_err(|err| JsonRpcError::internal(err.to_string()))?
        }
        "memory_get" => {
            let path = arguments.get("path").and_then(|v| v.as_str()).unwrap_or("");
            if path.trim().is_empty() {
                return Err(JsonRpcError::invalid_params("missing path"));
            }
            memory::memory_get(paths, &arguments)
                .map_err(|err| JsonRpcError::internal(err.to_string()))?
        }
        "message.send" => {
            let text = arguments
                .get("text")
                .or_else(|| arguments.get("message"))
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if text.trim().is_empty() {
                return Err(JsonRpcError::invalid_params("missing text"));
            }
            gateway::send_message(paths, &arguments)
                .map_err(|err| JsonRpcError::internal(err.to_string()))?
        }
        "channels.list" => gateway::list_channels(paths)
            .map_err(|err| JsonRpcError::internal(err.to_string()))?,
        "channels.resolve_target" => gateway::resolve_target(paths, &arguments)
            .map_err(|err| JsonRpcError::internal(err.to_string()))?,
        "heartbeat.wake" => {
            if !heartbeat_enabled {
                json!({ "ok": false, "reason": "heartbeat disabled" })
            } else {
                let reason = arguments.get("reason").and_then(|v| v.as_str()).map(|s| s.to_string());
                heartbeat::wake(cfg, paths, reason)
                    .map_err(|err| JsonRpcError::internal(err.to_string()))?
            }
        }
        _ => return Err(JsonRpcError::invalid_params(format!("unknown tool: {name}"))),
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
