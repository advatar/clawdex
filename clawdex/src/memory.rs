use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::{Duration, UNIX_EPOCH};

use anyhow::{Context, Result};
use reqwest::blocking::Client;
use rusqlite::{params, params_from_iter, Connection, OptionalExtension};
use rusqlite::types::Value as SqlValue;
use serde::Deserialize;
use serde_json::{json, Value};
use walkdir::WalkDir;

use crate::config::{
    resolve_embeddings_config, resolve_memory_enabled, ClawdConfig, ClawdPaths, EmbeddingsConfig,
};
use crate::util::{now_ms, read_to_string};

const DB_FILE: &str = "fts.sqlite";
const DEFAULT_CHUNK_TOKENS: usize = 400;
const DEFAULT_CHUNK_OVERLAP: usize = 80;
const SCHEMA_VERSION: i64 = 2;
const EMBEDDINGS_QUERY_FAILURE_PATH: &str = "__query__";
const EMBEDDINGS_QUERY_FAILURE_SOURCE: &str = "__query__";

#[derive(Debug, Clone)]
struct SearchRow {
    path: String,
    start_line: i64,
    end_line: i64,
    text: String,
    source: String,
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

#[derive(Debug, Clone)]
struct MemoryChunk {
    start_line: i64,
    end_line: i64,
    text: String,
}

#[derive(Debug, Clone)]
struct IndexedFile {
    rel_path: String,
    mtime: i64,
    size: i64,
    source: String,
    content: String,
}

pub fn memory_get(paths: &ClawdPaths, args: &Value) -> Result<Value> {
    let cfg = paths_config(paths)?;
    if !resolve_memory_enabled(&cfg) {
        return Ok(json!({
            "path": args.get("path").and_then(|v| v.as_str()).unwrap_or(""),
            "text": "",
            "disabled": true,
            "error": "memory disabled"
        }));
    }

    let raw_path = args
        .get("path")
        .and_then(|v| v.as_str())
        .context("memory_get requires path")?;

    let abs_path = if Path::new(raw_path).is_absolute() {
        PathBuf::from(raw_path)
    } else {
        paths.workspace_dir.join(raw_path)
    };
    let rel_path = abs_path
        .strip_prefix(&paths.workspace_dir)
        .ok()
        .map(|p| normalize_rel_path(&p.to_string_lossy()))
        .unwrap_or_else(|| abs_path.to_string_lossy().to_string());

    let extra_paths = normalize_extra_paths(&paths.workspace_dir, cfg.memory.as_ref());
    let allowed_workspace = is_memory_rel_path(&rel_path);
    let allowed_extra = is_allowed_extra_path(&abs_path, &extra_paths)?;
    if !allowed_workspace && !allowed_extra {
        anyhow::bail!("path required");
    }
    if !abs_path.to_string_lossy().ends_with(".md") {
        anyhow::bail!("path required");
    }
    let stat = std::fs::symlink_metadata(&abs_path)?;
    if stat.file_type().is_symlink() || !stat.is_file() {
        anyhow::bail!("path required");
    }

    let contents = read_to_string(&abs_path)?;
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
        "path": rel_path,
        "from": from,
        "lines": end_idx.saturating_sub(start_idx),
        "totalLines": total_lines,
        "content": slice,
        "text": slice,
    }))
}

