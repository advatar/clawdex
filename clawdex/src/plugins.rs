use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use serde_yaml::{Mapping, Value as YamlValue};
use walkdir::WalkDir;

use crate::app_server::{ApprovalMode, CodexClient};
use crate::config::{load_config, resolve_mcp_policy, ClawdPaths, McpPolicy};
use crate::runner::workspace_sandbox_policy;
use crate::task_db::{PluginRecord, TaskStore};
use crate::util::{ensure_dir, home_dir, now_ms, read_to_string, write_json_value};

#[derive(Debug, Deserialize)]
struct PluginManifest {
    id: Option<String>,
    name: Option<String>,
    version: Option<String>,
    description: Option<String>,
}

#[derive(Debug, Default, Serialize)]
struct PluginAssets {
    skills: usize,
    commands: usize,
    has_mcp: bool,
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

pub fn list_plugins_command(
    state_dir: Option<PathBuf>,
    workspace: Option<PathBuf>,
    include_disabled: bool,
) -> Result<Value> {
    let (cfg, paths) = load_config(state_dir, workspace)?;
    let policy = resolve_mcp_policy(&cfg);
    let store = TaskStore::open(&paths)?;
    let plugins = store.list_plugins(include_disabled)?;
    let items: Vec<Value> = plugins
        .into_iter()
        .map(|plugin| {
            let assets = plugin_assets(Path::new(&plugin.path));
            let mcp_enabled = assets.has_mcp && policy.is_plugin_enabled(&plugin.id);
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

    let prompt = resolve_plugin_command_prompt(&paths, &plugin, command, input.as_deref())?;
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

fn read_manifest(plugin_dir: &Path) -> Result<PluginManifest> {
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
        for entry in WalkDir::new(&skills_dir)
            .into_iter()
            .filter_map(Result::ok)
        {
            if entry.file_type().is_file() {
                if entry.path().extension().and_then(|s| s.to_str()) == Some("md") {
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

fn plugin_skill_root(plugin_id: &str) -> Result<PathBuf> {
    Ok(codex_home_dir()?
        .join("skills")
        .join("clawdex")
        .join("plugins")
        .join(plugin_id))
}

fn sync_plugin_skills(paths: &ClawdPaths, plugin: &PluginRecord, policy: &McpPolicy) -> Result<()> {
    let root = PathBuf::from(&plugin.path);
    let skills_dir = root.join("skills");
    let dest_root = plugin_skill_root(&plugin.id)?;
    if !skills_dir.exists() {
        if dest_root.exists() {
            fs::remove_dir_all(&dest_root)
                .with_context(|| format!("remove {}", dest_root.display()))?;
        }
        return Ok(());
    }

    if dest_root.exists() {
        fs::remove_dir_all(&dest_root)
            .with_context(|| format!("remove {}", dest_root.display()))?;
    }
    ensure_dir(&dest_root)?;

    for entry in WalkDir::new(&skills_dir)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        if entry.path().extension().and_then(|s| s.to_str()) != Some("md") {
            continue;
        }
        let rel = entry
            .path()
            .strip_prefix(&skills_dir)
            .unwrap_or(entry.path());
        let mut skill_dir = dest_root.join(rel);
        skill_dir.set_extension("");
        ensure_dir(&skill_dir)?;

        let body = read_to_string(entry.path())?;
        let skill_name = rel
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("Skill");
        let display_name = format!("{} / {}", plugin.name, skill_name);
        let description = plugin
            .description
            .clone()
            .unwrap_or_else(|| format!("{} plugin skill", plugin.name));
        let yaml = render_skill_frontmatter(&display_name, &description, &plugin.id)?;
        let contents = format!("---\n{}---\n{}", yaml, body);
        fs::write(skill_dir.join("SKILL.md"), contents)?;
    }

    let _ = export_plugin_mcp(paths, plugin, policy);
    Ok(())
}

fn remove_plugin_skills(paths: &ClawdPaths, plugin_id: &str) -> Result<()> {
    let dest_root = plugin_skill_root(plugin_id)?;
    if dest_root.exists() {
        fs::remove_dir_all(&dest_root)
            .with_context(|| format!("remove {}", dest_root.display()))?;
    }
    let mcp_path = paths.state_dir.join("mcp").join(format!("{plugin_id}.json"));
    if mcp_path.exists() {
        fs::remove_file(&mcp_path)
            .with_context(|| format!("remove {}", mcp_path.display()))?;
    }
    Ok(())
}

fn render_skill_frontmatter(name: &str, description: &str, plugin_id: &str) -> Result<String> {
    let mut mapping = Mapping::new();
    mapping.insert(
        YamlValue::String("name".to_string()),
        YamlValue::String(name.to_string()),
    );
    mapping.insert(
        YamlValue::String("description".to_string()),
        YamlValue::String(description.to_string()),
    );
    mapping.insert(
        YamlValue::String("plugin".to_string()),
        YamlValue::String(plugin_id.to_string()),
    );
    serde_yaml::to_string(&mapping).context("serialize skill frontmatter")
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
    let commands_dir = root.join("commands");
    if !commands_dir.exists() {
        return Ok(entries);
    }
    for entry in WalkDir::new(&commands_dir).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
        if ext == "json" || ext == "json5" {
            if let Ok(spec) = read_command_json(path) {
                if let Some(command) = spec
                    .name
                    .clone()
                    .or_else(|| path.file_stem().and_then(|s| s.to_str()).map(|s| s.to_string()))
                {
                    entries.push(CommandEntry {
                        plugin_id: plugin.id.clone(),
                        plugin_name: plugin.name.clone(),
                        command,
                        description: spec.description.clone(),
                        source: path.to_string_lossy().to_string(),
                    });
                }
            }
        } else if ext == "md" {
            let command = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("command")
                .to_string();
            let description = extract_description(path).ok();
            entries.push(CommandEntry {
                plugin_id: plugin.id.clone(),
                plugin_name: plugin.name.clone(),
                command,
                description,
                source: path.to_string_lossy().to_string(),
            });
        }
    }
    Ok(entries)
}

pub fn resolve_plugin_command_prompt(
    _paths: &ClawdPaths,
    plugin: &PluginRecord,
    command: &str,
    input: Option<&str>,
) -> Result<String> {
    let root = PathBuf::from(&plugin.path);
    let commands_dir = root.join("commands");
    if !commands_dir.exists() {
        anyhow::bail!("plugin has no commands directory");
    }
    let mut candidates = Vec::new();
    for entry in WalkDir::new(&commands_dir).into_iter().filter_map(Result::ok) {
        if !entry.file_type().is_file() {
            continue;
        }
        let path = entry.path();
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if stem != command {
            continue;
        }
        candidates.push(path.to_path_buf());
    }
    let path = candidates
        .into_iter()
        .next()
        .context("command not found")?;

    let ext = path.extension().and_then(|s| s.to_str()).unwrap_or("");
    let mut prompt = if ext == "json" || ext == "json5" {
        let spec = read_command_json(&path)?;
        let base = spec
            .prompt
            .clone()
            .unwrap_or_else(|| "".to_string());
        if let Some(system) = spec.system {
            format!("System:\n{}\n\n{}", system, base)
        } else {
            base
        }
    } else {
        read_to_string(&path)?
    };

    if let Some(input) = input {
        if prompt.contains("{{input}}") {
            prompt = prompt.replace("{{input}}", input);
        } else {
            prompt.push_str("\n\nUser input:\n");
            prompt.push_str(input);
        }
    }
    if prompt.trim().is_empty() {
        anyhow::bail!("command prompt is empty");
    }
    Ok(prompt)
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
