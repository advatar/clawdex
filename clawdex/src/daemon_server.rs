use std::path::PathBuf;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc, Arc,
};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use reqwest::blocking::Client;
use serde_json::{json, Map, Value};
use tiny_http::{Method, Response, Server, StatusCode};

use crate::approvals::{
    ApprovalBroker, ApprovalDecision, ResolveApprovalResult, UserInputResolution,
};
use crate::config::{
    merge_config_value, read_config_value, write_config_value, ClawdConfig, ClawdPaths,
};
use crate::cron;
use crate::daemon::{run_daemon_loop, DaemonCommand, DaemonRunResult};
use crate::gateway;
use crate::permissions::{self, PermissionsUpdate};
use crate::plugins;
use crate::task_db::TaskStore;
use crate::tasks::{TaskEngine, TaskRunOptions};
use crate::util::now_ms;

const ADMIN_DASHBOARD_HTML: &str = include_str!("admin_dashboard.html");
#[cfg(unix)]
const DEFAULT_IPC_SOCKET_MODE: u32 = 0o600;

#[derive(Debug)]
struct IpcProxyRequest {
    method: String,
    path: String,
    body: Value,
}

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
    ipc_uds: Option<PathBuf>,
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
    #[cfg(unix)]
    let _ipc_guard = start_ipc_proxy_server(ipc_uds.clone(), bind.to_string(), shutdown.clone())?;
    #[cfg(not(unix))]
    if let Some(path) = ipc_uds.as_ref() {
        eprintln!(
            "[clawdexd] ignoring --ipc-uds {}; unix sockets are unavailable on this platform",
            path.display()
        );
    }

    let server =
        Server::http(bind).map_err(|err| anyhow::anyhow!("bind daemon server {bind}: {err}"))?;
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

#[cfg(unix)]
fn start_ipc_proxy_server(
    ipc_uds: Option<PathBuf>,
    bind: String,
    shutdown: Arc<AtomicBool>,
) -> Result<Option<thread::JoinHandle<()>>> {
    use std::io::ErrorKind;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixListener;

    let Some(socket_path) = ipc_uds else {
        return Ok(None);
    };
    if let Some(parent) = socket_path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create ipc parent {}", parent.display()))?;
    }
    if socket_path.exists() {
        std::fs::remove_file(&socket_path)
            .with_context(|| format!("remove stale ipc socket {}", socket_path.display()))?;
    }
    let listener = UnixListener::bind(&socket_path)
        .with_context(|| format!("bind daemon ipc socket {}", socket_path.display()))?;
    listener
        .set_nonblocking(true)
        .context("set ipc listener nonblocking")?;
    std::fs::set_permissions(
        &socket_path,
        std::fs::Permissions::from_mode(DEFAULT_IPC_SOCKET_MODE),
    )
    .with_context(|| format!("set ipc socket mode {}", socket_path.display()))?;

    let http_base = format!("http://{}", bind.trim_end_matches('/'));
    let handle = thread::spawn(move || {
        while !shutdown.load(Ordering::SeqCst) {
            match listener.accept() {
                Ok((stream, _addr)) => {
                    let base = http_base.clone();
                    thread::spawn(move || {
                        if let Err(err) = serve_ipc_proxy_connection(stream, &base) {
                            eprintln!("[clawdexd][ipc] connection failed: {err}");
                        }
                    });
                }
                Err(err) if err.kind() == ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(100));
                }
                Err(err) => {
                    eprintln!("[clawdexd][ipc] accept failed: {err}");
                    thread::sleep(Duration::from_millis(250));
                }
            }
        }
        let _ = std::fs::remove_file(&socket_path);
    });
    Ok(Some(handle))
}

