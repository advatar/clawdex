use std::collections::{HashMap, VecDeque};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

use anyhow::{Context, Result};
use codex_app_server_protocol::{
    AskForApproval, ClientInfo, ClientRequest, CommandExecutionApprovalDecision,
    CommandExecutionRequestApprovalParams, CommandExecutionRequestApprovalResponse,
    FileChangeApprovalDecision, FileChangeRequestApprovalParams,
    FileChangeRequestApprovalResponse, InitializeCapabilities, InitializeParams, JSONRPCMessage,
    JSONRPCNotification, JSONRPCRequest, JSONRPCResponse, RequestId, ServerNotification,
    SandboxPolicy, ServerRequest, ThreadStartParams, ToolRequestUserInputAnswer,
    ToolRequestUserInputParams, ToolRequestUserInputResponse, TurnStartParams, TurnStatus,
    UserInput as V2UserInput,
};
use serde::de::DeserializeOwned;
use serde::Serialize;
use uuid::Uuid;

#[derive(Debug, Clone, Copy)]
pub enum ApprovalMode {
    AutoApprove,
    AutoDeny,
}

impl ApprovalMode {
    pub fn from_env() -> Self {
        match std::env::var("CLAWDEX_APPROVAL_MODE")
            .unwrap_or_else(|_| "deny".to_string())
            .to_lowercase()
            .as_str()
        {
            "approve" | "auto-approve" | "allow" => ApprovalMode::AutoApprove,
            _ => ApprovalMode::AutoDeny,
        }
    }

    fn decision(self) -> CommandExecutionApprovalDecision {
        match self {
            ApprovalMode::AutoApprove => CommandExecutionApprovalDecision::Accept,
            ApprovalMode::AutoDeny => CommandExecutionApprovalDecision::Decline,
        }
    }

    fn file_decision(self) -> FileChangeApprovalDecision {
        match self {
            ApprovalMode::AutoApprove => FileChangeApprovalDecision::Accept,
            ApprovalMode::AutoDeny => FileChangeApprovalDecision::Decline,
        }
    }
}

pub trait ApprovalHandler {
    fn command_decision(
        &mut self,
        params: &CommandExecutionRequestApprovalParams,
    ) -> CommandExecutionApprovalDecision;
    fn file_decision(
        &mut self,
        params: &FileChangeRequestApprovalParams,
    ) -> FileChangeApprovalDecision;
}

pub trait UserInputHandler {
    fn request_user_input(
        &mut self,
        params: &ToolRequestUserInputParams,
    ) -> HashMap<String, ToolRequestUserInputAnswer>;
}

pub trait EventSink {
    fn record_event(&mut self, kind: &str, payload: &serde_json::Value);
}

pub struct AutoApprovalHandler {
    mode: ApprovalMode,
}

impl AutoApprovalHandler {
    pub fn new(mode: ApprovalMode) -> Self {
        Self { mode }
    }
}

impl ApprovalHandler for AutoApprovalHandler {
    fn command_decision(
        &mut self,
        _params: &CommandExecutionRequestApprovalParams,
    ) -> CommandExecutionApprovalDecision {
        self.mode.decision()
    }

    fn file_decision(
        &mut self,
        _params: &FileChangeRequestApprovalParams,
    ) -> FileChangeApprovalDecision {
        self.mode.file_decision()
    }
}

pub struct AutoUserInputHandler;

impl UserInputHandler for AutoUserInputHandler {
    fn request_user_input(
        &mut self,
        _params: &ToolRequestUserInputParams,
    ) -> HashMap<String, ToolRequestUserInputAnswer> {
        HashMap::new()
    }
}

#[derive(Debug)]
pub struct TurnOutcome {
    pub message: String,
    pub warnings: Vec<String>,
}

pub struct CodexClient {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    pending_notifications: VecDeque<JSONRPCNotification>,
    approval_handler: Option<Box<dyn ApprovalHandler>>,
    user_input_handler: Option<Box<dyn UserInputHandler>>,
    event_sink: Option<Box<dyn EventSink>>,
    warnings: Vec<String>,
}

impl CodexClient {
    pub fn spawn(
        codex_bin: &Path,
        config_overrides: &[String],
        extra_env: &[(String, String)],
        approval_mode: ApprovalMode,
    ) -> Result<Self> {
        let mut cmd = Command::new(codex_bin);
        for kv in config_overrides {
            cmd.arg("--config").arg(kv);
        }
        let mut child = cmd
            .arg("app-server")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .envs(extra_env.iter().cloned())
            .spawn()
            .with_context(|| format!("failed to start `{}` app-server", codex_bin.display()))?;

        let stdin = child.stdin.take().context("codex stdin unavailable")?;
        let stdout = child.stdout.take().context("codex stdout unavailable")?;

        Ok(Self {
            child,
            stdin: Some(stdin),
            stdout: BufReader::new(stdout),
            pending_notifications: VecDeque::new(),
            approval_handler: Some(Box::new(AutoApprovalHandler::new(approval_mode))),
            user_input_handler: Some(Box::new(AutoUserInputHandler)),
            event_sink: None,
            warnings: Vec::new(),
        })
    }

