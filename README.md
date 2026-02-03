# clawdex

clawdex is a compatibility runtime that lets Codex behave like an OpenClaw-grade assistant by exposing OpenClaw-compatible tools via MCP, plus cron and heartbeat scheduling, memory access, and a skills sync workflow. In this repo, clawdex is implemented as an `openclaw` CLI subcommand.

This README focuses on local development setup and the clawdex CLI surface.

**Quick Start**
1. Initialize submodules: `git submodule update --init --recursive`
2. Install dependencies: `cd openclaw && pnpm install`
3. Run MCP server: `pnpm openclaw clawd mcp-server`

---

**Prereqs**
1. Node.js 22+ and `pnpm`
2. A working Codex binary on your PATH as `codex` (used by clawdex to spawn `codex app-server`)
3. macOS Xcode + CLT only if you plan to build the macOS app starter in `clawd-macos-app-starter`

---

**Repo Layout**
1. `openclaw/` contains the clawdex implementation and CLI integration.
2. `codex/` is a submodule for the Codex CLI/app-server.
3. `clawd-macos-app-starter/` is a SwiftUI starter app that embeds `codex` and `clawdex`.

---

**Development Setup**
1. Initialize submodules:
   - `git submodule update --init --recursive`
2. Install dependencies:
   - `cd openclaw`
   - `pnpm install`
3. Build and typecheck:
   - `pnpm build`
   - `pnpm check`
4. Run tests (optional):
   - `pnpm vitest run src/clawd/memory.test.ts src/clawd/skills-sync.test.ts`

---

**Running clawdex**

clawdex is exposed through the OpenClaw CLI:

```bash
pnpm openclaw clawd <command> [options]
```

The runtime spawns `codex app-server` internally. Ensure `codex` is resolvable on PATH or set `codex.command` in the clawdex config.

---

**Configuration**

clawdex reads JSON5 config from:

1. `~/.codex/clawd/config.json` (default)
2. `CODEX_CLAWD_CONFIG_PATH` (override path)

Other path overrides:
1. `CODEX_CLAWD_STATE_DIR` or `CODEX_STATE_DIR`
2. `CODEX_CLAWD_WORKSPACE_DIR` or `CODEX_WORKSPACE_DIR`

Default state layout:
1. `~/.codex/clawd/config.json`
2. `~/.codex/clawd/cron/jobs.json`
3. `~/.codex/clawd/cron/runs/<jobId>.jsonl`
4. `~/.codex/clawd/memory/<agentId>.sqlite`
5. `~/.codex/clawd/sessions.json`
6. `~/.codex/clawd/workspace/MEMORY.md`
7. `~/.codex/clawd/workspace/memory/YYYY-MM-DD.md`
8. `~/.codex/clawd/workspace/HEARTBEAT.md` (optional)

Example `config.json` (JSON5 is allowed):

```json5
{
  codex: {
    command: "codex",
    args: ["app-server"],
    cwd: "/path/to/workspace",
    approvalPolicy: "on-request",
    sandbox: "workspace-write",
    autoApprove: false,
    model: "gpt-4.1",
    modelProvider: "openai",
    baseInstructions: "You are Clawdex.",
    developerInstructions: "Prefer concise replies."
  },
  cron: { enabled: true },
  heartbeat: {
    enabled: true,
    intervalMs: 1800000,
    activeHours: { start: "09:00", end: "17:00", timezone: "local" }
  },
  memory: { enabled: true, citations: "auto" },
  gateway: {
    url: "ws://127.0.0.1:18789",
    token: "optional-token"
  },
  sessions: { storePath: "~/.codex/clawd/sessions.json" }
}
```

---

**CLI Reference**

clawdex is accessed through `openclaw clawd`. The commands below are the canonical interface.

`openclaw clawd mcp-server`
1. Description: Run the clawdex MCP server (cron + memory + messaging).
2. Options:
   - `--no-cron` disables the cron scheduler.
   - `--no-heartbeat` disables the heartbeat loop.

`openclaw clawd daemon`
1. Description: Run the background daemon (cron + heartbeat).
2. Options: none.

`openclaw clawd skills sync`
1. Description: Sync OpenClaw skills into Codex skill directories.
2. Options:
   - `--prefix <prefix>` sets a skill name prefix. Default: `oc-`.
   - `--link` uses symlinks instead of copies.
   - `--dry-run` prints a summary without writing files.
   - `--user-dir <dir>` sets the user skills target directory. Default: `~/.codex/skills/openclaw`.
   - `--repo-dir <dir>` sets the repo skills target directory. Default: `.codex/skills/openclaw`.
   - `--source-dir <dir>` sets the OpenClaw skills source directory. Default: `openclaw/skills`.

---

**MCP Tool Surface**

The clawdex MCP server exposes OpenClaw-compatible tool names so OpenClaw skills can run without edits.

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

Messaging tools:
1. `message.send({ channel, to, text|message, accountId?, sessionKey?, bestEffort?, dryRun? })`
2. `channels.list()` (stub)
3. `channels.resolve_target({ channel?, to?, accountId? })` (stub)

Heartbeat tool:
1. `heartbeat.wake({ reason? })`

---

**Mac App Starter**

If you want to run the macOS app starter:
1. `cd clawd-macos-app-starter`
2. `xcodegen generate`
3. `xcodebuild -project Clawdex.xcodeproj -scheme Clawdex -configuration Debug build`

The app embeds `codex` and `clawdex` binaries under `Clawdex.app/Contents/Resources/bin/` and launches `clawdex ui-bridge` per `MAC_APP.md`.

---

**Troubleshooting**

1. `codex app-server` not found:
   - Ensure `codex` is on PATH or set `codex.command` in config.
2. Cron/heartbeat not running:
   - Confirm `cron.enabled` and `heartbeat.enabled` are true in config.
3. MCP tools not visible:
   - Ensure Codex `config.toml` points to the clawdex MCP server (stdio or HTTP).
