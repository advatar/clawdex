Below is a concrete build plan that gets you to “Codex + OpenClaw-grade capabilities” while keeping upstream Codex changes low-risk by leaning on **Skills**, **MCP**, and stable **Codex server protocols**. It’s intentionally compatibility-first so you can **copy OpenClaw skills largely as-is** and make Codex “feel” like a ClawdBot without rewriting everything.

---

## Target architecture: “Codex Core + Claw Compatibility Layer”

### Principle
- **Don’t fork the Codex agent loop.** Treat upstream Codex as a black box you *drive* via a stable protocol, and extend it via MCP + Skills. (Codex already supports MCP servers and Skills.)  [oai_citation:0‡OpenAI Developers](https://developers.openai.com/codex/mcp)  
- Implement the missing “assistant platform” pieces (scheduler, memory, gateway/routing) as **sidecars/daemons** that Codex connects to through MCP. This mirrors how OpenClaw keeps cron/heartbeat “outside the model” (in the Gateway) rather than baking scheduling into the LLM.  [oai_citation:1‡OpenClaw](https://docs.openclaw.ai/automation/cron-jobs)

### Recommended component split
1. **Codex (upstream, unmodified)**  
   - Loads Skills (AgentSkills-style `SKILL.md`)  [oai_citation:2‡OpenAI Developers](https://developers.openai.com/codex/skills/create-skill)  
   - Connects to MCP servers via `mcp_servers.*` in `config.toml`  [oai_citation:3‡OpenAI Developers](https://developers.openai.com/codex/config-reference/)  
   - Optionally run via **Codex app-server** (best for long-running “assistant” mode)  [oai_citation:4‡OpenAI Developers](https://developers.openai.com/codex/app-server)  

2. **clawdex (new, yours)** — the “always-on assistant runtime”
   - Runs continuously (like OpenClaw Gateway does) and provides:
     - **Internal scheduler**: cron + heartbeat (OpenClaw semantics)  [oai_citation:5‡OpenClaw](https://docs.openclaw.ai/automation/cron-jobs)  
     - **Persistent memory service**: `memory_search`/`memory_get` compatible tools  [oai_citation:6‡OpenClaw](https://docs.openclaw.ai/concepts/memory)  
     - **Gateway bridge**: route inbound/outbound messages between channels and Codex threads (either reuse OpenClaw Gateway or implement a minimal equivalent)  [oai_citation:7‡OpenClaw](https://docs.openclaw.ai/concepts/architecture)  
   - Exposes an **MCP server** that implements OpenClaw-compatible tool names (cron.*, memory_*, message.*, etc.).

3. **(Optional) Reuse OpenClaw Gateway as the “channels/UI layer”**
   - If you want “all the messaging platforms + dashboard + routing + presence” quickly, the fastest path is: **keep OpenClaw Gateway for channels/UI** and replace its “agent runtime” with Codex.  
   - OpenClaw’s Gateway protocol is WebSocket with a connect handshake and req/res/events framing.  [oai_citation:8‡OpenClaw](https://docs.openclaw.ai/gateway/protocol)  

This combination gets you: **Codex for reasoning + tools; OpenClaw-grade platform features via your daemon and MCP surface**.

---

## Implementation plan (phased) + step-by-step checklist

### Phase 0 — Define the parity contract (1–2 days of work)
Goal: lock down **what “capabilities parity” means** in terms of tool surface + behaviors so skills can be copied, not rewritten.

**Checklist**
- [ ] Inventory OpenClaw tool calls used by your OpenClaw skills (grep for `cron.`, `memory_search`, `memory_get`, message tools, channel tools, etc.).
- [ ] Write a “compatibility spec” document:
  - Tool names + JSON schemas you will implement (start with cron + memory because they unlock automation).
  - Routing semantics (main session vs isolated session, last-route delivery fallback, etc.).  [oai_citation:9‡OpenClaw](https://docs.openclaw.ai/automation/cron-jobs)
- [ ] Decide the state directory layout under `~/.codex/` for this system:
  - `~/.codex/clawd/cron/…`
  - `~/.codex/clawd/memory/…`
  - `~/.codex/clawd/sessions/…`
- [ ] Decide how Codex will be driven for “daemon mode”:
  - **Preferred:** spawn and control `codex app-server` (threads/turns/streamed events).  [oai_citation:10‡OpenAI Developers](https://developers.openai.com/codex/app-server)  
  - **Alternative:** treat Codex as an MCP tool (`codex mcp-server`) and orchestrate turns by calling `codex` / `codex-reply`.  [oai_citation:11‡OpenAI Developers](https://developers.openai.com/codex/guides/agents-sdk)  

**Acceptance criteria**
- You can point at a specific list of OpenClaw tool names and say “these will exist in Codex via MCP, unchanged.”

---

### Phase 1 — Skills: copy first, fix later (same day)
Goal: make Codex *see* OpenClaw skills immediately.

OpenClaw and Codex both use AgentSkills-style skill folders with `SKILL.md` frontmatter. OpenClaw explicitly loads AgentSkills-compatible folders.  [oai_citation:12‡OpenClaw](https://docs.openclaw.ai/tools/skills) Codex’s skill format is also YAML frontmatter in `SKILL.md`, with optional scripts/assets.  [oai_citation:13‡OpenAI Developers](https://developers.openai.com/codex/skills/create-skill)

**Checklist**
- [ ] Create a **sync script** `clawd skills sync` that:
  - Copies or symlinks OpenClaw skill directories into:
    - user scope: `~/.codex/skills/openclaw/<skill>`  [oai_citation:14‡OpenAI Developers](https://developers.openai.com/codex/skills/create-skill)  
    - repo scope (optional): `.codex/skills/openclaw/<skill>`  [oai_citation:15‡OpenAI Developers](https://developers.openai.com/codex/skills/create-skill)  
- [ ] Normalize frontmatter:
  - Ensure each `SKILL.md` has `name` (<=100 chars) and `description` (<=500 chars).  [oai_citation:16‡OpenAI Developers](https://developers.openai.com/codex/skills/create-skill)  
  - Keep OpenClaw extra keys; Codex ignores unknown keys.  [oai_citation:17‡OpenAI Developers](https://developers.openai.com/codex/skills/create-skill)  
- [ ] De-conflict duplicates:
  - Codex does **not** dedupe skills with the same name; decide a renaming policy (e.g., prefix with `oc-` or group by pack).  [oai_citation:18‡OpenAI Developers](https://developers.openai.com/codex/skills/)  
- [ ] Add a profile in `~/.codex/config.toml` dedicated to “assistant mode” and enable only the skill pack you want (using `skills.config` entries).  [oai_citation:19‡OpenAI Developers](https://developers.openai.com/codex/config-reference/)  

**Acceptance criteria**
- Running Codex shows the OpenClaw skills in the loaded skills list.
- No name collisions; skills are discoverable by description.

---

### Phase 2 — Build the OpenClaw-compat MCP server (core enabler)
Goal: implement the OpenClaw tool surface so copied skills “just work”.

Codex can connect to MCP servers via `mcp_servers.<id>…` config entries (stdio or HTTP).  [oai_citation:20‡OpenAI Developers](https://developers.openai.com/codex/mcp)

**Checklist**
- [ ] Create `clawdex mcp-server` (Rust or Node—pick what reuses the most OpenClaw code fastest).
- [ ] Implement tool namespaces (initially):
  - **cron**: `cron.list`, `cron.status`, `cron.add`, `cron.update`, `cron.remove`, `cron.run`, `cron.runs`  [oai_citation:21‡OpenClaw](https://docs.openclaw.ai/automation/cron-jobs)  
  - **memory**: `memory_search`, `memory_get`  [oai_citation:22‡OpenClaw](https://docs.openclaw.ai/concepts/memory)  
  - **messaging (stub first)**: `message.send` (or whatever your skills expect—match names used in your OpenClaw skills)
  - **channels (stub first)**: `channels.list`, `channels.resolve_target`, etc. (can start as pass-through to your gateway later)
- [ ] Add to `~/.codex/config.toml`:
  - `[mcp_servers.clawd]` with `command` + `args` or an HTTP URL (whichever you implement).  [oai_citation:23‡OpenAI Developers](https://developers.openai.com/codex/config-reference/)  

**Acceptance criteria**
- Codex can successfully call `memory_search` and `cron.list` via MCP.
- A copied OpenClaw skill that uses cron/memory can execute without changing the skill text.

---

## Phase 3 — Internal scheduler: Cron (OpenClaw semantics, but owned by Codex)
Goal: deliver a true “assistant scheduler” inside your Codex ecosystem (no OS cron dependency).

OpenClaw cron design details you’ll want to match:
- Jobs persist to `~/.openclaw/cron/jobs.json` and run history to `~/.openclaw/cron/runs/<jobId>.jsonl`.  [oai_citation:24‡OpenClaw](https://docs.openclaw.ai/automation/cron-jobs)  
- Execution styles:
  - main session via system event + next heartbeat
  - isolated session with delivery  [oai_citation:25‡OpenClaw](https://docs.openclaw.ai/automation/cron-jobs)  
- Tool-call JSON schemas include schedule kinds (`at`, `every`, `cron`) and `wakeMode`.  [oai_citation:26‡OpenClaw](https://docs.openclaw.ai/automation/cron-jobs)  

**Checklist**
- [ ] Implement cron store + run log:
  - `~/.codex/clawd/cron/jobs.json`
  - `~/.codex/clawd/cron/runs/<jobId>.jsonl`
  - store fields aligned to OpenClaw schema (`schedule.kind`, `sessionTarget`, `wakeMode`, `payload.kind`, `deleteAfterRun`, `enabled`, etc.).  [oai_citation:27‡OpenClaw](https://docs.openclaw.ai/automation/cron-jobs)  
- [ ] Implement schedule evaluators:
  - `atMs` (epoch ms, one-shot)
  - `everyMs` (fixed interval)
  - `cron expr + tz` (cron parsing + timezone)  [oai_citation:28‡OpenClaw](https://docs.openclaw.ai/automation/cron-jobs)  
- [ ] Implement job runner with concurrency cap (`maxConcurrentRuns` default 1).  [oai_citation:29‡OpenClaw](https://docs.openclaw.ai/automation/cron-jobs)  
- [ ] Implement “main session job” behavior:
  - payload kind `systemEvent`
  - `wakeMode: now` triggers an immediate Codex turn
  - `wakeMode: next-heartbeat` enqueues and waits for heartbeat tick  [oai_citation:30‡OpenClaw](https://docs.openclaw.ai/automation/cron-jobs)  
- [ ] Implement “isolated job” behavior:
  - payload kind `agentTurn` with `message`
  - run in a dedicated thread/session `cron:<jobId>`
  - optional “post-to-main” summary behavior (mirror OpenClaw’s `isolation` block).  [oai_citation:31‡OpenClaw](https://docs.openclaw.ai/automation/cron-jobs)  
- [ ] Implement delivery semantics:
  - If `to` is specified, auto-deliver final output even if `deliver` omitted.
  - If channel/to omitted, fall back to “last route” when available.  [oai_citation:32‡OpenClaw](https://docs.openclaw.ai/automation/cron-jobs)  
- [ ] Expose the cron API exactly through MCP tool calls:
  - `cron.add` takes the documented JSON shape
  - `cron.update` supports patch semantics (`jobId`, `patch`, `agentId: null` clears)  [oai_citation:33‡OpenClaw](https://docs.openclaw.ai/automation/cron-jobs)  

**Acceptance criteria**
- You can reproduce OpenClaw examples with the same JSON shapes (cron.add/cron.update).
- Jobs persist across daemon restarts.
- Runs append JSONL history.

---

## Phase 4 — Internal scheduler: Heartbeat (proactive checks)
OpenClaw heartbeat behavior to match:
- Runs periodic turns in the main session (default 30m), optionally restricted to active hours.
- Reads `HEARTBEAT.md` if present; if nothing needs attention, reply `HEARTBEAT_OK` and suppress delivery.  [oai_citation:34‡OpenClaw](https://docs.openclaw.ai/gateway/heartbeat)

**Checklist**
- [ ] Add heartbeat config to `~/.codex/clawd/config.json` (or map into Codex profiles):
  - interval (default 30m)
  - active hours (timezone-aware)
  - delivery target (default “last route”)  [oai_citation:35‡OpenClaw](https://docs.openclaw.ai/gateway/heartbeat)  
- [ ] Implement heartbeat tick loop (can reuse cron runner internally):
  - for each enabled agent/session, enqueue a heartbeat turn
- [ ] Implement heartbeat prompt contract:
  - include instruction “read HEARTBEAT.md if it exists”
  - suppress sending anything if response is exactly `HEARTBEAT_OK`  [oai_citation:36‡OpenClaw](https://docs.openclaw.ai/gateway/heartbeat)  
- [ ] Add a “manual wake” tool (`heartbeat.wake`) for debugging/on-demand checks (OpenClaw has manual wake concepts).  [oai_citation:37‡OpenClaw](https://docs.openclaw.ai/gateway/heartbeat)  

**Acceptance criteria**
- Heartbeats run on schedule, don’t spam, and can be quiet via `HEARTBEAT_OK`.

---

## Phase 5 — Memory: persistent recall with OpenClaw-style tools
OpenClaw memory design to mirror:
- Source-of-truth is Markdown files in the workspace (`memory/YYYY-MM-DD.md` + optional `MEMORY.md`).  [oai_citation:38‡OpenClaw](https://docs.openclaw.ai/concepts/memory)  
- Tools:
  - `memory_search` returns snippets with file + line ranges
  - `memory_get` reads content by path  [oai_citation:39‡OpenClaw](https://docs.openclaw.ai/concepts/memory)  
- Indexing:
  - per-agent SQLite store, async refresh, optional session indexing, hybrid BM25+vector approach.  [oai_citation:40‡OpenClaw](https://docs.openclaw.ai/concepts/memory)  

**Checklist**
- [ ] Standardize workspace layout for the “assistant agent”:
  - `WORKSPACE/`
    - `MEMORY.md`
    - `memory/YYYY-MM-DD.md`
    - `HEARTBEAT.md` (optional)
- [ ] Implement `memory_get(path)`:
  - allow `MEMORY.md` and `memory/**/*.md`
  - enforce scope rules if you support group contexts (OpenClaw keeps some memory private to main session).  [oai_citation:41‡OpenClaw](https://docs.openclaw.ai/concepts/memory)  
- [ ] Implement `memory_search(query)` in two stages:
  1) **Stage A (fast):** SQLite FTS5 only (BM25-ish keyword search)  
  2) **Stage B (parity):** hybrid search (FTS + embeddings) with merge logic similar to OpenClaw’s described approach.  [oai_citation:42‡OpenClaw](https://docs.openclaw.ai/concepts/memory)  
- [ ] Store index per agent:
  - `~/.codex/clawd/memory/<agentId>.sqlite` (mirroring OpenClaw’s per-agent store concept).  [oai_citation:43‡OpenClaw](https://docs.openclaw.ai/concepts/memory)  
- [ ] Implement indexing lifecycle:
  - file watcher or debounced rebuild on demand
  - do not block `memory_search` on indexing (best-effort async), as OpenClaw does.  [oai_citation:44‡OpenClaw](https://docs.openclaw.ai/concepts/memory)  
- [ ] Implement “memory flush” hook pre-compaction:
  - when you detect a thread nearing compaction, enqueue a silent turn prompting durable memory writes (OpenClaw does this as an internal reminder).  [oai_citation:45‡OpenClaw](https://docs.openclaw.ai/concepts/memory)  

**Acceptance criteria**
- A user can say “remember X” and your system reliably writes it to Markdown, and later `memory_search` finds it.
- `memory_search` and `memory_get` names match OpenClaw so copied skills don’t need edits.

---

## Phase 6 — Gateway + channels: make it omni-channel
This is the “big surface area” part. The fastest parity route is to **reuse OpenClaw’s Gateway for channels/UI** and make Codex the brain behind it.

OpenClaw Gateway basics:
- WebSocket control plane; connect handshake; req/res/events framing; optional token auth; idempotency keys for side-effect methods.  [oai_citation:46‡OpenClaw](https://docs.openclaw.ai/gateway/protocol)  
- Gateway owns channels, sessions, routing.  [oai_citation:47‡OpenClaw](https://docs.openclaw.ai/cli/gateway)  

**Checklist (reuse OpenClaw Gateway route)**
- [ ] Run OpenClaw Gateway with agent execution disabled (or no-op), and cron disabled to avoid double scheduling:
  - cron is normally inside Gateway; disable it so **your Codex scheduler is authoritative**.  [oai_citation:48‡OpenClaw](https://docs.openclaw.ai/automation/cron-jobs)  
- [ ] Implement `clawd-gateway-bridge` (inside `clawdex`):
  - Connect to OpenClaw Gateway WS as operator.
  - Subscribe to inbound message events and resolve channel/session identity.  [oai_citation:49‡OpenClaw](https://docs.openclaw.ai/gateway/protocol)  
- [ ] Implement session mapping:
  - `<channel + sender>` → persistent Codex thread id
  - store mapping in `~/.codex/clawd/sessions.json`
- [ ] Implement outbound delivery:
  - When Codex produces a user-visible message, call Gateway “send” (or equivalent) with an idempotency key.
  - Maintain “last route” per session for cron delivery fallback semantics.  [oai_citation:50‡OpenClaw](https://docs.openclaw.ai/automation/cron-jobs)  
- [ ] Expose messaging tool(s) to Codex via MCP:
  - `message.send({channel,to,text,...})` calls out through the bridge to Gateway.
  - Optional: typing indicators / presence if you want polish.

**Acceptance criteria**
- Send a message in Telegram/WhatsApp/Discord → Codex replies in that same thread.
- Cron “deliver” can post to the correct place.

**If you don’t want to run OpenClaw Gateway at all**
- Build a minimal Rust gateway that supports:
  - inbound webhook/bot updates
  - outbound send
  - route storage
  - (optional) Web UI  
…but this is slower than reusing the existing OpenClaw channel stack.

---

## Phase 7 — Codex driving strategy (daemon-grade)
To behave like a real assistant, you need a long-running driver. Codex app-server is explicitly meant for rich clients and streaming agent events.  [oai_citation:51‡OpenAI Developers](https://developers.openai.com/codex/app-server)

**Checklist**
- [ ] Implement a “Codex runner” abstraction in `clawdex`:
  - start/stop the Codex backend
  - `ensure_thread(routeKey) -> threadId`
  - `run_turn(threadId, input, overrides) -> streamed output`
- [ ] Prefer `codex app-server`:
  - threads and turns are first-class and event-streamed.  [oai_citation:52‡OpenAI Developers](https://developers.openai.com/codex/app-server)  
- [ ] Support per-job overrides for isolated cron jobs:
  - model override
  - verbosity/thinking level analog (map to Codex config fields you control)
- [ ] Make approvals safe by default:
  - set `approval_policy = "on-request"` (or stricter)  [oai_citation:53‡OpenAI Developers](https://developers.openai.com/codex/config-reference/)  
  - set `sandbox_mode = "workspace-write"` for the assistant workspace; avoid `danger-full-access` unless explicitly requested  [oai_citation:54‡OpenAI Developers](https://developers.openai.com/codex/config-reference/)  
  - disable or restrict network in workspace sandbox unless needed (`sandbox_workspace_write.network_access`).  [oai_citation:55‡OpenAI Developers](https://developers.openai.com/codex/config-reference/)  

**Acceptance criteria**
- The daemon can run for hours; cron/heartbeat triggers turns; approvals behave predictably and safely.

---

## Phase 8 — Hardening, regression tests, and “upgrade-proofing”
**Checklist**
- [ ] Add deterministic unit tests for cron schedule calculations (at/every/cron+tz) based on OpenClaw schema examples.  [oai_citation:56‡OpenClaw](https://docs.openclaw.ai/automation/cron-jobs)  
- [ ] Add integration tests:
  - `memory_search` returns stable line-ranged snippets
  - cron jobs persist and run after restart
- [ ] Add end-to-end tests:
  - inbound gateway message → Codex response → outbound send
- [ ] Implement “protocol version pinning”:
  - OpenClaw Gateway protocol has protocol versioning in connect handshake; respect it.  [oai_citation:57‡OpenClaw](https://docs.openclaw.ai/gateway/protocol)  
  - For Codex app-server, generate schemas per version if you build typed clients (Codex can generate TS/JSON schemas).  [oai_citation:58‡OpenAI Developers](https://developers.openai.com/codex/app-server)  
- [ ] Keep all Codex modifications isolated:
  - Ideally **zero changes** to Codex core.
  - If you add a `codex clawd` subcommand, keep it as a thin wrapper that launches your daemon.

**Acceptance criteria**
- You can update Codex upstream with minimal conflict because your integration is via protocols (MCP/app-server) and separate binaries.

---

# “Copy skills, don’t port”: the compatibility trick
The most leverage comes from this tactic:

### Implement OpenClaw tool names in your MCP server
If your MCP server exposes:
- `cron.add`, `cron.update`, … with the *same JSON schemas*  [oai_citation:59‡OpenClaw](https://docs.openclaw.ai/automation/cron-jobs)  
- `memory_search` / `memory_get`  [oai_citation:60‡OpenClaw](https://docs.openclaw.ai/concepts/memory)  
- your message/channel tool names as used by your skills,

…then **OpenClaw skills can be copied into Codex skill folders nearly unchanged**, because the model sees the same tool affordances.

Codex only injects skill name/description/path until invoked, so keep descriptions crisp to avoid spurious activation.  [oai_citation:61‡OpenAI Developers](https://developers.openai.com/codex/skills/create-skill)

---

## Minimal “first milestone” that unlocks most of the system
If you want the shortest path to something that already feels like a ClawdBot:

1. **Skills sync** (Phase 1)  
2. **MCP compat server with cron + memory** (Phase 2)  
3. **Cron runner with persistence + isolated delivery** (Phase 3)  
4. **Heartbeat** (Phase 4)  
5. **Gateway bridge** (Phase 6, reuse OpenClaw Gateway)

That sequence gets you proactive automation + memory + messaging, which is the core of the “personal assistant” experience.

---
