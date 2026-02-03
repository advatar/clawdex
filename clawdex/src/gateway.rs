use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tiny_http::{Method, Response, Server, StatusCode};
use uuid::Uuid;

use crate::config::ClawdPaths;
use crate::util::{append_json_line, now_ms, read_json_value, write_json_value};

const GATEWAY_DIR: &str = "gateway";
const OUTBOX_FILE: &str = "outbox.jsonl";
const INBOX_FILE: &str = "inbox.jsonl";
const ROUTES_FILE: &str = "routes.json";
const IDEMPOTENCY_FILE: &str = "idempotency.json";
const DEFAULT_CHANNEL: &str = "local";
const DEFAULT_TO: &str = "console";

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

pub fn send_message(paths: &ClawdPaths, args: &Value) -> Result<Value> {
    let text = args
        .get("text")
        .or_else(|| args.get("message"))
        .and_then(|v| v.as_str())
        .context("message.send requires text or message")?;

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

    let mut route_store = RouteStore::load(paths)?;
    let route = match (channel, to) {
        (Some(c), Some(t)) => RouteEntry {
            channel: c.to_string(),
            to: t.to_string(),
            account_id: args.get("accountId").and_then(|v| v.as_str()).map(|s| s.to_string()),
            updated_at_ms: now_ms(),
        },
        _ => route_store.get_route(&session_key).unwrap_or(RouteEntry {
            channel: DEFAULT_CHANNEL.to_string(),
            to: DEFAULT_TO.to_string(),
            account_id: None,
            updated_at_ms: now_ms(),
        }),
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

    Ok(json!({ "ok": true, "queued": true, "message": entry }))
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
