use std::path::PathBuf;

use anyhow::Result;
use serde_json::{json, Value};

use chrono::{Local, TimeZone, Timelike, Utc};
use chrono_tz::Tz;

use crate::config::{ClawdConfig, ClawdPaths};
use crate::util::{append_json_line, now_ms};

const DEFAULT_HEARTBEAT_PROMPT: &str = "Read HEARTBEAT.md if it exists (workspace context). Follow it strictly. Do not infer or repeat old tasks from prior chats. If nothing needs attention, reply HEARTBEAT_OK.";
const DEFAULT_HEARTBEAT_ACK_MAX_CHARS: usize = 300;

fn heartbeat_log_path(paths: &ClawdPaths) -> PathBuf {
    paths.state_dir.join("heartbeat.jsonl")
}

fn heartbeat_payload(_cfg: &ClawdConfig, paths: &ClawdPaths, reason: &str) -> Result<Value> {
    let heartbeat_path = paths.workspace_dir.join("HEARTBEAT.md");
    if !heartbeat_path.exists() {
        return Ok(json!({
            "status": "queued",
            "reason": reason,
        }));
    }
    let contents = std::fs::read_to_string(&heartbeat_path).unwrap_or_default();
    if is_effectively_empty(&contents) {
        return Ok(json!({
            "status": "skipped",
            "reason": "empty heartbeat",
        }));
    }

    Ok(json!({
        "status": "queued",
        "reason": reason,
        "message": "heartbeat execution not implemented",
    }))
}

pub fn wake(cfg: &ClawdConfig, paths: &ClawdPaths, reason: Option<String>) -> Result<Value> {
    let reason = reason.unwrap_or_else(|| "manual".to_string());
    let now = now_ms();
    let payload = if !is_within_active_hours(cfg, now) {
        json!({
            "status": "skipped",
            "reason": "outside active hours",
        })
    } else {
        heartbeat_payload(cfg, paths, &reason)?
    };
    let entry = json!({
        "timestampMs": now,
        "reason": reason,
        "payload": payload,
    });
    append_json_line(&heartbeat_log_path(paths), &entry)?;
    Ok(entry)
}

// Daemon loop moved to daemon.rs

pub fn is_within_active_hours(cfg: &ClawdConfig, now_ms: i64) -> bool {
    let active = cfg.heartbeat.as_ref().and_then(|h| h.active_hours.as_ref());
    let Some(active) = active else {
        return true;
    };
    let start = parse_time_minutes(active.start.as_deref(), false);
    let end = parse_time_minutes(active.end.as_deref(), true);
    let Some(start) = start else {
        return true;
    };
    let Some(end) = end else {
        return true;
    };
    if start == end {
        return true;
    }
    let tz = resolve_active_hours_timezone(active.timezone.as_deref());
    let minutes = resolve_minutes_in_zone(now_ms, tz);
    let Some(minutes) = minutes else {
        return true;
    };
    if end > start {
        minutes >= start && minutes < end
    } else {
        minutes >= start || minutes < end
    }
}

pub fn resolve_prompt(cfg: &ClawdConfig) -> String {
    let prompt = cfg
        .heartbeat
        .as_ref()
        .and_then(|h| h.prompt.as_ref())
        .map(|s| s.trim().to_string())
        .unwrap_or_default();
    if prompt.is_empty() {
        DEFAULT_HEARTBEAT_PROMPT.to_string()
    } else {
        prompt
    }
}

pub fn resolve_ack_max_chars(cfg: &ClawdConfig) -> usize {
    cfg.heartbeat
        .as_ref()
        .and_then(|h| h.ack_max_chars)
        .unwrap_or(DEFAULT_HEARTBEAT_ACK_MAX_CHARS)
}

fn resolve_active_hours_timezone(raw: Option<&str>) -> Option<Tz> {
    let trimmed = raw.unwrap_or("").trim();
    if trimmed.is_empty() || trimmed.eq_ignore_ascii_case("user") || trimmed.eq_ignore_ascii_case("local") {
        return None;
    }
    Some(trimmed.parse::<Tz>().unwrap_or(chrono_tz::UTC))
}

fn parse_time_minutes(raw: Option<&str>, allow_24: bool) -> Option<i32> {
    let raw = raw?.trim();
    if raw.is_empty() {
        return None;
    }
    let mut parts = raw.split(':');
    let hour: i32 = parts.next()?.parse().ok()?;
    let minute: i32 = parts.next()?.parse().ok()?;
    if minute < 0 || minute > 59 {
        return None;
    }
    if hour == 24 {
        if allow_24 && minute == 0 {
            return Some(24 * 60);
        }
        return None;
    }
    if hour < 0 || hour > 23 {
        return None;
    }
    Some(hour * 60 + minute)
}

fn resolve_minutes_in_zone(now_ms: i64, tz: Option<Tz>) -> Option<i32> {
    let utc = Utc.timestamp_millis_opt(now_ms).single()?;
    if let Some(tz) = tz {
        let local = utc.with_timezone(&tz);
        return Some(local.hour() as i32 * 60 + local.minute() as i32);
    }
    let local = utc.with_timezone(&Local);
    Some(local.hour() as i32 * 60 + local.minute() as i32)
}

fn is_effectively_empty(content: &str) -> bool {
    for line in content.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if trimmed.starts_with('#') {
            let rest = trimmed.trim_start_matches('#');
            if rest.is_empty() || rest.starts_with(char::is_whitespace) {
                continue;
            }
        }
        if let Some(first) = trimmed.chars().next() {
            if first == '-' || first == '*' || first == '+' {
                let rest = trimmed[1..].trim();
                if rest.is_empty() {
                    continue;
                }
                if rest.starts_with('[') && rest.ends_with(']') {
                    let inner = rest[1..rest.len() - 1].trim();
                    if inner.is_empty() || inner.eq_ignore_ascii_case("x") {
                        continue;
                    }
                }
            }
        }
        return false;
    }
    true
}