#[cfg(unix)]
fn serve_ipc_proxy_connection(
    mut stream: std::os::unix::net::UnixStream,
    http_base: &str,
) -> Result<()> {
    use std::io::BufRead;

    let reader = stream.try_clone().context("clone ipc stream")?;
    let mut reader = std::io::BufReader::new(reader);
    loop {
        let mut line = String::new();
        let bytes = reader
            .read_line(&mut line)
            .context("read ipc request line")?;
        if bytes == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let response = match serde_json::from_str::<Value>(trimmed) {
            Ok(request) => handle_ipc_jsonrpc_request(http_base, request),
            Err(err) => Some(jsonrpc_error(
                Value::Null,
                -32700,
                &format!("parse error: {err}"),
            )),
        };
        if let Some(response) = response {
            let line = serde_json::to_string(&response).context("serialize ipc response")?;
            use std::io::Write;
            writeln!(stream, "{line}").context("write ipc response")?;
            stream.flush().ok();
        }
    }
    Ok(())
}

fn handle_ipc_jsonrpc_request(http_base: &str, request: Value) -> Option<Value> {
    let jsonrpc = request.get("jsonrpc").and_then(|v| v.as_str());
    if jsonrpc != Some("2.0") {
        return Some(jsonrpc_error(
            request.get("id").cloned().unwrap_or(Value::Null),
            -32600,
            "invalid jsonrpc version",
        ));
    }

    let id = request.get("id").cloned();
    let method = request.get("method").and_then(|v| v.as_str()).unwrap_or("");
    if method.is_empty() {
        return Some(jsonrpc_error(
            id.unwrap_or(Value::Null),
            -32600,
            "missing method",
        ));
    }

    let result = match method {
        "daemon.request" => {
            let proxy = match parse_ipc_proxy_request(request.get("params")) {
                Ok(proxy) => proxy,
                Err(err) => {
                    return Some(jsonrpc_error(
                        id.unwrap_or(Value::Null),
                        -32602,
                        &err.to_string(),
                    ))
                }
            };
            proxy_http_request(http_base, &proxy)
        }
        "health" => proxy_http_request(
            http_base,
            &IpcProxyRequest {
                method: "GET".to_string(),
                path: "/v1/health".to_string(),
                body: Value::Null,
            },
        ),
        _ => {
            return Some(jsonrpc_error(
                id.unwrap_or(Value::Null),
                -32601,
                "method not found",
            ))
        }
    };

    let Some(id) = id else {
        // JSON-RPC notification: no response payload.
        return None;
    };
    match result {
        Ok(value) => Some(jsonrpc_result(id, value)),
        Err(err) => Some(jsonrpc_error(id, -32000, &err.to_string())),
    }
}

fn parse_ipc_proxy_request(params: Option<&Value>) -> Result<IpcProxyRequest> {
    let params = params.context("params required")?;
    let object = params.as_object().context("params must be an object")?;
    let method = object
        .get("httpMethod")
        .or_else(|| object.get("method"))
        .and_then(|v| v.as_str())
        .context("params.httpMethod required")?
        .trim()
        .to_uppercase();
    let path = object
        .get("path")
        .and_then(|v| v.as_str())
        .context("params.path required")?
        .trim()
        .to_string();

    if path.is_empty() || !path.starts_with('/') {
        anyhow::bail!("params.path must start with '/'");
    }
    if !matches!(method.as_str(), "GET" | "POST" | "PUT" | "PATCH" | "DELETE") {
        anyhow::bail!("unsupported http method: {method}");
    }

    Ok(IpcProxyRequest {
        method,
        path,
        body: object.get("body").cloned().unwrap_or(Value::Null),
    })
}

fn proxy_http_request(http_base: &str, request: &IpcProxyRequest) -> Result<Value> {
    let url = format!("{}{}", http_base.trim_end_matches('/'), request.path);
    let client = Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("build ipc proxy client")?;
    let mut builder = match request.method.as_str() {
        "GET" => client.get(&url),
        "POST" => client.post(&url),
        "PUT" => client.put(&url),
        "PATCH" => client.patch(&url),
        "DELETE" => client.delete(&url),
        _ => anyhow::bail!("unsupported http method: {}", request.method),
    };
    if request.method != "GET" && !request.body.is_null() {
        builder = builder.json(&request.body);
    }
    let response = builder
        .send()
        .with_context(|| format!("proxy request {} {}", request.method, request.path))?;
    let status = response.status().as_u16();
    let content_type = response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.to_string());
    let text = response.text().unwrap_or_default();
    let body = if text.trim().is_empty() {
        Value::Null
    } else {
        serde_json::from_str::<Value>(&text).unwrap_or(Value::String(text))
    };
    Ok(json!({
        "status": status,
        "contentType": content_type,
        "body": body,
    }))
}

