use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};

use crate::util::{ensure_dir, home_dir, read_to_string};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClawdConfig {
    pub workspace: Option<String>,
    pub cron: Option<CronConfig>,
    pub heartbeat: Option<HeartbeatConfig>,
    pub memory: Option<MemoryConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CronConfig {
    pub enabled: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HeartbeatConfig {
    pub enabled: Option<bool>,
    pub interval_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MemoryConfig {
    pub enabled: Option<bool>,
    pub citations: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ClawdPaths {
    pub state_dir: PathBuf,
    pub cron_dir: PathBuf,
    pub memory_dir: PathBuf,
    pub sessions_dir: PathBuf,
    pub workspace_dir: PathBuf,
}

pub fn load_config(
    state_dir_override: Option<PathBuf>,
    workspace_override: Option<PathBuf>,
) -> Result<(ClawdConfig, ClawdPaths)> {
    let state_dir = state_dir_override
        .or_else(state_dir_from_env)
        .unwrap_or(default_state_dir()?);
    ensure_dir(&state_dir)?;

    let config = if let Some(config_path) = config_path_from_env() {
        if config_path.exists() {
            let raw = read_to_string(&config_path)?;
            if config_path.extension().and_then(|s| s.to_str()) == Some("json5") {
                json5::from_str::<ClawdConfig>(&raw).context("parse config.json5")?
            } else {
                serde_json::from_str::<ClawdConfig>(&raw).context("parse config.json")?
            }
        } else {
            ClawdConfig::default()
        }
    } else {
        let config_path = state_dir.join("config.json5");
        if config_path.exists() {
            let raw = read_to_string(&config_path)?;
            json5::from_str::<ClawdConfig>(&raw).context("parse config.json5")?
        } else {
            let json_path = state_dir.join("config.json");
            if json_path.exists() {
                let raw = read_to_string(&json_path)?;
                serde_json::from_str::<ClawdConfig>(&raw).context("parse config.json")?
            } else {
                ClawdConfig::default()
            }
        }
    };

    let workspace_dir = resolve_workspace_dir(workspace_override, &config)?;
    let cron_dir = state_dir.join("cron");
    let memory_dir = state_dir.join("memory");
    let sessions_dir = state_dir.join("sessions");

    ensure_dir(&cron_dir)?;
    ensure_dir(&memory_dir)?;
    ensure_dir(&sessions_dir)?;

    Ok((
        config,
        ClawdPaths {
            state_dir,
            cron_dir,
            memory_dir,
            sessions_dir,
            workspace_dir,
        },
    ))
}

pub fn resolve_workspace_dir(
    workspace_override: Option<PathBuf>,
    config: &ClawdConfig,
) -> Result<PathBuf> {
    if let Some(path) = workspace_override {
        return Ok(path);
    }
    if let Ok(env) = std::env::var("CLAWDEX_WORKSPACE") {
        if !env.trim().is_empty() {
            return Ok(PathBuf::from(env));
        }
    }
    if let Ok(env) = std::env::var("CODEX_CLAWD_WORKSPACE_DIR") {
        if !env.trim().is_empty() {
            return Ok(PathBuf::from(env));
        }
    }
    if let Ok(env) = std::env::var("CODEX_WORKSPACE_DIR") {
        if !env.trim().is_empty() {
            return Ok(PathBuf::from(env));
        }
    }
    if let Some(path) = config.workspace.as_ref() {
        if !path.trim().is_empty() {
            return Ok(PathBuf::from(path));
        }
    }
    std::env::current_dir().context("resolve current dir")
}

pub fn default_state_dir() -> Result<PathBuf> {
    let home = home_dir()?;
    Ok(home.join(".codex").join("clawdex"))
}

pub fn resolve_memory_enabled(cfg: &ClawdConfig) -> bool {
    cfg.memory
        .as_ref()
        .and_then(|m| m.enabled)
        .unwrap_or(true)
}

pub fn resolve_cron_enabled(cfg: &ClawdConfig) -> bool {
    cfg.cron.as_ref().and_then(|c| c.enabled).unwrap_or(true)
}

pub fn resolve_heartbeat_enabled(cfg: &ClawdConfig) -> bool {
    cfg.heartbeat
        .as_ref()
        .and_then(|h| h.enabled)
        .unwrap_or(true)
}

pub fn resolve_heartbeat_interval_ms(cfg: &ClawdConfig) -> u64 {
    cfg.heartbeat
        .as_ref()
        .and_then(|h| h.interval_ms)
        .unwrap_or(30 * 60 * 1000)
}

pub fn resolve_citations_mode(cfg: &ClawdConfig) -> String {
    cfg.memory
        .as_ref()
        .and_then(|m| m.citations.clone())
        .unwrap_or_else(|| "auto".to_string())
}

pub fn resolve_workspace_path(paths: &ClawdPaths, rel: &str) -> Result<PathBuf> {
    let candidate = Path::new(rel);
    let abs = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        paths.workspace_dir.join(candidate)
    };
    let abs = abs
        .canonicalize()
        .unwrap_or_else(|_| abs.clone());
    let root = paths
        .workspace_dir
        .canonicalize()
        .unwrap_or_else(|_| paths.workspace_dir.clone());
    if !abs.starts_with(&root) {
        anyhow::bail!("path outside workspace");
    }
    Ok(abs)
}

fn state_dir_from_env() -> Option<PathBuf> {
    if let Ok(env) = std::env::var("CLAWDEX_STATE_DIR") {
        if !env.trim().is_empty() {
            return Some(PathBuf::from(env));
        }
    }
    if let Ok(env) = std::env::var("CODEX_CLAWD_STATE_DIR") {
        if !env.trim().is_empty() {
            return Some(PathBuf::from(env));
        }
    }
    None
}

fn config_path_from_env() -> Option<PathBuf> {
    if let Ok(env) = std::env::var("CLAWDEX_CONFIG_PATH") {
        if !env.trim().is_empty() {
            return Some(PathBuf::from(env));
        }
    }
    if let Ok(env) = std::env::var("CODEX_CLAWD_CONFIG_PATH") {
        if !env.trim().is_empty() {
            return Some(PathBuf::from(env));
        }
    }
    None
}
