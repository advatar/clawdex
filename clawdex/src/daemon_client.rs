use std::time::Duration;

use reqwest::blocking::Client;
use serde_json::{json, Value};

pub fn cron_run(job_id: &str, mode: &str) -> Option<Value> {
    #[cfg(unix)]
    if let Some(value) = cron_run_via_ipc(job_id, mode) {
        return Some(value);
    }
    cron_run_via_http(job_id, mode)
}

fn cron_run_via_http(job_id: &str, mode: &str) -> Option<Value> {
    let base = std::env::var("CLAWDEX_DAEMON_URL")
        .unwrap_or_else(|_| "http://127.0.0.1:18791".to_string());
    let base = base.trim_end_matches('/');
    let endpoint = format!("{}/v1/cron/jobs/{}/run", base, job_id);

    let client = Client::builder()
        .timeout(Duration::from_millis(500))
        .build()
        .ok()?;
    let response = client
        .post(endpoint)
        .json(&json!({ "mode": mode }))
        .send()
        .ok()?;

    if !response.status().is_success() {
        return None;
    }
    response.json::<Value>().ok()
}

#[cfg(unix)]
fn cron_run_via_ipc(job_id: &str, mode: &str) -> Option<Value> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let socket = std::env::var("CLAWDEX_DAEMON_IPC").ok()?;
    let socket = socket.trim();
    if socket.is_empty() {
        return None;
    }

    let mut stream = UnixStream::connect(socket).ok()?;
    let request = json!({
        "jsonrpc": "2.0",
        "id": format!("cron-run-{}", job_id),
        "method": "daemon.request",
        "params": {
            "httpMethod": "POST",
            "path": format!("/v1/cron/jobs/{}/run", job_id),
            "body": {
                "mode": mode
            }
        }
    });
    let line = serde_json::to_string(&request).ok()?;
    writeln!(stream, "{line}").ok()?;
    stream.flush().ok();

    let mut reader = BufReader::new(stream);
    let mut response_line = String::new();
    let bytes = reader.read_line(&mut response_line).ok()?;
    if bytes == 0 {
        return None;
    }
    let response: Value = serde_json::from_str(response_line.trim()).ok()?;
    if let Some(status) = response
        .pointer("/result/status")
        .and_then(|value| value.as_u64())
    {
        if status >= 400 {
            return None;
        }
    }
    response.pointer("/result/body").cloned()
}
