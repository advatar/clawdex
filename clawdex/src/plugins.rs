use std::collections::{HashMap, HashSet};
use std::fs::{self, File};
use std::io;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};
use flate2::read::GzDecoder;
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use serde_yaml::{Mapping, Value as YamlValue};
use tar::Archive;
use uuid::Uuid;
use walkdir::WalkDir;
use zip::ZipArchive;

use crate::app_server::{ApprovalMode, CodexClient};
use crate::config::{load_config, resolve_mcp_policy, ClawdConfig, ClawdPaths, McpPolicy, WorkspacePolicy};
use crate::runner::workspace_sandbox_policy;
use crate::task_db::{PluginRecord, TaskStore};
use crate::util::{ensure_dir, home_dir, now_ms, read_to_string, write_json_value};

const COWORK_MANIFEST_PATH: &str = ".claude-plugin/plugin.json";
const OPENCLAW_MANIFEST_FILENAME: &str = "openclaw.plugin.json";
const INSTALLS_FILE: &str = "installs.json";
const BUNDLED_CLAUDE_PLUGINS_DIR_ENV: &str = "CLAWDEX_BUNDLED_CLAUDE_PLUGINS_DIR";
const DISABLE_BUNDLED_CLAUDE_PLUGINS_ENV: &str = "CLAWDEX_DISABLE_DEFAULT_CLAUDE_PLUGINS";
const BUNDLED_CLAUDE_PLUGINS_SOURCE_LABEL: &str = "bundled-claude";
const CLAWDEX_PLUGIN_ID_FRONTMATTER_KEY: &str = "clawdex-plugin-id";

pub fn ensure_default_claude_plugins_installed(cfg: &ClawdConfig, paths: &ClawdPaths) -> Result<()> {
    if std::env::var(DISABLE_BUNDLED_CLAUDE_PLUGINS_ENV)
        .ok()
        .is_some_and(|value| value.trim() == "1")
    {
        return Ok(());
    }

    let Some(root) = discover_bundled_claude_plugins_dir() else {
        return Ok(());
    };

    let plugin_roots = list_bundled_claude_plugins(&root)?;
    if plugin_roots.is_empty() {
        return Ok(());
    }

    let store = TaskStore::open(paths)?;
    let policy = resolve_mcp_policy(cfg);

    for plugin_root in plugin_roots {
        if let Err(err) = ensure_bundled_claude_plugin(paths, &store, &policy, &plugin_root) {
            eprintln!(
                "[clawdex][plugins] bundled Claude plugin install failed for {}: {err}",
                plugin_root.display()
            );
        }
    }

    Ok(())
}

fn discover_bundled_claude_plugins_dir() -> Option<PathBuf> {
    if let Ok(env) = std::env::var(BUNDLED_CLAUDE_PLUGINS_DIR_ENV) {
        let trimmed = env.trim();
        if !trimmed.is_empty() {
            let path = PathBuf::from(trimmed);
            if path.exists() {
                return Some(path);
            }
        }
    }

    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            // App embed layout: .../Contents/Resources/bin/clawdex
            if let Some(resources) = parent.parent() {
                let candidate = resources.join("claude-plugins");
                if candidate.exists() {
                    return Some(candidate);
                }
            }
            // CLI dist layout: .../bin/clawdex
            let candidate = parent.join("claude-plugins");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }

    if let Ok(cwd) = std::env::current_dir() {
        let candidate = cwd.join("plugins");
        if candidate.exists() {
            return Some(candidate);
        }
        if let Some(parent) = cwd.parent() {
            let candidate = parent.join("plugins");
            if candidate.exists() {
                return Some(candidate);
            }
        }
    }

    None
}

fn list_bundled_claude_plugins(root: &Path) -> Result<Vec<PathBuf>> {
    let mut plugins = Vec::new();
    for entry in fs::read_dir(root).with_context(|| format!("read {}", root.display()))? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path
            .file_name()
            .and_then(|s| s.to_str())
            .is_some_and(|name| name.starts_with('.'))
        {
            continue;
        }
        // Support both "formal" Claude plugins (with `.claude-plugin/plugin.json`) and
        // lightweight bundles that only ship `commands/` and/or `skills/` (e.g. command packs).
        if load_plugin_manifest(&path).is_ok() {
            plugins.push(path);
        }
    }
    plugins.sort_by(|a, b| a.to_string_lossy().to_lowercase().cmp(&b.to_string_lossy().to_lowercase()));
    Ok(plugins)
}

fn ensure_bundled_claude_plugin(
    paths: &ClawdPaths,
    store: &TaskStore,
    policy: &McpPolicy,
    plugin_root: &Path,
) -> Result<()> {
    let manifest = load_plugin_manifest(plugin_root)?;
    if store.get_plugin(&manifest.id)?.is_some() {
        return Ok(());
    }

    let source = PluginInstallSource::Path(plugin_root.to_path_buf());
    let _ = install_plugin(
        paths,
        store,
        source,
        false,
        Some(BUNDLED_CLAUDE_PLUGINS_SOURCE_LABEL.to_string()),
        policy,
        PluginInstallMode::Install,
        Some(&manifest.id),
    )?;
    Ok(())
}

#[derive(Debug, Clone, Copy)]
enum PluginManifestKind {
    Cowork,
    OpenClaw,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PluginPermissions {
    #[serde(default, alias = "mcp_allow", alias = "mcpAllow")]
    mcp_allow: Option<Vec<String>>,
    #[serde(default, alias = "mcp_deny", alias = "mcpDeny")]
    mcp_deny: Option<Vec<String>>,
    #[serde(default, alias = "allowed_roots", alias = "allowedRoots")]
    allowed_roots: Option<Vec<String>>,
    #[serde(default, alias = "read_only", alias = "readOnly")]
    read_only: Option<bool>,
    #[serde(default, alias = "network_access", alias = "networkAccess")]
    network_access: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct ClaudePluginManifest {
    id: Option<String>,
    name: Option<String>,
    version: Option<String>,
    description: Option<String>,
    permissions: Option<PluginPermissions>,
    skills: Option<ComponentPathSpec>,
    commands: Option<ComponentPathSpec>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
enum ComponentPathSpec {
    Single(String),
    Many(Vec<String>),
    Object(ComponentPathObject),
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct ComponentPathObject {
    path: Option<String>,
    paths: Option<Vec<String>>,
}

#[derive(Debug, Deserialize)]
struct OpenClawManifest {
    id: String,
    #[serde(rename = "configSchema")]
    config_schema: Value,
    name: Option<String>,
    description: Option<String>,
    version: Option<String>,
    permissions: Option<PluginPermissions>,
}

#[derive(Debug, Deserialize)]
struct PackageManifest {
    name: Option<String>,
    version: Option<String>,
    description: Option<String>,
    dependencies: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone)]
struct PluginManifestInfo {
    id: String,
    name: String,
    version: Option<String>,
    description: Option<String>,
    kind: PluginManifestKind,
    manifest_path: PathBuf,
    config_schema: Option<Value>,
    permissions: PluginPermissions,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PluginInstallRecord {
    source: String,
    spec: Option<String>,
    #[serde(alias = "source_path")]
    source_path: Option<String>,
    #[serde(alias = "install_path")]
    install_path: Option<String>,
    version: Option<String>,
    #[serde(alias = "installedAt", alias = "installed_at_ms")]
    installed_at_ms: i64,
    #[serde(alias = "updatedAt", alias = "updated_at_ms")]
    updated_at_ms: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PluginInstallMode {
    Install,
    Update,
}

#[derive(Debug, Clone)]
enum PluginInstallSource {
    Path(PathBuf),
    Npm { spec: String },
}

#[derive(Debug, Default, Serialize)]
struct PluginAssets {
    skills: usize,
    commands: usize,
    has_mcp: bool,
}

#[derive(Debug, Default)]
struct PluginComponentPaths {
    skills: Vec<PathBuf>,
    commands: Vec<PathBuf>,
}

#[derive(Debug, Clone)]
enum SkillSource {
    Directory { root: PathBuf, name: String },
    LegacyFile { path: PathBuf, name: String },
}

#[derive(Debug, Clone)]
struct CommandTemplate {
    name: String,
    description: Option<String>,
    template: String,
}

#[derive(Debug, Clone, Deserialize)]
struct CommandSpec {
    name: Option<String>,
    description: Option<String>,
    prompt: Option<String>,
    system: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
struct CommandEntry {
    plugin_id: String,
    plugin_name: String,
    command: String,
    description: Option<String>,
    source: String,
}

fn manifest_kind_label(kind: PluginManifestKind) -> &'static str {
    match kind {
        PluginManifestKind::Cowork => "cowork",
        PluginManifestKind::OpenClaw => "openclaw",
    }
}

fn installs_path(paths: &ClawdPaths) -> PathBuf {
    paths.state_dir.join("plugins").join(INSTALLS_FILE)
}

fn load_install_records(paths: &ClawdPaths) -> Result<HashMap<String, PluginInstallRecord>> {
    let path = installs_path(paths);
    if !path.exists() {
        return Ok(HashMap::new());
    }
    let raw = read_to_string(&path)?;
    let map: HashMap<String, PluginInstallRecord> =
        serde_json::from_str(&raw).unwrap_or_default();
    Ok(map)
}

fn save_install_records(paths: &ClawdPaths, records: &HashMap<String, PluginInstallRecord>) -> Result<()> {
    let path = installs_path(paths);
    if let Some(parent) = path.parent() {
        ensure_dir(parent)?;
    }
    write_json_value(&path, &serde_json::to_value(records).unwrap_or(Value::Object(Map::new())))
}

fn read_package_manifest(root: &Path) -> Option<PackageManifest> {
    let path = root.join("package.json");
    if !path.exists() {
        return None;
    }
    let raw = read_to_string(&path).ok()?;
    serde_json::from_str(&raw).ok()
}

fn normalize_plugin_id(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        anyhow::bail!("plugin id missing");
    }
    if trimmed == "." || trimmed == ".." {
        anyhow::bail!("invalid plugin id");
    }
    if trimmed.contains('/') || trimmed.contains('\\') {
        anyhow::bail!("invalid plugin id: path separators not allowed");
    }
    let normalized = trimmed.to_ascii_lowercase();
    let safe = normalized
        .chars()
        .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '_');
    if safe {
        return Ok(normalized);
    }
    Ok(slugify(trimmed))
}

fn resolve_plugin_name(id: &str, name: Option<&str>, package: Option<&PackageManifest>) -> String {
    if let Some(name) = name.and_then(|s| {
        let trimmed = s.trim();
        if trimmed.is_empty() { None } else { Some(trimmed) }
    }) {
        return name.to_string();
    }
    if let Some(pkg) = package {
        if let Some(pkg_name) = pkg.name.as_ref() {
            let trimmed = pkg_name.trim();
            if !trimmed.is_empty() {
                return trimmed.to_string();
            }
        }
    }
    id.to_string()
}

fn resolve_plugin_description(
    description: Option<&str>,
    package: Option<&PackageManifest>,
) -> Option<String> {
    if let Some(desc) = description {
        let trimmed = desc.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_string());
        }
    }
    package
        .and_then(|pkg| pkg.description.as_ref())
        .and_then(|desc| {
            let trimmed = desc.trim();
            if trimmed.is_empty() { None } else { Some(trimmed.to_string()) }
        })
}

fn load_plugin_manifest(root: &Path) -> Result<PluginManifestInfo> {
    let openclaw_path = root.join(OPENCLAW_MANIFEST_FILENAME);
    let cowork_path = root.join(COWORK_MANIFEST_PATH);
    let package = read_package_manifest(root);

    if openclaw_path.exists() {
        let raw = read_to_string(&openclaw_path)?;
        let manifest: OpenClawManifest =
            serde_json::from_str(&raw).context("parse openclaw.plugin.json")?;
        if !manifest.config_schema.is_object() {
            anyhow::bail!("openclaw.plugin.json configSchema must be an object");
        }
        let id = normalize_plugin_id(&manifest.id)?;
        let name = resolve_plugin_name(&id, manifest.name.as_deref(), package.as_ref());
        let description = resolve_plugin_description(manifest.description.as_deref(), package.as_ref());
        let version = manifest
            .version
            .clone()
            .or_else(|| package.as_ref().and_then(|pkg| pkg.version.clone()));
        return Ok(PluginManifestInfo {
            id,
            name,
            version,
            description,
            kind: PluginManifestKind::OpenClaw,
            manifest_path: openclaw_path,
            config_schema: Some(manifest.config_schema),
            permissions: manifest.permissions.unwrap_or_default(),
        });
    }

    if cowork_path.exists() {
        let raw = read_to_string(&cowork_path)?;
        let manifest: ClaudePluginManifest =
            serde_json::from_str(&raw).context("parse plugin.json")?;
        let raw_id = manifest
            .id
            .clone()
            .or_else(|| manifest.name.clone())
            .or_else(|| root.file_name().and_then(|s| s.to_str()).map(|s| s.to_string()))
            .context("plugin id not found")?;
        let id = normalize_plugin_id(&raw_id)?;
        let name = resolve_plugin_name(&id, manifest.name.as_deref(), package.as_ref());
        let description = resolve_plugin_description(manifest.description.as_deref(), package.as_ref());
        let version = manifest
            .version
            .clone()
            .or_else(|| package.as_ref().and_then(|pkg| pkg.version.clone()));
        return Ok(PluginManifestInfo {
            id,
            name,
            version,
            description,
            kind: PluginManifestKind::Cowork,
            manifest_path: cowork_path,
            config_schema: None,
            permissions: manifest.permissions.unwrap_or_default(),
        });
    }

    // Claude plugin.json is optional; if absent, derive metadata from directory and/or package.json.
    let looks_like_claude_plugin = root.join(".claude-plugin").is_dir()
        || root.join("skills").is_dir()
        || root.join("commands").is_dir()
        || root.join(".mcp.json").is_file()
        || root.join(".lsp.json").is_file();
    if !looks_like_claude_plugin {
        anyhow::bail!("plugin manifest not found");
    }

    let raw_id = root
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_string())
        .context("plugin id not found")?;
    let id = normalize_plugin_id(&raw_id)?;
    let name = resolve_plugin_name(&id, None, package.as_ref());
    let description = resolve_plugin_description(None, package.as_ref());
    let version = package.as_ref().and_then(|pkg| pkg.version.clone());
    Ok(PluginManifestInfo {
        id,
        name,
        version,
        description,
        kind: PluginManifestKind::Cowork,
        manifest_path: cowork_path,
        config_schema: None,
        permissions: PluginPermissions::default(),
    })
}

