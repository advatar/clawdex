use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use base64::engine::general_purpose::STANDARD as BASE64_STANDARD;
use base64::Engine;
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use tiny_http::{Method, Response, Server, StatusCode};
use tungstenite::{accept, Message};
use uuid::Uuid;

use crate::config::{ClawdPaths, GatewayConfig};
use crate::task_db::TaskStore;
use crate::util::{append_json_line, now_ms, read_json_lines, read_json_value, write_json_value};

const GATEWAY_DIR: &str = "gateway";
const OUTBOX_FILE: &str = "outbox.jsonl";
const INBOX_FILE: &str = "inbox.jsonl";
const RECEIPTS_FILE: &str = "receipts.jsonl";
const ATTACHMENTS_DIR: &str = "attachments";
const ATTACHMENTS_INDEX_FILE: &str = "attachments.jsonl";
const ROUTES_FILE: &str = "routes.json";
const IDEMPOTENCY_FILE: &str = "idempotency.json";
const INBOX_OFFSET_FILE: &str = "inbox_offset.json";
const AUTH_TOKENS_FILE: &str = "auth_tokens.json";
const DEVICE_AUTH_FILE: &str = "device_auth.json";
const DEFAULT_ATTACHMENTS_MAX_BYTES: usize = 5_000_000;
const DEVICE_CODE_TTL_MS: i64 = 10 * 60 * 1000;
const DEFAULT_CHANNEL_ORDER: &[&str] = &[
    "telegram",
    "whatsapp",
    "discord",
    "googlechat",
    "slack",
    "signal",
    "imessage",
];
const CHANNEL_ALIASES: &[(&str, &str)] = &[
    ("imsg", "imessage"),
    ("google-chat", "googlechat"),
    ("gchat", "googlechat"),
];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum GatewayAuthMode {
    None,
    Token,
    Password,
}

#[derive(Debug, Clone)]
struct GatewayAuth {
    mode: GatewayAuthMode,
    token: Option<String>,
    password: Option<String>,
}

#[derive(Debug, Clone, Default)]
struct GatewayAuthAttempt {
    token: Option<String>,
    password: Option<String>,
}

#[derive(Debug, Clone)]
struct GatewayAuthFailure {
    message: String,
}

impl GatewayAuth {
    fn none() -> Self {
        Self {
            mode: GatewayAuthMode::None,
            token: None,
            password: None,
        }
    }

    fn required(&self) -> bool {
        self.mode != GatewayAuthMode::None
    }

    #[cfg(test)]
    fn token(value: &str) -> Self {
        Self {
            mode: GatewayAuthMode::Token,
            token: Some(value.to_string()),
            password: None,
        }
    }

    #[cfg(test)]
    fn password(value: &str) -> Self {
        Self {
            mode: GatewayAuthMode::Password,
            token: None,
            password: Some(value.to_string()),
        }
    }
}

fn trimmed_env(name: &str) -> Option<String> {
    std::env::var(name)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn resolve_gateway_auth(cfg: &GatewayConfig) -> GatewayAuth {
    let token = trimmed_env("CLAWDEX_GATEWAY_TOKEN")
        .or_else(|| trimmed_env("OPENCLAW_GATEWAY_TOKEN"))
        .or_else(|| trimmed_env("CLAWDBOT_GATEWAY_TOKEN"))
        .or_else(|| {
            cfg.token
                .as_ref()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        });
    let password = trimmed_env("CLAWDEX_GATEWAY_PASSWORD")
        .or_else(|| trimmed_env("OPENCLAW_GATEWAY_PASSWORD"))
        .or_else(|| trimmed_env("CLAWDBOT_GATEWAY_PASSWORD"))
        .or_else(|| {
            cfg.password
                .as_ref()
                .map(|value| value.trim().to_string())
                .filter(|value| !value.is_empty())
        });
    let mode = if password.is_some() {
        GatewayAuthMode::Password
    } else if token.is_some() {
        GatewayAuthMode::Token
    } else {
        GatewayAuthMode::None
    };
    GatewayAuth {
        mode,
        token,
        password,
    }
}

fn authorize_gateway_auth(
    auth: &GatewayAuth,
    attempt: &GatewayAuthAttempt,
    mut token_store: Option<&mut TokenStore>,
) -> Result<(), GatewayAuthFailure> {
    let provided_token = attempt
        .token
        .as_deref()
        .or_else(|| attempt.password.as_deref())
        .map(|value| value.trim())
        .filter(|value| !value.is_empty());
    let provided_password = attempt
        .password
        .as_deref()
        .or_else(|| attempt.token.as_deref())
        .map(|value| value.trim())
        .filter(|value| !value.is_empty());

    let check_token_store = |token: &str, store: &mut TokenStore| -> Result<bool, GatewayAuthFailure> {
        if store.is_valid(token) {
            let _ = store.mark_used(token);
            return Ok(true);
        }
        Ok(false)
    };

    match auth.mode {
        GatewayAuthMode::None => Ok(()),
        GatewayAuthMode::Token => {
            let Some(provided) = provided_token else {
                return Err(GatewayAuthFailure {
                    message: "missing gateway token".to_string(),
                });
            };
            if let Some(expected) = auth.token.as_deref() {
                if provided == expected {
                    return Ok(());
                }
            }
            if let Some(store) = token_store.as_deref_mut() {
                if check_token_store(provided, store)? {
                    return Ok(());
                }
            }
            if auth.token.is_none() {
                return Err(GatewayAuthFailure {
                    message: "gateway token not configured".to_string(),
                });
            }
            Err(GatewayAuthFailure {
                message: "invalid gateway token".to_string(),
            })
        }
        GatewayAuthMode::Password => {
            let Some(provided) = provided_password else {
                return Err(GatewayAuthFailure {
                    message: "missing gateway password".to_string(),
                });
            };
            if let Some(expected) = auth.password.as_deref() {
                if provided == expected {
                    return Ok(());
                }
            }
            if let Some(store) = token_store.as_deref_mut() {
                if check_token_store(provided, store)? {
                    return Ok(());
                }
            }
            Err(GatewayAuthFailure {
                message: "invalid gateway password".to_string(),
            })
        }
    }
}

#[derive(Debug, Clone)]
struct PresenceEntry {
    host: Option<String>,
    ip: Option<String>,
    version: Option<String>,
    platform: Option<String>,
    device_family: Option<String>,
    model_identifier: Option<String>,
    mode: Option<String>,
    last_input_ms: Option<i64>,
    reason: Option<String>,
    tags: Option<Vec<String>>,
    text: Option<String>,
    ts_ms: i64,
    device_id: Option<String>,
    roles: Option<Vec<String>>,
    scopes: Option<Vec<String>>,
    instance_id: Option<String>,
}

impl PresenceEntry {
    fn to_value(&self, now_ms: i64) -> Value {
        let mut map = Map::new();
        if let Some(value) = self.host.as_ref() {
            map.insert("host".to_string(), Value::String(value.clone()));
        }
        if let Some(value) = self.ip.as_ref() {
            map.insert("ip".to_string(), Value::String(value.clone()));
        }
        if let Some(value) = self.version.as_ref() {
            map.insert("version".to_string(), Value::String(value.clone()));
        }
        if let Some(value) = self.platform.as_ref() {
            map.insert("platform".to_string(), Value::String(value.clone()));
        }
        if let Some(value) = self.device_family.as_ref() {
            map.insert("deviceFamily".to_string(), Value::String(value.clone()));
        }
        if let Some(value) = self.model_identifier.as_ref() {
            map.insert("modelIdentifier".to_string(), Value::String(value.clone()));
        }
        if let Some(value) = self.mode.as_ref() {
            map.insert("mode".to_string(), Value::String(value.clone()));
        }
        if let Some(last_input_ms) = self.last_input_ms {
            let delta = now_ms.saturating_sub(last_input_ms);
            map.insert("lastInputSeconds".to_string(), Value::Number((delta / 1000).into()));
        }
        if let Some(value) = self.reason.as_ref() {
            map.insert("reason".to_string(), Value::String(value.clone()));
        }
        if let Some(value) = self.tags.as_ref() {
            map.insert(
                "tags".to_string(),
                Value::Array(value.iter().map(|v| Value::String(v.clone())).collect()),
            );
        }
        if let Some(value) = self.text.as_ref() {
            map.insert("text".to_string(), Value::String(value.clone()));
        }
        map.insert("ts".to_string(), Value::Number(self.ts_ms.into()));
        if let Some(value) = self.device_id.as_ref() {
            map.insert("deviceId".to_string(), Value::String(value.clone()));
        }
        if let Some(value) = self.roles.as_ref() {
            map.insert(
                "roles".to_string(),
                Value::Array(value.iter().map(|v| Value::String(v.clone())).collect()),
            );
        }
        if let Some(value) = self.scopes.as_ref() {
            map.insert(
                "scopes".to_string(),
                Value::Array(value.iter().map(|v| Value::String(v.clone())).collect()),
            );
        }
        if let Some(value) = self.instance_id.as_ref() {
            map.insert("instanceId".to_string(), Value::String(value.clone()));
        }
        Value::Object(map)
    }
}

struct PresenceState {
    started_at: Instant,
    presence_version: u64,
    health_version: u64,
    entries: HashMap<String, PresenceEntry>,
    self_key: String,
}

impl PresenceState {
    fn new() -> Self {
        let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".to_string());
        let platform = match std::env::consts::OS {
            "macos" => "macos",
            "windows" => "windows",
            "linux" => "linux",
            other => other,
        }
        .to_string();
        let device_family = match std::env::consts::OS {
            "macos" => Some("Mac".to_string()),
            "windows" => Some("Windows".to_string()),
            "linux" => Some("Linux".to_string()),
            _ => None,
        };
        let version = env!("CARGO_PKG_VERSION").to_string();
        let text = format!("Gateway: {host} · app {version} · mode gateway · reason self");
        let now = now_ms();
        let self_entry = PresenceEntry {
            host: Some(host.clone()),
            ip: None,
            version: Some(version.clone()),
            platform: Some(platform),
            device_family,
            model_identifier: Some(std::env::consts::ARCH.to_string()),
            mode: Some("gateway".to_string()),
            last_input_ms: None,
            reason: Some("self".to_string()),
            tags: None,
            text: Some(text),
            ts_ms: now,
            device_id: None,
            roles: None,
            scopes: None,
            instance_id: None,
        };
        let mut entries = HashMap::new();
        let key = host.to_lowercase();
        entries.insert(key.clone(), self_entry);
        Self {
            started_at: Instant::now(),
            presence_version: 1,
            health_version: 0,
            entries,
            self_key: key,
        }
    }

    fn snapshot(&mut self) -> (Vec<Value>, u64, u64, i64) {
        let now = now_ms();
        if let Some(entry) = self.entries.get_mut(&self.self_key) {
            entry.ts_ms = now;
        }
        self.prune(now);
        let list = self
            .entries
            .values()
            .map(|entry| entry.to_value(now))
            .collect::<Vec<_>>();
        let uptime_ms = self.started_at.elapsed().as_millis() as i64;
        (list, self.presence_version, self.health_version, uptime_ms)
    }

    fn prune(&mut self, now: i64) {
        const TTL_MS: i64 = 5 * 60 * 1000;
        self.entries.retain(|key, entry| {
            if *key == self.self_key {
                return true;
            }
            now.saturating_sub(entry.ts_ms) <= TTL_MS
        });
    }

    fn upsert(&mut self, key: String, entry: PresenceEntry) {
        self.entries.insert(key, entry);
        self.presence_version = self.presence_version.saturating_add(1);
    }

    fn touch(&mut self, key: &str) {
        if let Some(entry) = self.entries.get_mut(key) {
            let now = now_ms();
            entry.last_input_ms = Some(now);
            entry.ts_ms = now;
            self.presence_version = self.presence_version.saturating_add(1);
        }
    }

    fn mark_disconnect(&mut self, key: &str) {
        if let Some(entry) = self.entries.get_mut(key) {
            let now = now_ms();
            entry.reason = Some("disconnect".to_string());
            entry.ts_ms = now;
            self.presence_version = self.presence_version.saturating_add(1);
        }
    }
}

fn presence_state() -> &'static Mutex<PresenceState> {
    static STATE: OnceLock<Mutex<PresenceState>> = OnceLock::new();
    STATE.get_or_init(|| Mutex::new(PresenceState::new()))
}

