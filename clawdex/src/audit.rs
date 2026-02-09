use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::config::ClawdPaths;
use crate::task_db::TaskEvent;
use crate::util::{append_json_line, now_ms, read_json_lines};

const AUDIT_DIR: &str = "audit";

#[derive(Debug, Clone)]
struct RiskAssessment {
    level: &'static str,
    score: f32,
    reasons: Vec<String>,
    checkpoint: Option<&'static str>,
}

pub fn audit_dir(paths: &ClawdPaths) -> PathBuf {
    paths.state_dir.join(AUDIT_DIR)
}

pub fn append_event(audit_dir: &Path, event: &TaskEvent) -> Result<()> {
    let payload = json!({
        "eventId": event.id,
        "eventKind": event.kind,
        "payload": event.payload,
    });
    let intent = action_intent_for_event(&event.kind, &payload);
    append_entry(audit_dir, &event.task_run_id, "event", &payload, intent)
}

pub fn append_approval(
    audit_dir: &Path,
    run_id: &str,
    kind: &str,
    request: &Value,
    decision: Option<&str>,
) -> Result<()> {
    let payload = json!({
        "kind": kind,
        "request": request,
        "decision": decision,
    });
    let intent = action_intent_for_approval(kind, request);
    append_entry(audit_dir, run_id, "approval", &payload, intent)
}

pub fn append_artifact(
    audit_dir: &Path,
    run_id: &str,
    path: &str,
    mime: Option<&str>,
    sha256: Option<&str>,
) -> Result<()> {
    let payload = json!({
        "path": path,
        "mime": mime,
        "sha256": sha256,
    });
    append_entry(audit_dir, run_id, "artifact", &payload, None)
}

pub fn append_tool_call(
    audit_dir: &Path,
    run_id: &str,
    tool: &str,
    arguments: &Value,
) -> Result<()> {
    let payload = json!({
        "tool": tool,
        "arguments": arguments,
    });
    let intent = action_intent_for_tool_call(tool, arguments);
    append_entry(audit_dir, run_id, "tool_call", &payload, Some(intent))
}

pub fn read_audit_log(audit_dir: &Path, run_id: &str, limit: Option<usize>) -> Result<Vec<Value>> {
    let path = audit_log_path(audit_dir, run_id);
    read_json_lines(&path, limit)
}

