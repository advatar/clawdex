use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use serde_json::json;
use uuid::Uuid;

use clawdex::config::load_config;
use clawdex::cron;
use clawdex::daemon::deliver_heartbeat_response_for_test;
use clawdex::gateway;
use clawdex::memory;
use clawdex::util::{now_ms, read_json_lines};

fn temp_paths() -> Result<(PathBuf, clawdex::config::ClawdPaths)> {
    let base = std::env::temp_dir().join(format!("clawdex-test-{}", Uuid::new_v4()));
    let state_dir = base.join("state");
    let workspace_dir = base.join("workspace");
    fs::create_dir_all(&workspace_dir)?;
    let (_cfg, paths) = load_config(Some(state_dir), Some(workspace_dir))?;
    Ok((base, paths))
}

#[test]
fn cron_job_persists_across_restart() -> Result<()> {
    let (_base, paths) = temp_paths()?;
    let job = json!({
        "name": "persist-test",
        "enabled": true,
        "schedule": { "kind": "every", "everyMs": 60000 },
        "sessionTarget": "main",
        "wakeMode": "now",
        "payload": { "kind": "systemEvent", "text": "hello" }
    });
    let _ = cron::add_job(&paths, &job)?;
    let list1 = cron::list_jobs(&paths, false)?;
    assert_eq!(list1["jobs"].as_array().unwrap().len(), 1);

    let (_cfg2, paths2) = load_config(Some(paths.state_dir.clone()), Some(paths.workspace_dir.clone()))?;
    let list2 = cron::list_jobs(&paths2, false)?;
    assert_eq!(list2["jobs"].as_array().unwrap().len(), 1);
    Ok(())
}

#[test]
fn cron_main_session_job_generates_system_event_prompt() -> Result<()> {
    let (_base, paths) = temp_paths()?;
    let now = now_ms();
    let job = json!({
        "name": "main-job",
        "enabled": true,
        "schedule": { "kind": "at", "atMs": now - 60000 },
        "sessionTarget": "main",
        "wakeMode": "now",
        "payload": { "kind": "systemEvent", "text": "ping" }
    });
    let created = cron::add_job(&paths, &job)?;
    let (queued, _) = cron::collect_due_jobs(&paths, now, "due", None)?;
    assert_eq!(queued.len(), 1);
    let cron_job = &queued[0];
    assert_eq!(cron::job_session_key(cron_job), "agent:main:main");
    let prompt = cron::job_prompt(cron_job, now).unwrap_or_default();
    assert!(prompt.contains("System event"));
    assert!(prompt.contains(created.get("id").and_then(|v| v.as_str()).unwrap_or("")));
    Ok(())
}

#[test]
fn cron_isolated_job_uses_isolated_session_key() -> Result<()> {
    let (_base, paths) = temp_paths()?;
    let now = now_ms();
    let job = json!({
        "name": "isolated-job",
        "enabled": true,
        "schedule": { "kind": "at", "atMs": now - 60000 },
        "sessionTarget": "isolated",
        "wakeMode": "now",
        "payload": { "kind": "agentTurn", "message": "hello" }
    });
    let created = cron::add_job(&paths, &job)?;
    let (queued, _) = cron::collect_due_jobs(&paths, now, "due", None)?;
    assert_eq!(queued.len(), 1);
    let cron_job = &queued[0];
    let job_id = created.get("id").and_then(|v| v.as_str()).unwrap_or("");
    assert_eq!(cron::job_session_key(cron_job), format!("cron:{}", job_id));
    Ok(())
}

#[test]
fn heartbeat_ok_suppresses_delivery() -> Result<()> {
    let (_base, paths) = temp_paths()?;
    let delivered = deliver_heartbeat_response_for_test(&paths, "HEARTBEAT_OK")?;
    assert!(!delivered);

    let outbox = paths.state_dir.join("gateway").join("outbox.jsonl");
    assert!(!outbox.exists());

    let _ = gateway::record_incoming(&paths, &json!({
        "channel": "agent",
        "from": "main:main",
        "text": "seed route"
    }))?;

    let delivered = deliver_heartbeat_response_for_test(&paths, "Needs attention")?;
    assert!(delivered);
    let entries = read_json_lines(&outbox, Some(10))?;
    assert!(!entries.is_empty());
    Ok(())
}

#[test]
fn memory_search_returns_line_ranges() -> Result<()> {
    let (_base, paths) = temp_paths()?;
    let memory_dir = paths.workspace_dir.join("memory");
    fs::create_dir_all(&memory_dir)?;
    let memory_path = memory_dir.join("2026-02-01.md");
    fs::write(&memory_path, "alpha\nbeta\nneedle here\ngamma\n")?;

    let result = memory::memory_search(&paths, &json!({ "query": "needle" }))?;
    let results = result["results"].as_array().unwrap();
    assert!(!results.is_empty());
    let first = &results[0];
    assert!(first.get("startLine").is_some() || first.get("lineStart").is_some());
    Ok(())
}

#[test]
fn last_route_delivery_falls_back_when_channel_missing() -> Result<()> {
    let (_base, paths) = temp_paths()?;
    let _ = gateway::record_incoming(&paths, &json!({
        "channel": "slack",
        "from": "U123",
        "text": "hi"
    }))?;

    let _ = gateway::send_message(&paths, &json!({
        "sessionKey": "slack:U123",
        "text": "follow up"
    }))?;

    let outbox = paths.state_dir.join("gateway").join("outbox.jsonl");
    let entries = read_json_lines(&outbox, Some(10))?;
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry["channel"].as_str().unwrap_or(""), "slack");
    assert_eq!(entry["to"].as_str().unwrap_or(""), "U123");
    Ok(())
}
