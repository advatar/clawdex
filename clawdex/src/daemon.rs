use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use codex_app_server_protocol::AskForApproval;
use reqwest::blocking::Client;
use serde_json::{json, Value};

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};

use crate::config::{
    resolve_context_max_input_chars, resolve_heartbeat_enabled, resolve_heartbeat_interval_ms,
    ClawdConfig, ClawdPaths,
};
use crate::cron::{
    build_cron_job, collect_due_jobs, drain_pending_jobs, is_job_due_value, job_prompt,
    load_job_value, mark_job_running, normalize_http_webhook_url, record_run, CronJob,
};
use crate::gateway;
use crate::heartbeat;
use crate::memory;
use crate::runner::{CodexRunner, CodexRunnerConfig};
use crate::sessions;
use crate::task_db::TaskStore;
use crate::util::{apply_text_budget, now_ms};

#[derive(Debug, Clone)]
enum DeliveryMode {
    None,
    Announce,
    Webhook,
}

#[derive(Debug, Clone)]
struct DeliveryPlan {
    mode: DeliveryMode,
    channel: Option<String>,
    to: Option<String>,
    best_effort: bool,
    requested: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum AgentBackendKind {
    Codex,
    Kline,
}

#[derive(Debug, Clone)]
struct ExternalAgentBackend {
    kind: AgentBackendKind,
    url: String,
    token: Option<String>,
    timeout_ms: u64,
}

#[derive(Debug, Clone)]
struct AgentBackendRouting {
    default_agent_id: String,
    backends: HashMap<String, ExternalAgentBackend>,
}

#[derive(Debug, Clone)]
struct AgentTurnOutcome {
    message: String,
    warnings: Vec<String>,
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
    let agent_routing = resolve_agent_backend_routing(&cfg);

    let heartbeat_enabled = resolve_heartbeat_enabled(&cfg);
    let interval = resolve_heartbeat_interval_ms(&cfg);
    let mut next_heartbeat = now_ms() + interval as i64;
    let context_max_input_chars = resolve_context_max_input_chars(&cfg);

    let memory_sync_minutes = cfg
        .memory
        .as_ref()
        .and_then(|m| m.sync.as_ref())
        .and_then(|s| s.interval_minutes)
        .unwrap_or(0);
    let memory_sync_interval_ms = memory_sync_minutes.saturating_mul(60_000);
    let mut next_memory_sync = now_ms() + memory_sync_interval_ms as i64;

    loop {
        if shutdown.load(Ordering::SeqCst) {
            break;
        }
        let now = now_ms();

        if let Some(receiver) = commands.as_ref() {
            drain_daemon_commands(
                &cfg,
                &mut runner,
                &paths,
                approval_policy,
                &workspace_policy,
                &workspace,
                context_max_input_chars,
                receiver,
            );
        }

        // Drain inbound messages from the gateway and run agent turns.
        let inbound = gateway::drain_inbox(&paths)?;
        for entry in inbound {
            handle_incoming_message(&mut runner, &agent_routing, &paths, entry)?;
        }

        // Drain pending jobs (wakeMode = next-heartbeat or manual cron.run)
        let pending_jobs = drain_pending_jobs(&paths)?;
        for job in pending_jobs {
            execute_job(
                &cfg,
                &mut runner,
                &paths,
                &job,
                approval_policy,
                &workspace_policy,
                &workspace,
                context_max_input_chars,
            )?;
        }

        // Execute due jobs
        let (due_jobs, _entries) = collect_due_jobs(&paths, now, "due", None)?;
        for job in due_jobs {
            execute_job(
                &cfg,
                &mut runner,
                &paths,
                &job,
                approval_policy,
                &workspace_policy,
                &workspace,
                context_max_input_chars,
            )?;
        }

        if heartbeat_enabled && now >= next_heartbeat {
            if let Err(err) = execute_heartbeat(&mut runner, &cfg, &paths) {
                eprintln!("[clawdex][heartbeat] tick failed: {err}");
            }
            next_heartbeat = now + interval as i64;
        }

        if memory_sync_interval_ms > 0 && now >= next_memory_sync {
            if let Err(err) = memory::sync_memory_index(&paths, None) {
                eprintln!("[clawdex][memory] sync failed: {err}");
            }
            next_memory_sync = now + memory_sync_interval_ms as i64;
        }

        thread::sleep(Duration::from_millis(500));
    }
    Ok(())
}

fn drain_daemon_commands(
    cfg: &ClawdConfig,
    runner: &mut CodexRunner,
    paths: &ClawdPaths,
    base_approval_policy: AskForApproval,
    base_workspace_policy: &crate::config::WorkspacePolicy,
    base_workspace: &PathBuf,
    context_max_input_chars: Option<usize>,
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
                    cfg,
                    runner,
                    paths,
                    &job_id,
                    &mode,
                    base_approval_policy,
                    base_workspace_policy,
                    base_workspace,
                    context_max_input_chars,
                );
                let _ = respond_to.send(result);
            }
        }
    }
}

