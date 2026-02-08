# clawdex

Clawdex is a Rust compatibility runtime that makes Codex feel like an OpenClaw‑grade assistant by exposing OpenClaw‑compatible tools via MCP, plus cron/heartbeat scheduling, memory access, and a skills sync workflow.

This README focuses on local development and the `clawdex` CLI surface.

**Quick Start (Dev)**
1. Init submodules: `git submodule update --init --recursive`
2. Build Codex: `cd codex/codex-rs && cargo build --release -p codex-cli`
3. Build Clawdex: `cd ../../clawdex && cargo build --release`
4. Run MCP server: `./target/release/clawdex mcp-server`

---

**Repo Layout**
1. `clawdex/` is the Rust CLI/runtime (this repo’s primary implementation).
2. `codex/` is the Codex submodule (used by `clawdex ui-bridge`).
3. `openclaw/` is the OpenClaw submodule (optional; used as a skills source).
4. `macClawdex/` is the macOS menu‑bar app starter that embeds `codex` + `clawdex`.

---

**Prereqs (Dev)**
1. Rust toolchain (stable) for `clawdex` and `codex` builds.
2. macOS + Xcode only if you plan to build `macClawdex`.
3. OpenClaw is optional and only needed if you want to sync its skills.

---

**Build Clawdex**
From the repo root:

```bash
cargo build --manifest-path clawdex/Cargo.toml
```

Release build:

```bash
cargo build --manifest-path clawdex/Cargo.toml --release
```

---

**Run the MCP Server**

```bash
clawdex mcp-server
```

Override workspace/state:

```bash
clawdex mcp-server --workspace /path/to/workspace --state-dir /path/to/state
```

---

**Run the Daemon (Cron + Heartbeat Loop)**

```bash
clawdex daemon --codex-path /path/to/codex --workspace /path/to/workspace --state-dir /path/to/state
```

The daemon executes due cron jobs + heartbeat turns by spawning `codex app-server`.
Provide a `codex` path via `--codex-path` or `codex.path` in config.

---

**Run the Gateway (HTTP)**

```bash
clawdex gateway --bind 127.0.0.1:18789
```

The gateway accepts:
- `GET /v1/health`
- `POST /v1/send` (queue outbound message)
- `POST /v1/incoming` (record inbound message + update last route)
- `GET /v1/receipts` (list message receipts)

---

**Sync OpenClaw Skills**

```bash
clawdex skills sync --source-dir openclaw/skills
```

Optional flags:
- `--prefix <prefix>`: prefix skill names (e.g. `oc-`).
- `--link`: symlink instead of copy (skips frontmatter edits).
- `--dry-run`: print actions without writing.
- `--user-dir <dir>`: user skills target (default: `~/.codex/skills/openclaw`).
- `--repo-dir <dir>`: repo skills target (optional; no default).

---

**Run the UI Bridge (macOS app)**
The macOS app launches `clawdex ui-bridge` and communicates via JSONL on stdin/stdout.

```bash
clawdex ui-bridge --stdio \
  --codex-path /path/to/codex \
  --state-dir /path/to/state \
  --workspace /path/to/workspace
```

The bridge spawns `codex app-server` and streams assistant messages back to stdout:
- `{"type":"assistant_message","text":"..."}`
- `{"type":"error","message":"..."}`

---

**Configuration + State**

Default state root:
- `~/.codex/clawdex/`

Override paths via env:
- `CLAWDEX_STATE_DIR` (or `CODEX_CLAWD_STATE_DIR`)
- `CLAWDEX_CONFIG_PATH` (or `CODEX_CLAWD_CONFIG_PATH`)
- `CLAWDEX_WORKSPACE` (or `CODEX_CLAWD_WORKSPACE_DIR` / `CODEX_WORKSPACE_DIR`)
- `CLAWDEX_CODEX_PATH` (daemon only; overrides `codex.path`)

Default state layout:
1. `~/.codex/clawdex/config.json` (optional)
2. `~/.codex/clawdex/cron/jobs.json`
3. `~/.codex/clawdex/cron/runs/<jobId>.jsonl`
4. `~/.codex/clawdex/cron/pending.json`
5. `~/.codex/clawdex/memory/fts.sqlite`
6. `~/.codex/clawdex/gateway/outbox.jsonl`
7. `~/.codex/clawdex/gateway/inbox.jsonl`
8. `~/.codex/clawdex/gateway/receipts.jsonl`
9. `~/.codex/clawdex/gateway/routes.json`
10. `~/.codex/clawdex/gateway/idempotency.json`
11. `~/.codex/clawdex/tasks.sqlite`
12. `~/.codex/clawdex/task_events/<runId>.jsonl`
13. `WORKSPACE/MEMORY.md`
14. `WORKSPACE/memory/YYYY-MM-DD.md`
15. `WORKSPACE/HEARTBEAT.md` (optional)
16. `~/.codex/clawdex/plugins/installs.json`
17. `~/.codex/clawdex/plugins/<pluginId>/...`
18. `~/.codex/clawdex/mcp/plugins.json`
19. `~/.codex/skills/clawdex/plugins/<pluginId>/<skill>/SKILL.md`