fn plugin_permissions_for_root(root: &Path) -> Option<PluginPermissions> {
    load_plugin_manifest(root).ok().map(|manifest| manifest.permissions)
}

fn resolve_permission_root(raw: &str, workspace_dir: &Path) -> Option<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let mut path = PathBuf::from(trimmed);
    if trimmed == "~" {
        if let Ok(home) = home_dir() {
            path = home;
        }
    } else if let Some(stripped) = trimmed.strip_prefix("~/") {
        if let Ok(home) = home_dir() {
            path = home.join(stripped);
        }
    }
    if path.is_relative() {
        path = workspace_dir.join(path);
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return None;
    }
    Some(path)
}

fn is_root_allowed(candidate: &Path, allowed_roots: &[PathBuf]) -> bool {
    allowed_roots.iter().any(|root| candidate.starts_with(root))
}

fn apply_plugin_permissions_to_policy(
    policy: &WorkspacePolicy,
    permissions: Option<&PluginPermissions>,
    workspace_dir: &Path,
) -> WorkspacePolicy {
    let mut next = policy.clone();
    let Some(perms) = permissions else { return next };

    if perms.read_only.unwrap_or(false) {
        next.read_only = true;
    }
    if let Some(net) = perms.network_access {
        if !net {
            next.network_access = false;
        }
    }
    if let Some(roots) = perms.allowed_roots.as_ref() {
        let mut resolved = Vec::new();
        for raw in roots {
            if let Some(root) = resolve_permission_root(raw, workspace_dir) {
                if is_root_allowed(&root, &policy.allowed_roots) {
                    resolved.push(root);
                }
            }
        }
        if !resolved.is_empty() {
            next.allowed_roots = resolved;
        }
    }

    next
}

fn load_claude_manifest(root: &Path) -> Result<Option<ClaudePluginManifest>> {
    let path = root.join(COWORK_MANIFEST_PATH);
    if !path.exists() {
        return Ok(None);
    }
    let raw = read_to_string(&path)?;
    let manifest: ClaudePluginManifest =
        serde_json::from_str(&raw).context("parse plugin.json")?;
    Ok(Some(manifest))
}

fn component_paths_from_spec(spec: &ComponentPathSpec) -> Vec<String> {
    match spec {
        ComponentPathSpec::Single(value) => vec![value.clone()],
        ComponentPathSpec::Many(values) => values.clone(),
        ComponentPathSpec::Object(obj) => {
            let mut out = Vec::new();
            if let Some(path) = obj.path.as_ref() {
                out.push(path.clone());
            }
            if let Some(paths) = obj.paths.as_ref() {
                out.extend(paths.clone());
            }
            out
        }
    }
}

fn resolve_manifest_path(root: &Path, raw: &str) -> Result<PathBuf> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        anyhow::bail!("manifest path is empty");
    }
    if Path::new(trimmed).is_absolute() {
        anyhow::bail!("manifest path must be relative to plugin root: {trimmed}");
    }
    if !trimmed.starts_with("./") {
        anyhow::bail!("manifest path must start with ./ ({trimmed})");
    }
    let without_prefix = trimmed.trim_start_matches("./");
    if without_prefix.is_empty() {
        anyhow::bail!("manifest path must not be empty");
    }
    let rel = Path::new(without_prefix);
    for component in rel.components() {
        if matches!(component, Component::ParentDir) {
            anyhow::bail!("manifest path cannot traverse outside plugin root: {trimmed}");
        }
    }
    Ok(root.join(rel))
}

fn resolve_plugin_component_paths(root: &Path) -> Result<PluginComponentPaths> {
    let mut paths = PluginComponentPaths::default();
    paths.skills.push(root.join("skills"));
    paths.commands.push(root.join("commands"));

    if let Some(manifest) = load_claude_manifest(root)? {
        if let Some(spec) = manifest.skills.as_ref() {
            for raw in component_paths_from_spec(spec) {
                let resolved = resolve_manifest_path(root, &raw)?;
                if resolved.exists() {
                    paths.skills.push(resolved);
                }
            }
        }
        if let Some(spec) = manifest.commands.as_ref() {
            for raw in component_paths_from_spec(spec) {
                let resolved = resolve_manifest_path(root, &raw)?;
                if resolved.exists() {
                    paths.commands.push(resolved);
                }
            }
        }
    }

    paths.skills = dedupe_paths(paths.skills);
    paths.commands = dedupe_paths(paths.commands);
    Ok(paths)
}

fn dedupe_paths(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for path in paths {
        let key = path.to_string_lossy().to_string();
        if seen.insert(key) {
            out.push(path);
        }
    }
    out
}

pub fn list_plugins_command(
    state_dir: Option<PathBuf>,
    workspace: Option<PathBuf>,
    include_disabled: bool,
) -> Result<Value> {
    let (cfg, paths) = load_config(state_dir, workspace)?;
    let policy = resolve_mcp_policy(&cfg);
    let store = TaskStore::open(&paths)?;
    let installs = load_install_records(&paths)?;
    let plugins = store.list_plugins(include_disabled)?;
    let items: Vec<Value> = plugins
        .into_iter()
        .map(|plugin| {
            let assets = plugin_assets(Path::new(&plugin.path));
            let manifest = load_plugin_manifest(Path::new(&plugin.path)).ok();
            let permissions = manifest.as_ref().map(|m| m.permissions.clone());
            let install = installs.get(&plugin.id).cloned();
            let mcp_enabled =
                plugin.enabled && assets.has_mcp && policy.is_plugin_enabled(&plugin.id);
            json!({
                "id": plugin.id,
                "name": plugin.name,
                "version": plugin.version,
                "description": plugin.description,
                "source": plugin.source,
                "path": plugin.path,
                "enabled": plugin.enabled,
                "installedAtMs": plugin.installed_at_ms,
                "updatedAtMs": plugin.updated_at_ms,
                "skills": assets.skills,
                "commands": assets.commands,
                "hasMcp": assets.has_mcp,
                "mcpEnabled": mcp_enabled,
                "manifestType": manifest.as_ref().map(|m| manifest_kind_label(m.kind)),
                "manifestPath": manifest
                    .as_ref()
                    .map(|m| m.manifest_path.to_string_lossy().to_string()),
                "permissions": permissions,
                "install": install,
            })
        })
        .collect();
    Ok(json!({ "plugins": items }))
}

