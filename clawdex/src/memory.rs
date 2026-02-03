use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;

use anyhow::{Context, Result};
use reqwest::blocking::Client;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Deserialize;
use serde_json::{json, Value};
use walkdir::WalkDir;

use crate::config::{
    resolve_embeddings_config, resolve_memory_enabled, resolve_workspace_path, ClawdPaths,
    EmbeddingsConfig,
};
use crate::util::read_to_string;

const DB_FILE: &str = "fts.sqlite";

#[derive(Debug, Clone)]
struct SearchRow {
    path: String,
    line_no: i64,
    text: String,
    fts_score: f64,
    embed_score: Option<f64>,
    final_score: f64,
}

#[derive(Debug, Clone)]
struct EmbeddingProvider {
    client: Client,
    api_base: String,
    model: String,
    api_key: String,
    batch_size: usize,
}

#[derive(Debug, Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingData>,
}

#[derive(Debug, Deserialize)]
struct EmbeddingData {
    embedding: Vec<f32>,
    index: usize,
}

pub fn memory_get(paths: &ClawdPaths, args: &Value) -> Result<Value> {
    let cfg = paths_config(paths)?;
    if !resolve_memory_enabled(&cfg) {
        return Ok(json!({ "ok": false, "reason": "memory disabled" }));
    }

    let path_str = args
        .get("path")
        .and_then(|v| v.as_str())
        .context("memory_get requires path")?;

    let resolved = resolve_workspace_path(paths, path_str)?;
    let rel = resolved
        .strip_prefix(&paths.workspace_dir)
        .unwrap_or(&resolved)
        .to_path_buf();

    if !allowed_memory_path(&rel) {
        anyhow::bail!("path not allowed for memory_get");
    }

    let contents = read_to_string(&resolved)?;
    let lines: Vec<&str> = contents.lines().collect();
    let total_lines = lines.len();

    let from = args.get("from").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
    let count = args.get("lines").and_then(|v| v.as_u64()).map(|v| v as usize);
    let start_idx = from.saturating_sub(1).min(total_lines);
    let end_idx = match count {
        Some(count) => (start_idx + count).min(total_lines),
        None => total_lines,
    };

    let slice = lines[start_idx..end_idx].join("\n");

    Ok(json!({
        "path": rel.to_string_lossy(),
        "from": from,
        "lines": end_idx.saturating_sub(start_idx),
        "totalLines": total_lines,
        "content": slice,
    }))
}

pub fn memory_search(paths: &ClawdPaths, args: &Value) -> Result<Value> {
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .context("memory_search requires query")?;
    let max_results = args
        .get("maxResults")
        .or_else(|| args.get("max_results"))
        .and_then(|v| v.as_u64())
        .unwrap_or(20) as usize;
    let min_score = args
        .get("minScore")
        .or_else(|| args.get("min_score"))
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);

    let cfg = paths_config(paths)?;
    if !resolve_memory_enabled(&cfg) {
        return Ok(json!({ "results": [], "query": query, "disabled": true }));
    }
    let embeddings_cfg = resolve_embeddings_config(&cfg);
    ensure_index(paths, &embeddings_cfg)?;
    let conn = open_db(paths)?;

    let mut rows = Vec::new();
    let mut stmt = conn.prepare(
        "SELECT path, line_no, text, bm25(memory_fts) as score FROM memory_fts WHERE memory_fts MATCH ? ORDER BY score LIMIT ?",
    )?;
    let mut results = stmt.query(params![query, max_results as i64])?;
    while let Some(row) = results.next()? {
        let path: String = row.get(0)?;
        let line_no: i64 = row.get(1)?;
        let text: String = row.get(2)?;
        let bm25: f64 = row.get(3)?;
        let fts_score = 1.0 / (1.0 + bm25.abs());
        rows.push(SearchRow {
            path,
            line_no,
            text,
            fts_score,
            embed_score: None,
            final_score: fts_score,
        });
    }

    if let Some(provider) = build_embedding_provider(&embeddings_cfg)? {
        if !rows.is_empty() {
            let query_vec = provider.embed(&[query.to_string()])?;
            if let Some(query_embedding) = query_vec.first() {
                let mut embed_scores = HashMap::new();
                for chunk in rows.chunks(provider.batch_size) {
                    let mut inputs = Vec::new();
                    for row in chunk {
                        inputs.push(row.text.clone());
                    }
                    let vectors = provider.embed(&inputs)?;
                    for (idx, vec) in vectors.into_iter().enumerate() {
                        let row = &chunk[idx];
                        let score = cosine_similarity(query_embedding, &vec);
                        embed_scores.insert((row.path.clone(), row.line_no), score);
                    }
                }

                for row in &mut rows {
                    if let Some(score) = embed_scores.get(&(row.path.clone(), row.line_no)) {
                        row.embed_score = Some(*score);
                        row.final_score = 0.6 * row.fts_score + 0.4 * score;
                    }
                }
            }
        }
    }

    rows.sort_by(|a, b| b.final_score.partial_cmp(&a.final_score).unwrap());

    let mut output = Vec::new();
    for row in rows.into_iter() {
        if row.final_score < min_score {
            continue;
        }
        let rel = Path::new(&row.path)
            .strip_prefix(&paths.workspace_dir)
            .unwrap_or(Path::new(&row.path))
            .to_string_lossy()
            .to_string();
        output.push(json!({
            "path": rel,
            "lineStart": row.line_no,
            "lineEnd": row.line_no,
            "snippet": row.text,
            "score": row.final_score,
            "ftsScore": row.fts_score,
            "embeddingScore": row.embed_score,
        }));
    }

    Ok(json!({
        "results": output,
        "query": query,
    }))
}