Example `config.json5`:

```json5
{
  workspace: "/path/to/workspace",
  workspace_policy: {
    allowed_roots: ["/path/to/workspace"],
    deny_patterns: ["**/.git/**", "**/.env", "**/.DS_Store"],
    read_only: false
  },
  permissions: {
    internet: true,
    mcp: {
      allow: ["slack", "notion"],
      deny: ["experimental-server"],
      plugins: {
        "sales": true,
        "finance": false
      }
    }
  },
  cron: { enabled: true },
  heartbeat: { enabled: true, interval_ms: 1800000 },
  memory: {
    enabled: true,
    citations: "auto",
    embeddings: {
      enabled: true,
      provider: "openai",
      model: "text-embedding-3-large",
      api_base: "https://api.openai.com",
      api_key_env: "OPENAI_API_KEY",
      batch_size: 32
    }
  },
  codex: {
    path: "/path/to/codex",
    approval_policy: "on-request",
    config_overrides: ["model=gpt-5.2-codex"]
  },
  gateway: { bind: "127.0.0.1:18789", route_ttl_ms: 86400000 }
}
```

Memory embeddings defaults:
- If `memory.embeddings` is omitted, Clawdex derives the provider from `codex.config_overrides` (falls back to `openai`) and defaults the model to `text-embedding-3-small`, with `enabled: true` (unless memory is disabled).

Workspace policy notes:
- `workspace_policy.allowed_roots` expands writable roots for Codex sandbox.
- `workspace_policy.deny_patterns` blocks tool access via `resolve_workspace_path`.
- `workspace_policy.read_only` switches Codex sandbox to read-only.
- `permissions.internet` toggles sandbox network access.
- `permissions.mcp.allow` / `permissions.mcp.deny` gate MCP servers when exporting plugin configs.
- `permissions.mcp.plugins` overrides MCP enablement per plugin id (true/false).

---

**CLI Reference**

`clawdex mcp-server`
1. Description: Run the MCP server that exposes OpenClaw‑compatible tool names.
2. Options:
   - `--no-cron` disables cron behavior.
   - `--no-heartbeat` disables heartbeat behavior.
   - `--workspace <path>` overrides workspace directory.
   - `--state-dir <path>` overrides state directory.

`clawdex daemon`
1. Description: Run the background loop for cron + heartbeat execution.
2. Options:
   - `--workspace <path>` overrides workspace directory.
   - `--state-dir <path>` overrides state directory.
   - `--codex-path <path>` overrides the `codex` binary path.

`clawdexd`
1. Description: Run the daemon runtime with HTTP IPC (`/v1/tasks`, `/v1/runs`).
2. Options:
   - `--bind <addr>` bind address (default `127.0.0.1:18791`).
   - `--workspace <path>` overrides workspace directory.
   - `--state-dir <path>` overrides state directory.
   - `--codex-path <path>` overrides the `codex` binary path.

`clawdex gateway`
1. Description: Run the minimal HTTP gateway (outbox/inbox + route tracking).
2. Options:
   - `--bind <addr>` overrides bind address (default `127.0.0.1:18789`).
   - `--workspace <path>` overrides workspace directory.
   - `--state-dir <path>` overrides state directory.

`clawdex skills sync`
1. Description: Sync OpenClaw skills into Codex skill directories.
2. Options:
   - `--prefix <prefix>` apply a name prefix to each skill.
   - `--link` create symlinks instead of copies.
   - `--dry-run` print actions without writing.
   - `--user-dir <dir>` override the user skills directory.
   - `--repo-dir <dir>` override the repo skills directory.
   - `--source-dir <dir>` override the source skills directory.

`clawdex ui-bridge`
1. Description: JSONL bridge used by the macOS app.
2. Options:
   - `--stdio` required by the current macOS app.
   - `--codex-path <path>` path to the `codex` binary.
   - `--state-dir <path>` state directory (also seeds CODEX_HOME under `<state>/codex`).
   - `--workspace <path>` workspace directory.

`clawdex tasks list`
1. Description: List tasks in the local task store.
2. Options:
   - `--state-dir <path>` overrides state directory.
   - `--workspace <path>` overrides workspace directory.

