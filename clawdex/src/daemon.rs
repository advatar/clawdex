use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use codex_app_server_protocol::AskForApproval;
use serde_json::json;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

use crate::config::{resolve_heartbeat_enabled, resolve_heartbeat_interval_ms, ClawdConfig, ClawdPaths};
use crate::cron::{
    build_cron_job, collect_due_jobs, drain_pending_jobs, is_job_due_value, job_prompt,
    load_job_value, mark_job_running, record_run, CronJob,
};
use crate::gateway;
use crate::heartbeat;
use crate::sessions;
use crate::runner::{CodexRunner, CodexRunnerConfig};
use crate::util::now_ms;

#[derive(Debug, Clone)]
struct DeliveryPlan {
    channel: Option<String>,
    to: Option<String>,
    best_effort: bool,
    requested: bool,
}

pub fn run_daemon(
    cfg: ClawdConfig,
    paths: ClawdPaths,
    codex_path_override: Option<PathBuf>,
) -> Result<()> {
    run_daemon_loop(
        cfg,
        paths,
        codex_path_override,
        Arc::new(AtomicBool::new(false)),
        None,
    )
}

#[derive(Debug, Clone)]
pub struct DaemonRunResult {
    pub ok: bool,
    pub ran: bool,
    pub reason: Option<String>,
}

pub enum DaemonCommand {
    RunCronJob {
        job_id: String,
        mode: String,
        respond_to: mpsc::Sender<DaemonRunResult>,
    },
}

pub fn run_daemon_loop(
    cfg: ClawdConfig,
    paths: ClawdPaths,
    codex_path_override: Option<PathBuf>,
    shutdown: Arc<AtomicBool>,
    commands: Option<mpsc::Receiver<DaemonCommand>>,
) -> Result<()> {
    let codex_path = resolve_codex_path(&cfg, codex_path_override)?;
    let workspace = paths.workspace_dir.clone();
    let workspace_policy = paths.workspace_policy.clone();

    let approval_policy = resolve_approval_policy(&cfg);
    let runner_cfg = CodexRunnerConfig {
        codex_path,
        codex_home: paths.state_dir.join("codex"),
        workspace: workspace.clone(),
        workspace_policy: workspace_policy.clone(),
        approval_policy,
        config_overrides: resolve_codex_overrides(&cfg),
    };
    let mut runner = CodexRunner::start(runner_cfg)?;

    let heartbeat_enabled = resolve_heartbeat_enabled(&cfg);
    let interval = resolve_heartbeat_interval_ms(&cfg);
    let mut next_heartbeat = now_ms() + interval as i64;

    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let now = now_ms();

        if let Some(receiver) = commands.as_ref() {
            drain_daemon_commands(
                &mut runner,
                &paths,
                approval_policy,
                &workspace_policy,
                &workspace,
                receiver,
            );
        }

        // Drain inbound messages from the gateway and run Codex turns.
        let inbound = gateway::drain_inbox(&paths)?;
        for entry in inbound {
            handle_incoming_message(&mut runner, &paths, entry)?;
        }

        // Drain pending jobs (wakeMode = next-heartbeat or manual cron.run)
        let pending_jobs = drain_pending_jobs(&paths)?;
        for job in pending_jobs {
            execute_job(
                &mut runner,
                &paths,
                &job,
                approval_policy,
                &workspace_policy,
                &workspace,
            )?;
        }

        // Execute due jobs
        let (due_jobs, _entries) = collect_due_jobs(&paths, now, "due", None)?;
        for job in due_jobs {
            execute_job(
                &mut runner,
                &paths,
                &job,
                approval_policy,
                &workspace_policy,
                &workspace,
            )?;
        }

        if heartbeat_enabled && now >= next_heartbeat {
            execute_heartbeat(&mut runner, &cfg, &paths)?;
            next_heartbeat = now + interval as i64;
        }

        thread::sleep(Duration::from_millis(500));
    }
    Ok(())
}

fn drain_daemon_commands(
    runner: &mut CodexRunner,
    paths: &ClawdPaths,
    base_approval_policy: AskForApproval,
    base_workspace_policy: &crate::config::WorkspacePolicy,
    base_workspace: &PathBuf,
    receiver: &mpsc::Receiver<DaemonCommand>,
) {
    while let Ok(cmd) = receiver.try_recv() {
        match cmd {
            DaemonCommand::RunCronJob {
                job_id,
                mode,
                respond_to,
            } => {
                let result = run_cron_job_now(
                    runner,
                    paths,
                    &job_id,
                    &mode,
                    base_approval_policy,
                    base_workspace_policy,
                    base_workspace,
                );
                let _ = respond_to.send(result);
            }
        }
    }
}

