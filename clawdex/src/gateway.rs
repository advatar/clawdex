use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tiny_http::{Method, Response, Server, StatusCode};
use uuid::Uuid;

use crate::config::{ClawdPaths, GatewayConfig};
use crate::util::{append_json_line, now_ms, read_json_lines, read_json_value, write_json_value};

const GATEWAY_DIR: &str = "gateway";
const OUTBOX_FILE: &str = "outbox.jsonl";
const INBOX_FILE: &str = "inbox.jsonl";
const ROUTES_FILE: &str = "routes.json";
const IDEMPOTENCY_FILE: &str = "idempotency.json";
const INBOX_OFFSET_FILE: &str = "inbox_offset.json";

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

fn route_cutoff_ms(cfg: &GatewayConfig) -> Option<i64> {
    cfg.route_ttl_ms
        .map(|ttl| now_ms().saturating_sub(ttl as i64))
}

fn route_is_fresh(route: &RouteEntry, cutoff: Option<i64>) -> bool {
    cutoff.map(|cutoff| route.updated_at_ms >= cutoff).unwrap_or(true)
}

pub fn send_message(paths: &ClawdPaths, args: &Value) -> Result<Value> {
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

    let channel = args.get("channel").and_then(|v| v.as_str());
    let to = args.get("to").and_then(|v| v.as_str());
    let session_key = args
        .get("sessionKey")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .or_else(|| match (channel, to) {
            (Some(c), Some(t)) => Some(format!("{c}:{t}")),
            _ => None,
        })
        .unwrap_or_else(|| "agent:main:main".to_string());

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
    let route = match (channel, to) {
        (Some(c), Some(t)) => RouteEntry {
            channel: c.to_string(),
            to: t.to_string(),
            account_id: args.get("accountId").and_then(|v| v.as_str()).map(|s| s.to_string()),
            updated_at_ms: now_ms(),
        },
        _ => match route_store.get_route(&session_key) {
            Some(route) if route_is_fresh(&route, cutoff) => route,
            _ => {
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
        },
    };

    let mut idempotency = IdempotencyStore::load(paths)?;
    if idempotency.seen(&idempotency_key) {
        return Ok(json!({ "ok": true, "deduped": true }));
    }

    let entry = json!({
        "id": Uuid::new_v4().to_string(),
        "sessionKey": session_key,
        "channel": route.channel,
        "to": route.to,
        "accountId": route.account_id,
        "text": text,
        "idempotencyKey": idempotency_key,
        "createdAtMs": now_ms(),
    });

    append_json_line(&outbox_path(paths), &entry)?;
    route_store.update_route(
        &session_key,
        RouteEntry {
            updated_at_ms: now_ms(),
            ..route.clone()
        },
    )?;
    idempotency.insert(&idempotency_key, now_ms())?;

    Ok(json!({ "ok": true, "queued": true, "message": entry, "result": entry }))
}

pub fn list_channels(paths: &ClawdPaths) -> Result<Value> {
    let store = RouteStore::load(paths)?;
    let cfg = load_gateway_config(paths)?;
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

    Ok(json!({
        "channels": entries,
        "count": entries.len(),
        "routeTtlMs": cfg.route_ttl_ms,
        "disabled": false,
    }))
}

pub fn resolve_target(paths: &ClawdPaths, args: &Value) -> Result<Value> {
    let channel = args
        .get("channel")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let to = args
        .get("to")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let account_id = args
        .get("accountId")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

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

fn handle_request(
    paths: &ClawdPaths,
    request: &mut tiny_http::Request,
) -> Result<Response<std::io::Cursor<Vec<u8>>>> {
    let method = request.method().clone();
    let url = request.url().to_string();
    match (&method, url.as_str()) {
        (&Method::Get, "/v1/health") => Ok(json_response(json!({ "ok": true }))?),
        (&Method::Post, "/v1/send") => {
            let body = read_body(request)?;
            let payload: Value = serde_json::from_slice(&body).context("invalid json")?;
            let result = send_message(paths, &payload)?;
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
