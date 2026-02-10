use std::time::Duration;

use reqwest::blocking::Client;
use serde_json::{json, Value};

pub fn cron_run(job_id: &str, mode: &str) -> Option<Value> {
    let base = std::env::var("CLAWDEX_DAEMON_URL").unwrap_or_else(|_| {
        "http://127.0.0.1:18791".to_string()
    });
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