fn with_presence_state<F, T>(f: F) -> T
where
    F: FnOnce(&mut PresenceState) -> T,
{
    let mut guard = presence_state()
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    f(&mut guard)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendMode {
    Direct,
    Queue,
}

#[derive(Debug)]
enum GatewayMethodError {
    InvalidRequest(String),
    NotImplemented(String),
    Internal(String),
}

impl GatewayMethodError {
    fn code(&self) -> &'static str {
        match self {
            Self::InvalidRequest(_) => "invalid_request",
            Self::NotImplemented(_) => "not_implemented",
            Self::Internal(_) => "internal_error",
        }
    }

    fn message(&self) -> &str {
        match self {
            Self::InvalidRequest(message)
            | Self::NotImplemented(message)
            | Self::Internal(message) => message,
        }
    }
}

type GatewayMethodResult = std::result::Result<Value, GatewayMethodError>;
type GatewayMethodHandler = Box<dyn Fn(&ClawdPaths, &Value) -> GatewayMethodResult + Send + Sync>;

struct GatewayMethodDefinition {
    version: u32,
    handler: GatewayMethodHandler,
}

#[derive(Default)]
struct GatewayMethodRegistry {
    methods: HashMap<String, GatewayMethodDefinition>,
}

impl GatewayMethodRegistry {
    fn register(&mut self, name: &str, version: u32, handler: GatewayMethodHandler) {
        let key = name.trim().to_string();
        if key.is_empty() {
            return;
        }
        self.methods.insert(
            key,
            GatewayMethodDefinition {
                version,
                handler,
            },
        );
    }

    fn handle(&self, name: &str, paths: &ClawdPaths, params: &Value) -> GatewayMethodResult {
        let Some(def) = self.methods.get(name) else {
            return Err(GatewayMethodError::NotImplemented(format!(
                "unsupported method: {name}"
            )));
        };
        (def.handler)(paths, params)
    }

    fn list_versions(&self) -> Vec<Value> {
        let mut entries = self
            .methods
            .iter()
            .map(|(name, def)| json!({ "name": name, "version": def.version }))
            .collect::<Vec<_>>();
        entries.sort_by(|a, b| {
            let a_name = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let b_name = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
            a_name.cmp(b_name)
        });
        entries
    }
}

#[derive(Debug, Default, Clone, Copy)]
struct ReceiptQuery {
    after: Option<i64>,
    limit: Option<usize>,
}

#[derive(Debug, Default, Clone, Copy)]
struct AttachmentQuery {
    after: Option<i64>,
    limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RouteEntry {
    pub channel: String,
    pub to: String,
    pub account_id: Option<String>,
    pub updated_at_ms: i64,
}

#[derive(Debug, Default)]
struct RouteStore {
    routes: HashMap<String, RouteEntry>,
    path: PathBuf,
}

impl RouteStore {
    fn load(paths: &ClawdPaths) -> Result<Self> {
        let path = gateway_dir(paths).join(ROUTES_FILE);
        let mut store = RouteStore {
            routes: HashMap::new(),
            path,
        };
        if let Some(value) = read_json_value(&store.path)? {
            if let Some(map) = value.get("routes") {
                store.routes = serde_json::from_value(map.clone()).unwrap_or_default();
            }
        }
        Ok(store)
    }

    fn save(&self) -> Result<()> {
        let value = json!({ "routes": self.routes });
        write_json_value(&self.path, &value)
    }

    fn update_route(&mut self, session_key: &str, entry: RouteEntry) -> Result<()> {
        self.routes.insert(session_key.to_string(), entry);
        self.save()
    }

    fn get_route(&self, session_key: &str) -> Option<RouteEntry> {
        self.routes.get(session_key).cloned()
    }

    fn entries(&self) -> Vec<(String, RouteEntry)> {
        self.routes
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect()
    }
}

#[derive(Debug, Default)]
struct IdempotencyStore {
    keys: HashMap<String, i64>,
    path: PathBuf,
}

impl IdempotencyStore {
    fn load(paths: &ClawdPaths) -> Result<Self> {
        let path = gateway_dir(paths).join(IDEMPOTENCY_FILE);
        let mut store = IdempotencyStore {
            keys: HashMap::new(),
            path,
        };
        if let Some(value) = read_json_value(&store.path)? {
            if let Some(map) = value.get("keys") {
                store.keys = serde_json::from_value(map.clone()).unwrap_or_default();
            }
        }
        Ok(store)
    }

    fn save(&self) -> Result<()> {
        let value = json!({ "keys": self.keys });
        write_json_value(&self.path, &value)
    }

    fn seen(&self, key: &str) -> bool {
        self.keys.contains_key(key)
    }