`clawdex tasks create`
1. Description: Create a new task.
2. Options:
   - `--title <text>` task title.
   - `--state-dir <path>` overrides state directory.
   - `--workspace <path>` overrides workspace directory.

`clawdex tasks run`
1. Description: Run a task with Codex app-server and stream events into the task store.
2. Options:
   - `--task-id <id>` run an existing task.
   - `--title <text>` create or reuse a task by title.
   - `--prompt <text>` prompt text (or provide via stdin).
   - `--codex-path <path>` overrides Codex binary.
   - `--auto-approve` accepts approvals automatically (default is interactive prompting).
   - `--state-dir <path>` overrides state directory.
   - `--workspace <path>` overrides workspace directory.

`clawdex tasks events`
1. Description: List events for a task run.
2. Options:
   - `--run-id <id>` task run id.
   - `--limit <n>` limit results.
   - `--state-dir <path>` overrides state directory.
   - `--workspace <path>` overrides workspace directory.

`clawdex tasks server`
1. Description: Start a minimal HTTP server for task state (`/v1/tasks`, `/v1/runs/<id>/events`).
2. Options:
   - `--bind <addr>` bind address (default `127.0.0.1:18790`).
   - `--state-dir <path>` overrides state directory.
   - `--workspace <path>` overrides workspace directory.

`clawdex plugins list`
1. Description: List installed plugins and their assets.
2. Options:
   - `--include-disabled` include disabled plugins.
   - `--state-dir <path>` overrides state directory.
   - `--workspace <path>` overrides workspace directory.

`clawdex plugins add`
1. Description: Install a plugin from a local path (folder/archive) or npm spec.
2. Options:
   - `--path <dir|archive>` plugin root folder or archive (`.tgz`, `.tar.gz`, `.tar`, `.zip`).
   - `--npm <spec>` npm spec (example: `@openclaw/telegram`).
   - `--link` create a symlink instead of copying (local path only).
   - `--source <text>` optional source label.
   - `--state-dir <path>` overrides state directory.
   - `--workspace <path>` overrides workspace directory.

Bundled Claude plugins are installed automatically on first run (when available).

Built-in output builder plugin (spreadsheets, decks, reports):
- `clawdex-artifacts` (source: `bundled-claude`)

`clawdex plugins update`
1. Description: Update installed plugins (npm installs only).
2. Options:
   - `--id <pluginId>` plugin id (omit with `--all`).
   - `--all` update all tracked npm plugins.
   - `--dry-run` show what would change without writing.
   - `--state-dir <path>` overrides state directory.
   - `--workspace <path>` overrides workspace directory.

`clawdex plugins enable`
1. Description: Enable a plugin and sync its skills.
2. Options:
   - `--id <pluginId>` plugin id.
   - `--state-dir <path>` overrides state directory.
   - `--workspace <path>` overrides workspace directory.

`clawdex plugins disable`
1. Description: Disable a plugin and remove its skills.
2. Options:
   - `--id <pluginId>` plugin id.
   - `--state-dir <path>` overrides state directory.
   - `--workspace <path>` overrides workspace directory.

`clawdex plugins remove`
1. Description: Remove a plugin and its stored files.
2. Options:
   - `--id <pluginId>` plugin id.
   - `--keep-files` remove registry entry but keep stored files.
   - `--state-dir <path>` overrides state directory.
   - `--workspace <path>` overrides workspace directory.

`clawdex plugins sync`
1. Description: Re-sync skills for all installed plugins.
2. Options:
   - `--state-dir <path>` overrides state directory.
   - `--workspace <path>` overrides workspace directory.

`clawdex plugins export-mcp`
1. Description: Export merged `.mcp.json` entries to `~/.codex/clawdex/mcp/plugins.json` (or `--output`).
2. Options:
   - `--output <path>` output file.
   - `--state-dir <path>` overrides state directory.
   - `--workspace <path>` overrides workspace directory.

`clawdex plugins commands list`
1. Description: List plugin commands (optionally for one plugin).
2. Options:
   - `--id <pluginId>` filter to a single plugin.
   - `--state-dir <path>` overrides state directory.
   - `--workspace <path>` overrides workspace directory.

`clawdex plugins commands run`
1. Description: Run a plugin command via Codex.
2. Options:
   - `--id <pluginId>` plugin id.
   - `--command <name>` command name.
   - `--input <text>` optional input appended to the command prompt.
   - `--codex-path <path>` overrides Codex binary.
   - `--auto-approve` accepts approvals automatically.
   - `--state-dir <path>` overrides state directory.
   - `--workspace <path>` overrides workspace directory.

`clawdex permissions get`
1. Description: Show current permission settings.
2. Options:
   - `--state-dir <path>` overrides state directory.
   - `--workspace <path>` overrides workspace directory.

