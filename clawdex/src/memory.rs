use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde_json::{json, Value};
use walkdir::WalkDir;

use crate::config::{resolve_workspace_path, ClawdPaths};
use crate::util::read_to_string;

fn allowed_memory_path(relative: &Path) -> bool {
    let rel = relative.to_string_lossy();
    if rel == "MEMORY.md" || rel == "memory/MEMORY.md" {
        return true;
    }
    rel.starts_with("memory/") && rel.ends_with(".md")
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

pub fn memory_get(paths: &ClawdPaths, args: &Value) -> Result<Value> {
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
    let query_lower = query.to_lowercase();
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

    let mut results = Vec::new();
    for file in memory_files(&paths.workspace_dir) {
        let contents = read_to_string(&file).unwrap_or_default();
        for (idx, line) in contents.lines().enumerate() {
            let lower = line.to_lowercase();
            if !lower.contains(&query_lower) {
                continue;
            }
            let score = 1.0 + (lower.matches(&query_lower).count() as f64);
            if score < min_score {
                continue;
            }
            let rel = file
                .strip_prefix(&paths.workspace_dir)
                .unwrap_or(&file)
                .to_string_lossy()
                .to_string();
            results.push(json!({
                "path": rel,
                "lineStart": idx + 1,
                "lineEnd": idx + 1,
                "snippet": line,
                "score": score,
            }));
            if results.len() >= max_results {
                break;
            }
        }
        if results.len() >= max_results {
            break;
        }
    }

    Ok(json!({
        "results": results,
        "query": query,
    }))
}
