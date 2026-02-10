use std::fs;
use std::net::TcpListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use anyhow::Result;
use serde_json::{json, Value};
use tiny_http::{Method, Response, Server, StatusCode};
use uuid::Uuid;

use clawdex::config::load_config;
use clawdex::memory;

fn env_test_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

struct EmbeddingsServer {
    base_url: String,
    shutdown: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
    requests: Arc<AtomicUsize>,
}

impl EmbeddingsServer {
    fn start() -> Result<Self> {
        Self::start_with_handler(handle_embeddings_request)
    }

    fn start_failing() -> Result<Self> {
        Self::start_with_handler(handle_embeddings_request_failing)
    }

    fn start_rejecting_empty() -> Result<Self> {
        Self::start_with_handler(handle_embeddings_request_reject_empty)
    }

    fn start_ollama() -> Result<Self> {
        Self::start_with_handler(handle_ollama_embeddings_request)
    }

    fn start_with_handler(handler: fn(tiny_http::Request) -> Result<()>) -> Result<Self> {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let addr = listener.local_addr()?;
        let server = Server::from_listener(listener, None).expect("tiny_http server");
        let base_url = format!("http://{}", addr);
        let shutdown = Arc::new(AtomicBool::new(false));
        let requests = Arc::new(AtomicUsize::new(0));

        let shutdown_thread = shutdown.clone();
        let requests_thread = requests.clone();
        let handle = thread::spawn(move || {
            while !shutdown_thread.load(Ordering::SeqCst) {
                let req = match server.recv_timeout(Duration::from_millis(100)) {
                    Ok(Some(req)) => req,
                    Ok(None) => continue,
                    Err(_) => break,
                };
                requests_thread.fetch_add(1, Ordering::SeqCst);
                let _ = handler(req);
            }
        });

        Ok(Self {
            base_url,
            shutdown,
            handle: Some(handle),
            requests,
        })
    }

    fn api_base(&self) -> String {
        format!("{}/v1", self.base_url.trim_end_matches('/'))
    }

    fn count(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }
}

impl Drop for EmbeddingsServer {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::SeqCst);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn handle_embeddings_request(mut request: tiny_http::Request) -> Result<()> {
    if request.method() != &Method::Post {
        let _ = request.respond(Response::empty(StatusCode(405)));
        return Ok(());
    }
    let url = request.url().to_string();
    if url != "/v1/embeddings" && url != "/embeddings" {
        let _ = request.respond(Response::empty(StatusCode(404)));
        return Ok(());
    }

    let mut body = String::new();
    request.as_reader().read_to_string(&mut body)?;
    let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);

    let mut inputs = Vec::new();
    if let Some(array) = parsed.get("input").and_then(|v| v.as_array()) {
        for item in array {
            if let Some(text) = item.as_str() {
                inputs.push(text.to_string());
            }
        }
    } else if let Some(text) = parsed.get("input").and_then(|v| v.as_str()) {
        inputs.push(text.to_string());
    }

    let data = inputs
        .iter()
        .enumerate()
        .map(|(idx, text)| {
            let lower = text.to_lowercase();
            let embedding = if lower.contains("needle") {
                vec![1.0_f32, 0.0_f32, 0.0_f32]
            } else {
                vec![0.0_f32, 1.0_f32, 0.0_f32]
            };
            json!({ "index": idx, "embedding": embedding })
        })
        .collect::<Vec<_>>();

    let response = json!({ "data": data });
    let bytes = serde_json::to_vec(&response)?;
    let header = tiny_http::Header::from_bytes(
        &b"Content-Type"[..],
        &b"application/json"[..],
    )
    .expect("content-type header");
    let _ = request.respond(Response::from_data(bytes).with_header(header));
    Ok(())
}

