use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::rc::Rc;
use std::thread;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use codex_app_server_protocol::{
    AskForApproval, CommandExecutionApprovalDecision, CommandExecutionRequestApprovalParams,
    FileChangeApprovalDecision, FileChangeRequestApprovalParams, ToolRequestUserInputAnswer,
    ToolRequestUserInputParams,
};
use serde_json::{json, Value};
use tiny_http::{Method, Response, Server, StatusCode};

use crate::app_server::{ApprovalHandler, ApprovalMode, CodexClient, EventSink, UserInputHandler};
use crate::config::{load_config, ClawdConfig, ClawdPaths};
use crate::runner::workspace_sandbox_policy;
use crate::approvals::{ApprovalBroker, BrokerApprovalHandler, BrokerUserInputHandler};
use crate::audit;
use crate::task_db::{Task, TaskEvent, TaskRun, TaskStore};
use crate::util::{now_ms, write_json_value};

pub struct TaskEngine {
    cfg: ClawdConfig,
    paths: ClawdPaths,
}

#[derive(Clone)]
pub struct TaskRunOptions {
    pub codex_path: Option<PathBuf>,
    pub workspace: Option<PathBuf>,
    pub state_dir: Option<PathBuf>,
    pub auto_approve: bool,
    pub approval_policy: Option<AskForApproval>,
    pub prompt: Option<String>,
    pub title: Option<String>,
    pub task_id: Option<String>,
    pub resume_from_run_id: Option<String>,
    pub fork_from_run_id: Option<String>,
}

pub fn run_task_command(opts: TaskRunOptions) -> Result<()> {
    let (cfg, paths) = load_config(opts.state_dir.clone(), opts.workspace.clone())?;
    let engine = TaskEngine { cfg, paths };
    engine.run_task(opts)
}

pub fn start_task_command(opts: TaskRunOptions) -> Result<TaskRun> {
    let (cfg, paths) = load_config(opts.state_dir.clone(), opts.workspace.clone())?;
    let engine = TaskEngine { cfg, paths };
    engine.start_task_async(opts)
}

pub fn list_tasks_command(state_dir: Option<PathBuf>, workspace: Option<PathBuf>) -> Result<Value> {
    let (_cfg, paths) = load_config(state_dir, workspace)?;
    let store = TaskStore::open(&paths)?;
    let tasks = store.list_tasks()?;
    Ok(json!({ "tasks": tasks }))
}

pub fn create_task_command(
    title: &str,
    state_dir: Option<PathBuf>,
    workspace: Option<PathBuf>,
) -> Result<Value> {
    let (_cfg, paths) = load_config(state_dir, workspace)?;
    let store = TaskStore::open(&paths)?;
    let task = store.create_task(title)?;
    Ok(json!(task))
}

pub fn list_events_command(
    run_id: &str,
    limit: Option<usize>,
    state_dir: Option<PathBuf>,
    workspace: Option<PathBuf>,
) -> Result<Value> {
    let (_cfg, paths) = load_config(state_dir, workspace)?;
    let store = TaskStore::open(&paths)?;
    let events = store.list_events(run_id, limit)?;
    Ok(json!({ "events": events }))
}

pub fn follow_events_command(
    run_id: &str,
    poll_ms: u64,
    state_dir: Option<PathBuf>,
    workspace: Option<PathBuf>,
) -> Result<()> {
    let (_cfg, paths) = load_config(state_dir, workspace)?;
    let store = TaskStore::open(&paths)?;
    let run = store
        .get_run(run_id)?
        .with_context(|| format!("run id not found: {run_id}"))?;

    let mut cursor = 0_i64;
    let mut seen_ids = std::collections::HashSet::new();

    // Replay the recent event tail first so users get immediate context.
    let mut initial_events = store.list_events(run_id, Some(200))?;
    initial_events.reverse();
    for event in initial_events {
        print_follow_event(&event)?;
        cursor = cursor.max(event.ts_ms);
        seen_ids.insert(event.id);
    }

    let interval = Duration::from_millis(poll_ms.clamp(100, 10_000));
    if is_terminal_run_status(&run.status) {
        return Ok(());
    }

    loop {
        let after = cursor.saturating_sub(1);
        let events = store.list_events_after(run_id, after, 200)?;
        let mut emitted = 0usize;
        for event in events {
            if seen_ids.contains(&event.id) {
                continue;
            }
            print_follow_event(&event)?;
            cursor = cursor.max(event.ts_ms);
            seen_ids.insert(event.id);
            emitted += 1;
        }

        let run = store
            .get_run(run_id)?
            .with_context(|| format!("run disappeared while following events: {run_id}"))?;
        if emitted == 0 && is_terminal_run_status(&run.status) {
            break;
        }
        if emitted == 0 {
            thread::sleep(interval);
        }
    }
    Ok(())
}

