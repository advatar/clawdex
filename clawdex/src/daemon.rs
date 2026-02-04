use std::path::PathBuf;
use std::thread;
use std::time::Duration;

use anyhow::Result;
use codex_app_server_protocol::AskForApproval;
use serde_json::json;

use crate::config::{resolve_heartbeat_enabled, resolve_heartbeat_interval_ms, ClawdConfig, ClawdPaths};
use crate::cron::{collect_due_jobs, drain_pending_jobs, job_prompt, job_session_key, record_run};
use crate::gateway;
use crate::heartbeat;
use crate::runner::{CodexRunner, CodexRunnerConfig};
use crate::util::now_ms;

pub fn run_daemon(
    cfg: ClawdConfig,
    paths: ClawdPaths,
    codex_path_override: Option<PathBuf>,
) -> Result<()> {
    let codex_path = resolve_codex_path(&cfg, codex_path_override)?;
    let workspace = paths.workspace_dir.clone();
    let workspace_policy = paths.workspace_policy.clone();

    let approval_policy = resolve_approval_policy(&cfg);
    let runner_cfg = CodexRunnerConfig {
        codex_path,
        codex_home: paths.state_dir.join("codex"),
        workspace,
        workspace_policy,
        approval_policy,
        config_overrides: resolve_codex_overrides(&cfg),
    };
    let mut runner = CodexRunner::start(runner_cfg)?;

    let heartbeat_enabled = resolve_heartbeat_enabled(&cfg);
    let interval = resolve_heartbeat_interval_ms(&cfg);
    let mut next_heartbeat = now_ms() + interval as i64;

    loop {
        let now = now_ms();

        // Drain pending jobs (wakeMode = next-heartbeat or manual cron.run)
        let pending_jobs = drain_pending_jobs(&paths)?;
        for job in pending_jobs {
            execute_job(&mut runner, &paths, &job)?;
        }

        // Execute due jobs
        let (due_jobs, _entries) = collect_due_jobs(&paths, now, "due", None)?;
        for job in due_jobs {
            execute_job(&mut runner, &paths, &job)?;
        }

        if heartbeat_enabled && now >= next_heartbeat {
            execute_heartbeat(&mut runner, &paths)?;
            next_heartbeat = now + interval as i64;
        }

        thread::sleep(Duration::from_millis(500));
    }
}

fn execute_job(runner: &mut CodexRunner, paths: &ClawdPaths, job: &crate::cron::CronJob) -> Result<()> {
    let now = now_ms();
    let Some(prompt) = job_prompt(job, now) else {
        record_run(paths, &job.id, "skipped", "missing payload message", None)?;
        return Ok(());
    };

    let outcome = if job.session_target == "isolated" {
        runner.run_isolated(&job.id, &prompt)?
    } else {
        runner.run_main(&prompt)?
    };

    let summary = outcome.message.trim().to_string();
    record_run(
        paths,
        &job.id,
        "completed",
        "executed",
        Some(json!({ "summary": summary })),
    )?;

    if should_deliver(job) {
        let args = json!({
            "sessionKey": job_session_key(job),
            "channel": job.channel,
            "to": job.to,
            "text": summary,
            "bestEffortDeliver": job.best_effort,
            "idempotencyKey": format!("cron:{}:{}", job.id, now),
        });
        if let Err(err) = gateway::send_message(paths, &args) {
            record_run(
                paths,
                &job.id,
                "delivery_failed",
                "message.send failed",
                Some(json!({ "error": err.to_string() })),
            )?;
        }
    }

    Ok(())
}

fn execute_heartbeat(runner: &mut CodexRunner, paths: &ClawdPaths) -> Result<()> {
    let entry = heartbeat::wake(paths, Some("interval".to_string()))?;
    let status = entry
        .get("payload")
        .and_then(|p| p.get("status"))
        .and_then(|v| v.as_str())
        .unwrap_or("skipped");
    if status != "queued" {
        return Ok(());
    }

    let prompt = "Heartbeat check. If HEARTBEAT.md exists in the workspace, read it and act. If nothing needs attention, respond with exactly HEARTBEAT_OK.";
    let outcome = runner.run_main(prompt)?;
    let response = outcome.message.trim().to_string();
    if response == "HEARTBEAT_OK" {
        return Ok(());
    }

    let args = json!({
        "sessionKey": "agent:main:main",
        "text": response,
        "idempotencyKey": format!("heartbeat:{}", now_ms()),
    });
    let _ = gateway::send_message(paths, &args);
    Ok(())
}

fn should_deliver(job: &crate::cron::CronJob) -> bool {
    job.deliver || job.channel.is_some() || job.to.is_some()
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
        .map(|s| s.to_lowercase())
        .unwrap_or_else(|| "on-request".to_string());
    match raw.as_str() {
        "never" => AskForApproval::Never,
        "on-failure" | "onfailure" => AskForApproval::OnFailure,
        "unless-trusted" | "unlesstrusted" => AskForApproval::UnlessTrusted,
        _ => AskForApproval::OnRequest,
    }
}

fn resolve_codex_overrides(cfg: &ClawdConfig) -> Vec<String> {
    cfg.codex
        .as_ref()
        .and_then(|c| c.config_overrides.clone())
        .unwrap_or_default()
}
