Yes — you can make **Clawdex** consume the *same* “Claude Code / Cowork plugin” format (the open‑sourced “knowledge‑work” plugins included) without porting them, but your current implementation needs a few structural + semantic changes to reach real compatibility.

Anthropic’s move matters because it’s not just “new prompts” — it’s **packaged workflows** (skills, commands, agents, hooks, MCP/LSP configs) distributed like software. That distribution shift is exactly what spooked markets in early Feb 2026 (Reuters + Bloomberg coverage), alongside Anthropic’s open‑sourcing of **11 starter Cowork plugins**.  [oai_citation:0‡Axios](https://www.axios.com/2026/01/30/ai-anthropic-enterprise-claude)

Below is a **spec / change request** that’s grounded in Claude’s plugin reference + your current Rust code (notably `src/plugins.rs`, `src/main.rs`, `src/ui_bridge.rs` in your `share.zip`). It’s designed to **maximize upstream Codex compatibility** by keeping changes in Clawdex (overlay generation + config plumbing), not by forking Codex.

---

## Compatibility target

Implement Claude Code’s plugin system model:

- Plugins can contain **skills**, **legacy commands**, **agents**, **hooks**, **MCP servers**, **LSP servers**, **output styles**.  [oai_citation:1‡Claude Code](https://code.claude.com/docs/en/discover-plugins)  
- Plugin manifest `.claude-plugin/plugin.json` is optional; if present, it provides metadata + **custom component paths** that are **additive** to defaults.  [oai_citation:2‡Claude Code](https://code.claude.com/docs/en/plugins-reference)  
- Plugins are **namespaced** (`plugin-name:thing`) and **can’t conflict** with user/project skills.  [oai_citation:3‡Claude Code](https://code.claude.com/docs/en/skills)  
- Plugins are installed via **marketplaces** and support **install scopes** (`user`, `project`, `local`).  [oai_citation:4‡Claude Code](https://code.claude.com/docs/en/discover-plugins)  
- Plugins are **cached by copying** into a cache directory; paths can’t traverse outside root; symlinks are honored during copy (the linked content ends up in cache).  [oai_citation:5‡Claude Code](https://code.claude.com/docs/en/plugins-reference)  

Also note: Anthropic’s `knowledge-work-plugins` repo explicitly emphasizes the “no code / no infra” nature (Markdown + JSON), which is perfect for Clawdex reuse if you implement the loader correctly.  [oai_citation:6‡GitHub](https://github.com/anthropics/knowledge-work-plugins)

---

## Current Clawdex state (from your source) and the key gaps

### What you already have (good base)
In `src/plugins.rs` you already:
- Install plugins from **local paths** (copy into state dir), track in sqlite, enable/disable/remove.
- Detect `.claude-plugin/plugin.json` (you call it “Cowork manifest”).
- Enumerate plugin `commands/` and run one via `run_plugin_command()` by sending a prompt to Codex.
- Export `.mcp.json` into `state_dir/mcp/*.json` (but it’s not wired into Codex runtime yet).
- Have approvals + audit infrastructure (`approvals.rs`, `audit.rs`) you can reuse for “trust checkpoints”.

### Gaps that will break real Claude plugin compatibility

#### 1) **Skills directory structure is currently mishandled**
Claude skills live at `skills/<skill-name>/SKILL.md`.  [oai_citation:7‡Claude Code](https://code.claude.com/docs/en/plugins-reference)  
Your `sync_plugin_skills()` walks for `*.md` and uses `file_stem()` as skill name. This will mis-import `skills/foo/SKILL.md` as a skill named `SKILL`, and create a wrong nested directory (`.../foo/SKILL/SKILL.md`).

**Impact:** Any modern Claude plugin that uses proper skills won’t work as expected.

#### 2) You don’t implement Claude’s **skill semantics**
Claude skills support:
- YAML frontmatter controls (e.g. `disable-model-invocation`, `user-invocable`, `allowed-tools`, `context: fork`, `agent`).
- `$ARGUMENTS` / `$ARGUMENTS[N]` / `$N` substitutions.
- `!`command preprocessing that executes shell commands *before* prompt is sent.  [oai_citation:8‡Claude Code](https://code.claude.com/docs/en/skills)

Right now, plugin commands only support `{{input}}` replacement; nothing else.

#### 3) Plugin manifest parsing is incomplete
Claude’s manifest schema includes `commands`, `agents`, `skills`, `hooks`, `mcpServers`, `outputStyles`, `lspServers` and allows each to be string|array|object, additive to defaults, with strict “`./` relative path” rules.  [oai_citation:9‡Claude Code](https://code.claude.com/docs/en/plugins-reference)  
You only parse basic metadata + permissions and assume default directories.

#### 4) No **marketplace** support (so you can’t “use the same plugins” the same way)
Claude supports adding marketplaces and installing plugins as `plugin@marketplace`, plus updating catalogs.  [oai_citation:10‡Claude Code](https://code.claude.com/docs/en/discover-plugins)  
Your current install is local-path-only.

#### 5) No install **scopes** (user/project/local)
Claude uses `~/.claude/settings.json`, `.claude/settings.json`, `.claude/settings.local.json` to determine availability.  [oai_citation:11‡Claude Code](https://code.claude.com/docs/en/plugins-reference)  
You only have a single global enable/disable state.

#### 6) `.mcp.json` is exported but not **activated**
Claude loads MCP server definitions from plugin configs; you export but don’t feed Codex a unified MCP config.

#### 7) Hooks / LSP / output styles not implemented
Not required for “MVP compatibility” but needed for true parity.

---

## Change Request: “Claude-Compatible Plugin System for Clawdex”

### Goals
1. **Install + run** unmodified Claude plugins (especially `knowledge-work-plugins`) in Clawdex CLI + macOS app.  [oai_citation:12‡GitHub](https://github.com/anthropics/knowledge-work-plugins)  
2. Implement **namespaced commands** like `/commit-commands:commit` and skills like `plugin:skill`.  [oai_citation:13‡Claude Code](https://code.claude.com/docs/en/discover-plugins)  
3. Support **marketplaces + scopes** so teams can share plugin sets safely.  [oai_citation:14‡Claude Code](https://code.claude.com/docs/en/discover-plugins)  
4. Add a **MAS-safe mode** for the App Store build that maximizes features without tripping over App Review 2.5.2 (details below).  [oai_citation:15‡Apple Developer](https://developer.apple.com/app-store/review/guidelines/)  

### Non-goals
- Perfect UI parity with Claude’s `/plugin` TUI in v1 (you can do it later).
- Multi-user enterprise managed settings (can come later).
- Full “code intelligence plugin” parity (LSP) in v1, unless Codex already supports it cleanly.

---

## Proposed architecture (keeps upstream Codex untouched)

### A) Add a “Plugin Resolver” layer (pure Clawdex)
A new module (or refactor `plugins.rs`) produces an **EffectivePlugin** graph:

- `PluginIdentity`: `{ name, marketplace?, version?, source, scope }`
- `ComponentPaths`: resolved sets of:
  - skills roots (directories)
  - commands roots (files/dirs)
  - agents roots (files/dirs)
  - hooks configs (files/inline)
  - mcp configs (files/inline)
  - lsp configs (files/inline)
  - output styles (files/dirs)

Use Claude’s additive behavior: defaults + manifest-specified custom paths.  [oai_citation:16‡Claude Code](https://code.claude.com/docs/en/plugins-reference)  

### B) Generate a **Codex Overlay** directory from enabled plugins
Instead of trying to teach Codex “plugins”, generate an overlay inside `CODEX_HOME`:

- `CODEX_HOME/skills/_clawdex_plugins/<plugin>/<skill>/SKILL.md` (plus supporting files)
- `CODEX_HOME/skills/_clawdex_plugins/<plugin>/<command>/SKILL.md` (convert legacy command `.md` to a skill wrapper)
- `CODEX_HOME/agents/_clawdex_plugins/<plugin>/<agent>.md` (if you implement agents)
- `CODEX_HOME/output-styles/_clawdex_plugins/...` (if you implement styles)
- `CODEX_HOME/mcp/plugins.json` (single merged file) OR whatever Codex expects

**Why this is good:** no upstream patch; everything is “data” Codex already knows how to load.

### C) Build a “Skill/Command Renderer” (Clawdex-side)
When a plugin command is invoked through:
- `clawdex plugins run ...`
- or the macOS app UI

…Clawdex renders the final prompt, implementing Claude semantics:

1. Parse YAML frontmatter when present.
2. Apply `$ARGUMENTS` substitutions.  [oai_citation:17‡Claude Code](https://code.claude.com/docs/en/skills)  
3. Execute `!` commands (preprocessing) **only if allowed by policy**.  [oai_citation:18‡Claude Code](https://code.claude.com/docs/en/skills)  
4. Enforce `allowed-tools` by configuring Codex approval policy and/or denying tool calls in your approval handler.  [oai_citation:19‡Claude Code](https://code.claude.com/docs/en/skills)  
5. If `context: fork`, run in a fresh Codex thread (you already have thread primitives).  [oai_citation:20‡Claude Code](https://code.claude.com/docs/en/skills)  

---

## Step-by-step implementation checklist

### Phase 1 — Fix skills import + namespacing (unblocks real plugins)

**1.1 Replace `sync_plugin_skills()` with “directory-based skills sync”**
- Detect skill roots:
  - Default `plugin_root/skills/`
  - plus any manifest `skills` paths.  [oai_citation:21‡Claude Code](https://code.claude.com/docs/en/plugins-reference)
- For each `skills/**/SKILL.md`, treat the *parent directory name* as `skill-name`.
- Copy the entire directory (supporting files) into overlay:
  - dest dir: `CODEX_HOME/skills/_clawdex_plugins/<plugin-name>:<skill-name>/`
  - keep `SKILL.md` inside dest root.
- Do **not** create the extra `SKILL/` directory currently happening.

**1.2 Ensure the skill’s frontmatter `name:` is namespaced**
Claude plugin skills are namespaced by plugin: `plugin-name:skill-name`.  [oai_citation:22‡Claude Code](https://code.claude.com/docs/en/skills)  
Implementation options:
- If source SKILL.md already has `name: skill-name`, rewrite only `name:` to `plugin:skill`.
- If it already has `name: plugin:skill`, leave as is.
- Add a comment header like `# Source: <plugin>` if you want traceability (optional).

**1.3 Add deterministic conflict rules**
- Plugin namespace prevents collisions with user skills, but you should still:
  - refuse to sync if two enabled plugins resolve to the same `plugin-name` (or same namespace string).
  - record plugin load errors under a “plugin errors” UI tab (see Phase 4).

---

### Phase 2 — Convert legacy plugin commands into skills (so “/plugin:cmd” works)

Claude treats legacy commands as skills; skills/commands are merged conceptually.  [oai_citation:23‡Claude Code](https://code.claude.com/docs/en/skills)  

**2.1 Create `sync_plugin_commands_as_skills()`**
- Discover:
  - Default `plugin_root/commands/`
  - plus manifest `commands` paths (files/dirs).  [oai_citation:24‡Claude Code](https://code.claude.com/docs/en/plugins-reference)
- For each command markdown file:
  - Create an overlay skill directory:
    - `CODEX_HOME/skills/_clawdex_plugins/<plugin-name>:<command-name>/SKILL.md`
  - Generate SKILL.md with frontmatter:
    - `name: <plugin>:<command>`
    - `description: <best-effort from H1 or first paragraph>`
    - `disable-model-invocation: true` (safe default; avoids autonomous firing)
    - optionally `user-invocable: true`
  - Body: include the original command markdown verbatim.

**2.2 Update your existing command runner to use the shared renderer**
Right now `resolve_plugin_command_prompt()` uses `{{input}}`.
Replace that with:
- `render_skill_or_command(plugin_id, path, args_string)` which implements `$ARGUMENTS`, `!` preprocess, and appends arguments when not referenced (Claude behavior).  [oai_citation:25‡Claude Code](https://code.claude.com/docs/en/skills)  

---

### Phase 3 — Implement manifest schema + additive custom paths

**3.1 Replace `CoworkManifest` with `ClaudePluginManifest`**
Match the documented schema and types:

- `name` required if manifest exists
- component path fields are string|array|object (hooks/mcp/lsp can be inline object too).  [oai_citation:26‡Claude Code](https://code.claude.com/docs/en/plugins-reference)  

**3.2 Enforce path rules**
- Paths must be relative to plugin root and start with `./`.  [oai_citation:27‡Claude Code](https://code.claude.com/docs/en/plugins-reference)  
- Reject absolute paths.
- Reject `..` traversal.

**3.3 Implement `${CLAUDE_PLUGIN_ROOT}` substitution**
Claude provides `${CLAUDE_PLUGIN_ROOT}` for hooks/MCP/scripts.  [oai_citation:28‡Claude Code](https://code.claude.com/docs/en/plugins-reference)  
For Clawdex:
- When executing hooks or launching MCP servers, set:
  - `CLAUDE_PLUGIN_ROOT=<cached_plugin_root_abs>`
- When rendering skills, optionally substitute `${CLAUDE_PLUGIN_ROOT}` in the prompt (helps portability).

---

### Phase 4 — Marketplace support (so you can install “the same plugins”)

Claude: marketplaces are catalogs; you add a marketplace, then install plugins `name@marketplace`.  [oai_citation:29‡Claude Code](https://code.claude.com/docs/en/discover-plugins)  

**4.1 Add marketplace data model**
Add sqlite tables:
- `marketplaces(id, name, source, last_refreshed_at, etag/hash, error)`
- `marketplace_plugins(marketplace_id, plugin_name, version, description, source, metadata_json)`

**4.2 Implement marketplace sources**
Start with the sources that matter most for your world:

- GitHub repo shorthand `owner/repo` containing `.claude-plugin/marketplace.json`  [oai_citation:30‡Claude Code](https://code.claude.com/docs/en/discover-plugins)  
- Local path to `marketplace.json` (for internal teams)  [oai_citation:31‡Claude Code](https://code.claude.com/docs/en/discover-plugins)  
- Remote URL to hosted `marketplace.json` (optional)  [oai_citation:32‡Claude Code](https://code.claude.com/docs/en/discover-plugins)  

(You can add npm/pip sources later; Claude supports them, but GitHub/local covers most OSS workflows.  [oai_citation:33‡Claude Code](https://code.claude.com/docs/en/plugins-reference))

**4.3 Add CLI surface (mirrors Claude, but “clawdex” flavored)**
In `src/main.rs`, under `plugins`:

- `clawdex plugins marketplace add <source>`
- `clawdex plugins marketplace list`
- `clawdex plugins marketplace refresh [name]`
- `clawdex plugins marketplace remove <name>`
- `clawdex plugins install <plugin[@marketplace]> --scope user|project|local`
- `clawdex plugins uninstall <plugin[@marketplace]> --scope ...`
- `clawdex plugins search <query>`
- `clawdex plugins errors`

**4.4 Record + display errors**
Claude has an “Errors” tab in the plugin manager UI.  [oai_citation:34‡Claude Code](https://code.claude.com/docs/en/discover-plugins)  
Add:
- `PluginLoadError { plugin_id, kind, message, path?, ts }`
- Surface via UI bridge (Phase 6).

---

### Phase 5 — Implement scopes (user/project/local)

Claude scopes map to settings files (user/project/local).  [oai_citation:35‡Claude Code](https://code.claude.com/docs/en/plugins-reference)  

You can mirror the behavior but rename `.claude` → `.clawdex` (to avoid brand confusion), while still being conceptually compatible:

- User scope: `~/.clawdex/settings.json`
- Project scope: `<repo>/.clawdex/settings.json` (commit)
- Local scope: `<repo>/.clawdex/settings.local.json` (gitignored)

Each includes:
```json
{
  "enabledPlugins": ["plugin@marketplace", "other@marketplace"],
  "disabledPlugins": ["..."],
  "extraKnownMarketplaces": ["owner/repo", "https://.../marketplace.json"]
}
```

Then implement:
- Effective enabled set = merge scopes with precedence (managed > project > user > local, or your chosen order).
- The resolver creates the overlay based on the effective set.

This gives you “cowork tasks mode for a single user” *and* a path to team sharing.

---

### Phase 6 — Wire up MCP configs (and keep it safe)

Claude plugins can ship MCP server definitions via `.mcp.json` and manifest `mcpServers`.  [oai_citation:36‡Claude Code](https://code.claude.com/docs/en/plugins-reference)  

**6.1 Decide on integration strategy**
You have two realistic options:

**Option A: “Merged MCP config file” approach (preferred if Codex supports it)**
- Aggregate all enabled plugin MCP configs into one generated file:
  - `CODEX_HOME/mcp.json` (or whatever Codex expects)
- Add `CLAUDE_PLUGIN_ROOT` env var per server where applicable.

**Option B: “Clawdex Gateway MCP server” approach**
- Don’t let Codex spawn arbitrary MCP servers.
- Codex only talks to Clawdex MCP server.
- Clawdex gateway proxies tool calls to configured MCP servers (remote/local) with enforcement + audit.

Option B fits your compliance story better (central policy enforcement + evidence logging).

---

### Phase 7 — Hooks (compliance and automation)

Claude hooks can run commands after events (example shown in plugin reference).  [oai_citation:37‡Claude Code](https://code.claude.com/docs/en/plugins-reference)  

In Clawdex, hooks should integrate tightly with:
- approvals
- audit log (tamper-evident chain you already have)
- dynamic trust checkpoints

Implementation approach:
- Add a `HookEngine` that subscribes to Codex events through your `EventSink`/approval handlers.
- Support a limited but useful subset first:
  - `PostToolUse` and `PostFileChange` equivalents (map from your event kinds)
- Hook actions:
  - `type: command` (exec)
  - `type: prompt` (send message to Codex as follow-up)
- Always record hook executions as audit entries (include plugin id + hook id).

---

### Phase 8 — Plugin creation tooling (CLI + macOS app)

Claude’s docs emphasize plugin components and structure.  [oai_citation:38‡Claude Code](https://code.claude.com/docs/en/plugins-reference)  

**8.1 CLI commands**
Add:
- `clawdex plugins init <dir> --name <plugin-name> [--template cowork|mcp|skills-only]`
  - Creates:
    - `.claude-plugin/plugin.json`
    - `skills/<skill>/SKILL.md`
    - `commands/` (optional)
    - `hooks/hooks.json` (optional)
    - `.mcp.json` (optional)
    - `.lsp.json` (optional)
- `clawdex plugins validate <dir>`
  - Checks schema, path rules, required fields, namespacing
- `clawdex plugins pack <dir> --out <file>` (optional)
- `clawdex plugins publish` (later; for ClawdexHub workflow)

**8.2 Mac app (via `ui_bridge.rs`)**
Add UI bridge messages:
- `marketplace_list`, `marketplace_add`, `marketplace_refresh`, `marketplace_remove`
- `plugin_search`, `plugin_install`, `plugin_uninstall`
- `plugin_validate`, `plugin_init`
- `plugin_errors_list`
- `plugin_open_in_finder` (nice for dev UX)

Then implement screens:
- “Discover” (marketplaces + search)
- “Installed”
- “Marketplaces”
- “Errors”
- “Create Plugin” wizard

This matches Claude’s mental model (Discover / Installed / Marketplaces / Errors).  [oai_citation:39‡Claude Code](https://code.claude.com/docs/en/discover-plugins)  

---

## macOS App Store compliance mode (maximize features while staying reviewable)

You said: “allow as much as possible in line with Apple rules.” The tricky part is **App Review 2.5.2** (downloading/executing code that changes app behavior).  [oai_citation:40‡Apple Developer](https://developer.apple.com/app-store/review/guidelines/)  

### Practical approach: two capability tiers
**Tier 1: “App Store build” (sandboxed)**
- Allow:
  - Local plugin folders the user imports via file picker (treat as content).
  - Skills + commands (Markdown/JSON) execution via Codex (your bundled binaries).
  - Remote MCP endpoints *if they’re remote HTTP services* (no local process spawning).
  - Audit logs + approvals (this actually helps your review story).
- Disable or gate:
  - Marketplace auto-install/auto-update (downloading new “logic” is what reviewers flag).
  - Running plugin hook shell commands by default.
  - Spawning local MCP servers via `command: npx ...` inside the sandbox.

**Tier 2: “Direct download / notarized outside App Store”**
- Full feature set: marketplaces, local MCP servers, hooks executing commands, etc.

### Sandbox realities you must build for
- Sandboxed apps must use **user-granted access** to read/write outside the container, typically via security‑scoped bookmarks.  [oai_citation:41‡Apple Developer](https://developer.apple.com/documentation/professional-video-applications/enabling-security-scoped-bookmark-and-url-access)  
- If you bundle helper executables and spawn them (Codex + Clawdex), child processes inherit the sandbox of the parent process (so your CLI tools don’t magically escape).  [oai_citation:42‡Apple Developer](https://developer.apple.com/documentation/foundation/process?language=objc)  

This suggests a clean MAS story: “We bundle our executables; no self-updating code; user explicitly grants folder access; high-risk actions require explicit approval.”

---

## Why you *can* reuse the Cowork plugins specifically

- Anthropic’s open-sourced Cowork starter plugins are positioned as **editable, file-based** workflows (no code) — which aligns perfectly with a Clawdex “plugin as content” model.  [oai_citation:43‡GitHub](https://github.com/anthropics/knowledge-work-plugins)  
- The core of those plugins is the same stuff you already handle (Markdown prompts + workflow scaffolding). The missing pieces are mostly:
  - correct `skills/<name>/SKILL.md` handling
  - substitutions + preprocessing semantics
  - marketplace/scopes plumbing

---

## Quick “definition of done” acceptance tests

Use these as your PR checklist.

### A) Loader correctness
- [ ] Install a plugin that contains **skills/** with `skills/foo/SKILL.md` and supporting files; verify it appears as `/plugin:foo`.
- [ ] Install a plugin that contains **commands/**; verify each becomes `/plugin:command`.
- [ ] Manifest paths (`commands`, `skills`, etc.) are additive and enforced as `./relative`.  [oai_citation:44‡Claude Code](https://code.claude.com/docs/en/plugins-reference)  

### B) Execution semantics
- [ ] `$ARGUMENTS`, `$ARGUMENTS[0]`, `$0` render correctly.  [oai_citation:45‡Claude Code](https://code.claude.com/docs/en/skills)  
- [ ] `!`command preprocessing runs only when allowed, output is injected, command itself isn’t shown to the model.  [oai_citation:46‡Claude Code](https://code.claude.com/docs/en/skills)  
- [ ] `allowed-tools` is enforced (tool calls outside list are blocked and logged).  [oai_citation:47‡Claude Code](https://code.claude.com/docs/en/skills)  

### C) Marketplace + scopes
- [ ] Add marketplace, browse plugins, install `name@marketplace`.  [oai_citation:48‡Claude Code](https://code.claude.com/docs/en/discover-plugins)  
- [ ] `--scope project` writes to repo settings file; clone repo and it works.  [oai_citation:49‡Claude Code](https://code.claude.com/docs/en/plugins-reference)  

### D) macOS app
- [ ] UI can list installed plugins, run commands, show errors.
- [ ] MAS build runs with sandbox folder access + bookmarks.  [oai_citation:50‡Apple Developer](https://developer.apple.com/documentation/professional-video-applications/enabling-security-scoped-bookmark-and-url-access)  

---

## If youFocusing on what you asked: “Can we use those same plugins in Clawdex?”
**Answer:** yes — but you need to fix skills import + implement Claude’s skill rendering semantics + add marketplace/scopes. Once you do, you can consume Cowork plugins essentially “as-is” (and that’s the whole distribution advantage).  [oai_citation:51‡GitHub](https://github.com/anthropics/knowledge-work-plugins)

If you want, next I can turn the above into a **PR-style change request** with:
- exact Rust structs for manifest + marketplace schema,
- the new sqlite migrations,
- and a concrete diff plan for `plugins.rs` + `ui_bridge.rs` (function-by-function).