pub fn list_plugin_commands_command(
    plugin_id: Option<String>,
    state_dir: Option<PathBuf>,
    workspace: Option<PathBuf>,
) -> Result<Value> {
    let (_cfg, paths) = load_config(state_dir, workspace)?;
    let store = TaskStore::open(&paths)?;
    let plugins = if let Some(ref id) = plugin_id {
        store
            .get_plugin(id)?
            .map(|p| vec![p])
            .unwrap_or_default()
    } else {
        store.list_plugins(true)?
    };

    let mut commands = Vec::new();
    for plugin in plugins {
        let root = PathBuf::from(&plugin.path);
        let entries = load_plugin_commands(&root, &plugin)?;
        commands.extend(entries);
    }

    Ok(json!({ "commands": commands }))
}

pub fn run_plugin_command_command(
    plugin_id: &str,
    command: &str,
    input: Option<String>,
    codex_path: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    workspace: Option<PathBuf>,
    auto_approve: bool,
) -> Result<Value> {
    let (cfg, paths) = load_config(state_dir, workspace)?;
    let store = TaskStore::open(&paths)?;
    let plugin = store
        .get_plugin(plugin_id)?
        .context("plugin not found")?;
    if !plugin.enabled {
        anyhow::bail!("plugin is disabled");
    }

    let prompt = resolve_plugin_command_prompt(
        &paths,
        &plugin,
        command,
        input.as_deref(),
        auto_approve,
    )?;
    let codex_path = resolve_codex_path(&cfg, codex_path)?;
    let permissions = plugin_permissions_for_root(Path::new(&plugin.path));
    let effective_policy = apply_plugin_permissions_to_policy(
        &paths.workspace_policy,
        permissions.as_ref(),
        &paths.workspace_dir,
    );
    let sandbox_policy = workspace_sandbox_policy(&effective_policy)?;

    let codex_home = paths.state_dir.join("codex");
    std::fs::create_dir_all(&codex_home)
        .with_context(|| format!("create {}", codex_home.display()))?;

    let mut env = Vec::new();
    env.push((
        "CODEX_HOME".to_string(),
        codex_home.to_string_lossy().to_string(),
    ));
    env.push((
        "CODEX_WORKSPACE_DIR".to_string(),
        paths.workspace_dir.to_string_lossy().to_string(),
    ));

    let config_overrides = cfg
        .codex
        .as_ref()
        .and_then(|c| c.config_overrides.clone())
        .unwrap_or_default();

    let approval_mode = if auto_approve {
        ApprovalMode::AutoApprove
    } else {
        ApprovalMode::AutoDeny
    };

    let mut client = CodexClient::spawn(&codex_path, &config_overrides, &env, approval_mode)?;
    client.initialize()?;
    let thread_id = client.thread_start()?;
    let outcome = client.run_turn(&thread_id, &prompt, None, sandbox_policy, Some(paths.workspace_dir.clone()))?;

    Ok(json!({
        "ok": true,
        "message": outcome.message,
        "warnings": outcome.warnings
    }))
}

pub fn add_plugin_command(
    path: Option<PathBuf>,
    npm: Option<String>,
    link: bool,
    source: Option<String>,
    state_dir: Option<PathBuf>,
    workspace: Option<PathBuf>,
) -> Result<Value> {
    let (cfg, paths) = load_config(state_dir, workspace)?;
    let store = TaskStore::open(&paths)?;
    let policy = resolve_mcp_policy(&cfg);
    let install_source = if let Some(spec) = npm {
        PluginInstallSource::Npm { spec }
    } else if let Some(path) = path {
        PluginInstallSource::Path(path)
    } else {
        anyhow::bail!("missing plugin source");
    };
    let plugin = install_plugin(
        &paths,
        &store,
        install_source,
        link,
        source,
        &policy,
        PluginInstallMode::Install,
        None,
    )?;
    let assets = plugin_assets(Path::new(&plugin.path));
    Ok(json!({ "plugin": plugin, "assets": assets }))
}

fn read_installed_version(paths: &ClawdPaths, plugin_id: &str, install_path: Option<&str>) -> Option<String> {
    let root = install_path
        .map(PathBuf::from)
        .unwrap_or_else(|| plugin_root(paths, plugin_id));
    load_plugin_manifest(&root).ok().and_then(|manifest| manifest.version)
}

fn probe_npm_spec(spec: &str, expected_id: Option<&str>) -> Result<Option<String>> {
    let source = PluginInstallSource::Npm { spec: spec.to_string() };
    let prepared = prepare_plugin_source(&source)?;
    if let Some(expected) = expected_id {
        let expected_id = normalize_plugin_id(expected)?;
        if prepared.plugin.manifest.id != expected_id {
            anyhow::bail!(
                "plugin id mismatch: expected {expected_id}, got {}",
                prepared.plugin.manifest.id
            );
        }
    }
    Ok(prepared.plugin.manifest.version.clone())
}

pub fn update_plugin_command(
    plugin_id: Option<String>,
    all: bool,
    dry_run: bool,
    state_dir: Option<PathBuf>,
    workspace: Option<PathBuf>,
) -> Result<Value> {
    let (cfg, paths) = load_config(state_dir, workspace)?;
    let store = TaskStore::open(&paths)?;
    let policy = resolve_mcp_policy(&cfg);
    let installs = load_install_records(&paths)?;
    let targets: Vec<String> = if all {
        installs.keys().cloned().collect()
    } else if let Some(id) = plugin_id {
        vec![id]
    } else {
        anyhow::bail!("provide --all or a plugin id");
    };

    let mut outcomes = Vec::new();
    for plugin_id in targets {
        let record = match installs.get(&plugin_id) {
            Some(record) => record.clone(),
            None => {
                outcomes.push(json!({
                    "pluginId": plugin_id,
                    "status": "skipped",
                    "message": format!("No install record for \"{plugin_id}\".")
                }));
                continue;
            }
        };

        if record.source != "npm" {
            outcomes.push(json!({
                "pluginId": plugin_id,
                "status": "skipped",
                "message": format!("Skipping \"{plugin_id}\" (source: {}).", record.source)
            }));
            continue;
        }

        let Some(spec) = record.spec.clone().filter(|s| !s.trim().is_empty()) else {
            outcomes.push(json!({
                "pluginId": plugin_id,
                "status": "skipped",
                "message": format!("Skipping \"{plugin_id}\" (missing npm spec).")
            }));
            continue;
        };

        let current_version = read_installed_version(&paths, &plugin_id, record.install_path.as_deref());

        if dry_run {
            match probe_npm_spec(&spec, Some(&plugin_id)) {
                Ok(next_version) => {
                    let current_label = current_version.clone().unwrap_or_else(|| "unknown".to_string());
                    let next_label = next_version.clone().unwrap_or_else(|| "unknown".to_string());
                    let status = if current_version.is_some() && next_version.is_some() && current_version == next_version {
                        "unchanged"
                    } else {
                        "updated"
                    };
                    let message = if status == "unchanged" {
                        format!("{plugin_id} already at {current_label}.")
                    } else {
                        format!("Would update {plugin_id}: {current_label} -> {next_label}.")
                    };
                    outcomes.push(json!({
                        "pluginId": plugin_id,
                        "status": status,
                        "currentVersion": current_version,
                        "nextVersion": next_version,
                        "message": message
                    }));
                }
                Err(err) => {
                    outcomes.push(json!({
                        "pluginId": plugin_id,
                        "status": "error",
                        "message": format!("Failed to check {plugin_id}: {err}")
                    }));
                }
            }
            continue;
        }

        let install_source = PluginInstallSource::Npm { spec: spec.clone() };
        let plugin = match install_plugin(
            &paths,
            &store,
            install_source,
            false,
            None,
            &policy,
            PluginInstallMode::Update,
            Some(&plugin_id),
        ) {
            Ok(plugin) => plugin,
            Err(err) => {
                outcomes.push(json!({
                    "pluginId": plugin_id,
                    "status": "error",
                    "message": format!("Failed to update {plugin_id}: {err}")
                }));
                continue;
            }
        };

        let next_version = plugin.version.clone();
        let current_label = current_version.clone().unwrap_or_else(|| "unknown".to_string());
        let next_label = next_version.clone().unwrap_or_else(|| "unknown".to_string());
        let status = if current_version.is_some() && next_version.is_some() && current_version == next_version {
            "unchanged"
        } else {
            "updated"
        };
        let message = if status == "unchanged" {
            format!("{plugin_id} already at {current_label}.")
        } else {
            format!("Updated {plugin_id}: {current_label} -> {next_label}.")
        };
        outcomes.push(json!({
            "pluginId": plugin_id,
            "status": status,
            "currentVersion": current_version,
            "nextVersion": next_version,
            "message": message
        }));
    }

    Ok(json!({
        "dryRun": dry_run,
        "outcomes": outcomes
    }))
}

pub fn enable_plugin_command(
    plugin_id: &str,
    state_dir: Option<PathBuf>,
    workspace: Option<PathBuf>,
) -> Result<Value> {
    let (cfg, paths) = load_config(state_dir, workspace)?;
    let store = TaskStore::open(&paths)?;
    let plugin = store
        .set_plugin_enabled(plugin_id, true)?
        .context("plugin not found")?;
    let policy = resolve_mcp_policy(&cfg);
    sync_plugin_skills(&paths, &plugin, &policy)?;
    Ok(json!({ "plugin": plugin }))
}

pub fn disable_plugin_command(
    plugin_id: &str,
    state_dir: Option<PathBuf>,
    workspace: Option<PathBuf>,
) -> Result<Value> {
    let (_cfg, paths) = load_config(state_dir, workspace)?;
    let store = TaskStore::open(&paths)?;
    let plugin = store
        .set_plugin_enabled(plugin_id, false)?
        .context("plugin not found")?;
    remove_plugin_skills(&paths, &plugin.id)?;
    Ok(json!({ "plugin": plugin }))
}

pub fn remove_plugin_command(
    plugin_id: &str,
    keep_files: bool,
    state_dir: Option<PathBuf>,
    workspace: Option<PathBuf>,
) -> Result<Value> {
    let (_cfg, paths) = load_config(state_dir, workspace)?;
    let store = TaskStore::open(&paths)?;
    if let Some(plugin) = store.get_plugin(plugin_id)? {
        remove_plugin_skills(&paths, &plugin.id)?;
        if !keep_files {
            let root = plugin_root(&paths, &plugin.id);
            if root.exists() {
                fs::remove_dir_all(&root)
                    .with_context(|| format!("remove {}", root.display()))?;
            }
        }
        store.remove_plugin(plugin_id)?;
    }
    let mut installs = load_install_records(&paths)?;
    if installs.remove(plugin_id).is_some() {
        save_install_records(&paths, &installs)?;
    }
    Ok(json!({ "ok": true }))
}

