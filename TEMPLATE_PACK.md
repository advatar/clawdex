Below is a **drop-in template pack** you can use as the single “source of truth” for parity: tools, schemas, skill dependencies, routing rules, and the test matrix. It’s designed so you can fill it mostly by **grepping OpenClaw skills + tool definitions**, then implement against it in your **`clawdex` MCP server + scheduler daemon**.

---

## 1) Suggested repo layout

```text
compat/
  README.md
  clawd-compat.yaml
  skills-inventory.yaml
  tool-schemas/
    cron.add.request.schema.json
    cron.add.response.schema.json
    cron.update.request.schema.json
    cron.list.response.schema.json
    memory_search.request.schema.json
    memory_search.response.schema.json
    memory_get.request.schema.json
    memory_get.response.schema.json
    message.send.request.schema.json
    message.send.response.schema.json
  routing/
    routes.schema.json
    routes.example.json
  tests/
    matrix.md
    fixtures/
      sample-memory/
        MEMORY.md
        memory/2026-02-01.md
      sample-cron/
        jobs.json
```

---

## 2) `compat/README.md` template (how your team uses this)

```md
# Codex ⇄ OpenClaw Compatibility Spec

This folder defines the parity contract for instrumenting Codex to behave like an OpenClaw/ClawdBot runtime.

## Rules
1) This spec is the only definition of “done”.
2) Tools listed under `tools[]` MUST exist as callable MCP tools with request/response schemas.
3) Skills listed under `skills-inventory.yaml` MUST run without editing their bodies, except for documented shims.
4) Any divergence from OpenClaw naming MUST be handled inside `clawdex` (tool aliasing), not in skills.
5) Upstream Codex must remain unmodified unless explicitly approved in `meta.exceptions`.

## How to fill this spec
- Collect tool usage from OpenClaw skills:
  - ripgrep: `rg -n "cron\\.|memory_(search|get)|message\\.send|channels\\.|gateway\\." path/to/openclaw/skills`
- Collect tool definitions from OpenClaw runtime (source-of-truth):
  - locate the OpenClaw tool registry, copy schemas into `tool-schemas/*.json`
- Update `clawd-compat.yaml`:
  - mark each tool status: todo → stub → wired → complete
- Keep the schemas accurate:
  - request/response JSON Schema should match OpenClaw exactly (or include a translation rule).

## Definition of status
- todo: not implemented
- stub: exists, returns correct shape but not full behavior
- wired: implemented, but missing 1+ acceptance tests
- complete: behavior + tests match spec
- deprecated: supported for backwards compat only

## Test policy
- Every tool has:
  - 1 unit test for schema validation
  - 1 integration test for behavior
- Every top-tier skill has:
  - 1 scenario test proving it completes end-to-end

## Deliverables
- `clawdex` MCP server implements all tools in this spec.
- `clawdex` scheduler implements cron + heartbeat semantics.
- Optional gateway bridge implements message routing/delivery.

```

---

## 3) The main spec: `compat/clawd-compat.yaml` (authoritative checklist)

> This is the one file you review in PRs. Treat it like an API contract.

```yaml
meta:
  spec_version: "0.1.0"
  goal: "Codex runtime with OpenClaw/ClawdBot tool & behavior parity via MCP + sidecar daemon"
  updated: "2026-02-03"
  upstreams:
    codex:
      repo: "YOUR_FORK_OR_UPSTREAM_URL"
      commit: "FILL_ME"
      integration_mode: "app_server"  # app_server | cli_driver
    openclaw:
      repo: "YOUR_REPO_URL"
      commit: "FILL_ME"
  constraints:
    - "Prefer extension mechanisms (MCP + Skills) over patching Codex core"
    - "Any naming mismatch resolved in clawdex tool aliasing"
  exceptions: []
  owners:
    - handle: "FILL_ME"
      area: "scheduler+cron"
    - handle: "FILL_ME"
      area: "memory"
    - handle: "FILL_ME"
      area: "gateway+routing"

runtime:
  codex_runner:
    mode: "app_server"  # preferred
    thread_keying: "routeKey"  # routeKey = channel+sender or explicit session id
  state_dirs:
    root: "~/.codex/clawd"
    cron:
      jobs_json: "~/.codex/clawd/cron/jobs.json"
      runs_dir: "~/.codex/clawd/cron/runs"
    memory:
      index_dir: "~/.codex/clawd/memory"
      workspace_default: "~/clawd-workspace"
    routing:
      routes_json: "~/.codex/clawd/routing/routes.json"
  concurrency:
    max_concurrent_runs: 1
    queue_policy: "fifo"
    drop_policy: "never"  # never | drop_if_over_limit | drop_oldest

mcp:
  server_id: "clawd"
  transport: "stdio"  # stdio | http
  command: "clawdex"
  args: ["mcp-server"]
  env:
    # API keys etc. resolved externally; keep secrets out of this spec
    OPENAI_API_KEY: "${OPENAI_API_KEY}"

# ------------------------------------------------------------------------------
# Tool contract: each entry = 1 callable MCP tool
# ------------------------------------------------------------------------------
tools:
  # ==========================
  # Scheduling / Cron (OpenClaw-compatible)
  # ==========================
  - name: "cron.add"
    status: "todo"
    priority: "P0"
    openclaw:
      canonical_name: "cron.add"
      used_by_skills: []
      source_refs:
        - "openclaw: tools/cron add definition (fill exact file+line)"
      request_schema: "tool-schemas/cron.add.request.schema.json"
      response_schema: "tool-schemas/cron.add.response.schema.json"
    clawdex:
      implemented_in: "scheduler/cron.rs::add"
      idempotency: "optional" # required | optional | none
      side_effects:
        - "disk_write: jobs.json"
      permissions:
        - "disk_write"
    acceptance:
      - "Persists job to jobs.json"
      - "Validates schedule kinds: at/every/cron"
      - "Supports wakeMode and isolation semantics"
      - "Returns stable jobId"
    tests:
      unit:
        - id: "cron_add_schema_validation"
          fixture: "tool-schemas/cron.add.request.schema.json"
      integration:
        - id: "cron_add_every_runs"
          steps:
            - "call cron.add schedule=every(5000ms) payload=systemEvent"
            - "wait >= 1 interval"
            - "verify run appended to runs/<jobId>.jsonl"
          asserts:
            - "jobs.json contains enabled job"
            - "run log contains success or captured error"

  - name: "cron.update"
    status: "todo"
    priority: "P0"
    openclaw:
      canonical_name: "cron.update"
      used_by_skills: []
      source_refs: ["openclaw: tools/cron update definition (fill)"]
      request_schema: "tool-schemas/cron.update.request.schema.json"
      response_schema: "tool-schemas/cron.add.response.schema.json"
    clawdex:
      implemented_in: "scheduler/cron.rs::update"
      idempotency: "optional"
      side_effects: ["disk_write: jobs.json"]
      permissions: ["disk_write"]
    acceptance:
      - "Patch semantics match OpenClaw (null clears agentId, etc.)"
      - "Update does not reorder unrelated jobs"
    tests:
      integration:
        - id: "cron_update_disable"
          steps:
            - "create job"
            - "call cron.update enabled=false"
          asserts:
            - "job enabled=false"
            - "no further runs occur"

  - name: "cron.list"
    status: "todo"
    priority: "P0"
    openclaw:
      canonical_name: "cron.list"
      used_by_skills: []
      source_refs: ["openclaw: tools/cron list definition (fill)"]
      request_schema: "tool-schemas/cron.list.request.schema.json"
      response_schema: "tool-schemas/cron.list.response.schema.json"
    clawdex:
      implemented_in: "scheduler/cron.rs::list"
      side_effects: []
      permissions: ["disk_read"]
    acceptance:
      - "Returns all jobs with computed nextRunAt"
      - "Supports filters (if OpenClaw supports them)"
    tests:
      integration:
        - id: "cron_list_next_run"
          steps:
            - "create job with cron expr"
            - "call cron.list"
          asserts:
            - "nextRunAt present and parseable"

  - name: "cron.remove"
    status: "todo"
    priority: "P1"
    openclaw:
      canonical_name: "cron.remove"
      used_by_skills: []
      source_refs: ["openclaw: tools/cron remove definition (fill)"]
      request_schema: "tool-schemas/cron.remove.request.schema.json"
      response_schema: "tool-schemas/cron.remove.response.schema.json"
    clawdex:
      implemented_in: "scheduler/cron.rs::remove"
      side_effects: ["disk_write: jobs.json"]
      permissions: ["disk_write"]
    acceptance:
      - "Removes job and stops scheduling"
      - "Does not delete run history unless OpenClaw does"
    tests:
      integration:
        - id: "cron_remove_stops_runs"
          steps:
            - "create job"
            - "remove job"
          asserts:
            - "no runs after removal"

  # ==========================
  # Heartbeat (proactive loop)
  # ==========================
  - name: "heartbeat.wake"
    status: "todo"
    priority: "P1"
    openclaw:
      canonical_name: "heartbeat.wake"
      used_by_skills: []
      source_refs: ["openclaw: heartbeat wake definition (fill)"]
      request_schema: "tool-schemas/heartbeat.wake.request.schema.json"
      response_schema: "tool-schemas/heartbeat.wake.response.schema.json"
    clawdex:
      implemented_in: "scheduler/heartbeat.rs::wake"
      side_effects: ["may_run_turn"]
      permissions: ["may_trigger_agent_turn"]
    acceptance:
      - "Triggers a main-session heartbeat turn"
      - "Suppresses delivery if response == HEARTBEAT_OK (daemon behavior)"
    tests:
      integration:
        - id: "heartbeat_ok_suppressed"
          steps:
            - "set HEARTBEAT.md to no-op instructions"
            - "call heartbeat.wake"
          asserts:
            - "no outbound message delivered"

  # ==========================
  # Memory tools (OpenClaw-compatible)
  # ==========================
  - name: "memory_search"
    status: "todo"
    priority: "P0"
    openclaw:
      canonical_name: "memory_search"
      used_by_skills: []
      source_refs: ["openclaw: memory_search tool definition (fill)"]
      request_schema: "tool-schemas/memory_search.request.schema.json"
      response_schema: "tool-schemas/memory_search.response.schema.json"
    clawdex:
      implemented_in: "memory/search.rs::search"
      side_effects: ["may_update_index_async"]
      permissions: ["disk_read"]
    acceptance:
      - "Searches MEMORY.md + memory/**/*.md"
      - "Returns snippets with file + line ranges"
      - "Stable ranking (FTS baseline), optional hybrid (FTS+embedding)"
    tests:
      integration:
        - id: "memory_search_finds_phrase"
          fixture: "tests/fixtures/sample-memory"
          steps:
            - "index fixture"
            - "call memory_search query='project codename'"
          asserts:
            - ">=1 result"
            - "result includes file path and line range"

  - name: "memory_get"
    status: "todo"
    priority: "P0"
    openclaw:
      canonical_name: "memory_get"
      used_by_skills: []
      source_refs: ["openclaw: memory_get tool definition (fill)"]
      request_schema: "tool-schemas/memory_get.request.schema.json"
      response_schema: "tool-schemas/memory_get.response.schema.json"
    clawdex:
      implemented_in: "memory/get.rs::get"
      side_effects: []
      permissions: ["disk_read"]
    acceptance:
      - "Reads allowed memory paths only"
      - "Returns raw content"
    tests:
      integration:
        - id: "memory_get_reads_file"
          fixture: "tests/fixtures/sample-memory"
          steps:
            - "call memory_get path='MEMORY.md'"
          asserts:
            - "content includes expected heading"

  # ==========================
  # Messaging + routing (gateway bridge)
  # ==========================
  - name: "message.send"
    status: "todo"
    priority: "P0"
    openclaw:
      canonical_name: "message.send"
      used_by_skills: []
      source_refs: ["openclaw: message.send tool definition (fill)"]
      request_schema: "tool-schemas/message.send.request.schema.json"
      response_schema: "tool-schemas/message.send.response.schema.json"
    clawdex:
      implemented_in: "gateway/send.rs::send"
      idempotency: "required"
      side_effects: ["network_send"]
      permissions: ["network_send"]
    acceptance:
      - "Delivers to explicit channel+to"
      - "If missing, uses lastRoute (daemon-managed)"
      - "Idempotency key prevents duplicates"
    tests:
      integration:
        - id: "message_send_idempotent"
          steps:
            - "send same message twice with same idempotency_key"
          asserts:
            - "only 1 message delivered"

