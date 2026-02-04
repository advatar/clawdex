use std::collections::HashMap;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use codex_app_server_protocol::{
    CommandExecutionApprovalDecision, CommandExecutionRequestApprovalParams,
    FileChangeApprovalDecision, FileChangeRequestApprovalParams, ToolRequestUserInputAnswer,
    ToolRequestUserInputParams,
};
use serde::Serialize;
use serde_json::Value;
use uuid::Uuid;

use crate::task_db::TaskStore;
use crate::util::now_ms;
use crate::config::ClawdPaths;

const APPROVAL_TIMEOUT_SECS: u64 = 60 * 30;

#[derive(Debug, Clone, Serialize)]
pub struct PendingApproval {
    pub id: String,
    pub run_id: String,
    pub kind: String,
    pub request: Value,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Serialize)]
pub struct PendingUserInput {
    pub id: String,
    pub run_id: String,
    pub params: Value,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, Copy)]
pub enum ApprovalDecision {
    Accept,
    Decline,
    Cancel,
}

pub struct ApprovalBroker {
    paths: ClawdPaths,
    approvals: Mutex<HashMap<String, (PendingApproval, mpsc::Sender<ApprovalDecision>)>>,
    inputs: Mutex<HashMap<String, (PendingUserInput, mpsc::Sender<HashMap<String, ToolRequestUserInputAnswer>>)>>,
}

impl ApprovalBroker {
    pub fn new(paths: ClawdPaths) -> Self {
        Self {
            paths,
            approvals: Mutex::new(HashMap::new()),
            inputs: Mutex::new(HashMap::new()),
        }
    }

    pub fn list_pending_approvals(&self) -> Vec<PendingApproval> {
        self.approvals
            .lock()
            .map(|map| map.values().map(|(pending, _)| pending.clone()).collect())
            .unwrap_or_default()
    }

    pub fn list_pending_inputs(&self) -> Vec<PendingUserInput> {
        self.inputs
            .lock()
            .map(|map| map.values().map(|(pending, _)| pending.clone()).collect())
            .unwrap_or_default()
    }

    pub fn resolve_approval(&self, id: &str, decision: ApprovalDecision) -> bool {
        let sender = match self.approvals.lock() {
            Ok(mut map) => map.remove(id).map(|(_, sender)| sender),
            Err(_) => None,
        };
        if let Some(sender) = sender {
            let _ = sender.send(decision);
            true
        } else {
            false
        }
    }

    pub fn resolve_user_input(
        &self,
        id: &str,
        answers: HashMap<String, ToolRequestUserInputAnswer>,
    ) -> bool {
        let sender = match self.inputs.lock() {
            Ok(mut map) => map.remove(id).map(|(_, sender)| sender),
            Err(_) => None,
        };
        if let Some(sender) = sender {
            let _ = sender.send(answers);
            true
        } else {
            false
        }
    }

    pub fn request_command_approval(
        &self,
        run_id: &str,
        params: &CommandExecutionRequestApprovalParams,
    ) -> CommandExecutionApprovalDecision {
        let request = serde_json::to_value(params).unwrap_or(Value::Null);
        let decision = self.request_approval(run_id, "command", request);
        match decision {
            ApprovalDecision::Accept => CommandExecutionApprovalDecision::Accept,
            ApprovalDecision::Cancel => CommandExecutionApprovalDecision::Cancel,
            _ => CommandExecutionApprovalDecision::Decline,
        }
    }

    pub fn request_file_approval(
        &self,
        run_id: &str,
        params: &FileChangeRequestApprovalParams,
    ) -> FileChangeApprovalDecision {
        let request = serde_json::to_value(params).unwrap_or(Value::Null);
        let decision = self.request_approval(run_id, "file_change", request);
        match decision {
            ApprovalDecision::Accept => FileChangeApprovalDecision::Accept,
            ApprovalDecision::Cancel => FileChangeApprovalDecision::Cancel,
            _ => FileChangeApprovalDecision::Decline,
        }
    }

    pub fn request_user_input(
        &self,
        run_id: &str,
        params: &ToolRequestUserInputParams,
    ) -> HashMap<String, ToolRequestUserInputAnswer> {
        let request = serde_json::to_value(params).unwrap_or(Value::Null);
        let (tx, rx) = mpsc::channel();
        let id = Uuid::new_v4().to_string();
        let pending = PendingUserInput {
            id: id.clone(),
            run_id: run_id.to_string(),
            params: request,
            created_at_ms: now_ms(),
        };
        if let Ok(mut map) = self.inputs.lock() {
            map.insert(id.clone(), (pending, tx));
        }

        let result = rx
            .recv_timeout(Duration::from_secs(APPROVAL_TIMEOUT_SECS))
            .unwrap_or_default();

        result
    }

    fn request_approval(&self, run_id: &str, kind: &str, request: Value) -> ApprovalDecision {
        let request_for_record = request.clone();
        let (tx, rx) = mpsc::channel();
        let id = Uuid::new_v4().to_string();
        let pending = PendingApproval {
            id: id.clone(),
            run_id: run_id.to_string(),
            kind: kind.to_string(),
            request,
            created_at_ms: now_ms(),
        };
        if let Ok(mut map) = self.approvals.lock() {
            map.insert(id.clone(), (pending, tx));
        }

        let decision = rx
            .recv_timeout(Duration::from_secs(APPROVAL_TIMEOUT_SECS))
            .unwrap_or(ApprovalDecision::Decline);

        let decision_str = match decision {
            ApprovalDecision::Accept => "accept",
            ApprovalDecision::Cancel => "cancel",
            ApprovalDecision::Decline => "decline",
        };
        if let Ok(store) = TaskStore::open(&self.paths) {
            let _ = store.record_approval(run_id, kind, &request_for_record, Some(decision_str));
        }

        decision
    }
}

#[derive(Clone)]
pub struct BrokerApprovalHandler {
    broker: Arc<ApprovalBroker>,
    run_id: String,
}

impl BrokerApprovalHandler {
    pub fn new(broker: Arc<ApprovalBroker>, run_id: String) -> Self {
        Self { broker, run_id }
    }
}

impl crate::app_server::ApprovalHandler for BrokerApprovalHandler {
    fn command_decision(
        &mut self,
        params: &CommandExecutionRequestApprovalParams,
    ) -> CommandExecutionApprovalDecision {
        self.broker.request_command_approval(&self.run_id, params)
    }

    fn file_decision(
        &mut self,
        params: &FileChangeRequestApprovalParams,
    ) -> FileChangeApprovalDecision {
        self.broker.request_file_approval(&self.run_id, params)
    }
}

#[derive(Clone)]
pub struct BrokerUserInputHandler {
    broker: Arc<ApprovalBroker>,
    run_id: String,
}

impl BrokerUserInputHandler {
    pub fn new(broker: Arc<ApprovalBroker>, run_id: String) -> Self {
        Self { broker, run_id }
    }
}

impl crate::app_server::UserInputHandler for BrokerUserInputHandler {
    fn request_user_input(
        &mut self,
        params: &ToolRequestUserInputParams,
    ) -> HashMap<String, ToolRequestUserInputAnswer> {
        self.broker.request_user_input(&self.run_id, params)
    }
}
