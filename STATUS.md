# Status

- [x] Add clawdex daemon + MCP server scaffold in openclaw (cron, memory, heartbeat, message/channel stubs)
- [x] Implement skills sync for OpenClaw skills into Codex skill paths
- [x] Document compatibility spec + clawd state layout
- [x] Wire CLI entrypoints and basic tests

Notes:
Use `TEMPLATE_PACK.md` later as the parity spec checklist: fill tool schemas, routing rules, and tests, then implement remaining gaps with the name `clawdex` (replace any older `codex-clawd` labels).
Proceed with `MAC_APP.md` next: update any stale names to `Clawdex`/`clawdex` and wire the macOS app to run the `clawdex ui-bridge` contract.