fn run_cron_job_now(
    runner: &mut CodexRunner,
    paths: &ClawdPaths,
    job_id: &str,
    mode: &str,
    base_approval_policy: AskForApproval,
    base_workspace_policy: &crate::config::WorkspacePolicy,
    base_workspace: &PathBuf,
) -> DaemonRunResult {
    let now = now_ms();
    let job_value = match load_job_value(paths, job_id) {
        Ok(Some(job)) => job,
        Ok(None) => {
            return DaemonRunResult {
                ok: false,
                ran: false,
                reason: Some("not-found".to_string()),
            }
        }
        Err(err) => {
            return DaemonRunResult {
                ok: false,
                ran: false,
                reason: Some(err.to_string()),
            }
        }
    };

    let forced = mode.eq_ignore_ascii_case("force");
    if !is_job_due_value(&job_value, now, forced) {
        return DaemonRunResult {
            ok: true,
            ran: false,
            reason: Some("not-due".to_string()),
        };
    }

    let Some(job) = build_cron_job(&job_value) else {
        return DaemonRunResult {
            ok: false,
            ran: false,
            reason: Some("invalid-job".to_string()),
        };
    };

    match execute_job(
        runner,
        paths,
        &job,
        base_approval_policy,
        base_workspace_policy,
        base_workspace,
    ) {
        Ok(()) => DaemonRunResult {
            ok: true,
            ran: true,
            reason: None,
        },
        Err(err) => DaemonRunResult {
            ok: false,
            ran: false,
            reason: Some(err.to_string()),
        },
    }
}

fn execute_job(
    runner: &mut CodexRunner,
    paths: &ClawdPaths,
    job: &CronJob,
    base_approval_policy: AskForApproval,
    base_workspace_policy: &crate::config::WorkspacePolicy,
    base_workspace: &PathBuf,
) -> Result<()> {
    let started_at = now_ms();
    let Some(prompt) = job_prompt(job, started_at) else {
        record_run(
            paths,
            &job.id,
            "skipped",
            "missing payload message",
            Some(json!({ "applyState": false })),
        )?;
        return Ok(());
    };

    let _lock = match acquire_job_lock(paths, &job.id)? {
        Some(lock) => lock,
        None => {
            record_run(
                paths,
                &job.id,
                "skipped",
                "locked",
                Some(json!({ "applyState": false })),
            )?;
            return Ok(());
        }
    };

    mark_job_running(paths, &job.id, started_at)?;

    let policy = job_policy_overrides(job);
    let approval_policy = policy.approval_policy.unwrap_or(base_approval_policy);
    let (workspace_policy, workspace) =
        apply_workspace_overrides(base_workspace_policy, base_workspace, &policy)?;

    let outcome = if job.session_target == "isolated" {
        runner.run_isolated_with_policy(&job.id, &prompt, approval_policy, &workspace_policy, workspace.clone())?
    } else {
        runner.run_main_with_policy(&prompt, approval_policy, &workspace_policy, workspace.clone())?
    };

    let ended_at = now_ms();
    let duration_ms = ended_at.saturating_sub(started_at);
    let summary = outcome.message.trim().to_string();
    let mut status = "completed";
    let mut reason = "executed";
    let mut error: Option<String> = None;

    let plan = resolve_delivery_plan(job);
    if plan.requested {
        let mut channel = plan.channel.clone();
        let mut to = plan.to.clone();
        let mut account_id: Option<String> = None;

        if channel.as_deref() == Some("last") || to.is_none() {
            if let Some(resolved) = resolve_delivery_target(paths, channel.clone(), to.clone()) {
                channel = Some(resolved.channel);
                to = Some(resolved.to);
                account_id = resolved.account_id;
            }
        }

        if channel.is_none() || to.is_none() {
            if plan.best_effort {
                status = "skipped";
                reason = "no delivery target (best effort)";
            } else {
                status = "delivery_failed";
                reason = "no delivery target";
                error = Some("no delivery target".to_string());
            }
        } else {
            let args = json!({
                "channel": channel,
                "to": to,
                "accountId": account_id,
                "text": summary,
                "bestEffort": plan.best_effort,
                "idempotencyKey": format!("cron:{}:{}", job.id, started_at),
            });
            if let Err(err) = gateway::send_message(paths, &args) {
                let err_text = err.to_string();
                error = Some(err_text.clone());
                if plan.best_effort {
                    status = "skipped";
                    reason = "message.send failed (best effort)";
                } else {
                    status = "delivery_failed";
                    reason = "message.send failed";
                }
            }
        }
    }

    let details = json!({
        "summary": summary,
        "error": error,
        "runAtMs": started_at,
        "durationMs": duration_ms
    });
    record_run(paths, &job.id, status, reason, Some(details))?;

    Ok(())
}