pub fn resolve_run_id_from_args(args: &Value) -> Option<String> {
    args.get("taskRunId")
        .or_else(|| args.get("task_run_id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.trim().is_empty())
}

pub fn resolve_run_id_from_env() -> Option<String> {
    std::env::var("CLAWDEX_TASK_RUN_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
}

fn append_entry(
    audit_dir: &Path,
    run_id: &str,
    kind: &str,
    payload: &Value,
    action_intent: Option<Value>,
) -> Result<()> {
    std::fs::create_dir_all(audit_dir)
        .with_context(|| format!("create {}", audit_dir.display()))?;
    let path = audit_log_path(audit_dir, run_id);
    let prev_hash = read_last_hash(&path)?;

    let base = json!({
        "id": Uuid::new_v4().to_string(),
        "runId": run_id,
        "tsMs": now_ms(),
        "kind": kind,
        "payload": payload,
        "actionIntent": action_intent,
        "prevHash": prev_hash,
    });

    let hash = compute_hash(&base)?;
    let mut entry = base;
    entry["hash"] = Value::String(hash);

    append_json_line(&path, &entry)
}

fn audit_log_path(audit_dir: &Path, run_id: &str) -> PathBuf {
    audit_dir.join(format!("{run_id}.jsonl"))
}

fn read_last_hash(path: &Path) -> Result<Option<String>> {
    let lines = read_json_lines(path, Some(1))?;
    Ok(lines
        .last()
        .and_then(|val| val.get("hash"))
        .and_then(|val| val.as_str())
        .map(|val| val.to_string()))
}

fn compute_hash(value: &Value) -> Result<String> {
    let data = serde_json::to_string(value).context("serialize audit entry")?;
    let mut hasher = Sha256::new();
    hasher.update(data.as_bytes());
    let hash = hasher.finalize();
    Ok(hex::encode(hash))
}

fn action_intent_for_event(event_kind: &str, payload: &Value) -> Option<Value> {
    if event_kind == "mcp_tool_call_progress" {
        let message = payload
            .get("payload")
            .and_then(|p| p.get("message"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if !message.trim().is_empty() {
            let assessment = RiskAssessment {
                level: "low",
                score: 0.1,
                reasons: vec!["tool call progress".to_string()],
                checkpoint: None,
            };
            return Some(build_action_intent(
                "tool_call",
                format!("Tool call progress: {}", message.trim()),
                vec![message.trim().to_string()],
                assessment,
            ));
        }
    }
    None
}

fn action_intent_for_tool_call(tool: &str, _args: &Value) -> Value {
    let assessment = risk_for_tool(tool);
    build_action_intent(
        "tool_call",
        format!("Tool call: {tool}"),
        vec![tool.to_string()],
        assessment,
    )
}

fn action_intent_for_approval(kind: &str, request: &Value) -> Option<Value> {
    match kind {
        "command" => {
            let command = request.get("command").and_then(|v| v.as_str()).unwrap_or("");
            let assessment = risk_for_command(command);
            Some(build_action_intent(
                "command",
                format!("Run command: {}", command.trim()),
                vec![command.trim().to_string()],
                assessment,
            ))
        }
        "file_change" => {
            let diff = request.get("diff").and_then(|v| v.as_str()).unwrap_or("");
            let mut targets = Vec::new();
            if let Some(paths) = request.get("paths").and_then(|v| v.as_array()) {
                for path in paths.iter().filter_map(|v| v.as_str()) {
                    targets.push(path.to_string());
                }
            }
            if targets.is_empty() {
                targets.push("workspace".to_string());
            }
            let assessment = risk_for_file_change(diff, &targets);
            Some(build_action_intent(
                "file_change",
                "File change approval".to_string(),
                targets,
                assessment,
            ))
        }
        _ => None,
    }
}

fn build_action_intent(
    kind: &str,
    summary: String,
    targets: Vec<String>,
    assessment: RiskAssessment,
) -> Value {
    json!({
        "id": Uuid::new_v4().to_string(),
        "kind": kind,
        "summary": summary,
        "targets": targets,
        "risk": {
            "level": assessment.level,
            "score": assessment.score,
            "reasons": assessment.reasons,
        },
        "checkpoint": assessment.checkpoint,
    })
}

fn risk_for_tool(tool: &str) -> RiskAssessment {
    let lower = tool.to_lowercase();
    if lower == "message.send" {
        return RiskAssessment {
            level: "medium",
            score: 0.5,
            reasons: vec!["external messaging".to_string()],
            checkpoint: Some("explicit_approval"),
        };
    }
    RiskAssessment {
        level: "low",
        score: 0.2,
        reasons: vec!["tool call".to_string()],
        checkpoint: None,
    }
}

fn risk_for_command(command: &str) -> RiskAssessment {
    let cmd = command.to_lowercase();
    let mut score = 0.2f32;
    let mut reasons = Vec::new();

    let high_risk = ["rm -rf", "rm -r", "rm ", "sudo", "chmod", "chown", "kill", "dd "];
    if high_risk.iter().any(|token| cmd.contains(token)) {
        score = 0.9;
        reasons.push("destructive command".to_string());
    }

    let medium_risk = ["curl ", "wget ", "scp ", "ssh ", "git push", "npm publish", "pip install", "brew install"];
    if medium_risk.iter().any(|token| cmd.contains(token)) {
        score = score.max(0.6);
        reasons.push("external network or install".to_string());
    }

    let level = if score >= 0.8 {
        "high"
    } else if score >= 0.4 {
        "medium"
    } else {
        "low"
    };
    let checkpoint = if level == "low" { None } else { Some("explicit_approval") };

    RiskAssessment {
        level,
        score,
        reasons,
        checkpoint,
    }
}

fn risk_for_file_change(diff: &str, targets: &[String]) -> RiskAssessment {
    let mut score = 0.4f32;
    let mut reasons = Vec::new();

    let delete_line = diff
        .lines()
        .any(|line| line.starts_with('-') && !line.starts_with("---"));
    if delete_line {
        score = 0.8;
        reasons.push("deletions detected".to_string());
    }

    if targets.iter().any(|path| path.contains(".env") || path.contains("secrets")) {
        score = score.max(0.7);
        reasons.push("sensitive paths".to_string());
    }

    let level = if score >= 0.8 {
        "high"
    } else if score >= 0.4 {
        "medium"
    } else {
        "low"
    };
    let checkpoint = if level == "low" { None } else { Some("explicit_approval") };

    RiskAssessment {
        level,
        score,
        reasons,
        checkpoint,
    }
}