fn jsonrpc_result(id: Value, result: Value) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "result": result,
    })
}

fn jsonrpc_error(id: Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
        }
    })
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
        (&Method::Get, "/admin") | (&Method::Get, "/admin/") => Ok(text_response(
            ADMIN_DASHBOARD_HTML,
            "text/html; charset=utf-8",
        )?),
        (&Method::Get, "/v1/admin/overview") => {
            let value = admin_overview(cfg, paths, broker.as_ref())?;
            Ok(json_response(value)?)
        }
        _ if method == Method::Get && url.starts_with("/v1/admin/plugins") => {
            let (path, query) = split_path_query(&url);
            if path != "/v1/admin/plugins" {
                return Ok(Response::from_data(Vec::new()).with_status_code(StatusCode(404)));
            }
            let include_disabled = query_param_bool(query, "includeDisabled").unwrap_or(true);
            let value = plugins::list_plugins_command(
                Some(paths.state_dir.clone()),
                Some(paths.workspace_dir.clone()),
                include_disabled,
            )?;
            Ok(json_response(value)?)
        }
        (&Method::Post, "/v1/admin/plugins/install") => {
            let payload = parse_json_body_or_null(request).context("parse admin plugin install")?;
            let npm = payload
                .get("npm")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let path = payload
                .get("path")
                .and_then(|v| v.as_str())
                .map(PathBuf::from);
            let link = payload
                .get("link")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let source = payload
                .get("source")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let value = plugins::add_plugin_command(
                path,
                npm,
                link,
                source,
                Some(paths.state_dir.clone()),
                Some(paths.workspace_dir.clone()),
            )?;
            Ok(json_response(value)?)
        }
        (&Method::Post, "/v1/admin/plugins/update") => {
            let payload = parse_json_body_or_null(request).context("parse admin plugin update")?;
            let plugin_id = payload
                .get("pluginId")
                .or_else(|| payload.get("plugin_id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let all = payload
                .get("all")
                .and_then(|v| v.as_bool())
                .unwrap_or(plugin_id.is_none());
            let dry_run = payload
                .get("dryRun")
                .or_else(|| payload.get("dry_run"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let value = plugins::update_plugin_command(
                plugin_id,
                all,
                dry_run,
                Some(paths.state_dir.clone()),
                Some(paths.workspace_dir.clone()),
            )?;
            Ok(json_response(value)?)
        }
        _ if method == Method::Post && url.starts_with("/v1/admin/plugins/") => {
            let (path, _) = split_path_query(&url);
            let Some((plugin_id, action)) = parse_admin_plugin_action(path) else {
                return Ok(Response::from_data(Vec::new()).with_status_code(StatusCode(404)));
            };
            let value = match action {
                "enable" => plugins::enable_plugin_command(
                    plugin_id,
                    Some(paths.state_dir.clone()),
                    Some(paths.workspace_dir.clone()),
                )?,
                "disable" => plugins::disable_plugin_command(
                    plugin_id,
                    Some(paths.state_dir.clone()),
                    Some(paths.workspace_dir.clone()),
                )?,
                "remove" => {
                    let payload =
                        parse_json_body_or_null(request).context("parse admin plugin remove")?;
                    let keep_files = payload
                        .get("keepFiles")
                        .or_else(|| payload.get("keep_files"))
                        .and_then(|v| v.as_bool())
                        .unwrap_or(false);
                    plugins::remove_plugin_command(
                        plugin_id,
                        keep_files,
                        Some(paths.state_dir.clone()),
                        Some(paths.workspace_dir.clone()),
                    )?
                }
                _ => return Ok(Response::from_data(Vec::new()).with_status_code(StatusCode(404))),
            };
            Ok(json_response(value)?)
        }
        (&Method::Get, "/v1/admin/gateway/channels") => {
            let value = gateway::list_channels(paths)?;
            Ok(json_response(value)?)
        }
        (&Method::Get, "/v1/admin/permissions") => {
            let value = permissions::get_permissions_command(
                Some(paths.state_dir.clone()),
                Some(paths.workspace_dir.clone()),
            )?;
            Ok(json_response(value)?)
        }
        (&Method::Post, "/v1/admin/permissions") => {
            let payload = parse_json_body_or_null(request).context("parse admin permissions")?;
            let update = parse_permissions_update(&payload)?;
            let value = permissions::set_permissions_command(
                update,
                Some(paths.state_dir.clone()),
                Some(paths.workspace_dir.clone()),
            )?;
            Ok(json_response(value)?)
        }
        (&Method::Get, "/v1/admin/config") => {
            let value = read_config_value(&paths.state_dir)?;
            Ok(json_response(json!({
                "ok": true,
                "stateDir": paths.state_dir.to_string_lossy(),
                "workspaceDir": paths.workspace_dir.to_string_lossy(),
                "config": value,
            }))?)
        }
        (&Method::Post, "/v1/admin/config") => {
            let payload = parse_json_body_or_null(request).context("parse admin config patch")?;
            let patch = payload.get("patch").cloned().unwrap_or(payload);
            if !patch.is_object() {
                anyhow::bail!("config patch must be an object");
            }
            let mut value = read_config_value(&paths.state_dir)?;
            merge_config_value(&mut value, &patch);
            let _ = serde_json::from_value::<ClawdConfig>(value.clone())
                .map_err(|err| anyhow::anyhow!("invalid config update: {err}"))?;
            let path_written = write_config_value(&paths.state_dir, &value)?;
            Ok(json_response(json!({
                "ok": true,
                "configPath": path_written.to_string_lossy(),
                "config": value,
            }))?)
        }
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

            let task_id = payload
                .get("taskId")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let title = payload
                .get("title")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let prompt = payload
                .get("prompt")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let codex_path = payload
                .get("codexPath")
                .and_then(|v| v.as_str())
                .map(PathBuf::from);
            let resume_from_run_id = payload
                .get("resumeFromRunId")
                .or_else(|| payload.get("resume_from_run_id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
            let fork_from_run_id = payload
                .get("forkFromRunId")
                .or_else(|| payload.get("fork_from_run_id"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());

            let engine = TaskEngine::new(cfg.clone(), paths.clone());
            let run = engine.start_task_async_with_broker(
                TaskRunOptions {
                    codex_path,
                    workspace: None,
                    state_dir: None,
                    auto_approve,
                    approval_policy: None,
                    prompt,
                    title,
                    task_id,
                    resume_from_run_id,
                    fork_from_run_id,
                },
                broker.clone(),
            )?;
            Ok(json_response(json!({ "run": run }))?)
        }
        _ if method == Method::Post && url.starts_with("/v1/runs/") && url.ends_with("/cancel") => {
            let run_id = url
                .trim_start_matches("/v1/runs/")
                .trim_end_matches("/cancel")
                .trim_matches('/');
            if run_id.is_empty() {
                return Ok(Response::from_data(Vec::new()).with_status_code(StatusCode(404)));
            }
            let value = crate::tasks::cancel_run_command(
                run_id,
                Some(paths.state_dir.clone()),
                Some(paths.workspace_dir.clone()),
            )?;
            Ok(json_response(value)?)
        }
        _ if method == Method::Get && url.starts_with("/v1/runs") => {
            let (path, query) = split_path_query(&url);
            if path == "/v1/runs" {
                let task_id = query_param_string(query, "taskId");
                let limit = query_param_usize(query, "limit")
                    .unwrap_or(50)
                    .clamp(1, 500);
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
                let limit = query_param_usize(query, "limit")
                    .unwrap_or(200)
                    .clamp(1, 2000);
                let wait_ms = query_param_i64(query, "wait").unwrap_or(0);
                let store = TaskStore::open(paths)?;
                let mut events = match after {
                    None => store.list_events(run_id, Some(limit))?,
                    Some(after) if wait_ms > 0 => {
                        wait_for_events(&store, run_id, after, limit, wait_ms)?
                    }
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
        _ if method == Method::Post
            && url.starts_with("/v1/cron/jobs/")
            && url.ends_with("/run") =>
        {
            let trimmed = url
                .trim_start_matches("/v1/cron/jobs/")
                .trim_end_matches("/run");
            if trimmed.is_empty() {
                return Ok(Response::from_data(Vec::new()).with_status_code(StatusCode(404)));
            }
            let body = read_body(request)?;
            let payload: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
            let mode = payload
                .get("mode")
                .and_then(|v| v.as_str())
                .unwrap_or("due");
            let result = control.run_cron_job(trimmed, mode);
            Ok(json_response(
                json!({ "ok": result.ok, "ran": result.ran, "reason": result.reason }),
            )?)
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
            let mut evidence = Map::new();
            if let Some(reason) = payload.get("reason").and_then(|v| v.as_str()) {
                let trimmed = reason.trim();
                if !trimmed.is_empty() {
                    evidence.insert("reason".to_string(), Value::String(trimmed.to_string()));
                }
            }
            if let Some(confirmation) = payload
                .get("confirmation")
                .or_else(|| payload.get("confirmationText"))
                .and_then(|v| v.as_str())
            {
                let trimmed = confirmation.trim();
                if !trimmed.is_empty() {
                    evidence.insert(
                        "confirmation".to_string(),
                        Value::String(trimmed.to_string()),
                    );
                }
            }
            if let Some(source) = payload.get("source").and_then(|v| v.as_str()) {
                let trimmed = source.trim();
                if !trimmed.is_empty() {
                    evidence.insert("source".to_string(), Value::String(trimmed.to_string()));
                }
            }
            if let Some(extra) = payload.get("evidence").and_then(|v| v.as_object()) {
                for (key, value) in extra {
                    evidence.insert(key.clone(), value.clone());
                }
            }
            if !evidence.is_empty() {
                evidence.insert("decidedAtMs".to_string(), Value::from(now_ms()));
            }
            let result = broker.resolve_approval(
                id,
                decision,
                if evidence.is_empty() {
                    None
                } else {
                    Some(Value::Object(evidence))
                },
            );
            let body = match result {
                ResolveApprovalResult::Resolved => json!({ "ok": true }),
                ResolveApprovalResult::NotFound => json!({
                    "ok": false,
                    "reason": "not_found",
                }),
                ResolveApprovalResult::Rejected { reason } => json!({
                    "ok": false,
                    "reason": "rejected",
                    "message": reason,
                }),
            };
            Ok(json_response(body)?)
        }
        _ if method == Method::Post && url.starts_with("/v1/user-input/") => {
            let id = url.trim_start_matches("/v1/user-input/");
            let body = read_body(request)?;
            let payload: Value = serde_json::from_str(&body).context("parse user input")?;
            let action = payload
                .get("action")
                .and_then(|v| v.as_str())
                .unwrap_or("submit")
                .trim()
                .to_ascii_lowercase();
            let resolution = match action.as_str() {
                "skip" => UserInputResolution::Skip,
                "cancel" => UserInputResolution::Cancel,
                _ => {
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
                    UserInputResolution::Submit(answers)
                }
            };
            let ok = broker.resolve_user_input(id, resolution);
            Ok(json_response(json!({ "ok": ok }))?)
        }
        _ => Ok(Response::from_data(Vec::new()).with_status_code(StatusCode(404))),
    }
}

fn admin_overview(cfg: &ClawdConfig, paths: &ClawdPaths, broker: &ApprovalBroker) -> Result<Value> {
    let store = TaskStore::open(paths)?;
    let tasks = store.list_tasks()?;
    let runs = store.list_runs(None, Some(25))?;
    let approvals = broker.list_pending_approvals();
    let inputs = broker.list_pending_inputs();

    let plugins_value = plugins::list_plugins_command(
        Some(paths.state_dir.clone()),
        Some(paths.workspace_dir.clone()),
        true,
    )
    .unwrap_or_else(|_| json!({ "plugins": [] }));
    let channels_value =
        gateway::list_channels(paths).unwrap_or_else(|_| json!({ "channels": [] }));
    let permissions_value = permissions::get_permissions_command(
        Some(paths.state_dir.clone()),
        Some(paths.workspace_dir.clone()),
    )
    .unwrap_or_else(|_| json!({}));
    let cron_value = cron::list_jobs(paths, true).unwrap_or_else(|_| json!({ "jobs": [] }));
    let config_value = read_config_value(&paths.state_dir).unwrap_or_else(|_| json!({}));

    let plugin_count = plugins_value
        .get("plugins")
        .and_then(|v| v.as_array())
        .map(|v| v.len())
        .unwrap_or(0);
    let channel_count = channels_value
        .get("channels")
        .and_then(|v| v.as_array())
        .map(|v| v.len())
        .unwrap_or(0);

    Ok(json!({
        "ok": true,
        "generatedAtMs": now_ms(),
        "daemon": {
            "stateDir": paths.state_dir.to_string_lossy(),
            "workspaceDir": paths.workspace_dir.to_string_lossy(),
            "cronEnabled": cfg.cron.as_ref().and_then(|c| c.enabled).unwrap_or(true),
            "heartbeatEnabled": cfg.heartbeat.as_ref().and_then(|h| h.enabled).unwrap_or(true),
        },
        "counts": {
            "tasks": tasks.len(),
            "runs": runs.len(),
            "plugins": plugin_count,
            "channels": channel_count,
            "pendingApprovals": approvals.len(),
            "pendingUserInputs": inputs.len(),
        },
        "tasks": tasks,
        "runs": runs,
        "approvals": approvals,
        "userInputs": inputs,
        "plugins": plugins_value.get("plugins").cloned().unwrap_or_else(|| json!([])),
        "gateway": channels_value,
        "permissions": permissions_value,
        "cron": cron_value,
        "memory": config_value.get("memory").cloned().unwrap_or_else(|| json!({})),
    }))
}

fn parse_json_body_or_null(request: &mut tiny_http::Request) -> Result<Value> {
    let body = read_body(request)?;
    if body.trim().is_empty() {
        return Ok(Value::Null);
    }
    let payload: Value = serde_json::from_str(&body).context("parse json body")?;
    Ok(payload)
}

fn parse_admin_plugin_action(path: &str) -> Option<(&str, &str)> {
    let rest = path.strip_prefix("/v1/admin/plugins/")?;
    let mut parts = rest.split('/');
    let plugin_id = parts.next()?.trim();
    let action = parts.next()?.trim();
    if plugin_id.is_empty() || action.is_empty() || parts.next().is_some() {
        return None;
    }
    Some((plugin_id, action))
}

fn parse_permissions_update(payload: &Value) -> Result<PermissionsUpdate> {
    let internet = payload.get("internet").and_then(|v| v.as_bool());
    let read_only = payload
        .get("readOnly")
        .or_else(|| payload.get("read_only"))
        .and_then(|v| v.as_bool());

    let mcp_allow = extract_string_list(
        payload
            .get("mcpAllow")
            .or_else(|| payload.pointer("/mcp/allow")),
    )?;
    let mcp_deny = extract_string_list(
        payload
            .get("mcpDeny")
            .or_else(|| payload.pointer("/mcp/deny")),
    )?;

    let mcp_plugins = if let Some(value) = payload
        .get("mcpPlugins")
        .or_else(|| payload.pointer("/mcp/plugins"))
    {
        let map = value.as_object().context("mcpPlugins must be an object")?;
        let mut out = Vec::new();
        for (plugin_id, enabled) in map {
            let enabled = enabled
                .as_bool()
                .with_context(|| format!("mcpPlugins.{plugin_id} must be boolean"))?;
            out.push((plugin_id.to_string(), enabled));
        }
        Some(out)
    } else {
        None
    };

    Ok(PermissionsUpdate {
        internet,
        read_only,
        mcp_allow,
        mcp_deny,
        mcp_plugins,
    })
}

fn extract_string_list(value: Option<&Value>) -> Result<Option<Vec<String>>> {
    let Some(value) = value else {
        return Ok(None);
    };
    let list = value.as_array().context("expected array of strings")?;
    let mut out = Vec::with_capacity(list.len());
    for entry in list {
        let item = entry.as_str().context("expected string")?.trim();
        if !item.is_empty() {
            out.push(item.to_string());
        }
    }
    Ok(Some(out))
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
    response_with_content_type(data, "application/json")
}

fn text_response(body: &str, content_type: &str) -> Result<Response<std::io::Cursor<Vec<u8>>>> {
    response_with_content_type(body.as_bytes().to_vec(), content_type)
}

fn response_with_content_type(
    data: Vec<u8>,
    content_type: &str,
) -> Result<Response<std::io::Cursor<Vec<u8>>>> {
    let header = tiny_http::Header::from_bytes(b"Content-Type".as_slice(), content_type.as_bytes())
        .map_err(|_| anyhow::anyhow!("invalid content-type header"))?;
    Ok(Response::from_data(data).with_header(header))
}

fn json_error_response(message: &str, status: StatusCode) -> Response<std::io::Cursor<Vec<u8>>> {
    match json_response(json!({ "ok": false, "error": message, "ts": now_ms() })) {
        Ok(resp) => resp.with_status_code(status),
        Err(_) => Response::from_string("error").with_status_code(status),
    }
}

#[cfg(test)]
mod tests {
    use super::{parse_admin_plugin_action, parse_ipc_proxy_request, parse_permissions_update};
    use serde_json::json;

    #[test]
    fn parse_admin_plugin_action_valid() {
        let parsed = parse_admin_plugin_action("/v1/admin/plugins/demo/enable");
        assert_eq!(parsed, Some(("demo", "enable")));
    }

    #[test]
    fn parse_admin_plugin_action_rejects_nested_paths() {
        let parsed = parse_admin_plugin_action("/v1/admin/plugins/demo/enable/extra");
        assert!(parsed.is_none());
    }

    #[test]
    fn parse_permissions_update_accepts_mcp_plugins_object() {
        let payload = json!({
            "internet": false,
            "readOnly": true,
            "mcpPlugins": {
                "alpha": true,
                "beta": false
            }
        });
        let update = parse_permissions_update(&payload).expect("parse");
        assert_eq!(update.internet, Some(false));
        assert_eq!(update.read_only, Some(true));
        assert_eq!(
            update.mcp_plugins,
            Some(vec![
                ("alpha".to_string(), true),
                ("beta".to_string(), false)
            ])
        );
    }

    #[test]
    fn parse_ipc_proxy_request_accepts_valid_request() {
        let payload = json!({
            "httpMethod": "post",
            "path": "/v1/health",
            "body": { "ping": true }
        });
        let parsed = parse_ipc_proxy_request(Some(&payload)).expect("parse ipc request");
        assert_eq!(parsed.method, "POST");
        assert_eq!(parsed.path, "/v1/health");
        assert_eq!(parsed.body, json!({ "ping": true }));
    }

    #[test]
    fn parse_ipc_proxy_request_rejects_relative_path() {
        let payload = json!({
            "httpMethod": "GET",
            "path": "v1/health"
        });
        let err = parse_ipc_proxy_request(Some(&payload)).expect_err("parse should fail");
        assert!(err.to_string().contains("must start with '/'"));
    }
}
