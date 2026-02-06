# Diff plan: `plugins.rs` + `ui_bridge.rs` (Claude plugin compatibility)

This is a *surgical* checklist referencing the current Clawdex source layout (from `share/src/...`).

---

## 1) `share/src/plugins.rs`

### 1.1 Remove / fix the broken manifest loader
**Problem:** `install_plugin()` calls `read_manifest()` returning an undefined `PluginManifest`.  
**Fix:** delete `read_manifest()` and make `install_plugin()` call the existing `load_plugin_manifest()` (rename it to `load_claude_manifest()`), returning the new `ClaudePluginManifest`.

- Replace:
  - `struct CoworkManifest` with `ClaudePluginManifest` (new module `claude_plugins/schema.rs`)
- Keep:
  - `OpenClawManifest` support, but load it as a separate `PluginKind::OpenClaw`.

### 1.2 Implement “manifest optional” plugins
Per Claude docs, `plugin.json` is optional; name defaults to directory name.

Add:
- `fn derive_plugin_name_from_dir(root: &Path) -> String`
- `fn load_claude_manifest_or_derive(root: &Path) -> Result<ClaudePluginManifestLike>` where:
  - `ClaudePluginManifestLike { name, description?, version?, component_paths? }`
  - If file exists: parse it
  - Else: `name=dirname`, component paths default to standard layout.

### 1.3 Fix skill sync for `skills/<name>/SKILL.md`
**Current bug:** `sync_plugin_skills()` walks `*.md` and uses file stem, producing `SKILL` instead of folder name.

Replace discovery with:
- Walk `skills/**/SKILL.md`
- Skill name = parent directory name
- Preserve other files in the skill directory (supporting docs/scripts)

Also include `commands/*.md`:
- command name = file stem
- map to `/plugin-name:command-name`

### 1.4 Merge frontmatter instead of double-frontmatter
**Current:** `render_skill_frontmatter()` prepends YAML with `name` + `description` but does not preserve fields and will duplicate frontmatter.

Implement:
- `parse_frontmatter(md: &str) -> (Option<serde_yaml::Value>, &str_body)`
- Merge strategy:
  - always set `name` to `{plugin}:{skill}` (namespaced)
  - if SKILL.md has `description`, keep it
  - preserve `disable-model-invocation`, `allowed-tools`, and any other keys.

### 1.5 Resolve component paths per manifest schema
Add a resolver that returns concrete paths:
- commands paths: string|array, each entry may be a file or directory
- agents paths: file/dir
- skills paths: dir(s)
- hooks: path(s) or inline object
- mcpServers: path(s) or inline object
- outputStyles: file/dir(s)
- lspServers: path(s) or inline map

Create:
- `struct ResolvedPluginComponents { commands: Vec<PathBuf>, skill_dirs: Vec<PathBuf>, ... }`
- `fn resolve_components(root, manifest, marketplace_overrides?) -> ResolvedPluginComponents`

### 1.6 Install-from-marketplace path
Add new entrypoint:
- `pub fn install_from_marketplace_command(paths, store, spec: &str, scope: Scope, ...)`

Spec parsing:
- `plugin@marketplace`
- validate marketplace exists
- resolve plugin entry source:
  - relative path (requires marketplace to be git-based clone)
  - github object
  - url object

Then:
- fetch source to temp dir
- copy to cache root: `state_dir/plugins/cache/<plugin>/<sha-or-version>/...`
- register `root_path` = that cache path
- store `PluginInstallRecord` with `source_json`

---

## 2) `share/src/task_db.rs` (migrations)
Add `CREATE TABLE IF NOT EXISTS ...` statements from `sql/001_create_plugin_marketplaces.sql`.

Optionally also add:
- `fn upsert_marketplace(...)`
- `fn list_marketplaces(...)`
- `fn upsert_marketplace_plugin_entries(...)`
- `fn list_marketplace_plugins(marketplace: &str)`

---

## 3) `share/src/ui_bridge.rs`

### 3.1 Extend request/response enums
Add new request variants:
- `MarketplaceAdd { source: String }`
- `MarketplaceList`
- `MarketplaceUpdate { name: Option<String> }`
- `MarketplaceRemove { name: String }`
- `MarketplacePlugins { marketplace: String }`
- `PluginInstall { spec: String, scope: Option<String> }`
- `PluginValidate { path: String }`
- `PluginInit { name: String, dir: String }`
- `PluginCreateSkill { plugin_dir: String, name: String, description: String }`
- `PluginCreateCommand { plugin_dir: String, name: String, description: String }`

Add response variants mirroring these.

### 3.2 Surface errors
Add:
- `ListPluginErrors`
- `ResolvePluginError { id: String }` (mark resolved)
Return rows from `plugin_errors`.

---

## 4) Mac App Store gated behavior
Implement a runtime policy (read from config or compile-time `cfg(feature="mac_app_store")`):
- deny remote marketplace add/install
- deny `mcpServers` unless command is allowlisted/bundled
- deny hook type `command` unless allowlisted/bundled
- allow markdown-only plugins (skills/commands/agents/styles)

---

## 5) Minimal end-to-end demo (acceptance)
- Add marketplace: `anthropics/knowledge-work-plugins`
- Install: `sales@knowledge-work-plugins`
- Verify `/sales:call-prep` appears and runs.
- Verify plugin MCP configs are either started (CLI build) or blocked (Mac App Store build) with a clear error in the errors list.

