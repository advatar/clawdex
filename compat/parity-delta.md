# OpenClaw TypeScript Parity Delta Checklist

This checklist compares Clawdex behavior to the OpenClaw TypeScript implementation. It tracks deltas to close for full parity.

Legend:
- [ ] Open
- [~] Partial
- [x] Done

## Cron
- [x] Accept wrapped inputs (`job` / `data`) for cron.add/cron.update.
- [x] Coerce schedule fields: `expr` → cron, `tz` → timezone, `at` string → `atMs`.
- [x] Infer schedule kind when missing (`at`, `every`, `cron`).
- [x] Default `wakeMode` to `next-heartbeat` when missing.
- [x] Default `sessionTarget` based on payload kind.
- [x] Default `deleteAfterRun=true` for `at` schedules.
- [x] Delivery parity: OpenClaw uses `delivery` object + legacy migration; Clawdex now normalizes/merges delivery and strips legacy fields.
- [x] Patch merging: payload patches merge and delivery object patch rules are applied.
- [x] Store format parity (`{ version: 1, jobs: [...] }` vs array).
- [x] State fields parity (`state.nextRunAtMs`, `runningAtMs`, `lastStatus`, `lastError`, etc.).
- [x] cron.list sorting by `state.nextRunAtMs`.
- [x] cron.run semantics: OpenClaw executes job immediately and returns `{ ok, ran, reason? }`; Clawdex now uses daemon fast-path and returns the same shape.
- [x] cron.runs log format parity (OpenClaw `action: finished` vs Clawdex run log schema).

## Memory
- [x] Response shape parity: `startLine/endLine`, `snippet`, `provider/model`, `citations` included.
- [x] Chunking + overlap behavior (OpenClaw ~400 token chunks, 80 token overlap).
- [x] Session transcript indexing + `sessionKey`-aware results.
- [x] `memorySearch.extraPaths` allowlist support.
- [x] Citation rules based on `sessionKey` (group/channel suppression).

## Heartbeat
- [x] Suppress delivery when response == `HEARTBEAT_OK`.
- [x] Active hours gating and timezone support.
- [x] Heartbeat config parity (delivery routing, per-agent behavior).

## Gateway / Channels
- [x] Last-route fallback supported for outbound delivery.
- [x] Immediate send vs queue parity (OpenClaw sends; Clawdex queues to outbox when gateway running).
- [x] `channels.list` disabled flag parity (OpenClaw returns `disabled: true` when gateway not configured).
- [x] Resolve target parity for `channel: "last"` semantics.
- [x] Gateway protocol parity (WebSocket framing / server-methods behavior).

## MCP Tool Shapes
- [x] Tool request schemas aligned and validated against OpenClaw JSON schemas.
- [x] Enforce OpenClaw validation errors (invalid request codes/messages).
- [x] Enforce MCP response strictness (sanitize + validate tool outputs against response schemas).

## Tests
- [x] P0 scenario tests (cron persistence, isolated session key, heartbeat suppression, memory line ranges, last-route delivery).
- [x] Add parity tests for cron normalization edge cases and delivery object rules.
- [x] Add gateway protocol integration tests.