pub fn sync_plugins_command(
    state_dir: Option<PathBuf>,
    workspace: Option<PathBuf>,
) -> Result<Value> {
    let (cfg, paths) = load_config(state_dir, workspace)?;
    let store = TaskStore::open(&paths)?;
    let plugins = store.list_plugins(true)?;
    let policy = resolve_mcp_policy(&cfg);
    let mut synced = Vec::new();
    for plugin in plugins {
        if plugin.enabled {
            sync_plugin_skills(&paths, &plugin, &policy)?;
            synced.push(plugin.id);
        } else {
            remove_plugin_skills(&paths, &plugin.id)?;
        }
    }
    Ok(json!({ "synced": synced }))
}

pub fn export_mcp_command(
    output: Option<PathBuf>,
    state_dir: Option<PathBuf>,
    workspace: Option<PathBuf>,
) -> Result<Value> {
    let (cfg, paths) = load_config(state_dir, workspace)?;
    let store = TaskStore::open(&paths)?;
    let plugins = store.list_plugins(false)?;
    let mut mcp_servers = Map::new();
    let mut included = Vec::new();
    let policy = resolve_mcp_policy(&cfg);

    for plugin in plugins {
        let root = PathBuf::from(&plugin.path);
        let Some(mcp_value) = read_plugin_mcp(&root)? else { continue };
        let permissions = plugin_permissions_for_root(&root);
        let count =
            merge_mcp_config(&mut mcp_servers, &plugin.id, &mcp_value, &policy, permissions.as_ref());
        if count > 0 {
            included.push(plugin.id);
        }
    }

    let output_value = json!({ "mcpServers": mcp_servers });
    let output_path = output.unwrap_or_else(|| paths.state_dir.join("mcp").join("plugins.json"));
    write_json_value(&output_path, &output_value)?;

    Ok(json!({
        "output": output_path.to_string_lossy(),
        "plugins": included
    }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveKind {
    Tar,
    Zip,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PreparedSourceKind {
    Path,
    Archive,
    Npm,
}

struct PreparedPlugin {
    root: PathBuf,
    manifest: PluginManifestInfo,
    package: Option<PackageManifest>,
    temp_dir: Option<PathBuf>,
}

impl Drop for PreparedPlugin {
    fn drop(&mut self) {
        if let Some(dir) = self.temp_dir.take() {
            let _ = fs::remove_dir_all(dir);
        }
    }
}

struct PreparedPluginSource {
    plugin: PreparedPlugin,
    kind: PreparedSourceKind,
    source_path: Option<PathBuf>,
    spec: Option<String>,
    link_source: Option<PathBuf>,
}

fn resolve_user_path(path: &Path) -> Result<PathBuf> {
    let raw = path.to_string_lossy();
    if raw == "~" {
        return Ok(home_dir()?);
    }
    if let Some(stripped) = raw.strip_prefix("~/") {
        return Ok(home_dir()?.join(stripped));
    }
    Ok(path.to_path_buf())
}

fn detect_archive_kind(path: &Path) -> Option<ArchiveKind> {
    let name = path.file_name()?.to_string_lossy().to_lowercase();
    if name.ends_with(".zip") {
        return Some(ArchiveKind::Zip);
    }
    if name.ends_with(".tgz") || name.ends_with(".tar.gz") || name.ends_with(".tar") {
        return Some(ArchiveKind::Tar);
    }
    None
}

fn sanitize_archive_path(path: &Path) -> Result<PathBuf> {
    let mut cleaned = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::ParentDir => {
                anyhow::bail!("archive entry escapes destination: {}", path.display());
            }
            Component::CurDir => {}
            Component::Normal(part) => cleaned.push(part),
        }
    }
    if cleaned.as_os_str().is_empty() {
        anyhow::bail!("archive entry has empty path");
    }
    Ok(cleaned)
}

fn extract_tar_archive(archive_path: &Path, dest_dir: &Path) -> Result<()> {
    let file = File::open(archive_path)
        .with_context(|| format!("open {}", archive_path.display()))?;
    let is_gz = archive_path
        .file_name()
        .and_then(|s| s.to_str())
        .map(|s| s.to_lowercase())
        .map(|s| s.ends_with(".tgz") || s.ends_with(".tar.gz"))
        .unwrap_or(false);
    let reader: Box<dyn io::Read> = if is_gz {
        Box::new(GzDecoder::new(file))
    } else {
        Box::new(file)
    };
    let mut archive = Archive::new(reader);
    for entry in archive.entries().context("read tar entries")? {
        let mut entry = entry.context("read tar entry")?;
        let raw_path = entry.path().context("read tar path")?.to_path_buf();
        let rel = sanitize_archive_path(&raw_path)?;
        let out = dest_dir.join(&rel);
        if entry.header().entry_type().is_dir() {
            ensure_dir(&out)?;
        } else {
            if let Some(parent) = out.parent() {
                ensure_dir(parent)?;
            }
            entry
                .unpack(&out)
                .with_context(|| format!("extract {}", out.display()))?;
        }
    }
    Ok(())
}

fn extract_zip_archive(archive_path: &Path, dest_dir: &Path) -> Result<()> {
    let file = File::open(archive_path)
        .with_context(|| format!("open {}", archive_path.display()))?;
    let mut archive = ZipArchive::new(file).context("read zip archive")?;
    for i in 0..archive.len() {
        let mut entry = archive.by_index(i).context("read zip entry")?;
        let raw = entry.name().replace('\\', "/");
        let rel = sanitize_archive_path(Path::new(&raw))?;
        let out = dest_dir.join(&rel);
        if entry.is_dir() {
            ensure_dir(&out)?;
            continue;
        }
        if let Some(parent) = out.parent() {
            ensure_dir(parent)?;
        }
        let mut out_file = File::create(&out)
            .with_context(|| format!("create {}", out.display()))?;
        io::copy(&mut entry, &mut out_file)
            .with_context(|| format!("extract {}", out.display()))?;
    }
    Ok(())
}

fn extract_archive(archive_path: &Path, dest_dir: &Path) -> Result<()> {
    match detect_archive_kind(archive_path) {
        Some(ArchiveKind::Tar) => extract_tar_archive(archive_path, dest_dir),
        Some(ArchiveKind::Zip) => extract_zip_archive(archive_path, dest_dir),
        None => anyhow::bail!("unsupported archive: {}", archive_path.display()),
    }
}

fn resolve_packed_root_dir(extract_dir: &Path) -> Result<PathBuf> {
    let direct = extract_dir.join("package");
    if direct.is_dir() {
        return Ok(direct);
    }
    let mut dirs = Vec::new();
    for entry in fs::read_dir(extract_dir)
        .with_context(|| format!("read {}", extract_dir.display()))?
    {
        let entry = entry?;
        if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
            dirs.push(entry.path());
        }
    }
    if dirs.len() != 1 {
        let names: Vec<String> = dirs
            .iter()
            .filter_map(|p| p.file_name().and_then(|s| s.to_str()).map(|s| s.to_string()))
            .collect();
        anyhow::bail!("unexpected archive layout (dirs: {})", names.join(", "));
    }
    Ok(dirs.remove(0))
}

fn create_temp_dir(prefix: &str) -> Result<PathBuf> {
    let mut path = std::env::temp_dir();
    path.push(format!("{}{}", prefix, Uuid::new_v4()));
    ensure_dir(&path)?;
    Ok(path)
}

fn run_npm_pack(spec: &str, dest_dir: &Path) -> Result<PathBuf> {
    let output = Command::new("npm")
        .arg("pack")
        .arg(spec)
        .current_dir(dest_dir)
        .env("COREPACK_ENABLE_DOWNLOAD_PROMPT", "0")
        .output()
        .context("npm pack")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if stderr.trim().is_empty() {
            stdout.trim().to_string()
        } else {
            stderr.trim().to_string()
        };
        anyhow::bail!("npm pack failed: {}", detail);
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let packed = stdout
        .lines()
        .map(|line| line.trim())
        .filter(|line| !line.is_empty())
        .last()
        .context("npm pack produced no archive")?;
    Ok(dest_dir.join(packed))
}

fn install_dependencies_if_needed(root: &Path, package: Option<&PackageManifest>) -> Result<()> {
    let has_deps = package
        .and_then(|pkg| pkg.dependencies.as_ref())
        .map(|deps| !deps.is_empty())
        .unwrap_or(false);
    if !has_deps {
        return Ok(());
    }
    let output = Command::new("npm")
        .args(["install", "--omit=dev", "--silent"])
        .current_dir(root)
        .env("COREPACK_ENABLE_DOWNLOAD_PROMPT", "0")
        .output()
        .context("npm install")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = if stderr.trim().is_empty() {
            stdout.trim().to_string()
        } else {
            stderr.trim().to_string()
        };
        anyhow::bail!("npm install failed: {}", detail);
    }
    Ok(())
}

fn prepare_plugin_source(source: &PluginInstallSource) -> Result<PreparedPluginSource> {
    match source {
        PluginInstallSource::Path(path) => {
            let resolved = resolve_user_path(path)?;
            let metadata = fs::metadata(&resolved)
                .with_context(|| format!("stat {}", resolved.display()))?;
            if metadata.is_dir() {
                let manifest = load_plugin_manifest(&resolved)?;
                let package = read_package_manifest(&resolved);
                return Ok(PreparedPluginSource {
                    plugin: PreparedPlugin {
                        root: resolved.clone(),
                        manifest,
                        package,
                        temp_dir: None,
                    },
                    kind: PreparedSourceKind::Path,
                    source_path: Some(resolved.clone()),
                    spec: None,
                    link_source: Some(resolved),
                });
            }
            if metadata.is_file() {
                let kind = detect_archive_kind(&resolved)
                    .context("plugin path must be a directory or archive")?;
                let temp_dir = create_temp_dir("clawdex-plugin-")?;
                let extract_dir = temp_dir.join("extract");
                ensure_dir(&extract_dir)?;
                extract_archive(&resolved, &extract_dir)?;
                let root = resolve_packed_root_dir(&extract_dir)?;
                let manifest = load_plugin_manifest(&root)?;
                let package = read_package_manifest(&root);
                return Ok(PreparedPluginSource {
                    plugin: PreparedPlugin {
                        root,
                        manifest,
                        package,
                        temp_dir: Some(temp_dir),
                    },
                    kind: match kind {
                        ArchiveKind::Tar | ArchiveKind::Zip => PreparedSourceKind::Archive,
                    },
                    source_path: Some(resolved),
                    spec: None,
                    link_source: None,
                });
            }
            anyhow::bail!("unsupported plugin path: {}", resolved.display())
        }
        PluginInstallSource::Npm { spec } => {
            let trimmed = spec.trim();
            if trimmed.is_empty() {
                anyhow::bail!("missing npm spec");
            }
            let temp_dir = create_temp_dir("clawdex-npm-")?;
            let archive_path = run_npm_pack(trimmed, &temp_dir)?;
            let extract_dir = temp_dir.join("extract");
            ensure_dir(&extract_dir)?;
            extract_archive(&archive_path, &extract_dir)?;
            let root = resolve_packed_root_dir(&extract_dir)?;
            let manifest = load_plugin_manifest(&root)?;
            let package = read_package_manifest(&root);
            Ok(PreparedPluginSource {
                plugin: PreparedPlugin {
                    root,
                    manifest,
                    package,
                    temp_dir: Some(temp_dir),
                },
                kind: PreparedSourceKind::Npm,
                source_path: None,
                spec: Some(trimmed.to_string()),
                link_source: None,
            })
        }
    }
}

