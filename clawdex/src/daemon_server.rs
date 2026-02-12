use std::path::PathBuf;
use std::sync::{Arc, atomic::{AtomicBool, Ordering}, mpsc};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use tiny_http::{Method, Response, Server, StatusCode};

use crate::approvals::{ApprovalBroker, ApprovalDecision};
use crate::config::{ClawdConfig, ClawdPaths};
use crate::cron;
use crate::daemon::{DaemonCommand, DaemonRunResult, run_daemon_loop};
use crate::task_db::TaskStore;
use crate::tasks::{TaskEngine, TaskRunOptions};
use crate::util::now_ms;

struct DaemonControl {
    sender: mpsc::Sender<DaemonCommand>,
}

impl DaemonControl {
    fn run_cron_job(&self, job_id: &str, mode: &str) -> DaemonRunResult {
        let (respond_to, receiver) = mpsc::channel();
        let cmd = DaemonCommand::RunCronJob {
            job_id: job_id.to_string(),
            mode: mode.to_string(),
            respond_to,
        };
        if self.sender.send(cmd).is_err() {
            return DaemonRunResult {
                ok: false,
                ran: false,
                reason: Some("daemon unavailable".to_string()),
            };
        }
        match receiver.recv() {
            Ok(result) => result,
            Err(err) => DaemonRunResult {
                ok: false,
                ran: false,
                reason: Some(err.to_string()),
            },
        }
    }
}

pub fn run_daemon_server(
    cfg: ClawdConfig,
    paths: ClawdPaths,
    codex_path_override: Option<PathBuf>,
    bind: &str,
) -> Result<()> {
    let (command_tx, command_rx) = std::sync::mpsc::channel::<DaemonCommand>();
    let broker = Arc::new(ApprovalBroker::new(paths.clone()));
    let shutdown = Arc::new(AtomicBool::new(false));
    let daemon_shutdown = shutdown.clone();
    let cfg_clone = cfg.clone();
    let paths_clone = paths.clone();
    let codex_path_clone = codex_path_override.clone();

    thread::spawn(move || {
        let _ = run_daemon_loop(
            cfg_clone,
            paths_clone,
            codex_path_clone,
            daemon_shutdown,
            Some(command_rx),
        );
    });

    let control = DaemonControl { sender: command_tx };
    let server = Server::http(bind)
        .map_err(|err| anyhow::anyhow!("bind daemon server {bind}: {err}"))?;
    for mut request in server.incoming_requests() {
        let response = match handle_request(&cfg, &paths, broker.clone(), &control, &mut request) {
            Ok(resp) => resp,
            Err(err) => json_error_response(&err.to_string(), StatusCode(500)),
        };
        let _ = request.respond(response);
    }
    shutdown.store(true, Ordering::SeqCst);
    Ok(())
}

