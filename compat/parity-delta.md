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
- [~] Delivery parity: OpenClaw uses `delivery` object + legacy migration; Clawdex still uses `deliver/channel/to/bestEffort` fields.
- [~] Patch merging: payload patches merge; still missing delivery object merge rules.
- [ ] Store format parity (`{ version: 1, jobs: [...] }` vs array).
- [ ] State fields parity (`state.nextRunAtMs`, `runningAtMs`, `lastStatus`, `lastError`, etc.).
- [ ] cron.list sorting by `state.nextRunAtMs`.
- [ ] cron.run semantics: OpenClaw executes job immediately and returns `{ ok, ran, reason? }`; Clawdex currently queues.
- [ ] cron.runs log format parity (OpenClaw `action: finished` vs Clawdex run log schema).

## Memory
- [~] Response shape parity: `startLine/endLine`, `snippet`, `provider/model`, `citations` now included. Chunking rules still differ.
- [ ] Chunking + overlap behavior (OpenClaw ~400 token chunks, 80 token overlap).
- [ ] Session transcript indexing + `sessionKey`-aware results.
- [ ] `memorySearch.extraPaths` allowlist support.
- [ ] Citation rules based on `sessionKey` (group/channel suppression).

## Heartbeat
- [x] Suppress delivery when response == `HEARTBEAT_OK`.
- [ ] Active hours gating and timezone support.
- [ ] Heartbeat config parity (delivery routing, per-agent behavior).

## Gateway / Channels
- [~] Last-route fallback supported for outbound delivery.
- [ ] Immediate send vs queue parity (OpenClaw sends; Clawdex queues to outbox).
- [ ] `channels.list` disabled flag parity (OpenClaw returns `disabled: true` when gateway not configured).
- [ ] Resolve target parity for `channel: "last"` semantics.
- [ ] Gateway protocol parity (WebSocket framing / server-methods behavior).

## MCP Tool Shapes
- [~] Tool request schemas aligned; response shapes include required fields but may include extra fields.
- [ ] Enforce OpenClaw validation errors (invalid request codes/messages).

## Tests
- [x] P0 scenario tests (cron persistence, isolated session key, heartbeat suppression, memory line ranges, last-route delivery).
- [ ] Add parity tests for cron normalization edge cases and delivery object rules.
- [ ] Add gateway protocol integration tests.