pub fn memory_search(paths: &ClawdPaths, args: &Value) -> Result<Value> {
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .context("memory_search requires query")?
        .trim()
        .to_string();
    if query.is_empty() {
        return Ok(json!({ "results": [] }));
    }

    let cfg = paths_config(paths)?;
    if !resolve_memory_enabled(&cfg) {
        return Ok(json!({ "results": [], "disabled": true, "error": "memory disabled" }));
    }

    let max_results = args
        .get("maxResults")
        .or_else(|| args.get("max_results"))
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(6)
        .clamp(1, 50);
    let min_score = args
        .get("minScore")
        .or_else(|| args.get("min_score"))
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0)
        .clamp(0.0, 1.0);
    let session_key = args
        .get("sessionKey")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    let citations_mode = resolve_citations_mode(&cfg);
    let include_citations = should_include_citations(citations_mode.as_str(), session_key.as_deref());

    let (chunk_tokens, chunk_overlap) = resolve_chunking(&cfg);
    let extra_paths = normalize_extra_paths(&paths.workspace_dir, cfg.memory.as_ref());
    let include_sessions = cfg
        .memory
        .as_ref()
        .and_then(|m| m.session_memory)
        .unwrap_or(false)
        || session_key.is_some();

    let embeddings_cfg = resolve_embeddings_config(&cfg);
    ensure_index(
        paths,
        &embeddings_cfg,
        &extra_paths,
        include_sessions,
        session_key.as_deref(),
        chunk_tokens,
        chunk_overlap,
    )?;
    let conn = open_db(paths)?;

    let sources = resolve_sources(include_sessions);
    let source_filter = if sources.is_empty() {
        String::new()
    } else {
        format!(
            " AND source IN ({})",
            vec!["?"; sources.len()].join(", ")
        )
    };
    let sql = format!(
        "SELECT path, start_line, end_line, text, source, bm25(memory_fts) as score FROM memory_fts WHERE memory_fts MATCH ?{source_filter} ORDER BY score LIMIT ?"
    );
    let mut params_vec: Vec<SqlValue> = Vec::new();
    params_vec.push(SqlValue::from(query.clone()));
    for source in &sources {
        params_vec.push(SqlValue::from(source.clone()));
    }
    params_vec.push(SqlValue::from(max_results as i64));

    let mut rows = Vec::new();
    let mut stmt = conn.prepare(&sql)?;
    let mut results = stmt.query(params_from_iter(params_vec.iter()))?;
    while let Some(row) = results.next()? {
        let path: String = row.get(0)?;
        let start_line: i64 = row.get(1)?;
        let end_line: i64 = row.get(2)?;
        let text: String = row.get(3)?;
        let source: String = row.get(4)?;
        let bm25: f64 = row.get(5)?;
        let fts_score = 1.0 / (1.0 + bm25.abs());
        rows.push(SearchRow {
            path,
            start_line,
            end_line,
            text,
            source,
            fts_score,
            embed_score: None,
            final_score: fts_score,
        });
    }

    if let Some(provider) = build_embedding_provider(&embeddings_cfg)? {
        if !rows.is_empty()
            && should_attempt_embeddings(
                &conn,
                EMBEDDINGS_QUERY_FAILURE_PATH,
                EMBEDDINGS_QUERY_FAILURE_SOURCE,
            )?
        {
            match provider.embed(&[query.clone()]) {
                Ok(query_vec) => {
                    let _ = clear_embedding_failure(
                        &conn,
                        EMBEDDINGS_QUERY_FAILURE_PATH,
                        EMBEDDINGS_QUERY_FAILURE_SOURCE,
                    );
                    if let Some(query_embedding) = query_vec.first() {
                        for row in &mut rows {
                            if let Some(vector) = load_row_embedding(&conn, row)? {
                                let score = cosine_similarity(query_embedding, &vector);
                                row.embed_score = Some(score);
                                row.final_score = 0.6 * row.fts_score + 0.4 * score;
                            }
                        }
                    }
                }
                Err(err) => {
                    // Best-effort: fall back to FTS-only scoring on embeddings failure.
                    let _ = record_embedding_failure(
                        &conn,
                        EMBEDDINGS_QUERY_FAILURE_PATH,
                        EMBEDDINGS_QUERY_FAILURE_SOURCE,
                        &err.to_string(),
                    );
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
        let mut entry = json!({
            "path": row.path,
            "startLine": row.start_line,
            "endLine": row.end_line,
            "lineStart": row.start_line,
            "lineEnd": row.end_line,
            "snippet": row.text,
            "score": row.final_score,
            "ftsScore": row.fts_score,
            "embeddingScore": row.embed_score,
            "source": row.source,
        });
        if include_citations {
            if let (Some(path), Some(start), Some(end)) = (
                entry.get("path").and_then(|v| v.as_str()),
                entry.get("startLine").and_then(|v| v.as_i64()),
                entry.get("endLine").and_then(|v| v.as_i64()),
            ) {
                entry["citation"] = Value::String(format_citation(path, start, end));
            }
        }
        output.push(entry);
    }

    let mut response = json!({
        "results": output,
        "citations": citations_mode,
    });
    if let Some(provider) = embeddings_cfg.provider {
        response["provider"] = Value::String(provider);
    }
    if let Some(model) = embeddings_cfg.model {
        response["model"] = Value::String(model);
    }

    Ok(response)
}

pub fn sync_memory_index(paths: &ClawdPaths, session_key: Option<&str>) -> Result<()> {
    let cfg = paths_config(paths)?;
    if !resolve_memory_enabled(&cfg) {
        return Ok(());
    }

    let (chunk_tokens, chunk_overlap) = resolve_chunking(&cfg);
    let extra_paths = normalize_extra_paths(&paths.workspace_dir, cfg.memory.as_ref());
    let include_sessions = cfg
        .memory
        .as_ref()
        .and_then(|m| m.session_memory)
        .unwrap_or(false)
        || session_key.is_some();
    let embeddings_cfg = resolve_embeddings_config(&cfg);

    ensure_index(
        paths,
        &embeddings_cfg,
        &extra_paths,
        include_sessions,
        session_key,
        chunk_tokens,
        chunk_overlap,
    )?;

    Ok(())
}

fn ensure_index(
    paths: &ClawdPaths,
    embeddings_cfg: &EmbeddingsConfig,
    extra_paths: &[PathBuf],
    include_sessions: bool,
    session_key: Option<&str>,
    chunk_tokens: usize,
    chunk_overlap: usize,
) -> Result<()> {
    let mut conn = open_db(paths)?;
    ensure_schema(&conn)?;

    let provider = build_embedding_provider(embeddings_cfg)?;
    let mut active: HashSet<(String, String)> = HashSet::new();

    for file in list_memory_files(&paths.workspace_dir, extra_paths) {
        if let Some(entry) = build_index_entry(&paths.workspace_dir, &file, "memory")? {
            active.insert((entry.rel_path.clone(), entry.source.clone()));
            index_file(
                &mut conn,
                entry,
                provider.as_ref(),
                chunk_tokens,
                chunk_overlap,
            )?;
        }
    }

    if include_sessions {
        for file in list_session_files(paths, session_key) {
            if let Some(entry) = build_session_entry(&file)? {
                active.insert((entry.rel_path.clone(), entry.source.clone()));
                index_file(
                    &mut conn,
                    entry,
                    provider.as_ref(),
                    chunk_tokens,
                    chunk_overlap,
                )?;
            }
        }
    }

    let mut stale = Vec::new();
    {
        let mut stmt = conn.prepare("SELECT path, source FROM memory_files")?;
        let mut rows = stmt.query([])?;
        while let Some(row) = rows.next()? {
            let path: String = row.get(0)?;
            let source: String = row.get(1)?;
            if !active.contains(&(path.clone(), source.clone())) {
                stale.push((path, source));
            }
        }
    }

    for (path, source) in stale {
        conn.execute(
            "DELETE FROM memory_fts WHERE path = ? AND source = ?",
            params![path, source],
        )?;
        conn.execute(
            "DELETE FROM memory_embeddings WHERE path = ? AND source = ?",
            params![path, source],
        )?;
        conn.execute(
            "DELETE FROM memory_embedding_failures WHERE path = ? AND source = ?",
            params![path, source],
        )?;
        conn.execute(
            "DELETE FROM memory_files WHERE path = ? AND source = ?",
            params![path, source],
        )?;
    }

    Ok(())
}

fn index_file(
    conn: &mut Connection,
    entry: IndexedFile,
    provider: Option<&EmbeddingProvider>,
    chunk_tokens: usize,
    chunk_overlap: usize,
) -> Result<()> {
    let rel_path = entry.rel_path.clone();
    let source = entry.source.clone();
    let existing = conn
        .query_row(
            "SELECT mtime, size FROM memory_files WHERE path = ? AND source = ?",
            params![&rel_path, &source],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;

    if let Some((mtime, size)) = existing {
        if mtime == entry.mtime && size == entry.size {
            if let Some(provider) = provider {
                let embedded: i64 = conn.query_row(
                    "SELECT COUNT(1) FROM memory_embeddings WHERE path = ? AND source = ?",
                    params![&rel_path, &source],
                    |row| row.get(0),
                )?;
                if embedded <= 0 {
                    if should_attempt_embeddings(conn, &rel_path, &source)? {
                        match backfill_embeddings(conn, &rel_path, &source, provider) {
                            Ok(()) => {
                                let _ = clear_embedding_failure(conn, &rel_path, &source);
                            }
                            Err(err) => {
                                let _ =
                                    record_embedding_failure(conn, &rel_path, &source, &err.to_string());
                            }
                        }
                    }
                }
            }
            return Ok(());
        }
    }

    conn.execute(
        "DELETE FROM memory_fts WHERE path = ? AND source = ?",
        params![&rel_path, &source],
    )?;
    conn.execute(
        "DELETE FROM memory_embeddings WHERE path = ? AND source = ?",
        params![&rel_path, &source],
    )?;

    let chunks = chunk_markdown(&entry.content, chunk_tokens, chunk_overlap)
        .into_iter()
        .filter(|chunk| !chunk.text.trim().is_empty())
        .collect::<Vec<_>>();

    let tx = conn.transaction()?;
    for chunk in &chunks {
        tx.execute(
            "INSERT INTO memory_fts(path, start_line, end_line, source, text) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                &rel_path,
                chunk.start_line,
                chunk.end_line,
                &source,
                &chunk.text
            ],
        )?;
    }
    tx.execute(
        "INSERT OR REPLACE INTO memory_files(path, source, mtime, size) VALUES (?1, ?2, ?3, ?4)",
        params![&rel_path, &source, entry.mtime, entry.size],
    )?;
    tx.commit()?;

    if let Some(provider) = provider {
        if should_attempt_embeddings(conn, &rel_path, &source)? {
            match index_embeddings_for_chunks(conn, &rel_path, &source, &chunks, provider) {
                Ok(()) => {
                    let _ = clear_embedding_failure(conn, &rel_path, &source);
                }
                Err(err) => {
                    let _ = record_embedding_failure(conn, &rel_path, &source, &err.to_string());
                }
            }
        }
    }

    Ok(())
}

fn load_row_embedding(conn: &Connection, row: &SearchRow) -> Result<Option<Vec<f32>>> {
    let vector_json: Option<String> = conn
        .query_row(
            "SELECT vector FROM memory_embeddings WHERE path = ? AND start_line = ? AND end_line = ? AND source = ?",
            params![&row.path, row.start_line, row.end_line, &row.source],
            |row| row.get(0),
        )
        .optional()?;
    let Some(vector_json) = vector_json else {
        return Ok(None);
    };
    let parsed = serde_json::from_str::<Vec<f32>>(&vector_json).ok();
    Ok(parsed)
}

fn backfill_embeddings(
    conn: &mut Connection,
    rel_path: &str,
    source: &str,
    provider: &EmbeddingProvider,
) -> Result<()> {
    let chunks: Vec<(i64, i64, String)> = {
        let mut stmt = conn.prepare(
            "SELECT start_line, end_line, text FROM memory_fts WHERE path = ? AND source = ? ORDER BY start_line ASC",
        )?;
        let mut rows = stmt.query(params![rel_path, source])?;
        let mut chunks: Vec<(i64, i64, String)> = Vec::new();
        while let Some(row) = rows.next()? {
            let start_line: i64 = row.get(0)?;
            let end_line: i64 = row.get(1)?;
            let text: String = row.get(2)?;
            if text.trim().is_empty() {
                continue;
            }
            chunks.push((start_line, end_line, text));
        }
        chunks
    };

    for batch in chunks.chunks(provider.batch_size) {
        let inputs = batch
            .iter()
            .map(|chunk| chunk.2.clone())
            .collect::<Vec<_>>();
        let vectors = provider.embed(&inputs)?;
        let tx = conn.transaction()?;
        for (idx, vector) in vectors.into_iter().enumerate() {
            let (start_line, end_line, _) = &batch[idx];
            let vec_json = serde_json::to_string(&vector)?;
            tx.execute(
                "INSERT OR REPLACE INTO memory_embeddings(path, start_line, end_line, source, vector) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![rel_path, start_line, end_line, source, vec_json],
            )?;
        }
        tx.commit()?;
    }
    Ok(())
}

fn index_embeddings_for_chunks(
    conn: &mut Connection,
    rel_path: &str,
    source: &str,
    chunks: &[MemoryChunk],
    provider: &EmbeddingProvider,
) -> Result<()> {
    for batch in chunks.chunks(provider.batch_size) {
        let filtered = batch
            .iter()
            .filter(|chunk| !chunk.text.trim().is_empty())
            .collect::<Vec<_>>();
        if filtered.is_empty() {
            continue;
        }
        let inputs = filtered
            .iter()
            .map(|chunk| chunk.text.clone())
            .collect::<Vec<_>>();
        let vectors = provider.embed(&inputs)?;
        if vectors.len() != filtered.len() {
            anyhow::bail!(
                "embeddings provider returned {} vectors for {} inputs",
                vectors.len(),
                filtered.len()
            );
        }
        let tx = conn.transaction()?;
        for (chunk, vector) in filtered.into_iter().zip(vectors.into_iter()) {
            let vec_json = serde_json::to_string(&vector)?;
            tx.execute(
                "INSERT INTO memory_embeddings(path, start_line, end_line, source, vector) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![rel_path, chunk.start_line, chunk.end_line, source, vec_json],
            )?;
        }
        tx.commit()?;
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
    let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version < SCHEMA_VERSION {
        conn.execute("DROP TABLE IF EXISTS memory_fts", [])?;
        conn.execute("DROP TABLE IF EXISTS memory_embeddings", [])?;
        conn.execute("DROP TABLE IF EXISTS memory_files", [])?;
        conn.execute("DROP TABLE IF EXISTS memory_embedding_failures", [])?;
        conn.execute(&format!("PRAGMA user_version = {}", SCHEMA_VERSION), [])?;
    }
    conn.execute(
        "CREATE VIRTUAL TABLE IF NOT EXISTS memory_fts USING fts5(path, start_line UNINDEXED, end_line UNINDEXED, source UNINDEXED, text, tokenize='unicode61')",
        [],
    )?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS memory_files (path TEXT, source TEXT, mtime INTEGER, size INTEGER, PRIMARY KEY(path, source))",
        [],
    )?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS memory_embeddings (path TEXT, start_line INTEGER, end_line INTEGER, source TEXT, vector TEXT, PRIMARY KEY(path, start_line, end_line, source))",
        [],
    )?;
    conn.execute(
        "CREATE TABLE IF NOT EXISTS memory_embedding_failures (path TEXT, source TEXT, attempts INTEGER, last_attempt_ms INTEGER, next_retry_ms INTEGER, last_error TEXT, PRIMARY KEY(path, source))",
        [],
    )?;
    Ok(())
}

fn should_attempt_embeddings(conn: &Connection, rel_path: &str, source: &str) -> Result<bool> {
    let next_retry_ms: Option<i64> = conn
        .query_row(
            "SELECT next_retry_ms FROM memory_embedding_failures WHERE path = ? AND source = ?",
            params![rel_path, source],
            |row| row.get(0),
        )
        .optional()?;
    let Some(next_retry_ms) = next_retry_ms else {
        return Ok(true);
    };
    Ok(now_ms() >= next_retry_ms)
}

fn clear_embedding_failure(conn: &Connection, rel_path: &str, source: &str) -> Result<()> {
    conn.execute(
        "DELETE FROM memory_embedding_failures WHERE path = ? AND source = ?",
        params![rel_path, source],
    )?;
    Ok(())
}

fn record_embedding_failure(conn: &Connection, rel_path: &str, source: &str, error: &str) -> Result<()> {
    let attempts: Option<i64> = conn
        .query_row(
            "SELECT attempts FROM memory_embedding_failures WHERE path = ? AND source = ?",
            params![rel_path, source],
            |row| row.get(0),
        )
        .optional()?;
    let attempts = attempts.unwrap_or(0).saturating_add(1).max(1);
    let now = now_ms();
    let backoff_ms = embedding_backoff_ms(attempts);
    let next_retry_ms = now.saturating_add(backoff_ms);
    conn.execute(
        r#"
        INSERT INTO memory_embedding_failures(path, source, attempts, last_attempt_ms, next_retry_ms, last_error)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6)
        ON CONFLICT(path, source) DO UPDATE SET
          attempts = excluded.attempts,
          last_attempt_ms = excluded.last_attempt_ms,
          next_retry_ms = excluded.next_retry_ms,
          last_error = excluded.last_error
        "#,
        params![rel_path, source, attempts, now, next_retry_ms, error],
    )?;
    Ok(())
}