`clawdex permissions set`
1. Description: Update permission settings and write config.
2. Options:
   - `--internet <on|off|true|false>` toggles sandbox network access.
   - `--read-only <true|false>` toggles read-only workspace sandbox.
   - `--mcp-allow <a,b,c>` allowlisted MCP server names (comma-separated).
   - `--mcp-deny <a,b,c>` denylisted MCP server names (comma-separated).
   - `--mcp-plugin <pluginId=on|off>` override MCP enablement per plugin (repeatable).
   - `--state-dir <path>` overrides state directory.
   - `--workspace <path>` overrides workspace directory.

---

**UI Bridge Env Overrides**
- `CLAWDEX_APPROVAL_MODE` = `deny` (default) or `approve`.
- `CLAWDEX_APPROVAL_POLICY` = `never`, `on-request`, `on-failure`, `unless-trusted`.
- `CLAWDEX_CODEX_CONFIG` = semicolon‑separated `--config key=value` overrides forwarded to Codex.

Approval policy options:
- `never`
- `on-request` (default for `clawdex daemon`)
- `on-failure`
- `unless-trusted`

---

**MCP Tool Surface (Implemented)**

Cron tools:
1. `cron.list({ includeDisabled?: boolean })`
2. `cron.status()`
3. `cron.add(CronJobCreate)`
4. `cron.update({ id?: string, jobId?: string, patch: CronJobPatch })`
5. `cron.remove({ id?: string, jobId?: string })`
6. `cron.run({ id?: string, jobId?: string, mode?: "due" | "force" })`
7. `cron.runs({ id?: string, jobId?: string, limit?: number })`

Memory tools:
1. `memory_search({ query, maxResults?, minScore?, sessionKey? })`
2. `memory_get({ path, from?, lines? })`

Artifact tools:
1. `artifact.create_xlsx({ outputPath, sheets, title?, taskRunId? })`
2. `artifact.create_pptx({ outputPath, slides, title?, taskRunId? })`
3. `artifact.create_docx({ outputPath, sections?, title?, taskRunId? })`
4. `artifact.create_pdf({ outputPath, sections?, title?, taskRunId? })`

Messaging tools:
1. `message.send({ channel, to, text|message, accountId?, sessionKey?, bestEffort?, dryRun? })` (queues to gateway outbox)
2. `channels.list()` (returns known routes)
3. `channels.resolve_target({ channel?, to?, accountId? })` (resolves from last routes)

Heartbeat tool:
1. `heartbeat.wake({ reason? })`

---

**Mac App Starter (`macClawdex`)**

Build steps:
1. `cd macClawdex`
2. `xcodegen generate`
3. `DEVELOPMENT_TEAM=YOURTEAMID xcodebuild -project Clawdex.xcodeproj -scheme Clawdex -configuration Debug build`

The build script `Scripts/build_and_embed_rust.sh`:
- Builds **Codex** and **Clawdex** as universal2 Rust binaries (arm64 + x86_64).
- Builds **Clawdexd** (daemon) alongside **Clawdex** and embeds all tools into `Clawdex.app/Contents/Resources/bin/`.
- Codesigns embedded tools with helper entitlements.

If you want a prebuilt `clawdexd` without building the full app, run:
- `macClawdex/Scripts/build_prebuilt_clawdexd.sh`

Plugin commands in the mac app:
- Type `/plugin <id> <command> [input]` in chat to run a plugin command.
- Use the **Commands** button to open the command palette.

Approvals in the mac app:
- Use the **Approvals** tab to review pending approvals and respond to tool input requests.
- Use the **Schedule** tab to create and edit cron jobs.

Override inputs as needed:
- `CODEX_CARGO_ROOT` (default `../codex/codex-rs`)
- `CODEX_BIN` (prebuilt Mach‑O codex binary)
- `CLAWDEX_CARGO_ROOT` (default `../clawdex`)
- `CLAWDEX_BIN` (prebuilt Mach‑O clawdex binary)
- `CLAWDEXD_BIN` (prebuilt Mach‑O clawdexd binary)
- `PREBUILT_DIR` (default `macClawdex/Resources/prebuilt`, looks for `codex`/`clawdex`/`clawdexd` if Rust is unavailable)

---

**Troubleshooting**
1. `codex app-server` fails to start:
   - Ensure `codex` is built and the path passed to `--codex-path` is correct.
2. MCP tools not visible:
   - Ensure Codex `config.toml` includes an MCP server entry pointing to `clawdex mcp-server`.
3. Cron/heartbeat not running:
   - Confirm `cron.enabled` and `heartbeat.enabled` are true in config.