fn run_cron_job_now(
    cfg: &ClawdConfig,
    runner: &mut CodexRunner,
    paths: &ClawdPaths,
    job_id: &str,
    mode: &str,
    base_approval_policy: AskForApproval,
    base_workspace_policy: &crate::config::WorkspacePolicy,
    base_workspace: &PathBuf,
    context_max_input_chars: Option<usize>,
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
        cfg,
        runner,
        paths,
        &job,
        base_approval_policy,
        base_workspace_policy,
        base_workspace,
        context_max_input_chars,
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
    cfg: &ClawdConfig,
    runner: &mut CodexRunner,
    paths: &ClawdPaths,
    job: &CronJob,
    base_approval_policy: AskForApproval,
    base_workspace_policy: &crate::config::WorkspacePolicy,
    base_workspace: &PathBuf,
    context_max_input_chars: Option<usize>,
) -> Result<()> {
    let started_at = now_ms();
    let policy = job_policy_overrides(job);
    let approval_policy = policy.approval_policy.unwrap_or(base_approval_policy);
    let (workspace_policy, workspace) =
        apply_workspace_overrides(base_workspace_policy, base_workspace, &policy)?;
    let mut task_run = start_cron_task_run(
        paths,
        job,
        approval_policy,
        &workspace_policy,
        &workspace,
        started_at,
    );
    record_cron_task_event(
        &mut task_run,
        "controller_state",
        json!({
            "state": "plan",
            "phase": "cron_job_ready",
            "jobId": job.id,
            "runAtMs": started_at,
        }),
    );

    let Some(prompt) = job_prompt(job, started_at) else {
        record_run(
            paths,
            &job.id,
            "skipped",
            "missing payload message",
            Some(json!({ "applyState": false })),
        )?;
        record_cron_task_event(
            &mut task_run,
            "cron_job_skipped",
            json!({ "jobId": job.id, "reason": "missing payload message" }),
        );
        record_cron_task_event(
            &mut task_run,
            "controller_state",
            json!({
                "state": "verify",
                "phase": "job_skipped",
                "jobId": job.id,
                "status": "skipped",
                "reason": "missing payload message",
            }),
        );
        finish_cron_task_run(
            &mut task_run,
            "completed",
            "cron_job_finished",
            json!({
                "jobId": job.id,
                "status": "skipped",
                "reason": "missing payload message",
                "endedAtMs": now_ms(),
            }),
        );
        return Ok(());
    };
    let prompt_budget = apply_text_budget(&prompt, context_max_input_chars);
    if prompt_budget.truncated {
        record_cron_task_event(
            &mut task_run,
            "context_budget_applied",
            json!({
                "maxInputChars": prompt_budget.max_chars,
                "originalChars": prompt_budget.original_chars,
                "finalChars": prompt_budget.final_chars,
                "jobId": job.id,
            }),
        );
    }

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
            record_cron_task_event(
                &mut task_run,
                "cron_job_skipped",
                json!({ "jobId": job.id, "reason": "locked" }),
            );
            record_cron_task_event(
                &mut task_run,
                "controller_state",
                json!({
                    "state": "verify",
                    "phase": "job_skipped",
                    "jobId": job.id,
                    "status": "skipped",
                    "reason": "locked",
                }),
            );
            finish_cron_task_run(
                &mut task_run,
                "completed",
                "cron_job_finished",
                json!({
                    "jobId": job.id,
                    "status": "skipped",
                    "reason": "locked",
                    "endedAtMs": now_ms(),
                }),
            );
            return Ok(());
        }
    };

    mark_job_running(paths, &job.id, started_at)?;
    record_cron_task_event(
        &mut task_run,
        "controller_state",
        json!({
            "state": "act",
            "phase": "turn_start",
            "jobId": job.id,
            "sessionTarget": job.session_target,
        }),
    );
    let outcome = if job.session_target == "isolated" {
        runner.run_isolated_with_policy(
            &job.id,
            &prompt_budget.text,
            approval_policy,
            &workspace_policy,
            workspace.clone(),
        )
    } else {
        runner.run_main_with_policy(
            &prompt_budget.text,
            approval_policy,
            &workspace_policy,
            workspace.clone(),
        )
    };
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(err) => {
            let ended_at = now_ms();
            let duration_ms = ended_at.saturating_sub(started_at);
            let error_text = err.to_string();
            let _ = record_run(
                paths,
                &job.id,
                "failed",
                "execution failed",
                Some(json!({
                    "error": error_text,
                    "runAtMs": started_at,
                    "durationMs": duration_ms,
                })),
            );
            record_cron_task_event(&mut task_run, "turn_failed", json!({ "error": error_text }));
            record_cron_task_event(
                &mut task_run,
                "controller_state",
                json!({
                    "state": "verify",
                    "phase": "turn_failed",
                    "jobId": job.id,
                    "status": "failed",
                    "reason": "execution failed",
                    "error": error_text,
                }),
            );
            finish_cron_task_run(
                &mut task_run,
                "failed",
                "cron_job_finished",
                json!({
                    "jobId": job.id,
                    "status": "failed",
                    "reason": "execution failed",
                    "runAtMs": started_at,
                    "endedAtMs": ended_at,
                    "durationMs": duration_ms,
                }),
            );
            return Err(err);
        }
    };

    let ended_at = now_ms();
    let duration_ms = ended_at.saturating_sub(started_at);
    let summary = outcome.message.trim().to_string();
    let mut status = "completed";
    let mut reason = "executed";
    let mut error: Option<String> = None;

    let plan = resolve_delivery_plan(job);
    match plan.mode {
        DeliveryMode::None => {}
        DeliveryMode::Announce => {
            let mut channel = plan.channel.clone();
            let mut to = plan.to.clone();
            let mut account_id: Option<String> = None;
            let mut session_key = job.session_key.clone();

            if channel.as_deref() == Some("last") || to.is_none() {
                if let Some(resolved) = resolve_delivery_target(
                    paths,
                    channel.clone(),
                    to.clone(),
                    job.session_key.clone(),
                ) {
                    channel = Some(resolved.channel);
                    to = Some(resolved.to);
                    account_id = resolved.account_id;
                    if session_key.is_none() {
                        session_key = resolved.session_key;
                    }
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
                    "sessionKey": session_key,
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
        DeliveryMode::Webhook => {
            if !summary.is_empty() {
                if let Some(target_url) = plan.to.as_ref() {
                    let payload = json!({
                        "jobId": job.id,
                        "action": "finished",
                        "status": "ok",
                        "summary": summary,
                        "runAtMs": started_at,
                        "durationMs": duration_ms,
                    });
                    if let Err(err) = post_cron_webhook(cfg, target_url, &payload) {
                        let err_text = err.to_string();
                        error = Some(err_text.clone());
                        if plan.best_effort {
                            status = "skipped";
                            reason = "cron webhook failed (best effort)";
                        } else {
                            status = "delivery_failed";
                            reason = "cron webhook failed";
                        }
                    }
                } else if plan.best_effort {
                    status = "skipped";
                    reason = "no webhook target (best effort)";
                } else {
                    status = "delivery_failed";
                    reason = "no webhook target";
                    error = Some("no webhook target".to_string());
                }
            }
        }
    }

    let error_for_record = error.clone();
    let details = json!({
        "summary": summary,
        "error": error_for_record,
        "runAtMs": started_at,
        "durationMs": duration_ms
    });
    record_run(paths, &job.id, status, reason, Some(details))?;
    record_cron_task_event(
        &mut task_run,
        "turn_completed",
        json!({
            "message": outcome.message,
            "warnings": outcome.warnings,
        }),
    );
    record_cron_task_event(
        &mut task_run,
        "cron_delivery_result",
        json!({
            "requested": plan.requested,
            "bestEffort": plan.best_effort,
            "status": status,
            "reason": reason,
            "error": error,
        }),
    );
    record_cron_task_event(
        &mut task_run,
        "controller_state",
        json!({
            "state": "verify",
            "phase": "turn_completed",
            "jobId": job.id,
            "status": status,
            "reason": reason,
        }),
    );
    let run_status = if status == "delivery_failed" {
        "failed"
    } else {
        "completed"
    };
    finish_cron_task_run(
        &mut task_run,
        run_status,
        "cron_job_finished",
        json!({
            "jobId": job.id,
            "status": status,
            "reason": reason,
            "runAtMs": started_at,
            "endedAtMs": ended_at,
            "durationMs": duration_ms,
        }),
    );

    Ok(())
}

fn start_cron_task_run(
    paths: &ClawdPaths,
    job: &CronJob,
    approval_policy: AskForApproval,
    workspace_policy: &crate::config::WorkspacePolicy,
    workspace: &PathBuf,
    started_at_ms: i64,
) -> Option<(TaskStore, String)> {
    let store = match TaskStore::open(paths) {
        Ok(store) => store,
        Err(err) => {
            eprintln!("[clawdex][cron] failed to open task store: {err}");
            return None;
        }
    };
    let title = cron_task_title(job);
    let task = match store.get_task_by_title(&title) {
        Ok(Some(task)) => task,
        Ok(None) => match store.create_task(&title) {
            Ok(task) => task,
            Err(err) => {
                eprintln!(
                    "[clawdex][cron] failed to create task for {}: {err}",
                    job.id
                );
                return None;
            }
        },
        Err(err) => {
            eprintln!("[clawdex][cron] failed to load task for {}: {err}", job.id);
            return None;
        }
    };
    let run = match store.create_run(
        &task.id,
        "running",
        None,
        Some(cron_sandbox_label(workspace_policy).to_string()),
        Some(format!("{approval_policy:?}")),
    ) {
        Ok(run) => run,
        Err(err) => {
            eprintln!(
                "[clawdex][cron] failed to create task run for {}: {err}",
                job.id
            );
            return None;
        }
    };
    let _ = store.record_event(
        &run.id,
        "cron_job_started",
        &json!({
            "jobId": job.id,
            "jobName": job.name,
            "sessionTarget": job.session_target,
            "workspace": workspace.to_string_lossy(),
            "startedAtMs": started_at_ms,
        }),
    );
    Some((store, run.id))
}

fn record_cron_task_event(task_run: &mut Option<(TaskStore, String)>, kind: &str, payload: Value) {
    if let Some((store, run_id)) = task_run.as_mut() {
        let _ = store.record_event(run_id, kind, &payload);
    }
}

fn finish_cron_task_run(
    task_run: &mut Option<(TaskStore, String)>,
    status: &str,
    event_kind: &str,
    payload: Value,
) {
    if let Some((store, run_id)) = task_run.as_mut() {
        let _ = store.update_run_status(run_id, status);
        let _ = store.record_event(run_id, event_kind, &payload);
    }
}

fn cron_task_title(job: &CronJob) -> String {
    if let Some(name) = job
        .name
        .as_deref()
        .map(str::trim)
        .filter(|name| !name.is_empty())
    {
        format!("[cron:{}] {}", job.id, name)
    } else {
        format!("[cron:{}]", job.id)
    }
}

fn cron_sandbox_label(workspace_policy: &crate::config::WorkspacePolicy) -> &'static str {
    if workspace_policy.read_only {
        "read-only"
    } else if workspace_policy.network_access {
        "workspace-write"
    } else {
        "workspace-write-no-network"
    }
}

fn execute_heartbeat(
    runner: &mut CodexRunner,
    cfg: &ClawdConfig,
    paths: &ClawdPaths,
) -> Result<()> {
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
    let prompt_budget = apply_text_budget(&prompt, resolve_context_max_input_chars(cfg));
    if prompt_budget.truncated {
        eprintln!(
            "[clawdex][heartbeat] prompt truncated by context budget ({} -> {} chars)",
            prompt_budget.original_chars, prompt_budget.final_chars
        );
    }
    let outcome = runner.run_main(&prompt_budget.text)?;
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
    let payload_deliver = payload
        .and_then(|p| p.get("deliver"))
        .and_then(|v| v.as_bool());
    let payload_best_effort = payload
        .and_then(|p| p.get("bestEffortDeliver"))
        .and_then(|v| v.as_bool());

    if let Some(delivery) = job.delivery.as_ref() {
        let mode = match delivery.mode.trim().to_lowercase().as_str() {
            "announce" | "deliver" => DeliveryMode::Announce,
            "webhook" => DeliveryMode::Webhook,
            _ => DeliveryMode::None,
        };
        let best_effort = delivery
            .best_effort
            .or(payload_best_effort)
            .unwrap_or(false);
        let requested = !matches!(mode, DeliveryMode::None);
        let channel = if matches!(mode, DeliveryMode::Announce) {
            delivery
                .channel
                .clone()
                .or(payload_channel)
                .or_else(|| Some("last".to_string()))
        } else {
            None
        };
        let to = if matches!(mode, DeliveryMode::Webhook) {
            delivery
                .to
                .as_deref()
                .and_then(normalize_http_webhook_url)
                .or_else(|| payload_to.as_deref().and_then(normalize_http_webhook_url))
        } else {
            delivery.to.clone().or(payload_to)
        };
        return DeliveryPlan {
            mode,
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
        mode: if requested {
            DeliveryMode::Announce
        } else {
            DeliveryMode::None
        },
        channel,
        to,
        best_effort,
        requested,
    }
}

fn post_cron_webhook(cfg: &ClawdConfig, target_url: &str, payload: &Value) -> Result<()> {
    let client = Client::builder()
        .timeout(Duration::from_millis(10_000))
        .build()
        .context("build cron webhook client")?;
    let mut request = client
        .post(target_url)
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .json(payload);
    if let Some(token) = cfg
        .cron
        .as_ref()
        .and_then(|cron| cron.webhook_token.as_ref())
        .map(|token| token.trim())
        .filter(|token| !token.is_empty())
    {
        request = request.bearer_auth(token);
    }

    let response = request.send().context("send cron webhook request")?;
    if !response.status().is_success() {
        anyhow::bail!("cron webhook delivery failed: HTTP {}", response.status());
    }
    Ok(())
}

struct ResolvedTarget {
    channel: String,
    to: String,
    account_id: Option<String>,
    session_key: Option<String>,
}

fn resolve_delivery_target(
    paths: &ClawdPaths,
    channel: Option<String>,
    to: Option<String>,
    session_key: Option<String>,
) -> Option<ResolvedTarget> {
    let channel_arg = match channel.as_deref() {
        Some("last") => None,
        _ => channel.clone(),
    };
    let args = json!({
        "channel": channel_arg,
        "to": to,
        "sessionKey": session_key,
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
        session_key: resolved
            .get("sessionKey")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string()),
    })
}

fn deliver_heartbeat_response(
    cfg: &ClawdConfig,
    paths: &ClawdPaths,
    response: &str,
) -> Result<bool> {
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
        if let Some(last) = resolve_delivery_target(paths, None, None, None) {
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

fn handle_incoming_message(
    runner: &mut CodexRunner,
    routing: &AgentBackendRouting,
    paths: &ClawdPaths,
    entry: serde_json::Value,
) -> Result<()> {
    let text = entry
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if text.is_empty() {
        return Ok(());
    }
    let session_key = resolve_inbound_session_key(&entry);
    let _ = sessions::append_session_message(paths, &session_key, "user", text);
    let outcome = run_incoming_turn(runner, routing, &session_key, text)?;
    for warning in &outcome.warnings {
        eprintln!("[clawdex][agent] session {} warning: {}", session_key, warning);
    }
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
    let agent_id = entry
        .get("agentId")
        .or_else(|| entry.get("agent_id"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(|v| v.to_string());

    if let Some(key) = entry.get("sessionKey").and_then(|v| v.as_str()) {
        let key = key.trim();
        if key.is_empty() {
            return "agent:main:main".to_string();
        }
        if key.starts_with("agent:") || agent_id.is_none() {
            return key.to_string();
        }
        return format!("agent:{}:{key}", agent_id.unwrap_or_default());
    }
    let channel = entry.get("channel").and_then(|v| v.as_str()).unwrap_or("");
    let from = entry.get("from").and_then(|v| v.as_str()).unwrap_or("");
    if !channel.is_empty() && !from.is_empty() {
        if let Some(agent_id) = agent_id {
            return format!("agent:{agent_id}:{channel}:{from}");
        }
        return format!("{channel}:{from}");
    }
    if let Some(agent_id) = agent_id {
        return format!("agent:{agent_id}:main");
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

fn resolve_agent_backend_routing(cfg: &ClawdConfig) -> AgentBackendRouting {
    let default_agent_id = cfg
        .agents
        .as_ref()
        .and_then(|agents| agents.default_agent_id.as_deref())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("main")
        .to_string();

    let mut backends = HashMap::new();
    if let Some(configured) = cfg
        .agents
        .as_ref()
        .and_then(|agents| agents.backends.as_ref())
    {
        for (agent_id, backend) in configured {
            let normalized_agent_id = agent_id.trim();
            if normalized_agent_id.is_empty() {
                continue;
            }
            let kind = match backend
                .kind
                .as_deref()
                .map(str::trim)
                .unwrap_or("codex")
                .to_ascii_lowercase()
                .as_str()
            {
                "kline" => AgentBackendKind::Kline,
                _ => AgentBackendKind::Codex,
            };
            if kind == AgentBackendKind::Codex {
                continue;
            }
            let Some(url) = backend
                .url
                .as_deref()
                .map(str::trim)
                .filter(|value| !value.is_empty())
            else {
                eprintln!(
                    "[clawdex][agents] backend for agent `{}` is missing url; using codex",
                    normalized_agent_id
                );
                continue;
            };
            let timeout_ms = backend.timeout_ms.unwrap_or(30_000).clamp(1_000, 120_000);
            let token = resolve_external_backend_token(backend);
            backends.insert(
                normalized_agent_id.to_string(),
                ExternalAgentBackend {
                    kind,
                    url: url.to_string(),
                    token,
                    timeout_ms,
                },
            );
        }
    }

    AgentBackendRouting {
        default_agent_id,
        backends,
    }
}

fn resolve_external_backend_token(backend: &crate::config::AgentBackendConfig) -> Option<String> {
    let from_env = backend
        .token_env
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|env_name| std::env::var(env_name).ok())
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty());
    from_env.or_else(|| {
        backend
            .token
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(|value| value.to_string())
    })
}

fn run_incoming_turn(
    runner: &mut CodexRunner,
    routing: &AgentBackendRouting,
    session_key: &str,
    text: &str,
) -> Result<AgentTurnOutcome> {
    let agent_id = resolve_agent_id_for_session(session_key, &routing.default_agent_id);
    if let Some(external) = routing.backends.get(&agent_id) {
        return run_external_agent_turn(external, &agent_id, session_key, text);
    }

    let outcome = if session_key == "agent:main:main" {
        runner.run_main(text)?
    } else {
        runner.run_isolated(session_key, text)?
    };
    Ok(AgentTurnOutcome {
        message: outcome.message,
        warnings: outcome.warnings,
    })
}

fn resolve_agent_id_for_session(session_key: &str, default_agent_id: &str) -> String {
    let trimmed = session_key.trim();
    if let Some(rest) = trimmed.strip_prefix("agent:") {
        if let Some((agent_id, _)) = rest.split_once(':') {
            let agent_id = agent_id.trim();
            if !agent_id.is_empty() {
                return agent_id.to_string();
            }
        }
    }
    default_agent_id.to_string()
}

fn run_external_agent_turn(
    backend: &ExternalAgentBackend,
    agent_id: &str,
    session_key: &str,
    message: &str,
) -> Result<AgentTurnOutcome> {
    match backend.kind {
        AgentBackendKind::Kline => run_kline_turn(backend, agent_id, session_key, message),
        AgentBackendKind::Codex => anyhow::bail!("external codex backend is not supported"),
    }
}

fn run_kline_turn(
    backend: &ExternalAgentBackend,
    agent_id: &str,
    session_key: &str,
    message: &str,
) -> Result<AgentTurnOutcome> {
    let endpoint = format!(
        "{}/v1/agent/turn",
        backend.url.trim_end_matches('/').trim_end_matches('\\')
    );
    let client = Client::builder()
        .timeout(Duration::from_millis(backend.timeout_ms))
        .build()
        .context("build kline backend client")?;

    let mut request = client.post(&endpoint).json(&json!({
        "agentId": agent_id,
        "sessionKey": session_key,
        "message": message,
    }));
    if let Some(token) = backend.token.as_deref() {
        request = request.bearer_auth(token);
    }

    let response = request
        .send()
        .with_context(|| format!("kline backend request failed: {}", endpoint))?;
    let status = response.status();
    let body = response
        .json::<Value>()
        .unwrap_or_else(|_| json!({ "ok": status.is_success() }));
    if !status.is_success() {
        let detail = body
            .get("error")
            .and_then(|v| v.as_str())
            .or_else(|| body.get("message").and_then(|v| v.as_str()))
            .unwrap_or("kline backend request failed");
        anyhow::bail!("kline backend HTTP {}: {}", status, detail);
    }

    let Some(message) = extract_backend_message(&body) else {
        anyhow::bail!("kline backend response missing message");
    };
    let warnings = extract_backend_warnings(&body);
    Ok(AgentTurnOutcome { message, warnings })
}

fn extract_backend_message(value: &Value) -> Option<String> {
    for candidate in [
        value.get("message"),
        value.get("response"),
        value.get("text"),
        value.get("result").and_then(|v| v.get("message")),
        value.get("result").and_then(|v| v.get("response")),
        value.get("result").and_then(|v| v.get("text")),
    ] {
        if let Some(message) = candidate.and_then(|v| v.as_str()) {
            let trimmed = message.trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    None
}

fn extract_backend_warnings(value: &Value) -> Vec<String> {
    let mut warnings = Vec::new();
    if let Some(items) = value
        .get("warnings")
        .or_else(|| value.get("result").and_then(|v| v.get("warnings")))
        .and_then(|v| v.as_array())
    {
        for item in items {
            if let Some(text) = item.as_str().map(str::trim).filter(|text| !text.is_empty()) {
                warnings.push(text.to_string());
            }
        }
    }
    warnings
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn resolve_inbound_session_key_namespaces_with_agent_id() {
        let key = resolve_inbound_session_key(&json!({
            "agentId": "kline",
            "channel": "telegram",
            "from": "1234"
        }));
        assert_eq!(key, "agent:kline:telegram:1234");
    }

    #[test]
    fn resolve_inbound_session_key_prefixes_existing_non_namespaced_key() {
        let key = resolve_inbound_session_key(&json!({
            "agentId": "kline",
            "sessionKey": "telegram:1234"
        }));
        assert_eq!(key, "agent:kline:telegram:1234");
    }

    #[test]
    fn resolve_agent_id_for_session_prefers_session_namespace() {
        let agent_id = resolve_agent_id_for_session("agent:kline:telegram:1234", "main");
        assert_eq!(agent_id, "kline");
    }

    #[test]
    fn resolve_agent_id_for_session_falls_back_to_default() {
        let agent_id = resolve_agent_id_for_session("telegram:1234", "kline");
        assert_eq!(agent_id, "kline");
    }
}