fn embedding_backoff_ms(attempts: i64) -> i64 {
    // Exponential backoff (1m, 2m, 4m, ...), capped at 1h.
    let exp = attempts.saturating_sub(1).min(10) as u32;
    let multiplier = 1_i64.checked_shl(exp).unwrap_or(i64::MAX);
    let delay = 60_000_i64.saturating_mul(multiplier);
    delay.min(3_600_000)
}

fn normalize_rel_path(value: &str) -> String {
    let trimmed = value.trim();
    let stripped = trimmed.trim_start_matches(|c| c == '.' || c == '/' || c == '\\');
    stripped.replace('\\', "/")
}

fn normalize_extra_paths(workspace: &Path, cfg: Option<&crate::config::MemoryConfig>) -> Vec<PathBuf> {
    let Some(cfg) = cfg else {
        return Vec::new();
    };
    let Some(raw) = cfg.extra_paths.as_ref() else {
        return Vec::new();
    };
    let mut seen = HashSet::new();
    let mut resolved = Vec::new();
    for value in raw {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        let path = if Path::new(trimmed).is_absolute() {
            PathBuf::from(trimmed)
        } else {
            workspace.join(trimmed)
        };
        let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if seen.insert(key.clone()) {
            resolved.push(path);
        }
    }
    resolved
}

