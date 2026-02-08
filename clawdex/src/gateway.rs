use std::collections::HashMap;
use std::net::TcpListener;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use reqwest::blocking::Client;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tiny_http::{Method, Response, Server, StatusCode};
use tungstenite::{accept, Message};
use uuid::Uuid;

use crate::config::{ClawdPaths, GatewayConfig};
use crate::util::{append_json_line, now_ms, read_json_lines, read_json_value, write_json_value};

const GATEWAY_DIR: &str = "gateway";
const OUTBOX_FILE: &str = "outbox.jsonl";
const INBOX_FILE: &str = "inbox.jsonl";
const ROUTES_FILE: &str = "routes.json";
const IDEMPOTENCY_FILE: &str = "idempotency.json";
const INBOX_OFFSET_FILE: &str = "inbox_offset.json";

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

fn authorize_gateway_auth(auth: &GatewayAuth, attempt: &GatewayAuthAttempt) -> Result<(), GatewayAuthFailure> {
    match auth.mode {
        GatewayAuthMode::None => Ok(()),
        GatewayAuthMode::Token => {
            let provided = attempt
                .token
                .as_deref()
                .or_else(|| attempt.password.as_deref())
                .map(|value| value.trim())
                .filter(|value| !value.is_empty());
            let expected = auth.token.as_deref().unwrap_or("");
            if provided.is_none() {
                return Err(GatewayAuthFailure {
                    message: "missing gateway token".to_string(),
                });
            }
            if provided != Some(expected) {
                return Err(GatewayAuthFailure {
                    message: "invalid gateway token".to_string(),
                });
            }
            Ok(())
        }
        GatewayAuthMode::Password => {
            let provided = attempt
                .password
                .as_deref()
                .or_else(|| attempt.token.as_deref())
                .map(|value| value.trim())
                .filter(|value| !value.is_empty());
            let expected = auth.password.as_deref().unwrap_or("");
            if provided.is_none() {
                return Err(GatewayAuthFailure {
                    message: "missing gateway password".to_string(),
                });
            }
            if provided != Some(expected) {
                return Err(GatewayAuthFailure {
                    message: "invalid gateway password".to_string(),
                });
            }
            Ok(())
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SendMode {
    Direct,
    Queue,
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

fn gateway_dir(paths: &ClawdPaths) -> PathBuf {
    paths.state_dir.join(GATEWAY_DIR)
}

fn outbox_path(paths: &ClawdPaths) -> PathBuf {
    gateway_dir(paths).join(OUTBOX_FILE)
}

fn inbox_path(paths: &ClawdPaths) -> PathBuf {
    gateway_dir(paths).join(INBOX_FILE)
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
        .map(|s| s.trim().to_lowercase())
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
    let entry = json!({
        "id": Uuid::new_v4().to_string(),
        "sessionKey": session_key,
        "channel": route.channel,
        "to": route.to,
        "accountId": entry_account_id,
        "text": text,
        "message": text,
        "idempotencyKey": idempotency_key,
        "createdAtMs": now_ms(),
    });

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
        return Ok(json!({ "ok": true, "queued": true, "message": entry, "result": entry }));
    }

    let gateway_url = resolve_gateway_url(&cfg);
    if let Some(base_url) = gateway_url {
        let send_url = resolve_send_url(&base_url);
        let response = send_via_http(&send_url, &entry);
        let response = match response {
            Ok(value) => value,
            Err(err) => {
                if best_effort {
                    return Ok(json!({ "ok": false, "bestEffort": true, "error": err.to_string() }));
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
        let result = response.get("result").cloned().unwrap_or(response);
        return Ok(json!({ "ok": true, "result": result }));
    }

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
        let a_ts = a.get("updatedAtMs").and_then(|v| v.as_i64()).unwrap_or(0);
        let b_ts = b.get("updatedAtMs").and_then(|v| v.as_i64()).unwrap_or(0);
        b_ts.cmp(&a_ts)
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
        .map(|s| s.trim().to_string())
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

    routes.sort_by(|a, b| b.1.updated_at_ms.cmp(&a.1.updated_at_ms));

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
    let channel = payload
        .get("channel")
        .and_then(|v| v.as_str())
        .context("incoming requires channel")?;
    let from = payload
        .get("from")
        .and_then(|v| v.as_str())
        .context("incoming requires from")?;
    let text = payload
        .get("text")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let session_key = format!("{channel}:{from}");
    let entry = json!({
        "id": Uuid::new_v4().to_string(),
        "sessionKey": session_key,
        "channel": channel,
        "from": from,
        "accountId": payload.get("accountId").and_then(|v| v.as_str()),
        "text": text,
        "receivedAtMs": now_ms(),
    });

    append_json_line(&inbox_path(paths), &entry)?;

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
                let response = handle_ws_frame(&frame, &paths, &conn_id, &auth, &mut authorized);
                if let Some(response) = response {
                    let _ = websocket.send(Message::Text(response.to_string()));
                }
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
) -> Option<Value> {
    if frame.get("type").and_then(|v| v.as_str()) != Some("req") {
        return None;
    }
    let id = frame.get("id")?.as_str()?.to_string();
    let method = frame.get("method").and_then(|v| v.as_str()).unwrap_or("");
    let params = frame.get("params").cloned().unwrap_or_else(|| json!({}));

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
                if let Err(err) = authorize_gateway_auth(auth, &attempt) {
                    return Some(ws_response_err(&id, "unauthorized", &err.message));
                }
                *authorized = true;
            }
            Some(ws_response_ok(&id, hello_ok_payload(conn_id)))
        }
        "send" => {
            let result = send_message_with_mode(paths, &params, SendMode::Queue);
            match result {
                Ok(payload) => Some(ws_response_ok(&id, payload)),
                Err(err) => Some(ws_response_err(&id, "invalid_request", &err.to_string())),
            }
        }
        "health" => Some(ws_response_ok(&id, json!({ "ok": true }))),
        _ => Some(ws_response_err(
            &id,
            "method_not_found",
            &format!("unsupported method: {method}"),
        )),
    }
}

fn hello_ok_payload(conn_id: &str) -> Value {
    let host = std::env::var("HOSTNAME").unwrap_or_else(|_| "localhost".to_string());
    json!({
        "type": "hello-ok",
        "protocol": 1,
        "server": {
            "version": env!("CARGO_PKG_VERSION"),
            "host": host,
            "connId": conn_id,
        },
        "features": {
            "methods": ["send", "health"],
            "events": [],
        },
        "snapshot": {
            "presence": [],
            "health": {},
            "stateVersion": { "presence": 0, "health": 0 },
            "uptimeMs": 0,
        },
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
    let requires_auth = !matches!((&method, url.as_str()), (&Method::Get, "/v1/health"));
    if requires_auth {
        let cfg = load_gateway_config(paths)?;
        let auth = resolve_gateway_auth(&cfg);
        if auth.required() {
            let attempt = extract_http_auth(request);
            if let Err(err) = authorize_gateway_auth(&auth, &attempt) {
                return Ok(unauthorized_response(&err.message));
            }
        }
    }
    match (&method, url.as_str()) {
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

    #[test]
    fn gateway_auth_none_allows() {
        let auth = GatewayAuth::none();
        let attempt = GatewayAuthAttempt::default();
        assert!(authorize_gateway_auth(&auth, &attempt).is_ok());
    }

    #[test]
    fn gateway_auth_token_requires_match() {
        let auth = GatewayAuth::token("secret");
        let missing = GatewayAuthAttempt::default();
        assert!(authorize_gateway_auth(&auth, &missing).is_err());

        let wrong = GatewayAuthAttempt {
            token: Some("wrong".to_string()),
            password: None,
        };
        assert!(authorize_gateway_auth(&auth, &wrong).is_err());

        let ok = GatewayAuthAttempt {
            token: Some("secret".to_string()),
            password: None,
        };
        assert!(authorize_gateway_auth(&auth, &ok).is_ok());
    }

    #[test]
    fn gateway_auth_password_requires_match() {
        let auth = GatewayAuth::password("secret");
        let missing = GatewayAuthAttempt::default();
        assert!(authorize_gateway_auth(&auth, &missing).is_err());

        let wrong = GatewayAuthAttempt {
            token: Some("wrong".to_string()),
            password: None,
        };
        assert!(authorize_gateway_auth(&auth, &wrong).is_err());

        let ok = GatewayAuthAttempt {
            token: None,
            password: Some("secret".to_string()),
        };
        assert!(authorize_gateway_auth(&auth, &ok).is_ok());
    }
}