fn handle_ollama_embeddings_request(mut request: tiny_http::Request) -> Result<()> {
    if request.method() != &Method::Post {
        let _ = request.respond(Response::empty(StatusCode(405)));
        return Ok(());
    }
    let url = request.url().to_string();
    if url != "/api/embeddings" {
        let _ = request.respond(Response::empty(StatusCode(404)));
        return Ok(());
    }

    let mut body = String::new();
    request.as_reader().read_to_string(&mut body)?;
    let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
    let prompt = parsed
        .get("prompt")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();

    let lower = prompt.to_lowercase();
    let embedding = if lower.contains("needle") {
        vec![1.0_f32, 0.0_f32, 0.0_f32]
    } else {
        vec![0.0_f32, 1.0_f32, 0.0_f32]
    };

    let response = json!({ "embedding": embedding });
    let bytes = serde_json::to_vec(&response)?;
    let header = tiny_http::Header::from_bytes(
        &b"Content-Type"[..],
        &b"application/json"[..],
    )
    .expect("content-type header");
    let _ = request.respond(Response::from_data(bytes).with_header(header));
    Ok(())
}

fn handle_embeddings_request_reject_empty(mut request: tiny_http::Request) -> Result<()> {
    if request.method() != &Method::Post {
        let _ = request.respond(Response::empty(StatusCode(405)));
        return Ok(());
    }
    let url = request.url().to_string();
    if url != "/v1/embeddings" && url != "/embeddings" {
        let _ = request.respond(Response::empty(StatusCode(404)));
        return Ok(());
    }

    let mut body = String::new();
    request.as_reader().read_to_string(&mut body)?;
    let parsed: Value = serde_json::from_str(&body).unwrap_or(Value::Null);

    let mut inputs = Vec::new();
    if let Some(array) = parsed.get("input").and_then(|v| v.as_array()) {
        for item in array {
            if let Some(text) = item.as_str() {
                inputs.push(text.to_string());
            }
        }
    } else if let Some(text) = parsed.get("input").and_then(|v| v.as_str()) {
        inputs.push(text.to_string());
    }

    if inputs.iter().any(|text| text.trim().is_empty()) {
        let _ = request.respond(Response::from_string("empty input").with_status_code(StatusCode(400)));
        return Ok(());
    }

    let data = inputs
        .iter()
        .enumerate()
        .map(|(idx, text)| {
            let lower = text.to_lowercase();
            let embedding = if lower.contains("needle") {
                vec![1.0_f32, 0.0_f32, 0.0_f32]
            } else {
                vec![0.0_f32, 1.0_f32, 0.0_f32]
            };
            json!({ "index": idx, "embedding": embedding })
        })
        .collect::<Vec<_>>();

    let response = json!({ "data": data });
    let bytes = serde_json::to_vec(&response)?;
    let header = tiny_http::Header::from_bytes(
        &b"Content-Type"[..],
        &b"application/json"[..],
    )
    .expect("content-type header");
    let _ = request.respond(Response::from_data(bytes).with_header(header));
    Ok(())
}

fn handle_embeddings_request_failing(request: tiny_http::Request) -> Result<()> {
    if request.method() != &Method::Post {
        let _ = request.respond(Response::empty(StatusCode(405)));
        return Ok(());
    }
    let url = request.url().to_string();
    if url != "/v1/embeddings" && url != "/embeddings" {
        let _ = request.respond(Response::empty(StatusCode(404)));
        return Ok(());
    }
    let _ = request.respond(Response::from_string("boom").with_status_code(StatusCode(500)));
    Ok(())
}

fn temp_paths() -> Result<(PathBuf, clawdex::config::ClawdPaths)> {
    let base = std::env::temp_dir().join(format!("clawdex-memory-test-{}", Uuid::new_v4()));
    let state_dir = base.join("state");
    let workspace_dir = base.join("workspace");
    fs::create_dir_all(&workspace_dir)?;
    let (_cfg, paths) = load_config(Some(state_dir), Some(workspace_dir))?;
    Ok((base, paths))
}

fn write_config(paths: &clawdex::config::ClawdPaths, value: &Value) -> Result<()> {
    fs::write(
        paths.state_dir.join("config.json5"),
        serde_json::to_string_pretty(value)?,
    )?;
    Ok(())
}