fn install_plugin(
    paths: &ClawdPaths,
    store: &TaskStore,
    source: PluginInstallSource,
    link: bool,
    source_label: Option<String>,
    policy: &McpPolicy,
    mode: PluginInstallMode,
    expected_id: Option<&str>,
) -> Result<PluginRecord> {
    if mode == PluginInstallMode::Update && link {
        anyhow::bail!("--link is not supported for updates");
    }
    let prepared = prepare_plugin_source(&source)?;
    if link && prepared.link_source.is_none() {
        anyhow::bail!("--link requires a directory path");
    }

    let plugin_id = prepared.plugin.manifest.id.clone();
    if let Some(expected) = expected_id {
        let expected_id = normalize_plugin_id(expected)?;
        if plugin_id != expected_id {
            anyhow::bail!("plugin id mismatch: expected {expected_id}, got {plugin_id}");
        }
    }

    let root = plugin_root(paths, &plugin_id);
    if root.exists() && mode == PluginInstallMode::Install {
        anyhow::bail!("plugin already exists: {}", root.display());
    }
    if let Some(parent) = root.parent() {
        ensure_dir(parent)?;
    }

    let mut backup = None;
    if !link && root.exists() {
        let backup_path = root.with_extension(format!("backup-{}", now_ms()));
        fs::rename(&root, &backup_path)
            .with_context(|| format!("backup {}", root.display()))?;
        backup = Some(backup_path);
    }

    if link {
        #[cfg(unix)]
        {
            let link_source = prepared
                .link_source
                .as_ref()
                .context("link source missing")?;
            std::os::unix::fs::symlink(link_source, &root)
                .with_context(|| format!("symlink {}", root.display()))?;
        }
        #[cfg(not(unix))]
        {
            anyhow::bail!("--link is only supported on unix platforms");
        }
    } else {
        if let Err(err) = copy_dir(&prepared.plugin.root, &root) {
            if let Some(backup_path) = backup.take() {
                let _ = fs::remove_dir_all(&root);
                let _ = fs::rename(&backup_path, &root);
            }
            return Err(err);
        }
        if let Err(err) = install_dependencies_if_needed(&root, prepared.plugin.package.as_ref()) {
            if let Some(backup_path) = backup.take() {
                let _ = fs::remove_dir_all(&root);
                let _ = fs::rename(&backup_path, &root);
            }
            return Err(err);
        }
        if let Err(err) = maybe_rewrite_claude_code_install_paths(&root, &plugin_id) {
            if let Some(backup_path) = backup.take() {
                let _ = fs::remove_dir_all(&root);
                let _ = fs::rename(&backup_path, &root);
            }
            return Err(err);
        }
        if let Some(backup_path) = backup.take() {
            let _ = fs::remove_dir_all(&backup_path);
        }
    }

    let now = now_ms();
    let existing = store.get_plugin(&plugin_id)?;
    let installed_at_ms = if mode == PluginInstallMode::Install {
        now
    } else {
        existing
            .as_ref()
            .map(|p| p.installed_at_ms)
            .unwrap_or(now)
    };
    let enabled = if mode == PluginInstallMode::Install {
        true
    } else {
        existing.as_ref().map(|p| p.enabled).unwrap_or(true)
    };

    let default_source = match prepared.kind {
        PreparedSourceKind::Path | PreparedSourceKind::Archive => prepared
            .source_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|| "path".to_string()),
        PreparedSourceKind::Npm => prepared
            .spec
            .as_ref()
            .map(|s| format!("npm:{s}"))
            .unwrap_or_else(|| "npm".to_string()),
    };

    let plugin = PluginRecord {
        id: plugin_id.clone(),
        name: prepared.plugin.manifest.name.clone(),
        version: prepared.plugin.manifest.version.clone(),
        description: prepared.plugin.manifest.description.clone(),
        source: source_label.or(Some(default_source)),
        path: root.to_string_lossy().to_string(),
        enabled,
        installed_at_ms,
        updated_at_ms: now,
    };
    store.upsert_plugin(&plugin)?;

    let mut installs = load_install_records(paths)?;
    let previous = installs.get(&plugin_id).cloned();
    let installed_at_ms = if mode == PluginInstallMode::Install {
        now
    } else {
        previous
            .as_ref()
            .map(|record| record.installed_at_ms)
            .unwrap_or(now)
    };
    let record = PluginInstallRecord {
        source: match prepared.kind {
            PreparedSourceKind::Path => "path".to_string(),
            PreparedSourceKind::Archive => "archive".to_string(),
            PreparedSourceKind::Npm => "npm".to_string(),
        },
        spec: prepared.spec.clone(),
        source_path: prepared
            .source_path
            .as_ref()
            .map(|p| p.to_string_lossy().to_string()),
        install_path: Some(root.to_string_lossy().to_string()),
        version: prepared.plugin.manifest.version.clone(),
        installed_at_ms,
        updated_at_ms: now,
    };
    installs.insert(plugin_id.clone(), record);
    save_install_records(paths, &installs)?;

    if enabled {
        sync_plugin_skills(paths, &plugin, policy)?;
    } else {
        remove_plugin_skills(paths, &plugin.id)?;
    }
    Ok(plugin)
}

fn maybe_rewrite_claude_code_install_paths(root: &Path, plugin_id: &str) -> Result<()> {
    // `get-shit-done` (GSD) is primarily distributed as a Claude Code install under `~/.claude/`.
    // When we install it as a Clawdex plugin, its internal references should resolve relative to
    // the plugin install directory instead.
    if plugin_id != "get-shit-done" {
        return Ok(());
    }

    let root_str = root.to_string_lossy().to_string();
    let replacement_with_slash = format!("{}/", root_str);

    for entry in WalkDir::new(root).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        if !is_claude_code_rewrite_candidate(path) {
            continue;
        }

        let bytes = fs::read(path).with_context(|| format!("read {}", path.display()))?;
        let Ok(raw) = String::from_utf8(bytes) else { continue };
        if !raw.contains("~/.claude") {
            continue;
        }

        let updated = raw
            .replace("~/.claude/", &replacement_with_slash)
            .replace("~/.claude", &root_str);
        if updated == raw {
            continue;
        }
        fs::write(path, updated).with_context(|| format!("write {}", path.display()))?;
    }

    Ok(())
}

fn is_claude_code_rewrite_candidate(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|s| s.to_str()).unwrap_or(""),
        "md" | "txt"
            | "json"
            | "json5"
            | "js"
            | "mjs"
            | "cjs"
            | "ts"
            | "tsx"
            | "sh"
            | "yml"
            | "yaml"
            | "toml"
    )
}

fn plugin_root(paths: &ClawdPaths, plugin_id: &str) -> PathBuf {
    paths.state_dir.join("plugins").join(plugin_id)
}

fn plugin_assets(root: &Path) -> PluginAssets {
    let mut assets = PluginAssets::default();
    let skills_dir = root.join("skills");
    if skills_dir.exists() {
        for entry in WalkDir::new(&skills_dir).into_iter().filter_map(Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            if path.file_name().and_then(|s| s.to_str()) == Some("SKILL.md") {
                assets.skills += 1;
                continue;
            }
            if path.parent() == Some(skills_dir.as_path()) {
                if path.extension().and_then(|s| s.to_str()) == Some("md") {
                    assets.skills += 1;
                }
            }
        }
    }
    let commands_dir = root.join("commands");
    if commands_dir.exists() {
        for entry in WalkDir::new(&commands_dir)
            .into_iter()
            .filter_map(Result::ok)
        {
            if entry.file_type().is_file() {
                assets.commands += 1;
            }
        }
    }
    assets.has_mcp = root.join(".mcp.json").exists();
    assets
}

fn codex_home_dir(paths: &ClawdPaths) -> PathBuf {
    paths.state_dir.join("codex")
}

fn plugin_overlay_root(paths: &ClawdPaths) -> PathBuf {
    codex_home_dir(paths).join("skills").join("_clawdex_plugins")
}

fn legacy_plugin_skill_root(paths: &ClawdPaths, plugin_id: &str) -> PathBuf {
    codex_home_dir(paths)
        .join("skills")
        .join("clawdex")
        .join("plugins")
        .join(plugin_id)
}

