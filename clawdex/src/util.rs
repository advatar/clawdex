use std::fs;
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};

#[derive(Debug, Clone)]
pub struct TextBudgetResult {
    pub text: String,
    pub max_chars: Option<usize>,
    pub original_chars: usize,
    pub final_chars: usize,
    pub truncated: bool,
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

pub fn home_dir() -> Result<PathBuf> {
    dirs::home_dir().context("home directory not found")
}

pub fn ensure_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("create dir {}", path.display()))
}

pub fn read_to_string(path: &Path) -> Result<String> {
    fs::read_to_string(path).with_context(|| format!("read {}", path.display()))
}

pub fn write_string(path: &Path, contents: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create dir {}", parent.display()))?;
    }
    let mut file = fs::File::create(path).with_context(|| format!("write {}", path.display()))?;
    file.write_all(contents.as_bytes())?;
    Ok(())
}

pub fn read_json_value(path: &Path) -> Result<Option<serde_json::Value>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = read_to_string(path)?;
    let value = serde_json::from_str(&raw).context("parse json")?;
    Ok(Some(value))
}

pub fn write_json_value(path: &Path, value: &serde_json::Value) -> Result<()> {
    let data = serde_json::to_string_pretty(value).context("serialize json")?;
    write_string(path, &format!("{}\n", data))
}

pub fn append_json_line(path: &Path, value: &serde_json::Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| format!("create dir {}", parent.display()))?;
    }
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .with_context(|| format!("append {}", path.display()))?;
    let data = serde_json::to_string(value).context("serialize json")?;
    writeln!(file, "{}", data)?;
    Ok(())
}

pub fn read_json_lines(path: &Path, limit: Option<usize>) -> Result<Vec<serde_json::Value>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    let file = fs::File::open(path).with_context(|| format!("open {}", path.display()))?;
    let reader = io::BufReader::new(file);
    let mut out = Vec::new();
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Ok(val) = serde_json::from_str::<serde_json::Value>(&line) {
            out.push(val);
        }
    }
    if let Some(limit) = limit {
        if out.len() > limit {
            out = out.split_off(out.len() - limit);
        }
    }
    Ok(out)
}

pub fn apply_text_budget(text: &str, max_chars: Option<usize>) -> TextBudgetResult {
    let original_chars = text.chars().count();
    let limit = max_chars.and_then(|value| (value > 0).then_some(value));
    let Some(limit) = limit else {
        return TextBudgetResult {
            text: text.to_string(),
            max_chars: None,
            original_chars,
            final_chars: original_chars,
            truncated: false,
        };
    };

    if original_chars <= limit {
        return TextBudgetResult {
            text: text.to_string(),
            max_chars: Some(limit),
            original_chars,
            final_chars: original_chars,
            truncated: false,
        };
    }

    let mut clipped = String::new();
    for (idx, ch) in text.chars().enumerate() {
        if idx >= limit {
            break;
        }
        clipped.push(ch);
    }

    TextBudgetResult {
        text: clipped,
        max_chars: Some(limit),
        original_chars,
        final_chars: limit,
        truncated: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_budget_keeps_input_when_under_limit() {
        let result = apply_text_budget("hello", Some(10));
        assert!(!result.truncated);
        assert_eq!(result.text, "hello");
        assert_eq!(result.original_chars, 5);
        assert_eq!(result.final_chars, 5);
    }

    #[test]
    fn text_budget_truncates_to_char_boundary() {
        let result = apply_text_budget("abc😀def", Some(4));
        assert!(result.truncated);
        assert_eq!(result.text, "abc😀");
        assert_eq!(result.original_chars, 7);
        assert_eq!(result.final_chars, 4);
    }
}
