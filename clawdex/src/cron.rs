use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use chrono_tz::Tz;
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
    pub session_target: String,
    pub wake_mode: String,
    pub payload: Value,
    pub policy: Option<Value>,
    pub deliver: bool,
    pub channel: Option<String>,
    pub to: Option<String>,
    pub best_effort: bool,
    pub delete_after_run: bool,
}

#[derive(Debug, Clone)]
struct ScheduleSpec {
    kind: String,
    at_ms: Option<i64>,
    every_ms: Option<i64>,
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
        let cron = obj.get("cron").and_then(|v| v.as_str()).map(|s| s.to_string());
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
            cron,
            tz,
        })
    }

    fn next_run_after(&self, last_run_at_ms: Option<i64>, created_at_ms: Option<i64>, now: i64) -> Option<i64> {
        match self.kind.as_str() {
            "at" => {
                let at_ms = self.at_ms?;
                if now < at_ms {
                    Some(at_ms)
                } else {
                    None
                }
            }
            "every" => {
                let every_ms = self.every_ms?;
                let base = last_run_at_ms.or(created_at_ms).unwrap_or(now);
                if base > now {
                    return Some(base);
                }
                let elapsed = now.saturating_sub(base);
                let intervals = elapsed / every_ms + 1;
                Some(base + intervals * every_ms)
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

    fn is_due(&self, last_run_at_ms: Option<i64>, created_at_ms: Option<i64>, now: i64) -> bool {
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
                let base = last_run_at_ms.or(created_at_ms).unwrap_or(now);
                now.saturating_sub(base) >= every_ms
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

fn jobs_path(paths: &ClawdPaths) -> PathBuf {
    paths.cron_dir.join(JOBS_FILE)
}

fn runs_path(paths: &ClawdPaths, job_id: &str) -> PathBuf {
    paths.cron_dir.join(RUNS_DIR).join(format!("{job_id}.jsonl"))
}

fn pending_path(paths: &ClawdPaths) -> PathBuf {
    paths.cron_dir.join(PENDING_FILE)
}

fn load_jobs(paths: &ClawdPaths) -> Result<Vec<Value>> {
    let path = jobs_path(paths);
    let Some(value) = read_json_value(&path)? else {
        return Ok(Vec::new());
    };
    match value {
        Value::Array(items) => Ok(items),
        _ => anyhow::bail!("cron jobs file is not an array"),
    }
}

fn save_jobs(paths: &ClawdPaths, jobs: &[Value]) -> Result<()> {
    let path = jobs_path(paths);
    write_json_value(&path, &Value::Array(jobs.to_vec()))
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

fn job_created_at(job: &Value) -> Option<i64> {
    job.get("createdAtMs")
        .or_else(|| job.get("created_at_ms"))
        .and_then(|v| v.as_i64())
}

fn job_last_run_at(job: &Value) -> Option<i64> {
    job.get("lastRunAtMs")
        .or_else(|| job.get("last_run_at_ms"))
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

fn build_cron_job(job: &Value) -> Option<CronJob> {
    let id = job
        .get("id")
        .or_else(|| job.get("jobId"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())?;
    let name = job.get("name").and_then(|v| v.as_str()).map(|s| s.to_string());
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
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let delete_after_run = job
        .get("deleteAfterRun")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    Some(CronJob {
        id,
        name,
        session_target,
        wake_mode,
        payload,
        policy,
        deliver,
        channel,
        to,
        best_effort,
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
    let filtered: Vec<Value> = if include_disabled {
        jobs
    } else {
        jobs.into_iter().filter(job_enabled).collect()
    };
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
            let next = schedule.next_run_after(job_last_run_at(job), job_created_at(job), now);
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
    let mut map = args
        .as_object()
        .cloned()
        .context("cron.add expects an object")?;

    let _id = ensure_job_id(&mut map);
    let now = now_ms();
    map.entry("createdAtMs".to_string())
        .or_insert_with(|| Value::Number(now.into()));
    map.insert("updatedAtMs".to_string(), Value::Number(now.into()));
    map.entry("enabled".to_string())
        .or_insert_with(|| Value::Bool(true));

    let value = Value::Object(map);
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
    let patch_map = patch
        .as_object()
        .cloned()
        .context("cron.update patch must be an object")?;
    let now = now_ms();

    let job = find_job_mut(&mut jobs, &job_id).context("job not found")?;
    for (key, value) in patch_map {
        job.insert(key, value);
    }
    job.insert("updatedAtMs".to_string(), Value::Number(now.into()));
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
    let removed = before.saturating_sub(jobs.len());
    save_jobs(paths, &jobs)?;
    Ok(json!({ "ok": true, "removed": removed }))
}

pub fn runs(paths: &ClawdPaths, args: &Value) -> Result<Value> {
    let job_id = job_id_from_args(args).context("cron.runs requires jobId or id")?;
    let limit = args.get("limit").and_then(|v| v.as_u64()).map(|v| v as usize);
    let entries = read_json_lines(&runs_path(paths, &job_id), limit)?;
    Ok(json!({ "entries": entries }))
}

pub fn record_run(
    paths: &ClawdPaths,
    job_id: &str,
    status: &str,
    reason: &str,
    details: Option<Value>,
) -> Result<Value> {
    let now = now_ms();
    let entry = json!({
        "runId": Uuid::new_v4().to_string(),
        "jobId": job_id,
        "startedAtMs": now,
        "endedAtMs": now,
        "status": status,
        "reason": reason,
        "details": details,
    });
    append_json_line(&runs_path(paths, job_id), &entry)?;
    Ok(entry)
}

pub fn run_jobs(paths: &ClawdPaths, args: &Value) -> Result<Value> {
    let mode = args
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("due");
    let job_id = job_id_from_args(args);
    let now = now_ms();
    let (queued_jobs, entries) = collect_due_jobs(paths, now, mode, job_id)?;
    Ok(json!({
        "ok": true,
        "queued": queued_jobs.len(),
        "entries": entries
    }))
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

    for job in &mut jobs {
        let Some(map) = job.as_object_mut() else { continue };
        let enabled = map.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
        if !enabled {
            continue;
        }
        let schedule = map
            .get("schedule")
            .and_then(ScheduleSpec::from_value);
        let last_run = map.get("lastRunAtMs").and_then(|v| v.as_i64());
        let created_at = map.get("createdAtMs").and_then(|v| v.as_i64());
        let due = schedule
            .as_ref()
            .map(|s| s.is_due(last_run, created_at, now))
            .unwrap_or(false);

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

        map.insert("lastRunAtMs".to_string(), Value::Number(now.into()));
        map.insert("updatedAtMs".to_string(), Value::Number(now.into()));

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
            if cron_job.delete_after_run {
                map.clear();
            }
        } else {
            let entry = record_run(paths, &job_id, "skipped", "invalid job", None)?;
            entries.push(entry);
        }
    }

    jobs.retain(|job| !job.as_object().map(|m| m.is_empty()).unwrap_or(false));
    save_jobs(paths, &jobs)?;
    Ok((queued, entries))
}

#[cfg(test)]
mod tests {
    use super::ScheduleSpec;
    use chrono::{TimeZone, Utc};
    use serde_json::json;

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
        assert_eq!(spec.next_run_after(None, None, at_ms), None);
    }

    #[test]
    fn every_schedule_due_and_next_run() {
        let created_at = ms(2026, 2, 4, 12, 0, 0);
        let spec = ScheduleSpec::from_value(&json!({
            "kind": "every",
            "everyMs": 60_000
        }))
        .expect("schedule spec");

        assert!(!spec.is_due(None, Some(created_at), created_at + 30_000));
        assert!(spec.is_due(None, Some(created_at), created_at + 60_000));
        assert_eq!(
            spec.next_run_after(None, Some(created_at), created_at + 30_000),
            Some(created_at + 60_000)
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
}
