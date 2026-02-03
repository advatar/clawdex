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
clawdex daemon --workspace /path/to/workspace --state-dir /path/to/state
```

The daemon is a stub runner today: it persists cron runs and heartbeat logs but does not execute Codex turns yet.

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

Default state layout:
1. `~/.codex/clawdex/config.json` (optional)
2. `~/.codex/clawdex/cron/jobs.json`
3. `~/.codex/clawdex/cron/runs/<jobId>.jsonl`
4. `~/.codex/clawdex/memory/<agentId>.sqlite` (future; index placeholder)
5. `~/.codex/clawdex/sessions.json`
6. `WORKSPACE/MEMORY.md`
7. `WORKSPACE/memory/YYYY-MM-DD.md`
8. `WORKSPACE/HEARTBEAT.md` (optional)

Example `config.json5`:

```json5
{
  workspace: "/path/to/workspace",
  cron: { enabled: true },
  heartbeat: { enabled: true, interval_ms: 1800000 },
  memory: { enabled: true, citations: "auto" }
}
```

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
1. Description: Run the background loop for cron + heartbeat persistence.
2. Options:
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

---

**UI Bridge Env Overrides**
- `CLAWDEX_APPROVAL_MODE` = `deny` (default) or `approve`.
- `CLAWDEX_APPROVAL_POLICY` = `never`, `on-request`, `on-failure`, `unless-trusted`.
- `CLAWDEX_CODEX_CONFIG` = semicolon‑separated `--config key=value` overrides forwarded to Codex.

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

Messaging tools (stub):
1. `message.send({ channel, to, text|message, accountId?, sessionKey?, bestEffort?, dryRun? })`
2. `channels.list()`
3. `channels.resolve_target({ channel?, to?, accountId? })`

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
- Embeds them into `Clawdex.app/Contents/Resources/bin/`.
- Codesigns embedded tools with helper entitlements.

Override inputs as needed:
- `CODEX_CARGO_ROOT` (default `../codex/codex-rs`)
- `CODEX_BIN` (prebuilt Mach‑O codex binary)
- `CLAWDEX_CARGO_ROOT` (default `../clawdex`)
- `CLAWDEX_BIN` (prebuilt Mach‑O clawdex binary)

---

**Troubleshooting**
1. `codex app-server` fails to start:
   - Ensure `codex` is built and the path passed to `--codex-path` is correct.
2. MCP tools not visible:
   - Ensure Codex `config.toml` includes an MCP server entry pointing to `clawdex mcp-server`.
3. Cron/heartbeat not running:
   - Confirm `cron.enabled` and `heartbeat.enabled` are true in config.