fn is_memory_rel_path(rel_path: &str) -> bool {
    let normalized = normalize_rel_path(rel_path);
    if normalized.is_empty() {
        return false;
    }
    if normalized == "MEMORY.md" || normalized == "memory.md" {
        return true;
    }
    normalized.starts_with("memory/")
}

fn is_allowed_extra_path(abs_path: &Path, extra_paths: &[PathBuf]) -> Result<bool> {
    if extra_paths.is_empty() {
        return Ok(false);
    }
    for extra in extra_paths {
        let meta = match std::fs::symlink_metadata(extra) {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            if abs_path == extra || abs_path.starts_with(extra) {
                return Ok(true);
            }
            continue;
        }
        if meta.is_file() {
            if abs_path == extra && abs_path.to_string_lossy().ends_with(".md") {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn list_memory_files(workspace: &Path, extra_paths: &[PathBuf]) -> Vec<PathBuf> {
    let mut files = Vec::new();
    let mut add_markdown = |path: &Path| {
        if let Ok(meta) = std::fs::symlink_metadata(path) {
            if meta.file_type().is_symlink() || !meta.is_file() {
                return;
            }
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                return;
            }
            files.push(path.to_path_buf());
        }
    };
    add_markdown(&workspace.join("MEMORY.md"));
    add_markdown(&workspace.join("memory.md"));

    let mem_dir = workspace.join("memory");
    if let Ok(meta) = std::fs::symlink_metadata(&mem_dir) {
        if meta.is_dir() && !meta.file_type().is_symlink() {
            for entry in WalkDir::new(&mem_dir)
                .follow_links(false)
                .into_iter()
                .filter_map(Result::ok)
            {
                if entry.file_type().is_symlink() {
                    continue;
                }
                if !entry.file_type().is_file() {
                    continue;
                }
                if entry.path().extension().and_then(|s| s.to_str()) != Some("md") {
                    continue;
                }
                files.push(entry.path().to_path_buf());
            }
        }
    }

    for extra in extra_paths {
        let meta = match std::fs::symlink_metadata(extra) {
            Ok(meta) => meta,
            Err(_) => continue,
        };
        if meta.file_type().is_symlink() {
            continue;
        }
        if meta.is_dir() {
            for entry in WalkDir::new(extra)
                .follow_links(false)
                .into_iter()
                .filter_map(Result::ok)
            {
                if entry.file_type().is_symlink() || !entry.file_type().is_file() {
                    continue;
                }
                if entry.path().extension().and_then(|s| s.to_str()) != Some("md") {
                    continue;
                }
                files.push(entry.path().to_path_buf());
            }
            continue;
        }
        if meta.is_file() && extra.extension().and_then(|s| s.to_str()) == Some("md") {
            files.push(extra.to_path_buf());
        }
    }

    if files.len() <= 1 {
        return files;
    }
    let mut seen = HashSet::new();
    let mut deduped = Vec::new();
    for entry in files {
        let key = std::fs::canonicalize(&entry).unwrap_or_else(|_| entry.clone());
        if seen.insert(key) {
            deduped.push(entry);
        }
    }
    deduped
}

fn build_index_entry(workspace: &Path, abs_path: &Path, source: &str) -> Result<Option<IndexedFile>> {
    let meta = match std::fs::symlink_metadata(abs_path) {
        Ok(meta) => meta,
        Err(_) => return Ok(None),
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Ok(None);
    }
    let mtime = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let size = meta.len() as i64;
    let content = read_to_string(abs_path)?;
    let rel_path = match abs_path.strip_prefix(workspace) {
        Ok(rel) => normalize_rel_path(&rel.to_string_lossy()),
        Err(_) => abs_path.to_string_lossy().to_string(),
    };
    Ok(Some(IndexedFile {
        rel_path,
        mtime,
        size,
        source: source.to_string(),
        content,
    }))
}

fn list_session_files(paths: &ClawdPaths, session_key: Option<&str>) -> Vec<PathBuf> {
    if let Some(key) = session_key {
        let file = crate::sessions::session_transcript_path(paths, key);
        if file.exists() {
            return vec![file];
        }
        return Vec::new();
    }
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&paths.sessions_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) != Some("jsonl") {
                continue;
            }
            files.push(path);
        }
    }
    files
}