# ------------------------------------------------------------------------------
# Cross-cutting semantics (routing, sessions, delivery)
# ------------------------------------------------------------------------------
routing:
  route_key_format: "{channel}:{sender_id}"
  last_route_policy:
    enabled: true
    ttl_ms: 604800000 # 7 days
  delivery:
    default_channel: "fill_or_none"
    suppress_tokens:
      - "HEARTBEAT_OK"

observability:
  logging:
    tool_calls_jsonl: "~/.codex/clawd/logs/tool_calls.jsonl"
    turns_jsonl: "~/.codex/clawd/logs/turns.jsonl"
    cron_runs_jsonl: "~/.codex/clawd/cron/runs/<jobId>.jsonl"
  trace_fields:
    - "trace_id"
    - "job_id"
    - "thread_id"
    - "route_key"

definition_of_done:
  tools:
    p0_all_complete: true
    p1_all_wired_or_complete: true
  skills:
    top_20_skills_pass: true
  scheduler:
    cron_persistence: true
    heartbeat_ok_suppression: true
  gateway:
    round_trip_message: true
  upgrades:
    codex_upstream_update_smoke_test: true
```

---

## 4) Skill inventory template: `compat/skills-inventory.yaml`

This is where you list the OpenClaw skills you’re copying and the tool calls they depend on. The point is to keep “skill parity” measurable.

```yaml
meta:
  updated: "2026-02-03"
  openclaw_commit: "FILL_ME"
  codex_commit: "FILL_ME"

