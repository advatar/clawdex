use std::path::PathBuf;

use anyhow::Result;
use serde_json::{json, Value};

use crate::config::ClawdPaths;
use crate::util::{append_json_line, now_ms};

fn heartbeat_log_path(paths: &ClawdPaths) -> PathBuf {
    paths.state_dir.join("heartbeat.jsonl")
}

fn heartbeat_payload(paths: &ClawdPaths, reason: &str) -> Result<Value> {
    let heartbeat_path = paths.workspace_dir.join("HEARTBEAT.md");
    if !heartbeat_path.exists() {
        return Ok(json!({
            "status": "skipped",
            "reason": "HEARTBEAT.md not found",
        }));
    }
    let contents = std::fs::read_to_string(&heartbeat_path).unwrap_or_default();
    if contents.trim().is_empty() {
        return Ok(json!({
            "status": "skipped",
            "reason": "HEARTBEAT.md empty",
            "message": "HEARTBEAT_OK",
        }));
    }

    Ok(json!({
        "status": "queued",
        "reason": reason,
        "message": "heartbeat execution not implemented",
    }))
}

pub fn wake(paths: &ClawdPaths, reason: Option<String>) -> Result<Value> {
    let reason = reason.unwrap_or_else(|| "manual".to_string());
    let now = now_ms();
    let payload = heartbeat_payload(paths, &reason)?;
    let entry = json!({
        "timestampMs": now,
        "reason": reason,
        "payload": payload,
    });
    append_json_line(&heartbeat_log_path(paths), &entry)?;
    Ok(entry)
}

// Daemon loop moved to daemon.rs
