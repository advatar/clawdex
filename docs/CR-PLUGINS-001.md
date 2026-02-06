# CR-PLUGINS-001: Claude Code / Cowork Plugin Compatibility in Clawdex

**Status:** Proposed  
**Owner:** Clawdex maintainers  
**Scope:** `clawdex` (CLI + macOS app)  
**Goal:** Run unmodified Claude Code plugins (and the open-source Cowork “knowledge work plugins”) inside Clawdex with a minimal compatibility shim, while preserving auditability + human-in-the-loop controls.

---

## Background

Anthropic’s Claude Code plugin system standardizes a file-based extension format (`.claude-plugin/plugin.json`, plus `skills/`, `commands/`, `agents/`, `hooks/`, `.mcp.json`, `.lsp.json`, `styles/`) and distributes plugins via “marketplaces” (`.claude-plugin/marketplace.json`). The system is intentionally transparent (mostly Markdown + JSON) and is now being reused in **Claude Cowork** via open-source “knowledge work plugins.”

Clawdex already has:
- A plugin registry + installation directory (`state_dir/plugins/<id>`)
- A skill sync pipeline that writes to `CODEX_HOME/skills/...`
- A Mac app bridge (`ui_bridge.rs`) that can list plugins + run plugin commands
- A Codex app-server integration with **approvals**, **file-change gating**, and an **event stream** (useful for hooks)

This CR defines a step-by-step change set to support:
1) Claude plugin manifests and component discovery (including “manifest optional” plugins)  
2) Marketplace discovery + install flows compatible with `/plugin marketplace add` + `/plugin install x@y` semantics  
3) A compatibility layer for `$ARGUMENTS`, namespacing, and `${CLAUDE_PLUGIN_ROOT}`  
4) Optional-but-important: hook execution using the existing codex app-server notification stream.

> **Key Claude behaviors we must match**
> - Plugin namespacing (`/plugin-name:hello`) is always on for plugins.  
> - `plugin.json` is optional; if missing, Claude derives the plugin name from the directory.  
> - Installing a plugin copies it into a cache location; plugins should not rely on `../` outside the plugin.  
> - Marketplaces can reference plugins via relative paths (when the marketplace itself is git-based), or via GitHub / git URL sources.

---

## External behavior we’re implementing (compatibility targets)

### Plugin structure + namespacing
- Plugin directory contains `.claude-plugin/plugin.json` (optional) plus component directories at the root.
- Skills in a plugin are invoked with a namespace prefix: `/my-plugin:hello` (preventing collisions).
- Skills are either:
  - **commands**: markdown files in `commands/`  
  - **skills**: directories in `skills/` containing `SKILL.md`

### Marketplace behavior
- Marketplace is a repository (or local directory) with `.claude-plugin/marketplace.json`
- Users add a marketplace, then install a plugin: `plugin-name@marketplace-name`
- Marketplace entries support:
  - `source` as relative paths (when marketplace is installed via git)
  - `source` as GitHub repo objects
  - `source` as git URL objects

---

## Security & compliance constraints (Clawdex-specific)

Clawdex’s product goals include compliance, auditability, and human-in-the-loop execution. That intersects directly with:
- **Prompt-injection & “rogue automation”** risks
- **Downloaded plugin code** (shell hooks, MCP servers) risks
- **Mac App Store rules** around downloading/executing new code

We implement:
1) **Install-time validation** (manifest, marketplace, paths, scopes)  
2) **Run-time dynamic checkpoints** (already supported via Codex app-server approvals; extended to plugin hooks and plugin-run MCP)  
3) **Event logging** of: plugin installed/updated/enabled/disabled, hook fired, approvals requested, decision, and outputs.  
4) A **Mac App Store “constrained execution mode”** (build flag + runtime policy) that disables plugin-provided executables and remote installation.

---

## Functional requirements

### FR1 — Install plugins from:
- Local path (existing)
- npm (existing)
- Marketplace entry `plugin@marketplace` (new)
- GitHub repo (new, via marketplace `source` objects)
- Git URL repo (new, via marketplace `source` objects)

### FR2 — Marketplace management (CLI + UI bridge)
- `marketplace add <path|owner/repo|git-url>`  
- `marketplace list`  
- `marketplace update [name]`  
- `marketplace remove <name>`  
- `marketplace sync` (refresh cached plugin list)

