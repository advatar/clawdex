use std::fs;
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::Result;
use serde_json::json;
use tungstenite::{connect, Message};
use uuid::Uuid;

use clawdex::config::load_config;
use clawdex::cron;
use clawdex::daemon::deliver_heartbeat_response_for_test;
use clawdex::gateway;
use clawdex::memory;
use clawdex::task_db::{PluginRecord, TaskStore};
use clawdex::util::{now_ms, read_json_lines};

fn temp_paths() -> Result<(PathBuf, clawdex::config::ClawdPaths)> {
    let base = std::env::temp_dir().join(format!("clawdex-test-{}", Uuid::new_v4()));
    let state_dir = base.join("state");
    let workspace_dir = base.join("workspace");
    fs::create_dir_all(&workspace_dir)?;
    let (_cfg, paths) = load_config(Some(state_dir), Some(workspace_dir))?;
    Ok((base, paths))
}

fn start_gateway(paths: &clawdex::config::ClawdPaths) -> Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    drop(listener);
    let bind = format!("{}", addr);
    let thread_bind = bind.clone();
    let paths_clone = paths.clone();
    std::thread::spawn(move || {
        let _ = gateway::run_gateway(&thread_bind, &paths_clone);
    });
    std::thread::sleep(Duration::from_millis(50));
    Ok(format!("http://{bind}"))
}

fn start_gateway_ws(paths: &clawdex::config::ClawdPaths) -> Result<String> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    drop(listener);
    let bind = format!("{}", addr);
    let thread_bind = bind.clone();
    let paths_clone = paths.clone();
    std::thread::spawn(move || {
        let _ = gateway::run_gateway_ws(&thread_bind, &paths_clone);
    });
    std::thread::sleep(Duration::from_millis(50));
    Ok(format!("ws://{bind}"))
}

