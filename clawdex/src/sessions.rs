use std::path::PathBuf;

use anyhow::Result;
use serde_json::json;

use crate::config::ClawdPaths;
use crate::util::{append_json_line, now_ms};

pub fn session_transcript_path(paths: &ClawdPaths, session_key: &str) -> PathBuf {
    let trimmed = session_key.trim();
    if trimmed.is_empty() {
        return paths.sessions_dir.join("session-unknown.jsonl");
    }
    let mut sanitized: String = trimmed
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if sanitized.len() > 48 {
        sanitized.truncate(48);
    }
    let hash = fnv1a_64(trimmed);
    let filename = format!("{sanitized}-{hash:016x}.jsonl");
    paths.sessions_dir.join(filename)
}

pub fn append_session_message(
    paths: &ClawdPaths,
    session_key: &str,
    role: &str,
    text: &str,
) -> Result<()> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return Ok(());
    }
    let path = session_transcript_path(paths, session_key);
    std::fs::create_dir_all(&paths.sessions_dir)?;
    let entry = json!({
        "type": "message",
        "timestampMs": now_ms(),
        "message": {
            "role": role,
            "content": [
                {
                    "type": "text",
                    "text": trimmed,
                }
            ],
        }
    });
    append_json_line(&path, &entry)?;
    Ok(())
}

fn fnv1a_64(value: &str) -> u64 {
    const OFFSET: u64 = 0xcbf29ce484222325;
    const PRIME: u64 = 0x100000001b3;
    let mut hash = OFFSET;
    for byte in value.as_bytes() {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}
