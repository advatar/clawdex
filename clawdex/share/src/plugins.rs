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
use crate::config::{load_config, resolve_mcp_policy, ClawdPaths, McpPolicy, WorkspacePolicy};
use crate::runner::workspace_sandbox_policy;
use crate::task_db::{PluginRecord, TaskStore};
use crate::util::{ensure_dir, home_dir, now_ms, read_to_string, write_json_value};

const COWORK_MANIFEST_PATH: &str = ".claude-plugin/plugin.json";
const OPENCLAW_MANIFEST_FILENAME: &str = "openclaw.plugin.json";
const INSTALLS_FILE: &str = "installs.json";

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

    anyhow::bail!("plugin manifest not found")
}

fn plugin_permissions_for_root(root: &Path) -> Option<PluginPermissions> {
    load_plugin_manifest(root).ok().map(|manifest| manifest.permissions)
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
    let sandbox_policy = workspace_sandbox_policy(&paths.workspace_policy)?;

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
    path: PathBuf,
    link: bool,
    source: Option<String>,
    state_dir: Option<PathBuf>,
    workspace: Option<PathBuf>,
) -> Result<Value> {
    let (cfg, paths) = load_config(state_dir, workspace)?;
    let store = TaskStore::open(&paths)?;
    let policy = resolve_mcp_policy(&cfg);
    let plugin = install_plugin(&paths, &store, &path, link, source, &policy)?;
    let assets = plugin_assets(Path::new(&plugin.path));
    Ok(json!({ "plugin": plugin, "assets": assets }))
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
        let count = merge_mcp_config(&mut mcp_servers, &plugin.id, &mcp_value, &policy);
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

fn install_plugin(
    paths: &ClawdPaths,
    store: &TaskStore,
    plugin_dir: &Path,
    link: bool,
    source: Option<String>,
    policy: &McpPolicy,
) -> Result<PluginRecord> {
    let manifest = read_manifest(plugin_dir)?;
    let raw_id = manifest
        .id
        .clone()
        .or_else(|| manifest.name.clone())
        .or_else(|| plugin_dir.file_name().and_then(|s| s.to_str()).map(|s| s.to_string()))
        .context("plugin id not found")?;
    let plugin_id = slugify(&raw_id);
    let name = manifest
        .name
        .clone()
        .unwrap_or_else(|| raw_id.clone());
    let version = manifest.version.clone();
    let description = manifest.description.clone();

    let root = plugin_root(paths, &plugin_id);
    if root.exists() {
        fs::remove_dir_all(&root).with_context(|| format!("remove {}", root.display()))?;
    }
    if let Some(parent) = root.parent() {
        ensure_dir(parent)?;
    }

    if link {
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(plugin_dir, &root)
                .with_context(|| format!("symlink {}", root.display()))?;
        }
        #[cfg(not(unix))]
        {
            anyhow::bail!("--link is only supported on unix platforms");
        }
    } else {
        copy_dir(plugin_dir, &root)?;
    }

    let now = now_ms();
    let plugin = PluginRecord {
        id: plugin_id.clone(),
        name,
        version,
        description,
        source: source.or_else(|| Some(plugin_dir.to_string_lossy().to_string())),
        path: root.to_string_lossy().to_string(),
        enabled: true,
        installed_at_ms: now,
        updated_at_ms: now,
    };
    store.upsert_plugin(&plugin)?;
    sync_plugin_skills(paths, &plugin, policy)?;
    Ok(plugin)
}