    pub fn set_event_sink(&mut self, sink: Option<Box<dyn EventSink>>) {
        self.event_sink = sink;
    }

    pub fn set_approval_handler(&mut self, handler: Option<Box<dyn ApprovalHandler>>) {
        self.approval_handler = handler;
    }

    pub fn set_user_input_handler(&mut self, handler: Option<Box<dyn UserInputHandler>>) {
        self.user_input_handler = handler;
    }

    pub fn initialize(&mut self) -> Result<()> {
        let request_id = self.request_id();
        let request = ClientRequest::Initialize {
            request_id: request_id.clone(),
            params: InitializeParams {
                client_info: ClientInfo {
                    name: "clawdex-ui-bridge".to_string(),
                    title: Some("Clawdex UI Bridge".to_string()),
                    version: env!("CARGO_PKG_VERSION").to_string(),
                },
                capabilities: Some(InitializeCapabilities {
                    experimental_api: true,
                }),
            },
        };
        let _: codex_app_server_protocol::InitializeResponse =
            self.send_request(request, request_id, "initialize")?;
        Ok(())
    }

    pub fn thread_start(&mut self) -> Result<String> {
        let request_id = self.request_id();
        let request = ClientRequest::ThreadStart {
            request_id: request_id.clone(),
            params: ThreadStartParams::default(),
        };
        let response: codex_app_server_protocol::ThreadStartResponse =
            self.send_request(request, request_id, "thread/start")?;
        Ok(response.thread.id)
    }

    pub fn run_turn(
        &mut self,
        thread_id: &str,
        message: &str,
        approval_policy: Option<AskForApproval>,
        sandbox_policy: Option<SandboxPolicy>,
        cwd: Option<std::path::PathBuf>,
    ) -> Result<TurnOutcome> {
        self.run_turn_with_inputs(
            thread_id,
            vec![V2UserInput::Text {
                text: message.to_string(),
                text_elements: Vec::new(),
            }],
            approval_policy,
            sandbox_policy,
            cwd,
        )
    }

    pub fn run_turn_with_inputs(
        &mut self,
        thread_id: &str,
        input: Vec<V2UserInput>,
        approval_policy: Option<AskForApproval>,
        sandbox_policy: Option<SandboxPolicy>,
        cwd: Option<std::path::PathBuf>,
    ) -> Result<TurnOutcome> {
        let request_id = self.request_id();
        let mut params = TurnStartParams {
            thread_id: thread_id.to_string(),
            input,
            ..Default::default()
        };
        params.approval_policy = approval_policy;
        params.sandbox_policy = sandbox_policy;
        params.cwd = cwd;

        let request = ClientRequest::TurnStart {
            request_id: request_id.clone(),
            params,
        };
        let response: codex_app_server_protocol::TurnStartResponse =
            self.send_request(request, request_id, "turn/start")?;

        let outcome = self.stream_turn(thread_id, &response.turn.id)?;
        Ok(outcome)
    }

    fn stream_turn(&mut self, thread_id: &str, turn_id: &str) -> Result<TurnOutcome> {
        let mut delta = String::new();
        let mut last_agent_message: Option<String> = None;

        loop {
            let notification = self.next_notification()?;
            let Ok(server_notification) = ServerNotification::try_from(notification) else {
                continue;
            };
            if let Some(sink) = self.event_sink.as_mut() {
                if let Ok(payload) = serde_json::to_value(&server_notification) {
                    sink.record_event(notification_kind(&server_notification), &payload);
                }
            }

            match server_notification {
                ServerNotification::AgentMessageDelta(payload) => {
                    if payload.thread_id == thread_id && payload.turn_id == turn_id {
                        delta.push_str(&payload.delta);
                    }
                }
                ServerNotification::ItemCompleted(payload) => {
                    if payload.thread_id == thread_id && payload.turn_id == turn_id {
                        if let codex_app_server_protocol::ThreadItem::AgentMessage { text, .. } =
                            payload.item
                        {
                            last_agent_message = Some(text);
                        }
                    }
                }
                ServerNotification::TurnCompleted(payload) => {
                    if payload.thread_id == thread_id && payload.turn.id == turn_id {
                        if payload.turn.status == TurnStatus::Failed {
                            if let Some(err) = payload.turn.error {
                                return Err(anyhow::anyhow!(err.message));
                            }
                        }
                        break;
                    }
                }
                ServerNotification::Error(payload) => {
                    if payload.thread_id == thread_id && payload.turn_id == turn_id {
                        self.warnings.push(payload.error.message);
                    }
                }
                _ => {}
            }
        }

        let message = if !delta.is_empty() {
            delta
        } else if let Some(text) = last_agent_message {
            text
        } else {
            String::new()
        };
        let warnings = std::mem::take(&mut self.warnings);
        Ok(TurnOutcome { message, warnings })
    }