pub fn export_audit_packet_command(
    run_id: &str,
    output: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    workspace: Option<PathBuf>,
) -> Result<Value> {
    let (_cfg, paths) = load_config(state_dir, workspace)?;
    let store = TaskStore::open(&paths)?;
    let events = store.list_events(run_id, None)?;
    let approvals = store.list_approvals(run_id)?;
    let artifacts = store.list_artifacts(run_id)?;
    let plugins = store.list_plugins(true)?;
    let audit_log = audit::read_audit_log(&audit::audit_dir(&paths), run_id, None)?;

    let packet = json!({
        "runId": run_id,
        "exportedAtMs": now_ms(),
        "events": events,
        "approvals": approvals,
        "artifacts": artifacts,
        "plugins": plugins,
        "auditLog": audit_log,
        "connectors": [],
    });

    if let Some(path) = output {
        write_json_value(&path, &packet)?;
    }

    Ok(packet)
}

pub fn run_task_server(bind: &str, state_dir: Option<PathBuf>, workspace: Option<PathBuf>) -> Result<()> {
    let (_cfg, paths) = load_config(state_dir, workspace)?;
    let server = Server::http(bind).map_err(|err| anyhow::anyhow!("bind task server {bind}: {err}"))?;

    for mut request in server.incoming_requests() {
        let response = match handle_task_request(&paths, &mut request) {
            Ok(resp) => resp,
            Err(err) => json_error_response(&err.to_string(), StatusCode(500)),
        };
        let _ = request.respond(response);
    }
    Ok(())
}

impl TaskEngine {
    pub fn new(cfg: ClawdConfig, paths: ClawdPaths) -> Self {
        Self { cfg, paths }
    }

    pub fn run_task(&self, opts: TaskRunOptions) -> Result<()> {
        let prepared = self.prepare_run(&opts)?;
        self.execute_run(prepared, opts.auto_approve, true, None)
    }

    pub fn start_task_async(&self, opts: TaskRunOptions) -> Result<TaskRun> {
        let prepared = self.prepare_run(&opts)?;
        let run = prepared.run.clone();
        let cfg = self.cfg.clone();
        let paths = self.paths.clone();
        let auto_approve = opts.auto_approve;
        thread::spawn(move || {
            let engine = TaskEngine { cfg, paths };
            let _ = engine.execute_run(prepared, auto_approve, false, None);
        });
        Ok(run)
    }

    pub fn start_task_async_with_broker(
        &self,
        opts: TaskRunOptions,
        broker: Arc<ApprovalBroker>,
    ) -> Result<TaskRun> {
        let prepared = self.prepare_run(&opts)?;
        let run = prepared.run.clone();
        let cfg = self.cfg.clone();
        let paths = self.paths.clone();
        let auto_approve = opts.auto_approve;
        thread::spawn(move || {
            let engine = TaskEngine { cfg, paths };
            let _ = engine.execute_run(prepared, auto_approve, false, Some(broker));
        });
        Ok(run)
    }
}

struct PreparedRun {
    task: Task,
    created: bool,
    run: TaskRun,
    prompt: String,
    codex_path: PathBuf,
    approval_policy: Option<AskForApproval>,
    thread_launch: ThreadLaunch,
}

enum ThreadLaunch {
    Start,
    Resume {
        source_run_id: String,
        source_thread_id: String,
    },
    Fork {
        source_run_id: String,
        source_thread_id: String,
    },
}

