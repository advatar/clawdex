use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};

use crate::util::{ensure_dir, home_dir, read_to_string, write_string};

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClawdConfig {
    pub workspace: Option<String>,
    pub workspace_policy: Option<WorkspacePolicyConfig>,
    pub permissions: Option<PermissionsConfig>,
    pub cron: Option<CronConfig>,
    pub heartbeat: Option<HeartbeatConfig>,
    pub memory: Option<MemoryConfig>,
    pub context: Option<ContextConfig>,
    pub codex: Option<CodexConfig>,
    pub gateway: Option<GatewayConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct WorkspacePolicyConfig {
    pub allowed_roots: Option<Vec<String>>,
    pub deny_patterns: Option<Vec<String>>,
    pub read_only: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PermissionsConfig {
    pub internet: Option<bool>,
    pub mcp: Option<McpPermissionsConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct McpPermissionsConfig {
    pub allow: Option<Vec<String>>,
    pub deny: Option<Vec<String>>,
    pub plugins: Option<std::collections::HashMap<String, bool>>,
    #[serde(alias = "serverPolicies")]
    pub server_policies: Option<std::collections::HashMap<String, String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CronConfig {
    pub enabled: Option<bool>,
    pub webhook: Option<String>,
    #[serde(alias = "webhookToken")]
    pub webhook_token: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HeartbeatConfig {
    pub enabled: Option<bool>,
    #[serde(alias = "intervalMs")]
    pub interval_ms: Option<u64>,
    pub prompt: Option<String>,
    #[serde(alias = "ackMaxChars")]
    pub ack_max_chars: Option<usize>,
    #[serde(alias = "activeHours")]
    pub active_hours: Option<HeartbeatActiveHoursConfig>,
    pub delivery: Option<HeartbeatDeliveryConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HeartbeatActiveHoursConfig {
    pub start: Option<String>,
    pub end: Option<String>,
    pub timezone: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct HeartbeatDeliveryConfig {
    pub channel: Option<String>,
    pub to: Option<String>,
    #[serde(alias = "accountId")]
    pub account_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MemoryConfig {
    pub enabled: Option<bool>,
    pub citations: Option<String>,
    #[serde(alias = "writeScope")]
    #[serde(alias = "write_scope")]
    pub write_scope: Option<String>,
    #[serde(alias = "writeRequiresApproval")]
    #[serde(alias = "write_requires_approval")]
    pub write_requires_approval: Option<bool>,
    pub embeddings: Option<EmbeddingsConfig>,
    pub sync: Option<MemorySyncConfig>,
    #[serde(alias = "extraPaths")]
    #[serde(alias = "extra_paths")]
    pub extra_paths: Option<Vec<String>>,
    #[serde(alias = "chunkTokens")]
    #[serde(alias = "chunk_tokens")]
    pub chunk_tokens: Option<usize>,
    #[serde(alias = "chunkOverlap")]
    #[serde(alias = "chunk_overlap")]
    pub chunk_overlap: Option<usize>,
    #[serde(alias = "sessionMemory")]
    #[serde(alias = "session_memory")]
    pub session_memory: Option<bool>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ContextConfig {
    #[serde(alias = "maxInputChars")]
    #[serde(alias = "max_input_chars")]
    pub max_input_chars: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct MemorySyncConfig {
    #[serde(alias = "intervalMinutes")]
    #[serde(alias = "interval_minutes")]
    pub interval_minutes: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EmbeddingsConfig {
    pub enabled: Option<bool>,
    pub provider: Option<String>,
    pub model: Option<String>,
    pub api_base: Option<String>,
    pub api_key_env: Option<String>,
    pub batch_size: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct CodexConfig {
    pub path: Option<String>,
    pub approval_policy: Option<String>,
    pub config_overrides: Option<Vec<String>>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GatewayConfig {
    pub bind: Option<String>,
    #[serde(alias = "wsBind")]
    pub ws_bind: Option<String>,
    pub route_ttl_ms: Option<u64>,
    #[serde(alias = "channelOrder")]
    pub channel_order: Option<Vec<String>>,
    #[serde(alias = "attachmentsMaxBytes")]
    pub attachments_max_bytes: Option<u64>,
    pub url: Option<String>,
    pub token: Option<String>,
    pub password: Option<String>,
    #[serde(alias = "tlsFingerprint")]
    pub tls_fingerprint: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ClawdPaths {
    pub state_dir: PathBuf,
    pub cron_dir: PathBuf,
    pub memory_dir: PathBuf,
    pub sessions_dir: PathBuf,
    pub workspace_dir: PathBuf,
    pub workspace_policy: WorkspacePolicy,
}

#[derive(Debug, Clone)]
pub struct WorkspacePolicy {
    pub allowed_roots: Vec<PathBuf>,
    pub deny_patterns: Vec<String>,
    pub read_only: bool,
    pub network_access: bool,
    deny_set: GlobSet,
}

#[derive(Debug, Clone)]
pub struct McpPolicy {
    allow: std::collections::HashSet<String>,
    deny: std::collections::HashSet<String>,
    plugins: std::collections::HashMap<String, bool>,
    server_policies: std::collections::HashMap<String, String>,
}

impl McpPolicy {
    pub fn is_allowed(&self, name: &str) -> bool {
        let key = normalize_mcp_name(name);
        if self.deny.contains(&key) {
            return false;
        }
        if self.allow.is_empty() {
            return true;
        }
        self.allow.contains(&key)
    }

    pub fn allows_any<'a, I>(&self, names: I) -> bool
    where
        I: IntoIterator<Item = &'a str>,
    {
        let keys = names
            .into_iter()
            .map(normalize_mcp_name)
            .filter(|key| !key.is_empty())
            .collect::<Vec<_>>();

        if keys.iter().any(|key| self.deny.contains(key)) {
            return false;
        }

        if self.allow.is_empty() {
            return true;
        }

        keys.iter().any(|key| self.allow.contains(key))
    }

    pub fn is_plugin_enabled(&self, plugin_id: &str) -> bool {
        let key = normalize_mcp_name(plugin_id);
        self.plugins.get(&key).copied().unwrap_or(true)
    }

    pub fn server_policy(&self, server_name: &str) -> &str {
        let key = normalize_mcp_name(server_name);
        self.server_policies
            .get(&key)
            .map(|v| v.as_str())
            .unwrap_or("allow_always")
    }
}

impl WorkspacePolicy {
    fn new(
        allowed_roots: Vec<PathBuf>,
        deny_patterns: Vec<String>,
        read_only: bool,
        network_access: bool,
    ) -> Result<Self> {
        let deny_set = build_deny_set(&deny_patterns)?;
        Ok(Self {
            allowed_roots,
            deny_patterns,
            read_only,
            network_access,
            deny_set,
        })
    }

    pub fn deny_match(&self, rel_path: &str) -> bool {
        self.deny_set.is_match(rel_path)
    }
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
    let workspace_policy = resolve_workspace_policy(&config, &workspace_dir)?;
    let cron_dir = state_dir.join("cron");
    let memory_dir = state_dir.join("memory");
    let sessions_dir = state_dir.join("sessions");

    ensure_dir(&cron_dir)?;
    ensure_dir(&memory_dir)?;
    ensure_dir(&sessions_dir)?;

    let paths = ClawdPaths {
        state_dir,
        cron_dir,
        memory_dir,
        sessions_dir,
        workspace_dir,
        workspace_policy,
    };

    // Best-effort: install bundled Claude plugins (if present) for first-run UX.
    if let Err(err) = crate::plugins::ensure_default_claude_plugins_installed(&config, &paths) {
        eprintln!("[clawdex][plugins] default install failed: {err}");
    }

    Ok((config, paths))
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
    cfg.memory.as_ref().and_then(|m| m.enabled).unwrap_or(true)
}

pub fn resolve_network_access(cfg: &ClawdConfig) -> bool {
    cfg.permissions
        .as_ref()
        .and_then(|p| p.internet)
        .unwrap_or(true)
}

pub fn resolve_mcp_policy(cfg: &ClawdConfig) -> McpPolicy {
    let mut allow = std::collections::HashSet::new();
    let mut deny = std::collections::HashSet::new();
    let mut plugins = std::collections::HashMap::new();
    let mut server_policies = std::collections::HashMap::new();
    if let Some(mcp) = cfg.permissions.as_ref().and_then(|p| p.mcp.as_ref()) {
        if let Some(list) = mcp.allow.as_ref() {
            for entry in list {
                let key = normalize_mcp_name(entry);
                if !key.is_empty() {
                    allow.insert(key);
                }
            }
        }
        if let Some(list) = mcp.deny.as_ref() {
            for entry in list {
                let key = normalize_mcp_name(entry);
                if !key.is_empty() {
                    deny.insert(key);
                }
            }
        }
        if let Some(map) = mcp.plugins.as_ref() {
            for (key, value) in map {
                let normalized = normalize_mcp_name(key);
                if !normalized.is_empty() {
                    plugins.insert(normalized, *value);
                }
            }
        }
        if let Some(map) = mcp.server_policies.as_ref() {
            for (key, value) in map {
                let normalized = normalize_mcp_name(key);
                let policy = value.trim().to_lowercase().replace('-', "_");
                if normalized.is_empty() || policy.is_empty() {
                    continue;
                }
                server_policies.insert(normalized, policy);
            }
        }
    }
    McpPolicy {
        allow,
        deny,
        plugins,
        server_policies,
    }
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
    let interval = cfg
        .heartbeat
        .as_ref()
        .and_then(|h| h.interval_ms)
        .unwrap_or(30 * 60 * 1000);
    interval.max(30_000)
}

pub fn resolve_context_max_input_chars(cfg: &ClawdConfig) -> Option<usize> {
    cfg.context
        .as_ref()
        .and_then(|c| c.max_input_chars)
        .filter(|value| *value > 0)
}

pub fn resolve_citations_mode(cfg: &ClawdConfig) -> String {
    cfg.memory
        .as_ref()
        .and_then(|m| m.citations.clone())
        .unwrap_or_else(|| "auto".to_string())
}

pub fn resolve_embeddings_config(cfg: &ClawdConfig) -> EmbeddingsConfig {
    let mut resolved = cfg
        .memory
        .as_ref()
        .and_then(|m| m.embeddings.clone())
        .unwrap_or_default();

    let memory_enabled = cfg.memory.as_ref().and_then(|m| m.enabled).unwrap_or(true);
    if !memory_enabled {
        return resolved;
    }

    let overrides = collect_codex_overrides(cfg);
    let override_map = parse_codex_overrides(&overrides);
    let codex_provider = resolve_codex_provider(&override_map);

    if resolved.provider.is_none() {
        let provider = codex_provider.unwrap_or_else(|| "openai".to_string());
        resolved.provider = Some(provider);
    }

    if resolved.model.is_none() {
        if let Some(provider) = resolved.provider.as_deref() {
            if let Some(default_model) = default_embedding_model_for_provider(provider) {
                resolved.model = Some(default_model);
            }
        }
    }

    if resolved.enabled.is_none() {
        resolved.enabled = Some(true);
    }

    resolved
}

fn collect_codex_overrides(cfg: &ClawdConfig) -> Vec<String> {
    let mut overrides = cfg
        .codex
        .as_ref()
        .and_then(|c| c.config_overrides.clone())
        .unwrap_or_default();
    if let Ok(raw) = std::env::var("CLAWDEX_CODEX_CONFIG") {
        for entry in raw.split(';') {
            let trimmed = entry.trim();
            if !trimmed.is_empty() {
                overrides.push(trimmed.to_string());
            }
        }
    }
    overrides
}

fn parse_codex_overrides(overrides: &[String]) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for entry in overrides {
        let Some((key, value)) = entry.split_once('=') else {
            continue;
        };
        let key = normalize_override_key(key);
        let value = value.trim().to_string();
        if !key.is_empty() && !value.is_empty() {
            map.insert(key, value);
        }
    }
    map
}

fn normalize_override_key(raw: &str) -> String {
    raw.trim().to_lowercase().replace('-', "_").replace(' ', "")
}

fn resolve_codex_provider(overrides: &std::collections::HashMap<String, String>) -> Option<String> {
    if let Some(provider) =
        get_override_value(overrides, &["model_provider", "modelprovider", "provider"])
    {
        let trimmed = provider.trim().to_lowercase();
        if !trimmed.is_empty() {
            return Some(trimmed);
        }
    }

    if let Some(model) = get_override_value(overrides, &["model", "models.default", "model.name"]) {
        if let Some(provider) = infer_provider_from_model(&model) {
            return Some(provider);
        }
    }

    None
}

fn get_override_value(
    overrides: &std::collections::HashMap<String, String>,
    keys: &[&str],
) -> Option<String> {
    for key in keys {
        let normalized = normalize_override_key(key);
        if let Some(value) = overrides.get(&normalized) {
            if !value.trim().is_empty() {
                return Some(value.trim().to_string());
            }
        }
    }
    None
}

fn infer_provider_from_model(model: &str) -> Option<String> {
    let trimmed = model.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Some((prefix, _)) = trimmed.split_once('/') {
        let provider = prefix.trim().to_lowercase();
        if !provider.is_empty() {
            return Some(provider);
        }
    }
    if let Some((prefix, _)) = trimmed.split_once(':') {
        let provider = prefix.trim().to_lowercase();
        if !provider.is_empty() {
            return Some(provider);
        }
    }
    let lower = trimmed.to_lowercase();
    if lower.starts_with("gpt-")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
        || lower.starts_with("text-embedding")
        || lower.starts_with("codex")
    {
        return Some("openai".to_string());
    }
    None
}

fn default_embedding_model_for_provider(provider: &str) -> Option<String> {
    let provider = provider.trim().to_lowercase();
    if provider.is_empty() {
        return None;
    }
    if provider == "openai" || provider == "codex" || provider == "openai-compatible" {
        return Some("text-embedding-3-small".to_string());
    }
    if provider.starts_with("http://") || provider.starts_with("https://") {
        return Some("text-embedding-3-small".to_string());
    }
    None
}

pub fn resolve_workspace_path(paths: &ClawdPaths, rel: &str) -> Result<PathBuf> {
    let candidate = Path::new(rel);
    let abs = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        paths.workspace_dir.join(candidate)
    };
    let abs = abs.canonicalize().unwrap_or_else(|_| abs.clone());
    let root = select_allowed_root(&abs, &paths.workspace_policy.allowed_roots)
        .context("path outside allowed workspace roots")?;
    let rel = abs
        .strip_prefix(root)
        .unwrap_or(&abs)
        .to_string_lossy()
        .replace('\\', "/");
    if paths.workspace_policy.deny_match(&rel) {
        anyhow::bail!("path denied by workspace policy");
    }
    Ok(abs)
}

fn resolve_workspace_policy(cfg: &ClawdConfig, workspace_dir: &Path) -> Result<WorkspacePolicy> {
    let mut allowed_roots = Vec::new();
    if let Some(policy) = cfg.workspace_policy.as_ref() {
        if let Some(roots) = policy.allowed_roots.as_ref() {
            for root in roots {
                let path = PathBuf::from(root);
                let abs = if path.is_absolute() {
                    path
                } else {
                    workspace_dir.join(path)
                };
                allowed_roots.push(normalize_root(&abs));
            }
        }
    }
    if allowed_roots.is_empty() {
        allowed_roots.push(normalize_root(workspace_dir));
    }
    allowed_roots.sort_by(|a, b| a.as_os_str().cmp(b.as_os_str()));
    allowed_roots.dedup();

    let deny_patterns = cfg
        .workspace_policy
        .as_ref()
        .and_then(|p| p.deny_patterns.clone())
        .unwrap_or_else(default_deny_patterns);
    let read_only = cfg
        .workspace_policy
        .as_ref()
        .and_then(|p| p.read_only)
        .unwrap_or(false);
    let network_access = resolve_network_access(cfg);

    WorkspacePolicy::new(allowed_roots, deny_patterns, read_only, network_access)
}

fn normalize_root(path: &Path) -> PathBuf {
    path.canonicalize().unwrap_or_else(|_| path.to_path_buf())
}

fn select_allowed_root<'a>(path: &Path, roots: &'a [PathBuf]) -> Option<&'a PathBuf> {
    let mut best: Option<&PathBuf> = None;
    let mut best_len = 0usize;
    for root in roots {
        if path.starts_with(root) {
            let len = root.components().count();
            if len >= best_len {
                best_len = len;
                best = Some(root);
            }
        }
    }
    best
}

fn default_deny_patterns() -> Vec<String> {
    vec![
        "**/.git/**".to_string(),
        "**/.env".to_string(),
        "**/.DS_Store".to_string(),
    ]
}

fn build_deny_set(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).with_context(|| format!("invalid deny pattern {pattern}"))?;
        builder.add(glob);
    }
    Ok(builder.build().context("compile deny patterns")?)
}

pub fn read_config_value(state_dir: &Path) -> Result<serde_json::Value> {
    let json5_path = state_dir.join("config.json5");
    if json5_path.exists() {
        let raw = read_to_string(&json5_path)?;
        let value: serde_json::Value = json5::from_str(&raw).context("parse config.json5")?;
        return Ok(value);
    }
    let json_path = state_dir.join("config.json");
    if json_path.exists() {
        let raw = read_to_string(&json_path)?;
        let value: serde_json::Value = serde_json::from_str(&raw).context("parse config.json")?;
        return Ok(value);
    }
    Ok(serde_json::json!({}))
}

pub fn write_config_value(state_dir: &Path, value: &serde_json::Value) -> Result<PathBuf> {
    let path = state_dir.join("config.json5");
    let data = serde_json::to_string_pretty(value).context("serialize config")?;
    write_string(&path, &format!("{data}\n"))?;
    Ok(path)
}

pub fn merge_config_value(base: &mut serde_json::Value, patch: &serde_json::Value) {
    match (base, patch) {
        (serde_json::Value::Object(base_map), serde_json::Value::Object(patch_map)) => {
            for (key, value) in patch_map {
                merge_config_value(
                    base_map
                        .entry(key.clone())
                        .or_insert(serde_json::Value::Null),
                    value,
                );
            }
        }
        (base_slot, patch_value) => {
            *base_slot = patch_value.clone();
        }
    }
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

fn normalize_mcp_name(name: &str) -> String {
    name.trim().to_lowercase()
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