fn sync_plugin_skills(paths: &ClawdPaths, plugin: &PluginRecord, policy: &McpPolicy) -> Result<()> {
    let root = PathBuf::from(&plugin.path);
    remove_plugin_skills(paths, &plugin.id)?;

    let component_paths = resolve_plugin_component_paths(&root)?;
    let skill_sources = collect_skill_sources(&component_paths.skills)?;
    let command_sources = collect_command_sources(&component_paths.commands)?;

    let overlay_root = plugin_overlay_root(paths);
    ensure_dir(&overlay_root)?;

    let mut skill_names = HashSet::new();
    for skill in skill_sources {
        let namespaced = format!("{}:{}", plugin.id, skill.name());
        if !skill_names.insert(namespaced.clone()) {
            continue;
        }
        let dest = overlay_root.join(&namespaced);
        if dest.exists() {
            fs::remove_dir_all(&dest)
                .with_context(|| format!("remove {}", dest.display()))?;
        }
        ensure_dir(&dest)?;
        match skill {
            SkillSource::Directory { root, .. } => {
                copy_dir(&root, &dest)?;
            }
            SkillSource::LegacyFile { path, .. } => {
                fs::copy(&path, dest.join("SKILL.md"))
                    .with_context(|| format!("copy {} -> {}", path.display(), dest.display()))?;
            }
        }
        let skill_md = dest.join("SKILL.md");
        ensure_namespaced_frontmatter(&skill_md, &namespaced, &plugin.id)?;
    }

    let mut command_names = HashSet::new();
    for path in command_sources {
        let template = load_command_template(&path)?;
        if template.template.trim().is_empty() {
            continue;
        }
        let namespaced = if template.name.contains(':') {
            template.name.clone()
        } else {
            format!("{}:{}", plugin.id, template.name)
        };
        if skill_names.contains(&namespaced) {
            continue;
        }
        if !command_names.insert(namespaced.clone()) {
            continue;
        }
        let dest = overlay_root.join(&namespaced);
        if dest.exists() {
            fs::remove_dir_all(&dest)
                .with_context(|| format!("remove {}", dest.display()))?;
        }
        ensure_dir(&dest)?;
        let yaml = render_command_frontmatter(&plugin.id, &namespaced, template.description.as_deref())?;
        let mut rendered_template = rewrite_claude_code_paths(&template.template);
        if rendered_template.contains("${CLAUDE_PLUGIN_ROOT}") {
            rendered_template = rendered_template.replace("${CLAUDE_PLUGIN_ROOT}", &root.to_string_lossy());
        }
        let contents = format!("---\n{}---\n{}", yaml, rendered_template);
        fs::write(dest.join("SKILL.md"), contents)
            .with_context(|| format!("write {}", dest.display()))?;
    }

    let _ = export_plugin_mcp(paths, plugin, policy);
    Ok(())
}

fn remove_plugin_skills(paths: &ClawdPaths, plugin_id: &str) -> Result<()> {
    remove_plugin_skills_in_state_dir(&paths.state_dir, plugin_id)
}

fn collect_plugin_overlay_skill_names(plugin_root: &Path, plugin_id: &str) -> Result<HashSet<String>> {
    let component_paths = resolve_plugin_component_paths(plugin_root)?;
    let skill_sources = collect_skill_sources(&component_paths.skills)?;
    let command_sources = collect_command_sources(&component_paths.commands)?;

    let mut names = HashSet::new();
    let mut skill_names = HashSet::new();
    for skill in skill_sources {
        let namespaced = format!("{}:{}", plugin_id, skill.name());
        if skill_names.insert(namespaced.clone()) {
            names.insert(namespaced);
        }
    }

    let mut command_names = HashSet::new();
    for path in command_sources {
        let template = load_command_template(&path)?;
        if template.template.trim().is_empty() {
            continue;
        }
        let namespaced = if template.name.contains(':') {
            template.name.clone()
        } else {
            format!("{}:{}", plugin_id, template.name)
        };
        if skill_names.contains(&namespaced) {
            continue;
        }
        if command_names.insert(namespaced.clone()) {
            names.insert(namespaced);
        }
    }

    Ok(names)
}

fn skill_dir_owned_by_plugin(skill_dir: &Path, plugin_id: &str, plugin_root_str: &str) -> bool {
    let skill_md = skill_dir.join("SKILL.md");
    let Ok(contents) = read_to_string(&skill_md) else {
        return false;
    };

    let (frontmatter, _body) = split_frontmatter(&contents);
    if let Some(mapping) = frontmatter {
        let key = YamlValue::String(CLAWDEX_PLUGIN_ID_FRONTMATTER_KEY.to_string());
        if let Some(YamlValue::String(existing)) = mapping.get(&key) {
            if existing.trim() == plugin_id {
                return true;
            }
        }
    }

    // Back-compat: older generated skills may not have the plugin marker, but may still contain
    // the install root when rewriting Claude Code `~/.claude/...` references.
    if !plugin_root_str.is_empty() && contents.contains(plugin_root_str) {
        return true;
    }

    false
}

fn remove_plugin_skills_in_state_dir(state_dir: &Path, plugin_id: &str) -> Result<()> {
    let overlay_root = state_dir
        .join("codex")
        .join("skills")
        .join("_clawdex_plugins");

    let plugin_root = state_dir.join("plugins").join(plugin_id);
    let plugin_root_str = plugin_root.to_string_lossy().to_string();

    let expected_names = if plugin_root.exists() {
        collect_plugin_overlay_skill_names(&plugin_root, plugin_id).unwrap_or_default()
    } else {
        HashSet::new()
    };

    if overlay_root.exists() {
        let prefix = format!("{plugin_id}:");
        for entry in fs::read_dir(&overlay_root)
            .with_context(|| format!("read {}", overlay_root.display()))?
        {
            let entry = entry?;
            if !entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                continue;
            }
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }

            let owned = name.starts_with(&prefix)
                || expected_names.contains(name)
                || skill_dir_owned_by_plugin(&path, plugin_id, &plugin_root_str);
            if !owned {
                continue;
            }

            fs::remove_dir_all(&path)
                .with_context(|| format!("remove {}", path.display()))?;
        }
    }

    let legacy_root = state_dir
        .join("codex")
        .join("skills")
        .join("clawdex")
        .join("plugins")
        .join(plugin_id);
    if legacy_root.exists() {
        fs::remove_dir_all(&legacy_root)
            .with_context(|| format!("remove {}", legacy_root.display()))?;
    }

    let mcp_path = state_dir.join("mcp").join(format!("{plugin_id}.json"));
    if mcp_path.exists() {
        fs::remove_file(&mcp_path)
            .with_context(|| format!("remove {}", mcp_path.display()))?;
    }

    Ok(())
}

impl SkillSource {
    fn name(&self) -> &str {
        match self {
            SkillSource::Directory { name, .. } => name,
            SkillSource::LegacyFile { name, .. } => name,
        }
    }
}

fn collect_skill_sources(paths: &[PathBuf]) -> Result<Vec<SkillSource>> {
    let mut out = Vec::new();
    for path in paths {
        if !path.exists() {
            continue;
        }
        if path.is_file() {
            if path.extension().and_then(|s| s.to_str()) != Some("md") {
                continue;
            }
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("skill")
                .to_string();
            out.push(SkillSource::LegacyFile {
                path: path.to_path_buf(),
                name,
            });
            continue;
        }
        collect_skills_from_dir(path, &mut out)?;
    }
    Ok(out)
}

fn collect_skills_from_dir(dir: &Path, out: &mut Vec<SkillSource>) -> Result<()> {
    if dir.join("SKILL.md").exists() {
        let name = dir
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("skill")
            .to_string();
        out.push(SkillSource::Directory {
            root: dir.to_path_buf(),
            name,
        });
        return Ok(());
    }

    for entry in fs::read_dir(dir).with_context(|| format!("read {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if path.is_dir() {
            if path.join("SKILL.md").exists() {
                let name = path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("skill")
                    .to_string();
                out.push(SkillSource::Directory { root: path, name });
            }
            continue;
        }
        if path.extension().and_then(|s| s.to_str()) == Some("md") {
            let name = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("skill")
                .to_string();
            out.push(SkillSource::LegacyFile { path, name });
        }
    }
    Ok(())
}

fn ensure_namespaced_frontmatter(path: &Path, namespaced: &str, plugin_id: &str) -> Result<()> {
    let contents = read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let (frontmatter, body) = split_frontmatter(&contents);
    let mut mapping = frontmatter.unwrap_or_else(Mapping::new);

    let name_key = YamlValue::String("name".to_string());
    let mut needs_update = true;
    if let Some(YamlValue::String(existing)) = mapping.get(&name_key) {
        if existing == namespaced {
            needs_update = false;
        }
    }
    if needs_update {
        mapping.insert(name_key, YamlValue::String(namespaced.to_string()));
    }

    let plugin_key = YamlValue::String(CLAWDEX_PLUGIN_ID_FRONTMATTER_KEY.to_string());
    let mut plugin_needs_update = true;
    if let Some(YamlValue::String(existing)) = mapping.get(&plugin_key) {
        if existing == plugin_id {
            plugin_needs_update = false;
        }
    }
    if plugin_needs_update {
        mapping.insert(plugin_key, YamlValue::String(plugin_id.to_string()));
    }

    let yaml = serde_yaml::to_string(&mapping).context("serialize skill frontmatter")?;
    let updated = format!("---\n{}---\n{}", yaml, body);
    fs::write(path, updated).with_context(|| format!("write {}", path.display()))?;
    Ok(())
}

fn split_frontmatter(contents: &str) -> (Option<Mapping>, String) {
    let mut lines = contents.lines();
    let first = lines.next().unwrap_or("");
    if first.trim() != "---" {
        return (None, contents.to_string());
    }
    let mut yaml_lines = Vec::new();
    for line in lines.by_ref() {
        if line.trim() == "---" {
            break;
        }
        yaml_lines.push(line);
    }
    let rest = lines.collect::<Vec<&str>>().join("\n");
    let yaml_text = yaml_lines.join("\n");
    let mapping = serde_yaml::from_str::<Mapping>(&yaml_text).ok();
    (mapping, rest)
}

fn render_command_frontmatter(plugin_id: &str, name: &str, description: Option<&str>) -> Result<String> {
    let mut mapping = Mapping::new();
    mapping.insert(
        YamlValue::String("name".to_string()),
        YamlValue::String(name.to_string()),
    );
    mapping.insert(
        YamlValue::String(CLAWDEX_PLUGIN_ID_FRONTMATTER_KEY.to_string()),
        YamlValue::String(plugin_id.to_string()),
    );
    mapping.insert(
        YamlValue::String("disable-model-invocation".to_string()),
        YamlValue::Bool(true),
    );
    mapping.insert(
        YamlValue::String("user-invocable".to_string()),
        YamlValue::Bool(true),
    );
    if let Some(desc) = description {
        if !desc.trim().is_empty() {
            mapping.insert(
                YamlValue::String("description".to_string()),
                YamlValue::String(desc.trim().to_string()),
            );
        }
    }
    serde_yaml::to_string(&mapping).context("serialize command frontmatter")
}

fn collect_command_sources(paths: &[PathBuf]) -> Result<Vec<PathBuf>> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for path in paths {
        if !path.exists() {
            continue;
        }
        if path.is_file() {
            if is_command_file(path) {
                let key = path.to_string_lossy().to_string();
                if seen.insert(key) {
                    out.push(path.to_path_buf());
                }
            }
            continue;
        }
        for entry in WalkDir::new(path).into_iter().filter_map(Result::ok) {
            if !entry.file_type().is_file() {
                continue;
            }
            let entry_path = entry.path();
            if !is_command_file(entry_path) {
                continue;
            }
            let key = entry_path.to_string_lossy().to_string();
            if seen.insert(key) {
                out.push(entry_path.to_path_buf());
            }
        }
    }
    Ok(out)
}