fn build_session_entry(abs_path: &Path) -> Result<Option<IndexedFile>> {
    let meta = match std::fs::symlink_metadata(abs_path) {
        Ok(meta) => meta,
        Err(_) => return Ok(None),
    };
    if meta.file_type().is_symlink() || !meta.is_file() {
        return Ok(None);
    }
    let raw = read_to_string(abs_path)?;
    let mut collected = Vec::new();
    for line in raw.lines() {
        if line.trim().is_empty() {
            continue;
        }
        let value: Value = match serde_json::from_str(line) {
            Ok(value) => value,
            Err(_) => continue,
        };
        if value.get("type").and_then(|v| v.as_str()) != Some("message") {
            continue;
        }
        let message = value.get("message").and_then(|v| v.as_object());
        let role = message.and_then(|m| m.get("role")).and_then(|v| v.as_str());
        let role = match role {
            Some("user") => "User",
            Some("assistant") => "Assistant",
            _ => continue,
        };
        let content = message.and_then(|m| m.get("content"));
        let text = content.and_then(extract_session_text);
        let Some(text) = text else { continue };
        collected.push(format!("{role}: {text}"));
    }
    let content = collected.join("\n");
    if content.trim().is_empty() {
        return Ok(None);
    }
    let mtime = meta
        .modified()
        .ok()
        .and_then(|m| m.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0);
    let size = meta.len() as i64;
    let rel_path = session_rel_path(abs_path);
    Ok(Some(IndexedFile {
        rel_path,
        mtime,
        size,
        source: "sessions".to_string(),
        content,
    }))
}