fn execute_heartbeat(runner: &mut CodexRunner, cfg: &ClawdConfig, paths: &ClawdPaths) -> Result<()> {
    let entry = heartbeat::wake(cfg, paths, Some("interval".to_string()))?;
    let status = entry
        .get("payload")
        .and_then(|p| p.get("status"))
        .and_then(|v| v.as_str())
        .unwrap_or("skipped");
    if status != "queued" {
        return Ok(());
    }

    let prompt = heartbeat::resolve_prompt(cfg);
    let outcome = runner.run_main(&prompt)?;
    let response = outcome.message.trim().to_string();
    let _ = deliver_heartbeat_response(cfg, paths, &response)?;
    Ok(())
}

fn resolve_delivery_plan(job: &CronJob) -> DeliveryPlan {
    let payload = job.payload.as_object();
    let payload_channel = payload
        .and_then(|p| p.get("channel"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty());
    let payload_to = payload
        .and_then(|p| p.get("to"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let payload_deliver = payload.and_then(|p| p.get("deliver")).and_then(|v| v.as_bool());
    let payload_best_effort = payload
        .and_then(|p| p.get("bestEffortDeliver"))
        .and_then(|v| v.as_bool());

    if let Some(delivery) = job.delivery.as_ref() {
        let normalized_mode = match delivery.mode.to_lowercase().as_str() {
            "announce" => "announce".to_string(),
            "deliver" => "announce".to_string(),
            "none" => "none".to_string(),
            other => other.to_string(),
        };
        let channel = delivery
            .channel
            .clone()
            .or(payload_channel)
            .or_else(|| Some("last".to_string()));
        let to = delivery.to.clone().or(payload_to);
        let requested = normalized_mode == "announce";
        let best_effort = delivery
            .best_effort
            .or(payload_best_effort)
            .unwrap_or(false);
        return DeliveryPlan {
            channel,
            to,
            best_effort,
            requested,
        };
    }

    let legacy_mode = match payload_deliver {
        Some(true) => Some("explicit"),
        Some(false) => Some("off"),
        None => None,
    };
    let requested = match legacy_mode {
        Some("explicit") => true,
        Some("off") => false,
        _ => payload_to.is_some() || job.to.is_some(),
    };
    let channel = payload_channel
        .or(job.channel.clone())
        .or_else(|| Some("last".to_string()));
    let to = payload_to.or(job.to.clone());
    let best_effort = payload_best_effort.unwrap_or(job.best_effort);
    DeliveryPlan {
        channel,
        to,
        best_effort,
        requested,
    }
}

struct ResolvedTarget {
    channel: String,
    to: String,
    account_id: Option<String>,
}

fn resolve_delivery_target(
    paths: &ClawdPaths,
    channel: Option<String>,
    to: Option<String>,
) -> Option<ResolvedTarget> {
    let channel_arg = match channel.as_deref() {
        Some("last") => None,
        _ => channel.clone(),
    };
    let args = json!({
        "channel": channel_arg,
        "to": to
    });
    let resolved = gateway::resolve_target(paths, &args).ok()?;
    if resolved.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        return None;
    }
    Some(ResolvedTarget {
        channel: resolved.get("channel")?.as_str()?.to_string(),
        to: resolved.get("to")?.as_str()?.to_string(),
        account_id: resolved
            .get("accountId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    })
}

fn deliver_heartbeat_response(cfg: &ClawdConfig, paths: &ClawdPaths, response: &str) -> Result<bool> {
    let trimmed = response.trim();
    if trimmed.is_empty() || trimmed == "HEARTBEAT_OK" {
        return Ok(false);
    }

    let max_chars = heartbeat::resolve_ack_max_chars(cfg);
    let deliver_text = if max_chars == 0 {
        String::new()
    } else {
        trimmed.chars().take(max_chars).collect::<String>()
    };
    if deliver_text.trim().is_empty() {
        return Ok(false);
    }

    let mut channel = cfg
        .heartbeat
        .as_ref()
        .and_then(|h| h.delivery.as_ref())
        .and_then(|d| d.channel.clone());
    let mut to = cfg
        .heartbeat
        .as_ref()
        .and_then(|h| h.delivery.as_ref())
        .and_then(|d| d.to.clone());
    let mut account_id = cfg
        .heartbeat
        .as_ref()
        .and_then(|h| h.delivery.as_ref())
        .and_then(|d| d.account_id.clone());

    if channel.is_none() || to.is_none() {
        if let Some(last) = resolve_delivery_target(paths, None, None) {
            channel = channel.or(Some(last.channel));
            to = to.or(Some(last.to));
            account_id = account_id.or(last.account_id);
        }
    }

    let Some(channel) = channel else {
        return Ok(false);
    };
    let Some(to) = to else {
        return Ok(false);
    };

    let args = json!({
        "sessionKey": "agent:main:main",
        "channel": channel,
        "to": to,
        "accountId": account_id,
        "text": deliver_text,
        "idempotencyKey": format!("heartbeat:{}", now_ms()),
    });
    let _ = gateway::send_message(paths, &args);
    Ok(true)
}

/// Test helper for validating heartbeat delivery behavior.
pub fn deliver_heartbeat_response_for_test(
    cfg: &ClawdConfig,
    paths: &ClawdPaths,
    response: &str,
) -> Result<bool> {
    deliver_heartbeat_response(cfg, paths, response)
}

fn handle_incoming_message(runner: &mut CodexRunner, paths: &ClawdPaths, entry: serde_json::Value) -> Result<()> {
    let text = entry.get("text").and_then(|v| v.as_str()).unwrap_or("").trim();
    if text.is_empty() {
        return Ok(());
    }
    let session_key = resolve_inbound_session_key(&entry);
    let _ = sessions::append_session_message(paths, &session_key, "user", text);
    let outcome = if session_key == "agent:main:main" {
        runner.run_main(text)?
    } else {
        runner.run_isolated(&session_key, text)?
    };
    let response = outcome.message.trim();
    if response.is_empty() {
        return Ok(());
    }
    let _ = sessions::append_session_message(paths, &session_key, "assistant", response);
    let args = json!({
        "sessionKey": session_key,
        "text": response,
        "idempotencyKey": format!("inbox:{}:{}", now_ms(), session_key),
    });
    let _ = gateway::send_message(paths, &args);
    Ok(())
}

fn resolve_inbound_session_key(entry: &serde_json::Value) -> String {
    if let Some(key) = entry.get("sessionKey").and_then(|v| v.as_str()) {
        return key.to_string();
    }
    let channel = entry.get("channel").and_then(|v| v.as_str()).unwrap_or("");
    let from = entry.get("from").and_then(|v| v.as_str()).unwrap_or("");
    if !channel.is_empty() && !from.is_empty() {
        return format!("{channel}:{from}");
    }
    "agent:main:main".to_string()
}

fn resolve_codex_path(cfg: &ClawdConfig, override_path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = override_path {
        return Ok(path);
    }
    if let Ok(env) = std::env::var("CLAWDEX_CODEX_PATH") {
        if !env.trim().is_empty() {
            return Ok(PathBuf::from(env));
        }
    }
    if let Some(codex) = cfg.codex.as_ref().and_then(|c| c.path.as_ref()) {
        return Ok(PathBuf::from(codex));
    }
    Ok(PathBuf::from("codex"))
}

fn resolve_approval_policy(cfg: &ClawdConfig) -> AskForApproval {
    let raw = cfg
        .codex
        .as_ref()
        .and_then(|c| c.approval_policy.as_ref())
        .cloned()
        .unwrap_or_else(|| "on-request".to_string());
    parse_approval_policy(&raw)
}

fn resolve_codex_overrides(cfg: &ClawdConfig) -> Vec<String> {
    cfg.codex
        .as_ref()
        .and_then(|c| c.config_overrides.clone())
        .unwrap_or_default()
}

fn parse_approval_policy(raw: &str) -> AskForApproval {
    match raw.to_lowercase().as_str() {
        "never" => AskForApproval::Never,
        "on-failure" | "onfailure" => AskForApproval::OnFailure,
        "unless-trusted" | "unlesstrusted" | "untrusted" => AskForApproval::UnlessTrusted,
        _ => AskForApproval::OnRequest,
    }
}

#[derive(Default)]
struct JobPolicyOverrides {
    approval_policy: Option<AskForApproval>,
    read_only: Option<bool>,
    network_access: Option<bool>,
    allowed_roots: Option<Vec<PathBuf>>,
    workspace: Option<PathBuf>,
}

fn job_policy_overrides(job: &CronJob) -> JobPolicyOverrides {
    let mut overrides = JobPolicyOverrides::default();
    let Some(policy) = job.policy.as_ref().and_then(|value| value.as_object()) else {
        return overrides;
    };

    if let Some(raw) = policy
        .get("approvalPolicy")
        .or_else(|| policy.get("approval_policy"))
        .and_then(|v| v.as_str())
    {
        overrides.approval_policy = Some(parse_approval_policy(raw));
    }

    if let Some(mode) = policy
        .get("sandboxMode")
        .or_else(|| policy.get("sandbox_mode"))
        .and_then(|v| v.as_str())
    {
        if mode.eq_ignore_ascii_case("read-only") || mode.eq_ignore_ascii_case("readonly") {
            overrides.read_only = Some(true);
        } else if mode.eq_ignore_ascii_case("workspace-write")
            || mode.eq_ignore_ascii_case("workspace")
            || mode.eq_ignore_ascii_case("write")
        {
            overrides.read_only = Some(false);
        }
    }

    if let Some(read_only) = policy
        .get("readOnly")
        .or_else(|| policy.get("read_only"))
        .and_then(|v| v.as_bool())
    {
        overrides.read_only = Some(read_only);
    }

    if let Some(network) = policy
        .get("networkAccess")
        .or_else(|| policy.get("network_access"))
        .or_else(|| policy.get("internet"))
        .and_then(|v| v.as_bool())
    {
        overrides.network_access = Some(network);
    }

    if let Some(roots) = policy
        .get("allowedRoots")
        .or_else(|| policy.get("allowed_roots"))
        .and_then(|v| v.as_array())
    {
        let parsed: Vec<PathBuf> = roots
            .iter()
            .filter_map(|v| v.as_str())
            .map(PathBuf::from)
            .collect();
        if !parsed.is_empty() {
            overrides.allowed_roots = Some(parsed);
        }
    }

    if let Some(workspace) = policy.get("workspace").and_then(|v| v.as_str()) {
        overrides.workspace = Some(PathBuf::from(workspace));
    }

    overrides
}

fn apply_workspace_overrides(
    base_policy: &crate::config::WorkspacePolicy,
    base_workspace: &PathBuf,
    overrides: &JobPolicyOverrides,
) -> Result<(crate::config::WorkspacePolicy, PathBuf)> {
    let mut policy = base_policy.clone();
    let mut workspace = base_workspace.clone();

    if let Some(read_only) = overrides.read_only {
        policy.read_only = read_only;
    }
    if let Some(network_access) = overrides.network_access {
        policy.network_access = network_access;
    }
    if let Some(allowed_roots) = overrides.allowed_roots.clone() {
        policy.allowed_roots = allowed_roots;
    }
    if let Some(override_workspace) = overrides.workspace.clone() {
        workspace = override_workspace;
        if overrides.allowed_roots.is_none() {
            policy.allowed_roots = vec![workspace.clone()];
        }
    }

    Ok((policy, workspace))
}

struct JobLock {
    path: PathBuf,
}

impl Drop for JobLock {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn acquire_job_lock(paths: &ClawdPaths, job_id: &str) -> Result<Option<JobLock>> {
    let locks_dir = paths.cron_dir.join("locks");
    fs::create_dir_all(&locks_dir)?;
    let path = locks_dir.join(format!("{job_id}.lock"));
    match OpenOptions::new().write(true).create_new(true).open(&path) {
        Ok(mut file) => {
            let _ = writeln!(file, "{}", now_ms());
            Ok(Some(JobLock { path }))
        }
        Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Ok(None),
        Err(err) => Err(err.into()),
    }
}