impl TaskEngine {
    fn prepare_run(&self, opts: &TaskRunOptions) -> Result<PreparedRun> {
        let store = TaskStore::open(&self.paths)?;
        if opts.resume_from_run_id.is_some() && opts.fork_from_run_id.is_some() {
            anyhow::bail!("cannot set both resume and fork source run ids");
        }
        if let Some(source_run_id) = opts.resume_from_run_id.as_deref() {
            return self.prepare_run_from_existing(
                &store,
                opts,
                source_run_id,
                |source_run_id, source_thread_id| ThreadLaunch::Resume {
                    source_run_id,
                    source_thread_id,
                },
            );
        }
        if let Some(source_run_id) = opts.fork_from_run_id.as_deref() {
            return self.prepare_run_from_existing(
                &store,
                opts,
                source_run_id,
                |source_run_id, source_thread_id| ThreadLaunch::Fork {
                    source_run_id,
                    source_thread_id,
                },
            );
        }
        let (task, created) = resolve_task(&store, opts)?;

        let approval_policy = opts
            .approval_policy
            .or_else(|| resolve_approval_policy(&self.cfg));

        let sandbox_label = if self.paths.workspace_policy.read_only {
            "read-only"
        } else {
            "workspace-write"
        };
        let run = store.create_run(
            &task.id,
            "running",
            None,
            Some(sandbox_label.to_string()),
            approval_policy.map(|p| format!("{p:?}")),
        )?;

        let prompt = resolve_prompt(opts)?;
        let codex_path = resolve_codex_path(&self.cfg, opts.codex_path.clone())?;

        Ok(PreparedRun {
            task,
            created,
            run,
            prompt,
            codex_path,
            approval_policy,
            thread_launch: ThreadLaunch::Start,
        })
    }

    fn prepare_run_from_existing<F>(
        &self,
        store: &TaskStore,
        opts: &TaskRunOptions,
        source_run_id: &str,
        launch: F,
    ) -> Result<PreparedRun>
    where
        F: FnOnce(String, String) -> ThreadLaunch,
    {
        let source_run = store
            .get_run(source_run_id)?
            .with_context(|| format!("source run not found: {source_run_id}"))?;
        let source_thread_id = source_run
            .codex_thread_id
            .clone()
            .with_context(|| format!("source run missing codex thread id: {source_run_id}"))?;
        let task = store
            .get_task(&source_run.task_id)?
            .with_context(|| format!("task missing for source run: {}", source_run.task_id))?;

        let approval_policy = opts
            .approval_policy
            .or_else(|| resolve_approval_policy(&self.cfg));
        let sandbox_label = if self.paths.workspace_policy.read_only {
            "read-only"
        } else {
            "workspace-write"
        };
        let run = store.create_run(
            &task.id,
            "running",
            Some(source_thread_id.clone()),
            Some(sandbox_label.to_string()),
            approval_policy.map(|p| format!("{p:?}")),
        )?;
        let prompt = resolve_prompt(opts)?;
        let codex_path = resolve_codex_path(&self.cfg, opts.codex_path.clone())?;

        Ok(PreparedRun {
            task,
            created: false,
            run,
            prompt,
            codex_path,
            approval_policy,
            thread_launch: launch(source_run_id.to_string(), source_thread_id),
        })
    }

