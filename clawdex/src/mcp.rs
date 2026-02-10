use std::io::{self, BufRead, Write};

use anyhow::Result;
use serde_json::{json, Map, Value};

use codex_protocol::mcp::{CallToolResult, Tool};
use jsonschema::{Draft, JSONSchema};

use crate::config::{
    resolve_cron_enabled, resolve_heartbeat_enabled, ClawdConfig, ClawdPaths,
};
use crate::artifacts;
use crate::cron;
use crate::daemon_client;
use crate::gateway;
use crate::heartbeat;
use crate::memory;
use crate::text_sanitize::strip_reasoning_tags_from_text;

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
const ARTIFACT_CREATE_XLSX_REQUEST_SCHEMA: &str =
    include_str!("../../compat/tool-schemas/artifact.create_xlsx.request.schema.json");
const ARTIFACT_CREATE_XLSX_RESPONSE_SCHEMA: &str =
    include_str!("../../compat/tool-schemas/artifact.create_xlsx.response.schema.json");
const ARTIFACT_CREATE_PPTX_REQUEST_SCHEMA: &str =
    include_str!("../../compat/tool-schemas/artifact.create_pptx.request.schema.json");
const ARTIFACT_CREATE_PPTX_RESPONSE_SCHEMA: &str =
    include_str!("../../compat/tool-schemas/artifact.create_pptx.response.schema.json");
const ARTIFACT_CREATE_DOCX_REQUEST_SCHEMA: &str =
    include_str!("../../compat/tool-schemas/artifact.create_docx.request.schema.json");
const ARTIFACT_CREATE_DOCX_RESPONSE_SCHEMA: &str =
    include_str!("../../compat/tool-schemas/artifact.create_docx.response.schema.json");
const ARTIFACT_CREATE_PDF_REQUEST_SCHEMA: &str =
    include_str!("../../compat/tool-schemas/artifact.create_pdf.request.schema.json");
