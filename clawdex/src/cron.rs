use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use chrono_tz::Tz;
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::config::ClawdPaths;
use crate::util::{append_json_line, now_ms, read_json_lines, read_json_value, write_json_value};

const JOBS_FILE: &str = "jobs.json";
const RUNS_DIR: &str = "runs";
const PENDING_FILE: &str = "pending.json";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronJob {
    pub id: String,
    pub name: Option<String>,
    #[serde(default)]
    pub session_key: Option<String>,
    pub session_target: String,
    pub wake_mode: String,
    pub payload: Value,
    pub policy: Option<Value>,
    #[serde(default)]
    pub deliver: bool,
    #[serde(default)]
    pub channel: Option<String>,
    #[serde(default)]
    pub to: Option<String>,
    #[serde(default)]
    pub best_effort: bool,
    #[serde(default)]
    pub delivery: Option<CronDelivery>,
    #[serde(default)]
    pub delete_after_run: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CronDelivery {
    pub mode: String,
    pub channel: Option<String>,
    pub to: Option<String>,
    pub best_effort: Option<bool>,
}

#[derive(Debug, Clone)]
struct ScheduleSpec {
    kind: String,
    at_ms: Option<i64>,
    every_ms: Option<i64>,
    anchor_ms: Option<i64>,
    cron: Option<String>,
    tz: Option<String>,
}

impl ScheduleSpec {
    fn from_value(value: &Value) -> Option<Self> {
        let obj = value.as_object()?;
        let kind = obj
            .get("kind")
            .and_then(|v| v.as_str())
            .unwrap_or("cron")
            .to_string();
        let at_ms = obj
            .get("atMs")
            .or_else(|| obj.get("at_ms"))
            .and_then(|v| v.as_i64());
        let every_ms = obj
            .get("everyMs")
            .or_else(|| obj.get("every_ms"))
            .and_then(|v| v.as_i64());
        let anchor_ms = obj
            .get("anchorMs")
            .or_else(|| obj.get("anchor_ms"))
            .and_then(|v| v.as_i64());
        let cron = obj
            .get("cron")
            .or_else(|| obj.get("expr"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let tz = obj
            .get("timezone")
            .or_else(|| obj.get("tz"))
            .or_else(|| obj.get("timeZone"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        Some(Self {
            kind,
            at_ms,
            every_ms,
            anchor_ms,
            cron,
            tz,
        })
    }

    fn next_run_after(
        &self,
        _last_run_at_ms: Option<i64>,
        _created_at_ms: Option<i64>,
        now: i64,
    ) -> Option<i64> {
        match self.kind.as_str() {
            "at" => {
                let at_ms = self.at_ms?;
                Some(at_ms)
            }
            "every" => {
                let every_ms = self.every_ms?;
                let anchor = self.anchor_ms.unwrap_or(now);
                if now < anchor {
                    return Some(anchor);
                }
                let elapsed = now.saturating_sub(anchor);
                let intervals = ((elapsed + every_ms - 1) / every_ms).max(1);
                Some(anchor + intervals * every_ms)
            }
            "cron" => {
                let expr = self.cron.as_ref()?;
                let schedule = cron::Schedule::from_str(expr).ok()?;
                let tz: Tz = self
                    .tz
                    .as_deref()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(chrono_tz::UTC);
                let now_dt = Utc.timestamp_millis_opt(now).single()?;
                let now_tz = now_dt.with_timezone(&tz);
                let next = schedule.after(&now_tz).next()?;
                Some(next.timestamp_millis())
            }
            _ => None,
        }
    }

    #[allow(dead_code)]
    fn is_due(&self, last_run_at_ms: Option<i64>, _created_at_ms: Option<i64>, now: i64) -> bool {
        match self.kind.as_str() {
            "at" => {
                let at_ms = match self.at_ms {
                    Some(value) => value,
                    None => return false,
                };
                if now < at_ms {
                    return false;
                }
                last_run_at_ms.map(|last| last < at_ms).unwrap_or(true)
            }
            "every" => {
                let every_ms = match self.every_ms {
                    Some(value) => value,
                    None => return false,
                };
                let anchor = self.anchor_ms.or(last_run_at_ms).unwrap_or(now);
                now.saturating_sub(anchor) >= every_ms
            }
            "cron" => {
                let expr = match self.cron.as_ref() {
                    Some(value) => value,
                    None => return false,
                };
                let schedule = match cron::Schedule::from_str(expr) {
                    Ok(value) => value,
                    Err(_) => return false,
                };
                let tz: Tz = self
                    .tz
                    .as_deref()
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(chrono_tz::UTC);
                let last_marker = last_run_at_ms.unwrap_or(now - 60_000);
                let last_dt = match Utc.timestamp_millis_opt(last_marker).single() {
                    Some(dt) => dt.with_timezone(&tz),
                    None => return false,
                };
                let next = match schedule.after(&last_dt).next() {
                    Some(dt) => dt.timestamp_millis(),
                    None => return false,
                };
                next <= now
            }
            _ => false,
        }
    }
}

fn parse_at_ms(value: &Value) -> Option<i64> {
    match value {
        Value::Number(num) => num.as_i64(),
        Value::String(s) => {
            if let Ok(ms) = s.parse::<i64>() {
                return Some(ms);
            }
            if let Ok(dt) = chrono::DateTime::parse_from_rfc3339(s) {
                return Some(dt.timestamp_millis());
            }
            None
        }
        _ => None,
    }
}

fn normalize_schedule(mut schedule: Map<String, Value>) -> Map<String, Value> {
    if !schedule.contains_key("kind") {
        if schedule.contains_key("atMs") || schedule.contains_key("at") {
            schedule.insert("kind".to_string(), Value::String("at".to_string()));
        } else if schedule.contains_key("everyMs") || schedule.contains_key("every_ms") {
            schedule.insert("kind".to_string(), Value::String("every".to_string()));
        } else if schedule.contains_key("expr") || schedule.contains_key("cron") {
            schedule.insert("kind".to_string(), Value::String("cron".to_string()));
        }
    }

    if let Some(at_value) = schedule.get("at").cloned() {
        if let Some(ms) = parse_at_ms(&at_value) {
            schedule.insert("atMs".to_string(), Value::Number(ms.into()));
        }
        schedule.remove("at");
    }

    if let Some(Value::String(expr)) = schedule.get("expr").cloned() {
        if !expr.is_empty() {
            schedule
                .entry("cron".to_string())
                .or_insert_with(|| Value::String(expr));
        }
    }

    if let Some(Value::String(tz)) = schedule.get("tz").cloned() {
        if !tz.is_empty() {
            schedule
                .entry("timezone".to_string())
                .or_insert_with(|| Value::String(tz));
        }
    }

    schedule
}

fn normalize_payload(mut payload: Map<String, Value>) -> Map<String, Value> {
    if let Some(Value::String(channel)) = payload.get("channel").cloned() {
        let trimmed = channel.trim().to_lowercase();
        if trimmed.is_empty() {
            payload.remove("channel");
        } else if trimmed != channel {
            payload.insert("channel".to_string(), Value::String(trimmed));
        }
    }

    if let Some(Value::String(provider)) = payload.get("provider").cloned() {
        let trimmed = provider.trim().to_lowercase();
        if !trimmed.is_empty() {
            payload.insert("channel".to_string(), Value::String(trimmed));
        }
        payload.remove("provider");
    }

    payload
}

fn normalize_delivery_mode(raw_mode: &str) -> String {
    let trimmed = raw_mode.trim().to_lowercase();
    if trimmed == "deliver" {
        "announce".to_string()
    } else {
        trimmed
    }
}

fn delivery_mode(value: Option<&Value>) -> Option<String> {
    value
        .and_then(|delivery| delivery.get("mode"))
        .and_then(|mode| mode.as_str())
        .map(normalize_delivery_mode)
}

pub(crate) fn normalize_http_webhook_url(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let parsed = Url::parse(trimmed).ok()?;
    match parsed.scheme() {
        "http" | "https" => Some(parsed.to_string()),
        _ => None,
    }
}

fn normalize_delivery(mut delivery: Map<String, Value>) -> Map<String, Value> {
    if let Some(Value::String(mode)) = delivery.get("mode").cloned() {
        let normalized = normalize_delivery_mode(&mode);
        delivery.insert("mode".to_string(), Value::String(normalized));
    }

    if let Some(Value::String(channel)) = delivery.get("channel").cloned() {
        let trimmed = channel.trim().to_lowercase();
        if trimmed.is_empty() {
            delivery.remove("channel");
        } else if trimmed != channel {
            delivery.insert("channel".to_string(), Value::String(trimmed));
        }
    }

    if let Some(Value::String(to)) = delivery.get("to").cloned() {
        let trimmed = to.trim().to_string();
        if trimmed.is_empty() {
            delivery.remove("to");
        } else if trimmed != to {
            delivery.insert("to".to_string(), Value::String(trimmed));
        }
    }

    if delivery_mode(Some(&Value::Object(delivery.clone()))).as_deref() == Some("webhook") {
        if let Some(Value::String(to)) = delivery.get("to").cloned() {
            if let Some(normalized) = normalize_http_webhook_url(&to) {
                delivery.insert("to".to_string(), Value::String(normalized));
            }
        }
    }

    delivery
}

fn has_legacy_delivery_hints(
    payload: &Map<String, Value>,
    job: Option<&Map<String, Value>>,
) -> bool {
    if payload.get("deliver").and_then(|v| v.as_bool()).is_some() {
        return true;
    }
    if payload
        .get("bestEffortDeliver")
        .and_then(|v| v.as_bool())
        .is_some()
    {
        return true;
    }
    if payload
        .get("to")
        .and_then(|v| v.as_str())
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
    {
        return true;
    }
    if let Some(job) = job {
        if job.get("deliver").and_then(|v| v.as_bool()).is_some() {
            return true;
        }
        if job
            .get("bestEffortDeliver")
            .or_else(|| job.get("bestEffort"))
            .and_then(|v| v.as_bool())
            .is_some()
        {
            return true;
        }
        if job
            .get("to")
            .and_then(|v| v.as_str())
            .map(|v| !v.trim().is_empty())
            .unwrap_or(false)
        {
            return true;
        }
    }
    false
}

fn build_delivery_from_legacy(
    payload: &Map<String, Value>,
    job: Option<&Map<String, Value>>,
) -> Option<Map<String, Value>> {
    if !has_legacy_delivery_hints(payload, job) {
        return None;
    }

    let deliver = payload
        .get("deliver")
        .and_then(|v| v.as_bool())
        .or_else(|| job.and_then(|j| j.get("deliver").and_then(|v| v.as_bool())));
    let mode = if deliver == Some(false) {
        "none"
    } else {
        "announce"
    };

    let channel = payload
        .get("channel")
        .or_else(|| job.and_then(|j| j.get("channel")))
        .and_then(|v| v.as_str())
        .map(|v| v.trim().to_lowercase())
        .filter(|v| !v.is_empty());
    let to = payload
        .get("to")
        .or_else(|| job.and_then(|j| j.get("to")))
        .and_then(|v| v.as_str())
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty());
    let best_effort = payload
        .get("bestEffortDeliver")
        .or_else(|| job.and_then(|j| j.get("bestEffortDeliver")))
        .or_else(|| job.and_then(|j| j.get("bestEffort")))
        .and_then(|v| v.as_bool());

    let mut delivery = Map::new();
    delivery.insert("mode".to_string(), Value::String(mode.to_string()));
    if let Some(channel) = channel {
        delivery.insert("channel".to_string(), Value::String(channel));
    }
    if let Some(to) = to {
        delivery.insert("to".to_string(), Value::String(to));
    }
    if let Some(best_effort) = best_effort {
        delivery.insert("bestEffort".to_string(), Value::Bool(best_effort));
    }
    Some(delivery)
}

fn strip_legacy_delivery_fields(payload: &mut Map<String, Value>) {
    payload.remove("deliver");
    payload.remove("channel");
    payload.remove("to");
    payload.remove("bestEffortDeliver");
    payload.remove("provider");
}

fn strip_legacy_job_delivery_fields(job: &mut Map<String, Value>) {
    job.remove("deliver");
    job.remove("channel");
    job.remove("to");
    job.remove("bestEffortDeliver");
    job.remove("bestEffort");
}

fn merge_delivery(existing: Option<&Value>, patch: &Map<String, Value>) -> Map<String, Value> {
    let mut next = Map::new();
    let existing_obj = existing.and_then(|v| v.as_object());
    if let Some(Value::String(mode)) = existing_obj.and_then(|o| o.get("mode")) {
        next.insert("mode".to_string(), Value::String(mode.clone()));
    } else {
        next.insert("mode".to_string(), Value::String("none".to_string()));
    }
    if let Some(Value::String(channel)) = existing_obj.and_then(|o| o.get("channel")) {
        next.insert("channel".to_string(), Value::String(channel.clone()));
    }
    if let Some(Value::String(to)) = existing_obj.and_then(|o| o.get("to")) {
        next.insert("to".to_string(), Value::String(to.clone()));
    }
    if let Some(Value::Bool(best_effort)) = existing_obj.and_then(|o| o.get("bestEffort")) {
        next.insert("bestEffort".to_string(), Value::Bool(*best_effort));
    }

    if let Some(Value::String(mode)) = patch.get("mode") {
        let trimmed = mode.trim().to_lowercase();
        let normalized = if trimmed == "deliver" {
            "announce".to_string()
        } else {
            trimmed
        };
        next.insert("mode".to_string(), Value::String(normalized));
    }
    if patch.contains_key("channel") {
        let channel = patch
            .get("channel")
            .and_then(|v| v.as_str())
            .map(|v| v.trim().to_lowercase())
            .unwrap_or_default();
        if channel.is_empty() {
            next.remove("channel");
        } else {
            next.insert("channel".to_string(), Value::String(channel));
        }
    }
    if patch.contains_key("to") {
        let to = patch
            .get("to")
            .and_then(|v| v.as_str())
            .map(|v| v.trim().to_string())
            .unwrap_or_default();
        if to.is_empty() {
            next.remove("to");
        } else {
            next.insert("to".to_string(), Value::String(to));
        }
    }
    if let Some(Value::Bool(best_effort)) = patch.get("bestEffort") {
        next.insert("bestEffort".to_string(), Value::Bool(*best_effort));
    }

    next
}

fn parse_delivery_map(delivery: &Map<String, Value>) -> CronDelivery {
    let mode = delivery
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("none")
        .to_string();
    let channel = delivery
        .get("channel")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let to = delivery
        .get("to")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let best_effort = delivery.get("bestEffort").and_then(|v| v.as_bool());
    CronDelivery {
        mode,
        channel,
        to,
        best_effort,
    }
}

fn normalize_job_input(raw: &Value, apply_defaults: bool) -> Result<Map<String, Value>> {
    let base = if let Some(obj) = raw.as_object() {
        if let Some(data) = obj.get("data").and_then(|v| v.as_object()) {
            data.clone()
        } else if let Some(job) = obj.get("job").and_then(|v| v.as_object()) {
            job.clone()
        } else {
            obj.clone()
        }
    } else {
        anyhow::bail!("cron job input must be an object");
    };

    let mut map = base.clone();

    if let Some(agent) = map.get("agentId") {
        match agent {
            Value::Null => {}
            Value::String(s) => {
                let trimmed = s.trim();
                if trimmed.is_empty() {
                    map.remove("agentId");
                } else {
                    map.insert("agentId".to_string(), Value::String(trimmed.to_string()));
                }
            }
            _ => {}
        }
    }

    if !map.contains_key("sessionKey") {
        if let Some(session_key) = map.remove("session_key") {
            map.insert("sessionKey".to_string(), session_key);
        }
    }
    if let Some(session_key) = map.get("sessionKey").cloned() {
        match session_key {
            Value::Null => {
                if apply_defaults {
                    map.remove("sessionKey");
                }
            }
            Value::String(value) => {
                let trimmed = value.trim();
                if trimmed.is_empty() {
                    map.remove("sessionKey");
                } else if trimmed != value {
                    map.insert("sessionKey".to_string(), Value::String(trimmed.to_string()));
                }
            }
            _ => {
                map.remove("sessionKey");
            }
        }
    }

    if let Some(enabled) = map.get("enabled") {
        if let Value::String(s) = enabled {
            let trimmed = s.trim().to_lowercase();
            if trimmed == "true" {
                map.insert("enabled".to_string(), Value::Bool(true));
            } else if trimmed == "false" {
                map.insert("enabled".to_string(), Value::Bool(false));
            }
        }
    }

    if let Some(Value::Object(schedule)) = map.get("schedule").cloned() {
        map.insert(
            "schedule".to_string(),
            Value::Object(normalize_schedule(schedule)),
        );
    }

    if let Some(Value::Object(payload)) = map.get("payload").cloned() {
        map.insert(
            "payload".to_string(),
            Value::Object(normalize_payload(payload)),
        );
    }

    if let Some(Value::Object(delivery)) = map.get("delivery").cloned() {
        map.insert(
            "delivery".to_string(),
            Value::Object(normalize_delivery(delivery)),
        );
    }

    if apply_defaults {
        map.entry("enabled".to_string())
            .or_insert_with(|| Value::Bool(true));
        map.entry("wakeMode".to_string())
            .or_insert_with(|| Value::String("next-heartbeat".to_string()));
        if !map.contains_key("sessionTarget") {
            if let Some(kind) = map
                .get("payload")
                .and_then(|v| v.get("kind"))
                .and_then(|v| v.as_str())
            {
                let target = match kind {
                    "systemEvent" => "main",
                    "agentTurn" => "isolated",
                    _ => "",
                };
                if !target.is_empty() {
                    map.insert(
                        "sessionTarget".to_string(),
                        Value::String(target.to_string()),
                    );
                }
            }
        }
        if let Some(Value::Object(schedule)) = map.get("schedule") {
            if schedule
                .get("kind")
                .and_then(|v| v.as_str())
                .map(|v| v == "at")
                .unwrap_or(false)
                && !map.contains_key("deleteAfterRun")
            {
                map.insert("deleteAfterRun".to_string(), Value::Bool(true));
            }
        }
        let session_target = map
            .get("sessionTarget")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let payload_kind = map
            .get("payload")
            .and_then(|v| v.get("kind"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let is_isolated_agent_turn = session_target == "isolated"
            || (session_target.is_empty() && payload_kind == "agentTurn");
        let has_delivery = map.get("delivery").is_some();
        if is_isolated_agent_turn && payload_kind == "agentTurn" && !has_delivery {
            let legacy_delivery =
                map.get("payload")
                    .and_then(|v| v.as_object())
                    .and_then(|payload| {
                        if has_legacy_delivery_hints(payload, Some(&map)) {
                            build_delivery_from_legacy(payload, Some(&map))
                        } else {
                            None
                        }
                    });

            if let Some(delivery) = legacy_delivery {
                map.insert(
                    "delivery".to_string(),
                    Value::Object(normalize_delivery(delivery)),
                );
                if let Some(Value::Object(payload_mut)) = map.get_mut("payload") {
                    strip_legacy_delivery_fields(payload_mut);
                }
                strip_legacy_job_delivery_fields(&mut map);
            } else {
                map.insert(
                    "delivery".to_string(),
                    Value::Object(Map::from_iter([(
                        "mode".to_string(),
                        Value::String("announce".to_string()),
                    )])),
                );
            }
        }
    }

    Ok(map)
}

fn merge_payload(existing: &Value, patch: &Value) -> Value {
    let Some(Value::Object(existing_map)) = existing.as_object().map(|m| Value::Object(m.clone()))
    else {
        return patch.clone();
    };
    if let Value::Object(patch_map) = patch {
        if let Some(Value::String(kind)) = patch_map.get("kind") {
            if existing_map
                .get("kind")
                .and_then(|v| v.as_str())
                .map(|v| v != kind.as_str())
                .unwrap_or(true)
            {
                return Value::Object(patch_map.clone());
            }
        }
        let mut merged = existing_map.clone();
        for (key, value) in patch_map {
            merged.insert(key.clone(), value.clone());
        }
        return Value::Object(merged);
    }
    patch.clone()
}

fn validate_job_spec(map: &Map<String, Value>) -> Result<()> {
    let session_target = map
        .get("sessionTarget")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let payload_kind = map
        .get("payload")
        .and_then(|v| v.get("kind"))
        .and_then(|v| v.as_str())
        .unwrap_or("");
    let delivery = map.get("delivery");
    let has_delivery = delivery.is_some();

    if session_target == "main" && payload_kind != "systemEvent" {
        anyhow::bail!("main cron jobs require payload.kind=\"systemEvent\"");
    }
    if session_target == "isolated" && payload_kind != "agentTurn" {
        anyhow::bail!("isolated cron jobs require payload.kind=\"agentTurn\"");
    }
    if has_delivery {
        let mode = delivery_mode(delivery).unwrap_or_else(|| "announce".to_string());
        if mode == "webhook" {
            let webhook_target = delivery
                .and_then(|value| value.get("to"))
                .and_then(|value| value.as_str())
                .and_then(normalize_http_webhook_url);
            if webhook_target.is_none() {
                anyhow::bail!(
                    "cron webhook delivery requires delivery.to to be a valid http(s) URL"
                );
            }
        } else if session_target != "isolated" {
            anyhow::bail!(
                "cron channel delivery config is only supported for sessionTarget=\"isolated\""
            );
        }
    }
    Ok(())
}

fn jobs_path(paths: &ClawdPaths) -> PathBuf {
    paths.cron_dir.join(JOBS_FILE)
}

fn runs_path(paths: &ClawdPaths, job_id: &str) -> PathBuf {
    paths
        .cron_dir
        .join(RUNS_DIR)
        .join(format!("{job_id}.jsonl"))
}

fn pending_path(paths: &ClawdPaths) -> PathBuf {
    paths.cron_dir.join(PENDING_FILE)
}

fn load_jobs(paths: &ClawdPaths) -> Result<Vec<Value>> {
    let path = jobs_path(paths);
    let Some(value) = read_json_value(&path)? else {
        return Ok(Vec::new());
    };
    let mut jobs = match value {
        Value::Array(items) => items,
        Value::Object(mut obj) => match obj.remove("jobs") {
            Some(Value::Array(items)) => items,
            _ => anyhow::bail!("cron jobs file missing jobs array"),
        },
        _ => anyhow::bail!("cron jobs file is not an array or object"),
    };

    let mut mutated = false;
    for job in &mut jobs {
        let Some(map) = job.as_object_mut() else {
            continue;
        };

        if !map.contains_key("sessionKey") {
            if let Some(session_key) = map.remove("session_key") {
                map.insert("sessionKey".to_string(), session_key);
                mutated = true;
            }
        }
        if let Some(session_key) = map.get("sessionKey").cloned() {
            match session_key {
                Value::String(value) => {
                    let trimmed = value.trim();
                    if trimmed.is_empty() {
                        map.remove("sessionKey");
                        mutated = true;
                    } else if trimmed != value {
                        map.insert("sessionKey".to_string(), Value::String(trimmed.to_string()));
                        mutated = true;
                    }
                }
                _ => {
                    map.remove("sessionKey");
                    mutated = true;
                }
            }
        }

        if let Some(Value::Object(payload)) = map.get_mut("payload") {
            let normalized = normalize_payload(payload.clone());
            if *payload != normalized {
                *payload = normalized;
                mutated = true;
            }
        }

        if let Some(Value::Object(delivery)) = map.get_mut("delivery") {
            let normalized = normalize_delivery(delivery.clone());
            if *delivery != normalized {
                *delivery = normalized;
                mutated = true;
            }
        }

        let payload_kind = map
            .get("payload")
            .and_then(|v| v.get("kind"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let session_target = map
            .get("sessionTarget")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let is_isolated_agent_turn = session_target == "isolated"
            || (session_target.is_empty() && payload_kind == "agentTurn");

        if is_isolated_agent_turn && payload_kind == "agentTurn" {
            let legacy_patch = map
                .get("payload")
                .and_then(|v| v.as_object())
                .and_then(|payload| build_delivery_from_legacy(payload, Some(map)));

            let has_delivery = map.get("delivery").is_some();
            if !has_delivery {
                if let Some(delivery) = legacy_patch {
                    map.insert(
                        "delivery".to_string(),
                        Value::Object(normalize_delivery(delivery)),
                    );
                    if let Some(Value::Object(payload_mut)) = map.get_mut("payload") {
                        strip_legacy_delivery_fields(payload_mut);
                    }
                    strip_legacy_job_delivery_fields(map);
                    mutated = true;
                } else {
                    map.insert(
                        "delivery".to_string(),
                        Value::Object(Map::from_iter([(
                            "mode".to_string(),
                            Value::String("announce".to_string()),
                        )])),
                    );
                    mutated = true;
                }
            } else if let Some(delivery) = legacy_patch {
                let merged = merge_delivery(map.get("delivery"), &delivery);
                map.insert("delivery".to_string(), Value::Object(merged));
                if let Some(Value::Object(payload_mut)) = map.get_mut("payload") {
                    strip_legacy_delivery_fields(payload_mut);
                }
                strip_legacy_job_delivery_fields(map);
                mutated = true;
            }
        }

        if session_target == "main" {
            let keep_webhook = delivery_mode(map.get("delivery")).as_deref() == Some("webhook");
            if !keep_webhook && map.remove("delivery").is_some() {
                mutated = true;
            }
        }
    }

    if mutated {
        save_jobs(paths, &jobs)?;
    }

    Ok(jobs)
}

fn save_jobs(paths: &ClawdPaths, jobs: &[Value]) -> Result<()> {
    let path = jobs_path(paths);
    write_json_value(
        &path,
        &json!({
            "version": 1,
            "jobs": jobs
        }),
    )
}

pub(crate) fn load_job_value(paths: &ClawdPaths, job_id: &str) -> Result<Option<Value>> {
    let jobs = load_jobs(paths)?;
    for job in jobs {
        if job
            .get("id")
            .and_then(|v| v.as_str())
            .map(|v| v == job_id)
            .unwrap_or(false)
        {
            return Ok(Some(job));
        }
    }
    Ok(None)
}

fn ensure_job_id(map: &mut Map<String, Value>) -> String {
    if let Some(id) = map.get("id").and_then(|v| v.as_str()) {
        return id.to_string();
    }
    if let Some(id) = map
        .get("jobId")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
    {
        map.insert("id".to_string(), Value::String(id.clone()));
        return id;
    }
    let id = Uuid::new_v4().to_string();
    map.insert("id".to_string(), Value::String(id.clone()));
    id
}

fn job_schedule(job: &Value) -> Option<ScheduleSpec> {
    let schedule = job.get("schedule")?;
    ScheduleSpec::from_value(schedule)
}

fn job_enabled(job: &Value) -> bool {
    job.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true)
}

fn job_running(job: &Value) -> bool {
    job.get("state")
        .and_then(|v| v.get("runningAtMs"))
        .and_then(|v| v.as_i64())
        .is_some()
}

fn job_created_at(job: &Value) -> Option<i64> {
    job.get("createdAtMs")
        .or_else(|| job.get("created_at_ms"))
        .or_else(|| job.get("state").and_then(|v| v.get("createdAtMs")))
        .and_then(|v| v.as_i64())
}

fn job_last_run_at(job: &Value) -> Option<i64> {
    job.get("lastRunAtMs")
        .or_else(|| job.get("last_run_at_ms"))
        .or_else(|| job.get("state").and_then(|v| v.get("lastRunAtMs")))
        .and_then(|v| v.as_i64())
}

fn job_id_from_args(args: &Value) -> Option<String> {
    args.get("jobId")
        .or_else(|| args.get("id"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn find_job_mut<'a>(jobs: &'a mut [Value], job_id: &str) -> Option<&'a mut Map<String, Value>> {
    for job in jobs {
        if let Value::Object(map) = job {
            if map
                .get("id")
                .and_then(|v| v.as_str())
                .map(|v| v == job_id)
                .unwrap_or(false)
            {
                return Some(map);
            }
        }
    }
    None
}

fn job_state_mut(job: &mut Map<String, Value>) -> &mut Map<String, Value> {
    let needs_init = match job.get("state") {
        Some(Value::Object(_)) => false,
        _ => true,
    };
    if needs_init {
        job.insert("state".to_string(), Value::Object(Map::new()));
    }
    match job.get_mut("state") {
        Some(Value::Object(map)) => map,
        _ => unreachable!("state should be object"),
    }
}

fn set_state_field(job: &mut Map<String, Value>, key: &str, value: Value) {
    let state = job_state_mut(job);
    state.insert(key.to_string(), value);
}

fn compute_next_run(job: &Value, now: i64) -> Option<i64> {
    let schedule = job_schedule(job)?;
    if schedule.kind == "at" {
        let last_status = job
            .get("state")
            .and_then(|v| v.get("lastStatus"))
            .and_then(|v| v.as_str());
        let last_run = job_last_run_at(job);
        if last_status == Some("ok") && last_run.is_some() {
            return None;
        }
        return schedule.at_ms;
    }
    schedule.next_run_after(job_last_run_at(job), job_created_at(job), now)
}

pub(crate) fn is_job_due_value(job: &Value, now: i64, forced: bool) -> bool {
    if forced {
        return true;
    }
    if !job_enabled(job) {
        return false;
    }
    if let Some(next) = job
        .get("state")
        .and_then(|v| v.get("nextRunAtMs"))
        .and_then(|v| v.as_i64())
    {
        return now >= next;
    }
    compute_next_run(job, now)
        .map(|next| now >= next)
        .unwrap_or(false)
}

fn payload_message(payload: &Value) -> Option<String> {
    if let Some(text) = payload.as_str() {
        return Some(text.to_string());
    }
    payload
        .get("message")
        .or_else(|| payload.get("text"))
        .or_else(|| payload.get("prompt"))
        .or_else(|| payload.get("event"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

pub(crate) fn build_cron_job(job: &Value) -> Option<CronJob> {
    let id = job
        .get("id")
        .or_else(|| job.get("jobId"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())?;
    let name = job
        .get("name")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let session_key = job
        .get("sessionKey")
        .or_else(|| job.get("session_key"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let session_target = job
        .get("sessionTarget")
        .and_then(|v| v.as_str())
        .unwrap_or("main")
        .to_string();
    let wake_mode = job
        .get("wakeMode")
        .and_then(|v| v.as_str())
        .unwrap_or("now")
        .to_string();
    let payload = job.get("payload").cloned().unwrap_or(Value::Null);
    let policy = job
        .get("policy")
        .cloned()
        .or_else(|| payload.get("policy").cloned());
    let deliver = payload
        .get("deliver")
        .or_else(|| job.get("deliver"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let channel = payload
        .get("channel")
        .or_else(|| job.get("channel"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let to = payload
        .get("to")
        .or_else(|| job.get("to"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let best_effort = payload
        .get("bestEffortDeliver")
        .or_else(|| job.get("bestEffortDeliver"))
        .or_else(|| job.get("bestEffort"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let mut delivery = job
        .get("delivery")
        .and_then(|v| v.as_object())
        .map(|v| normalize_delivery(v.clone()))
        .map(|v| parse_delivery_map(&v));
    if delivery.is_none() {
        if let Some(payload_map) = payload.as_object() {
            if let Some(map) = build_delivery_from_legacy(payload_map, job.as_object()) {
                delivery = Some(parse_delivery_map(&normalize_delivery(map)));
            }
        }
    }
    if delivery.is_none()
        && session_target == "isolated"
        && payload
            .get("kind")
            .and_then(|v| v.as_str())
            .map(|v| v == "agentTurn")
            .unwrap_or(false)
    {
        delivery = Some(CronDelivery {
            mode: "announce".to_string(),
            channel: None,
            to: None,
            best_effort: None,
        });
    }
    let delete_after_run = job
        .get("deleteAfterRun")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    Some(CronJob {
        id,
        name,
        session_key,
        session_target,
        wake_mode,
        payload,
        policy,
        deliver,
        channel,
        to,
        best_effort,
        delivery,
        delete_after_run,
    })
}

pub fn job_prompt(job: &CronJob, now: i64) -> Option<String> {
    let kind = job
        .payload
        .get("kind")
        .and_then(|v| v.as_str())
        .unwrap_or("agentTurn");
    let message = payload_message(&job.payload)?;
    let stamp = Utc
        .timestamp_millis_opt(now)
        .single()
        .map(|dt| dt.to_rfc3339())
        .unwrap_or_else(|| "unknown-time".to_string());
    let label = match &job.name {
        Some(name) => format!("[cron:{} {}]", job.id, name),
        None => format!("[cron:{}]", job.id),
    };
    let prefix = match kind {
        "systemEvent" => "System event",
        _ => "Cron job",
    };
    Some(format!("{label} {prefix} @ {stamp}\n\n{message}"))
}

pub fn job_session_key(job: &CronJob) -> String {
    if let Some(session_key) = job
        .session_key
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        return session_key.to_string();
    }
    match job.session_target.as_str() {
        "isolated" => format!("cron:{}", job.id),
        _ => "agent:main:main".to_string(),
    }
}

pub fn drain_pending_jobs(paths: &ClawdPaths) -> Result<Vec<CronJob>> {
    let path = pending_path(paths);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let data = read_json_value(&path)?.unwrap_or(Value::Array(Vec::new()));
    let jobs: Vec<CronJob> = serde_json::from_value(data).unwrap_or_default();
    write_json_value(&path, &Value::Array(Vec::new()))?;
    Ok(jobs)
}

fn enqueue_pending_job(paths: &ClawdPaths, job: &CronJob) -> Result<()> {
    let path = pending_path(paths);
    let mut pending: Vec<CronJob> = if let Some(value) = read_json_value(&path)? {
        serde_json::from_value(value).unwrap_or_default()
    } else {
        Vec::new()
    };
    pending.push(job.clone());
    write_json_value(&path, &serde_json::to_value(pending)?)?;
    Ok(())
}

pub fn list_jobs(paths: &ClawdPaths, include_disabled: bool) -> Result<Value> {
    let jobs = load_jobs(paths)?;
    let now = now_ms();
    let mut filtered: Vec<Value> = if include_disabled {
        jobs
    } else {
        jobs.into_iter().filter(job_enabled).collect()
    };

    for job in &mut filtered {
        let enabled = job_enabled(job);
        let next_run = if enabled {
            compute_next_run(job, now)
        } else {
            None
        };
        if let Value::Object(map) = job {
            if enabled {
                if let Some(next) = next_run {
                    set_state_field(map, "nextRunAtMs", Value::Number(next.into()));
                }
            } else if let Some(Value::Object(state)) = map.get_mut("state") {
                state.remove("nextRunAtMs");
                state.remove("runningAtMs");
            }
        }
    }

    filtered.sort_by(|a, b| {
        let a_next = a
            .get("state")
            .and_then(|v| v.get("nextRunAtMs"))
            .and_then(|v| v.as_i64())
            .unwrap_or(i64::MAX);
        let b_next = b
            .get("state")
            .and_then(|v| v.get("nextRunAtMs"))
            .and_then(|v| v.as_i64())
            .unwrap_or(i64::MAX);
        a_next.cmp(&b_next)
    });

    Ok(json!({ "jobs": filtered }))
}

pub fn status(paths: &ClawdPaths, enabled: bool) -> Result<Value> {
    let jobs = load_jobs(paths)?;
    let now = now_ms();
    let mut next_wake: Option<i64> = None;
    let mut total_jobs = 0usize;
    for job in &jobs {
        total_jobs += 1;
        if !job_enabled(job) {
            continue;
        }
        if let Some(schedule) = job_schedule(job) {
            let next = job
                .get("state")
                .and_then(|v| v.get("nextRunAtMs"))
                .and_then(|v| v.as_i64())
                .or_else(|| {
                    schedule.next_run_after(job_last_run_at(job), job_created_at(job), now)
                });
            if let Some(next) = next {
                next_wake = Some(next_wake.map_or(next, |current| current.min(next)));
            }
        }
    }
    Ok(json!({
        "enabled": enabled,
        "storePath": jobs_path(paths).to_string_lossy(),
        "jobs": total_jobs,
        "nextWakeAtMs": next_wake,
    }))
}

pub fn add_job(paths: &ClawdPaths, args: &Value) -> Result<Value> {
    let mut jobs = load_jobs(paths)?;
    let mut map = normalize_job_input(args, true)?;

    let _id = ensure_job_id(&mut map);
    let now = now_ms();
    map.entry("createdAtMs".to_string())
        .or_insert_with(|| Value::Number(now.into()));
    map.insert("updatedAtMs".to_string(), Value::Number(now.into()));
    map.entry("enabled".to_string())
        .or_insert_with(|| Value::Bool(true));
    validate_job_spec(&map)?;

    let mut value = Value::Object(map);
    if job_enabled(&value) {
        if let Some(next) = compute_next_run(&value, now) {
            if let Value::Object(map) = &mut value {
                set_state_field(map, "nextRunAtMs", Value::Number(next.into()));
            }
        }
    }
    jobs.push(value.clone());
    save_jobs(paths, &jobs)?;
    Ok(value)
}

pub fn update_job(paths: &ClawdPaths, args: &Value) -> Result<Value> {
    let mut jobs = load_jobs(paths)?;
    let job_id = job_id_from_args(args).context("cron.update requires jobId or id")?;
    let patch = args
        .get("patch")
        .cloned()
        .context("cron.update requires patch")?;
    let patch_map = normalize_job_input(&patch, false)?;
    let now = now_ms();

    let job = find_job_mut(&mut jobs, &job_id).context("job not found")?;
    let mut saw_delivery_patch = false;
    let mut payload_patch: Option<Map<String, Value>> = None;
    for (key, value) in patch_map {
        if key == "payload" {
            if let Value::Object(map) = &value {
                payload_patch = Some(map.clone());
            }
            let merged = merge_payload(job.get("payload").unwrap_or(&Value::Null), &value);
            job.insert(key, merged);
        } else if key == "delivery" {
            saw_delivery_patch = true;
            match value {
                Value::Object(delivery_patch) => {
                    let normalized = normalize_delivery(delivery_patch);
                    let merged = merge_delivery(job.get("delivery"), &normalized);
                    job.insert("delivery".to_string(), Value::Object(merged));
                }
                Value::Null => {
                    job.remove("delivery");
                }
                _ => {
                    job.insert("delivery".to_string(), value);
                }
            }
        } else if key == "sessionKey" {
            match value {
                Value::Null => {
                    job.remove("sessionKey");
                }
                Value::String(raw) => {
                    let trimmed = raw.trim();
                    if trimmed.is_empty() {
                        job.remove("sessionKey");
                    } else {
                        job.insert("sessionKey".to_string(), Value::String(trimmed.to_string()));
                    }
                }
                _ => {}
            }
        } else if key == "state" {
            if let Value::Object(state_patch) = value {
                let state = job_state_mut(job);
                for (field, field_value) in state_patch {
                    state.insert(field, field_value);
                }
            } else {
                job.insert(key, value);
            }
        } else {
            job.insert(key, value);
        }
    }

    if !saw_delivery_patch {
        if let Some(payload_patch) = payload_patch.as_ref() {
            if job
                .get("sessionTarget")
                .and_then(|v| v.as_str())
                .map(|v| v == "isolated")
                .unwrap_or(false)
                && job
                    .get("payload")
                    .and_then(|v| v.get("kind"))
                    .and_then(|v| v.as_str())
                    .map(|v| v == "agentTurn")
                    .unwrap_or(false)
            {
                if let Some(delivery_patch) = build_delivery_from_legacy(payload_patch, None) {
                    let merged = merge_delivery(job.get("delivery"), &delivery_patch);
                    job.insert("delivery".to_string(), Value::Object(merged));
                }
            }
        }
    }

    if job
        .get("sessionTarget")
        .and_then(|v| v.as_str())
        .map(|v| v == "main")
        .unwrap_or(false)
    {
        let keep_webhook = delivery_mode(job.get("delivery")).as_deref() == Some("webhook");
        if !keep_webhook {
            job.remove("delivery");
        }
    }

    job.insert("updatedAtMs".to_string(), Value::Number(now.into()));
    validate_job_spec(job)?;
    let enabled = job_enabled(&Value::Object(job.clone()));
    if enabled {
        let value = Value::Object(job.clone());
        if let Some(next) = compute_next_run(&value, now) {
            set_state_field(job, "nextRunAtMs", Value::Number(next.into()));
        }
    } else {
        let state = job_state_mut(job);
        state.remove("nextRunAtMs");
        state.remove("runningAtMs");
    }
    let updated = Value::Object(job.clone());
    save_jobs(paths, &jobs)?;
    Ok(updated)
}

pub fn remove_job(paths: &ClawdPaths, args: &Value) -> Result<Value> {
    let mut jobs = load_jobs(paths)?;
    let job_id = job_id_from_args(args).context("cron.remove requires jobId or id")?;
    let before = jobs.len();
    jobs.retain(|job| {
        job.get("id")
            .and_then(|v| v.as_str())
            .map(|v| v != job_id)
            .unwrap_or(true)
    });
    let removed = before.saturating_sub(jobs.len()) > 0;
    save_jobs(paths, &jobs)?;
    Ok(json!({ "ok": true, "removed": removed }))
}

pub fn runs(paths: &ClawdPaths, args: &Value) -> Result<Value> {
    let job_id = job_id_from_args(args).context("cron.runs requires jobId or id")?;
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);
    let entries = read_json_lines(&runs_path(paths, &job_id), None)?;
    let mut finished = Vec::new();
    for entry in entries {
        let action = entry.get("action").and_then(|v| v.as_str());
        if action == Some("finished") {
            finished.push(entry);
            continue;
        }
        // Back-compat: map legacy statuses into finished entries.
        if let Some(status) = entry.get("status").and_then(|v| v.as_str()) {
            let mapped = match status {
                "completed" => Some("ok"),
                "delivery_failed" => Some("error"),
                "skipped" => Some("skipped"),
                _ => None,
            };
            if let Some(mapped_status) = mapped {
                let mut updated = entry.clone();
                updated["action"] = Value::String("finished".to_string());
                updated["status"] = Value::String(mapped_status.to_string());
                finished.push(updated);
            }
        }
    }
    if let Some(limit) = limit {
        if finished.len() > limit {
            finished = finished.split_off(finished.len() - limit);
        }
    }
    Ok(json!({ "entries": finished }))
}

pub fn mark_job_running(paths: &ClawdPaths, job_id: &str, started_at: i64) -> Result<()> {
    let mut jobs = load_jobs(paths)?;
    let Some(job) = find_job_mut(&mut jobs, job_id) else {
        return Ok(());
    };
    let state = job_state_mut(job);
    state.insert("runningAtMs".to_string(), Value::Number(started_at.into()));
    state.remove("lastError");
    job.insert("updatedAtMs".to_string(), Value::Number(started_at.into()));
    save_jobs(paths, &jobs)?;
    Ok(())
}

fn update_job_state_from_run(
    paths: &ClawdPaths,
    job_id: &str,
    status: &str,
    run_at_ms: i64,
    duration_ms: i64,
    error: Option<String>,
) -> Result<Option<i64>> {
    let mut jobs = load_jobs(paths)?;
    let Some(job) = find_job_mut(&mut jobs, job_id) else {
        return Ok(None);
    };
    let schedule = job.get("schedule").and_then(ScheduleSpec::from_value);
    let created_at = job.get("createdAtMs").and_then(|v| v.as_i64());
    let duration_ms = duration_ms.max(0);
    let ended_at = run_at_ms.saturating_add(duration_ms);

    let state = job_state_mut(job);
    state.remove("runningAtMs");
    state.insert("lastRunAtMs".to_string(), Value::Number(run_at_ms.into()));
    state.insert("lastStatus".to_string(), Value::String(status.to_string()));
    state.insert(
        "lastDurationMs".to_string(),
        Value::Number(duration_ms.into()),
    );
    if let Some(err) = error.clone() {
        state.insert("lastError".to_string(), Value::String(err));
    } else {
        state.remove("lastError");
    }

    job.insert("lastRunAtMs".to_string(), Value::Number(run_at_ms.into()));
    job.insert("updatedAtMs".to_string(), Value::Number(ended_at.into()));

    let mut delete_job = false;
    let mut enabled = job.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
    let mut next_run: Option<i64> = None;

    if let Some(schedule) = schedule {
        if schedule.kind == "at" && status == "ok" {
            if job
                .get("deleteAfterRun")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                delete_job = true;
            } else {
                enabled = false;
                job.insert("enabled".to_string(), Value::Bool(false));
            }
        }

        if enabled {
            next_run = schedule.next_run_after(Some(run_at_ms), created_at, ended_at);
        }
    }

    if delete_job {
        jobs.retain(|job| {
            job.get("id")
                .and_then(|v| v.as_str())
                .map(|v| v != job_id)
                .unwrap_or(true)
        });
        save_jobs(paths, &jobs)?;
        return Ok(None);
    }

    if let Some(next) = next_run {
        set_state_field(job, "nextRunAtMs", Value::Number(next.into()));
    } else {
        let state = job_state_mut(job);
        state.remove("nextRunAtMs");
    }

    if !enabled {
        let state = job_state_mut(job);
        state.remove("nextRunAtMs");
        state.remove("runningAtMs");
    }

    save_jobs(paths, &jobs)?;
    Ok(next_run)
}

pub fn record_run(
    paths: &ClawdPaths,
    job_id: &str,
    status: &str,
    reason: &str,
    details: Option<Value>,
) -> Result<Value> {
    let now = now_ms();
    let (action, run_status) = match status {
        "completed" => ("finished", Some("ok")),
        "delivery_failed" => ("finished", Some("error")),
        "skipped" => ("finished", Some("skipped")),
        _ => ("queued", None),
    };
    let summary = details
        .as_ref()
        .and_then(|d| d.get("summary"))
        .and_then(|v| v.as_str())
        .and_then(|s| {
            if s.trim().is_empty() {
                None
            } else {
                Some(s.to_string())
            }
        })
        .or_else(|| {
            if reason.trim().is_empty() {
                None
            } else {
                Some(reason.to_string())
            }
        });
    let mut error = if run_status == Some("error") {
        details
            .as_ref()
            .and_then(|d| d.get("error"))
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    } else {
        None
    };
    if run_status == Some("error") && error.is_none() && !reason.trim().is_empty() {
        error = Some(reason.to_string());
    }

    let mut run_at_ms = details
        .as_ref()
        .and_then(|d| d.get("runAtMs"))
        .and_then(|v| v.as_i64());
    let mut duration_ms = details
        .as_ref()
        .and_then(|d| d.get("durationMs"))
        .and_then(|v| v.as_i64());
    if run_status.is_some() {
        if run_at_ms.is_none() {
            run_at_ms = Some(now);
        }
        if duration_ms.is_none() {
            duration_ms = Some(0);
        }
    }

    let apply_state = details
        .as_ref()
        .and_then(|d| d.get("applyState"))
        .and_then(|v| v.as_bool())
        .unwrap_or(true);

    let mut next_run_at_ms = details
        .as_ref()
        .and_then(|d| d.get("nextRunAtMs"))
        .and_then(|v| v.as_i64());

    if let (Some(status), true) = (run_status, apply_state) {
        let run_at = run_at_ms.unwrap_or(now);
        let duration = duration_ms.unwrap_or(0).max(0);
        if let Some(next) =
            update_job_state_from_run(paths, job_id, status, run_at, duration, error.clone())?
        {
            if next_run_at_ms.is_none() {
                next_run_at_ms = Some(next);
            }
        }
    }

    let mut entry = Map::new();
    entry.insert("ts".to_string(), Value::Number(now.into()));
    entry.insert("jobId".to_string(), Value::String(job_id.to_string()));
    entry.insert("action".to_string(), Value::String(action.to_string()));
    if let Some(status) = run_status {
        entry.insert("status".to_string(), Value::String(status.to_string()));
    }
    if let Some(summary) = summary {
        if !summary.trim().is_empty() {
            entry.insert("summary".to_string(), Value::String(summary));
        }
    }
    if let Some(error) = error {
        entry.insert("error".to_string(), Value::String(error));
    }
    if let Some(run_at_ms) = run_at_ms {
        entry.insert("runAtMs".to_string(), Value::Number(run_at_ms.into()));
    }
    if let Some(duration_ms) = duration_ms {
        entry.insert(
            "durationMs".to_string(),
            Value::Number(duration_ms.max(0).into()),
        );
    }
    if let Some(next) = next_run_at_ms {
        entry.insert("nextRunAtMs".to_string(), Value::Number(next.into()));
    }

    let entry = Value::Object(entry);
    append_json_line(&runs_path(paths, job_id), &entry)?;
    Ok(entry)
}

pub fn run_jobs(paths: &ClawdPaths, args: &Value) -> Result<Value> {
    let mode = args.get("mode").and_then(|v| v.as_str()).unwrap_or("due");
    let job_id = job_id_from_args(args).context("cron.run requires jobId or id")?;
    let now = now_ms();
    let forced = mode.eq_ignore_ascii_case("force");

    let job_value = load_job_value(paths, &job_id)?;
    let Some(job_value) = job_value else {
        return Ok(json!({ "ok": false, "ran": false, "reason": "not-found" }));
    };

    if !is_job_due_value(&job_value, now, forced) {
        return Ok(json!({ "ok": true, "ran": false, "reason": "not-due" }));
    }

    let _ = collect_due_jobs(paths, now, mode, Some(job_id))?;

    Ok(json!({ "ok": true, "ran": true }))
}

pub fn collect_due_jobs(
    paths: &ClawdPaths,
    now: i64,
    mode: &str,
    job_filter: Option<String>,
) -> Result<(Vec<CronJob>, Vec<Value>)> {
    let mut jobs = load_jobs(paths)?;
    let mut queued = Vec::new();
    let mut entries = Vec::new();
    let mut dirty = false;

    for job in &mut jobs {
        if !job_enabled(job) {
            continue;
        }
        if job_running(job) {
            continue;
        }
        let Some(map) = job.as_object_mut() else {
            continue;
        };
        let schedule = map.get("schedule").and_then(ScheduleSpec::from_value);
        let last_run = map.get("lastRunAtMs").and_then(|v| v.as_i64());
        let created_at = map.get("createdAtMs").and_then(|v| v.as_i64());
        let mut state_next = map
            .get("state")
            .and_then(|v| v.get("nextRunAtMs"))
            .and_then(|v| v.as_i64());
        if state_next.is_none() {
            if let Some(schedule) = schedule.as_ref() {
                let last_status = map
                    .get("state")
                    .and_then(|v| v.get("lastStatus"))
                    .and_then(|v| v.as_str());
                if !(schedule.kind == "at" && last_status == Some("ok")) {
                    state_next = schedule.next_run_after(last_run, created_at, now);
                    if let Some(next) = state_next {
                        set_state_field(map, "nextRunAtMs", Value::Number(next.into()));
                        dirty = true;
                    }
                }
            }
        }
        let due = if mode == "force" {
            true
        } else {
            state_next.map(|next| now >= next).unwrap_or(false)
        };

        let job_id = map
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| Uuid::new_v4().to_string());

        if let Some(ref filter) = job_filter {
            if &job_id != filter {
                continue;
            }
        }

        if mode == "due" && !due {
            continue;
        }

        let job_value = Value::Object(map.clone());
        if let Some(cron_job) = build_cron_job(&job_value) {
            let wake_mode = cron_job.wake_mode.clone();
            if wake_mode == "next-heartbeat" {
                enqueue_pending_job(paths, &cron_job)?;
                let entry = record_run(paths, &job_id, "queued", "next-heartbeat", None)?;
                entries.push(entry);
            } else {
                let entry = record_run(paths, &job_id, "queued", "scheduled", None)?;
                entries.push(entry);
                queued.push(cron_job.clone());
            }
        } else {
            let entry = record_run(
                paths,
                &job_id,
                "skipped",
                "invalid job",
                Some(json!({ "applyState": false })),
            )?;
            entries.push(entry);
        }
    }

    if dirty {
        save_jobs(paths, &jobs)?;
    }
    Ok((queued, entries))
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::ScheduleSpec;
    use crate::config::load_config;
    use chrono::{TimeZone, Utc};
    use serde_json::{json, Value};
    use uuid::Uuid;

    fn temp_paths() -> crate::config::ClawdPaths {
        let base = std::env::temp_dir().join(format!("clawdex-cron-test-{}", Uuid::new_v4()));
        let state_dir = base.join("state");
        let workspace_dir = base.join("workspace");
        fs::create_dir_all(&workspace_dir).expect("create workspace");
        let (_cfg, paths) =
            load_config(Some(state_dir), Some(workspace_dir)).expect("create temp paths");
        paths
    }

    fn ms(year: i32, month: u32, day: u32, hour: u32, minute: u32, second: u32) -> i64 {
        Utc.with_ymd_and_hms(year, month, day, hour, minute, second)
            .single()
            .expect("valid datetime")
            .timestamp_millis()
    }

    #[test]
    fn at_schedule_due_and_next_run() {
        let now = ms(2026, 2, 4, 12, 0, 0);
        let at_ms = now + 60_000;
        let spec = ScheduleSpec::from_value(&json!({
            "kind": "at",
            "atMs": at_ms
        }))
        .expect("schedule spec");

        assert!(!spec.is_due(None, None, now));
        assert_eq!(spec.next_run_after(None, None, now), Some(at_ms));
        assert!(spec.is_due(None, None, at_ms));
        assert_eq!(spec.next_run_after(None, None, at_ms), Some(at_ms));
    }

    #[test]
    fn every_schedule_due_and_next_run() {
        let anchor = ms(2026, 2, 4, 12, 0, 0);
        let now = anchor + 10_000;
        let spec = ScheduleSpec::from_value(&json!({
            "kind": "every",
            "everyMs": 60_000,
            "anchorMs": anchor
        }))
        .expect("schedule spec");

        assert!(!spec.is_due(None, None, now));
        assert!(spec.is_due(None, None, anchor + 60_000));
        assert_eq!(spec.next_run_after(None, None, now), Some(anchor + 60_000));

        let now = ms(2026, 2, 4, 12, 10, 0);
        let spec_no_anchor = ScheduleSpec::from_value(&json!({
            "kind": "every",
            "everyMs": 60_000
        }))
        .expect("schedule spec");
        assert!(!spec_no_anchor.is_due(None, None, now));
        assert_eq!(
            spec_no_anchor.next_run_after(None, None, now),
            Some(now + 60_000)
        );
    }

    #[test]
    fn cron_schedule_due_and_next_run() {
        let last_run = ms(2026, 2, 4, 12, 0, 0);
        let now = ms(2026, 2, 4, 12, 1, 0);
        let spec = ScheduleSpec::from_value(&json!({
            "kind": "cron",
            "cron": "0 * * * * * *",
            "timezone": "UTC"
        }))
        .expect("schedule spec");

        assert!(spec.is_due(Some(last_run), None, now));
        assert_eq!(
            spec.next_run_after(Some(last_run), None, last_run + 30_000),
            Some(now)
        );
    }

    #[test]
    fn normalize_job_defaults_from_payload() {
        let input = json!({
            "name": "job",
            "schedule": { "kind": "at", "at": "2026-02-04T12:00:00Z" },
            "payload": { "kind": "agentTurn", "message": "hi" }
        });
        let normalized = super::normalize_job_input(&input, true).expect("normalize");
        assert_eq!(
            normalized.get("wakeMode").and_then(|v| v.as_str()),
            Some("next-heartbeat")
        );
        assert_eq!(
            normalized.get("enabled").and_then(|v| v.as_bool()),
            Some(true)
        );
        assert_eq!(
            normalized.get("sessionTarget").and_then(|v| v.as_str()),
            Some("isolated")
        );
        let schedule = normalized
            .get("schedule")
            .and_then(|v| v.as_object())
            .unwrap();
        assert_eq!(schedule.get("kind").and_then(|v| v.as_str()), Some("at"));
        assert!(schedule.get("atMs").is_some());
    }

    #[test]
    fn normalize_wraps_job_payload() {
        let input = json!({
            "job": {
                "name": "wrapped",
                "enabled": "false",
                "schedule": { "everyMs": 60000 },
                "payload": { "kind": "systemEvent", "text": "ping" },
                "sessionTarget": "main",
                "wakeMode": "now"
            }
        });
        let normalized = super::normalize_job_input(&input, true).expect("normalize");
        assert_eq!(
            normalized.get("enabled").and_then(|v| v.as_bool()),
            Some(false)
        );
        let schedule = normalized
            .get("schedule")
            .and_then(|v| v.as_object())
            .unwrap();
        assert_eq!(schedule.get("kind").and_then(|v| v.as_str()), Some("every"));
    }

    #[test]
    fn normalize_schedule_at_string_coerces_at_ms() {
        let input = json!({
            "name": "job",
            "schedule": { "at": "2026-02-04T12:00:00Z" },
            "payload": { "kind": "agentTurn", "message": "hi" }
        });
        let normalized = super::normalize_job_input(&input, true).expect("normalize");
        let schedule = normalized
            .get("schedule")
            .and_then(|v| v.as_object())
            .unwrap();
        assert_eq!(schedule.get("kind").and_then(|v| v.as_str()), Some("at"));
        assert!(schedule.get("atMs").is_some());
        assert!(schedule.get("at").is_none());
    }

    #[test]
    fn normalize_schedule_expr_and_tz() {
        let input = json!({
            "name": "job",
            "schedule": { "expr": "0 * * * * * *", "tz": "America/Los_Angeles" },
            "payload": { "kind": "systemEvent", "text": "ping" }
        });
        let normalized = super::normalize_job_input(&input, true).expect("normalize");
        let schedule = normalized
            .get("schedule")
            .and_then(|v| v.as_object())
            .unwrap();
        assert_eq!(schedule.get("kind").and_then(|v| v.as_str()), Some("cron"));
        assert_eq!(
            schedule.get("cron").and_then(|v| v.as_str()),
            Some("0 * * * * * *")
        );
        assert_eq!(
            schedule.get("timezone").and_then(|v| v.as_str()),
            Some("America/Los_Angeles")
        );
    }

    #[test]
    fn normalize_session_key_trims_and_drops_blank() {
        let with_key = json!({
            "name": "job",
            "schedule": { "kind": "at", "at": "2026-02-04T12:00:00Z" },
            "payload": { "kind": "systemEvent", "text": "ping" },
            "sessionKey": "  agent:main:telegram:group:-100123  "
        });
        let normalized = super::normalize_job_input(&with_key, true).expect("normalize");
        assert_eq!(
            normalized.get("sessionKey").and_then(|v| v.as_str()),
            Some("agent:main:telegram:group:-100123")
        );

        let cleared = json!({
            "name": "job",
            "schedule": { "kind": "at", "at": "2026-02-04T12:00:00Z" },
            "payload": { "kind": "systemEvent", "text": "ping" },
            "sessionKey": "   "
        });
        let normalized = super::normalize_job_input(&cleared, true).expect("normalize");
        assert!(!normalized.contains_key("sessionKey"));
    }

    #[test]
    fn normalize_session_key_patch_preserves_null() {
        let patch = json!({
            "sessionKey": null
        });
        let normalized = super::normalize_job_input(&patch, false).expect("normalize");
        assert!(normalized.get("sessionKey").is_some_and(Value::is_null));
    }

    #[test]
    fn normalize_delivery_from_legacy_payload() {
        let input = json!({
            "name": "job",
            "schedule": { "everyMs": 60000 },
            "payload": {
                "kind": "agentTurn",
                "message": "hi",
                "deliver": true,
                "channel": "Slack",
                "to": "U123",
                "bestEffortDeliver": true
            }
        });
        let normalized = super::normalize_job_input(&input, true).expect("normalize");
        let delivery = normalized
            .get("delivery")
            .and_then(|v| v.as_object())
            .unwrap();
        assert_eq!(
            delivery.get("mode").and_then(|v| v.as_str()),
            Some("announce")
        );
        assert_eq!(
            delivery.get("channel").and_then(|v| v.as_str()),
            Some("slack")
        );
        assert_eq!(delivery.get("to").and_then(|v| v.as_str()), Some("U123"));
        assert_eq!(
            delivery.get("bestEffort").and_then(|v| v.as_bool()),
            Some(true)
        );

        let payload = normalized
            .get("payload")
            .and_then(|v| v.as_object())
            .unwrap();
        assert!(payload.get("deliver").is_none());
        assert!(payload.get("channel").is_none());
        assert!(payload.get("to").is_none());
        assert!(payload.get("bestEffortDeliver").is_none());
    }

    #[test]
    fn normalize_delivery_mode_deliver_to_announce() {
        let input = json!({
            "name": "job",
            "schedule": { "everyMs": 60000 },
            "delivery": { "mode": "deliver", "channel": "Slack", "to": "U123" },
            "payload": { "kind": "agentTurn", "message": "hi" }
        });
        let normalized = super::normalize_job_input(&input, true).expect("normalize");
        let delivery = normalized
            .get("delivery")
            .and_then(|v| v.as_object())
            .unwrap();
        assert_eq!(
            delivery.get("mode").and_then(|v| v.as_str()),
            Some("announce")
        );
        assert_eq!(
            delivery.get("channel").and_then(|v| v.as_str()),
            Some("slack")
        );
        assert_eq!(delivery.get("to").and_then(|v| v.as_str()), Some("U123"));
    }

    #[test]
    fn normalize_delivery_mode_webhook_trims_and_validates_target() {
        let input = json!({
            "name": "job",
            "schedule": { "kind": "at", "at": "2026-02-04T12:00:00Z" },
            "sessionTarget": "main",
            "wakeMode": "now",
            "delivery": { "mode": " WeBhOoK ", "to": "  https://example.invalid/cron-finished  " },
            "payload": { "kind": "systemEvent", "text": "ping" }
        });
        let normalized = super::normalize_job_input(&input, true).expect("normalize");
        let delivery = normalized
            .get("delivery")
            .and_then(|v| v.as_object())
            .unwrap();
        assert_eq!(
            delivery.get("mode").and_then(|v| v.as_str()),
            Some("webhook")
        );
        assert_eq!(
            delivery.get("to").and_then(|v| v.as_str()),
            Some("https://example.invalid/cron-finished")
        );
    }

    #[test]
    fn validate_job_spec_allows_main_webhook_delivery() {
        let input = json!({
            "name": "job",
            "schedule": { "kind": "at", "at": "2026-02-04T12:00:00Z" },
            "sessionTarget": "main",
            "wakeMode": "now",
            "delivery": { "mode": "webhook", "to": "https://example.invalid/cron-finished" },
            "payload": { "kind": "systemEvent", "text": "ping" }
        });
        let normalized = super::normalize_job_input(&input, true).expect("normalize");
        assert!(super::validate_job_spec(&normalized).is_ok());
    }

    #[test]
    fn validate_job_spec_rejects_invalid_webhook_delivery_target() {
        let input = json!({
            "name": "job",
            "schedule": { "kind": "at", "at": "2026-02-04T12:00:00Z" },
            "sessionTarget": "main",
            "wakeMode": "now",
            "delivery": { "mode": "webhook", "to": "ftp://example.invalid/cron-finished" },
            "payload": { "kind": "systemEvent", "text": "ping" }
        });
        let normalized = super::normalize_job_input(&input, true).expect("normalize");
        let err = super::validate_job_spec(&normalized).expect_err("invalid webhook should fail");
        assert!(err
            .to_string()
            .contains("cron webhook delivery requires delivery.to to be a valid http(s) URL"));
    }

    #[test]
    fn load_jobs_keeps_main_webhook_and_drops_main_channel_delivery() {
        let paths = temp_paths();
        let jobs_path = paths.cron_dir.join("jobs.json");
        let jobs = json!({
            "version": 1,
            "jobs": [
                {
                    "id": "keep-webhook",
                    "name": "keep-webhook",
                    "enabled": true,
                    "schedule": { "kind": "at", "atMs": 1000 },
                    "sessionTarget": "main",
                    "wakeMode": "now",
                    "delivery": { "mode": "webhook", "to": "https://example.invalid/cron-finished" },
                    "payload": { "kind": "systemEvent", "text": "ping" },
                    "state": {}
                },
                {
                    "id": "drop-announce",
                    "name": "drop-announce",
                    "enabled": true,
                    "schedule": { "kind": "at", "atMs": 1000 },
                    "sessionTarget": "main",
                    "wakeMode": "now",
                    "delivery": { "mode": "announce", "channel": "slack", "to": "U123" },
                    "payload": { "kind": "systemEvent", "text": "ping" },
                    "state": {}
                }
            ]
        });
        fs::write(&jobs_path, serde_json::to_string_pretty(&jobs).unwrap()).expect("write jobs");

        let loaded = super::load_jobs(&paths).expect("load jobs");
        let keep = loaded
            .iter()
            .find(|entry| entry.get("id").and_then(|v| v.as_str()) == Some("keep-webhook"))
            .expect("keep-webhook");
        let drop = loaded
            .iter()
            .find(|entry| entry.get("id").and_then(|v| v.as_str()) == Some("drop-announce"))
            .expect("drop-announce");

        assert_eq!(
            keep.get("delivery")
                .and_then(|v| v.get("mode"))
                .and_then(|v| v.as_str()),
            Some("webhook")
        );
        assert!(drop.get("delivery").is_none());
    }

    #[test]
    fn job_session_key_prefers_explicit_session_key() {
        let job = super::build_cron_job(&json!({
            "id": "job-1",
            "name": "job",
            "sessionKey": "agent:main:discord:channel:ops",
            "sessionTarget": "main",
            "wakeMode": "next-heartbeat",
            "payload": { "kind": "systemEvent", "text": "ping" }
        }))
        .expect("job");

        assert_eq!(
            super::job_session_key(&job),
            "agent:main:discord:channel:ops".to_string()
        );
    }
}