    fn execute_run(
        &self,
        prepared: PreparedRun,
        auto_approve: bool,
        emit_output: bool,
        broker: Option<Arc<ApprovalBroker>>,
    ) -> Result<()> {
        let store = TaskStore::open(&self.paths)?;
        let PreparedRun {
            task,
            created,
            run,
            prompt,
            codex_path,
            approval_policy,
            thread_launch,
        } = prepared;

        let codex_home = self.paths.state_dir.join("codex");
        std::fs::create_dir_all(&codex_home)
            .with_context(|| format!("create {}", codex_home.display()))?;

        let mut env = Vec::new();
        env.push((
            "CODEX_HOME".to_string(),
            codex_home.to_string_lossy().to_string(),
        ));
        env.push((
            "CODEX_WORKSPACE_DIR".to_string(),
            self.paths.workspace_dir.to_string_lossy().to_string(),
        ));
        env.push((
            "CLAWDEX_TASK_RUN_ID".to_string(),
            run.id.clone(),
        ));

        let config_overrides = self
            .cfg
            .codex
            .as_ref()
            .and_then(|c| c.config_overrides.clone())
            .unwrap_or_default();

        let store_rc = Rc::new(RefCell::new(store));
        let event_sink = TaskEventSink::new(store_rc.clone(), run.id.clone());
        let approval_handler: Box<dyn ApprovalHandler> = if let Some(broker) = broker.clone() {
            Box::new(BrokerApprovalHandler::new(broker, run.id.clone()))
        } else {
            let approval_mode = if auto_approve {
                ApprovalPromptMode::AutoApprove
            } else {
                ApprovalPromptMode::Interactive
            };
            Box::new(TaskApprovalHandler::new(
                store_rc.clone(),
                run.id.clone(),
                approval_mode,
            ))
        };
        let user_input_handler: Box<dyn UserInputHandler> = if let Some(broker) = broker {
            Box::new(BrokerUserInputHandler::new(broker, run.id.clone()))
        } else {
            Box::new(TaskUserInputHandler::new(store_rc.clone(), run.id.clone()))
        };

        let mut client = CodexClient::spawn(&codex_path, &config_overrides, &env, ApprovalMode::AutoDeny)?;
        client.set_event_sink(Some(Box::new(event_sink)));
        client.set_approval_handler(Some(approval_handler));
        client.set_user_input_handler(Some(user_input_handler));
        client.initialize()?;

        let (thread_id, thread_event_kind, thread_event_payload) = match &thread_launch {
            ThreadLaunch::Start => {
                let thread_id = client.thread_start()?;
                (
                    thread_id.clone(),
                    "thread_started",
                    json!({ "threadId": thread_id }),
                )
            }
            ThreadLaunch::Resume {
                source_run_id,
                source_thread_id,
            } => {
                let thread_id = client.thread_resume(source_thread_id)?;
                (
                    thread_id.clone(),
                    "thread_resumed",
                    json!({
                        "threadId": thread_id,
                        "sourceRunId": source_run_id,
                        "sourceThreadId": source_thread_id,
                    }),
                )
            }
            ThreadLaunch::Fork {
                source_run_id,
                source_thread_id,
            } => {
                let thread_id = client.thread_fork(source_thread_id)?;
                (
                    thread_id.clone(),
                    "thread_forked",
                    json!({
                        "threadId": thread_id,
                        "sourceRunId": source_run_id,
                        "sourceThreadId": source_thread_id,
                    }),
                )
            }
        };
        {
            let store = store_rc.borrow();
            let _ = store.update_run_thread(&run.id, &thread_id);
            let _ = store.record_event(
                &run.id,
                thread_event_kind,
                &thread_event_payload,
            );
        }

        let sandbox_policy = workspace_sandbox_policy(&self.paths.workspace_policy)?;
        let outcome = client.run_turn(
            &thread_id,
            &prompt,
            approval_policy,
            sandbox_policy,
            Some(self.paths.workspace_dir.clone()),
        );

        let store = store_rc.borrow();
        match outcome {
            Ok(turn_outcome) => {
                store.update_run_status(&run.id, "completed")?;
                store.record_event(
                    &run.id,
                    "turn_completed",
                    &json!({ "message": turn_outcome.message, "warnings": turn_outcome.warnings }),
                )?;
                if let Ok(artifacts) = store.list_artifacts(&run.id) {
                    if !artifacts.is_empty() {
                        let _ = store.record_event(
                            &run.id,
                            "artifacts",
                            &json!({ "artifacts": artifacts }),
                        );
                        if emit_output {
                            println!("\n[task] outputs:");
                            for artifact in artifacts {
                                if let Some(mime) = artifact.mime.as_ref() {
                                    println!("  - {} ({})", artifact.path, mime);
                                } else {
                                    println!("  - {}", artifact.path);
                                }
                            }
                        }
                    }
                }
                if emit_output {
                    if created {
                        println!("[task] created {} ({})", task.id, task.title);
                    }
                    println!("{}", turn_outcome.message.trim());
                }
                Ok(())
            }
            Err(err) => {
                store.update_run_status(&run.id, "failed")?;
                store.record_event(
                    &run.id,
                    "turn_failed",
                    &json!({ "error": err.to_string() }),
                )?;
                Err(err)
            }
        }
    }
}

fn resolve_task(store: &TaskStore, opts: &TaskRunOptions) -> Result<(Task, bool)> {
    if let Some(ref task_id) = opts.task_id {
        let tasks = store.list_tasks()?;
        if let Some(task) = tasks.into_iter().find(|t| t.id == *task_id) {
            return Ok((task, false));
        }
        anyhow::bail!("task id not found");
    }
    if let Some(ref title) = opts.title {
        if let Some(task) = store.get_task_by_title(title)? {
            return Ok((task, false));
        }
        let task = store.create_task(title)?;
        return Ok((task, true));
    }
    anyhow::bail!("task run requires --task-id or --title");
}

