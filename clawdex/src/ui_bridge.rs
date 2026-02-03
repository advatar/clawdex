use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use codex_app_server_protocol::AskForApproval;
use serde_json::{json, Value};

use crate::app_server::{ApprovalMode, CodexClient};

pub fn run_ui_bridge(
    codex_path: PathBuf,
    state_dir: PathBuf,
    workspace: Option<PathBuf>,
) -> Result<()> {
    let approval_mode = ApprovalMode::from_env();
    let approval_policy = approval_policy_from_env();

    let codex_home = state_dir.join("codex");
    std::fs::create_dir_all(&codex_home)
        .with_context(|| format!("create {}", codex_home.display()))?;

    let mut env = Vec::new();
    env.push((
        "CODEX_HOME".to_string(),
        codex_home.to_string_lossy().to_string(),
    ));
    if let Some(ref workspace) = workspace {
        env.push((
            "CLAWDEX_WORKSPACE".to_string(),
            workspace.to_string_lossy().to_string(),
        ));
    }

    let config_overrides = config_overrides_from_env();
    let mut client = CodexClient::spawn(&codex_path, &config_overrides, &env, approval_mode)?;
    client.initialize()?;
    let thread_id = client.thread_start()?;

    let stdin = io::stdin();
    let mut stdout = io::stdout();
    for line in stdin.lock().lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let payload: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(err) => {
                emit_error(&mut stdout, &format!("Invalid JSON: {err}"))?;
                continue;
            }
        };
        match payload.get("type").and_then(|v| v.as_str()) {
            Some("user_message") => {
                let text = payload
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if text.trim().is_empty() {
                    continue;
                }
                match client.run_turn(
                    &thread_id,
                    text,
                    approval_policy,
                    workspace.as_ref().map(|p| p.to_path_buf()),
                ) {
                    Ok(outcome) => {
                        if !outcome.message.is_empty() {
                            emit_message(&mut stdout, &outcome.message)?;
                        }
                        for warning in outcome.warnings {
                            emit_error(&mut stdout, &warning)?;
                        }
                    }
                    Err(err) => {
                        emit_error(&mut stdout, &err.to_string())?;
                    }
                }
            }
            Some("ping") => {
                emit_json(&mut stdout, json!({ "type": "pong" }))?;
            }
            _ => {
                emit_error(&mut stdout, "Unknown message type")?;
            }
        }
    }

    Ok(())
}

fn approval_policy_from_env() -> Option<AskForApproval> {
    let raw = std::env::var("CLAWDEX_APPROVAL_POLICY").ok()?;
    match raw.to_lowercase().as_str() {
        "never" => Some(AskForApproval::Never),
        "on-request" | "onrequest" => Some(AskForApproval::OnRequest),
        "on-failure" | "onfailure" => Some(AskForApproval::OnFailure),
        "unless-trusted" | "unlesstrusted" => Some(AskForApproval::UnlessTrusted),
        _ => None,
    }
}

fn config_overrides_from_env() -> Vec<String> {
    let mut overrides = Vec::new();
    if let Ok(raw) = std::env::var("CLAWDEX_CODEX_CONFIG") {
        for line in raw.split(';') {
            let trimmed = line.trim();
            if !trimmed.is_empty() {
                overrides.push(trimmed.to_string());
            }
        }
    }
    overrides
}

fn emit_message(stdout: &mut impl Write, text: &str) -> Result<()> {
    emit_json(stdout, json!({ "type": "assistant_message", "text": text }))
}

fn emit_error(stdout: &mut impl Write, message: &str) -> Result<()> {
    emit_json(stdout, json!({ "type": "error", "message": message }))
}

fn emit_json(stdout: &mut impl Write, value: Value) -> Result<()> {
    let line = serde_json::to_string(&value)?;
    writeln!(stdout, "{line}")?;
    stdout.flush().ok();
    Ok(())
}
