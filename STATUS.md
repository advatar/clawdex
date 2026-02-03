# Status

- [x] Implement Rust `clawdex` core (MCP server, cron/memory stores, heartbeat stubs, skills sync, UI bridge)
- [x] Update macOS app embed script to build `codex` + `clawdex` Rust binaries (no Node/OpenClaw runtime)
- [x] Refresh docs for Rust-first flow (README, MAC_APP, COMPATIBILITY)
- [x] Replace remaining `codex-clawd` mentions with `clawdex`

Notes:
Use `TEMPLATE_PACK.md` later as the parity spec checklist: fill tool schemas, routing rules, and tests, then implement remaining gaps with the name `clawdex`.
MAC_APP.md updated to match `clawdex ui-bridge` and refreshed Clawdex naming.