fn resolve_prompt(opts: &TaskRunOptions) -> Result<String> {
    if let Some(ref prompt) = opts.prompt {
        if !prompt.trim().is_empty() {
            return Ok(prompt.clone());
        }
    }
    let mut input = String::new();
    io::stdin().read_to_string(&mut input)?;
    if input.trim().is_empty() {
        anyhow::bail!("prompt required (pass --prompt or pipe via stdin)");
    }
    Ok(input)
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

fn resolve_approval_policy(cfg: &ClawdConfig) -> Option<AskForApproval> {
    let raw = cfg
        .codex
        .as_ref()
        .and_then(|c| c.approval_policy.as_ref())
        .map(|s| s.to_lowercase())
        .unwrap_or_else(|| "on-request".to_string());
    let policy = match raw.as_str() {
        "never" => AskForApproval::Never,
        "on-failure" | "onfailure" => AskForApproval::OnFailure,
        "unless-trusted" | "unlesstrusted" => AskForApproval::UnlessTrusted,
        _ => AskForApproval::OnRequest,
    };
    Some(policy)
}

fn print_follow_event(event: &TaskEvent) -> Result<()> {
    let payload = serde_json::to_string(&event.payload)?;
    println!("{} {} {}", event.ts_ms, event.kind, payload);
    Ok(())
}

fn is_terminal_run_status(status: &str) -> bool {
    matches!(
        status.to_ascii_lowercase().as_str(),
        "completed" | "failed" | "cancelled" | "canceled" | "interrupted"
    )
}

struct TaskEventSink {
    store: Rc<RefCell<TaskStore>>,
    run_id: String,
}

impl TaskEventSink {
    fn new(store: Rc<RefCell<TaskStore>>, run_id: String) -> Self {
        Self { store, run_id }
    }
}

impl EventSink for TaskEventSink {
    fn record_event(&mut self, kind: &str, payload: &Value) {
        let _ = self.store.borrow().record_event(&self.run_id, kind, payload);
    }
}

enum ApprovalPromptMode {
    Interactive,
    AutoApprove,
}

struct TaskApprovalHandler {
    store: Rc<RefCell<TaskStore>>,
    run_id: String,
    mode: ApprovalPromptMode,
}

impl TaskApprovalHandler {
    fn new(store: Rc<RefCell<TaskStore>>, run_id: String, mode: ApprovalPromptMode) -> Self {
        Self { store, run_id, mode }
    }

    fn record_decision(&self, kind: &str, request: &Value, decision: &str) {
        let _ = self
            .store
            .borrow()
            .record_approval(&self.run_id, kind, request, Some(decision));
    }
}

impl ApprovalHandler for TaskApprovalHandler {
    fn command_decision(
        &mut self,
        params: &CommandExecutionRequestApprovalParams,
    ) -> CommandExecutionApprovalDecision {
        let request = serde_json::to_value(params).unwrap_or(Value::Null);
        match self.mode {
            ApprovalPromptMode::AutoApprove => {
                self.record_decision("command", &request, "accept");
                return CommandExecutionApprovalDecision::Accept;
            }
            ApprovalPromptMode::Interactive => {}
        }

        {
            println!("\n[approval] Command execution requested");
            if let Some(cmd) = params.command.as_deref() {
                println!("  command: {}", cmd);
            }
            if let Some(reason) = params.reason.as_deref() {
                println!("  reason: {}", reason);
            }
            if let Some(cwd) = params.cwd.as_ref() {
                println!("  cwd: {}", cwd.display());
            }
            if prompt_yes_no("Approve this command? [y/N] ") {
                self.record_decision("command", &request, "accept");
                return CommandExecutionApprovalDecision::Accept;
            }
            self.record_decision("command", &request, "decline");
            return CommandExecutionApprovalDecision::Decline;
        }
    }

    fn file_decision(
        &mut self,
        params: &FileChangeRequestApprovalParams,
    ) -> FileChangeApprovalDecision {
        let request = serde_json::to_value(params).unwrap_or(Value::Null);
        match self.mode {
            ApprovalPromptMode::AutoApprove => {
                self.record_decision("file_change", &request, "accept");
                return FileChangeApprovalDecision::Accept;
            }
            ApprovalPromptMode::Interactive => {}
        }

        {
            println!("\n[approval] File change requested");
            if let Some(reason) = params.reason.as_deref() {
                println!("  reason: {}", reason);
            }
            if let Some(root) = params.grant_root.as_ref() {
                println!("  grant root: {}", root.display());
            }
            if prompt_yes_no("Approve file changes? [y/N] ") {
                self.record_decision("file_change", &request, "accept");
                return FileChangeApprovalDecision::Accept;
            }
            self.record_decision("file_change", &request, "decline");
            return FileChangeApprovalDecision::Decline;
        }
    }
}

struct TaskUserInputHandler {
    store: Rc<RefCell<TaskStore>>,
    run_id: String,
}

impl TaskUserInputHandler {
    fn new(store: Rc<RefCell<TaskStore>>, run_id: String) -> Self {
        Self { store, run_id }
    }

    fn record_input(&self, params: &ToolRequestUserInputParams, answers: &HashMap<String, ToolRequestUserInputAnswer>) {
        let payload = json!({
            "threadId": params.thread_id,
            "turnId": params.turn_id,
            "itemId": params.item_id,
            "answers": answers,
        });
        let _ = self
            .store
            .borrow()
            .record_event(&self.run_id, "tool_user_input", &payload);
    }
}

impl UserInputHandler for TaskUserInputHandler {
    fn request_user_input(
        &mut self,
        params: &ToolRequestUserInputParams,
    ) -> HashMap<String, ToolRequestUserInputAnswer> {
        println!("\n[input] Codex requested user input");
        let mut answers = HashMap::new();
        for question in &params.questions {
            println!("\n{}", question.header);
            println!("{}", question.question);
            if let Some(options) = &question.options {
                for (idx, option) in options.iter().enumerate() {
                    println!("  {}) {} - {}", idx + 1, option.label, option.description);
                }
                if question.is_other {
                    println!("  {}) Other", options.len() + 1);
                }
                let selection = prompt_text("Select option: ");
                let answer = selection.trim().to_string();
                answers.insert(
                    question.id.clone(),
                    ToolRequestUserInputAnswer { answers: vec![answer] },
                );
            } else {
                let response = prompt_text("Answer: ");
                answers.insert(
                    question.id.clone(),
                    ToolRequestUserInputAnswer {
                        answers: vec![response],
                    },
                );
            }
        }
        self.record_input(params, &answers);
        answers
    }
}

fn prompt_yes_no(prompt: &str) -> bool {
    print!("{}", prompt);
    let _ = io::stdout().flush();
    let mut input = String::new();
    if io::stdin().read_line(&mut input).is_err() {
        return false;
    }
    matches!(input.trim().to_lowercase().as_str(), "y" | "yes")
}

fn prompt_text(prompt: &str) -> String {
    print!("{}", prompt);
    let _ = io::stdout().flush();
    let mut input = String::new();
    let _ = io::stdin().read_line(&mut input);
    input.trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::is_terminal_run_status;

    #[test]
    fn terminal_status_detection_handles_supported_values() {
        assert!(is_terminal_run_status("completed"));
        assert!(is_terminal_run_status("failed"));
        assert!(is_terminal_run_status("cancelled"));
        assert!(is_terminal_run_status("canceled"));
        assert!(is_terminal_run_status("interrupted"));
        assert!(!is_terminal_run_status("running"));
    }
}

fn handle_task_request(
    paths: &ClawdPaths,
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
        _ => {
            if method == Method::Get && url.starts_with("/v1/runs/") {
                let run_id = url.trim_start_matches("/v1/runs/");
                let run_id = run_id.trim_end_matches("/events");
                let store = TaskStore::open(paths)?;
                let events = store.list_events(run_id, Some(200))?;
                return Ok(json_response(json!({ "events": events }))?);
            }
            Ok(Response::from_data(Vec::new()).with_status_code(StatusCode(404)))
        }
    }
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
    match json_response(json!({ "ok": false, "error": message })) {
        Ok(resp) => resp.with_status_code(status),
        Err(_) => Response::from_string("error").with_status_code(status),
    }
}
