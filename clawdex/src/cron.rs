use std::path::PathBuf;
use std::str::FromStr;

use anyhow::{Context, Result};
use chrono::{TimeZone, Utc};
use chrono_tz::Tz;
use serde_json::{json, Map, Value};
use uuid::Uuid;

use crate::config::ClawdPaths;
use crate::util::{append_json_line, now_ms, read_json_lines, read_json_value, write_json_value};

const JOBS_FILE: &str = "jobs.json";
const RUNS_DIR: &str = "runs";

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

fn record_run(paths: &ClawdPaths, job_id: &str, status: &str, reason: &str) -> Result<Value> {
    let now = now_ms();
    let entry = json!({
        "runId": Uuid::new_v4().to_string(),
        "jobId": job_id,
        "startedAtMs": now,
        "endedAtMs": now,
        "status": status,
        "reason": reason,
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
    let mut jobs = load_jobs(paths)?;
    let mut ran = 0usize;
    let mut skipped = 0usize;
    let mut entries = Vec::new();

    let target_ids: Vec<String> = if let Some(id) = job_id {
        vec![id]
    } else {
        jobs.iter()
            .filter_map(|job| job.get("id").and_then(|v| v.as_str()).map(|s| s.to_string()))
            .collect()
    };

    for target in target_ids {
        let Some(job) = find_job_mut(&mut jobs, &target) else {
            continue;
        };
        let enabled = job.get("enabled").and_then(|v| v.as_bool()).unwrap_or(true);
        if !enabled {
            skipped += 1;
            continue;
        }
        let schedule = job
            .get("schedule")
            .and_then(ScheduleSpec::from_value);
        let last_run = job_last_run_at(&Value::Object(job.clone()));
        let created_at = job_created_at(&Value::Object(job.clone()));
        let due = schedule
            .as_ref()
            .map(|s| s.is_due(last_run, created_at, now))
            .unwrap_or(false);

        if mode == "due" && !due {
            skipped += 1;
            continue;
        }

        let entry = record_run(paths, &target, "skipped", "execution not implemented")?;
        entries.push(entry);
        ran += 1;
        job.insert("lastRunAtMs".to_string(), Value::Number(now.into()));

        let delete_after = job
            .get("deleteAfterRun")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if delete_after {
            *job = Map::new();
        }
    }

    jobs.retain(|job| !job.as_object().map(|m| m.is_empty()).unwrap_or(false));
    save_jobs(paths, &jobs)?;
    Ok(json!({ "ok": true, "ran": ran, "skipped": skipped, "entries": entries }))
}

pub fn run_due_jobs(paths: &ClawdPaths, now: i64) -> Result<Vec<Value>> {
    let mut jobs = load_jobs(paths)?;
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
        if !due {
            continue;
        }
        let job_id = map
            .get("id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| Uuid::new_v4().to_string());
        let entry = record_run(paths, &job_id, "skipped", "execution not implemented")?;
        entries.push(entry);
        map.insert("lastRunAtMs".to_string(), Value::Number(now.into()));
        let delete_after = map
            .get("deleteAfterRun")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        if delete_after {
            map.clear();
        }
    }
    jobs.retain(|job| !job.as_object().map(|m| m.is_empty()).unwrap_or(false));
    save_jobs(paths, &jobs)?;
    Ok(entries)
}
