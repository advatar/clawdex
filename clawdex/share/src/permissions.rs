use std::path::PathBuf;

use anyhow::Result;
use serde_json::{json, Map, Value};

use crate::config::{
    load_config, merge_config_value, read_config_value, write_config_value, ClawdConfig,
};

pub struct PermissionsUpdate {
    pub internet: Option<bool>,
    pub read_only: Option<bool>,
    pub mcp_allow: Option<Vec<String>>,
    pub mcp_deny: Option<Vec<String>>,
    pub mcp_plugins: Option<Vec<(String, bool)>>,
}

pub fn get_permissions_command(
    state_dir: Option<PathBuf>,
    workspace: Option<PathBuf>,
) -> Result<Value> {
    let (cfg, paths) = load_config(state_dir, workspace)?;
    let permissions = cfg.permissions.as_ref();
    let mcp = permissions.and_then(|p| p.mcp.as_ref());

    Ok(json!({
        "stateDir": paths.state_dir.to_string_lossy(),
        "internet": permissions.and_then(|p| p.internet).unwrap_or(true),
        "readOnly": cfg.workspace_policy.as_ref().and_then(|p| p.read_only).unwrap_or(false),
        "mcp": {
            "allow": mcp.and_then(|m| m.allow.clone()).unwrap_or_default(),
            "deny": mcp.and_then(|m| m.deny.clone()).unwrap_or_default(),
            "plugins": mcp.and_then(|m| m.plugins.clone()).unwrap_or_default()
        }
    }))
}

pub fn set_permissions_command(
    update: PermissionsUpdate,
    state_dir: Option<PathBuf>,
    workspace: Option<PathBuf>,
) -> Result<Value> {
    let (_cfg, paths) = load_config(state_dir, workspace)?;
    let mut patch = Map::new();

    if let Some(internet) = update.internet {
        patch.insert(
            "permissions".to_string(),
            json!({ "internet": internet }),
        );
    }

    if update.mcp_allow.is_some() || update.mcp_deny.is_some() {
        let mut mcp_patch = Map::new();
        if let Some(allow) = update.mcp_allow {
            mcp_patch.insert("allow".to_string(), json!(allow));
        }
        if let Some(deny) = update.mcp_deny {
            mcp_patch.insert("deny".to_string(), json!(deny));
        }
        let mut permissions_patch = Map::new();
        permissions_patch.insert("mcp".to_string(), Value::Object(mcp_patch));
        let entry = patch
            .entry("permissions".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        merge_config_value(entry, &Value::Object(permissions_patch));
    }

    if let Some(plugin_overrides) = update.mcp_plugins {
        let mut plugins_map = Map::new();
        for (plugin_id, enabled) in plugin_overrides {
            plugins_map.insert(plugin_id, Value::Bool(enabled));
        }
        let mut mcp_patch = Map::new();
        mcp_patch.insert("plugins".to_string(), Value::Object(plugins_map));
        let mut permissions_patch = Map::new();
        permissions_patch.insert("mcp".to_string(), Value::Object(mcp_patch));
        let entry = patch
            .entry("permissions".to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        merge_config_value(entry, &Value::Object(permissions_patch));
    }

    if let Some(read_only) = update.read_only {
        patch.insert(
            "workspace_policy".to_string(),
            json!({ "read_only": read_only }),
        );
    }

    let mut value = read_config_value(&paths.state_dir)?;
    merge_config_value(&mut value, &Value::Object(patch));
    let _ = serde_json::from_value::<ClawdConfig>(value.clone())
        .map_err(|err| anyhow::anyhow!("invalid config update: {err}"))?;
    let path = write_config_value(&paths.state_dir, &value)?;

    Ok(json!({
        "ok": true,
        "configPath": path.to_string_lossy(),
        "config": value
    }))
}

pub fn parse_csv_list(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

pub fn parse_on_off(raw: &str) -> Result<bool> {
    match raw.trim().to_lowercase().as_str() {
        "1" | "true" | "on" | "yes" => Ok(true),
        "0" | "false" | "off" | "no" => Ok(false),
        _ => Err(anyhow::anyhow!("expected on/off/true/false")),
    }
}

pub fn parse_plugin_toggle(raw: &str) -> Result<(String, bool)> {
    let mut parts = raw.splitn(2, '=');
    let name = parts.next().unwrap_or("").trim();
    let value = parts.next().unwrap_or("").trim();
    if name.is_empty() || value.is_empty() {
        return Err(anyhow::anyhow!("expected <pluginId>=<on|off>"));
    }
    Ok((name.to_string(), parse_on_off(value)?))
}
