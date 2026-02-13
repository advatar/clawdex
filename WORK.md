You can absolutely get to “Cowork tasks mode” (single‑user) **by wrapping Codex**, not by forking Codex into a new agent OS. The key is: Cowork is *primarily* (1) a **long‑running task runtime** with (2) **file + tool permissions** + (3) **role plugins/skills** + (4) **a rich client UI**. Codex already exposes the right primitives (skills, MCP, approvals, and an app‑server protocol) to let you build that as an **outer product layer**.  [oai_citation:0‡Claude Help Center](https://support.claude.com/en/articles/13345190-getting-started-with-cowork)

Below is a detailed “gap analysis → implementation plan → step‑by‑step checklist” grounded in:
- Cowork’s published feature set and limitations  [oai_citation:1‡Claude Help Center](https://support.claude.com/en/articles/13345190-getting-started-with-cowork)  
- Cowork’s plugin marketplace structure (11 role plugins)  [oai_citation:2‡GitHub](https://github.com/anthropics/knowledge-work-plugins)  
- Codex’s skills + sandbox/approval model + app‑server protocol  [oai_citation:3‡OpenAI Developers](https://developers.openai.com/codex/skills/)  
- The Rust code you attached (src.zip) that already has cron/memory/MCP + a Codex app‑server shim.

---

## 1) What you’re copying from Cowork (single‑user “tasks mode”)

### Core product behavior
Cowork is positioned as: you set a goal, it does multi‑step work with local files, produces real deliverables, and keeps going longer than chat—while you steer and approve risky actions.  [oai_citation:4‡Claude Help Center](https://support.claude.com/en/articles/13345190-getting-started-with-cowork)

### “Skills” users actually feel (high-value output)
Cowork explicitly calls out:
- **Excel spreadsheets with formulas (not CSV)**  
- **Presentations / slide decks**  
- **Reports from messy inputs** (notes, transcripts, voice memos)  
- **Data analysis** (stats, visualization, transforms)  [oai_citation:5‡Claude Help Center](https://support.claude.com/en/articles/13345190-getting-started-with-cowork)

### Permissions & safety model
Cowork:
- Runs in a **VM on the user’s computer**, to isolate work and control file/network access  [oai_citation:6‡Claude Help Center](https://support.claude.com/en/articles/13345190-getting-started-with-cowork)  
- Lets the user control **which MCP servers it can access** and **internet access**, and warns to only trust what you mean to trust  [oai_citation:7‡Claude Help Center](https://support.claude.com/en/articles/13345190-getting-started-with-cowork)  
- Emphasizes “review planned actions before allowing it to proceed”  [oai_citation:8‡Claude Help Center](https://support.claude.com/en/articles/13345190-getting-started-with-cowork)

### Product limitations you should mirror (by default)
Cowork currently states:
- **No projects support**
- **No memory across sessions**
- **No sharing**
- **No GSuite connector**
- **macOS only**
- **Session ends when the desktop app closes**  [oai_citation:9‡Claude Help Center](https://support.claude.com/en/articles/13345190-getting-started-with-cowork)

That’s very aligned with your “single user cowork tasks mode” target: don’t overcomplicate with multi‑user IAM at first. You can add “compliance mode” as an optional capability layer later.

---

## 2) Cowork’s “plugin marketplace” baseline you need to match

Anthropic’s open-source **knowledge-work-plugins** repo shows what “Cowork plugins” contain and how they’re packaged:

- 11 role plugins: **productivity, sales, customer-support, product-management, marketing, legal, finance, data, enterprise-search, bio-research, cowork-plugin-management**  [oai_citation:10‡GitHub](https://github.com/anthropics/knowledge-work-plugins)  
- Each plugin bundles:
  - **skills** (markdown workflows / domain expertise)
  - **connectors** via `.mcp.json` (Slack/Notion/Jira/etc)
  - **slash commands**
  - **sub-agents**  [oai_citation:11‡GitHub](https://github.com/anthropics/knowledge-work-plugins)  
- Structure is file-based and intentionally “no code / no build steps”:  
  `plugin-name/.claude-plugin/plugin.json`, `.mcp.json`, `commands/`, `skills/`  [oai_citation:12‡GitHub](https://github.com/anthropics/knowledge-work-plugins)

This is *very good news* for you: you can implement **plugin compatibility** without pulling in a huge runtime.

---

## 3) What Codex already gives you “for free” (so you don’t rebuild an agent OS)

### Codex app‑server: the UI integration layer you need
Codex app‑server is explicitly meant for “rich clients,” and provides:
- Threads / turns / items
- **Streamed agent events**
- **Approvals**
- Conversation history persistence APIs  [oai_citation:13‡OpenAI Developers](https://developers.openai.com/codex/app-server)

This is basically the “Cowork runtime API surface.”

### Skills: the “role expertise + workflows” layer
Codex supports skills as folders with `SKILL.md` (plus optional scripts/assets), and uses **progressive disclosure** (load metadata first, only load full skill when needed).  [oai_citation:14‡OpenAI Developers](https://developers.openai.com/codex/skills/)

### Sandbox + approvals: your safety rails
Codex supports:
- Sandbox modes (workspace write vs read-only, etc.)
- Approval policies (on-request, untrusted, etc.)  [oai_citation:15‡OpenAI Developers](https://developers.openai.com/codex/security/)

That maps cleanly to Cowork’s “controlled file & network access + permission prompts.”

### MCP support
Codex supports MCP servers in CLI/IDE, so your “tools/connectors” story can remain MCP-first.  [oai_citation:16‡OpenAI Developers](https://developers.openai.com/codex/mcp/)

---

## 4) Your current Rust baseline (from src.zip): what’s already there

You already have the beginnings of a Cowork-like runtime wrapper:

### Already present
- **Daemon-ish core** (`main.rs`) with:
  - MCP server exposing `cron.*`, `memory.*`, `heartbeat.*`
  - “skills sync” bridge from an OpenClaw manifest to Codex skill manifest
  - A UI bridge “reload_needed” file signal
- **Codex app-server integration**
  - Spawns `codex app-server`
  - Has an `ApprovalServer` implementation
  - Has a `CodexClient` that can send a prompt and collect assistant output

### The critical missing pieces (for Cowork mode)
- A real **Task model** (task list, task runs, steps, outputs)
- **Event streaming** into UI (not just final message)
- **Interactive approvals** (plan + file changes + command exec + “ask user input”)
- Workspace/file permission UI (Cowork’s “choose folder” / “allowed paths” feel)
- “Professional output” skills (xlsx/pptx/docx/pdf pipelines)
- Plugin manager (install/enable/disable/update; plugin → skills + commands + .mcp.json)
- A persistent store (SQLite) for tasks, approvals, events, artifacts, plugin state
- Actual cron runner (your cron module is mostly persistence stubs right now)

---

## 5) Gap analysis: Cowork vs “current Clawdex code” vs “what to build”

| Capability | Cowork expectation | Codex primitive | Your current status | What to implement |
|---|---|---|---|---|
| Task runs that stream progress | Long-running, visible progress | app-server streamed items/events | Not wired to UI | Task engine + event stream |
| Plan review + step approvals | “Review planned actions” | app-server approvals + sandbox | Auto approve/deny env flag | Approval broker + UI |
| File access boundaries | Allowed files only | sandbox + cwd + approval prompts | Not user-driven | Workspace selector + policy |
| “Ask user input” during tool use | Common in task mode | app-server elicitation/user-input | Not supported (returns empty) | Implement ToolRequestUserInput UX |
| Real outputs (xlsx/pptx/docs) | Explicitly marketed | skills + tools | Missing | Artifact toolchain tools/skills |
| Plugins & marketplace | 11 role plugins, file-based | skills + MCP + slash commands | Not present | Plugin manager + compatibility layer |
| “VM-like isolation” | Runs in VM | sandbox modes + OS sandbox | Not present | Multi-backend executor (sandbox/container/VM) |
| No memory across sessions (default) | Stated limitation | you can choose | You have memory stub | Ship “off by default”, explicit opt-in |
| Scheduler | Optional but valuable | your cron/MCP | Storage is there; runner stub | Real scheduler + UI integration |

---

# 6) Very detailed implementation plan (single-user Cowork mode)

I’ll lay this out as **phases** with **step-by-step checklists** and *exact* components to add/modify in your current Rust codebase.

---

## Phase 0 — Product boundary + repo structure (minimize upstream Codex interference)

**Goal:** Keep Codex upstream nearly untouched; treat it as an engine you orchestrate via app-server + skills + MCP config.

### Checklist
Unchecked items in Phases 0-8 below represent true remaining gaps after parity/port work.
- [x] Define three processes/binaries:
  - [x] `clawdexd` (daemon/runtime)
  - [x] `clawdex` (CLI client)
  - [x] `Clawdex.app` (macOS client)
- [x] Make `clawdexd` the only component that directly:
  - starts/stops Codex app-server processes  [oai_citation:17‡OpenAI Developers](https://developers.openai.com/codex/app-server)  
  - manages skills installation / plugin sync  [oai_citation:18‡OpenAI Developers](https://developers.openai.com/codex/skills/)  
  - manages MCP configs  [oai_citation:19‡OpenAI Developers](https://developers.openai.com/codex/mcp/)  
- [x] Create a stable local API between clients and daemon:
  - [x] **Unix domain socket** (best for CLI/macOS app)
  - [x] Optional localhost HTTP (for debugging / future UI)
- [x] Choose persistence:
  - [x] SQLite (single user, durable, queryable)
  - [x] Add a JSONL append-only event log for audit export (optional; see Phase 6)

**Why this matches Cowork:** Cowork is a “rich client + local VM runtime.” Your analog is “rich client + daemon runtime + Codex app-server engine.”  [oai_citation:20‡Claude Help Center](https://support.claude.com/en/articles/13345190-getting-started-with-cowork)

---

## Phase 1 — Task engine + streamed events (turn Codex into a task runner)

**Goal:** Model “tasks” the way a user experiences them: a task has a goal, progress, approvals, outputs, and can be resumed.

### 1.1 Define the canonical data model (SQLite)
Tables (minimum):
- `tasks(id, title, created_at, last_run_at, pinned, tags)`
- `task_runs(id, task_id, status, started_at, ended_at, codex_thread_id, sandbox_mode, approval_policy)`
- `events(id, task_run_id, ts, kind, payload_json)`
- `approvals(id, task_run_id, ts, kind, request_json, decision, decided_at)`
- `artifacts(id, task_run_id, path, mime, sha256, created_at)`
- `plugins(id, name, version, source, enabled, installed_at)`
- `connectors(id, name, type, config_json, enabled)`

### 1.2 Refactor `CodexClient` to stream instead of “one final string”
Codex app-server streams `item/started`, `item/completed`, `item/agentMessage/delta`, etc.  [oai_citation:21‡OpenAI Developers](https://developers.openai.com/codex/app-server)

#### Checklist
- [x] Replace single-response prompt flow with streamed turn execution/events.
- [x] Persist every event to `events` table in real time
- [x] Emit a separate “UI event” stream that clients can subscribe to:
  - CLI shows progress; mac app renders timeline

### 1.3 Implement cancellation + resume
Codex supports thread resume/fork and turn interrupts.  [oai_citation:22‡OpenAI Developers](https://developers.openai.com/codex/app-server)

#### Checklist
- [x] Implement `task_run.cancel()`: send `turn/interrupt`
- [x] Implement `task_run.resume()`:
  - [x] call `thread/resume`
  - [x] start a new turn within same thread
- [x] Support “branching” runs:
  - [x] `task_run.fork()` → `thread/fork` for “try a different approach”

---

## Phase 2 — Human-in-the-loop approvals (Cowork’s “review planned actions”)

Right now your `ApprovalServer` uses env flags to auto-approve/deny. For Cowork mode, approvals are the product.

Codex also explicitly positions approvals as part of the experience (“watch plan; approve/reject steps”).  [oai_citation:23‡OpenAI Developers](https://developers.openai.com/codex/cli/features/)

### 2.1 Implement an “Approval Broker” in `clawdexd`
**Pattern:** app-server request → store in DB → notify UI → await decision → respond.

#### Checklist (code changes)
- [x] Replace `CLAWD_AUTO_APPROVE` logic with a pluggable broker:
  - [x] Broker-backed approval handlers + daemon approval resolution API
- [x] On every approval request:
  - [x] store `request_json` in `approvals`
  - [x] expose pending approvals to clients (daemon API)
  - [x] block waiting for a response (with timeout + cancel path)
- [x] Support at least these approval types:
  - [x] Plan approval (covered via existing command/file approval checkpoints; Codex app-server v2 does not currently expose a dedicated plan-approval request type)
  - [x] File change approval (diff preview)
  - [x] Command execution approval
  - [x] Network / internet access approval (enforced via sandbox/network policy + command approvals; no separate request type surfaced today)

### 2.2 Add Cowork-style “Deletion protection”
Cowork emphasizes controlled access and reviewing actions. It runs in a VM and urges review, especially for sensitive files.  [oai_citation:24‡Claude Help Center](https://support.claude.com/en/articles/13345190-getting-started-with-cowork)

You can implement a stronger guarantee:
- **Never approve deletions automatically**
- Require explicit “type to confirm” or “hold to confirm” in UI

#### Checklist
- [x] Parse file-change approval payloads
- [x] Detect deletions/renames as “high risk”
- [x] Force **explicit** confirmation step (UI affordance)
- [x] Record “why approved” evidence in approvals table (for audit)

### 2.3 Implement “ToolRequestUserInput” (you currently stub it)
In your `ApprovalServer`, you currently log “not supported” and return empty answers. That breaks tasks-mode UX.

#### Checklist
- [x] When Codex requests user input:
  - [x] create a UI prompt (“Codex asks: …”)
  - [x] support multi-field forms if the request schema supports it
  - [x] support “cancel / skip” with a clear result
- [x] Persist the user response as an event + approval record

---

## Phase 3 — Workspace + permissions (single-user, Cowork feel)

Cowork uses a VM and “controlled file and network access,” and it explicitly says the agent can access local files you grant.  [oai_citation:25‡Claude Help Center](https://support.claude.com/en/articles/13345190-getting-started-with-cowork)

Codex already supports sandboxing + approvals (e.g., asks for approval to edit outside workspace or access network).  [oai_citation:26‡OpenAI Developers](https://developers.openai.com/codex/security/)

### 3.1 Workspace selection model
- CLI: workspace = current directory (plus optional additional mounts)
- macOS app: user chooses folder(s); store bookmarks

#### Checklist
- [x] Implement `Workspace` type:
  - allowed roots
  - read/write flags
  - deny patterns (e.g., `**/.git/**`, `**/.env`)
- [x] On each task run:
  - [x] set Codex `cwd` to workspace root
  - [x] set `sandbox_mode` appropriately
  - [x] set `approval_policy` appropriately

### 3.2 Internet access and MCP permissions UI
Cowork calls out controlling:
1) which MCPs are connected, and  
2) internet access.  [oai_citation:27‡Claude Help Center](https://support.claude.com/en/articles/13345190-getting-started-with-cowork)

#### Checklist
- [x] Add “Permissions” panel (macOS) and `/permissions` in CLI:
  - [x] internet: off / allowlist / on
  - [x] MCP servers: enabled/disabled + per-server “ask every time / allow once / allow always”
- [x] Persist these policies per task or globally

---

## Phase 4 — Plugin system (compatibility-first with Cowork plugins)

Your fastest path to feature parity is:
- Support Cowork’s **plugin layout**
- Convert plugin assets into:
  - Codex skills
  - Clawdex commands
  - Codex/Clawdex MCP connector config

Anthropic’s plugin repo spells out the structure + the role plugins and the connectors they expect (Slack, Notion, Jira, etc.).  [oai_citation:28‡GitHub](https://github.com/anthropics/knowledge-work-plugins)

### 4.1 Implement plugin loader + registry
#### Checklist
- [x] Plugin install sources:
  - [x] local folder
  - [x] zip import
  - [x] git URL (CLI only; App Store version may restrict)
- [x] Validate plugin structure:
  - `.claude-plugin/plugin.json`
  - optional `.mcp.json`
  - `skills/`
  - `commands/`  [oai_citation:29‡GitHub](https://github.com/anthropics/knowledge-work-plugins)
- [x] Store plugin metadata in SQLite
- [x] Provide enable/disable per plugin

### 4.2 Map plugins into Codex (skills + MCP)
Codex skills are folder-based (`SKILL.md`, scripts, resources) with progressive disclosure.  [oai_citation:30‡OpenAI Developers](https://developers.openai.com/codex/skills/)

#### Checklist
- [x] Convert plugin `skills/*.md` into Codex skill folders:
  - [x] create a folder per skill
  - [x] generate `SKILL.md` with metadata frontmatter (name/description)
  - [x] store the original markdown as the instructions body
- [x] Convert `.mcp.json` into Codex MCP config entries
  - Codex supports MCP servers; you can generate `mcp.json` or edit config accordingly  [oai_citation:31‡OpenAI Developers](https://developers.openai.com/codex/mcp/)
- [x] Add “skill provenance” to help the UI show “this came from plugin X”

### 4.3 Implement “slash commands” for your clients
Even if Codex doesn’t allow arbitrary user-defined slash commands, *you can* implement them in your wrapper UI:

#### Checklist
- [x] Parse `commands/` definitions
- [x] Expose in:
  - CLI: `/sales:call-prep`, `/data:write-query` style (matching the plugin repo examples)  [oai_citation:32‡GitHub](https://github.com/anthropics/knowledge-work-plugins)
  - mac app: command palette
- [x] Command execution = templated prompt + skill activation + optional connector gating

### 4.4 Ship a “Marketplace baseline”
Cowork’s repo has 11 role plugins with explicit connector expectations.  [oai_citation:33‡GitHub](https://github.com/anthropics/knowledge-work-plugins)

#### Checklist
- [x] Add `clawdex plugins add anthropics/knowledge-work-plugins/<role>`
- [x] Provide a “starter set” for single-user:
  - productivity
  - data
  - product-management
  - cowork-plugin-management

---

## Phase 5 — “Professional outputs” (Excel, slides, polished docs)

Cowork explicitly markets:
- Spreadsheets with formulas
- Presentations
- Reports from messy inputs  [oai_citation:34‡Claude Help Center](https://support.claude.com/en/articles/13345190-getting-started-with-cowork)

### Strategy: don’t rely on the model to “write a perfect binary”
Instead:
- Provide deterministic “artifact builder” tools (xlsx/pptx/docx/pdf)
- Teach the model to call them via skills

#### Checklist (core)
- [x] Add an “Artifact Service” inside `clawdexd` with tools like:
  - `artifact.create_xlsx(spec_json, output_path)`
  - `artifact.create_pptx(deck_spec, output_path)`
  - `artifact.create_docx(doc_spec, output_path)`
  - `artifact.create_pdf(report_spec, output_path)`
- [x] Provide “spec schemas” + validation
- [x] On success:
  - hash the output
  - store as `artifacts` record
  - emit `ArtifactCreated` event

#### Checklist (skills that drive it)
- [x] `Spreadsheet Builder` skill:
  - asks clarifying Qs
  - produces a validated spreadsheet spec
  - calls artifact tool
  - verifies formulas/tabs exist
- [x] `Slide Deck Builder` skill:
  - converts notes/transcript into outline → slides
  - calls artifact tool
- [x] `Report Writer` skill:
  - structured sections, citations, executive summary

---

## Phase 6 — Long-running tasks + internal scheduler (your cron module becomes real)

Cowork requires the app to stay open to keep the session running.  [oai_citation:35‡Claude Help Center](https://support.claude.com/en/articles/13345190-getting-started-with-cowork)  
You can match that *and* provide a “daemon continues running” option for the CLI / non–App Store build.

### 6.1 Implement cron runner
Your `cron.rs` already has job persistence and CRUD; it just doesn’t execute.

#### Checklist
- [x] Implement `cron_run_loop()`:
  - parse cron expressions
  - compute next_run
  - when due: run a Task Run with a stored prompt/command
- [x] Add job locking (avoid double-run)
- [x] Add per-job policy:
  - sandbox mode
  - approval policy
  - internet allowed or not
- [x] Store run events under the same `task_runs/events` pipeline

### 6.2 macOS background constraints
If you truly want **App Store** distribution, background daemons and spawning arbitrary binaries get tricky.

#### Practical plan
- App Store build:
  - scheduled tasks only run while app is open (Cowork parity)
- Non-App Store build (Homebrew/DMG):
  - optional launchd login item to keep `clawdexd` alive

---

## Phase 7 — Memory (match Cowork by default, exceed it optionally)

Cowork explicitly says “no memory across sessions.”  [oai_citation:36‡Claude Help Center](https://support.claude.com/en/articles/13345190-getting-started-with-cowork)  
You can:
- Default to Cowork behavior (memory OFF)
- Offer “opt‑in memory vault” as a differentiator (with explicit user approval)

#### Checklist
- [x] Implement real `memory_search` (your current search is a stub):
  - embeddings-based vector search (store vectors)
  - or SQLite FTS + heuristics as v1
- [x] Add “write-to-memory” as a gated action:
  - always requires user approval
  - stores provenance (which task wrote it, why)
- [x] Add “memory scope”:
  - global
  - per workspace
  - per plugin/role

---

## Phase 8 — Compliance, auditability, and “trust checkpoints” (your differentiator)

You described “Agent Passport + Dynamic Trust Checkpoints.” Even for single-user, this is valuable: it formalizes “what did the agent intend to do” + “what approvals happened” + “can we prove it later”.

Codex app-server already gives you the raw event stream; you turn it into **audit artifacts**.  [oai_citation:37‡OpenAI Developers](https://developers.openai.com/codex/app-server)

### Checklist
- [x] Define `ActionIntent` schema generated from:
  - planned steps
  - tool calls
  - file diffs
  - connector targets (domain/app)
- [x] Implement “risk scoring” rules:
  - deletion
  - credential use
  - external posting
  - money / procurement
  - exporting files outside workspace
- [x] Implement checkpoints:
  - “needs explicit approval”
  - “needs re-auth”
  - “needs dual control” (future multi-user)
- [x] Implement tamper-evident audit log:
  - append-only JSONL
  - hash chain (each record includes prev_hash)
- [x] Add “Export audit packet” UI:
  - events
  - approvals
  - artifact hashes
  - plugin versions
  - connector config (redacted)

---

## Phase 9 — macOS App Store distribution reality check (and how to still ship it)

You *can* ship a macOS app, but you must design around sandbox constraints:

### Key friction: “running arbitrary local MCP servers”
There’s an open request specifically calling out that sandboxed macOS apps can’t invoke user-installed binaries outside the sandbox, motivating an HTTP/localhost approach for MCP.  [oai_citation:38‡GitHub](https://github.com/openai/codex/issues/6634)

### Practical packaging approach
**Two tiers (recommended):**
1) **App Store “Clawdex Work”**
   - bundle `codex` + `clawdexd` inside the app
   - allow only:
     - built-in connectors you ship
     - remote MCP servers over HTTPS
     - plugins that are “markdown + json” (no executable code)
   - scheduled tasks run only while app open (Cowork parity)  [oai_citation:39‡Claude Help Center](https://support.claude.com/en/articles/13345190-getting-started-with-cowork)  

2) **Developer distribution (Homebrew/DMG)**
   - full local MCP support (spawn processes)
   - background daemon via launchd
   - plugin installs from git

This gives you the App Store presence without sacrificing power users.

---

# 7) Step-by-step checklist (the “do this next” list)

If you want this as a literal implementation sequence for the next commits, here it is:

## A. Foundation (daemon + DB + API)
- [x] Create `clawdexd` crate (or rename current binary) and add SQLite store
- [x] Add local IPC (UDS gRPC/JSON-RPC) for CLI/app
- [x] Convert `ui_bridge.rs` from “write a file” → “publish events to subscribers”

## B. Task engine
- [x] Implement `TaskEngine::start_run(prompt, workspace, policies)`
- [x] Store `task` + `task_run` rows
- [x] Wire Codex app-server thread/turn lifecycle to task runs  [oai_citation:40‡OpenAI Developers](https://developers.openai.com/codex/app-server)

## C. Streaming
- [x] Rewrite `CodexClient` to stream events and persist them
- [x] CLI “live view” that follows a run

## D. Approvals UX
- [x] Replace env-based approvals with Approval Broker
- [x] Add interactive CLI approvals (plan/file/command)
- [x] Implement ToolRequestUserInput prompting

## E. Workspace & permissions
- [x] Implement workspace allowlist + deny patterns
- [x] Default sandbox = workspace-write, approval = on-request/untrusted (your choice)  [oai_citation:41‡OpenAI Developers](https://developers.openai.com/codex/config-advanced/)
- [x] Add “permissions view” to CLI and app

## F. Plugin compatibility (Cowork plugins)
- [x] Implement plugin install/enable/disable
- [x] Parse `.claude-plugin/plugin.json` + `.mcp.json` + skills + commands  [oai_citation:42‡GitHub](https://github.com/anthropics/knowledge-work-plugins)
- [x] Convert plugin skills into Codex skills folders (`SKILL.md`)  [oai_citation:43‡OpenAI Developers](https://developers.openai.com/codex/skills/)
- [x] Implement wrapper slash commands in your UI (not inside Codex)

## G. Output generation (Excel/slides/docs)
- [x] Add artifact service tools
- [x] Add “Spreadsheet Builder” / “Deck Builder” / “Report Writer” skills
- [x] Save artifact hashes + list outputs at end of run

## H. Scheduler
- [x] Implement cron runner loop
- [x] Add “schedule this task” UI and CLI commands

## I. macOS app
- [x] Build a minimal shell UI:
  - task list
  - new task
  - run detail (stream)
  - approvals sheet
  - outputs list
  - plugins + permissions settings
- [x] Bundle `codex` + `clawdexd` in app
- [x] Implement folder picker + bookmarks
- [x] For App Store: restrict connectors to built-in/remote; document constraints  [oai_citation:44‡GitHub](https://github.com/openai/codex/issues/6634)

## J. Audit + compliance
- [x] Implement event/approval export
- [x] Add tamper-evident log chaining
- [x] Add “Action Intent + checkpoint” evidence records

---

## 8) Tiny set of questions (only to avoid building the wrong thing)
I can proceed without these, but your answers would let you lock scope faster:

1) For “Cowork parity”, do you want **internet access inside tasks** by default (like a browser/research mode), or opt-in per task?  [oai_citation:45‡Claude Help Center](https://support.claude.com/en/articles/13345190-getting-started-with-cowork)  
2) For App Store: are you okay with “remote MCP only” (plus built-ins), and keeping “local MCP processes” for the Homebrew/DMG build?  [oai_citation:46‡GitHub](https://github.com/openai/codex/issues/6634)  
3) Which “professional outputs” are must-have v1: **xlsx + pptx**, or also **docx/pdf**?  [oai_citation:47‡Claude Help Center](https://support.claude.com/en/articles/13345190-getting-started-with-cowork)
