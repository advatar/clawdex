# Status

- [x] Add artifact service tools (xlsx/pptx/docx/pdf) with schema validation + hashing
- [x] Record artifact events and list outputs at end of task runs
- [x] Add builder skills plugin for spreadsheet/slide/report outputs
- [ ] Add tamper-evident audit log with hash chain for task events/approvals/artifacts
- [ ] Add ActionIntent + risk scoring + checkpoint metadata for approvals/tool calls
- [ ] Add audit export command (events/approvals/artifacts/plugins + audit log)

- [x] Validate MCP tool arguments against OpenClaw JSON schemas
- [x] Expose OpenClaw JSON Schemas in MCP tool definitions
- [x] Default memory embeddings provider/model from Codex config overrides
- [x] Add parity tests for cron normalization edge cases + delivery rules
- [x] Add gateway protocol integration tests + tighten last-route fallback
- [x] Clean up CLI help text now that cron/heartbeat are live
- [x] Implement Rust `clawdex` core (MCP server, cron/memory stores, heartbeat stubs, skills sync, UI bridge)
- [x] Update macOS app embed script to build `codex` + `clawdex` Rust binaries (no Node/OpenClaw runtime)
- [x] Refresh docs for Rust-first flow (README, MAC_APP, COMPATIBILITY)
- [x] Replace remaining `codex-clawd` mentions with `clawdex`
- [x] Build minimal Rust gateway (HTTP + routing, `message.send` delivery)
- [x] Execute cron/heartbeat via Codex runner (`clawdex daemon`) with on-request approvals
- [x] Memory index: SQLite FTS5 + embeddings hybrid search (provider-configurable)
- [x] Update docs for gateway/daemon/memory/approval options + prebuilt binaries flow
- [x] Track `macClawdex/Resources/prebuilt` binaries in Git LFS
- [x] Implement non-stub channel tools (`channels.list`, `channels.resolve_target`) and complete `message.send` options
- [x] Implement WORK.md Phase 0-2 foundations (task DB + task engine + streaming events)
- [x] Implement interactive approvals + tool user input broker for task runs
- [x] Add task CLI (`tasks.*`) and daemon IPC stub for future UI
- [x] Add workspace policy controls (allow roots, deny patterns, read-only, network access toggle)
- [x] Implement plugin manager (install/list/enable/disable, skill sync, MCP export)
- [x] Add plugin command discovery + execution (CLI + macOS app support)
- [x] Add permissions UI plus MCP allow/deny policies
- [x] Add command palette UX for plugin commands
- [x] Add per-plugin MCP toggles in config + UI
- [x] Add richer approval detail rendering (diff/command previews) in mac app
- [x] Add cron schedule UI for creating/editing jobs (mac app)
- [x] Add approvals + user-input UI (macOS) backed by daemon IPC
- [x] Implement cron runner loop + per-job policy execution
- [x] Stop Xcode signing team resets by moving Development Team config to a local override
- [x] Build and stage prebuilt `clawdexd` universal binary
- [x] Add helper script to build/stage `clawdexd` prebuilt binary and document it in README
- [x] Create compat parity spec scaffolding (compat/README.md, clawd-compat.yaml, tool schemas, routing schema, test matrix)
- [x] Add cron schedule unit tests (at/every/cron+tz)
- [x] Fill compat tool schemas with OpenClaw-aligned JSON definitions
- [x] Implement P0 scenario tests from compat/tests/matrix.md
- [x] Implement gateway bridge session mapping + outbound delivery parity
- [x] Produce OpenClaw parity delta checklist and start closing gaps
- [x] Align cron storage format + run log responses with OpenClaw (versioned jobs, state.nextRunAtMs, finished runs)
- [x] Align cron.run semantics with OpenClaw (immediate run + {ok, ran, reason})
- [x] Align cron delivery object behavior and legacy patch merging with OpenClaw
- [x] Fix Claude plugin skill import (skills/<name>/SKILL.md), namespacing, and overlay paths
- [x] Convert plugin commands into namespaced skills with shared renderer semantics
- [x] Add Claude plugin manifest path resolution for skills/commands (additive + ./ rules)

Roadmap:
- [x] Add revised OpenClaw parity roadmap (mac app parity priority, web/admin nice-to-have) in `ROADMAP.md`.
- [x] Include plugin lifecycle + packaging + MCP/skills sync + mac app UX roadmap details.
- [x] Emphasize Rust-first parity and porting OpenClaw TypeScript components where needed.

Parity Execution (from ROADMAP.md):
- [ ] Phase 0: Enforce MCP response strictness + add response-schema regression tests + update `compat/parity-delta.md`.
- [x] Phase 1: Plugin parity (packaging layer, lifecycle parity, metadata/deps validation, permissions mapping, skill/MCP sync hooks).
- [x] Phase 1a: Add OpenClaw plugin manifest support (`openclaw.plugin.json`) + package metadata ingestion.
- [x] Phase 1b: Add plugin install/update semantics (npm spec/archive/path) with dependency install + recorded install source.
- [x] Phase 1c: Align enable/disable/remove flows with OpenClaw semantics + wire skill/MCP sync hooks.
- [x] Phase 1d: Map plugin permission hints into MCP allow/deny + workspace policy overrides (if present).
- [ ] Phase 2: Gateway core parity (auth flows, presence/last-seen, message lifecycle/receipts, channel stack order, attachments).
- [ ] Phase 3: Server methods + lifecycle parity (registry, versioned handlers, plugin method discovery/reload, compat tests).
- [ ] Phase 4: Advanced memory pipeline parity (watchers, batch embeddings queue/retry, cache/eviction, local backend).
- [ ] Phase 5: mac app functional parity (timeline, approvals UI, plugin management UI, connectors, attachments, config UI).
- [ ] Phase 6: Web/admin parity (dashboards + admin workflows where implemented).

Notes:
- `TEMPLATE_PACK.md` is the parity checklist: use it later to fill tool schemas, routing rules, and tests, then implement remaining gaps using the name `Clawdex`.
- `MAC_APP.md` already references the Rust UI bridge and the prebuilt binaries helper; keep it in sync if flow changes.