    fn insert(&mut self, key: &str, ts: i64) -> Result<()> {
        self.keys.insert(key.to_string(), ts);
        self.save()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct TokenEntry {
    created_at_ms: i64,
    last_used_at_ms: Option<i64>,
    revoked_at_ms: Option<i64>,
    label: Option<String>,
}

#[derive(Debug, Default)]
struct TokenStore {
    tokens: HashMap<String, TokenEntry>,
    path: PathBuf,
}

impl TokenStore {
    fn load(paths: &ClawdPaths) -> Result<Self> {
        let path = gateway_dir(paths).join(AUTH_TOKENS_FILE);
        let mut store = TokenStore {
            tokens: HashMap::new(),
            path,
        };
        if let Some(value) = read_json_value(&store.path)? {
            if let Some(map) = value.get("tokens") {
                store.tokens = serde_json::from_value(map.clone()).unwrap_or_default();
            }
        }
        Ok(store)
    }

    fn save(&self) -> Result<()> {
        let value = json!({ "tokens": self.tokens });
        write_json_value(&self.path, &value)
    }

    fn is_valid(&self, token: &str) -> bool {
        self.tokens
            .get(token)
            .map(|entry| entry.revoked_at_ms.is_none())
            .unwrap_or(false)
    }

    fn mark_used(&mut self, token: &str) -> Result<()> {
        if let Some(entry) = self.tokens.get_mut(token) {
            entry.last_used_at_ms = Some(now_ms());
            return self.save();
        }
        Ok(())
    }

    fn insert(&mut self, token: String, label: Option<String>) -> Result<TokenEntry> {
        let entry = TokenEntry {
            created_at_ms: now_ms(),
            last_used_at_ms: None,
            revoked_at_ms: None,
            label,
        };
        self.tokens.insert(token, entry.clone());
        self.save()?;
        Ok(entry)
    }

    fn revoke(&mut self, token: &str) -> Result<Option<TokenEntry>> {
        if let Some(entry) = self.tokens.get_mut(token) {
            entry.revoked_at_ms = Some(now_ms());
            let updated = entry.clone();
            self.save()?;
            return Ok(Some(updated));
        }
        Ok(None)
    }

    fn list(&self) -> Vec<Value> {
        let mut entries = self
            .tokens
            .iter()
            .map(|(token, entry)| {
                json!({
                    "token": token,
                    "createdAtMs": entry.created_at_ms,
                    "lastUsedAtMs": entry.last_used_at_ms,
                    "revokedAtMs": entry.revoked_at_ms,
                    "label": entry.label,
                })
            })
            .collect::<Vec<_>>();
        entries.sort_by(|a, b| {
            let a_ts = a.get("createdAtMs").and_then(|v| v.as_i64()).unwrap_or(0);
            let b_ts = b.get("createdAtMs").and_then(|v| v.as_i64()).unwrap_or(0);
            b_ts.cmp(&a_ts)
        });
        entries
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeviceAuthRequest {
    device_code: String,
    user_code: String,
    created_at_ms: i64,
    expires_at_ms: i64,
    approved_at_ms: Option<i64>,
    token: Option<String>,
    label: Option<String>,
}

#[derive(Debug, Default)]
struct DeviceAuthStore {
    requests: HashMap<String, DeviceAuthRequest>,
    path: PathBuf,
}

impl DeviceAuthStore {
    fn load(paths: &ClawdPaths) -> Result<Self> {
        let path = gateway_dir(paths).join(DEVICE_AUTH_FILE);
        let mut store = DeviceAuthStore {
            requests: HashMap::new(),
            path,
        };
        if let Some(value) = read_json_value(&store.path)? {
            if let Some(map) = value.get("requests") {
                store.requests = serde_json::from_value(map.clone()).unwrap_or_default();
            }
        }
        Ok(store)
    }

    fn save(&self) -> Result<()> {
        let value = json!({ "requests": self.requests });
        write_json_value(&self.path, &value)
    }

    fn insert_request(&mut self, request: DeviceAuthRequest) -> Result<()> {
        self.requests.insert(request.device_code.clone(), request);
        self.save()
    }

    fn find_by_user_code(&self, user_code: &str) -> Option<DeviceAuthRequest> {
        self.requests
            .values()
            .find(|entry| entry.user_code.eq_ignore_ascii_case(user_code))
            .cloned()
    }

    fn update(&mut self, request: DeviceAuthRequest) -> Result<()> {
        self.requests.insert(request.device_code.clone(), request);
        self.save()
    }

    fn remove(&mut self, device_code: &str) -> Result<()> {
        self.requests.remove(device_code);
        self.save()
    }
}

fn gateway_dir(paths: &ClawdPaths) -> PathBuf {
    paths.state_dir.join(GATEWAY_DIR)
}

fn outbox_path(paths: &ClawdPaths) -> PathBuf {
    gateway_dir(paths).join(OUTBOX_FILE)
}

fn inbox_path(paths: &ClawdPaths) -> PathBuf {
    gateway_dir(paths).join(INBOX_FILE)
}

fn receipts_path(paths: &ClawdPaths) -> PathBuf {
    gateway_dir(paths).join(RECEIPTS_FILE)
}

fn attachments_dir(paths: &ClawdPaths) -> PathBuf {
    gateway_dir(paths).join(ATTACHMENTS_DIR)
}

fn attachments_index_path(paths: &ClawdPaths) -> PathBuf {
    gateway_dir(paths).join(ATTACHMENTS_INDEX_FILE)
}

fn inbox_offset_path(paths: &ClawdPaths) -> PathBuf {
    gateway_dir(paths).join(INBOX_OFFSET_FILE)
}

fn load_inbox_offset(paths: &ClawdPaths) -> Result<usize> {
    if let Some(value) = read_json_value(&inbox_offset_path(paths))? {
        if let Some(offset) = value.get("offset").and_then(|v| v.as_u64()) {
            return Ok(offset as usize);
        }
    }
    Ok(0)
}

fn save_inbox_offset(paths: &ClawdPaths, offset: usize) -> Result<()> {
    write_json_value(&inbox_offset_path(paths), &json!({ "offset": offset }))
}

fn normalize_channel_id(raw: &str) -> String {
    let trimmed = raw.trim().to_lowercase();
    if trimmed.is_empty() {
        return trimmed;
    }
    for (alias, canonical) in CHANNEL_ALIASES {
        if trimmed == *alias {
            return (*canonical).to_string();
        }
    }
    trimmed
}

fn resolve_channel_order(cfg: &GatewayConfig) -> Vec<String> {
    let mut order = Vec::new();
    if let Some(entries) = cfg.channel_order.as_ref() {
        for entry in entries {
            let normalized = normalize_channel_id(entry);
            if !normalized.is_empty() && !order.contains(&normalized) {
                order.push(normalized);
            }
        }
    }
    if order.is_empty() {
        order = DEFAULT_CHANNEL_ORDER.iter().map(|entry| (*entry).to_string()).collect();
    }
    order
}

fn channel_order_index(order: &[String], channel: &str) -> usize {
    let normalized = normalize_channel_id(channel);
    order
        .iter()
        .position(|entry| entry == &normalized)
        .unwrap_or(usize::MAX - 1)
}

fn resolve_attachment_max_bytes(cfg: &GatewayConfig) -> usize {
    cfg.attachments_max_bytes
        .and_then(|value| usize::try_from(value).ok())
        .filter(|value| *value > 0)
        .unwrap_or(DEFAULT_ATTACHMENTS_MAX_BYTES)
}

fn generate_gateway_token() -> String {
    format!("gw_{}", Uuid::new_v4())
}

fn generate_device_code() -> String {
    Uuid::new_v4().to_string()
}

fn generate_user_code() -> String {
    let raw = Uuid::new_v4().to_string().replace('-', "");
    raw.chars().take(8).collect::<String>().to_uppercase()
}

fn load_manifest_value(path: &Path) -> Option<Value> {
    if !path.exists() {
        return None;
    }
    let raw = std::fs::read_to_string(path).ok()?;
    serde_json::from_str::<Value>(&raw).ok()
}

fn collect_gateway_methods_from_value(value: &Value) -> Vec<String> {
    let mut out = Vec::new();
    let methods = value
        .get("gatewayMethods")
        .or_else(|| value.get("gateway_methods"))
        .and_then(|v| v.as_array());
    if let Some(methods) = methods {
        for entry in methods {
            if let Some(name) = entry.as_str() {
                let trimmed = name.trim();
                if !trimmed.is_empty() {
                    out.push(trimmed.to_string());
                }
            }
        }
    }
    out
}

fn plugin_gateway_methods_for_root(root: &Path) -> Vec<String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    let candidates = [
        root.join("openclaw.plugin.json"),
        root.join(".claude-plugin").join("plugin.json"),
        root.join("plugin.json"),
    ];
    for path in candidates {
        let Some(value) = load_manifest_value(&path) else {
            continue;
        };
        for method in collect_gateway_methods_from_value(&value) {
            if seen.insert(method.clone()) {
                out.push(method);
            }
        }
    }
    out
}

fn load_plugin_gateway_methods(paths: &ClawdPaths) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    let Ok(store) = TaskStore::open(paths) else {
        return out;
    };
    let plugins = store.list_plugins(false).unwrap_or_default();
    for plugin in plugins {
        let root = Path::new(&plugin.path);
        for method in plugin_gateway_methods_for_root(root) {
            if seen.insert(method.clone()) {
                out.push(method);
            }
        }
    }
    out
}

fn build_gateway_registry(paths: &ClawdPaths) -> GatewayMethodRegistry {
    let mut registry = GatewayMethodRegistry::default();
    registry.register(
        "send",
        1,
        Box::new(|paths, params| {
            send_message_with_mode(paths, params, SendMode::Queue)
                .map_err(|err| GatewayMethodError::InvalidRequest(err.to_string()))
        }),
    );
    registry.register(
        "health",
        1,
        Box::new(|_paths, _params| Ok(json!({ "ok": true }))),
    );

    for method in load_plugin_gateway_methods(paths) {
        let name = method.clone();
        registry.register(
            &method,
            1,
            Box::new(move |_paths, _params| {
                Err(GatewayMethodError::NotImplemented(format!(
                    "method not implemented: {name}"
                )))
            }),
        );
    }

    registry
}

type GatewayRegistryHandle = Arc<RwLock<GatewayMethodRegistry>>;

fn gateway_registry_handle(paths: &ClawdPaths) -> GatewayRegistryHandle {
    static REGISTRIES: OnceLock<RwLock<HashMap<PathBuf, GatewayRegistryHandle>>> = OnceLock::new();
    let registries = REGISTRIES.get_or_init(|| RwLock::new(HashMap::new()));

    {
        let guard = registries.read().unwrap_or_else(|err| err.into_inner());
        if let Some(handle) = guard.get(&paths.state_dir) {
            return handle.clone();
        }
    }

    let mut guard = registries
        .write()
        .unwrap_or_else(|err| err.into_inner());
    guard
        .entry(paths.state_dir.clone())
        .or_insert_with(|| Arc::new(RwLock::new(build_gateway_registry(paths))))
        .clone()
}

fn reload_gateway_registry(paths: &ClawdPaths) {
    let handle = gateway_registry_handle(paths);
    let mut guard = handle
        .write()
        .unwrap_or_else(|err| err.into_inner());
    *guard = build_gateway_registry(paths);
}

fn list_gateway_method_versions(paths: &ClawdPaths) -> Vec<Value> {
    let handle = gateway_registry_handle(paths);
    let guard = handle
        .read()
        .unwrap_or_else(|err| err.into_inner());
    let mut entries = guard.list_versions();
    let mut names: HashSet<String> = entries
        .iter()
        .filter_map(|entry| entry.get("name").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .collect();
    for extra in ["methods.list", "gateway.reload"] {
        if names.insert(extra.to_string()) {
            entries.push(json!({ "name": extra, "version": 1 }));
        }
    }
    entries.sort_by(|a, b| {
        let a_name = a.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let b_name = b.get("name").and_then(|v| v.as_str()).unwrap_or("");
        a_name.cmp(b_name)
    });
    entries
}

fn list_gateway_methods(paths: &ClawdPaths) -> Vec<String> {
    let mut entries = list_gateway_method_versions(paths)
        .into_iter()
        .filter_map(|entry| entry.get("name").and_then(|v| v.as_str()).map(|s| s.to_string()))
        .collect::<Vec<_>>();
    entries.sort();
    entries
}

fn parse_attachment_query(query: Option<&str>) -> AttachmentQuery {
    let mut parsed = AttachmentQuery::default();
    let Some(query) = query else {
        return parsed;
    };
    for pair in query.split('&') {
        if pair.trim().is_empty() {
            continue;
        }
        let mut parts = pair.splitn(2, '=');
        let key = parts.next().unwrap_or("").trim();
        let value = parts.next().unwrap_or("").trim();
        if value.is_empty() {
            continue;
        }
        match key {
            "after" => {
                parsed.after = value.parse::<i64>().ok();
            }
            "limit" => {
                parsed.limit = value.parse::<usize>().ok();
            }
            _ => {}
        }
    }
    parsed
}

fn list_attachments(paths: &ClawdPaths, query: AttachmentQuery) -> Result<Vec<Value>> {
    let mut entries = read_json_lines(&attachments_index_path(paths), None)?;
    if let Some(after) = query.after {
        entries.retain(|entry| entry.get("createdAtMs").and_then(|v| v.as_i64()).unwrap_or(0) > after);
    }
    if let Some(limit) = query.limit {
        if entries.len() > limit {
            if query.after.is_some() {
                entries.truncate(limit);
            } else {
                entries = entries.split_off(entries.len() - limit);
            }
        }
    }
    Ok(entries)
}

fn find_attachment(paths: &ClawdPaths, attachment_id: &str) -> Result<Option<Value>> {
    let entries = read_json_lines(&attachments_index_path(paths), None)?;
    for entry in entries.into_iter().rev() {
        if entry.get("id").and_then(|v| v.as_str()) == Some(attachment_id) {
            return Ok(Some(entry));
        }
    }
    Ok(None)
}

fn decode_attachment_content(content: &str) -> Result<(Vec<u8>, Option<String>)> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Err(anyhow::anyhow!("attachment content empty"));
    }
    if let Some(rest) = trimmed.strip_prefix("data:") {
        let Some((meta, data)) = rest.split_once(',') else {
            return Err(anyhow::anyhow!("attachment content invalid data url"));
        };
        if !meta.to_lowercase().contains(";base64") {
            return Err(anyhow::anyhow!("attachment content must be base64 data url"));
        }
        let mime = meta
            .split(';')
            .next()
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty());
        let bytes = BASE64_STANDARD
            .decode(data.as_bytes())
            .context("attachment content invalid base64")?;
        return Ok((bytes, mime));
    }
    let bytes = BASE64_STANDARD
        .decode(trimmed.as_bytes())
        .context("attachment content invalid base64")?;
    Ok((bytes, None))
}

fn store_attachment(paths: &ClawdPaths, cfg: &GatewayConfig, attachment: &Value) -> Result<Value> {
    let content = attachment
        .get("content")
        .and_then(|v| v.as_str())
        .context("attachment content missing")?;
    let file_name = attachment
        .get("fileName")
        .or_else(|| attachment.get("filename"))
        .or_else(|| attachment.get("name"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let mut mime_type = attachment
        .get("mimeType")
        .or_else(|| attachment.get("mime_type"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let (bytes, data_mime) = decode_attachment_content(content)?;
    if mime_type.is_none() {
        mime_type = data_mime;
    }
    if mime_type.is_none() {
        mime_type = Some("application/octet-stream".to_string());
    }

    let max_bytes = resolve_attachment_max_bytes(cfg);
    if bytes.is_empty() {
        return Err(anyhow::anyhow!("attachment content empty"));
    }
    if bytes.len() > max_bytes {
        return Err(anyhow::anyhow!(
            "attachment exceeds max size ({} > {})",
            bytes.len(),
            max_bytes
        ));
    }

    let id = Uuid::new_v4().to_string();
    let file_name_on_disk = id.clone();
    let dir = attachments_dir(paths);
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create attachments dir {}", dir.display()))?;
    let path = dir.join(&file_name_on_disk);
    std::fs::write(&path, &bytes)
        .with_context(|| format!("write attachment {}", path.display()))?;

    let mut hasher = Sha256::new();
    hasher.update(&bytes);
    let sha256 = hex::encode(hasher.finalize());
    let created_at_ms = now_ms();

    let meta = json!({
        "id": id,
        "fileName": file_name,
        "mimeType": mime_type,
        "sizeBytes": bytes.len(),
        "sha256": sha256,
        "createdAtMs": created_at_ms,
        "path": format!("{ATTACHMENTS_DIR}/{file_name_on_disk}"),
    });

    append_json_line(&attachments_index_path(paths), &meta)?;
    Ok(meta)
}

fn process_attachments(
    paths: &ClawdPaths,
    cfg: &GatewayConfig,
    attachments: Option<&Value>,
) -> Result<Option<Vec<Value>>> {
    let Some(attachments) = attachments else {
        return Ok(None);
    };
    let list = attachments
        .as_array()
        .context("attachments must be an array")?;
    let mut out = Vec::with_capacity(list.len());
    for entry in list {
        if entry.get("content").is_some() {
            out.push(store_attachment(paths, cfg, entry)?);
            continue;
        }
        if let Some(id) = entry.get("id").and_then(|v| v.as_str()) {
            if let Some(found) = find_attachment(paths, id)? {
                out.push(found);
                continue;
            }
            return Err(anyhow::anyhow!("attachment id not found: {}", id));
        }
        out.push(entry.clone());
    }
    Ok(Some(out))
}

fn start_device_auth(paths: &ClawdPaths) -> Result<Value> {
    let mut store = DeviceAuthStore::load(paths)?;
    let now = now_ms();
    let request = DeviceAuthRequest {
        device_code: generate_device_code(),
        user_code: generate_user_code(),
        created_at_ms: now,
        expires_at_ms: now + DEVICE_CODE_TTL_MS,
        approved_at_ms: None,
        token: None,
        label: None,
    };
    store.insert_request(request.clone())?;
    Ok(json!({
        "ok": true,
        "deviceCode": request.device_code,
        "userCode": request.user_code,
        "expiresAtMs": request.expires_at_ms,
        "intervalMs": 2000
    }))
}

fn poll_device_auth(paths: &ClawdPaths, payload: &Value) -> Result<Value> {
    let device_code = payload
        .get("deviceCode")
        .or_else(|| payload.get("device_code"))
        .and_then(|v| v.as_str())
        .context("deviceCode required")?;
    let mut store = DeviceAuthStore::load(paths)?;
    let Some(request) = store.requests.get(device_code).cloned() else {
        return Ok(json!({ "ok": false, "error": "device code not found" }));
    };
    let now = now_ms();
    if now > request.expires_at_ms {
        let _ = store.remove(&request.device_code);
        return Ok(json!({ "ok": false, "error": "device code expired" }));
    }
    if let Some(token) = request.token.as_ref() {
        return Ok(json!({
            "ok": true,
            "status": "approved",
            "token": token,
            "approvedAtMs": request.approved_at_ms,
            "expiresAtMs": request.expires_at_ms,
        }));
    }
    Ok(json!({
        "ok": true,
        "status": "pending",
        "expiresAtMs": request.expires_at_ms,
    }))
}

fn approve_device_auth(
    paths: &ClawdPaths,
    token_store: &mut TokenStore,
    payload: &Value,
) -> Result<Value> {
    let device_code = payload.get("deviceCode").and_then(|v| v.as_str());
    let user_code = payload.get("userCode").and_then(|v| v.as_str());
    let label = payload
        .get("label")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    let mut store = DeviceAuthStore::load(paths)?;
    let mut request = if let Some(code) = device_code {
        store.requests.get(code).cloned()
    } else if let Some(code) = user_code {
        store.find_by_user_code(code)
    } else {
        None
    }
    .context("deviceCode or userCode required")?;

    let now = now_ms();
    if now > request.expires_at_ms {
        let _ = store.remove(&request.device_code);
        return Ok(json!({ "ok": false, "error": "device code expired" }));
    }

    let token = generate_gateway_token();
    let token_label = label
        .clone()
        .or_else(|| Some(format!("device:{}", request.user_code)));
    token_store.insert(token.clone(), token_label)?;

    request.approved_at_ms = Some(now);
    request.token = Some(token.clone());
    request.label = label.clone();
    store.update(request.clone())?;

    Ok(json!({
        "ok": true,
        "deviceCode": request.device_code,
        "userCode": request.user_code,
        "token": token,
        "approvedAtMs": request.approved_at_ms,
        "expiresAtMs": request.expires_at_ms,
    }))
}

fn create_auth_token(token_store: &mut TokenStore, payload: &Value) -> Result<Value> {
    let label = payload
        .get("label")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let token = generate_gateway_token();
    let entry = token_store.insert(token.clone(), label)?;
    Ok(json!({
        "token": token,
        "createdAtMs": entry.created_at_ms,
        "label": entry.label,
    }))
}

fn rotate_auth_token(
    token_store: &mut TokenStore,
    current_token: Option<&str>,
    payload: &Value,
) -> Result<Value> {
    let label = payload
        .get("label")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let token = generate_gateway_token();
    let entry = token_store.insert(token.clone(), label)?;
    let mut revoked = false;
    if let Some(current) = current_token {
        if token_store.revoke(current)?.is_some() {
            revoked = true;
        }
    }
    Ok(json!({
        "token": token,
        "createdAtMs": entry.created_at_ms,
        "revoked": revoked,
    }))
}

fn append_receipt(paths: &ClawdPaths, receipt: &Value) -> Result<()> {
    append_json_line(&receipts_path(paths), receipt)
}

fn record_receipt(paths: &ClawdPaths, receipt: &Value) {
    if let Err(err) = append_receipt(paths, receipt) {
        eprintln!("[clawdex][gateway] failed to record receipt: {err}");
    }
}

fn list_receipts(paths: &ClawdPaths, query: ReceiptQuery) -> Result<Vec<Value>> {
    let mut entries = read_json_lines(&receipts_path(paths), None)?;
    if let Some(after) = query.after {
        entries.retain(|entry| entry.get("tsMs").and_then(|v| v.as_i64()).unwrap_or(0) > after);
    }
    if let Some(limit) = query.limit {
        if entries.len() > limit {
            if query.after.is_some() {
                entries.truncate(limit);
            } else {
                entries = entries.split_off(entries.len() - limit);
            }
        }
    }
    Ok(entries)
}

fn parse_receipt_query(query: Option<&str>) -> ReceiptQuery {
    let mut parsed = ReceiptQuery::default();
    let Some(query) = query else {
        return parsed;
    };
    for pair in query.split('&') {
        if pair.trim().is_empty() {
            continue;
        }
        let mut parts = pair.splitn(2, '=');
        let key = parts.next().unwrap_or("").trim();
        let value = parts.next().unwrap_or("").trim();
        if value.is_empty() {
            continue;
        }
        match key {
            "after" => {
                parsed.after = value.parse::<i64>().ok();
            }
            "limit" => {
                parsed.limit = value.parse::<usize>().ok();
            }
            _ => {}
        }
    }
    parsed
}

fn build_receipt(
    status: &str,
    direction: &str,
    message_id: Option<&str>,
    session_key: Option<&str>,
    channel: Option<&str>,
    to: Option<&str>,
    from: Option<&str>,
    account_id: Option<&str>,
    idempotency_key: Option<&str>,
    ts_ms: i64,
) -> Value {
    json!({
        "id": Uuid::new_v4().to_string(),
        "status": status,
        "direction": direction,
        "messageId": message_id,
        "sessionKey": session_key,
        "channel": channel,
        "to": to,
        "from": from,
        "accountId": account_id,
        "idempotencyKey": idempotency_key,
        "tsMs": ts_ms,
    })
}

fn load_gateway_config(paths: &ClawdPaths) -> Result<GatewayConfig> {
    let (cfg, _) = crate::config::load_config(
        Some(paths.state_dir.clone()),
        Some(paths.workspace_dir.clone()),
    )?;
    Ok(cfg.gateway.unwrap_or_default())
}

fn resolve_gateway_url(cfg: &GatewayConfig) -> Option<String> {
    if let Ok(env) = std::env::var("CLAWDEX_GATEWAY_URL") {
        let trimmed = env.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    if let Some(url) = cfg.url.as_ref().map(|s| s.trim().to_string()).filter(|s| !s.is_empty()) {
        return Some(url);
    }
    cfg.bind
        .as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(|bind| format!("http://{bind}"))
}

fn gateway_configured(cfg: &GatewayConfig) -> bool {
    resolve_gateway_url(cfg).is_some()
        || cfg
            .bind
            .as_ref()
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
}

fn extract_bearer_token(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    let lower = trimmed.to_ascii_lowercase();
    if !lower.starts_with("bearer ") {
        return None;
    }
    Some(trimmed[7..].trim().to_string()).filter(|token| !token.is_empty())
}

fn extract_http_auth(request: &tiny_http::Request) -> GatewayAuthAttempt {
    let mut attempt = GatewayAuthAttempt::default();
    for header in request.headers() {
        let name = header.field.as_str().to_ascii_lowercase();
        let value = header.value.as_str();
        if name == "authorization" {
            if let Some(token) = extract_bearer_token(value) {
                attempt.token = Some(token);
                attempt.password = attempt.token.clone();
            }
        } else if name == "x-openclaw-token"
            || name == "x-clawdex-token"
            || name == "x-clawdbot-token"
        {
            let trimmed = value.trim();
            if !trimmed.is_empty() {
                attempt.token = Some(trimmed.to_string());
                attempt.password = attempt.token.clone();
            }
        }
    }
    attempt
}

fn extract_ws_auth(params: &Value) -> GatewayAuthAttempt {
    let mut attempt = GatewayAuthAttempt::default();
    let auth = params.get("auth").and_then(|v| v.as_object());
    if let Some(auth) = auth {
        if let Some(token) = auth.get("token").and_then(|v| v.as_str()) {
            let trimmed = token.trim();
            if !trimmed.is_empty() {
                attempt.token = Some(trimmed.to_string());
            }
        }
        if let Some(password) = auth.get("password").and_then(|v| v.as_str()) {
            let trimmed = password.trim();
            if !trimmed.is_empty() {
                attempt.password = Some(trimmed.to_string());
            }
        }
    }
    attempt
}

fn presence_key_from_params(params: &Value, conn_id: &str) -> String {
    let device_id = params
        .get("device")
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty());
    let instance_id = params
        .get("client")
        .and_then(|v| v.get("instanceId"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_lowercase())
        .filter(|s| !s.is_empty());
    device_id
        .or(instance_id)
        .unwrap_or_else(|| conn_id.to_string())
}

fn presence_from_params(params: &Value, conn_id: &str) -> Option<(String, PresenceEntry)> {
    let client = params.get("client")?.as_object()?;
    let id = client
        .get("id")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let display_name = client
        .get("displayName")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let version = client
        .get("version")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let platform = client
        .get("platform")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let device_family = client
        .get("deviceFamily")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let model_identifier = client
        .get("modelIdentifier")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let mode = client
        .get("mode")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let instance_id = client
        .get("instanceId")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let device_id = params
        .get("device")
        .and_then(|v| v.get("id"))
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let role = params
        .get("role")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .map(|r| vec![r]);
    let scopes = params
        .get("scopes")
        .and_then(|v| v.as_array())
        .map(|values| {
            values
                .iter()
                .filter_map(|v| v.as_str())
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
        })
        .filter(|values| !values.is_empty());
    let now = now_ms();
    let host = display_name.or_else(|| id.clone());
    let text = match (&id, &mode) {
        (Some(id), Some(mode)) => Some(format!("client {id} · mode {mode}")),
        (Some(id), None) => Some(format!("client {id}")),
        _ => None,
    };
    let entry = PresenceEntry {
        host,
        ip: None,
        version,
        platform,
        device_family,
        model_identifier,
        mode,
        last_input_ms: Some(now),
        reason: Some("connect".to_string()),
        tags: None,
        text,
        ts_ms: now,
        device_id,
        roles: role,
        scopes,
        instance_id,
    };
    let key = presence_key_from_params(params, conn_id);
    Some((key, entry))
}

fn resolve_config_path(paths: &ClawdPaths) -> Option<String> {
    let json5 = paths.state_dir.join("config.json5");
    if json5.exists() {
        return Some(json5.display().to_string());
    }
    let json = paths.state_dir.join("config.json");
    if json.exists() {
        return Some(json.display().to_string());
    }
    None
}

fn gateway_snapshot(paths: &ClawdPaths) -> Value {
    let (presence, presence_version, health_version, uptime_ms) =
        with_presence_state(|state| state.snapshot());
    let mut snapshot = json!({
        "presence": presence,
        "health": {},
        "stateVersion": { "presence": presence_version, "health": health_version },
        "uptimeMs": uptime_ms,
        "stateDir": paths.state_dir.display().to_string(),
    });
    if let Some(config_path) = resolve_config_path(paths) {
        snapshot["configPath"] = Value::String(config_path);
    }
    snapshot
}

fn resolve_send_url(base: &str) -> String {
    let trimmed = base.trim_end_matches('/');
    if trimmed.ends_with("/v1/send") || trimmed.ends_with("/send") {
        return trimmed.to_string();
    }
    if trimmed.ends_with("/v1") {
        return format!("{trimmed}/send");
    }
    format!("{trimmed}/v1/send")
}

fn send_via_http(url: &str, payload: &Value) -> Result<Value> {
    let client = Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .context("build gateway http client")?;
    let resp = client.post(url).json(payload).send();
    let resp = match resp {
        Ok(resp) => resp,
        Err(err) => return Err(anyhow::anyhow!("gateway send failed: {err}")),
    };
    let status = resp.status();
    let json_value = resp.json::<Value>().unwrap_or_else(|_| {
        json!({
            "ok": status.is_success(),
            "status": status.as_u16(),
        })
    });
    Ok(json_value)
}

fn route_cutoff_ms(cfg: &GatewayConfig) -> Option<i64> {
    cfg.route_ttl_ms
        .map(|ttl| now_ms().saturating_sub(ttl as i64))
}

fn route_is_fresh(route: &RouteEntry, cutoff: Option<i64>) -> bool {
    cutoff.map(|cutoff| route.updated_at_ms >= cutoff).unwrap_or(true)
}

fn route_matches(route: &RouteEntry, channel: Option<&str>, to: Option<&str>) -> bool {
    if let Some(channel) = channel {
        if route.channel != channel {
            return false;
        }
    }
    if let Some(to) = to {
        if route.to != to {
            return false;
        }
    }
    true
}

pub fn send_message(paths: &ClawdPaths, args: &Value) -> Result<Value> {
    send_message_with_mode(paths, args, SendMode::Direct)
}

fn send_message_with_mode(paths: &ClawdPaths, args: &Value, mode: SendMode) -> Result<Value> {
    let text = args
        .get("text")
        .or_else(|| args.get("message"))
        .and_then(|v| v.as_str())
        .context("message.send requires text or message")?;

    let best_effort = args
        .get("bestEffort")
        .or_else(|| args.get("best_effort"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let dry_run = args
        .get("dryRun")
        .or_else(|| args.get("dry_run"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let mut channel = args
        .get("channel")
        .and_then(|v| v.as_str())
        .map(normalize_channel_id)
        .filter(|s| !s.is_empty());
    if channel.as_deref() == Some("last") {
        channel = None;
    }
    let to = args
        .get("to")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let mut account_id = args
        .get("accountId")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let session_key = args
        .get("sessionKey")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.trim().is_empty());

    let idempotency_key = args
        .get("idempotency_key")
        .or_else(|| args.get("idempotencyKey"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| format!("auto-{}", Uuid::new_v4()));

    if dry_run {
        return Ok(json!({ "ok": true, "dryRun": true }));
    }

    let mut route_store = RouteStore::load(paths)?;
    let cfg = load_gateway_config(paths)?;
    let cutoff = route_cutoff_ms(&cfg);
    let attachments = process_attachments(paths, &cfg, args.get("attachments"))?;

    let mut resolved_session_key = session_key.clone();
    let mut route = None;

    if let (Some(channel), Some(to)) = (channel.clone(), to.clone()) {
        route = Some(RouteEntry {
            channel: channel.clone(),
            to: to.clone(),
            account_id: account_id.clone(),
            updated_at_ms: now_ms(),
        });
        if resolved_session_key.is_none() {
            resolved_session_key = Some(format!("{channel}:{to}"));
        }
    } else if let Some(ref session_key) = resolved_session_key {
        if let Some(found) = route_store.get_route(session_key) {
            if route_is_fresh(&found, cutoff)
                && route_matches(&found, channel.as_deref(), to.as_deref())
            {
                if account_id.is_none() {
                    account_id = found.account_id.clone();
                }
                route = Some(found);
            }
        }

        if route.is_none() {
            if best_effort {
                return Ok(json!({
                    "ok": false,
                    "bestEffort": true,
                    "error": "no route available"
                }));
            }
            return Err(anyhow::anyhow!(
                "message.send missing channel/to and no last route"
            ));
        }
    } else {
        let resolved = resolve_target(
            paths,
            &json!({
                "channel": channel,
                "to": to,
                "accountId": account_id
            }),
        )?;
        if resolved.get("ok").and_then(|v| v.as_bool()) != Some(true) {
            if best_effort {
                return Ok(json!({
                    "ok": false,
                    "bestEffort": true,
                    "error": "no route available"
                }));
            }
            return Err(anyhow::anyhow!(
                "message.send missing channel/to and no last route"
            ));
        }
        let channel = resolved
            .get("channel")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .context("message.send missing channel/to and no last route")?;
        let to = resolved
            .get("to")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .context("message.send missing channel/to and no last route")?;
        if account_id.is_none() {
            account_id = resolved
                .get("accountId")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string());
        }
        if resolved_session_key.is_none() {
            resolved_session_key = resolved
                .get("sessionKey")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .or_else(|| Some(format!("{channel}:{to}")));
        }
        route = Some(RouteEntry {
            channel,
            to,
            account_id: account_id.clone(),
            updated_at_ms: now_ms(),
        });
    }

    let session_key = resolved_session_key.unwrap_or_else(|| "agent:main:main".to_string());
    let route = route.expect("route resolution");

    let mut idempotency = IdempotencyStore::load(paths)?;
    if idempotency.seen(&idempotency_key) {
        return Ok(json!({ "ok": true, "deduped": true }));
    }

    let entry_account_id = account_id.clone().or(route.account_id.clone());
    let created_at_ms = now_ms();
    let mut entry = json!({
        "id": Uuid::new_v4().to_string(),
        "sessionKey": session_key,
        "channel": route.channel,
        "to": route.to,
        "accountId": entry_account_id,
        "text": text,
        "message": text,
        "idempotencyKey": idempotency_key,
        "createdAtMs": created_at_ms,
    });
    if let Some(attachments) = attachments {
        entry["attachments"] = Value::Array(attachments);
    }
    let message_id = entry
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    if mode == SendMode::Queue {
        append_json_line(&outbox_path(paths), &entry)?;
        route_store.update_route(
            &session_key,
            RouteEntry {
                account_id: account_id.clone().or(route.account_id.clone()),
                updated_at_ms: now_ms(),
                ..route.clone()
            },
        )?;
        idempotency.insert(&idempotency_key, now_ms())?;
        let receipt = build_receipt(
            "queued",
            "outgoing",
            Some(message_id.as_str()),
            Some(&session_key),
            Some(&route.channel),
            Some(&route.to),
            None,
            entry_account_id.as_deref(),
            Some(&idempotency_key),
            created_at_ms,
        );
        record_receipt(paths, &receipt);
        return Ok(json!({ "ok": true, "queued": true, "message": entry, "result": entry }));
    }

    let gateway_url = resolve_gateway_url(&cfg);
    if let Some(base_url) = gateway_url {
        let send_url = resolve_send_url(&base_url);
        let response = send_via_http(&send_url, &entry);
        let response = match response {
            Ok(value) => value,
            Err(err) => {
                let err_msg = err.to_string();
                let mut receipt = build_receipt(
                    "failed",
                    "outgoing",
                    Some(message_id.as_str()),
                    Some(&session_key),
                    Some(&route.channel),
                    Some(&route.to),
                    None,
                    entry_account_id.as_deref(),
                    Some(&idempotency_key),
                    now_ms(),
                );
                receipt["error"] = Value::String(err_msg.clone());
                record_receipt(paths, &receipt);
                if best_effort {
                    return Ok(json!({ "ok": false, "bestEffort": true, "error": err_msg }));
                }
                return Err(err);
            }
        };
        let ok = response.get("ok").and_then(|v| v.as_bool()).unwrap_or(true);
        if !ok {
            let err = response
                .get("error")
                .and_then(|v| v.as_str())
                .unwrap_or("message send failed")
                .to_string();
            let mut receipt = build_receipt(
                "failed",
                "outgoing",
                Some(message_id.as_str()),
                Some(&session_key),
                Some(&route.channel),
                Some(&route.to),
                None,
                entry_account_id.as_deref(),
                Some(&idempotency_key),
                now_ms(),
            );
            receipt["error"] = Value::String(err.clone());
            record_receipt(paths, &receipt);
            if best_effort {
                return Ok(json!({ "ok": false, "bestEffort": true, "error": err }));
            }
            return Err(anyhow::anyhow!(err));
        }
        route_store.update_route(
            &session_key,
            RouteEntry {
                account_id: account_id.clone().or(route.account_id.clone()),
                updated_at_ms: now_ms(),
                ..route.clone()
            },
        )?;
        idempotency.insert(&idempotency_key, now_ms())?;
        let result = response.get("result").cloned().unwrap_or_else(|| response.clone());
        let mut receipt = build_receipt(
            "sent",
            "outgoing",
            Some(message_id.as_str()),
            Some(&session_key),
            Some(&route.channel),
            Some(&route.to),
            None,
            entry_account_id.as_deref(),
            Some(&idempotency_key),
            now_ms(),
        );
        receipt["result"] = result.clone();
        record_receipt(paths, &receipt);
        return Ok(json!({ "ok": true, "result": result }));
    }

    let mut receipt = build_receipt(
        "failed",
        "outgoing",
        Some(message_id.as_str()),
        Some(&session_key),
        Some(&route.channel),
        Some(&route.to),
        None,
        entry_account_id.as_deref(),
        Some(&idempotency_key),
        now_ms(),
    );
    receipt["error"] = Value::String("gateway disabled".to_string());
    record_receipt(paths, &receipt);
    if best_effort {
        return Ok(json!({
            "ok": false,
            "bestEffort": true,
            "error": "gateway disabled"
        }));
    }
    Err(anyhow::anyhow!("gateway disabled"))
}

pub fn list_channels(paths: &ClawdPaths) -> Result<Value> {
    let store = RouteStore::load(paths)?;
    let cfg = load_gateway_config(paths)?;
    if !gateway_configured(&cfg) {
        let mut response = json!({
            "channels": [],
            "count": 0,
            "disabled": true,
        });
        if let Some(ttl) = cfg.route_ttl_ms {
            response["routeTtlMs"] = Value::Number(ttl.into());
        }
        return Ok(response);
    }
    let cutoff = route_cutoff_ms(&cfg);
    let channel_order = resolve_channel_order(&cfg);

    let mut entries = store
        .entries()
        .into_iter()
        .filter(|(_, route)| route_is_fresh(route, cutoff))
        .map(|(session_key, route)| {
            json!({
                "channel": route.channel,
                "to": route.to,
                "accountId": route.account_id,
                "sessionKey": session_key,
                "updatedAtMs": route.updated_at_ms,
            })
        })
        .collect::<Vec<_>>();

    entries.sort_by(|a, b| {
        let a_channel = a.get("channel").and_then(|v| v.as_str()).unwrap_or("");
        let b_channel = b.get("channel").and_then(|v| v.as_str()).unwrap_or("");
        let order_a = channel_order_index(&channel_order, a_channel);
        let order_b = channel_order_index(&channel_order, b_channel);
        let order_cmp = order_a.cmp(&order_b);
        if order_cmp != Ordering::Equal {
            return order_cmp;
        }
        let a_ts = a.get("updatedAtMs").and_then(|v| v.as_i64()).unwrap_or(0);
        let b_ts = b.get("updatedAtMs").and_then(|v| v.as_i64()).unwrap_or(0);
        let ts_cmp = b_ts.cmp(&a_ts);
        if ts_cmp != Ordering::Equal {
            return ts_cmp;
        }
        a_channel.cmp(b_channel)
    });

    let mut response = json!({
        "channels": entries,
        "count": entries.len(),
        "disabled": false,
    });
    if let Some(ttl) = cfg.route_ttl_ms {
        response["routeTtlMs"] = Value::Number(ttl.into());
    }
    Ok(response)
}

pub fn resolve_target(paths: &ClawdPaths, args: &Value) -> Result<Value> {
    let mut channel = args
        .get("channel")
        .and_then(|v| v.as_str())
        .map(normalize_channel_id)
        .filter(|s| !s.is_empty());
    if channel.as_deref() == Some("last") {
        channel = None;
    }
    let to = args
        .get("to")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    let account_id = args
        .get("accountId")
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    if let (Some(channel), Some(to)) = (channel.clone(), to.clone()) {
        let session_key = format!("{channel}:{to}");
        return Ok(json!({
            "ok": true,
            "channel": channel,
            "to": to,
            "accountId": account_id,
            "sessionKey": session_key,
        }));
    }

    let store = RouteStore::load(paths)?;
    let cfg = load_gateway_config(paths)?;
    let cutoff = route_cutoff_ms(&cfg);
    let channel_order = resolve_channel_order(&cfg);
    let use_channel_order = channel.is_none();

    let mut routes = store
        .entries()
        .into_iter()
        .filter(|(_, route)| route_is_fresh(route, cutoff))
        .collect::<Vec<_>>();

    if let Some(ref channel) = channel {
        routes.retain(|(_, route)| route.channel == *channel);
    }
    if let Some(ref to) = to {
        routes.retain(|(_, route)| route.to == *to);
    }
    if let Some(ref account_id) = account_id {
        routes.retain(|(_, route)| route.account_id.as_deref() == Some(account_id.as_str()));
    }

    if use_channel_order {
        routes.sort_by(|a, b| {
            let order_a = channel_order_index(&channel_order, &a.1.channel);
            let order_b = channel_order_index(&channel_order, &b.1.channel);
            let order_cmp = order_a.cmp(&order_b);
            if order_cmp != Ordering::Equal {
                return order_cmp;
            }
            let ts_cmp = b.1.updated_at_ms.cmp(&a.1.updated_at_ms);
            if ts_cmp != Ordering::Equal {
                return ts_cmp;
            }
            a.1.channel.cmp(&b.1.channel)
        });
    } else {
        routes.sort_by(|a, b| b.1.updated_at_ms.cmp(&a.1.updated_at_ms));
    }

    if let Some((session_key, route)) = routes.first() {
        return Ok(json!({
            "ok": true,
            "channel": route.channel,
            "to": route.to,
            "accountId": route.account_id,
            "sessionKey": session_key,
            "updatedAtMs": route.updated_at_ms,
        }));
    }

    Ok(json!({
        "ok": false,
        "reason": "no matching route",
        "channel": channel,
        "to": to,
        "accountId": account_id,
    }))
}

pub fn record_incoming(paths: &ClawdPaths, payload: &Value) -> Result<Value> {
    let cfg = load_gateway_config(paths)?;
    let channel_raw = payload
        .get("channel")
        .and_then(|v| v.as_str())
        .context("incoming requires channel")?;
    let channel = normalize_channel_id(channel_raw);
    if channel.is_empty() {
        return Err(anyhow::anyhow!("incoming requires channel"));
    }
    let from = payload
        .get("from")
        .and_then(|v| v.as_str())
        .context("incoming requires from")?;
    let text = payload
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let session_key = format!("{channel}:{from}");
    let received_at_ms = now_ms();
    let attachments = process_attachments(paths, &cfg, payload.get("attachments"))?;
    let mut entry = json!({
        "id": Uuid::new_v4().to_string(),
        "sessionKey": session_key,
        "channel": channel,
        "from": from,
        "accountId": payload.get("accountId").and_then(|v| v.as_str()),
        "text": text,
        "receivedAtMs": received_at_ms,
    });
    if let Some(attachments) = attachments {
        entry["attachments"] = Value::Array(attachments);
    }

    append_json_line(&inbox_path(paths), &entry)?;
    let message_id = payload
        .get("messageId")
        .or_else(|| payload.get("id"))
        .and_then(|v| v.as_str())
        .or_else(|| entry.get("id").and_then(|v| v.as_str()));
    let receipt = build_receipt(
        "received",
        "incoming",
        message_id,
        entry.get("sessionKey").and_then(|v| v.as_str()),
        Some(channel.as_str()),
        None,
        Some(from),
        payload.get("accountId").and_then(|v| v.as_str()),
        None,
        received_at_ms,
    );
    record_receipt(paths, &receipt);

    let mut route_store = RouteStore::load(paths)?;
    route_store.update_route(
        &session_key,
        RouteEntry {
            channel: channel.to_string(),
            to: from.to_string(),
            account_id: payload.get("accountId").and_then(|v| v.as_str()).map(|s| s.to_string()),
            updated_at_ms: now_ms(),
        },
    )?;

    Ok(json!({ "ok": true, "message": entry }))
}

pub fn drain_inbox(paths: &ClawdPaths) -> Result<Vec<Value>> {
    let path = inbox_path(paths);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let entries = read_json_lines(&path, None)?;
    let offset = load_inbox_offset(paths)?;
    let total = entries.len();
    let new_entries = if offset < total {
        entries[offset..].to_vec()
    } else {
        Vec::new()
    };
    save_inbox_offset(paths, total)?;
    Ok(new_entries)
}

pub fn run_gateway(bind: &str, paths: &ClawdPaths) -> Result<()> {
    std::fs::create_dir_all(gateway_dir(paths))
        .with_context(|| format!("create gateway dir {}", gateway_dir(paths).display()))?;
    let server = Server::http(bind)
        .map_err(|err| anyhow::anyhow!("bind gateway {bind}: {err}"))?;

    for mut request in server.incoming_requests() {
        let response = match handle_request(paths, &mut request) {
            Ok(resp) => resp,
            Err(err) => json_error_response(&err.to_string(), StatusCode(500)),
        };
        let _ = request.respond(response);
    }
    Ok(())
}

pub fn run_gateway_ws(bind: &str, paths: &ClawdPaths) -> Result<()> {
    let listener =
        TcpListener::bind(bind).map_err(|err| anyhow::anyhow!("bind gateway ws {bind}: {err}"))?;
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(err) => {
                eprintln!("[clawdex][gateway-ws] accept failed: {err}");
                continue;
            }
        };
        let paths = paths.clone();
        std::thread::spawn(move || {
            let mut websocket = match accept(stream) {
                Ok(ws) => ws,
                Err(err) => {
                    eprintln!("[clawdex][gateway-ws] handshake failed: {err}");
                    return;
                }
            };
            let auth = match load_gateway_config(&paths) {
                Ok(cfg) => resolve_gateway_auth(&cfg),
                Err(err) => {
                    eprintln!("[clawdex][gateway-ws] load config failed: {err}");
                    GatewayAuth::none()
                }
            };
            let mut authorized = !auth.required();
            let mut presence_key: Option<String> = None;
            let conn_id = Uuid::new_v4().to_string();
            loop {
                let msg = match websocket.read() {
                    Ok(msg) => msg,
                    Err(_) => break,
                };
                if msg.is_close() {
                    break;
                }
                let text = match msg {
                    Message::Text(text) => text,
                    Message::Binary(bin) => String::from_utf8(bin).unwrap_or_default(),
                    _ => continue,
                };
                let frame: Value = match serde_json::from_str(&text) {
                    Ok(frame) => frame,
                    Err(_) => continue,
                };
                let response = handle_ws_frame(
                    &frame,
                    &paths,
                    &conn_id,
                    &auth,
                    &mut authorized,
                    &mut presence_key,
                );
                if let Some(response) = response {
                    let _ = websocket.send(Message::Text(response.to_string()));
                }
            }
            if let Some(key) = presence_key.as_deref() {
                with_presence_state(|state| state.mark_disconnect(key));
            }
        });
    }
    Ok(())
}

fn handle_ws_frame(
    frame: &Value,
    paths: &ClawdPaths,
    conn_id: &str,
    auth: &GatewayAuth,
    authorized: &mut bool,
    presence_key: &mut Option<String>,
) -> Option<Value> {
    if frame.get("type").and_then(|v| v.as_str()) != Some("req") {
        return None;
    }
    let id = frame.get("id")?.as_str()?.to_string();
    let method = frame.get("method").and_then(|v| v.as_str()).unwrap_or("");
    let params = frame.get("params").cloned().unwrap_or_else(|| json!({}));

    if let Some(key) = presence_key.as_deref() {
        if method != "connect" && method != "hello" {
            with_presence_state(|state| state.touch(key));
        }
    }

    if auth.required() && !*authorized && method != "connect" && method != "hello" {
        return Some(ws_response_err(
            &id,
            "unauthorized",
            "gateway auth required",
        ));
    }

    match method {
        "connect" | "hello" => {
            if auth.required() {
                let attempt = extract_ws_auth(&params);
                let mut token_store = TokenStore::load(paths).unwrap_or_default();
                if let Err(err) = authorize_gateway_auth(auth, &attempt, Some(&mut token_store)) {
                    return Some(ws_response_err(&id, "unauthorized", &err.message));
                }
                *authorized = true;
            }
            if let Some((key, entry)) = presence_from_params(&params, conn_id) {
                *presence_key = Some(key.clone());
                with_presence_state(|state| state.upsert(key, entry));
            }
            Some(ws_response_ok(&id, hello_ok_payload(paths, conn_id)))
        }
        "methods.list" => {
            let methods = list_gateway_method_versions(paths);
            Some(ws_response_ok(&id, json!({ "methods": methods })))
        }
        "gateway.reload" => {
            reload_gateway_registry(paths);
            let methods = list_gateway_method_versions(paths);
            Some(ws_response_ok(&id, json!({ "methods": methods, "reloaded": true })))
        }
        _ => {
            let handle = gateway_registry_handle(paths);
            let registry = handle
                .read()
                .unwrap_or_else(|err| err.into_inner());
            match registry.handle(method, paths, &params) {
                Ok(payload) => Some(ws_response_ok(&id, payload)),
                Err(err) => Some(ws_response_err(&id, err.code(), err.message())),
            }
        }
    }
}

fn hello_ok_payload(paths: &ClawdPaths, conn_id: &str) -> Value {
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".to_string());
    let snapshot = gateway_snapshot(paths);
    let methods = list_gateway_methods(paths);
    json!({
        "type": "hello-ok",
        "protocol": 1,
        "server": {
            "version": env!("CARGO_PKG_VERSION"),
            "host": host,
            "connId": conn_id,
        },
        "features": {
            "methods": methods,
            "events": [],
        },
        "snapshot": snapshot,
        "policy": {
            "maxPayload": 1048576,
            "maxBufferedBytes": 1048576,
            "tickIntervalMs": 30000,
        },
    })
}

fn ws_response_ok(id: &str, payload: Value) -> Value {
    json!({
        "type": "res",
        "id": id,
        "ok": true,
        "payload": payload,
    })
}

fn ws_response_err(id: &str, code: &str, message: &str) -> Value {
    json!({
        "type": "res",
        "id": id,
        "ok": false,
        "error": {
            "code": code,
            "message": message,
        }
    })
}

fn handle_request(
    paths: &ClawdPaths,
    request: &mut tiny_http::Request,
) -> Result<Response<std::io::Cursor<Vec<u8>>>> {
    let method = request.method().clone();
    let url = request.url().to_string();
    let (path, query) = match url.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (url.as_str(), None),
    };
    let requires_auth = !matches!(
        (&method, path),
        (&Method::Get, "/v1/health")
            | (&Method::Post, "/v1/auth/device/start")
            | (&Method::Post, "/v1/auth/device/poll")
    );
    if requires_auth {
        let cfg = load_gateway_config(paths)?;
        let auth = resolve_gateway_auth(&cfg);
        if auth.required() {
            let attempt = extract_http_auth(request);
            let mut token_store = TokenStore::load(paths).unwrap_or_default();
            if let Err(err) = authorize_gateway_auth(&auth, &attempt, Some(&mut token_store)) {
                return Ok(unauthorized_response(&err.message));
            }
        }
    }
    if let Some(rest) = path.strip_prefix("/v1/attachments/") {
        let rest = rest.trim_matches('/');
        if rest.is_empty() {
            return Ok(Response::from_data(Vec::new()).with_status_code(StatusCode(404)));
        }
        let (attachment_id, wants_data) = match rest.strip_suffix("/data") {
            Some(id) => (id.trim_matches('/'), true),
            None => (rest, false),
        };
        if attachment_id.is_empty() {
            return Ok(Response::from_data(Vec::new()).with_status_code(StatusCode(404)));
        }
        let meta = find_attachment(paths, attachment_id)?;
        let Some(meta) = meta else {
            return Ok(Response::from_data(Vec::new()).with_status_code(StatusCode(404)));
        };
        if wants_data {
            let stored_path = meta.get("path").and_then(|v| v.as_str()).unwrap_or("");
            let file_path = if stored_path.is_empty() {
                attachments_dir(paths).join(attachment_id)
            } else {
                gateway_dir(paths).join(stored_path)
            };
            let data = std::fs::read(&file_path)
                .with_context(|| format!("read attachment {}", file_path.display()))?;
            let mime = meta
                .get("mimeType")
                .and_then(|v| v.as_str())
                .unwrap_or("application/octet-stream");
            return Ok(bytes_response(data, mime)?);
        }
        return Ok(json_response(json!({ "ok": true, "attachment": meta }))?);
    }
    match (&method, path) {
        (&Method::Get, "/v1/health") => Ok(json_response(json!({ "ok": true }))?),
        (&Method::Post, "/v1/send") => {
            let body = read_body(request)?;
            let payload: Value = serde_json::from_slice(&body).context("invalid json")?;
            let result = send_message_with_mode(paths, &payload, SendMode::Queue)?;
            Ok(json_response(result)?)
        }
        (&Method::Post, "/v1/incoming") => {
            let body = read_body(request)?;
            let payload: Value = serde_json::from_slice(&body).context("invalid json")?;
            let result = record_incoming(paths, &payload)?;
            Ok(json_response(result)?)
        }
        (&Method::Post, "/v1/attachments") => {
            let body = read_body(request)?;
            let payload: Value = serde_json::from_slice(&body).context("invalid json")?;
            let cfg = load_gateway_config(paths)?;
            let mut attachments = Vec::new();
            if let Some(list) = payload.get("attachments").and_then(|v| v.as_array()) {
                for entry in list {
                    attachments.push(store_attachment(paths, &cfg, entry)?);
                }
            } else if let Some(entry) = payload.get("attachment") {
                attachments.push(store_attachment(paths, &cfg, entry)?);
            } else {
                attachments.push(store_attachment(paths, &cfg, &payload)?);
            }
            let count = attachments.len();
            Ok(json_response(json!({ "ok": true, "attachments": attachments, "count": count }))?)
        }
        (&Method::Get, "/v1/attachments") => {
            let query = parse_attachment_query(query);
            let attachments = list_attachments(paths, query)?;
            let count = attachments.len();
            let next_after = attachments
                .last()
                .and_then(|value| value.get("createdAtMs"))
                .and_then(|value| value.as_i64());
            let mut response = json!({ "ok": true, "attachments": attachments, "count": count });
            if let Some(next_after) = next_after {
                response["nextAfter"] = Value::Number(next_after.into());
            }
            Ok(json_response(response)?)
        }
        (&Method::Post, "/v1/auth/device/start") => Ok(json_response(start_device_auth(paths)?)?),
        (&Method::Post, "/v1/auth/device/poll") => {
            let body = read_body(request)?;
            let payload: Value = serde_json::from_slice(&body).context("invalid json")?;
            let result = poll_device_auth(paths, &payload)?;
            Ok(json_response(result)?)
        }
        (&Method::Post, "/v1/auth/device/approve") => {
            let body = read_body(request)?;
            let payload: Value = serde_json::from_slice(&body).context("invalid json")?;
            let mut token_store = TokenStore::load(paths)?;
            let result = approve_device_auth(paths, &mut token_store, &payload)?;
            Ok(json_response(result)?)
        }
        (&Method::Get, "/v1/auth/tokens") => {
            let token_store = TokenStore::load(paths)?;
            let tokens = token_store.list();
            Ok(json_response(json!({ "ok": true, "tokens": tokens, "count": token_store.tokens.len() }))?)
        }
        (&Method::Post, "/v1/auth/tokens") => {
            let body = read_body(request)?;
            let payload: Value = serde_json::from_slice(&body).context("invalid json")?;
            let mut token_store = TokenStore::load(paths)?;
            let result = create_auth_token(&mut token_store, &payload)?;
            Ok(json_response(json!({ "ok": true, "token": result }))?)
        }
        (&Method::Post, "/v1/auth/tokens/revoke") => {
            let body = read_body(request)?;
            let payload: Value = serde_json::from_slice(&body).context("invalid json")?;
            let token = payload
                .get("token")
                .and_then(|v| v.as_str())
                .context("token required")?;
            let mut token_store = TokenStore::load(paths)?;
            let revoked = token_store.revoke(token)?;
            Ok(json_response(json!({ "ok": revoked.is_some(), "revoked": revoked }))?)
        }
        (&Method::Post, "/v1/auth/rotate") => {
            let body = read_body(request)?;
            let payload: Value = serde_json::from_slice(&body).context("invalid json")?;
            let current = extract_http_auth(request).token;
            let mut token_store = TokenStore::load(paths)?;
            let result = rotate_auth_token(&mut token_store, current.as_deref(), &payload)?;
            Ok(json_response(json!({ "ok": true, "token": result }))?)
        }
        (&Method::Get, "/v1/receipts") => {
            let query = parse_receipt_query(query);
            let receipts = list_receipts(paths, query)?;
            let count = receipts.len();
            let next_after = receipts
                .last()
                .and_then(|value| value.get("tsMs"))
                .and_then(|value| value.as_i64());
            let mut response = json!({ "ok": true, "receipts": receipts, "count": count });
            if let Some(next_after) = next_after {
                response["nextAfter"] = Value::Number(next_after.into());
            }
            Ok(json_response(response)?)
        }
        _ => Ok(Response::from_data(Vec::new()).with_status_code(StatusCode(404))),
    }
}

fn read_body(request: &mut tiny_http::Request) -> Result<Vec<u8>> {
    let mut body = Vec::new();
    request
        .as_reader()
        .read_to_end(&mut body)
        .context("read body")?;
    Ok(body)
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

fn bytes_response(data: Vec<u8>, mime: &str) -> Result<Response<std::io::Cursor<Vec<u8>>>> {
    let header = tiny_http::Header::from_bytes(
        &b"Content-Type"[..],
        mime.as_bytes(),
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

fn unauthorized_response(message: &str) -> Response<std::io::Cursor<Vec<u8>>> {
    json_error_response(message, StatusCode(401))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gateway_registry_test_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn gateway_auth_none_allows() {
        let auth = GatewayAuth::none();
        let attempt = GatewayAuthAttempt::default();
        assert!(authorize_gateway_auth(&auth, &attempt, None).is_ok());
    }

    #[test]
    fn gateway_auth_token_requires_match() {
        let auth = GatewayAuth::token("secret");
        let missing = GatewayAuthAttempt::default();
        assert!(authorize_gateway_auth(&auth, &missing, None).is_err());

        let wrong = GatewayAuthAttempt {
            token: Some("wrong".to_string()),
            password: None,
        };
        assert!(authorize_gateway_auth(&auth, &wrong, None).is_err());

        let ok = GatewayAuthAttempt {
            token: Some("secret".to_string()),
            password: None,
        };
        assert!(authorize_gateway_auth(&auth, &ok, None).is_ok());
    }

    #[test]
    fn gateway_auth_password_requires_match() {
        let auth = GatewayAuth::password("secret");
        let missing = GatewayAuthAttempt::default();
        assert!(authorize_gateway_auth(&auth, &missing, None).is_err());

        let wrong = GatewayAuthAttempt {
            token: Some("wrong".to_string()),
            password: None,
        };
        assert!(authorize_gateway_auth(&auth, &wrong, None).is_err());

        let ok = GatewayAuthAttempt {
            token: None,
            password: Some("secret".to_string()),
        };
        assert!(authorize_gateway_auth(&auth, &ok, None).is_ok());
    }

    #[test]
    fn gateway_auth_allows_stored_token() -> Result<()> {
        let base = std::env::temp_dir().join(format!("clawdex-auth-store-{}", Uuid::new_v4()));
        let state_dir = base.join("state");
        let workspace_dir = base.join("workspace");
        std::fs::create_dir_all(&workspace_dir)?;
        let (_cfg, paths) = crate::config::load_config(Some(state_dir), Some(workspace_dir))?;

        let mut store = TokenStore::load(&paths)?;
        store.insert("stored-token".to_string(), Some("test".to_string()))?;

        let auth = GatewayAuth::token("secret");
        let attempt = GatewayAuthAttempt {
            token: Some("stored-token".to_string()),
            password: None,
        };
        assert!(authorize_gateway_auth(&auth, &attempt, Some(&mut store)).is_ok());

        let _ = std::fs::remove_dir_all(base);
        Ok(())
    }

    #[test]
    fn presence_entry_reports_last_input() {
        let entry = PresenceEntry {
            host: None,
            ip: None,
            version: None,
            platform: None,
            device_family: None,
            model_identifier: None,
            mode: None,
            last_input_ms: Some(1_000),
            reason: None,
            tags: None,
            text: None,
            ts_ms: 1_000,
            device_id: None,
            roles: None,
            scopes: None,
            instance_id: None,
        };
        let value = entry.to_value(2_500);
        let last_input = value.get("lastInputSeconds").and_then(|v| v.as_i64());
        assert_eq!(last_input, Some(1));
    }

    #[test]
    fn receipts_filter_and_limit() -> Result<()> {
        let base = std::env::temp_dir().join(format!("clawdex-receipts-{}", Uuid::new_v4()));
        let state_dir = base.join("state");
        let workspace_dir = base.join("workspace");
        std::fs::create_dir_all(&workspace_dir)?;
        let (_cfg, paths) = crate::config::load_config(Some(state_dir), Some(workspace_dir))?;

        let r1 = build_receipt(
            "queued",
            "outgoing",
            Some("m1"),
            Some("s1"),
            Some("channel"),
            Some("user1"),
            None,
            None,
            Some("k1"),
            1_000,
        );
        let r2 = build_receipt(
            "sent",
            "outgoing",
            Some("m2"),
            Some("s1"),
            Some("channel"),
            Some("user1"),
            None,
            None,
            Some("k2"),
            2_000,
        );
        let r3 = build_receipt(
            "received",
            "incoming",
            Some("m3"),
            Some("s2"),
            Some("channel"),
            None,
            Some("user2"),
            None,
            None,
            3_000,
        );

        append_receipt(&paths, &r1)?;
        append_receipt(&paths, &r2)?;
        append_receipt(&paths, &r3)?;

        let filtered = list_receipts(
            &paths,
            ReceiptQuery {
                after: Some(1_500),
                limit: None,
            },
        )?;
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].get("tsMs").and_then(|v| v.as_i64()), Some(2_000));

        let limited = list_receipts(
            &paths,
            ReceiptQuery {
                after: Some(1_500),
                limit: Some(1),
            },
        )?;
        assert_eq!(limited.len(), 1);
        assert_eq!(limited[0].get("tsMs").and_then(|v| v.as_i64()), Some(2_000));

        let tail = list_receipts(
            &paths,
            ReceiptQuery {
                after: None,
                limit: Some(2),
            },
        )?;
        assert_eq!(tail.len(), 2);
        assert_eq!(tail[0].get("tsMs").and_then(|v| v.as_i64()), Some(2_000));

        let _ = std::fs::remove_dir_all(base);
        Ok(())
    }

    #[test]
    fn resolve_target_prefers_channel_order() -> Result<()> {
        let base = std::env::temp_dir().join(format!("clawdex-channel-order-{}", Uuid::new_v4()));
        let state_dir = base.join("state");
        let workspace_dir = base.join("workspace");
        std::fs::create_dir_all(&workspace_dir)?;
        std::fs::create_dir_all(&state_dir)?;
        let config_path = state_dir.join("config.json");
        let config_payload = json!({
            "gateway": { "channelOrder": ["whatsapp", "telegram"] }
        });
        std::fs::write(&config_path, serde_json::to_vec_pretty(&config_payload)?)?;

        let (_cfg, paths) = crate::config::load_config(Some(state_dir), Some(workspace_dir))?;

        let mut store = RouteStore::load(&paths)?;
        store.update_route(
            "slack:user",
            RouteEntry {
                channel: "slack".to_string(),
                to: "user".to_string(),
                account_id: None,
                updated_at_ms: 3_000,
            },
        )?;
        store.update_route(
            "telegram:user",
            RouteEntry {
                channel: "telegram".to_string(),
                to: "user".to_string(),
                account_id: None,
                updated_at_ms: 2_000,
            },
        )?;
        store.update_route(
            "whatsapp:user",
            RouteEntry {
                channel: "whatsapp".to_string(),
                to: "user".to_string(),
                account_id: None,
                updated_at_ms: 1_000,
            },
        )?;

        let resolved = resolve_target(&paths, &json!({}))?;
        assert_eq!(resolved.get("channel").and_then(|v| v.as_str()), Some("whatsapp"));

        let resolved = resolve_target(&paths, &json!({ "channel": "slack" }))?;
        assert_eq!(resolved.get("channel").and_then(|v| v.as_str()), Some("slack"));

        let _ = std::fs::remove_dir_all(base);
        Ok(())
    }

    #[test]
    fn attachments_store_and_list() -> Result<()> {
        let base = std::env::temp_dir().join(format!("clawdex-attachments-{}", Uuid::new_v4()));
        let state_dir = base.join("state");
        let workspace_dir = base.join("workspace");
        std::fs::create_dir_all(&workspace_dir)?;

        let (_cfg, paths) = crate::config::load_config(Some(state_dir), Some(workspace_dir))?;
        let cfg = load_gateway_config(&paths)?;

        let attachment = json!({
            "fileName": "hello.txt",
            "mimeType": "text/plain",
            "content": "aGVsbG8=",
        });
        let meta = store_attachment(&paths, &cfg, &attachment)?;
        let stored_path = meta
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        assert!(!stored_path.is_empty());
        let full_path = gateway_dir(&paths).join(stored_path);
        assert!(full_path.exists());

        let list = list_attachments(&paths, AttachmentQuery::default())?;
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].get("id"), meta.get("id"));

        let _ = std::fs::remove_dir_all(base);
        Ok(())
    }

    #[test]
    fn device_auth_flow_issues_token() -> Result<()> {
        let base = std::env::temp_dir().join(format!("clawdex-device-auth-{}", Uuid::new_v4()));
        let state_dir = base.join("state");
        let workspace_dir = base.join("workspace");
        std::fs::create_dir_all(&workspace_dir)?;

        let (_cfg, paths) = crate::config::load_config(Some(state_dir), Some(workspace_dir))?;

        let start = start_device_auth(&paths)?;
        let device_code = start
            .get("deviceCode")
            .and_then(|v| v.as_str())
            .context("deviceCode missing")?
            .to_string();

        let mut token_store = TokenStore::load(&paths)?;
        let approve = approve_device_auth(
            &paths,
            &mut token_store,
            &json!({ "deviceCode": device_code }),
        )?;
        let token = approve
            .get("token")
            .and_then(|v| v.as_str())
            .context("token missing")?
            .to_string();
        assert!(token_store.is_valid(&token));

        let poll = poll_device_auth(&paths, &json!({ "deviceCode": device_code }))?;
        assert_eq!(poll.get("status").and_then(|v| v.as_str()), Some("approved"));
        assert_eq!(poll.get("token").and_then(|v| v.as_str()), Some(token.as_str()));

        let _ = std::fs::remove_dir_all(base);
        Ok(())
    }

    #[test]
    fn gateway_methods_registry_discovers_plugin_methods() -> Result<()> {
        let base = std::env::temp_dir().join(format!("clawdex-methods-{}", Uuid::new_v4()));
        let state_dir = base.join("state");
        let workspace_dir = base.join("workspace");
        let plugin_dir = base.join("plugin-a");
        std::fs::create_dir_all(&workspace_dir)?;
        std::fs::create_dir_all(&plugin_dir)?;

        let manifest = json!({
            "id": "plugin-a",
            "gatewayMethods": ["plugin.foo", "plugin.bar"],
            "configSchema": {}
        });
        std::fs::write(
            plugin_dir.join("openclaw.plugin.json"),
            serde_json::to_vec_pretty(&manifest)?,
        )?;

        let (_cfg, paths) = crate::config::load_config(Some(state_dir), Some(workspace_dir))?;
        let store = TaskStore::open(&paths)?;
        let plugin = crate::task_db::PluginRecord {
            id: "plugin-a".to_string(),
            name: "Plugin A".to_string(),
            version: None,
            description: None,
            source: None,
            path: plugin_dir.to_string_lossy().to_string(),
            enabled: true,
            installed_at_ms: now_ms(),
            updated_at_ms: now_ms(),
        };
        store.upsert_plugin(&plugin)?;

        let registry = build_gateway_registry(&paths);
        assert!(registry.methods.contains_key("plugin.foo"));
        assert!(registry.methods.contains_key("plugin.bar"));

        let _ = std::fs::remove_dir_all(base);
        Ok(())
    }

    #[test]
    fn gateway_methods_reload_refreshes_manifest() -> Result<()> {
        let _guard = gateway_registry_test_lock()
            .lock()
            .unwrap_or_else(|err| err.into_inner());
        let base = std::env::temp_dir().join(format!("clawdex-methods-reload-{}", Uuid::new_v4()));
        let state_dir = base.join("state");
        let workspace_dir = base.join("workspace");
        let plugin_dir = base.join("plugin-b");
        std::fs::create_dir_all(&workspace_dir)?;
        std::fs::create_dir_all(&plugin_dir)?;

        let (_cfg, paths) = crate::config::load_config(Some(state_dir), Some(workspace_dir))?;
        let store = TaskStore::open(&paths)?;
        let plugin = crate::task_db::PluginRecord {
            id: "plugin-b".to_string(),
            name: "Plugin B".to_string(),
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
            "id": "plugin-b",
            "gatewayMethods": ["plugin.b.one"],
            "configSchema": {}
        });
        std::fs::write(
            plugin_dir.join("openclaw.plugin.json"),
            serde_json::to_vec_pretty(&manifest_a)?,
        )?;
        reload_gateway_registry(&paths);
        let methods = list_gateway_methods(&paths);
        assert!(methods.contains(&"plugin.b.one".to_string()));

        let manifest_b = json!({
            "id": "plugin-b",
            "gatewayMethods": ["plugin.b.two"],
            "configSchema": {}
        });
        std::fs::write(
            plugin_dir.join("openclaw.plugin.json"),
            serde_json::to_vec_pretty(&manifest_b)?,
        )?;
        reload_gateway_registry(&paths);
        let methods = list_gateway_methods(&paths);
        assert!(methods.contains(&"plugin.b.two".to_string()));

        let _ = std::fs::remove_dir_all(base);
        Ok(())
    }
}