#[test]
fn memory_search_uses_cached_embeddings_vectors() -> Result<()> {
    let _guard = env_test_lock()
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    let server = EmbeddingsServer::start()?;
    std::env::set_var("CLAWDEX_TEST_API_KEY_1", "ok");

    let (base, paths) = temp_paths()?;
    let memory_dir = paths.workspace_dir.join("memory");
    fs::create_dir_all(&memory_dir)?;
    fs::write(memory_dir.join("2026-02-01.md"), "alpha\nneedle here\nbeta\n")?;

    write_config(
        &paths,
        &json!({
            "memory": {
                "enabled": true,
                "session_memory": false,
                "embeddings": {
                    "enabled": true,
                    "provider": "openai",
                    "model": "test",
                    "api_base": server.api_base(),
                    "api_key_env": "CLAWDEX_TEST_API_KEY_1",
                    "batch_size": 32
                }
            }
        }),
    )?;

    let res1 = memory::memory_search(&paths, &json!({ "query": "needle" }))?;
    let results1 = res1.get("results").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    assert!(!results1.is_empty());
    assert!(results1[0].get("embeddingScore").and_then(|v| v.as_f64()).unwrap_or(0.0) > 0.9);

    let res2 = memory::memory_search(&paths, &json!({ "query": "needle" }))?;
    let results2 = res2.get("results").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    assert!(!results2.is_empty());

    // Expected requests:
    // - first search: one embed batch for indexing + one for query embedding
    // - second search: query embedding only (chunks are loaded from cache)
    assert_eq!(server.count(), 3);

    std::env::remove_var("CLAWDEX_TEST_API_KEY_1");
    let _ = fs::remove_dir_all(base);
    Ok(())
}

#[test]
fn memory_search_backfills_embeddings_when_enabled_later() -> Result<()> {
    let _guard = env_test_lock()
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    let server = EmbeddingsServer::start()?;

    let (base, paths) = temp_paths()?;
    let memory_dir = paths.workspace_dir.join("memory");
    fs::create_dir_all(&memory_dir)?;
    fs::write(memory_dir.join("2026-02-01.md"), "alpha\nneedle here\nbeta\n")?;

    // First pass: embeddings disabled (no API key, so provider resolves to None).
    std::env::remove_var("CLAWDEX_TEST_API_KEY_2");
    write_config(
        &paths,
        &json!({
            "memory": {
                "enabled": true,
                "session_memory": false,
                "embeddings": {
                    "enabled": true,
                    "provider": "openai",
                    "model": "test",
                    "api_base": server.api_base(),
                    "api_key_env": "CLAWDEX_TEST_API_KEY_2"
                }
            }
        }),
    )?;
    let res1 = memory::memory_search(&paths, &json!({ "query": "needle" }))?;
    let results1 = res1.get("results").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    assert!(!results1.is_empty());
    assert!(results1[0]
        .get("embeddingScore")
        .and_then(|v| v.as_f64())
        .is_none());
    assert_eq!(server.count(), 0);

    // Second pass: API key appears; we should backfill embeddings without reindexing the file.
    std::env::set_var("CLAWDEX_TEST_API_KEY_2", "ok");
    let res2 = memory::memory_search(&paths, &json!({ "query": "needle" }))?;
    let results2 = res2.get("results").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    assert!(!results2.is_empty());
    assert!(results2[0].get("embeddingScore").and_then(|v| v.as_f64()).unwrap_or(0.0) > 0.9);

    // Backfill embeddings + query embedding.
    assert_eq!(server.count(), 2);

    std::env::remove_var("CLAWDEX_TEST_API_KEY_2");
    let _ = fs::remove_dir_all(base);
    Ok(())
}

