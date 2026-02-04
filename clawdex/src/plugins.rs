use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Map, Value};
use serde_yaml::{Mapping, Value as YamlValue};
use walkdir::WalkDir;

use crate::config::{load_config, ClawdPaths};
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

pub fn list_plugins_command(
    state_dir: Option<PathBuf>,
    workspace: Option<PathBuf>,
    include_disabled: bool,
) -> Result<Value> {
    let (_cfg, paths) = load_config(state_dir, workspace)?;
    let store = TaskStore::open(&paths)?;
    let plugins = store.list_plugins(include_disabled)?;
    let items: Vec<Value> = plugins
        .into_iter()
        .map(|plugin| {
            let assets = plugin_assets(Path::new(&plugin.path));
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
            })
        })
        .collect();
    Ok(json!({ "plugins": items }))
}

pub fn add_plugin_command(
    path: PathBuf,
    link: bool,
    source: Option<String>,
    state_dir: Option<PathBuf>,
    workspace: Option<PathBuf>,
) -> Result<Value> {
    let (_cfg, paths) = load_config(state_dir, workspace)?;
    let store = TaskStore::open(&paths)?;
    let plugin = install_plugin(&paths, &store, &path, link, source)?;
    let assets = plugin_assets(Path::new(&plugin.path));
    Ok(json!({ "plugin": plugin, "assets": assets }))
}

pub fn enable_plugin_command(
    plugin_id: &str,
    state_dir: Option<PathBuf>,
    workspace: Option<PathBuf>,
) -> Result<Value> {
    let (_cfg, paths) = load_config(state_dir, workspace)?;
    let store = TaskStore::open(&paths)?;
    let plugin = store
        .set_plugin_enabled(plugin_id, true)?
        .context("plugin not found")?;
    sync_plugin_skills(&paths, &plugin)?;
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
    let (_cfg, paths) = load_config(state_dir, workspace)?;
    let store = TaskStore::open(&paths)?;
    let plugins = store.list_plugins(true)?;
    let mut synced = Vec::new();
    for plugin in plugins {
        if plugin.enabled {
            sync_plugin_skills(&paths, &plugin)?;
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
    let (_cfg, paths) = load_config(state_dir, workspace)?;
    let store = TaskStore::open(&paths)?;
    let plugins = store.list_plugins(false)?;
    let mut mcp_servers = Map::new();
    let mut included = Vec::new();

    for plugin in plugins {
        let root = PathBuf::from(&plugin.path);
        let Some(mcp_value) = read_plugin_mcp(&root)? else { continue };
        let count = merge_mcp_config(&mut mcp_servers, &plugin.id, &mcp_value);
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
    sync_plugin_skills(paths, &plugin)?;
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

fn sync_plugin_skills(paths: &ClawdPaths, plugin: &PluginRecord) -> Result<()> {
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

    let _ = export_plugin_mcp(paths, plugin);
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

fn export_plugin_mcp(paths: &ClawdPaths, plugin: &PluginRecord) -> Result<()> {
    let root = PathBuf::from(&plugin.path);
    let Some(value) = read_plugin_mcp(&root)? else { return Ok(()) };
    let dest = paths.state_dir.join("mcp").join(format!("{}.json", plugin.id));
    write_json_value(&dest, &value)?;
    Ok(())
}

fn merge_mcp_config(target: &mut Map<String, Value>, plugin_id: &str, value: &Value) -> usize {
    let candidate = value
        .get("mcpServers")
        .and_then(|v| v.as_object())
        .or_else(|| value.as_object());
    let Some(servers) = candidate else { return 0 };
    let mut inserted = 0usize;
    for (name, config) in servers {
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