fn ensure_index(paths: &ClawdPaths, embeddings_cfg: &EmbeddingsConfig) -> Result<()> {
    let mut conn = open_db(paths)?;
    ensure_schema(&conn)?;

    let files = memory_files(&paths.workspace_dir);
    let mut current = HashMap::new();
    for file in &files {
        let meta = std::fs::metadata(file).with_context(|| format!("metadata {}", file.display()))?;
        let modified = meta
            .modified()
            .ok()
            .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let size = meta.len() as i64;
        current.insert(file.to_string_lossy().to_string(), (modified, size));
    }

    let mut stale = Vec::new();
    let mut existing = Vec::new();
    {
        let mut stmt = conn.prepare("SELECT path, mtime, size FROM memory_files")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let path: String = row.get(0)?;
            let mtime: i64 = row.get(1)?;
            let size: i64 = row.get(2)?;
            existing.push((path, mtime, size));
        }
    }

    for (path, mtime, size) in existing {
        match current.get(&path) {
            Some((cur_mtime, cur_size)) if *cur_mtime == mtime && *cur_size == size => {}
            Some(_) => {
                reindex_file(&mut conn, Path::new(&path), embeddings_cfg)?;
            }
            None => stale.push(path),
        }
    }

    for (path, _) in current.iter() {
        let exists = conn.query_row(
            "SELECT 1 FROM memory_files WHERE path = ?",
            [path],
            |_| Ok(1),
        )
        .optional()?;
        if exists.is_none() {
            reindex_file(&mut conn, Path::new(path), embeddings_cfg)?;
        }
    }

    for path in stale {
        conn.execute("DELETE FROM memory_fts WHERE path = ?", [path.as_str()])?;
        conn.execute("DELETE FROM memory_embeddings WHERE path = ?", [path.as_str()])?;
        conn.execute("DELETE FROM memory_files WHERE path = ?", [path.as_str()])?;
    }

    Ok(())
}

fn reindex_file(conn: &mut Connection, path: &Path, embeddings_cfg: &EmbeddingsConfig) -> Result<()> {
    let contents = read_to_string(path)?;
    let meta = std::fs::metadata(path).with_context(|| format!("metadata {}", path.display()))?;
    let modified = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let size = meta.len() as i64;

    conn.execute("DELETE FROM memory_fts WHERE path = ?", [path.to_string_lossy().as_ref()])?;
    conn.execute(
        "DELETE FROM memory_embeddings WHERE path = ?",
        [path.to_string_lossy().as_ref()],
    )?;

    let tx = conn.transaction()?;
    for (idx, line) in contents.lines().enumerate() {
        tx.execute(
            "INSERT INTO memory_fts(path, line_no, text) VALUES (?1, ?2, ?3)",
            params![path.to_string_lossy().as_ref(), (idx + 1) as i64, line],
        )?;
    }
    tx.execute(
        "INSERT OR REPLACE INTO memory_files(path, mtime, size) VALUES (?1, ?2, ?3)",
        params![path.to_string_lossy().as_ref(), modified, size],
    )?;
    tx.commit()?;

    if let Some(provider) = build_embedding_provider(embeddings_cfg)? {
        let lines: Vec<String> = contents.lines().map(|s| s.to_string()).collect();
        for (chunk_index, chunk) in lines.chunks(provider.batch_size).enumerate() {
            let vectors = provider.embed(&chunk.to_vec())?;
            let tx = conn.transaction()?;
            for (offset, vector) in vectors.into_iter().enumerate() {
                let line_no = chunk_index * provider.batch_size + offset + 1;
                let vec_json = serde_json::to_string(&vector)?;
                tx.execute(
                    "INSERT INTO memory_embeddings(path, line_no, vector) VALUES (?1, ?2, ?3)",
                    params![path.to_string_lossy().as_ref(), line_no as i64, vec_json],
                )?;
            }
            tx.commit()?;
        }
    }

    Ok(())
}

fn open_db(paths: &ClawdPaths) -> Result<Connection> {
    let db_path = paths.memory_dir.join(DB_FILE);
    std::fs::create_dir_all(&paths.memory_dir)
        .with_context(|| format!("create memory dir {}", paths.memory_dir.display()))?;
    Connection::open(db_path).context("open sqlite")
}

fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute(
        "CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(path, line_no UNINDEXED, text, tokenize='unicode61')",
        [],
    )?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS memory_files (path TEXT PRIMARY KEY, mtime INTEGER, size INTEGER)",
        [],
    )?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS memory_embeddings (path TEXT, line_no INTEGER, vector TEXT, PRIMARY KEY(path, line_no))",
        [],
    )?;
    Ok(())
}

fn memory_files(workspace: &Path) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let memory_md = workspace.join("MEMORY.md");
    if memory_md.exists() {
        files.push(memory_md);
    }
    let mem_dir = workspace.join("memory");
    if mem_dir.exists() {
        for entry in WalkDir::new(mem_dir)
            .follow_links(false)
            .into_iter()
            .filter_map(Result::ok)
        {
            if entry.file_type().is_file() {
                if let Some(ext) = entry.path().extension() {
                    if ext == "md" {
                        files.push(entry.path().to_path_buf());
                    }
                }
            }
        }
    }
    files
}

fn allowed_memory_path(relative: &Path) -> bool {
    let rel = relative.to_string_lossy();
    if rel == "MEMORY.md" || rel == "memory/MEMORY.md" {
        return true;
    }
    rel.starts_with("memory/") && rel.ends_with(".md")
}

fn build_embedding_provider(cfg: &EmbeddingsConfig) -> Result<Option<EmbeddingProvider>> {
    let enabled = cfg.enabled.unwrap_or(false);
    if !enabled {
        return Ok(None);
    }
    let model = match cfg.model.as_ref() {
        Some(model) => model.clone(),
        None => return Ok(None),
    };

    let provider = cfg
        .provider
        .as_deref()
        .unwrap_or("openai")
        .to_lowercase();

    let api_base = cfg
        .api_base
        .clone()
        .or_else(|| default_api_base(&provider))
        .unwrap_or_default();
    if api_base.is_empty() {
        return Ok(None);
    }

    let api_key_env = cfg
        .api_key_env
        .clone()
        .unwrap_or_else(|| default_api_key_env(&provider));
    let api_key = std::env::var(&api_key_env).unwrap_or_default();
    if api_key.is_empty() {
        return Ok(None);
    }

    let batch_size = cfg.batch_size.unwrap_or(32);
    Ok(Some(EmbeddingProvider {
        client: Client::new(),
        api_base,
        model,
        api_key,
        batch_size,
    }))
}

fn default_api_base(provider: &str) -> Option<String> {
    if provider == "openai" || provider == "codex" || provider == "openai-compatible" {
        if let Ok(env) = std::env::var("OPENAI_API_BASE") {
            if !env.trim().is_empty() {
                return Some(env);
            }
        }
        if let Ok(env) = std::env::var("OPENAI_BASE_URL") {
            if !env.trim().is_empty() {
                return Some(env);
            }
        }
        return Some("https://api.openai.com".to_string());
    }

    if provider.starts_with("http://") || provider.starts_with("https://") {
        return Some(provider.to_string());
    }

    None
}

fn default_api_key_env(provider: &str) -> String {
    if provider == "openai" || provider == "codex" || provider == "openai-compatible" {
        return "OPENAI_API_KEY".to_string();
    }
    "OPENAI_API_KEY".to_string()
}

impl EmbeddingProvider {
    fn embed(&self, inputs: &[String]) -> Result<Vec<Vec<f32>>> {
        let url = if self.api_base.trim_end_matches('/').ends_with("/v1") {
            format!("{}/embeddings", self.api_base.trim_end_matches('/'))
        } else {
            format!("{}/v1/embeddings", self.api_base.trim_end_matches('/'))
        };
        let payload = json!({
            "model": self.model,
            "input": inputs,
        });
        let resp = self
            .client
            .post(url)
            .bearer_auth(&self.api_key)
            .json(&payload)
            .send()
            .context("embeddings request")?;
        if !resp.status().is_success() {
            let status = resp.status();
            let body = resp.text().unwrap_or_default();
            anyhow::bail!("embeddings request failed ({status}): {body}");
        }
        let data: EmbeddingResponse = resp.json().context("parse embeddings response")?;
        let mut out = data.data;
        out.sort_by_key(|d| d.index);
        Ok(out.into_iter().map(|d| d.embedding).collect())
    }
}

fn cosine_similarity(a: &[f32], b: &[f32]) -> f64 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let mut dot = 0.0f64;
    let mut norm_a = 0.0f64;
    let mut norm_b = 0.0f64;
    for (x, y) in a.iter().zip(b.iter()) {
        let xf = *x as f64;
        let yf = *y as f64;
        dot += xf * yf;
        norm_a += xf * xf;
        norm_b += yf * yf;
    }
    if norm_a == 0.0 || norm_b == 0.0 {
        return 0.0;
    }
    dot / (norm_a.sqrt() * norm_b.sqrt())
}

fn paths_config(paths: &ClawdPaths) -> Result<crate::config::ClawdConfig> {
    // Reload config to access memory settings. This avoids threading config through all calls.
    let (cfg, _) = crate::config::load_config(Some(paths.state_dir.clone()), Some(paths.workspace_dir.clone()))?;
    Ok(cfg)
}