fn handle_request(
    cfg: &ClawdConfig,
    paths: &ClawdPaths,
    broker: Arc<ApprovalBroker>,
    control: &DaemonControl,
    request: &mut tiny_http::Request,
) -> Result<Response<std::io::Cursor<Vec<u8>>>> {
    let method = request.method().clone();
    let url = request.url().to_string();

    match (&method, url.as_str()) {
        (&Method::Get, "/v1/health") => Ok(json_response(json!({ "ok": true }))?),
        (&Method::Get, "/v1/tasks") => {
            let store = TaskStore::open(paths)?;
            let tasks = store.list_tasks()?;
            Ok(json_response(json!({ "tasks": tasks }))?)
        }
        (&Method::Post, "/v1/tasks") => {
            let body = read_body(request)?;
            let payload: Value = serde_json::from_str(&body).context("parse task create")?;
            let title = payload
                .get("title")
                .and_then(|v| v.as_str())
                .context("title required")?;
            let store = TaskStore::open(paths)?;
            let task = store.create_task(title)?;
            Ok(json_response(json!({ "task": task }))?)
        }
        (&Method::Post, "/v1/runs") => {
            let body = read_body(request)?;
            let payload: Value = serde_json::from_str(&body).context("parse run request")?;

            let auto_approve = payload
                .get("autoApprove")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);

            let task_id = payload.get("taskId").and_then(|v| v.as_str()).map(|s| s.to_string());
            let title = payload.get("title").and_then(|v| v.as_str()).map(|s| s.to_string());
            let prompt = payload.get("prompt").and_then(|v| v.as_str()).map(|s| s.to_string());
            let codex_path = payload.get("codexPath").and_then(|v| v.as_str()).map(PathBuf::from);

            let engine = TaskEngine::new(cfg.clone(), paths.clone());
            let run = engine.start_task_async_with_broker(TaskRunOptions {
                codex_path,
                workspace: None,
                state_dir: None,
                auto_approve,
                approval_policy: None,
                prompt,
                title,
                task_id,
            }, broker.clone())?;
            Ok(json_response(json!({ "run": run }))?)
        }
        _ if method == Method::Get && url.starts_with("/v1/runs") => {
            let (path, query) = split_path_query(&url);
            if path == "/v1/runs" {
                let task_id = query_param_string(query, "taskId");
                let limit = query_param_usize(query, "limit").unwrap_or(50).clamp(1, 500);
                let store = TaskStore::open(paths)?;
                let runs = store.list_runs(task_id.as_deref(), Some(limit))?;
                return Ok(json_response(json!({ "runs": runs }))?);
            }

            let Some(rest) = path.strip_prefix("/v1/runs/") else {
                return Ok(Response::from_data(Vec::new()).with_status_code(StatusCode(404)));
            };
            let rest = rest.trim_matches('/');
            if rest.is_empty() {
                return Ok(Response::from_data(Vec::new()).with_status_code(StatusCode(404)));
            }
            if let Some(run_id) = rest.strip_suffix("/events") {
                let run_id = run_id.trim_matches('/');
                if run_id.is_empty() {
                    return Ok(Response::from_data(Vec::new()).with_status_code(StatusCode(404)));
                }
                let after = query_param_i64(query, "after");
                let limit = query_param_usize(query, "limit").unwrap_or(200).clamp(1, 2000);
                let wait_ms = query_param_i64(query, "wait").unwrap_or(0);
                let store = TaskStore::open(paths)?;
                let mut events = match after {
                    None => store.list_events(run_id, Some(limit))?,
                    Some(after) if wait_ms > 0 => wait_for_events(&store, run_id, after, limit, wait_ms)?,
                    Some(after) => store.list_events_after(run_id, after, limit)?,
                };
                if after.is_none() {
                    // list_events returns newest-first; return chronological for UI consumption.
                    events.reverse();
                }
                return Ok(json_response(json!({ "events": events }))?);
            }

            let run_id = rest;
            let store = TaskStore::open(paths)?;
            if let Some(run) = store.get_run(run_id)? {
                return Ok(json_response(json!({ "run": run }))?);
            }
            Ok(Response::from_data(Vec::new()).with_status_code(StatusCode(404)))
        }
        _ if method == Method::Get && url.starts_with("/v1/cron/jobs") => {
            let (path, query) = split_path_query(&url);
            if path != "/v1/cron/jobs" {
                return Ok(Response::from_data(Vec::new()).with_status_code(StatusCode(404)));
            }
            let include_disabled = query_param_bool(query, "includeDisabled").unwrap_or(true);
            let value = cron::list_jobs(paths, include_disabled)?;
            Ok(json_response(value)?)
        }
        (&Method::Post, "/v1/cron/jobs") => {
            let body = read_body(request)?;
            let payload: Value = serde_json::from_str(&body).context("parse cron job create")?;
            let job = cron::add_job(paths, &payload)?;
            Ok(json_response(json!({ "job": job }))?)
        }
        _ if method == Method::Post
            && url.starts_with("/v1/cron/jobs/")
            && !url.ends_with("/run") =>
        {
            let id = url.trim_start_matches("/v1/cron/jobs/");
            let body = read_body(request)?;
            let payload: Value = serde_json::from_str(&body).context("parse cron job patch")?;
            let patch = payload.get("patch").cloned().unwrap_or(payload);
            let update_args = json!({
                "id": id,
                "patch": patch
            });
            let job = cron::update_job(paths, &update_args)?;
            Ok(json_response(json!({ "job": job }))?)
        }
        _ if method == Method::Post && url.starts_with("/v1/cron/jobs/") && url.ends_with("/run") => {
            let trimmed = url.trim_start_matches("/v1/cron/jobs/").trim_end_matches("/run");
            if trimmed.is_empty() {
                return Ok(Response::from_data(Vec::new()).with_status_code(StatusCode(404)));
            }
            let body = read_body(request)?;
            let payload: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let mode = payload.get("mode").and_then(|v| v.as_str()).unwrap_or("due");
            let result = control.run_cron_job(trimmed, mode);
            Ok(json_response(json!({ "ok": result.ok, "ran": result.ran, "reason": result.reason }))?)
        }
        (&Method::Get, "/v1/approvals") => {
            let approvals = broker.list_pending_approvals();
            let inputs = broker.list_pending_inputs();
            Ok(json_response(json!({
                "approvals": approvals,
                "userInputs": inputs
            }))?)
        }
        _ if method == Method::Post && url.starts_with("/v1/approvals/") => {
            let id = url.trim_start_matches("/v1/approvals/");
            let body = read_body(request)?;
            let payload: Value = serde_json::from_str(&body).context("parse approval decision")?;
            let decision = payload
                .get("decision")
                .and_then(|v| v.as_str())
                .unwrap_or("decline");
            let decision = match decision.to_lowercase().as_str() {
                "accept" | "approved" => ApprovalDecision::Accept,
                "cancel" => ApprovalDecision::Cancel,
                _ => ApprovalDecision::Decline,
            };
            let ok = broker.resolve_approval(id, decision);
            Ok(json_response(json!({ "ok": ok }))?)
        }
        _ if method == Method::Post && url.starts_with("/v1/user-input/") => {
            let id = url.trim_start_matches("/v1/user-input/");
            let body = read_body(request)?;
            let payload: Value = serde_json::from_str(&body).context("parse user input")?;
            let answers_value = payload
                .get("answers")
                .and_then(|v| v.as_object())
                .cloned()
                .unwrap_or_default();
            let mut answers = std::collections::HashMap::new();
            for (key, value) in answers_value {
                let list = value
                    .get("answers")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let mut strings = Vec::new();
                for entry in list {
                    if let Some(text) = entry.as_str() {
                        strings.push(text.to_string());
                    }
                }
                answers.insert(
                    key,
                    codex_app_server_protocol::ToolRequestUserInputAnswer { answers: strings },
                );
            }
            let ok = broker.resolve_user_input(id, answers);
            Ok(json_response(json!({ "ok": ok }))?)
        }
        _ => {
            Ok(Response::from_data(Vec::new()).with_status_code(StatusCode(404)))
        }
    }
}

