use std::path::PathBuf;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::audit;
use crate::config::ClawdPaths;
use crate::util::{append_json_line, now_ms};

const DB_FILE: &str = "tasks.sqlite";
const EVENTS_DIR: &str = "task_events";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub title: String,
    pub created_at_ms: i64,
    pub last_run_at_ms: Option<i64>,
    pub pinned: bool,
    pub tags: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskRun {
    pub id: String,
    pub task_id: String,
    pub status: String,
    pub started_at_ms: i64,
    pub ended_at_ms: Option<i64>,
    pub codex_thread_id: Option<String>,
    pub sandbox_mode: Option<String>,
    pub approval_policy: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskEvent {
    pub id: String,
    pub task_run_id: String,
    pub ts_ms: i64,
    pub kind: String,
    pub payload: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArtifactRecord {
    pub id: String,
    pub task_run_id: String,
    pub path: String,
    pub mime: Option<String>,
    pub sha256: Option<String>,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApprovalRecord {
    pub id: String,
    pub task_run_id: String,
    pub ts_ms: i64,
    pub kind: String,
    pub request: Value,
    pub decision: Option<String>,
    pub decided_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginRecord {
    pub id: String,
    pub name: String,
    pub version: Option<String>,
    pub description: Option<String>,
    pub source: Option<String>,
    pub path: String,
    pub enabled: bool,
    pub installed_at_ms: i64,
    pub updated_at_ms: i64,
}

pub struct TaskStore {
    conn: Connection,
    events_dir: PathBuf,
    audit_dir: PathBuf,
}

impl TaskStore {
    pub fn open(paths: &ClawdPaths) -> Result<Self> {
        let db_path = paths.state_dir.join(DB_FILE);
        std::fs::create_dir_all(&paths.state_dir)
            .with_context(|| format!("create state dir {}", paths.state_dir.display()))?;
        let events_dir = paths.state_dir.join(EVENTS_DIR);
        std::fs::create_dir_all(&events_dir)
            .with_context(|| format!("create events dir {}", events_dir.display()))?;
        let audit_dir = audit::audit_dir(paths);
        std::fs::create_dir_all(&audit_dir)
            .with_context(|| format!("create audit dir {}", audit_dir.display()))?;

        let conn = Connection::open(db_path).context("open tasks database")?;
        conn.pragma_update(None, "journal_mode", "WAL")
            .ok();
        conn.pragma_update(None, "synchronous", "NORMAL")
            .ok();

        let store = TaskStore {
            conn,
            events_dir,
            audit_dir,
        };
        store.migrate()?;
        Ok(store)
    }

    fn migrate(&self) -> Result<()> {
        self.conn.execute_batch(
            r#"
            CREATE TABLE IF NOT EXISTS tasks (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                created_at_ms INTEGER NOT NULL,
                last_run_at_ms INTEGER,
                pinned INTEGER NOT NULL DEFAULT 0,
                tags_json TEXT
            );

            CREATE TABLE IF NOT EXISTS task_runs (
                id TEXT PRIMARY KEY,
                task_id TEXT NOT NULL,
                status TEXT NOT NULL,
                started_at_ms INTEGER NOT NULL,
                ended_at_ms INTEGER,
                codex_thread_id TEXT,
                sandbox_mode TEXT,
                approval_policy TEXT,
                FOREIGN KEY(task_id) REFERENCES tasks(id)
            );

            CREATE TABLE IF NOT EXISTS events (
                id TEXT PRIMARY KEY,
                task_run_id TEXT NOT NULL,
                ts_ms INTEGER NOT NULL,
                kind TEXT NOT NULL,
                payload_json TEXT NOT NULL,
                FOREIGN KEY(task_run_id) REFERENCES task_runs(id)
            );

            CREATE TABLE IF NOT EXISTS approvals (
                id TEXT PRIMARY KEY,
                task_run_id TEXT NOT NULL,
                ts_ms INTEGER NOT NULL,
                kind TEXT NOT NULL,
                request_json TEXT NOT NULL,
                decision TEXT,
                decided_at_ms INTEGER,
                FOREIGN KEY(task_run_id) REFERENCES task_runs(id)
            );

            CREATE TABLE IF NOT EXISTS artifacts (
                id TEXT PRIMARY KEY,
                task_run_id TEXT NOT NULL,
                path TEXT NOT NULL,
                mime TEXT,
                sha256 TEXT,
                created_at_ms INTEGER NOT NULL,
                FOREIGN KEY(task_run_id) REFERENCES task_runs(id)
            );

            CREATE TABLE IF NOT EXISTS plugins (
                id TEXT PRIMARY KEY,
                name TEXT NOT NULL,
                version TEXT,
                description TEXT,
                source TEXT,
                path TEXT NOT NULL,
                enabled INTEGER NOT NULL DEFAULT 1,
                installed_at_ms INTEGER NOT NULL,
                updated_at_ms INTEGER NOT NULL
            );

            CREATE INDEX IF NOT EXISTS idx_task_runs_task_id ON task_runs(task_id);
            CREATE INDEX IF NOT EXISTS idx_events_run_id ON events(task_run_id, ts_ms);
            CREATE INDEX IF NOT EXISTS idx_approvals_run_id ON approvals(task_run_id, ts_ms);
            CREATE INDEX IF NOT EXISTS idx_artifacts_run_id ON artifacts(task_run_id, created_at_ms);
            CREATE INDEX IF NOT EXISTS idx_plugins_enabled ON plugins(enabled, updated_at_ms);
            "#,
        )?;
        Ok(())
    }

    pub fn create_task(&self, title: &str) -> Result<Task> {
        let now = now_ms();
        let id = Uuid::new_v4().to_string();
        self.conn.execute(
            "INSERT INTO tasks(id, title, created_at_ms, pinned, tags_json) VALUES (?1, ?2, ?3, 0, ?4)",
            params![id, title, now, "[]"],
        )?;
        Ok(Task {
            id,
            title: title.to_string(),
            created_at_ms: now,
            last_run_at_ms: None,
            pinned: false,
            tags: Vec::new(),
        })
    }

    pub fn list_tasks(&self) -> Result<Vec<Task>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, title, created_at_ms, last_run_at_ms, pinned, tags_json FROM tasks ORDER BY created_at_ms DESC",
        )?;
        let rows = stmt.query_map([], |row| {
            let tags_json: Option<String> = row.get(5)?;
            let tags: Vec<String> = tags_json
                .as_deref()
                .and_then(|raw| serde_json::from_str(raw).ok())
                .unwrap_or_default();
            Ok(Task {
                id: row.get(0)?,
                title: row.get(1)?,
                created_at_ms: row.get(2)?,
                last_run_at_ms: row.get(3)?,
                pinned: row.get::<_, i64>(4)? != 0,
                tags,
            })
        })?;

        let mut tasks = Vec::new();
        for row in rows {
            tasks.push(row?);
        }
        Ok(tasks)
    }

    pub fn get_task_by_title(&self, title: &str) -> Result<Option<Task>> {
        self.conn
            .query_row(
                "SELECT id, title, created_at_ms, last_run_at_ms, pinned, tags_json FROM tasks WHERE title = ?",
                [title],
                |row| {
                    let tags_json: Option<String> = row.get(5)?;
                    let tags: Vec<String> = tags_json
                        .as_deref()
                        .and_then(|raw| serde_json::from_str(raw).ok())
                        .unwrap_or_default();
                    Ok(Task {
                        id: row.get(0)?,
                        title: row.get(1)?,
                        created_at_ms: row.get(2)?,
                        last_run_at_ms: row.get(3)?,
                        pinned: row.get::<_, i64>(4)? != 0,
                        tags,
                    })
                },
            )
            .optional()
            .context("query task by title")
    }

    pub fn create_run(
        &self,
        task_id: &str,
        status: &str,
        codex_thread_id: Option<String>,
        sandbox_mode: Option<String>,
        approval_policy: Option<String>,
    ) -> Result<TaskRun> {
        let now = now_ms();
        let id = Uuid::new_v4().to_string();
        self.conn.execute(
            "INSERT INTO task_runs(id, task_id, status, started_at_ms, codex_thread_id, sandbox_mode, approval_policy) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                id,
                task_id,
                status,
                now,
                codex_thread_id,
                sandbox_mode,
                approval_policy
            ],
        )?;
        self.conn.execute(
            "UPDATE tasks SET last_run_at_ms = ?1 WHERE id = ?2",
            params![now, task_id],
        )?;

        let plugins = self.list_plugins(true).unwrap_or_default();
        audit::append_plugins_snapshot(&self.audit_dir, &id, &plugins)?;

        Ok(TaskRun {
            id,
            task_id: task_id.to_string(),
            status: status.to_string(),
            started_at_ms: now,
            ended_at_ms: None,
            codex_thread_id,
            sandbox_mode,
            approval_policy,
        })
    }

    pub fn update_run_status(&self, run_id: &str, status: &str) -> Result<()> {
        let now = now_ms();
        self.conn.execute(
            "UPDATE task_runs SET status = ?1, ended_at_ms = ?2 WHERE id = ?3",
            params![status, now, run_id],
        )?;
        Ok(())
    }

    pub fn update_run_thread(&self, run_id: &str, thread_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE task_runs SET codex_thread_id = ?1 WHERE id = ?2",
            params![thread_id, run_id],
        )?;
        Ok(())
    }

    pub fn get_run(&self, run_id: &str) -> Result<Option<TaskRun>> {
        self.conn
            .query_row(
                "SELECT id, task_id, status, started_at_ms, ended_at_ms, codex_thread_id, sandbox_mode, approval_policy FROM task_runs WHERE id = ?1",
                params![run_id],
                |row| {
                    Ok(TaskRun {
                        id: row.get(0)?,
                        task_id: row.get(1)?,
                        status: row.get(2)?,
                        started_at_ms: row.get(3)?,
                        ended_at_ms: row.get(4)?,
                        codex_thread_id: row.get(5)?,
                        sandbox_mode: row.get(6)?,
                        approval_policy: row.get(7)?,
                    })
                },
            )
            .optional()
            .context("query run")
    }

    pub fn list_runs(
        &self,
        task_id: Option<&str>,
        limit: Option<usize>,
    ) -> Result<Vec<TaskRun>> {
        let limit = limit.unwrap_or(50).clamp(1, 500) as i64;
        let sql = if task_id.is_some() {
            "SELECT id, task_id, status, started_at_ms, ended_at_ms, codex_thread_id, sandbox_mode, approval_policy FROM task_runs WHERE task_id = ?1 ORDER BY started_at_ms DESC LIMIT ?2"
        } else {
            "SELECT id, task_id, status, started_at_ms, ended_at_ms, codex_thread_id, sandbox_mode, approval_policy FROM task_runs ORDER BY started_at_ms DESC LIMIT ?1"
        };

        let mut stmt = self.conn.prepare(sql)?;
        let mut runs = Vec::new();
        if let Some(task_id) = task_id {
            let rows = stmt.query_map(params![task_id, limit], |row| {
                Ok(TaskRun {
                    id: row.get(0)?,
                    task_id: row.get(1)?,
                    status: row.get(2)?,
                    started_at_ms: row.get(3)?,
                    ended_at_ms: row.get(4)?,
                    codex_thread_id: row.get(5)?,
                    sandbox_mode: row.get(6)?,
                    approval_policy: row.get(7)?,
                })
            })?;
            for row in rows {
                runs.push(row?);
            }
        } else {
            let rows = stmt.query_map(params![limit], |row| {
                Ok(TaskRun {
                    id: row.get(0)?,
                    task_id: row.get(1)?,
                    status: row.get(2)?,
                    started_at_ms: row.get(3)?,
                    ended_at_ms: row.get(4)?,
                    codex_thread_id: row.get(5)?,
                    sandbox_mode: row.get(6)?,
                    approval_policy: row.get(7)?,
                })
            })?;
            for row in rows {
                runs.push(row?);
            }
        }
        Ok(runs)
    }

    pub fn record_event(&self, run_id: &str, kind: &str, payload: &Value) -> Result<TaskEvent> {
        let now = now_ms();
        let id = Uuid::new_v4().to_string();
        let payload_json = serde_json::to_string(payload).context("serialize event payload")?;
        self.conn.execute(
            "INSERT INTO events(id, task_run_id, ts_ms, kind, payload_json) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![id, run_id, now, kind, payload_json],
        )?;
        let event = TaskEvent {
            id,
            task_run_id: run_id.to_string(),
            ts_ms: now,
            kind: kind.to_string(),
            payload: payload.clone(),
        };
        self.append_event_log(run_id, &event)?;
        audit::append_event(&self.audit_dir, &event)?;
        if kind == "item_completed" {
            if let Some((tool, arguments)) = extract_mcp_tool_call(payload) {
                audit::append_tool_call(&self.audit_dir, run_id, &tool, &arguments)?;
            }
        }
        Ok(event)
    }

    pub fn record_approval(
        &self,
        run_id: &str,
        kind: &str,
        request: &Value,
        decision: Option<&str>,
    ) -> Result<()> {
        let now = now_ms();
        let id = Uuid::new_v4().to_string();
        let request_json = serde_json::to_string(request).context("serialize approval request")?;
        self.conn.execute(
            "INSERT INTO approvals(id, task_run_id, ts_ms, kind, request_json, decision, decided_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![id, run_id, now, kind, request_json, decision, decision.map(|_| now)],
        )?;
        audit::append_approval(&self.audit_dir, run_id, kind, request, decision)?;
        Ok(())
    }

    pub fn record_artifact(
        &self,
        run_id: &str,
        path: &str,
        mime: Option<String>,
        sha256: Option<String>,
    ) -> Result<()> {
        let now = now_ms();
        let id = Uuid::new_v4().to_string();
        self.conn.execute(
            "INSERT INTO artifacts(id, task_run_id, path, mime, sha256, created_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![id, run_id, path, mime, sha256, now],
        )?;
        audit::append_artifact(
            &self.audit_dir,
            run_id,
            path,
            mime.as_deref(),
            sha256.as_deref(),
        )?;
        Ok(())
    }

    pub fn list_artifacts(&self, run_id: &str) -> Result<Vec<ArtifactRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_run_id, path, mime, sha256, created_at_ms FROM artifacts WHERE task_run_id = ? ORDER BY created_at_ms ASC",
        )?;
        let rows = stmt.query_map(params![run_id], |row| {
            Ok(ArtifactRecord {
                id: row.get(0)?,
                task_run_id: row.get(1)?,
                path: row.get(2)?,
                mime: row.get(3)?,
                sha256: row.get(4)?,
                created_at_ms: row.get(5)?,
            })
        })?;
        let mut artifacts = Vec::new();
        for row in rows {
            artifacts.push(row?);
        }
        Ok(artifacts)
    }

    pub fn list_approvals(&self, run_id: &str) -> Result<Vec<ApprovalRecord>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_run_id, ts_ms, kind, request_json, decision, decided_at_ms FROM approvals WHERE task_run_id = ? ORDER BY ts_ms ASC",
        )?;
        let rows = stmt.query_map(params![run_id], |row| {
            let request_json: String = row.get(4)?;
            let request: Value = serde_json::from_str(&request_json).unwrap_or(Value::Null);
            Ok(ApprovalRecord {
                id: row.get(0)?,
                task_run_id: row.get(1)?,
                ts_ms: row.get(2)?,
                kind: row.get(3)?,
                request,
                decision: row.get(5)?,
                decided_at_ms: row.get(6)?,
            })
        })?;
        let mut approvals = Vec::new();
        for row in rows {
            approvals.push(row?);
        }
        Ok(approvals)
    }

    pub fn list_events(&self, run_id: &str, limit: Option<usize>) -> Result<Vec<TaskEvent>> {
        let limit = limit.unwrap_or(200) as i64;
        let mut stmt = self.conn.prepare(
            "SELECT id, task_run_id, ts_ms, kind, payload_json FROM events WHERE task_run_id = ? ORDER BY ts_ms DESC LIMIT ?",
        )?;
        let rows = stmt.query_map(params![run_id, limit], |row| {
            let payload_json: String = row.get(4)?;
            let payload: Value = serde_json::from_str(&payload_json).unwrap_or(Value::Null);
            Ok(TaskEvent {
                id: row.get(0)?,
                task_run_id: row.get(1)?,
                ts_ms: row.get(2)?,
                kind: row.get(3)?,
                payload,
            })
        })?;
        let mut events = Vec::new();
        for row in rows {
            events.push(row?);
        }
        Ok(events)
    }

    pub fn list_events_after(
        &self,
        run_id: &str,
        after_ts_ms: i64,
        limit: usize,
    ) -> Result<Vec<TaskEvent>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, task_run_id, ts_ms, kind, payload_json FROM events WHERE task_run_id = ? AND ts_ms > ? ORDER BY ts_ms ASC LIMIT ?",
        )?;
        let rows = stmt.query_map(params![run_id, after_ts_ms, limit as i64], |row| {
            let payload_json: String = row.get(4)?;
            let payload: Value = serde_json::from_str(&payload_json).unwrap_or(Value::Null);
            Ok(TaskEvent {
                id: row.get(0)?,
                task_run_id: row.get(1)?,
                ts_ms: row.get(2)?,
                kind: row.get(3)?,
                payload,
            })
        })?;
        let mut events = Vec::new();
        for row in rows {
            events.push(row?);
        }
        Ok(events)
    }

    pub fn upsert_plugin(&self, plugin: &PluginRecord) -> Result<PluginRecord> {
        self.conn.execute(
            r#"
            INSERT INTO plugins(
                id, name, version, description, source, path, enabled, installed_at_ms, updated_at_ms
            ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
            ON CONFLICT(id) DO UPDATE SET
                name = excluded.name,
                version = excluded.version,
                description = excluded.description,
                source = excluded.source,
                path = excluded.path,
                enabled = excluded.enabled,
                updated_at_ms = excluded.updated_at_ms
            "#,
            params![
                plugin.id,
                plugin.name,
                plugin.version,
                plugin.description,
                plugin.source,
                plugin.path,
                if plugin.enabled { 1 } else { 0 },
                plugin.installed_at_ms,
                plugin.updated_at_ms
            ],
        )?;
        Ok(plugin.clone())
    }

    pub fn list_plugins(&self, include_disabled: bool) -> Result<Vec<PluginRecord>> {
        let sql = if include_disabled {
            "SELECT id, name, version, description, source, path, enabled, installed_at_ms, updated_at_ms FROM plugins ORDER BY updated_at_ms DESC"
        } else {
            "SELECT id, name, version, description, source, path, enabled, installed_at_ms, updated_at_ms FROM plugins WHERE enabled = 1 ORDER BY updated_at_ms DESC"
        };
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([], |row| {
            Ok(PluginRecord {
                id: row.get(0)?,
                name: row.get(1)?,
                version: row.get(2)?,
                description: row.get(3)?,
                source: row.get(4)?,
                path: row.get(5)?,
                enabled: row.get::<_, i64>(6)? != 0,
                installed_at_ms: row.get(7)?,
                updated_at_ms: row.get(8)?,
            })
        })?;
        let mut plugins = Vec::new();
        for row in rows {
            plugins.push(row?);
        }
        Ok(plugins)
    }

    pub fn get_plugin(&self, plugin_id: &str) -> Result<Option<PluginRecord>> {
        self.conn
            .query_row(
                "SELECT id, name, version, description, source, path, enabled, installed_at_ms, updated_at_ms FROM plugins WHERE id = ?1",
                [plugin_id],
                |row| {
                    Ok(PluginRecord {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        version: row.get(2)?,
                        description: row.get(3)?,
                        source: row.get(4)?,
                        path: row.get(5)?,
                        enabled: row.get::<_, i64>(6)? != 0,
                        installed_at_ms: row.get(7)?,
                        updated_at_ms: row.get(8)?,
                    })
                },
            )
            .optional()
            .context("query plugin by id")
    }

    pub fn set_plugin_enabled(&self, plugin_id: &str, enabled: bool) -> Result<Option<PluginRecord>> {
        let now = now_ms();
        self.conn.execute(
            "UPDATE plugins SET enabled = ?1, updated_at_ms = ?2 WHERE id = ?3",
            params![if enabled { 1 } else { 0 }, now, plugin_id],
        )?;
        self.get_plugin(plugin_id)
    }

    pub fn remove_plugin(&self, plugin_id: &str) -> Result<()> {
        self.conn
            .execute("DELETE FROM plugins WHERE id = ?1", [plugin_id])?;
        Ok(())
    }

    fn append_event_log(&self, run_id: &str, event: &TaskEvent) -> Result<()> {
        let path = self.events_dir.join(format!("{run_id}.jsonl"));
        let value = serde_json::to_value(event).unwrap_or(Value::Null);
        append_json_line(&path, &value)
    }
}

fn extract_mcp_tool_call(payload: &Value) -> Option<(String, Value)> {
    let method = payload.get("method").and_then(|v| v.as_str())?;
    if method != "item/completed" {
        return None;
    }
    let params = payload.get("params")?.as_object()?;
    let item = params.get("item")?.as_object()?;
    if item.get("type").and_then(|v| v.as_str()) != Some("mcpToolCall") {
        return None;
    }
    let tool = item.get("tool").and_then(|v| v.as_str())?.trim();
    if tool.is_empty() {
        return None;
    }
    let arguments = item.get("arguments").cloned().unwrap_or(Value::Null);
    Some((tool.to_string(), arguments))
}