fn write_gateway_config(paths: &clawdex::config::ClawdPaths, url: &str) -> Result<()> {
    let config_path = paths.state_dir.join("config.json5");
    let value = json!({ "gateway": { "url": url } });
    fs::write(config_path, serde_json::to_string_pretty(&value)?)?;
    Ok(())
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
    let url = start_gateway(&paths)?;
    write_gateway_config(&paths, &url)?;
    let (cfg, _) = load_config(Some(paths.state_dir.clone()), Some(paths.workspace_dir.clone()))?;
    let delivered = deliver_heartbeat_response_for_test(&cfg, &paths, "HEARTBEAT_OK")?;
    assert!(!delivered);

    let outbox = paths.state_dir.join("gateway").join("outbox.jsonl");
    assert!(!outbox.exists());

    let _ = gateway::record_incoming(&paths, &json!({
        "channel": "agent",
        "from": "main:main",
        "text": "seed route"
    }))?;

    let delivered = deliver_heartbeat_response_for_test(&cfg, &paths, "Needs attention")?;
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
    let url = start_gateway(&paths)?;
    write_gateway_config(&paths, &url)?;
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

#[test]
fn last_route_delivery_respects_channel_filter() -> Result<()> {
    let (_base, paths) = temp_paths()?;
    let url = start_gateway(&paths)?;
    write_gateway_config(&paths, &url)?;
    let _ = gateway::record_incoming(&paths, &json!({
        "channel": "slack",
        "from": "U123",
        "text": "hi"
    }))?;
    let _ = gateway::record_incoming(&paths, &json!({
        "channel": "telegram",
        "from": "U999",
        "text": "yo"
    }))?;

    let _ = gateway::send_message(&paths, &json!({
        "channel": "slack",
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

#[test]
fn last_route_delivery_respects_to_filter() -> Result<()> {
    let (_base, paths) = temp_paths()?;
    let url = start_gateway(&paths)?;
    write_gateway_config(&paths, &url)?;
    let _ = gateway::record_incoming(&paths, &json!({
        "channel": "slack",
        "from": "U123",
        "text": "hi"
    }))?;
    let _ = gateway::record_incoming(&paths, &json!({
        "channel": "telegram",
        "from": "U999",
        "text": "yo"
    }))?;

    let _ = gateway::send_message(&paths, &json!({
        "to": "U999",
        "text": "follow up"
    }))?;

    let outbox = paths.state_dir.join("gateway").join("outbox.jsonl");
    let entries = read_json_lines(&outbox, Some(10))?;
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry["channel"].as_str().unwrap_or(""), "telegram");
    assert_eq!(entry["to"].as_str().unwrap_or(""), "U999");
    Ok(())
}

#[test]
fn last_route_delivery_uses_session_key_route() -> Result<()> {
    let (_base, paths) = temp_paths()?;
    let url = start_gateway(&paths)?;
    write_gateway_config(&paths, &url)?;
    let _ = gateway::record_incoming(&paths, &json!({
        "channel": "slack",
        "from": "U123",
        "text": "hi"
    }))?;
    let _ = gateway::record_incoming(&paths, &json!({
        "channel": "telegram",
        "from": "U999",
        "text": "yo"
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

#[test]
fn gateway_ws_send_enqueues_outbox() -> Result<()> {
    let (_base, paths) = temp_paths()?;
    let ws_url = start_gateway_ws(&paths)?;
    let (mut socket, _) = connect(ws_url.as_str())?;

    let hello = json!({
        "type": "req",
        "id": "1",
        "method": "hello",
        "params": {}
    });
    socket.send(Message::Text(hello.to_string()))?;
    let msg = socket.read()?;
    let resp: serde_json::Value = serde_json::from_str(msg.to_text()?)?;
    assert_eq!(resp["ok"].as_bool(), Some(true));
    assert_eq!(resp["payload"]["type"].as_str(), Some("hello-ok"));

    let send = json!({
        "type": "req",
        "id": "2",
        "method": "send",
        "params": {
            "channel": "slack",
            "to": "U123",
            "text": "hi",
            "sessionKey": "slack:U123"
        }
    });
    socket.send(Message::Text(send.to_string()))?;
    let msg = socket.read()?;
    let resp: serde_json::Value = serde_json::from_str(msg.to_text()?)?;
    assert_eq!(resp["ok"].as_bool(), Some(true));
    assert_eq!(resp["payload"]["queued"].as_bool(), Some(true));

    let outbox = paths.state_dir.join("gateway").join("outbox.jsonl");
    let entries = read_json_lines(&outbox, Some(10))?;
    assert_eq!(entries.len(), 1);
    let entry = &entries[0];
    assert_eq!(entry["channel"].as_str().unwrap_or(""), "slack");
    assert_eq!(entry["to"].as_str().unwrap_or(""), "U123");
    Ok(())
}

#[test]
fn gateway_ws_methods_list_and_reload() -> Result<()> {
    let (base, paths) = temp_paths()?;
    let plugin_dir = base.join("plugin-methods");
    fs::create_dir_all(&plugin_dir)?;

    let store = TaskStore::open(&paths)?;
    let plugin = PluginRecord {
        id: "plugin-methods".to_string(),
        name: "Plugin Methods".to_string(),
        version: None,
        description: None,
        source: None,
        path: plugin_dir.to_string_lossy().to_string(),
        enabled: true,
        installed_at_ms: now_ms(),
        updated_at_ms: now_ms(),
    };
    store.upsert_plugin(&plugin)?;

    let manifest_a = json!({
        "id": "plugin-methods",
        "gatewayMethods": ["plugin.foo"],
        "configSchema": {}
    });
    fs::write(
        plugin_dir.join("openclaw.plugin.json"),
        serde_json::to_vec_pretty(&manifest_a)?,
    )?;

    let ws_url = start_gateway_ws(&paths)?;
    let (mut socket, _) = connect(ws_url.as_str())?;

    let hello = json!({
        "type": "req",
        "id": "1",
        "method": "hello",
        "params": {}
    });
    socket.send(Message::Text(hello.to_string()))?;
    let msg = socket.read()?;
    let resp: serde_json::Value = serde_json::from_str(msg.to_text()?)?;
    assert_eq!(resp["ok"].as_bool(), Some(true));
    let methods = resp["payload"]["features"]["methods"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(methods.iter().any(|m| m.as_str() == Some("plugin.foo")));

    let list = json!({
        "type": "req",
        "id": "2",
        "method": "methods.list",
        "params": {}
    });
    socket.send(Message::Text(list.to_string()))?;
    let msg = socket.read()?;
    let resp: serde_json::Value = serde_json::from_str(msg.to_text()?)?;
    assert_eq!(resp["ok"].as_bool(), Some(true));
    let listed = resp["payload"]["methods"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(listed.iter().any(|entry| {
        entry.get("name").and_then(|v| v.as_str()) == Some("plugin.foo")
            && entry.get("version").and_then(|v| v.as_u64()) == Some(1)
    }));

    let manifest_b = json!({
        "id": "plugin-methods",
        "gatewayMethods": ["plugin.bar"],
        "configSchema": {}
    });
    fs::write(
        plugin_dir.join("openclaw.plugin.json"),
        serde_json::to_vec_pretty(&manifest_b)?,
    )?;

    let reload = json!({
        "type": "req",
        "id": "3",
        "method": "gateway.reload",
        "params": {}
    });
    socket.send(Message::Text(reload.to_string()))?;
    let msg = socket.read()?;
    let resp: serde_json::Value = serde_json::from_str(msg.to_text()?)?;
    assert_eq!(resp["ok"].as_bool(), Some(true));
    assert_eq!(resp["payload"]["reloaded"].as_bool(), Some(true));
    let listed = resp["payload"]["methods"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(listed
        .iter()
        .any(|entry| entry.get("name").and_then(|v| v.as_str()) == Some("plugin.bar")));

    let _ = fs::remove_dir_all(base);
    Ok(())
}