    fn send_request<T>(
        &mut self,
        request: ClientRequest,
        request_id: RequestId,
        method: &str,
    ) -> Result<T>
    where
        T: DeserializeOwned,
    {
        self.write_request(&request)?;
        self.wait_for_response(request_id, method)
    }

    fn write_request(&mut self, request: &ClientRequest) -> Result<()> {
        let payload = serde_json::to_string(request)?;
        if let Some(stdin) = self.stdin.as_mut() {
            writeln!(stdin, "{payload}")?;
            stdin.flush().context("flush request")?;
            Ok(())
        } else {
            anyhow::bail!("codex app-server stdin closed")
        }
    }

    fn wait_for_response<T>(&mut self, request_id: RequestId, method: &str) -> Result<T>
    where
        T: DeserializeOwned,
    {
        loop {
            let message = self.read_jsonrpc_message()?;
            match message {
                JSONRPCMessage::Response(JSONRPCResponse { id, result }) => {
                    if id == request_id {
                        return serde_json::from_value(result)
                            .with_context(|| format!("{method} response missing payload"));
                    }
                }
                JSONRPCMessage::Error(err) => {
                    if err.id == request_id {
                        anyhow::bail!("{method} failed: {err:?}");
                    }
                }
                JSONRPCMessage::Notification(notification) => {
                    self.pending_notifications.push_back(notification);
                }
                JSONRPCMessage::Request(request) => {
                    self.handle_server_request(request)?;
                }
            }
        }
    }

    fn next_notification(&mut self) -> Result<JSONRPCNotification> {
        if let Some(notification) = self.pending_notifications.pop_front() {
            return Ok(notification);
        }

        loop {
            let message = self.read_jsonrpc_message()?;
            match message {
                JSONRPCMessage::Notification(notification) => return Ok(notification),
                JSONRPCMessage::Response(_) | JSONRPCMessage::Error(_) => {
                    continue;
                }
                JSONRPCMessage::Request(request) => {
                    self.handle_server_request(request)?;
                }
            }
        }
    }

    fn read_jsonrpc_message(&mut self) -> Result<JSONRPCMessage> {
        loop {
            let mut response_line = String::new();
            let bytes = self
                .stdout
                .read_line(&mut response_line)
                .context("read codex app-server")?;
            if bytes == 0 {
                anyhow::bail!("codex app-server closed stdout");
            }
            let trimmed = response_line.trim();
            if trimmed.is_empty() {
                continue;
            }
            let parsed: serde_json::Value = serde_json::from_str(trimmed)
                .context("invalid JSON-RPC from codex app-server")?;
            let message: JSONRPCMessage = serde_json::from_value(parsed)
                .context("invalid JSON-RPC message")?;
            return Ok(message);
        }
    }

    fn handle_server_request(&mut self, request: JSONRPCRequest) -> Result<()> {
        let server_request = ServerRequest::try_from(request)
            .context("failed to deserialize ServerRequest")?;

        match server_request {
            ServerRequest::CommandExecutionRequestApproval { request_id, params } => {
                self.handle_command_execution_request_approval(request_id, params)?;
            }
            ServerRequest::FileChangeRequestApproval { request_id, params } => {
                self.handle_file_change_request_approval(request_id, params)?;
            }
            ServerRequest::ToolRequestUserInput { request_id, params } => {
                let answers = if let Some(handler) = self.user_input_handler.as_mut() {
                    handler.request_user_input(&params)
                } else {
                    self.warnings
                        .push("tool requested user input; not supported".to_string());
                    HashMap::new()
                };
                let response = ToolRequestUserInputResponse { answers };
                self.send_server_request_response(request_id, &response)?;
            }
            _ => {
                self.warnings
                    .push("unsupported server request from codex".to_string());
            }
        }
        Ok(())
    }

