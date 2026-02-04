# Codex ⇄ OpenClaw Compatibility Spec

This folder defines the parity contract for instrumenting Codex to behave like an OpenClaw/ClawdBot runtime.

## Rules
1. This spec is the definition of “done”.
2. Tools listed in `clawd-compat.yaml` must exist as callable MCP tools with request/response schemas.
3. Skills listed in `skills-inventory.yaml` must run without editing their bodies, except for documented shims.
4. Any naming mismatch must be handled inside `clawdex` (tool aliasing), not in skills.
5. Upstream Codex must remain unmodified unless listed in `meta.exceptions`.

## How to update
- Inventory tool usage from OpenClaw skills:
  - `rg -n "cron\\.|memory_(search|get)|message\\.send|channels\\." path/to/openclaw/skills`
- Copy canonical tool schemas into `tool-schemas/`.
- Update each tool status: `todo → stub → wired → complete`.
- Add tests in `tests/matrix.md` and fixtures under `tests/fixtures/`.

## Status meanings
- `todo`: not implemented
- `stub`: exists, returns correct shape but not full behavior
- `wired`: implemented, missing tests or edge cases
- `complete`: behavior + tests match spec
- `deprecated`: supported for backwards compatibility only

## Deliverables
- `clawdex` MCP server implements all tools in this spec.
- `clawdex` scheduler implements cron + heartbeat semantics.
- Optional gateway bridge implements message routing/delivery.