fn is_command_file(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|s| s.to_str()),
        Some("md") | Some("json") | Some("json5")
    )
}

fn load_command_template(path: &Path) -> Result<CommandTemplate> {
    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    let name = |fallback: &str| -> String {
        path.file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or(fallback)
            .to_string()
    };
    if ext == "json" || ext == "json5" {
        let spec = read_command_json(path)?;
        let command_name = spec.name.clone().unwrap_or_else(|| name("command"));
        let template = command_template_from_spec(&spec);
        return Ok(CommandTemplate {
            name: command_name,
            description: spec.description.clone(),
            template,
        });
    }
    let raw = read_to_string(path)?;
    let (frontmatter, body) = split_frontmatter(&raw);

    let fm_name = frontmatter.as_ref().and_then(|mapping| {
        mapping
            .get(&YamlValue::String("name".to_string()))
            .and_then(|value| match value {
                YamlValue::String(s) => Some(s.clone()),
                _ => None,
            })
    });
    let fm_description = frontmatter.as_ref().and_then(|mapping| {
        mapping
            .get(&YamlValue::String("description".to_string()))
            .and_then(|value| match value {
                YamlValue::String(s) => Some(s.clone()),
                _ => None,
            })
    });

    let command_name = fm_name
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| name("command"));

    let description = fm_description
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| extract_description_from_text(if frontmatter.is_some() { &body } else { &raw }));

    // If the file has valid YAML frontmatter, strip it: Codex skills/commands have their own
    // frontmatter and we don't want nested `---` blocks in the rendered prompt.
    let template = if frontmatter.is_some() { body } else { raw };

    Ok(CommandTemplate {
        name: command_name,
        description,
        template,
    })
}

fn command_template_from_spec(spec: &CommandSpec) -> String {
    let base = spec.prompt.clone().unwrap_or_default();
    if let Some(system) = spec.system.as_ref() {
        if base.trim().is_empty() {
            format!("System:\n{}", system)
        } else {
            format!("System:\n{}\n\n{}", system, base)
        }
    } else {
        base
    }
}

fn read_plugin_mcp(root: &Path) -> Result<Option<Value>> {
    let path = root.join(".mcp.json");
    if !path.exists() {
        return Ok(None);
    }
    let raw = read_to_string(&path)?;
    let value = serde_json::from_str(&raw).context("parse .mcp.json")?;
    Ok(Some(value))
}

fn export_plugin_mcp(paths: &ClawdPaths, plugin: &PluginRecord, policy: &McpPolicy) -> Result<()> {
    let root = PathBuf::from(&plugin.path);
    let Some(value) = read_plugin_mcp(&root)? else { return Ok(()) };
    let dest = paths.state_dir.join("mcp").join(format!("{}.json", plugin.id));
    let mut mcp_servers = Map::new();
    let permissions = plugin_permissions_for_root(&root);
    let count =
        merge_mcp_config(&mut mcp_servers, &plugin.id, &value, policy, permissions.as_ref());
    if count == 0 {
        if dest.exists() {
            fs::remove_file(&dest)
                .with_context(|| format!("remove {}", dest.display()))?;
        }
        return Ok(());
    }
    let output = json!({ "mcpServers": mcp_servers });
    write_json_value(&dest, &output)?;
    Ok(())
}

fn normalize_mcp_name(name: &str) -> String {
    name.trim().to_lowercase()
}

fn merge_mcp_config(
    target: &mut Map<String, Value>,
    plugin_id: &str,
    value: &Value,
    policy: &McpPolicy,
    permissions: Option<&PluginPermissions>,
) -> usize {
    if !policy.is_plugin_enabled(plugin_id) {
        return 0;
    }
    let candidate = value
        .get("mcpServers")
        .and_then(|v| v.as_object())
        .or_else(|| value.as_object());
    let Some(servers) = candidate else { return 0 };
    let mut allow_set: Option<HashSet<String>> = None;
    let mut deny_set = HashSet::new();
    if let Some(perms) = permissions {
        if let Some(list) = perms.mcp_deny.as_ref() {
            for entry in list {
                let key = normalize_mcp_name(entry);
                if !key.is_empty() {
                    deny_set.insert(key);
                }
            }
        }
        if let Some(list) = perms.mcp_allow.as_ref() {
            let mut set = HashSet::new();
            for entry in list {
                let key = normalize_mcp_name(entry);
                if !key.is_empty() {
                    set.insert(key);
                }
            }
            if !set.is_empty() {
                allow_set = Some(set);
            }
        }
    }
    let mut inserted = 0usize;
    for (name, config) in servers {
        let key = normalize_mcp_name(name);
        if deny_set.contains(&key) {
            continue;
        }
        if let Some(allow) = allow_set.as_ref() {
            if !allow.contains(&key) {
                continue;
            }
        }
        if !policy.is_allowed(name) {
            continue;
        }
        let key = if target.contains_key(name) {
            format!("{}-{}", plugin_id, name)
        } else {
            name.clone()
        };
        target.insert(key, config.clone());
        inserted += 1;
    }
    inserted
}

fn load_plugin_commands(root: &Path, plugin: &PluginRecord) -> Result<Vec<CommandEntry>> {
    let mut entries = Vec::new();
    let component_paths = resolve_plugin_component_paths(root)?;
    let command_files = collect_command_sources(&component_paths.commands)?;
    for path in command_files {
        let template = load_command_template(&path)?;
        entries.push(CommandEntry {
            plugin_id: plugin.id.clone(),
            plugin_name: plugin.name.clone(),
            command: template.name,
            description: template.description,
            source: path.to_string_lossy().to_string(),
        });
    }
    Ok(entries)
}

pub fn resolve_plugin_command_prompt(
    _paths: &ClawdPaths,
    plugin: &PluginRecord,
    command: &str,
    input: Option<&str>,
    allow_preprocess: bool,
) -> Result<String> {
    let root = PathBuf::from(&plugin.path);
    let component_paths = resolve_plugin_component_paths(&root)?;
    let command_files = collect_command_sources(&component_paths.commands)?;
    let mut selected = None;
    for path in command_files {
        let template = load_command_template(&path)?;
        if template.name == command {
            selected = Some(template);
            break;
        }
    }
    let Some(template) = selected else {
        anyhow::bail!("command not found");
    };

    let rendered = render_command_prompt(
        &template.template,
        input.unwrap_or(""),
        allow_preprocess,
        &root,
    )?;
    if rendered.trim().is_empty() {
        anyhow::bail!("command prompt is empty");
    }
    Ok(rendered)
}

fn render_command_prompt(
    template: &str,
    args: &str,
    allow_preprocess: bool,
    plugin_root: &Path,
) -> Result<String> {
    let args_str = args.trim();
    let mut legacy_used = false;
    let mut working = template.to_string();
    if working.contains("{{input}}") {
        working = working.replace("{{input}}", args_str);
        legacy_used = true;
    }
    let (mut rendered, used_args) = apply_argument_substitutions(&working, args_str);
    let used_args = used_args || legacy_used;

    if allow_preprocess {
        rendered = apply_preprocess_commands(&rendered, plugin_root)?;
    } else if contains_preprocess_command(&rendered) {
        anyhow::bail!("preprocess commands are disabled");
    }

    // Claude Code command packs commonly refer to assets under `~/.claude/...` because that's the
    // default install root. When we run them as Clawdex plugins, the plugin install directory is
    // the equivalent root, so rewrite those references onto `${CLAUDE_PLUGIN_ROOT}`.
    rendered = rewrite_claude_code_paths(&rendered);

    if !used_args && !args_str.is_empty() {
        if !rendered.trim().is_empty() {
            rendered.push_str("\n\n");
        }
        rendered.push_str(args_str);
    }

    let plugin_root_str = plugin_root.to_string_lossy();
    if rendered.contains("${CLAUDE_PLUGIN_ROOT}") {
        rendered = rendered.replace("${CLAUDE_PLUGIN_ROOT}", &plugin_root_str);
    }

    Ok(rendered)
}

fn rewrite_claude_code_paths(template: &str) -> String {
    template
        .replace("~/.claude/", "${CLAUDE_PLUGIN_ROOT}/")
        .replace("~/.claude", "${CLAUDE_PLUGIN_ROOT}")
}

fn apply_argument_substitutions(template: &str, args_str: &str) -> (String, bool) {
    let args = split_arguments(args_str);
    let mut out = String::new();
    let mut used = false;
    let chars: Vec<char> = template.chars().collect();
    let mut i = 0usize;
    while i < chars.len() {
        if chars[i] != '$' {
            out.push(chars[i]);
            i += 1;
            continue;
        }

        if matches_sequence(&chars, i + 1, "ARGUMENTS") {
            let idx = i + "ARGUMENTS".len() + 1;
            if idx < chars.len() && chars[idx] == '[' {
                if let Some((value, next_idx)) = parse_indexed_argument(&args, &chars, idx + 1) {
                    out.push_str(&value);
                    used = true;
                    i = next_idx;
                    continue;
                }
            }
            out.push_str(args_str);
            used = true;
            i = i + "ARGUMENTS".len() + 1;
            continue;
        }

        if let Some((value, next_idx)) = parse_numeric_argument(&args, &chars, i + 1) {
            out.push_str(&value);
            used = true;
            i = next_idx;
            continue;
        }

        out.push('$');
        i += 1;
    }
    (out, used)
}

fn matches_sequence(chars: &[char], start: usize, sequence: &str) -> bool {
    let seq: Vec<char> = sequence.chars().collect();
    if start + seq.len() > chars.len() {
        return false;
    }
    chars[start..start + seq.len()] == seq[..]
}

fn parse_indexed_argument(args: &[String], chars: &[char], start: usize) -> Option<(String, usize)> {
    let mut idx = start;
    let mut digits = String::new();
    while idx < chars.len() {
        let ch = chars[idx];
        if ch == ']' {
            if digits.is_empty() {
                return None;
            }
            let value = digits.parse::<usize>().ok()?;
            let replacement = args.get(value).cloned().unwrap_or_default();
            return Some((replacement, idx + 1));
        }
        if !ch.is_ascii_digit() {
            return None;
        }
        digits.push(ch);
        idx += 1;
    }
    None
}

