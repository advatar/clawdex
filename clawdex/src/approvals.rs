use std::collections::HashMap;
use std::sync::{mpsc, Arc, Mutex};
use std::time::Duration;

use codex_app_server_protocol::{
    CommandExecutionApprovalDecision, CommandExecutionRequestApprovalParams,
    FileChangeApprovalDecision, FileChangeRequestApprovalParams, ToolRequestUserInputAnswer,
    ToolRequestUserInputParams,
};
use serde::Serialize;
use serde_json::{json, Map, Value};
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
    pub high_risk: bool,
    #[serde(default)]
    pub risk_reasons: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub confirmation_phrase: Option<String>,
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

#[derive(Debug, Clone, Serialize)]
pub enum ResolveApprovalResult {
    Resolved,
    NotFound,
    Rejected { reason: String },
}

#[derive(Debug, Clone)]
pub enum UserInputResolution {
    Submit(HashMap<String, ToolRequestUserInputAnswer>),
    Skip,
    Cancel,
}

#[derive(Debug, Clone)]
struct ApprovalResolution {
    decision: ApprovalDecision,
    evidence: Option<Value>,
}

pub struct ApprovalBroker {
    paths: ClawdPaths,
    approvals: Mutex<HashMap<String, (PendingApproval, mpsc::Sender<ApprovalResolution>)>>,
    inputs: Mutex<HashMap<String, (PendingUserInput, mpsc::Sender<UserInputResolution>)>>,
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

    pub fn resolve_approval(
        &self,
        id: &str,
        decision: ApprovalDecision,
        evidence: Option<Value>,
    ) -> ResolveApprovalResult {
        let (pending, sender) = match self.approvals.lock() {
            Ok(mut map) => {
                let Some((pending, sender)) = map.get(id).cloned() else {
                    return ResolveApprovalResult::NotFound;
                };
                if let ApprovalDecision::Accept = decision {
                    if pending.high_risk {
                        let provided = confirmation_text(evidence.as_ref());
                        let required = pending.confirmation_phrase.as_deref().unwrap_or_default();
                        if provided.as_deref() != Some(required) {
                            return ResolveApprovalResult::Rejected {
                                reason: format!(
                                    "high-risk approval requires explicit confirmation phrase: {required}"
                                ),
                            };
                        }
                    }
                }
                map.remove(id);
                (pending, sender)
            }
            Err(_) => return ResolveApprovalResult::NotFound,
        };

        let _ = sender.send(ApprovalResolution {
            decision,
            evidence: normalize_evidence(evidence, &pending),
        });
        ResolveApprovalResult::Resolved
    }

    pub fn resolve_user_input(
        &self,
        id: &str,
        resolution: UserInputResolution,
    ) -> bool {
        let sender = match self.inputs.lock() {
            Ok(mut map) => map.remove(id).map(|(_, sender)| sender),
            Err(_) => None,
        };
        if let Some(sender) = sender {
            let _ = sender.send(resolution);
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
            params: request.clone(),
            created_at_ms: now_ms(),
        };
        if let Ok(mut map) = self.inputs.lock() {
            map.insert(id.clone(), (pending, tx));
        }

        let resolution = rx
            .recv_timeout(Duration::from_secs(APPROVAL_TIMEOUT_SECS))
            .unwrap_or(UserInputResolution::Cancel);
        if let Ok(mut map) = self.inputs.lock() {
            map.remove(&id);
        }

        let (decision, event_payload, answers, evidence) = match &resolution {
            UserInputResolution::Submit(answers) => (
                "submit",
                json!({
                    "threadId": params.thread_id,
                    "turnId": params.turn_id,
                    "itemId": params.item_id,
                    "action": "submit",
                    "answers": answers,
                }),
                answers.clone(),
                json!({
                    "action": "submit",
                    "answers": answers,
                }),
            ),
            UserInputResolution::Skip => (
                "skip",
                json!({
                    "threadId": params.thread_id,
                    "turnId": params.turn_id,
                    "itemId": params.item_id,
                    "action": "skip",
                    "answers": {},
                }),
                HashMap::new(),
                json!({ "action": "skip" }),
            ),
            UserInputResolution::Cancel => (
                "cancel",
                json!({
                    "threadId": params.thread_id,
                    "turnId": params.turn_id,
                    "itemId": params.item_id,
                    "action": "cancel",
                    "answers": {},
                }),
                HashMap::new(),
                json!({ "action": "cancel" }),
            ),
        };
        if let Ok(store) = TaskStore::open(&self.paths) {
            let _ = store.record_event(run_id, "tool_user_input", &event_payload);
            let _ = store.record_approval(
                run_id,
                "tool_user_input",
                &request,
                Some(decision),
                Some(&evidence),
            );
        }

        answers
    }

