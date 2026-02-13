use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::io::{self, BufRead, Write};
use std::path::PathBuf;
use std::rc::Rc;

use anyhow::{Context, Result};
use codex_app_server_protocol::{AskForApproval, UserInput as V2UserInput};
use serde_json::{json, Value};

use crate::app_server::{ApprovalMode, CodexClient, EventSink};
use crate::config::{
    load_config, merge_config_value, read_config_value, write_config_value, ClawdConfig,
};
use crate::plugins;
use crate::runner::workspace_sandbox_policy;
use crate::task_db::TaskStore;

#[derive(Default)]
struct UiEventSubscribers {
    entries: HashMap<String, UiEventSubscription>,
    next_id: u64,
}

#[derive(Clone)]
struct UiEventSubscription {
    kinds: Option<HashSet<String>>,
}

impl UiEventSubscribers {
    fn subscribe(&mut self, requested_id: Option<&str>, kinds: Option<Vec<String>>) -> String {
        let id = requested_id
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| value.to_string())
            .unwrap_or_else(|| {
                self.next_id = self.next_id.saturating_add(1);
                format!("sub-{}", self.next_id)
            });
        let normalized = normalize_kinds(kinds);
        self.entries
            .insert(id.clone(), UiEventSubscription { kinds: normalized });
        id
    }

    fn unsubscribe(&mut self, id: &str) -> bool {
        self.entries.remove(id).is_some()
    }

    fn len(&self) -> usize {
        self.entries.len()
    }

    fn kinds_for(&self, id: &str) -> Vec<String> {
        self.entries
            .get(id)
            .map(subscription_kinds)
            .unwrap_or_default()
    }

    fn list(&self) -> Vec<Value> {
        let mut out = Vec::with_capacity(self.entries.len());
        for (id, subscription) in &self.entries {
            out.push(json!({
                "subscriptionId": id,
                "kinds": subscription_kinds(subscription),
            }));
        }
        out.sort_by(|a, b| {
            a.get("subscriptionId")
                .and_then(|value| value.as_str())
                .unwrap_or("")
                .cmp(
                    b.get("subscriptionId")
                        .and_then(|value| value.as_str())
                        .unwrap_or(""),
                )
        });
        out
    }

    fn matching_subscribers(&self, kind: &str) -> Vec<String> {
        let normalized_kind = kind.trim().to_ascii_lowercase();
        let mut ids = Vec::new();
        for (id, subscription) in &self.entries {
            let matches = match subscription.kinds.as_ref() {
                None => true,
                Some(kinds) => kinds.contains(&normalized_kind),
            };
            if matches {
                ids.push(id.clone());
            }
        }
        ids.sort();
        ids
    }
}

struct UiBridgeEventSink {
    subscribers: Rc<RefCell<UiEventSubscribers>>,
    stdout: Rc<RefCell<io::Stdout>>,
}

impl UiBridgeEventSink {
    fn new(subscribers: Rc<RefCell<UiEventSubscribers>>, stdout: Rc<RefCell<io::Stdout>>) -> Self {
        Self {
            subscribers,
            stdout,
        }
    }
}