### FR3 — Component discovery and sync
For each installed plugin:
- Discover commands, skills, agents, hooks, MCP servers, LSP servers, output styles.
- Respect `plugin.json` component path fields when provided.
- Fall back to standard locations if manifest missing.
- Ensure namespacing rules: `/plugin:thing` always.

### FR4 — `$ARGUMENTS` + `${CLAUDE_PLUGIN_ROOT}`
- Support `$ARGUMENTS` substitution for commands + skills.
- Provide `${CLAUDE_PLUGIN_ROOT}` for:
  - hook commands
  - MCP server command/cwd/env
  - any plugin-side scripts that reference it.

### FR5 — Hooks (MVP)
- Parse `hooks/hooks.json` and inline hooks config.
- Fire hooks on these events at minimum:
  - `UserPromptSubmit`
  - `PermissionRequest`
  - `PreToolUse` + `PostToolUse` (from Codex app-server items)
  - `PreCompact` (from context compact notifications)
- Support hook types:
  - `command` (shell command)
  - `prompt` (LLM evaluation)
  - `agent` (optional; phase 2)

### FR6 — Plugin creation tooling (CLI + macOS UI)
- `plugin init` (scaffold)
- `plugin validate` (plugin dir or marketplace dir)
- `plugin new-skill`, `plugin new-command`, `plugin new-agent`, `plugin new-hook`
- `plugin pack` (optional; outputs a zipball for sharing)

---

## macOS App Store constraints

For the Mac App Store build, enforce **Constrained Execution Mode**:
- Disallow remote marketplace add/install.
- Disallow plugin-provided MCP/LSP server executables (only allow built-in/bundled servers).
- Disallow hook type `command` unless the command is in an allowlist of bundled binaries.
- Allow prompt-based components (skills/commands/agents/output styles), because they are transparent and editable.

This aligns with Apple guidelines requiring Mac App Store apps to be sandboxed and not download/install additional code that changes functionality. (See guideline 2.4.5(iv) and 2.5.2.)

---

## Implementation phases (high-level)

### Phase 0 — Cleanup and internal model
- Introduce canonical internal structs:
  - `ClaudePluginManifest`
  - `ClaudeMarketplaceManifest`
  - `PluginResolvedPaths` (commands/skills/agents/hooks/mcp/lsp/styles)
- Remove duplicate/broken manifest loader (`read_manifest`) and unify on one parser.

### Phase 1 — Marketplace support + install
- Add `plugin_marketplaces` + `marketplace_plugins` tables (or JSON cache) and implement:
  - add/list/update/remove
- Implement install:
  - resolve `plugin@marketplace`
  - fetch plugin source
  - copy to cache dir (immutable)
  - register in DB and installs.json

### Phase 2 — Full component sync + namespacing
- Fix skill sync:
  - skills: `skills/<name>/SKILL.md` should map to `/plugin:<name>`
  - commands: `commands/<name>.md` should map to `/plugin:<name>`
- Preserve and merge SKILL frontmatter (do not double-frontmatter).
- Expand list/run UI to include both command + skill entries.

### Phase 3 — Hooks & audit events
- Implement hook engine inside `CodexClient` notification loop:
  - map notifications to hook events
  - apply `matcher` regex to tool or event name
  - execute hook actions with approvals + event logging

### Phase 4 — macOS app support
- Expose new capabilities via `ui_bridge.rs`:
  - marketplace CRUD
  - plugin validate
  - plugin init / create components
  - errors pane from `plugin_errors`

---

## Acceptance criteria

1) **Install & run** at least one plugin from `anthropics/knowledge-work-plugins` via marketplace flows.
2) Skills appear in Clawdex as namespaced commands (`/sales:call-prep`) with `$ARGUMENTS` working.
3) `claude plugin validate .`-equivalent errors are surfaced by `clawdex plugins validate`.
4) Hook of type `command` fires on `PostToolUse` and can be approved/denied.
5) Mac App Store build compiles with Constrained Execution Mode and blocks remote install + arbitrary code execution.
6) No upstream codex source modifications required **except** optional hook-event enrichment if the protocol lacks a necessary signal.

---

## Test plan (minimum)

- Unit tests:
  - manifest parsing (all variants of string|array|object fields)
  - marketplace parsing + reserved names checks
  - `$ARGUMENTS` substitution (edge cases)
  - path canonicalization + traversal prevention
- Integration tests:
  - install from GitHub zip (mocked HTTP)
  - sync to CODEX_HOME and invoke skill
  - hook execution + approvals
- UI tests (mac app):
  - add local marketplace dir
  - list plugins, install, enable/disable, run command, show errors