    fn request_approval(&self, run_id: &str, kind: &str, request: Value) -> ApprovalDecision {
        let request_for_record = request.clone();
        let (tx, rx) = mpsc::channel();
        let id = Uuid::new_v4().to_string();
        let (high_risk, risk_reasons, confirmation_phrase) = approval_risk(kind, &request);
        let pending = PendingApproval {
            id: id.clone(),
            run_id: run_id.to_string(),
            kind: kind.to_string(),
            request,
            created_at_ms: now_ms(),
            high_risk,
            risk_reasons,
            confirmation_phrase,
        };
        if let Ok(mut map) = self.approvals.lock() {
            map.insert(id.clone(), (pending, tx));
        }

        let resolution = rx
            .recv_timeout(Duration::from_secs(APPROVAL_TIMEOUT_SECS))
            .unwrap_or(ApprovalResolution {
                decision: ApprovalDecision::Decline,
                evidence: Some(json!({ "reason": "timeout" })),
            });
        if let Ok(mut map) = self.approvals.lock() {
            map.remove(&id);
        }

        let decision_str = match resolution.decision {
            ApprovalDecision::Accept => "accept",
            ApprovalDecision::Cancel => "cancel",
            ApprovalDecision::Decline => "decline",
        };
        if let Ok(store) = TaskStore::open(&self.paths) {
            let _ = store.record_approval(
                run_id,
                kind,
                &request_for_record,
                Some(decision_str),
                resolution.evidence.as_ref(),
            );
        }

        resolution.decision
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

fn approval_risk(kind: &str, request: &Value) -> (bool, Vec<String>, Option<String>) {
    if kind != "file_change" {
        return (false, Vec::new(), None);
    }
    let mut risks = Vec::new();
    if reason_suggests_delete_or_rename(request) {
        risks.push("reason mentions delete/rename".to_string());
    }
    if payload_contains_delete_or_rename(request) {
        risks.push("payload indicates delete/rename".to_string());
    }
    if patch_contains_delete_or_rename(request) {
        risks.push("diff indicates delete/rename".to_string());
    }
    if risks.is_empty() {
        return (false, risks, None);
    }
    (true, risks, Some("ALLOW_DELETE_OR_RENAME".to_string()))
}

fn reason_suggests_delete_or_rename(request: &Value) -> bool {
    let Some(reason) = request.get("reason").and_then(|v| v.as_str()) else {
        return false;
    };
    let lower = reason.to_lowercase();
    lower.contains("delete")
        || lower.contains("remove")
        || lower.contains("rename")
        || lower.contains("move")
}

fn payload_contains_delete_or_rename(request: &Value) -> bool {
    let Some(obj) = request.as_object() else {
        return false;
    };
    for key in ["fileChanges", "file_changes", "changes"] {
        let Some(value) = obj.get(key) else {
            continue;
        };
        if match_delete_or_rename_value(value) {
            return true;
        }
    }
    false
}

fn match_delete_or_rename_value(value: &Value) -> bool {
    match value {
        Value::String(raw) => {
            let lower = raw.to_lowercase();
            lower.contains("delete")
                || lower.contains("removed")
                || lower.contains("rename")
                || lower.contains("moved")
        }
        Value::Array(items) => items.iter().any(match_delete_or_rename_value),
        Value::Object(map) => map.values().any(match_delete_or_rename_value),
        _ => false,
    }
}

fn patch_contains_delete_or_rename(request: &Value) -> bool {
    let raw = request
        .get("diff")
        .or_else(|| request.get("patch"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_lowercase();
    raw.contains("deleted file mode")
        || raw.contains("rename from ")
        || raw.contains("rename to ")
        || raw.contains("\n--- /dev/null")
}

fn confirmation_text(evidence: Option<&Value>) -> Option<String> {
    let evidence = evidence?.as_object()?;
    evidence
        .get("confirmation")
        .or_else(|| evidence.get("confirmationText"))
        .and_then(|v| v.as_str())
        .map(|v| v.trim().to_string())
}

fn normalize_evidence(evidence: Option<Value>, pending: &PendingApproval) -> Option<Value> {
    let mut map = Map::new();
    if let Some(Value::Object(src)) = evidence {
        map.extend(src);
    }
    map.insert("highRisk".to_string(), Value::Bool(pending.high_risk));
    if !pending.risk_reasons.is_empty() {
        map.insert("riskReasons".to_string(), json!(pending.risk_reasons));
    }
    if let Some(phrase) = pending.confirmation_phrase.as_ref() {
        map.insert("requiredConfirmation".to_string(), Value::String(phrase.clone()));
    }
    if map.is_empty() {
        None
    } else {
        Some(Value::Object(map))
    }
}