skills:
  - id: "oc-daily-brief"
    source_path: "openclaw/skills/daily-brief"
    codex_path: "~/.codex/skills/openclaw/daily-brief"
    priority: "P0"
    owner: "FILL_ME"
    required_tools:
      - "cron.add"
      - "memory_search"
      - "message.send"
    expected_behavior:
      - "Creates a daily scheduled job"
      - "Pulls memory/context"
      - "Delivers a concise summary message"
    test_scenarios:
      - id: "daily_brief_e2e"
        steps:
          - "install skill"
          - "invoke skill to schedule"
          - "simulate run"
        asserts:
          - "message delivered"
          - "run log appended"

  - id: "oc-inbox-zero"
    source_path: "openclaw/skills/inbox-zero"
    codex_path: "~/.codex/skills/openclaw/inbox-zero"
    priority: "P1"
    owner: "FILL_ME"
    required_tools:
      - "message.send"
      - "memory_search"
      # add email tools as you implement them
    expected_behavior:
      - "Summarizes and triages messages"
    test_scenarios: []

reporting:
  dashboards:
    - name: "Parity by tool"
      query: "all skills where required_tools include status != complete"
```

---

## 5) Test matrix template: `compat/tests/matrix.md`

This is the “done-when checklist” you can use in PR reviews.

```md
# Compatibility Test Matrix

