# OpenClaw Parity Roadmap (Revised)

Last updated: 2026-02-05

**Priorities**
- Keep the OpenClaw-compatible tool surface at parity (cron, memory, heartbeat, gateway/channel tools, MCP validation).
- Close platform gaps that affect real workflows: gateway core, server methods/lifecycle, plugins, and advanced memory pipeline.
- Mac app functionality must be fully on par with OpenClaw expectations.
- OpenClaw web/admin surfaces are nice-to-have and are deliberately down-prioritized.

**Assumptions**
- Clawdex remains Rust-first unless a Node compatibility shim is clearly cheaper for parity.
- Parity means behavior and semantics, not visual UI fidelity.

**Phase 0 — Parity Guardrails (1–2 weeks)**
Deliverables:
- Enforce MCP response strictness so extra fields are rejected or stripped.
- Add regression tests that fail on schema drift.
- Update `compat/parity-delta.md` to mark MCP response strictness complete.
Dependencies:
- None.
Exit criteria:
- Strict MCP response tests green.
- Parity delta shows 100% for tracked tool surface.

**Phase 1 — Plugin Parity (3–5 weeks)**
Deliverables:
- OpenClaw-style plugin packaging compatibility layer.
- Plugin install, enable, disable, update, and removal semantics aligned with OpenClaw.
- Plugin metadata validation, versioning, and dependency resolution.
- Plugin permissions mapping into MCP allow/deny and workspace policies.
- Skill sync and MCP config export wired to plugin lifecycle events.
Dependencies:
- Existing plugin manager and skill sync.
Exit criteria:
- OpenClaw plugin bundles load and behave correctly.
- Plugin enable/disable and permissions produce expected MCP/skill availability.

**Phase 2 — Gateway Core Parity (4–6 weeks)**
Deliverables:
- Auth: token issuance, validation, rotation, and device auth flows.
- Presence and last-seen semantics.
- Rich session and message lifecycle parity, including delivery receipts.
- Channel plugin stack execution order and configuration parity.
- Attachments: upload, metadata, storage abstraction, and routing integration.
Dependencies:
- Plugin parity for channel stack integration.
Exit criteria:
- End-to-end gateway integration tests pass against OpenClaw behavioral expectations.

**Phase 3 — Server Methods + Lifecycle Semantics (4–6 weeks)**
Deliverables:
- Server method registry with versioned handlers and OpenClaw lifecycle hooks.
- Plugin method discovery and dynamic reload semantics.
- Optional Node compatibility shim for any methods not worth porting yet.
- Migration and compatibility tests for method signatures and error semantics.
Dependencies:
- Plugin parity and gateway core parity.
Exit criteria:
- OpenClaw server method behaviors match documented semantics in integration tests.

**Phase 4 — Advanced Memory Pipeline Parity (3–5 weeks)**
Deliverables:
- Watchers for incremental indexing of filesystem changes.
- Batch embeddings with queueing, retries, and throttling.
- Cache modes and eviction policies.
- Local embedding backend option with fallback to remote providers.
Dependencies:
- Stable memory index and config pipeline.
Exit criteria:
- Functional parity for watchers, batch embeddings, cache modes, and local backends.

**Phase 5 — Mac App Functional Parity (4–6 weeks, overlaps Phases 1–4)**
Deliverables:
- Full task timeline and streaming event UX for long-running runs.
- Approval review UI for plans, file diffs, and command execution.
- Plugin management UI: install, enable/disable, permissions, and command palette.
- Connector/MCP visibility and permission control.
- Attachment UX and gateway health/sessions UI.
- Config UI for workspace policy, network, and memory settings.
Dependencies:
- Gateway core parity and plugin parity for full functionality.
Exit criteria:
- Mac app supports all OpenClaw-equivalent workflows end-to-end.

**Phase 6 — OpenClaw Web/Admin Surfaces (Nice-to-Have, 6–10 weeks)**
Deliverables:
- Admin dashboards and session oversight tools.
- Web UI for plugin, memory, and gateway management.
Dependencies:
- Phases 1–5 completed.
Exit criteria:
- Parity for admin workflows where implemented.

**Cross-Cutting Work (ongoing)**
Deliverables:
- Compatibility and integration test suites for gateway, plugins, memory, and server methods.
- Observability: structured logs and diagnostics for daemon, gateway, and mac app.
- Migration tooling and documented upgrade paths.