fn wait_for_events(
    store: &TaskStore,
    run_id: &str,
    after: i64,
    limit: usize,
    wait_ms: i64,
) -> Result<Vec<crate::task_db::TaskEvent>> {
    let deadline = Instant::now() + Duration::from_millis(wait_ms.max(0) as u64);
    loop {
        let events = store.list_events_after(run_id, after, limit)?;
        if !events.is_empty() {
            return Ok(events);
        }
        if Instant::now() >= deadline {
            return Ok(Vec::new());
        }
        thread::sleep(Duration::from_millis(200));
    }
}

fn read_body(request: &mut tiny_http::Request) -> Result<String> {
    let mut body = String::new();
    request
        .as_reader()
        .read_to_string(&mut body)
        .context("read request body")?;
    Ok(body)
}

fn split_path_query(url: &str) -> (&str, Option<&str>) {
    if let Some(idx) = url.find('?') {
        (&url[..idx], Some(&url[idx + 1..]))
    } else {
        (url, None)
    }
}

fn query_param_i64(query: Option<&str>, key: &str) -> Option<i64> {
    let query = query?;
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        let k = parts.next()?.trim();
        let v = parts.next().unwrap_or("").trim();
        if k == key {
            return v.parse::<i64>().ok();
        }
    }
    None
}

fn query_param_string(query: Option<&str>, key: &str) -> Option<String> {
    let query = query?;
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        let k = parts.next()?.trim();
        let v = parts.next().unwrap_or("").trim();
        if k == key {
            let trimmed = v.trim();
            if trimmed.is_empty() {
                return None;
            }
            return Some(trimmed.to_string());
        }
    }
    None
}

fn query_param_usize(query: Option<&str>, key: &str) -> Option<usize> {
    let query = query?;
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        let k = parts.next()?.trim();
        let v = parts.next().unwrap_or("").trim();
        if k == key {
            return v.parse::<usize>().ok();
        }
    }
    None
}

fn query_param_bool(query: Option<&str>, key: &str) -> Option<bool> {
    let query = query?;
    for pair in query.split('&') {
        let mut parts = pair.splitn(2, '=');
        let k = parts.next()?.trim();
        let v = parts.next().unwrap_or("").trim().to_lowercase();
        if k == key {
            return match v.as_str() {
                "1" | "true" | "yes" | "on" => Some(true),
                "0" | "false" | "no" | "off" => Some(false),
                _ => None,
            };
        }
    }
    None
}

fn json_response(value: Value) -> Result<Response<std::io::Cursor<Vec<u8>>>> {
    let data = serde_json::to_vec(&value)?;
    let header = tiny_http::Header::from_bytes(
        &b"Content-Type"[..],
        &b"application/json"[..],
    )
    .map_err(|_| anyhow::anyhow!("invalid content-type header"))?;
    Ok(Response::from_data(data).with_header(header))
}

fn json_error_response(message: &str, status: StatusCode) -> Response<std::io::Cursor<Vec<u8>>> {
    match json_response(json!({ "ok": false, "error": message, "ts": now_ms() })) {
        Ok(resp) => resp.with_status_code(status),
        Err(_) => Response::from_string("error").with_status_code(status),
    }
}