## P0 Tools (must be COMPLETE)
- [ ] cron.add
- [ ] cron.update
- [ ] cron.list
- [ ] memory_search
- [ ] memory_get
- [ ] message.send

## P0 Behaviors (must pass end-to-end)
- [ ] Cron job persists across daemon restart
- [ ] Cron isolated run creates its own Codex thread and can deliver output
- [ ] Cron main-session run injects a system event and triggers a Codex turn
- [ ] Heartbeat runs on interval and suppresses delivery when response == HEARTBEAT_OK
- [ ] Memory search returns file+line ranges and can recall content from MEMORY.md
- [ ] Last-route delivery works (cron deliver omitted → still posts somewhere correct)

## P1 Tools
- [ ] cron.remove
- [ ] heartbeat.wake
- [ ] channels.list / channels.resolve_target (if needed by skills)
- [ ] session.list / session.get (if needed by gateway integration)

## Regression suite (run on every upstream Codex bump)
- [ ] MCP server still registers tools
- [ ] Codex can call each P0 tool
- [ ] One P0 skill scenario passes (daily brief or equivalent)
- [ ] One interactive message round-trip via gateway bridge passes

## Security/Approvals sanity checks
- [ ] No tool can execute shell commands directly unless explicitly approved
- [ ] message.send requires idempotency_key and prevents duplicates
- [ ] memory_get enforces path allowlist (no arbitrary file reads)
```

---

## 6) Optional: extraction commands you’ll actually run

These are practical “fill the inventory” helpers. Keep them in a dev script or in README.

```bash
# 1) find all tool calls in OpenClaw skills (adjust patterns to your codebase conventions)
rg -n "cron\\.|memory_(search|get)|message\\.send|channels\\.|heartbeat\\." openclaw/skills

# 2) build a unique tool list
rg -h "cron\\.|memory_(search|get)|message\\.send|channels\\.|heartbeat\\." -o openclaw/skills \
  | sed 's/[^a-zA-Z0-9_\\.]/\n/g' \
  | rg '^(cron\\.[a-z_]+|memory_(search|get)|message\\.send|heartbeat\\.[a-z_]+)$' \
  | sort -u

# 3) find canonical schema definitions / tool registry in OpenClaw source
rg -n "registerTool|tools\\s*=|memory_search|cron\\.add|message\\.send" openclaw/src
```

---

### How you’ll use this in practice
1) Fill `skills-inventory.yaml` by grep + quick manual grouping into P0/P1.  
2) For each P0 tool, copy its OpenClaw schema into `tool-schemas/*.json`.  
3) Mark tool `status: stub` once callable through MCP with correct shape.  
4) Promote to `wired` once it affects real state (jobs.json, routing, etc.).  
5) Promote to `complete` only when its integration test passes and a P0 skill scenario passes.
