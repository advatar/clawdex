use std::fs;
use std::path::PathBuf;

use anyhow::Result;
use serde_json::json;
use uuid::Uuid;

use clawdex::audit;
use clawdex::config::load_config;
use clawdex::task_db::TaskStore;
use clawdex::util::read_json_lines;

fn temp_paths() -> Result<(PathBuf, clawdex::config::ClawdPaths)> {
    let base = std::env::temp_dir().join(format!("clawdex-audit-test-{}", Uuid::new_v4()));
    let state_dir = base.join("state");
    let workspace_dir = base.join("workspace");
    fs::create_dir_all(&workspace_dir)?;
    let (_cfg, paths) = load_config(Some(state_dir), Some(workspace_dir))?;
    Ok((base, paths))
}

#[test]
fn audit_log_records_mcp_tool_calls() -> Result<()> {
    let (base, paths) = temp_paths()?;
    let store = TaskStore::open(&paths)?;
    let task = store.create_task("audit")?;
    let run = store.create_run(&task.id, "running", None, None, None)?;

    let payload = json!({
        "method": "item/completed",
        "params": {
            "threadId": "t1",
            "turnId": "u1",
            "itemId": "i1",
            "item": {
                "type": "mcpToolCall",
                "id": "item1",
                "server": "mcp",
                "tool": "message.send",
                "status": "completed",
                "arguments": { "channel": "slack", "to": "U123", "text": "hi" },
                "result": null,
                "error": null,
                "durationMs": 12
            }
        }
    });

    store.record_event(&run.id, "item_completed", &payload)?;

    let audit_dir = audit::audit_dir(&paths);
    let log = audit::read_audit_log(&audit_dir, &run.id, None)?;
    assert!(!log.is_empty());
    assert!(log.iter().any(|entry| {
        entry.get("kind").and_then(|v| v.as_str()) == Some("tool_call")
            && entry.get("payload").and_then(|p| p.get("tool")).and_then(|v| v.as_str())
                == Some("message.send")
    }));

    // Also confirm we wrote to the jsonl file on disk (not just in-memory).
    let path = audit_dir.join(format!("{}.jsonl", run.id));
    let lines = read_json_lines(&path, None)?;
    assert!(lines.iter().any(|entry| {
        entry.get("kind").and_then(|v| v.as_str()) == Some("tool_call")
    }));

    let _ = fs::remove_dir_all(base);
    Ok(())
}