fn read_manifest(plugin_dir: &Path) -> Result<ClaudePluginManifest> {
    let path = plugin_dir.join(".claude-plugin").join("plugin.json");
    let raw = read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    serde_json::from_str(&raw).context("parse plugin.json")
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

fn codex_home_dir() -> Result<PathBuf> {
    if let Ok(env) = std::env::var("CODEX_HOME") {
        if !env.trim().is_empty() {
            return Ok(PathBuf::from(env));
        }
    }
    Ok(home_dir()?.join(".codex"))
}

fn plugin_overlay_root() -> Result<PathBuf> {
    Ok(codex_home_dir()?.join("skills").join("_clawdex_plugins"))
}

fn legacy_plugin_skill_root(plugin_id: &str) -> Result<PathBuf> {
    Ok(codex_home_dir()?
        .join("skills")
        .join("clawdex")
        .join("plugins")
        .join(plugin_id))
}

fn sync_plugin_skills(paths: &ClawdPaths, plugin: &PluginRecord, policy: &McpPolicy) -> Result<()> {
    let root = PathBuf::from(&plugin.path);
    remove_plugin_skills(paths, &plugin.id)?;

    let component_paths = resolve_plugin_component_paths(&root)?;
    let skill_sources = collect_skill_sources(&component_paths.skills)?;
    let command_sources = collect_command_sources(&component_paths.commands)?;

    let overlay_root = plugin_overlay_root()?;
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
        ensure_namespaced_frontmatter(&skill_md, &namespaced)?;
    }

    let mut command_names = HashSet::new();
    for path in command_sources {
        let template = load_command_template(&path)?;
        if template.template.trim().is_empty() {
            continue;
        }
        let namespaced = format!("{}:{}", plugin.id, template.name);
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
        let yaml = render_command_frontmatter(&namespaced, template.description.as_deref())?;
        let contents = format!("---\n{}---\n{}", yaml, template.template);
        fs::write(dest.join("SKILL.md"), contents)
            .with_context(|| format!("write {}", dest.display()))?;
    }

    let _ = export_plugin_mcp(paths, plugin, policy);
    Ok(())
}

fn remove_plugin_skills(paths: &ClawdPaths, plugin_id: &str) -> Result<()> {
    let overlay_root = plugin_overlay_root()?;
    if overlay_root.exists() {
        let prefix = format!("{plugin_id}:");
        for entry in fs::read_dir(&overlay_root)
            .with_context(|| format!("read {}", overlay_root.display()))?
        {
            let entry = entry?;
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else { continue };
            if !name.starts_with(&prefix) {
                continue;
            }
            fs::remove_dir_all(&path)
                .with_context(|| format!("remove {}", path.display()))?;
        }
    }
    let legacy_root = legacy_plugin_skill_root(plugin_id)?;
    if legacy_root.exists() {
        fs::remove_dir_all(&legacy_root)
            .with_context(|| format!("remove {}", legacy_root.display()))?;
    }
    let mcp_path = paths.state_dir.join("mcp").join(format!("{plugin_id}.json"));
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

fn ensure_namespaced_frontmatter(path: &Path, namespaced: &str) -> Result<()> {
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

fn render_command_frontmatter(name: &str, description: Option<&str>) -> Result<String> {
    let mut mapping = Mapping::new();
    mapping.insert(
        YamlValue::String("name".to_string()),
        YamlValue::String(name.to_string()),
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
    let template = read_to_string(path)?;
    Ok(CommandTemplate {
        name: name("command"),
        description: extract_description(path).ok(),
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
    let count = merge_mcp_config(&mut mcp_servers, &plugin.id, &value, policy);
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

fn merge_mcp_config(
    target: &mut Map<String, Value>,
    plugin_id: &str,
    value: &Value,
    policy: &McpPolicy,
) -> usize {
    if !policy.is_plugin_enabled(plugin_id) {
        return 0;
    }
    let candidate = value
        .get("mcpServers")
        .and_then(|v| v.as_object())
        .or_else(|| value.as_object());
    let Some(servers) = candidate else { return 0 };
    let mut inserted = 0usize;
    for (name, config) in servers {
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

fn extract_description(path: &Path) -> Result<String> {
    let raw = read_to_string(path)?;
    for line in raw.lines() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        return Ok(trimmed.to_string());
    }
    Ok("".to_string())
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
