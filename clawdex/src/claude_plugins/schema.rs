//! Claude Code plugin / marketplace schemas for Clawdex
//!
//! Goal: parse *real-world* Claude plugin.json + marketplace.json with forward compatibility.
//!
//! References:
//! - plugin.json schema: https://code.claude.com/docs/en/plugins-reference
//! - marketplace.json schema: https://code.claude.com/docs/en/plugin-marketplaces
//!
//! Design notes:
//! - Many fields are `string | array` or `string | array | object`.
//! - We keep `raw` JSON for unknown future fields (serde(flatten)).
//! - We preserve “inline vs file” variants (hooks/mcp/lsp).

#![allow(non_snake_case)]

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Generic helper for fields that are either a single value or a list.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum OneOrMany<T> {
    One(T),
    Many(Vec<T>),
}

impl<T> OneOrMany<T> {
    pub fn as_slice(&self) -> &[T] {
        match self {
            OneOrMany::One(v) => std::slice::from_ref(v),
            OneOrMany::Many(v) => v.as_slice(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Author {
    pub name: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudePluginManifest {
    pub name: String,

    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub author: Option<Author>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub keywords: Option<Vec<String>>,

    // Component path fields (string | array)
    #[serde(default)]
    pub commands: Option<OneOrMany<String>>,
    #[serde(default)]
    pub agents: Option<OneOrMany<String>>,
    #[serde(default)]
    pub skills: Option<OneOrMany<String>>,
    #[serde(default)]
    pub outputStyles: Option<OneOrMany<String>>,

    // Component fields with inline-object possibility (string | array | object)
    #[serde(default)]
    pub hooks: Option<HooksSpec>,
    #[serde(default)]
    pub mcpServers: Option<McpServersSpec>,
    #[serde(default)]
    pub lspServers: Option<LspServersSpec>,

    /// Forward compatibility for unknown fields.
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HooksSpec {
    Path(String),
    Paths(Vec<String>),
    Inline(HooksInline),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum HooksInline {
    /// File-format style: { "hooks": { ... } }
    Wrapped(HooksFile),
    /// Some tools may inline the inner map directly.
    Unwrapped(HooksMap),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HooksFile {
    pub hooks: HooksMap,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

pub type HooksMap = HashMap<String, Vec<HookMatcher>>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookMatcher {
    #[serde(default)]
    pub matcher: Option<String>,
    pub hooks: Vec<HookAction>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HookAction {
    #[serde(rename = "type")]
    pub kind: String, // "command" | "prompt" | "agent"
    #[serde(default)]
    pub command: Option<String>,
    #[serde(default)]
    pub prompt: Option<String>,
    #[serde(default)]
    pub agent: Option<String>,
    #[serde(default)]
    pub arguments: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum McpServersSpec {
    Path(String),
    Paths(Vec<String>),
    Inline(McpServersMap),
}

pub type McpServersMap = HashMap<String, McpServerConfig>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum LspServersSpec {
    Path(String),
    Paths(Vec<String>),
    Inline(LspServersMap),
}

pub type LspServersMap = HashMap<String, LspServerConfig>;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LspServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Option<Vec<String>>,
    #[serde(default)]
    pub transport: Option<String>, // "stdio" | "socket"
    #[serde(default)]
    pub env: Option<HashMap<String, String>>,
    #[serde(default)]
    pub extensionToLanguage: Option<HashMap<String, String>>,
    #[serde(default)]
    pub initializationOptions: Option<serde_json::Value>,
    #[serde(default)]
    pub settings: Option<serde_json::Value>,
    #[serde(default)]
    pub workspaceFolder: Option<String>,
    #[serde(default)]
    pub startupTimeout: Option<i64>,
    #[serde(default)]
    pub shutdownTimeout: Option<i64>,
    #[serde(default)]
    pub restartOnCrash: Option<bool>,
    #[serde(default)]
    pub maxRestarts: Option<i64>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

/// ---- marketplace.json ----

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeMarketplaceManifest {
    pub name: String,
    pub owner: MarketplaceOwner,
    pub plugins: Vec<MarketplacePluginEntry>,

    #[serde(default)]
    pub metadata: Option<MarketplaceMetadata>,

    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplaceOwner {
    pub name: String,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplaceMetadata {
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub pluginRoot: Option<String>,
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MarketplacePluginEntry {
    pub name: String,
    pub source: PluginSource,

    /// When `strict` is true (default), merge marketplace component fields with plugin.json.
    /// When false, marketplace entry defines the plugin entirely.
    #[serde(default)]
    pub strict: Option<bool>,

    // Optional discovery metadata
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub version: Option<String>,
    #[serde(default)]
    pub author: Option<Author>,
    #[serde(default)]
    pub homepage: Option<String>,
    #[serde(default)]
    pub repository: Option<String>,
    #[serde(default)]
    pub license: Option<String>,
    #[serde(default)]
    pub keywords: Option<Vec<String>>,
    #[serde(default)]
    pub category: Option<String>,
    #[serde(default)]
    pub tags: Option<Vec<String>>,

    // Optional component overrides (same shapes as plugin.json)
    #[serde(default)]
    pub commands: Option<OneOrMany<String>>,
    #[serde(default)]
    pub agents: Option<OneOrMany<String>>,
    #[serde(default)]
    pub skills: Option<OneOrMany<String>>,
    #[serde(default)]
    pub outputStyles: Option<OneOrMany<String>>,
    #[serde(default)]
    pub hooks: Option<HooksSpec>,
    #[serde(default)]
    pub mcpServers: Option<McpServersSpec>,
    #[serde(default)]
    pub lspServers: Option<LspServersSpec>,

    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PluginSource {
    /// Relative path (works when marketplace is installed via git/clone)
    Path(String),
    /// Object sources: github or url
    Object(PluginSourceObject),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginSourceObject {
    pub source: String, // "github" | "url" (per docs)
    #[serde(default)]
    pub repo: Option<String>, // github
    #[serde(default)]
    pub url: Option<String>, // git url ending .git
    #[serde(default, rename = "ref")]
    pub git_ref: Option<String>, // branch/tag
    #[serde(default)]
    pub sha: Option<String>, // full 40-char commit
    #[serde(default)]
    pub path: Option<String>, // optional subdir (future-proof)
    #[serde(flatten)]
    pub extra: HashMap<String, serde_json::Value>,
}