fn session_rel_path(abs_path: &Path) -> String {
    let file_name = abs_path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("session.jsonl");
    format!("sessions/{file_name}")
}

fn normalize_session_text(value: &str) -> String {
    value
        .split_whitespace()
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

fn extract_session_text(value: &Value) -> Option<String> {
    if let Some(text) = value.as_str() {
        let normalized = normalize_session_text(text);
        return if normalized.is_empty() {
            None
        } else {
            Some(normalized)
        };
    }
    let array = value.as_array()?;
    let mut parts = Vec::new();
    for entry in array {
        let obj = entry.as_object()?;
        if obj.get("type").and_then(|v| v.as_str()) != Some("text") {
            continue;
        }
        let text = obj.get("text").and_then(|v| v.as_str()).unwrap_or("");
        let normalized = normalize_session_text(text);
        if !normalized.is_empty() {
            parts.push(normalized);
        }
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(" "))
    }
}

fn resolve_chunking(cfg: &ClawdConfig) -> (usize, usize) {
    let chunk_tokens = cfg
        .memory
        .as_ref()
        .and_then(|m| m.chunk_tokens)
        .unwrap_or(DEFAULT_CHUNK_TOKENS)
        .max(1);
    let overlap = cfg
        .memory
        .as_ref()
        .and_then(|m| m.chunk_overlap)
        .unwrap_or(DEFAULT_CHUNK_OVERLAP);
    (chunk_tokens, overlap.min(chunk_tokens))
}

