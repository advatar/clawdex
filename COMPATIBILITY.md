# Codex + OpenClaw Compatibility Spec

This document defines the Codex/OpenClaw compatibility contract implemented by `clawdex`.

## Tool Surface (MCP)

The MCP server exposes OpenClaw-compatible tool names so OpenClaw skills can be reused without edits.

### Cron

- `cron.list({ includeDisabled?: boolean }) -> { jobs: CronJob[] }`
- `cron.status() -> { enabled, storePath, jobs, nextWakeAtMs }`
- `cron.add(CronJobCreate) -> CronJob`
- `cron.update({ id?: string, jobId?: string, patch: CronJobPatch }) -> CronJob`
- `cron.remove({ id?: string, jobId?: string }) -> { ok, removed }`
- `cron.run({ id?: string, jobId?: string, mode?: "due" | "force" }) -> { ok, ran, reason? }`
- `cron.runs({ id?: string, jobId?: string, limit?: number }) -> { entries: CronRunLogEntry[] }`

### Memory

- `memory_search({ query, maxResults?, minScore?, sessionKey? })`
- `memory_get({ path, from?, lines? })`

`memory_get` only permits `MEMORY.md` and `memory/**/*.md` relative to the workspace.

### Messaging (stub-compatible)

- `message.send({ channel, to, text|message, accountId?, sessionKey?, bestEffort?, dryRun? })`
- `channels.list()` (stub)
- `channels.resolve_target({ channel?, to?, accountId? })` (stub)

### Heartbeat

- `heartbeat.wake({ reason? })`

## State Directory Layout

Default state root:

- `~/.codex/clawdex/`
  - `config.json` (optional)
  - `cron/jobs.json`
  - `cron/runs/<jobId>.jsonl`
  - `memory/<agentId>.sqlite` (index, when embeddings are available)
  - `sessions.json`
  - `workspace/`
    - `MEMORY.md`
    - `memory/YYYY-MM-DD.md`
    - `HEARTBEAT.md` (optional)

The state directory can be overridden via `CLAWDEX_STATE_DIR` (or `CODEX_CLAWD_STATE_DIR`).

## Session + Delivery Semantics

- Main session key: `agent:main:main`.
- Cron isolated sessions use `agent:main:cron:<jobId>`.
- If `message.send` succeeds, `sessions.json` updates the last route for the session.
- Cron delivery rules:
  - If `payload.to` is provided, delivery occurs even when `payload.deliver` is omitted.
  - If `payload.channel`/`payload.to` are omitted, delivery falls back to the last route.
  - If no route exists and `bestEffortDeliver` is `true`, the run is marked `skipped` instead of `error`.

## Cron Payload Semantics

- `sessionTarget: "main"` requires `payload.kind: "systemEvent"`.
- `sessionTarget: "isolated"` requires `payload.kind: "agentTurn"`.
- Isolated runs prepend `[cron:<jobId> <job.name>]` and the current ISO timestamp to the prompt.

## Heartbeat Contract

- Interval default: 30 minutes.
- If `HEARTBEAT.md` exists and is effectively empty, heartbeat is skipped.
- `HEARTBEAT_OK` responses are suppressed from delivery.
- Active hours are honored when configured in `config.json`.
