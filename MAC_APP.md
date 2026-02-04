This starter is a **menu‑bar macOS app** that:
- **Builds universal2** Rust binaries (`codex`, `clawdex`, `clawdexd`) in an Xcode build phase
- **Embeds** them into the app bundle, **codesigns** them, and copies them into the app’s **Application Support** container
- Runs `clawdex` (UI bridge) and `clawdexd` (daemon IPC) as **child processes**
- Provides UI for:
  - **Launch at login** (user‑controlled via `SMAppService`)
  - **OpenAI API key** stored in **Keychain**
  - **Workspace folder picker** using **security‑scoped bookmarks**
  - **Tasks + approvals** views (daemon IPC)
  - **Schedule** view for cron jobs

---

## 1) Rust UI Bridge Contract (clawdex)

The app expects `clawdex` to support a UI bridge mode:

- Command:
  - `clawdex ui-bridge --stdio --codex-path <path> --state-dir <path> [--workspace <path>]`
- Transport:
  - stdin: newline‑delimited JSON (JSONL)
  - stdout: JSONL events (assistant messages + errors)
  - stderr: debug logs (optional)

Minimal required stdout events:
- `{"type":"assistant_message","text":"..."}`
- `{"type":"error","message":"..."}`

---

## 2) Build + Embed Flow

### A) Build‑time: universal2 binaries + embed + sign
The Xcode build script does the following:

1. Builds Codex as a universal2 Rust binary (arm64 + x86_64).
2. Builds Clawdex as a universal2 Rust binary (arm64 + x86_64).
3. Builds Clawdexd as a universal2 Rust binary (arm64 + x86_64).
3. Copies tools into:
   - `Clawdex.app/Contents/Resources/bin/`
4. Codesigns embedded executables using helper entitlements:
   - `Resources/HelperTool.entitlements` includes `com.apple.security.inherit`

The build script is here:
- `Scripts/build_and_embed_rust.sh`

Key inputs (override via environment variables):
1. Codex (Rust):
   - `CODEX_CARGO_ROOT` (default `../codex/codex-rs`)
   - `CODEX_PACKAGE` (default `codex-cli`)
   - `CODEX_BINARY` (default `codex`)
   - `CODEX_BIN` (use a prebuilt Mach‑O binary instead of Cargo)
2. Clawdex (Rust):
   - `CLAWDEX_CARGO_ROOT` (default `../clawdex`)
   - `CLAWDEX_PACKAGE` (default `clawdex`)
   - `CLAWDEX_BINARY` (default `clawdex`)
   - `CLAWDEX_BIN` (use a prebuilt Mach‑O binary instead of Cargo)
   - `CLAWDEXD_BINARY` (default `clawdexd`)
   - `CLAWDEXD_BIN` (use a prebuilt Mach‑O binary instead of Cargo)
3. Prebuilt fallback (when Rust is not installed on the build machine):
   - `PREBUILT_DIR` (default `macClawdex/Resources/prebuilt`)
   - Place `codex`, `clawdex`, and/or `clawdexd` binaries at that path to skip Cargo.
   - Helper: `Scripts/stage_prebuilt.sh` copies built binaries into the prebuilt folder.
   - The `prebuilt/` folder is tracked with Git LFS; ensure `git lfs install` is run before committing binaries.
3. Skips:
   - `SKIP_CODEX_BUILD=1` (requires `CODEX_BIN`)
   - `SKIP_CLAWDEX_BUILD=1` (requires `CLAWDEX_BIN`)
   - `SKIP_TOOLS_EMBED=1` (skips the entire step)

### B) Run‑time: install tools into Application Support and run from there
On app start, `RuntimeManager` copies embedded tools into the app’s container:
- `~/Library/Application Support/<bundle-id>/tools/`

Then it runs `clawdex` and `clawdexd` from there.

This avoids mutating the app bundle and keeps all state inside the sandbox container.

---

## 3) Step‑by‑Step Checklist

### Step 0 — Place the project
- The folder lives at: `macClawdex/`

### Step 1 — Generate/open the Xcode project
This starter uses XcodeGen for reproducibility:
- Run `xcodegen generate`
- Open `Clawdex.xcodeproj`

### Step 2 — Set signing + bundle IDs
Edit `project.yml`:
- `DEVELOPMENT_TEAM: JS498PMF4Z` (replace if needed)
- `PRODUCT_BUNDLE_IDENTIFIER: com.yourcompany.Clawdex`

### Step 3 — Make sure Rust can build universal2
Install targets:
- `rustup target add aarch64-apple-darwin x86_64-apple-darwin`

### Step 4 — Hook in Codex + Clawdex paths
In `Scripts/build_and_embed_rust.sh`, update or override:
1. `CODEX_CARGO_ROOT` (where Codex `Cargo.toml` lives).
2. `CLAWDEX_CARGO_ROOT` (where Clawdex `Cargo.toml` lives).

### Step 5 — Implement `clawdex ui-bridge --stdio`
- Parse stdin JSONL `user_message`
- Spawn `codex app-server`
- Stream assistant output to stdout JSONL

---

## 4) App Store Constraints (Design Notes)

If your goal is “App Store + full agent,” align with Apple’s sandbox rules:
- Use a user‑selected workspace and security‑scoped bookmarks.
- Avoid downloading or executing new code at runtime.
- Stop background processes when the user quits.

The starter app already includes:
- `com.apple.security.files.user-selected.read-write` and bookmark entitlements
- Workspace picker + bookmark persistence
- Launch‑at‑login toggle via `SMAppService`

---

## 5) Embedding Additional Helpers

If you add more binaries (gateway, extra MCP servers, etc.):

1. Build them (Cargo/Go/etc.)
2. Create universal2 if needed
3. Copy into `Resources/bin/`
4. Codesign them with the same entitlements

Then update `clawdex` to discover and run them as needed.
