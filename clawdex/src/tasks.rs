use std::cell::RefCell;
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::rc::Rc;

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
use crate::task_db::{Task, TaskStore};

pub struct TaskEngine {
    cfg: ClawdConfig,
    paths: ClawdPaths,
}

pub struct TaskRunOptions {
    pub codex_path: Option<PathBuf>,
    pub workspace: Option<PathBuf>,
    pub state_dir: Option<PathBuf>,
    pub auto_approve: bool,
    pub approval_policy: Option<AskForApproval>,
    pub prompt: Option<String>,
    pub title: Option<String>,
    pub task_id: Option<String>,
}

pub fn run_task_command(opts: TaskRunOptions) -> Result<()> {
    let (cfg, paths) = load_config(opts.state_dir.clone(), opts.workspace.clone())?;
    let engine = TaskEngine { cfg, paths };
    engine.run_task(opts)
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
    pub fn run_task(&self, opts: TaskRunOptions) -> Result<()> {
        let store = TaskStore::open(&self.paths)?;
        let (task, created) = resolve_task(&store, &opts)?;

        let approval_policy = opts
            .approval_policy
            .or_else(|| resolve_approval_policy(&self.cfg));

        let run = store.create_run(
            &task.id,
            "running",
            None,
            Some("workspace-write".to_string()),
            approval_policy.map(|p| format!("{p:?}")),
        )?;

        let prompt = resolve_prompt(&opts)?;
        let codex_path = resolve_codex_path(&self.cfg, opts.codex_path)?;

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

        let config_overrides = self
            .cfg
            .codex
            .as_ref()
            .and_then(|c| c.config_overrides.clone())
            .unwrap_or_default();

        let store_rc = Rc::new(RefCell::new(store));
        let event_sink = TaskEventSink::new(store_rc.clone(), run.id.clone());
        let approval_mode = if opts.auto_approve {
            ApprovalPromptMode::AutoApprove
        } else {
            ApprovalPromptMode::Interactive
        };
        let approval_handler =
            TaskApprovalHandler::new(store_rc.clone(), run.id.clone(), approval_mode);
        let user_input_handler = TaskUserInputHandler::new(store_rc.clone(), run.id.clone());

        let mut client = CodexClient::spawn(&codex_path, &config_overrides, &env, ApprovalMode::AutoDeny)?;
        client.set_event_sink(Some(Box::new(event_sink)));
        client.set_approval_handler(Some(Box::new(approval_handler)));
        client.set_user_input_handler(Some(Box::new(user_input_handler)));
        client.initialize()?;

        let thread_id = client.thread_start()?;
        {
            let store = store_rc.borrow();
            let _ = store.update_run_thread(&run.id, &thread_id);
            let _ = store.record_event(
                &run.id,
                "thread_started",
                &json!({ "threadId": thread_id }),
            );
        }

        let outcome = client.run_turn(
            &thread_id,
            &prompt,
            approval_policy,
            None,
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
                if created {
                    println!("[task] created {} ({})", task.id, task.title);
                }
                println!("{}", turn_outcome.message.trim());
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