fn parse_numeric_argument(args: &[String], chars: &[char], start: usize) -> Option<(String, usize)> {
    if start >= chars.len() || !chars[start].is_ascii_digit() {
        return None;
    }
    let mut idx = start;
    let mut digits = String::new();
    while idx < chars.len() && chars[idx].is_ascii_digit() {
        digits.push(chars[idx]);
        idx += 1;
    }
    let value = digits.parse::<usize>().ok()?;
    let replacement = args.get(value).cloned().unwrap_or_default();
    Some((replacement, idx))
}

fn split_arguments(raw: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;

    for ch in raw.chars() {
        if escaped {
            current.push(ch);
            escaped = false;
            continue;
        }
        if ch == '\\' && !in_single {
            escaped = true;
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            continue;
        }
        if ch.is_whitespace() && !in_single && !in_double {
            if !current.is_empty() {
                out.push(current.clone());
                current.clear();
            }
            continue;
        }
        current.push(ch);
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}

fn contains_preprocess_command(template: &str) -> bool {
    template.lines().any(|line| is_preprocess_line(line))
}

fn apply_preprocess_commands(template: &str, plugin_root: &Path) -> Result<String> {
    let mut output = String::new();
    for line in template.lines() {
        if is_preprocess_line(line) {
            let cmd = line.trim_start();
            let cmd = cmd.trim_start_matches('!').trim();
            if cmd.is_empty() {
                continue;
            }
            let result = run_preprocess_command(cmd, plugin_root)?;
            output.push_str(&result);
            if !result.ends_with('\n') {
                output.push('\n');
            }
        } else {
            output.push_str(line);
            output.push('\n');
        }
    }
    if output.ends_with('\n') {
        output.pop();
    }
    Ok(output)
}

fn is_preprocess_line(line: &str) -> bool {
    let trimmed = line.trim_start();
    trimmed.starts_with('!') && trimmed.len() > 1
}

fn run_preprocess_command(command: &str, plugin_root: &Path) -> Result<String> {
    let mut cmd = if cfg!(windows) {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(command);
        c
    } else {
        let mut c = Command::new("sh");
        c.arg("-c").arg(command);
        c
    };
    cmd.current_dir(plugin_root);
    cmd.env(
        "CLAUDE_PLUGIN_ROOT",
        plugin_root.to_string_lossy().to_string(),
    );
    let output = cmd.output().with_context(|| format!("run preprocess command: {command}"))?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("preprocess command failed: {command}\n{stderr}");
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn read_command_json(path: &Path) -> Result<CommandSpec> {
    let raw = read_to_string(path)?;
    if path.extension().and_then(|s| s.to_str()) == Some("json5") {
        json5::from_str(&raw).context("parse command json5")
    } else {
        serde_json::from_str(&raw).context("parse command json")
    }
}

fn extract_description_from_text(raw: &str) -> Option<String> {
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed == "---" {
            continue;
        }
        return Some(trimmed.to_string());
    }
    None
}

fn copy_dir(src: &Path, dest: &Path) -> Result<()> {
    ensure_dir(dest)?;
    for entry in WalkDir::new(src).into_iter().filter_map(Result::ok) {
        let rel = entry.path().strip_prefix(src).unwrap_or(entry.path());
        let target = dest.join(rel);
        if entry.file_type().is_dir() {
            ensure_dir(&target)?;
        } else if entry.file_type().is_file() {
            if let Some(parent) = target.parent() {
                ensure_dir(parent)?;
            }
            fs::copy(entry.path(), &target).with_context(|| {
                format!("copy {} -> {}", entry.path().display(), target.display())
            })?;
        }
    }
    Ok(())
}

fn slugify(value: &str) -> String {
    let mut out = String::new();
    let mut last_dash = false;
    for ch in value.chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() {
            out.push(lower);
            last_dash = false;
        } else if !last_dash {
            out.push('-');
            last_dash = true;
        }
    }
    while out.starts_with('-') {
        out.remove(0);
    }
    while out.ends_with('-') {
        out.pop();
    }
    if out.is_empty() {
        "plugin".to_string()
    } else {
        out
    }
}

fn resolve_codex_path(cfg: &crate::config::ClawdConfig, override_path: Option<PathBuf>) -> Result<PathBuf> {
    if let Some(path) = override_path {
        return Ok(path);
    }
    if let Ok(env) = std::env::var("CLAWDEX_CODEX_PATH") {
        if !env.trim().is_empty() {
            return Ok(PathBuf::from(env));
        }
    }
    if let Some(codex) = cfg.codex.as_ref().and_then(|c| c.path.as_ref()) {
        return Ok(PathBuf::from(codex));
    }
    Ok(PathBuf::from("codex"))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(prefix: &str) -> Self {
            let path = create_temp_dir(prefix).expect("create temp dir");
            Self { path }
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn load_command_template_parses_yaml_frontmatter_and_strips_it() {
        let tmp = TempDir::new("clawdex-test-");
        let path = tmp.path.join("new-project.md");
        fs::write(
            &path,
            r#"---
name: gsd:new-project
description: Initialize a new project
allowed-tools:
  - Read
---
<objective>
Do the thing.
</objective>
"#,
        )
        .expect("write template");

        let template = load_command_template(&path).expect("load template");
        assert_eq!(template.name, "gsd:new-project");
        assert_eq!(template.description.as_deref(), Some("Initialize a new project"));
        assert!(
            !template.template.contains("name: gsd:new-project"),
            "expected command frontmatter to be stripped"
        );
        assert!(template.template.contains("<objective>"));
    }

    #[test]
    fn rewrite_claude_code_paths_maps_tilde_claude_to_plugin_root_placeholder() {
        let input = "A @~/.claude/get-shit-done/workflows/new-project.md B ~/.claude/agents/x.md";
        let output = rewrite_claude_code_paths(input);
        assert!(output.contains("@${CLAUDE_PLUGIN_ROOT}/get-shit-done/workflows/new-project.md"));
        assert!(output.contains("${CLAUDE_PLUGIN_ROOT}/agents/x.md"));
    }

    #[test]
    fn maybe_rewrite_claude_code_install_paths_rewrites_files_in_tree() {
        let tmp = TempDir::new("clawdex-test-");
        let file = tmp.path.join("example.md");
        fs::write(
            &file,
            "node ~/.claude/get-shit-done/bin/gsd-tools.js init new-project\n",
        )
        .expect("write");

        maybe_rewrite_claude_code_install_paths(&tmp.path, "get-shit-done").expect("rewrite");
        let updated = read_to_string(&file).expect("read updated");
        let expected_prefix = format!("node {}/get-shit-done/bin/gsd-tools.js", tmp.path.display());
        assert!(
            updated.contains(&expected_prefix),
            "expected rewrite to substitute plugin root: {updated}"
        );
    }

    #[test]
    fn list_bundled_claude_plugins_includes_command_only_bundle() {
        let tmp = TempDir::new("clawdex-test-");
        let root = tmp.path.join("root");
        ensure_dir(&root).expect("mkdir root");

        let plugin_dir = root.join("cmd-pack");
        ensure_dir(plugin_dir.join("commands").as_path()).expect("mkdir commands");
        fs::write(plugin_dir.join("commands").join("hello.md"), "hello").expect("write command");

        let plugins = list_bundled_claude_plugins(&root).expect("list plugins");
        assert_eq!(plugins, vec![plugin_dir]);
    }

    #[test]
    fn remove_plugin_skills_removes_colon_named_command_skill_dirs() {
        let tmp = TempDir::new("clawdex-test-");
        let state_dir = tmp.path.as_path();

        let overlay_root = state_dir
            .join("codex")
            .join("skills")
            .join("_clawdex_plugins");
        ensure_dir(&overlay_root).expect("mkdir overlay");

        let legacy_colon_skill = overlay_root.join("gsd:join-discord");
        ensure_dir(&legacy_colon_skill).expect("mkdir gsd skill");
        fs::write(
            legacy_colon_skill.join("SKILL.md"),
            "---\nname: gsd:join-discord\ndescription: Join the GSD Discord community\n---\nbody\n",
        )
        .expect("write legacy skill");

        let namespaced_skill = overlay_root.join("get-shit-done:some-skill");
        ensure_dir(&namespaced_skill).expect("mkdir namespaced skill");
        fs::write(
            namespaced_skill.join("SKILL.md"),
            "---\nname: get-shit-done:some-skill\ndescription: Some skill\n---\n",
        )
        .expect("write namespaced skill");

        let other_skill = overlay_root.join("other:skill");
        ensure_dir(&other_skill).expect("mkdir other skill");
        fs::write(
            other_skill.join("SKILL.md"),
            "---\nname: other:skill\ndescription: Other\n---\n",
        )
        .expect("write other skill");

        // Mirror the on-disk layout used by clawdex installs (`$STATE_DIR/plugins/<id>/commands/...`).
        let plugin_root = state_dir.join("plugins").join("get-shit-done");
        let commands_dir = plugin_root.join("commands").join("gsd");
        ensure_dir(&commands_dir).expect("mkdir commands");
        fs::write(
            commands_dir.join("join-discord.md"),
            "---\nname: gsd:join-discord\ndescription: Join the GSD Discord community\n---\nHello\n",
        )
        .expect("write command template");

        remove_plugin_skills_in_state_dir(state_dir, "get-shit-done").expect("remove skills");

        assert!(
            !legacy_colon_skill.exists(),
            "expected legacy colon-named skill dir to be removed"
        );
        assert!(
            !namespaced_skill.exists(),
            "expected namespaced skill dir to be removed"
        );
        assert!(other_skill.exists(), "expected other skill to remain");
    }

    #[test]
    fn remove_plugin_skills_removes_marker_owned_skill_dirs_without_plugin_root() {
        let tmp = TempDir::new("clawdex-test-");
        let state_dir = tmp.path.as_path();

        let overlay_root = state_dir
            .join("codex")
            .join("skills")
            .join("_clawdex_plugins");
        ensure_dir(&overlay_root).expect("mkdir overlay");

        let marker_skill = overlay_root.join("gsd:join-discord");
        ensure_dir(&marker_skill).expect("mkdir skill");
        fs::write(
            marker_skill.join("SKILL.md"),
            format!(
                "---\nname: gsd:join-discord\n{CLAWDEX_PLUGIN_ID_FRONTMATTER_KEY}: get-shit-done\ndescription: Join the GSD Discord community\n---\n"
            ),
        )
        .expect("write skill");

        remove_plugin_skills_in_state_dir(state_dir, "get-shit-done").expect("remove skills");

        assert!(
            !marker_skill.exists(),
            "expected marker-owned skill dir to be removed"
        );
    }
}
