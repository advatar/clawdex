use std::io::{self, BufRead, Write};
use std::path::PathBuf;

use anyhow::{Context, Result};
use codex_app_server_protocol::AskForApproval;
use serde_json::{json, Value};

use crate::app_server::{ApprovalMode, CodexClient};
use crate::config::{load_config, merge_config_value, read_config_value, write_config_value, ClawdConfig};
use crate::plugins;
use crate::runner::workspace_sandbox_policy;
use crate::task_db::TaskStore;

pub fn run_ui_bridge(
    codex_path: PathBuf,
    state_dir: PathBuf,
    workspace: Option<PathBuf>,
) -> Result<()> {
    let approval_mode = ApprovalMode::from_env();
    let approval_policy = approval_policy_from_env();
    let (_cfg, mut paths) = load_config(Some(state_dir.clone()), workspace.clone())?;
    let mut sandbox_policy = workspace_sandbox_policy(&paths.workspace_policy)?;

    let codex_home = state_dir.join("codex");
    std::fs::create_dir_all(&codex_home)
        .with_context(|| format!("create {}", codex_home.display()))?;

    let mut env = Vec::new();
    env.push((
        "CODEX_HOME".to_string(),
        codex_home.to_string_lossy().to_string(),
    ));
    env.push((
        "CLAWDEX_WORKSPACE".to_string(),
        paths.workspace_dir.to_string_lossy().to_string(),
    ));
    env.push((
        "CODEX_WORKSPACE_DIR".to_string(),
        paths.workspace_dir.to_string_lossy().to_string(),
    ));

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
                    sandbox_policy.clone(),
                    Some(paths.workspace_dir.clone()),
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
            Some("plugin_command") => {
                let plugin_id = payload.get("pluginId").and_then(|v| v.as_str()).unwrap_or("");
                let command = payload.get("command").and_then(|v| v.as_str()).unwrap_or("");
                let input = payload.get("input").and_then(|v| v.as_str());
                if plugin_id.is_empty() || command.is_empty() {
                    emit_error(&mut stdout, "plugin_command requires pluginId and command")?;
                    continue;
                }
                let store = TaskStore::open(&paths)?;
                let plugin = match store.get_plugin(plugin_id)? {
                    Some(p) => p,
                    None => {
                        emit_error(&mut stdout, "plugin not found")?;
                        continue;
                    }
                };
                if !plugin.enabled {
                    emit_error(&mut stdout, "plugin is disabled")?;
                    continue;
                }
                let prompt = match plugins::resolve_plugin_command_prompt(&paths, &plugin, command, input) {
                    Ok(prompt) => prompt,
                    Err(err) => {
                        emit_error(&mut stdout, &err.to_string())?;
                        continue;
                    }
                };
                match client.run_turn(
                    &thread_id,
                    &prompt,
                    approval_policy,
                    sandbox_policy.clone(),
                    Some(paths.workspace_dir.clone()),
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
            Some("list_plugin_commands") => {
                let plugin_id = payload.get("pluginId").and_then(|v| v.as_str()).map(|s| s.to_string());
                let result =
                    plugins::list_plugin_commands_command(plugin_id, Some(paths.state_dir.clone()), Some(paths.workspace_dir.clone()))?;
                emit_json(&mut stdout, json!({ "type": "plugin_commands", "commands": result.get("commands") }))?;
            }
            Some("list_plugins") => {
                let include_disabled = payload
                    .get("includeDisabled")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(true);
                let result = plugins::list_plugins_command(
                    Some(paths.state_dir.clone()),
                    Some(paths.workspace_dir.clone()),
                    include_disabled,
                )?;
                emit_json(&mut stdout, json!({ "type": "plugins_list", "plugins": result.get("plugins") }))?;
            }
            Some("get_config") => {
                let value = read_config_value(&paths.state_dir)?;
                emit_json(&mut stdout, json!({ "type": "config", "config": value }))?;
            }
            Some("update_config") => {
                let Some(patch) = payload.get("config") else {
                    emit_error(&mut stdout, "update_config missing config")?;
                    continue;
                };
                let mut value = read_config_value(&paths.state_dir)?;
                merge_config_value(&mut value, patch);
                let _ = serde_json::from_value::<ClawdConfig>(value.clone())
                    .map_err(|err| anyhow::anyhow!("invalid config update: {err}"))?;
                write_config_value(&paths.state_dir, &value)?;
                let (_cfg, new_paths) = load_config(Some(state_dir.clone()), workspace.clone())?;
                paths = new_paths;
                sandbox_policy = workspace_sandbox_policy(&paths.workspace_policy)?;
                emit_json(&mut stdout, json!({ "type": "config_updated", "config": value }))?;
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