#[test]
fn memory_search_falls_back_when_embeddings_fail_and_uses_backoff() -> Result<()> {
    let _guard = env_test_lock()
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    let server = EmbeddingsServer::start_failing()?;
    std::env::set_var("CLAWDEX_TEST_API_KEY_3", "ok");

    let (base, paths) = temp_paths()?;
    let memory_dir = paths.workspace_dir.join("memory");
    fs::create_dir_all(&memory_dir)?;
    fs::write(memory_dir.join("2026-02-01.md"), "alpha\nneedle here\nbeta\n")?;

    write_config(
        &paths,
        &json!({
            "memory": {
                "enabled": true,
                "session_memory": false,
                "embeddings": {
                    "enabled": true,
                    "provider": "openai",
                    "model": "test",
                    "api_base": server.api_base(),
                    "api_key_env": "CLAWDEX_TEST_API_KEY_3",
                    "batch_size": 32
                }
            }
        }),
    )?;

    let res1 = memory::memory_search(&paths, &json!({ "query": "needle" }))?;
    let results1 = res1.get("results").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    assert!(!results1.is_empty());
    assert!(results1[0]
        .get("embeddingScore")
        .and_then(|v| v.as_f64())
        .is_none());
    let count_after_first = server.count();
    assert!(count_after_first > 0);

    let res2 = memory::memory_search(&paths, &json!({ "query": "needle" }))?;
    let results2 = res2.get("results").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    assert!(!results2.is_empty());

    // Second search should not retry embeddings immediately (query + file chunks are in backoff).
    assert_eq!(server.count(), count_after_first);

    std::env::remove_var("CLAWDEX_TEST_API_KEY_3");
    let _ = fs::remove_dir_all(base);
    Ok(())
}

#[test]
fn memory_index_skips_empty_chunks_for_embeddings() -> Result<()> {
    let _guard = env_test_lock()
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    let server = EmbeddingsServer::start_rejecting_empty()?;
    std::env::set_var("CLAWDEX_TEST_API_KEY_4", "ok");

    let (base, paths) = temp_paths()?;
    let memory_dir = paths.workspace_dir.join("memory");
    fs::create_dir_all(&memory_dir)?;
    fs::write(memory_dir.join("2026-02-01.md"), "\n\n\n")?;

    write_config(
        &paths,
        &json!({
            "memory": {
                "enabled": true,
                "session_memory": false,
                "embeddings": {
                    "enabled": true,
                    "provider": "openai",
                    "model": "test",
                    "api_base": server.api_base(),
                    "api_key_env": "CLAWDEX_TEST_API_KEY_4",
                    "batch_size": 32
                }
            }
        }),
    )?;

    let res = memory::memory_search(&paths, &json!({ "query": "needle" }))?;
    let results = res.get("results").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    assert!(results.is_empty());

    // Indexing should skip whitespace-only chunks (no embedding requests should be made).
    assert_eq!(server.count(), 0);

    std::env::remove_var("CLAWDEX_TEST_API_KEY_4");
    let _ = fs::remove_dir_all(base);
    Ok(())
}

#[test]
fn memory_search_supports_ollama_without_api_key() -> Result<()> {
    let _guard = env_test_lock()
        .lock()
        .unwrap_or_else(|err| err.into_inner());
    let server = EmbeddingsServer::start_ollama()?;
    std::env::remove_var("CLAWDEX_TEST_API_KEY_OLLAMA");

    let (base, paths) = temp_paths()?;
    let memory_dir = paths.workspace_dir.join("memory");
    fs::create_dir_all(&memory_dir)?;
    fs::write(memory_dir.join("2026-02-01.md"), "alpha\nneedle here\nbeta\n")?;

    write_config(
        &paths,
        &json!({
            "memory": {
                "enabled": true,
                "session_memory": false,
                "embeddings": {
                    "enabled": true,
                    "provider": "ollama",
                    "model": "nomic-embed-text",
                    "api_base": server.base_url,
                    "api_key_env": "CLAWDEX_TEST_API_KEY_OLLAMA",
                    "batch_size": 32
                }
            }
        }),
    )?;

    let res1 = memory::memory_search(&paths, &json!({ "query": "needle" }))?;
    let results1 = res1.get("results").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    assert!(!results1.is_empty());
    assert!(results1[0].get("embeddingScore").and_then(|v| v.as_f64()).unwrap_or(0.0) > 0.9);

    // Expected requests:
    // - first search: one embed call for indexing + one for query embedding
    assert_eq!(server.count(), 2);

    std::env::remove_var("CLAWDEX_TEST_API_KEY_OLLAMA");
    let _ = fs::remove_dir_all(base);
    Ok(())
}
