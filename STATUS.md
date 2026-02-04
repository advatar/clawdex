# Status

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
- [ ] Add approvals + user-input UI (macOS) backed by daemon IPC
- [ ] Implement cron runner loop + per-job policy execution

Notes:
- `TEMPLATE_PACK.md` is the parity checklist: use it later to fill tool schemas, routing rules, and tests, then implement remaining gaps using the name `Clawdex`.
- `MAC_APP.md` already references the Rust UI bridge and the prebuilt binaries helper; keep it in sync if flow changes.