fn resolve_citations_mode(cfg: &ClawdConfig) -> String {
    match cfg
        .memory
        .as_ref()
        .and_then(|m| m.citations.as_deref())
        .unwrap_or("auto")
        .to_lowercase()
        .as_str()
    {
        "on" => "on".to_string(),
        "off" => "off".to_string(),
        _ => "auto".to_string(),
    }
}

fn should_include_citations(mode: &str, session_key: Option<&str>) -> bool {
    match mode {
        "on" => return true,
        "off" => return false,
        _ => {}
    }
    let raw = session_key.unwrap_or("").trim();
    if raw.is_empty() {
        return true;
    }
    let rest = parse_agent_session_key(raw).unwrap_or_else(|| raw.to_string());
    let tokens = rest.to_lowercase();
    let tokens = tokens.split(':').collect::<Vec<_>>();
    !tokens.iter().any(|t| *t == "channel" || *t == "group")
}

fn parse_agent_session_key(value: &str) -> Option<String> {
    let parts: Vec<&str> = value.split(':').filter(|s| !s.is_empty()).collect();
    if parts.len() < 3 {
        return None;
    }
    if parts[0] != "agent" {
        return None;
    }
    let rest = parts[2..].join(":");
    if rest.is_empty() {
        None
    } else {
        Some(rest)
    }
}

fn resolve_sources(include_sessions: bool) -> Vec<String> {
    let mut sources = vec!["memory".to_string()];
    if include_sessions {
        sources.push("sessions".to_string());
    }
    sources
}

fn format_citation(path: &str, start_line: i64, end_line: i64) -> String {
    if start_line <= 0 || end_line <= 0 {
        return path.to_string();
    }
    if start_line == end_line {
        format!("{path}#L{start_line}")
    } else {
        format!("{path}#L{start_line}-L{end_line}")
    }
}