impl EventSink for UiBridgeEventSink {
    fn record_event(&mut self, kind: &str, payload: &Value) {
        let subscribers = self.subscribers.borrow().matching_subscribers(kind);
        if subscribers.is_empty() {
            return;
        }
        let mut stdout = self.stdout.borrow_mut();
        for subscription_id in subscribers {
            let _ = emit_json(
                &mut *stdout,
                json!({
                    "type": "ui_event",
                    "subscriptionId": subscription_id,
                    "eventKind": kind,
                    "event": payload,
                }),
            );
        }
    }
}

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
    let stdout = Rc::new(RefCell::new(io::stdout()));
    let subscribers = Rc::new(RefCell::new(UiEventSubscribers::default()));
    client.set_event_sink(Some(Box::new(UiBridgeEventSink::new(
        subscribers.clone(),
        stdout.clone(),
    ))));

    for line in stdin.lock().lines() {
        let line = line?;
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let payload: Value = match serde_json::from_str(trimmed) {
            Ok(value) => value,
            Err(err) => {
                emit_error(&mut *stdout.borrow_mut(), &format!("Invalid JSON: {err}"))?;
                continue;
            }
        };
        match payload.get("type").and_then(|v| v.as_str()) {
            Some("user_message") => {
                let text = payload.get("text").and_then(|v| v.as_str()).unwrap_or("");
                let mut input = Vec::new();
                if !text.trim().is_empty() {
                    input.push(V2UserInput::Text {
                        text: text.to_string(),
                        text_elements: Vec::new(),
                    });
                }
                if let Some(images) = payload.get("localImages").and_then(|v| v.as_array()) {
                    for entry in images {
                        let path = entry
                            .as_str()
                            .map(|s| s.trim())
                            .filter(|s| !s.is_empty())
                            .map(PathBuf::from);
                        if let Some(path) = path {
                            input.push(V2UserInput::LocalImage { path });
                        }
                    }
                }
                if input.is_empty() {
                    continue;
                }
                match client.run_turn_with_inputs(
                    &thread_id,
                    input,
                    approval_policy,
                    sandbox_policy.clone(),
                    Some(paths.workspace_dir.clone()),
                ) {
                    Ok(outcome) => {
                        if !outcome.message.is_empty() {
                            emit_message(&mut *stdout.borrow_mut(), &outcome.message)?;
                        }
                        for warning in outcome.warnings {
                            emit_error(&mut *stdout.borrow_mut(), &warning)?;
                        }
                    }
                    Err(err) => {
                        emit_error(&mut *stdout.borrow_mut(), &err.to_string())?;
                    }
                }
            }
            Some("plugin_command") => {
                let plugin_id = payload
                    .get("pluginId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let command = payload
                    .get("command")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let input = payload.get("input").and_then(|v| v.as_str());
                if plugin_id.is_empty() || command.is_empty() {
                    emit_error(
                        &mut *stdout.borrow_mut(),
                        "plugin_command requires pluginId and command",
                    )?;
                    continue;
                }
                let store = TaskStore::open(&paths)?;
                let plugin = match store.get_plugin(plugin_id)? {
                    Some(p) => p,
                    None => {
                        emit_error(&mut *stdout.borrow_mut(), "plugin not found")?;
                        continue;
                    }
                };
                if !plugin.enabled {
                    emit_error(&mut *stdout.borrow_mut(), "plugin is disabled")?;
                    continue;
                }
                let allow_preprocess = matches!(approval_mode, ApprovalMode::AutoApprove);
                let prompt = match plugins::resolve_plugin_command_prompt(
                    &paths,
                    &plugin,
                    command,
                    input,
                    allow_preprocess,
                ) {
                    Ok(prompt) => prompt,
                    Err(err) => {
                        emit_error(&mut *stdout.borrow_mut(), &err.to_string())?;
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
                            emit_message(&mut *stdout.borrow_mut(), &outcome.message)?;
                        }
                        for warning in outcome.warnings {
                            emit_error(&mut *stdout.borrow_mut(), &warning)?;
                        }
                    }
                    Err(err) => {
                        emit_error(&mut *stdout.borrow_mut(), &err.to_string())?;
                    }
                }
            }
            Some("list_plugin_commands") => {
                let plugin_id = payload
                    .get("pluginId")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                let result = plugins::list_plugin_commands_command(
                    plugin_id,
                    Some(paths.state_dir.clone()),
                    Some(paths.workspace_dir.clone()),
                )?;
                emit_json(
                    &mut *stdout.borrow_mut(),
                    json!({ "type": "plugin_commands", "commands": result.get("commands") }),
                )?;
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
                emit_json(
                    &mut *stdout.borrow_mut(),
                    json!({ "type": "plugins_list", "plugins": result.get("plugins") }),
                )?;
            }
            Some("get_config") => {
                let value = read_config_value(&paths.state_dir)?;
                emit_json(
                    &mut *stdout.borrow_mut(),
                    json!({ "type": "config", "config": value }),
                )?;
            }
            Some("update_config") => {
                let Some(patch) = payload.get("config") else {
                    emit_error(&mut *stdout.borrow_mut(), "update_config missing config")?;
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
                emit_json(
                    &mut *stdout.borrow_mut(),
                    json!({ "type": "config_updated", "config": value }),
                )?;
            }
            Some("subscribe_events") => {
                let requested_id = payload
                    .get("subscriptionId")
                    .or_else(|| payload.get("id"))
                    .and_then(|v| v.as_str());
                let kinds = payload
                    .get("kinds")
                    .and_then(|v| v.as_array())
                    .map(|entries| {
                        entries
                            .iter()
                            .filter_map(|entry| entry.as_str().map(|value| value.to_string()))
                            .collect::<Vec<String>>()
                    });
                let (subscription_id, subscriber_count, normalized_kinds) = {
                    let mut registry = subscribers.borrow_mut();
                    let subscription_id = registry.subscribe(requested_id, kinds);
                    let normalized_kinds = registry.kinds_for(&subscription_id);
                    let subscriber_count = registry.len();
                    (subscription_id, subscriber_count, normalized_kinds)
                };
                emit_json(
                    &mut *stdout.borrow_mut(),
                    json!({
                        "type": "subscribed",
                        "subscriptionId": subscription_id,
                        "kinds": normalized_kinds,
                        "subscriberCount": subscriber_count,
                    }),
                )?;
            }
            Some("unsubscribe_events") => {
                let Some(subscription_id) = payload
                    .get("subscriptionId")
                    .or_else(|| payload.get("id"))
                    .and_then(|v| v.as_str())
                else {
                    emit_error(
                        &mut *stdout.borrow_mut(),
                        "unsubscribe_events requires subscriptionId",
                    )?;
                    continue;
                };
                let removed = subscribers.borrow_mut().unsubscribe(subscription_id);
                emit_json(
                    &mut *stdout.borrow_mut(),
                    json!({
                        "type": "unsubscribed",
                        "subscriptionId": subscription_id,
                        "removed": removed,
                        "subscriberCount": subscribers.borrow().len(),
                    }),
                )?;
            }
            Some("list_event_subscriptions") => {
                let subscriptions = subscribers.borrow().list();
                emit_json(
                    &mut *stdout.borrow_mut(),
                    json!({
                        "type": "event_subscriptions",
                        "subscriptions": subscriptions,
                    }),
                )?;
            }
            Some("ping") => {
                emit_json(&mut *stdout.borrow_mut(), json!({ "type": "pong" }))?;
            }
            _ => {
                emit_error(&mut *stdout.borrow_mut(), "Unknown message type")?;
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

fn normalize_kinds(kinds: Option<Vec<String>>) -> Option<HashSet<String>> {
    let kinds = kinds?;
    let mut out = HashSet::new();
    for kind in kinds {
        let normalized = kind.trim().to_ascii_lowercase();
        if !normalized.is_empty() {
            out.insert(normalized);
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn subscription_kinds(subscription: &UiEventSubscription) -> Vec<String> {
    let Some(kinds) = subscription.kinds.as_ref() else {
        return Vec::new();
    };
    let mut out = kinds.iter().cloned().collect::<Vec<String>>();
    out.sort();
    out
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

#[cfg(test)]
mod tests {
    use super::{normalize_kinds, UiEventSubscribers};

    #[test]
    fn subscribe_filters_event_kinds_case_insensitively() {
        let mut registry = UiEventSubscribers::default();
        let id = registry.subscribe(
            Some("alpha"),
            Some(vec![
                "Turn_Completed".to_string(),
                "ITEM_STARTED".to_string(),
            ]),
        );
        assert_eq!(id, "alpha");
        assert_eq!(
            registry.matching_subscribers("turn_completed"),
            vec!["alpha".to_string()]
        );
        assert_eq!(
            registry.matching_subscribers("item_started"),
            vec!["alpha".to_string()]
        );
        assert!(registry.matching_subscribers("turn_started").is_empty());
    }

    #[test]
    fn normalize_kinds_drops_empty_entries() {
        let kinds = normalize_kinds(Some(vec![
            "  ".to_string(),
            "turn_started".to_string(),
            "TURN_STARTED".to_string(),
        ]))
        .expect("normalized kinds");
        assert_eq!(kinds.len(), 1);
        assert!(kinds.contains("turn_started"));
    }
}