const ARTIFACT_CREATE_PDF_RESPONSE_SCHEMA: &str =
    include_str!("../../compat/tool-schemas/artifact.create_pdf.response.schema.json");

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
            name: "artifact.create_xlsx".to_string(),
            title: None,
            description: Some("Create an Excel workbook from a structured spec".to_string()),
            input_schema: schema_value(ARTIFACT_CREATE_XLSX_REQUEST_SCHEMA),
            output_schema: Some(schema_value(ARTIFACT_CREATE_XLSX_RESPONSE_SCHEMA)),
            annotations: None,
            icons: None,
            meta: None,
        },
        Tool {
            name: "artifact.create_pptx".to_string(),
            title: None,
            description: Some("Create a PowerPoint deck from a structured spec".to_string()),
            input_schema: schema_value(ARTIFACT_CREATE_PPTX_REQUEST_SCHEMA),
            output_schema: Some(schema_value(ARTIFACT_CREATE_PPTX_RESPONSE_SCHEMA)),
            annotations: None,
            icons: None,
            meta: None,
        },
        Tool {
            name: "artifact.create_docx".to_string(),
            title: None,
            description: Some("Create a Word document from a structured spec".to_string()),
            input_schema: schema_value(ARTIFACT_CREATE_DOCX_REQUEST_SCHEMA),
            output_schema: Some(schema_value(ARTIFACT_CREATE_DOCX_RESPONSE_SCHEMA)),
            annotations: None,
            icons: None,
            meta: None,
        },
        Tool {
            name: "artifact.create_pdf".to_string(),
            title: None,
            description: Some("Create a PDF report from a structured spec".to_string()),
            input_schema: schema_value(ARTIFACT_CREATE_PDF_REQUEST_SCHEMA),
            output_schema: Some(schema_value(ARTIFACT_CREATE_PDF_RESPONSE_SCHEMA)),
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
    let mut arguments = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
    if !arguments.is_object() {
        return Err(JsonRpcError::invalid_params("arguments must be an object"));
    }
    validate_tool_arguments(name, &arguments)?;

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
            if let Some(map) = arguments.as_object_mut() {
                for field in ["text", "message"] {
                    if let Some(Value::String(raw)) = map.get_mut(field) {
                        *raw = strip_reasoning_tags_from_text(raw);
                    }
                }
            }
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
        "artifact.create_xlsx" => artifacts::create_xlsx(paths, &arguments)
            .map_err(|err| JsonRpcError::internal(err.to_string()))?,
        "artifact.create_pptx" => artifacts::create_pptx(paths, &arguments)
            .map_err(|err| JsonRpcError::internal(err.to_string()))?,
        "artifact.create_docx" => artifacts::create_docx(paths, &arguments)
            .map_err(|err| JsonRpcError::internal(err.to_string()))?,
        "artifact.create_pdf" => artifacts::create_pdf(paths, &arguments)
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

    let sanitized = sanitize_tool_response(name, result);
    validate_tool_response(name, &sanitized)?;
    Ok(success_result(sanitized))
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

fn sanitize_tool_response(name: &str, value: Value) -> Value {
    match name {
        "cron.list" => sanitize_cron_list_response(value),
        "cron.status" => sanitize_object_fields(
            value,
            &["enabled", "storePath", "jobs", "nextWakeAtMs"],
        ),
        "cron.add" | "cron.update" => sanitize_cron_job(value),
        "cron.remove" => sanitize_object_fields(value, &["ok", "removed"]),
        "cron.run" => sanitize_object_fields(value, &["ok", "ran", "reason"]),
        "cron.runs" => sanitize_cron_runs_response(value),
        "memory_search" => sanitize_memory_search_response(value),
        "memory_get" => sanitize_object_fields(
            value,
            &[
                "path",
                "text",
                "content",
                "from",
                "lines",
                "totalLines",
                "disabled",
                "error",
            ],
        ),
        "message.send" => sanitize_object_fields(
            value,
            &["ok", "dryRun", "result", "error", "bestEffort", "queued", "message"],
        ),
        "channels.list" => sanitize_channels_list_response(value),
        "channels.resolve_target" => sanitize_object_fields(
            value,
            &[
                "ok",
                "channel",
                "to",
                "accountId",
                "sessionKey",
                "updatedAtMs",
                "reason",
            ],
        ),
        "heartbeat.wake" => sanitize_object_fields(value, &["ok", "reason"]),
        "artifact.create_xlsx"
        | "artifact.create_pptx"
        | "artifact.create_docx"
        | "artifact.create_pdf" => sanitize_object_fields(
            value,
            &[
                "ok",
                "path",
                "absolutePath",
                "mime",
                "sha256",
                "sizeBytes",
                "recorded",
                "taskRunId",
            ],
        ),
        _ => value,
    }
}

fn sanitize_cron_list_response(value: Value) -> Value {
    let mut out = Map::new();
    if let Value::Object(map) = value {
        if let Some(Value::Array(jobs)) = map.get("jobs") {
            let sanitized = jobs.iter().map(sanitize_cron_job_ref).collect::<Vec<_>>();
            out.insert("jobs".to_string(), Value::Array(sanitized));
        }
    }
    Value::Object(out)
}

fn sanitize_cron_runs_response(value: Value) -> Value {
    let mut out = Map::new();
    if let Value::Object(map) = value {
        if let Some(Value::Array(entries)) = map.get("entries") {
            let sanitized = entries
                .iter()
                .map(|entry| sanitize_object_fields_ref(
                    entry,
                    &[
                        "ts",
                        "jobId",
                        "action",
                        "status",
                        "error",
                        "summary",
                        "runAtMs",
                        "durationMs",
                        "nextRunAtMs",
                    ],
                ))
                .collect::<Vec<_>>();
            out.insert("entries".to_string(), Value::Array(sanitized));
        }
    }
    Value::Object(out)
}

fn sanitize_cron_job(value: Value) -> Value {
    if let Value::Object(map) = value {
        Value::Object(sanitize_cron_job_map(&map))
    } else {
        value
    }
}

fn sanitize_cron_job_ref(value: &Value) -> Value {
    if let Value::Object(map) = value {
        Value::Object(sanitize_cron_job_map(map))
    } else {
        value.clone()
    }
}

fn sanitize_cron_job_map(map: &Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    insert_field(&mut out, map, "id");
    insert_non_null_field(&mut out, map, "agentId");
    insert_field(&mut out, map, "name");
    insert_field(&mut out, map, "description");
    insert_field(&mut out, map, "enabled");
    insert_field(&mut out, map, "deleteAfterRun");
    insert_field(&mut out, map, "createdAtMs");
    insert_field(&mut out, map, "updatedAtMs");
    if let Some(schedule) = map.get("schedule") {
        out.insert("schedule".to_string(), sanitize_cron_schedule(schedule));
    }
    insert_field(&mut out, map, "sessionTarget");
    insert_field(&mut out, map, "wakeMode");
    if let Some(payload) = map.get("payload") {
        out.insert("payload".to_string(), sanitize_cron_payload(payload));
    }
    if let Some(isolation) = map.get("isolation") {
        out.insert("isolation".to_string(), sanitize_cron_isolation(isolation));
    }
    if let Some(state) = map.get("state") {
        out.insert("state".to_string(), sanitize_cron_state(state));
    }
    if let Some(delivery) = map.get("delivery") {
        out.insert("delivery".to_string(), sanitize_cron_delivery(delivery));
    }
    if let Some(policy) = map.get("policy") {
        out.insert("policy".to_string(), policy.clone());
    }
    out
}

fn sanitize_cron_schedule(value: &Value) -> Value {
    if let Value::Object(map) = value {
        let mut out = Map::new();
        if let Some(kind) = map.get("kind").and_then(|v| v.as_str()) {
            out.insert("kind".to_string(), Value::String(kind.to_string()));
            match kind {
                "at" => {
                    insert_field(&mut out, map, "atMs");
                }
                "every" => {
                    insert_field(&mut out, map, "everyMs");
                    insert_field(&mut out, map, "anchorMs");
                }
                "cron" => {
                    insert_field(&mut out, map, "expr");
                    insert_field(&mut out, map, "tz");
                }
                _ => {}
            }
        }
        return Value::Object(out);
    }
    value.clone()
}

fn sanitize_cron_payload(value: &Value) -> Value {
    if let Value::Object(map) = value {
        let mut out = Map::new();
        if let Some(kind) = map.get("kind").and_then(|v| v.as_str()) {
            out.insert("kind".to_string(), Value::String(kind.to_string()));
            match kind {
                "systemEvent" => {
                    insert_field(&mut out, map, "text");
                }
                "agentTurn" => {
                    insert_field(&mut out, map, "message");
                    insert_field(&mut out, map, "model");
                    insert_field(&mut out, map, "thinking");
                    insert_field(&mut out, map, "timeoutSeconds");
                    insert_field(&mut out, map, "allowUnsafeExternalContent");
                    insert_field(&mut out, map, "deliver");
                    insert_field(&mut out, map, "channel");
                    insert_field(&mut out, map, "to");
                    insert_field(&mut out, map, "bestEffortDeliver");
                    insert_field(&mut out, map, "policy");
                }
                _ => {}
            }
        }
        return Value::Object(out);
    }
    value.clone()
}

fn sanitize_cron_isolation(value: &Value) -> Value {
    sanitize_object_fields_ref(value, &["postToMainPrefix", "postToMainMode", "postToMainMaxChars"])
}

fn sanitize_cron_state(value: &Value) -> Value {
    sanitize_object_fields_ref(
        value,
        &[
            "nextRunAtMs",
            "runningAtMs",
            "lastRunAtMs",
            "lastStatus",
            "lastError",
            "lastDurationMs",
        ],
    )
}

fn sanitize_cron_delivery(value: &Value) -> Value {
    sanitize_object_fields_ref(value, &["mode", "channel", "to", "bestEffort"])
}

fn sanitize_memory_search_response(value: Value) -> Value {
    let mut out = Map::new();
    if let Value::Object(map) = value {
        if let Some(Value::Array(results)) = map.get("results") {
            let sanitized = results
                .iter()
                .map(|entry| {
                    sanitize_object_fields_ref(
                        entry,
                        &[
                            "path",
                            "startLine",
                            "endLine",
                            "lineStart",
                            "lineEnd",
                            "score",
                            "snippet",
                            "source",
                            "citation",
                        ],
                    )
                })
                .collect::<Vec<_>>();
            out.insert("results".to_string(), Value::Array(sanitized));
        }
        for key in ["provider", "model", "fallback", "citations", "disabled", "error"] {
            if let Some(value) = map.get(key) {
                out.insert(key.to_string(), value.clone());
            }
        }
    }
    Value::Object(out)
}

fn sanitize_channels_list_response(value: Value) -> Value {
    let mut out = Map::new();
    if let Value::Object(map) = value {
        if let Some(Value::Array(channels)) = map.get("channels") {
            let sanitized = channels
                .iter()
                .map(|entry| {
                    sanitize_object_fields_ref(
                        entry,
                        &["channel", "to", "accountId", "sessionKey", "updatedAtMs"],
                    )
                })
                .collect::<Vec<_>>();
            out.insert("channels".to_string(), Value::Array(sanitized));
        }
        insert_field(&mut out, &map, "disabled");
        insert_field(&mut out, &map, "count");
        insert_field(&mut out, &map, "routeTtlMs");
    }
    Value::Object(out)
}

fn sanitize_object_fields(value: Value, keys: &[&str]) -> Value {
    if let Value::Object(map) = value {
        Value::Object(sanitize_object_fields_map(&map, keys))
    } else {
        value
    }
}

fn sanitize_object_fields_ref(value: &Value, keys: &[&str]) -> Value {
    if let Value::Object(map) = value {
        Value::Object(sanitize_object_fields_map(map, keys))
    } else {
        value.clone()
    }
}

fn sanitize_object_fields_map(map: &Map<String, Value>, keys: &[&str]) -> Map<String, Value> {
    let mut out = Map::new();
    for key in keys {
        if let Some(value) = map.get(*key) {
            out.insert((*key).to_string(), value.clone());
        }
    }
    out
}

fn insert_field(target: &mut Map<String, Value>, source: &Map<String, Value>, key: &str) {
    if let Some(value) = source.get(key) {
        target.insert(key.to_string(), value.clone());
    }
}

fn insert_non_null_field(target: &mut Map<String, Value>, source: &Map<String, Value>, key: &str) {
    if let Some(value) = source.get(key) {
        if !value.is_null() {
            target.insert(key.to_string(), value.clone());
        }
    }
}

fn validate_tool_arguments(name: &str, arguments: &Value) -> std::result::Result<(), JsonRpcError> {
    let schema = match name {
        "cron.add" => Some(CRON_ADD_REQUEST_SCHEMA),
        "cron.update" => Some(CRON_UPDATE_REQUEST_SCHEMA),
        "cron.list" => Some(CRON_LIST_REQUEST_SCHEMA),
        "cron.remove" => Some(CRON_REMOVE_REQUEST_SCHEMA),
        "cron.run" => Some(CRON_RUN_REQUEST_SCHEMA),
        "cron.runs" => Some(CRON_RUNS_REQUEST_SCHEMA),
        "cron.status" => Some(CRON_STATUS_REQUEST_SCHEMA),
        "memory_search" => Some(MEMORY_SEARCH_REQUEST_SCHEMA),
        "memory_get" => Some(MEMORY_GET_REQUEST_SCHEMA),
        "message.send" => Some(MESSAGE_SEND_REQUEST_SCHEMA),
        "channels.list" => Some(CHANNELS_LIST_REQUEST_SCHEMA),
        "channels.resolve_target" => Some(CHANNELS_RESOLVE_REQUEST_SCHEMA),
        "heartbeat.wake" => Some(HEARTBEAT_WAKE_REQUEST_SCHEMA),
        "artifact.create_xlsx" => Some(ARTIFACT_CREATE_XLSX_REQUEST_SCHEMA),
        "artifact.create_pptx" => Some(ARTIFACT_CREATE_PPTX_REQUEST_SCHEMA),
        "artifact.create_docx" => Some(ARTIFACT_CREATE_DOCX_REQUEST_SCHEMA),
        "artifact.create_pdf" => Some(ARTIFACT_CREATE_PDF_REQUEST_SCHEMA),
        _ => None,
    };
    let Some(schema) = schema else {
        return Ok(());
    };
    let normalized = normalize_args_for_validation(name, arguments);
    let schema_value = schema_value(schema);
    let compiled = JSONSchema::options()
        .with_draft(Draft::Draft7)
        .compile(&schema_value)
        .map_err(|err| JsonRpcError::internal(format!("schema compile failed: {err}")))?;
    if let Err(mut errors) = compiled.validate(&normalized) {
        let message = errors
            .next()
            .map(|err| err.to_string())
            .unwrap_or_else(|| "invalid params".to_string());
        return Err(JsonRpcError::invalid_params(message));
    }
    Ok(())
}

fn validate_tool_response(name: &str, result: &Value) -> std::result::Result<(), JsonRpcError> {
    let schema = match name {
        "cron.add" => Some(CRON_ADD_RESPONSE_SCHEMA),
        "cron.update" => Some(CRON_UPDATE_RESPONSE_SCHEMA),
        "cron.list" => Some(CRON_LIST_RESPONSE_SCHEMA),
        "cron.remove" => Some(CRON_REMOVE_RESPONSE_SCHEMA),
        "cron.run" => Some(CRON_RUN_RESPONSE_SCHEMA),
        "cron.runs" => Some(CRON_RUNS_RESPONSE_SCHEMA),
        "cron.status" => Some(CRON_STATUS_RESPONSE_SCHEMA),
        "memory_search" => Some(MEMORY_SEARCH_RESPONSE_SCHEMA),
        "memory_get" => Some(MEMORY_GET_RESPONSE_SCHEMA),
        "message.send" => Some(MESSAGE_SEND_RESPONSE_SCHEMA),
        "channels.list" => Some(CHANNELS_LIST_RESPONSE_SCHEMA),
        "channels.resolve_target" => Some(CHANNELS_RESOLVE_RESPONSE_SCHEMA),
        "heartbeat.wake" => Some(HEARTBEAT_WAKE_RESPONSE_SCHEMA),
        "artifact.create_xlsx" => Some(ARTIFACT_CREATE_XLSX_RESPONSE_SCHEMA),
        "artifact.create_pptx" => Some(ARTIFACT_CREATE_PPTX_RESPONSE_SCHEMA),
        "artifact.create_docx" => Some(ARTIFACT_CREATE_DOCX_RESPONSE_SCHEMA),
        "artifact.create_pdf" => Some(ARTIFACT_CREATE_PDF_RESPONSE_SCHEMA),
        _ => None,
    };
    let Some(schema) = schema else {
        return Ok(());
    };
    let schema_value = schema_value(schema);
    let compiled = JSONSchema::options()
        .with_draft(Draft::Draft7)
        .compile(&schema_value)
        .map_err(|err| JsonRpcError::internal(format!("response schema compile failed: {err}")))?;
    if let Err(mut errors) = compiled.validate(result) {
        let message = errors
            .next()
            .map(|err| err.to_string())
            .unwrap_or_else(|| "invalid response".to_string());
        return Err(JsonRpcError::internal(format!(
            "tool response invalid for {name}: {message}"
        )));
    }
    Ok(())
}

fn normalize_args_for_validation(name: &str, arguments: &Value) -> Value {
    let mut value = unwrap_job_wrapper(arguments).unwrap_or(arguments.clone());
    let Some(map) = value.as_object_mut() else {
        return value;
    };

    match name {
        "cron.add" => {
            normalize_cron_job_for_validation(map, true);
        }
        "cron.update" => {
            normalize_cron_job_for_validation(map, false);
        }
        "memory_search" => {
            normalize_aliases(
                map,
                &[
                    ("max_results", "maxResults"),
                    ("min_score", "minScore"),
                    ("session_key", "sessionKey"),
                ],
            );
        }
        "message.send" => {
            normalize_aliases(
                map,
                &[
                    ("best_effort", "bestEffort"),
                    ("dry_run", "dryRun"),
                    ("account_id", "accountId"),
                    ("session_key", "sessionKey"),
                    ("idempotency_key", "idempotencyKey"),
                ],
            );
        }
        "channels.resolve_target" => {
            normalize_aliases(map, &[("account_id", "accountId")]);
        }
        "artifact.create_xlsx"
        | "artifact.create_pptx"
        | "artifact.create_docx"
        | "artifact.create_pdf" => {
            normalize_aliases(
                map,
                &[("output_path", "outputPath"), ("task_run_id", "taskRunId")],
            );
        }
        _ => {}
    }

    value
}

fn unwrap_job_wrapper(arguments: &Value) -> Option<Value> {
    let Some(map) = arguments.as_object() else {
        return None;
    };
    if let Some(value) = map.get("data").and_then(|v| v.as_object()) {
        return Some(Value::Object(value.clone()));
    }
    if let Some(value) = map.get("job").and_then(|v| v.as_object()) {
        return Some(Value::Object(value.clone()));
    }
    None
}

fn normalize_aliases(map: &mut Map<String, Value>, pairs: &[(&str, &str)]) {
    for (from, to) in pairs {
        if map.contains_key(*to) {
            continue;
        }
        if let Some(value) = map.remove(*from) {
            map.insert((*to).to_string(), value);
        }
    }
}

fn normalize_cron_job_for_validation(map: &mut Map<String, Value>, apply_defaults: bool) {
    if let Some(Value::Object(schedule)) = map.get_mut("schedule") {
        normalize_cron_schedule_for_validation(schedule);
    }
    let has_session_target = map.contains_key("sessionTarget");
    if let Some(Value::Object(payload)) = map.get_mut("payload") {
        normalize_cron_payload_for_validation(payload);
        if apply_defaults && !has_session_target {
            if let Some(kind) = payload.get("kind").and_then(|v| v.as_str()) {
                let target = match kind {
                    "systemEvent" => Some("main"),
                    "agentTurn" => Some("isolated"),
                    _ => None,
                };
                if let Some(target) = target {
                    map.insert("sessionTarget".to_string(), Value::String(target.to_string()));
                }
            }
        }
    }

    if apply_defaults {
        map.entry("enabled".to_string())
            .or_insert_with(|| Value::Bool(true));
        map.entry("wakeMode".to_string())
            .or_insert_with(|| Value::String("next-heartbeat".to_string()));
    }

    if let Some(Value::Object(patch)) = map.get_mut("patch") {
        if let Some(Value::Object(schedule)) = patch.get_mut("schedule") {
            normalize_cron_schedule_for_validation(schedule);
        }
        if let Some(Value::Object(payload)) = patch.get_mut("payload") {
            normalize_cron_payload_for_validation(payload);
        }
    }
}

fn normalize_cron_payload_for_validation(payload: &mut Map<String, Value>) {
    if !payload.contains_key("channel") {
        if let Some(Value::String(provider)) = payload.get("provider") {
            if !provider.trim().is_empty() {
                payload.insert("channel".to_string(), Value::String(provider.clone()));
            }
        }
    }
}

fn normalize_cron_schedule_for_validation(schedule: &mut Map<String, Value>) {
    normalize_aliases(
        schedule,
        &[
            ("every_ms", "everyMs"),
            ("anchor_ms", "anchorMs"),
            ("cron", "expr"),
            ("timezone", "tz"),
            ("timeZone", "tz"),
        ],
    );

    if schedule.get("kind").and_then(|v| v.as_str()).is_none() {
        let kind = if schedule.contains_key("atMs") || schedule.contains_key("at") {
            Some("at")
        } else if schedule.contains_key("everyMs") {
            Some("every")
        } else if schedule.contains_key("expr") {
            Some("cron")
        } else {
            None
        };
        if let Some(kind) = kind {
            schedule.insert("kind".to_string(), Value::String(kind.to_string()));
        }
    }

    if !schedule.contains_key("atMs") {
        if let Some(at) = schedule.get("at") {
            if let Some(ms) = parse_at_ms_value(at) {
                schedule.insert("atMs".to_string(), Value::Number(ms.into()));
            }
        }
    } else if let Some(Value::String(raw)) = schedule.get("atMs").cloned() {
        if let Some(ms) = parse_at_ms_value(&Value::String(raw)) {
            schedule.insert("atMs".to_string(), Value::Number(ms.into()));
        }
    }
}

fn parse_at_ms_value(value: &Value) -> Option<i64> {
    match value {
        Value::Number(num) => num.as_i64(),
        Value::String(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                return None;
            }
            if let Ok(ms) = trimmed.parse::<i64>() {
                return Some(ms);
            }
            chrono::DateTime::parse_from_rfc3339(trimmed)
                .ok()
                .map(|dt| dt.timestamp_millis())
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{json, Value};

    #[test]
    fn validates_memory_search_aliases() {
        let args = json!({ "query": "hi", "max_results": 5 });
        assert!(validate_tool_arguments("memory_search", &args).is_ok());
    }

    #[test]
    fn validates_cron_add_wrapped_defaults() {
        let args = json!({
            "job": {
                "name": "job",
                "schedule": { "at": "2026-02-04T12:00:00Z" },
                "payload": { "kind": "agentTurn", "message": "hi" }
            }
        });
        assert!(validate_tool_arguments("cron.add", &args).is_ok());
    }

    #[test]
    fn validates_message_send_requires_channel_and_to() {
        let args = json!({ "text": "hi" });
        assert!(validate_tool_arguments("message.send", &args).is_err());
    }

    #[test]
    fn validates_artifact_create_xlsx() {
        let args = json!({
            "outputPath": "reports/demo.xlsx",
            "sheets": [
                { "name": "Sheet1", "cells": [] }
            ]
        });
        assert!(validate_tool_arguments("artifact.create_xlsx", &args).is_ok());
    }

    fn sample_cron_job() -> Value {
        json!({
            "id": "job-1",
            "name": "job",
            "enabled": true,
            "schedule": { "kind": "at", "atMs": 123 },
            "sessionTarget": "main",
            "wakeMode": "next-heartbeat",
            "payload": { "kind": "systemEvent", "text": "hi" },
            "state": { "nextRunAtMs": 123 }
        })
    }

    fn assert_response_ok(name: &str, value: Value) {
        let sanitized = sanitize_tool_response(name, value);
        if let Err(err) = validate_tool_response(name, &sanitized) {
            panic!("response validation failed for {name}: {}", err.message);
        }
    }

    fn tool_response_schemas() -> Vec<(&'static str, &'static str)> {
        vec![
            ("cron.add", CRON_ADD_RESPONSE_SCHEMA),
            ("cron.update", CRON_UPDATE_RESPONSE_SCHEMA),
            ("cron.list", CRON_LIST_RESPONSE_SCHEMA),
            ("cron.remove", CRON_REMOVE_RESPONSE_SCHEMA),
            ("cron.run", CRON_RUN_RESPONSE_SCHEMA),
            ("cron.runs", CRON_RUNS_RESPONSE_SCHEMA),
            ("cron.status", CRON_STATUS_RESPONSE_SCHEMA),
            ("memory_search", MEMORY_SEARCH_RESPONSE_SCHEMA),
            ("memory_get", MEMORY_GET_RESPONSE_SCHEMA),
            ("message.send", MESSAGE_SEND_RESPONSE_SCHEMA),
            ("channels.list", CHANNELS_LIST_RESPONSE_SCHEMA),
            ("channels.resolve_target", CHANNELS_RESOLVE_RESPONSE_SCHEMA),
            ("heartbeat.wake", HEARTBEAT_WAKE_RESPONSE_SCHEMA),
            ("artifact.create_xlsx", ARTIFACT_CREATE_XLSX_RESPONSE_SCHEMA),
            ("artifact.create_pptx", ARTIFACT_CREATE_PPTX_RESPONSE_SCHEMA),
            ("artifact.create_docx", ARTIFACT_CREATE_DOCX_RESPONSE_SCHEMA),
            ("artifact.create_pdf", ARTIFACT_CREATE_PDF_RESPONSE_SCHEMA),
        ]
    }

    fn schema_properties(schema: &str) -> Map<String, Value> {
        let parsed: Value = serde_json::from_str(schema).expect("schema json");
        parsed
            .get("properties")
            .and_then(|v| v.as_object())
            .cloned()
            .unwrap_or_default()
    }

    fn placeholder_for_schema(schema: &Value) -> Value {
        if let Some(const_value) = schema.get("const") {
            return const_value.clone();
        }
        if let Some(enum_values) = schema.get("enum").and_then(|v| v.as_array()) {
            if let Some(first) = enum_values.first() {
                return first.clone();
            }
        }
        if let Some(any_of) = schema.get("anyOf").and_then(|v| v.as_array()) {
            if let Some(first) = any_of.first() {
                return placeholder_for_schema(first);
            }
        }
        if let Some(one_of) = schema.get("oneOf").and_then(|v| v.as_array()) {
            if let Some(first) = one_of.first() {
                return placeholder_for_schema(first);
            }
        }
        if let Some(all_of) = schema.get("allOf").and_then(|v| v.as_array()) {
            if let Some(first) = all_of.first() {
                return placeholder_for_schema(first);
            }
        }
        if let Some(type_value) = schema.get("type") {
            if let Some(type_str) = type_value.as_str() {
                return placeholder_for_type(type_str);
            }
            if let Some(types) = type_value.as_array() {
                if let Some(Value::String(type_str)) = types.first() {
                    return placeholder_for_type(type_str);
                }
            }
        }
        Value::String("x".to_string())
    }

    fn placeholder_for_type(type_str: &str) -> Value {
        match type_str {
            "object" => Value::Object(Map::new()),
            "array" => Value::Array(Vec::new()),
            "boolean" => Value::Bool(true),
            "integer" | "number" => Value::Number(1.into()),
            "string" => Value::String("x".to_string()),
            "null" => Value::Null,
            _ => Value::String("x".to_string()),
        }
    }

    #[test]
    fn validates_sample_tool_responses() {
        assert_response_ok("cron.list", json!({ "jobs": [sample_cron_job()] }));
        assert_response_ok(
            "cron.status",
            json!({ "enabled": true, "storePath": "/tmp/jobs.json", "jobs": 1, "nextWakeAtMs": 123 }),
        );
        assert_response_ok("cron.add", sample_cron_job());
        assert_response_ok("cron.update", sample_cron_job());
        assert_response_ok("cron.remove", json!({ "ok": true, "removed": true }));
        assert_response_ok("cron.run", json!({ "ok": true, "ran": true }));
        assert_response_ok(
            "cron.runs",
            json!({
                "entries": [
                    { "ts": 1, "jobId": "job-1", "action": "finished", "status": "ok" }
                ]
            }),
        );
        assert_response_ok(
            "memory_search",
            json!({
                "results": [
                    {
                        "path": "memory/2026-02-04.md",
                        "startLine": 1,
                        "endLine": 2,
                        "lineStart": 1,
                        "lineEnd": 2,
                        "score": 0.5,
                        "snippet": "hi",
                        "source": "memory"
                    }
                ],
                "citations": "auto"
            }),
        );
        assert_response_ok(
            "memory_get",
            json!({ "path": "MEMORY.md", "text": "hi", "content": "hi", "from": 1, "lines": 1 }),
        );
        assert_response_ok("message.send", json!({ "ok": true, "result": { "id": "msg" } }));
        assert_response_ok(
            "channels.list",
            json!({
                "channels": [
                    {
                        "channel": "slack",
                        "to": "U1",
                        "accountId": null,
                        "sessionKey": "slack:U1",
                        "updatedAtMs": 1
                    }
                ],
                "disabled": false,
                "count": 1,
                "routeTtlMs": 1000
            }),
        );
        assert_response_ok(
            "channels.resolve_target",
            json!({
                "ok": true,
                "channel": "slack",
                "to": "U1",
                "accountId": null,
                "sessionKey": "slack:U1",
                "updatedAtMs": 1
            }),
        );
        assert_response_ok("heartbeat.wake", json!({ "ok": true }));
        assert_response_ok(
            "artifact.create_xlsx",
            json!({
                "ok": true,
                "path": "reports/demo.xlsx",
                "absolutePath": "/tmp/reports/demo.xlsx",
                "mime": "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
                "sha256": "abc",
                "sizeBytes": 123,
                "recorded": false,
                "taskRunId": "run-1"
            }),
        );
    }

    #[test]
    fn sanitizes_message_send_extras() {
        let sanitized = sanitize_tool_response(
            "message.send",
            json!({ "ok": true, "deduped": true, "extra": "nope" }),
        );
        let Value::Object(map) = sanitized else {
            panic!("expected object");
        };
        assert!(map.get("deduped").is_none());
        assert!(map.get("extra").is_none());
    }

    #[test]
    fn sanitizer_tracks_schema_keys() {
        for (tool, schema) in tool_response_schemas() {
            let properties = schema_properties(schema);
            assert!(
                !properties.is_empty(),
                "schema for {tool} does not define properties"
            );
            let mut input = Map::new();
            for (key, prop_schema) in &properties {
                input.insert(key.clone(), placeholder_for_schema(prop_schema));
            }
            input.insert("extra".to_string(), Value::String("nope".to_string()));

            let sanitized = sanitize_tool_response(tool, Value::Object(input));
            let Value::Object(map) = sanitized else {
                panic!("sanitized response for {tool} is not an object");
            };

            for key in properties.keys() {
                assert!(
                    map.contains_key(key),
                    "sanitize dropped schema key {key} for {tool}"
                );
            }
            assert!(
                !map.contains_key("extra"),
                "sanitize kept extra key for {tool}"
            );
        }
    }
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