fn chunk_markdown(content: &str, chunk_tokens: usize, chunk_overlap: usize) -> Vec<MemoryChunk> {
    let lines: Vec<&str> = content.split('\n').collect();
    if lines.is_empty() {
        return Vec::new();
    }
    let max_chars = std::cmp::max(32, chunk_tokens.saturating_mul(4));
    let overlap_chars = chunk_overlap.saturating_mul(4);
    let mut chunks = Vec::new();
    let mut current: Vec<(String, i64)> = Vec::new();
    let mut current_chars = 0usize;

    let flush = |current: &Vec<(String, i64)>, chunks: &mut Vec<MemoryChunk>| {
        if current.is_empty() {
            return;
        }
        let start_line = current.first().map(|c| c.1).unwrap_or(1);
        let end_line = current.last().map(|c| c.1).unwrap_or(start_line);
        let text = current.iter().map(|c| c.0.as_str()).collect::<Vec<_>>().join("\n");
        chunks.push(MemoryChunk {
            start_line,
            end_line,
            text,
        });
    };

    let carry_overlap = |current: &mut Vec<(String, i64)>, current_chars: &mut usize| {
        if overlap_chars == 0 || current.is_empty() {
            current.clear();
            *current_chars = 0;
            return;
        }
        let mut acc = 0usize;
        let mut kept: Vec<(String, i64)> = Vec::new();
        for entry in current.iter().rev() {
            acc += entry.0.chars().count() + 1;
            kept.push(entry.clone());
            if acc >= overlap_chars {
                break;
            }
        }
        kept.reverse();
        *current_chars = kept.iter().map(|e| e.0.chars().count() + 1).sum();
        *current = kept;
    };

    for (idx, line) in lines.iter().enumerate() {
        let line_no = idx as i64 + 1;
        let segments = split_line_segments(line, max_chars);
        for segment in segments {
            let line_size = segment.chars().count() + 1;
            if current_chars + line_size > max_chars && !current.is_empty() {
                flush(&current, &mut chunks);
                carry_overlap(&mut current, &mut current_chars);
            }
            current.push((segment, line_no));
            current_chars += line_size;
        }
    }
    flush(&current, &mut chunks);
    chunks
}

fn split_line_segments(line: &str, max_chars: usize) -> Vec<String> {
    if line.is_empty() {
        return vec![String::new()];
    }
    if max_chars == 0 {
        return vec![line.to_string()];
    }
    let mut segments = Vec::new();
    let mut current = String::new();
    let mut count = 0usize;
    for ch in line.chars() {
        current.push(ch);
        count += 1;
        if count >= max_chars {
            segments.push(current);
            current = String::new();
            count = 0;
        }
    }
    if !current.is_empty() {
        segments.push(current);
    }
    segments
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
        if inputs.is_empty() {
            return Ok(Vec::new());
        }
        let url = if self.api_base.trim_end_matches('/').ends_with("/v1") {
            format!("{}/embeddings", self.api_base.trim_end_matches('/'))
        } else {
            format!("{}/v1/embeddings", self.api_base.trim_end_matches('/'))
        };
        let payload = json!({
            "model": self.model,
            "input": inputs,
        });

        let retry_delay = |attempt: usize| {
            let exp = attempt.saturating_sub(1).min(10) as u32;
            let multiplier = 1_u64.checked_shl(exp).unwrap_or(u64::MAX);
            let delay_ms = 200_u64.saturating_mul(multiplier).min(2_000);
            Duration::from_millis(delay_ms)
        };

        let max_attempts = 3usize;
        let mut last_err: Option<anyhow::Error> = None;
        for attempt in 1..=max_attempts {
            let resp = self
                .client
                .post(&url)
                .bearer_auth(&self.api_key)
                .json(&payload)
                .send()
                .context("embeddings request");
            match resp {
                Ok(resp) => {
                    if resp.status().is_success() {
                        let data: EmbeddingResponse =
                            resp.json().context("parse embeddings response")?;
                        let mut out = data.data;
                        out.sort_by_key(|d| d.index);
                        return Ok(out.into_iter().map(|d| d.embedding).collect());
                    }
                    let status = resp.status();
                    let body = resp.text().unwrap_or_default();
                    let err = anyhow::anyhow!("embeddings request failed ({status}): {body}");
                    last_err = Some(err);
                    let retryable = status.as_u16() == 429 || status.is_server_error();
                    if retryable && attempt < max_attempts {
                        std::thread::sleep(retry_delay(attempt));
                        continue;
                    }
                    break;
                }
                Err(err) => {
                    last_err = Some(err);
                    if attempt < max_attempts {
                        std::thread::sleep(retry_delay(attempt));
                        continue;
                    }
                    break;
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("embeddings request failed")))
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