    fn handle_command_execution_request_approval(
        &mut self,
        request_id: RequestId,
        params: CommandExecutionRequestApprovalParams,
    ) -> Result<()> {
        let decision = if let Some(handler) = self.approval_handler.as_mut() {
            handler.command_decision(&params)
        } else {
            CommandExecutionApprovalDecision::Decline
        };
        let response = CommandExecutionRequestApprovalResponse { decision };
        self.send_server_request_response(request_id, &response)?;
        Ok(())
    }

    fn handle_file_change_request_approval(
        &mut self,
        request_id: RequestId,
        params: FileChangeRequestApprovalParams,
    ) -> Result<()> {
        let decision = if let Some(handler) = self.approval_handler.as_mut() {
            handler.file_decision(&params)
        } else {
            FileChangeApprovalDecision::Decline
        };
        let response = FileChangeRequestApprovalResponse { decision };
        self.send_server_request_response(request_id, &response)?;
        Ok(())
    }

    fn send_server_request_response<T>(&mut self, request_id: RequestId, response: &T) -> Result<()>
    where
        T: Serialize,
    {
        let message = JSONRPCMessage::Response(JSONRPCResponse {
            id: request_id,
            result: serde_json::to_value(response)?,
        });
        self.write_jsonrpc_message(message)
    }

    fn write_jsonrpc_message(&mut self, message: JSONRPCMessage) -> Result<()> {
        let payload = serde_json::to_string(&message)?;
        if let Some(stdin) = self.stdin.as_mut() {
            writeln!(stdin, "{payload}")?;
            stdin.flush().context("flush response")?;
            Ok(())
        } else {
            anyhow::bail!("codex app-server stdin closed")
        }
    }

    fn request_id(&self) -> RequestId {
        RequestId::String(Uuid::new_v4().to_string())
    }
}

fn notification_kind(notification: &ServerNotification) -> &'static str {
    #[allow(unreachable_patterns)]
    match notification {
        ServerNotification::Error(_) => "error",
        ServerNotification::ThreadStarted(_) => "thread_started",
        ServerNotification::AppListUpdated(_) => "app_list_updated",
        ServerNotification::ThreadNameUpdated(_) => "thread_name_updated",
        ServerNotification::ThreadTokenUsageUpdated(_) => "thread_token_usage_updated",
        ServerNotification::TurnStarted(_) => "turn_started",
        ServerNotification::TurnCompleted(_) => "turn_completed",
        ServerNotification::TurnDiffUpdated(_) => "turn_diff_updated",
        ServerNotification::TurnPlanUpdated(_) => "turn_plan_updated",
        ServerNotification::ItemStarted(_) => "item_started",
        ServerNotification::ItemCompleted(_) => "item_completed",
        ServerNotification::RawResponseItemCompleted(_) => "raw_response_item_completed",
        ServerNotification::AgentMessageDelta(_) => "agent_message_delta",
        ServerNotification::PlanDelta(_) => "plan_delta",
        ServerNotification::CommandExecutionOutputDelta(_) => "command_execution_output_delta",
        ServerNotification::TerminalInteraction(_) => "terminal_interaction",
        ServerNotification::FileChangeOutputDelta(_) => "file_change_output_delta",
        ServerNotification::McpToolCallProgress(_) => "mcp_tool_call_progress",
        ServerNotification::McpServerOauthLoginCompleted(_) => "mcp_server_oauth_login_completed",
        ServerNotification::AccountUpdated(_) => "account_updated",
        ServerNotification::AccountRateLimitsUpdated(_) => "account_rate_limits_updated",
        ServerNotification::ReasoningSummaryTextDelta(_) => "reasoning_summary_text_delta",
        ServerNotification::ReasoningSummaryPartAdded(_) => "reasoning_summary_part_added",
        ServerNotification::ReasoningTextDelta(_) => "reasoning_text_delta",
        ServerNotification::ContextCompacted(_) => "context_compacted",
        ServerNotification::DeprecationNotice(_) => "deprecation_notice",
        ServerNotification::ConfigWarning(_) => "config_warning",
        ServerNotification::WindowsWorldWritableWarning(_) => "windows_world_writable_warning",
        ServerNotification::AccountLoginCompleted(_) => "account_login_completed",
        ServerNotification::AuthStatusChange(_) => "auth_status_change",
        ServerNotification::LoginChatGptComplete(_) => "login_chatgpt_complete",
        ServerNotification::SessionConfigured(_) => "session_configured",
        _ => "unknown",
    }
}

impl Drop for CodexClient {
    fn drop(&mut self) {
        let _ = self.stdin.take();

        if let Ok(Some(_status)) = self.child.try_wait() {
            return;
        }

        std::thread::sleep(Duration::from_millis(100));
        if let Ok(Some(_status)) = self.child.try_wait() {
            return;
        }

        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
