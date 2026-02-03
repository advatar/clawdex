
This starter is a **menu-bar macOS app** that:
- **Builds universal2** Rust binaries (`codex`, `clawdex`) in an Xcode build phase
- **Embeds** them into the app bundle, **codesigns** them, and copies them into the app’s **Application Support** container
- Runs `clawdex` as a **child process** and talks to it over **JSONL on stdin/stdout** (a small, reliable bridge)
- Provides UI for:
  - **Launch at login** (user-controlled via `SMAppService`)
  - **OpenAI key** stored in **Keychain**
  - **Workspace folder picker** using **security‑scoped bookmarks** for sandboxed file access

---

## 1) What you need to implement on the Rust side (clawdex)

The starter app expects `clawdex` to support a UI bridge mode:

- Command:
  - `clawdex ui-bridge --stdio --codex-path <path> --state-dir <path> [--workspace <path>]`
- Transport:
  - stdin: newline-delimited JSON (JSONL)
  - stdout: JSONL events (assistant messages + errors)
  - stderr: debug logs (optional)

The app’s suggested protocol is documented in the zip at:
- `Docs/RUNTIME_PROTOCOL.md`

Minimal required stdout events:
- `{"type":"assistant_message","text":"..."}`
- `{"type":"error","message":"..."}`

---

## 2) How bundling works in the starter

### A) Build-time: universal2 binaries + embed + sign
The Xcode build script does the following:

1. Builds both architectures:
   - `aarch64-apple-darwin`
   - `x86_64-apple-darwin`

2. Uses `lipo` to create universal2 binaries

3. Copies binaries into:
- `Clawdex.app/Contents/Resources/bin/`

4. Codesigns those embedded executables using an “inherit” entitlement:
- `Resources/HelperTool.entitlements` includes `com.apple.security.inherit` (common pattern for embedded helpers)

The build script is here:
- `Scripts/build_and_embed_rust.sh`

If your Cargo package or binary names differ, adjust:
- `RUST_PACKAGES` (Cargo package names)
- `RUST_BINARIES` (actual output executable names)

### B) Run-time: install tools into Application Support and run from there
On app start, `RuntimeManager` copies embedded tools into the app’s container:
- `~/Library/Application Support/<bundle-id>/tools/`

Then it runs `clawdex` from there.

This avoids trying to mutate the app bundle and keeps all state within the sandbox container.

---

## 3) App Store constraints you must design around

If your goal is “App Store + full agent,” you’ll need to align with Apple’s Mac App Store rules:

### A) Sandboxing + file access
Mac App Store apps must be sandboxed and should use the file APIs correctly.  [oai_citation:0‡Apple Developer](https://developer.apple.com/app-store/review/guidelines/)  
For workspace access outside the container, you typically need:
- user selection via Open/Folder dialog
- security-scoped bookmarks for persistence

A practical explanation of why *passing arbitrary paths is not enough* and how bookmarks help is here.  [oai_citation:1‡Timac](https://blog.timac.org/2021/0516-mac-app-store-embedding-a-command-line-tool-using-paths-as-arguments/)

The starter app already includes:
- `com.apple.security.files.user-selected.read-write` and bookmark entitlement in `Resources/Clawdex.entitlements`
- Folder picker + bookmark persistence (`WorkspaceAccess.swift`)

### B) Login/background behavior must be user-consented
Apple’s Mac App Store requirements explicitly call out:
- no auto-launch at login without consent
- no lingering processes after the user quits without consent  [oai_citation:2‡Apple Developer](https://developer.apple.com/app-store/review/guidelines/)

The starter app uses a Settings toggle based on `SMAppService.mainApp.register()` / `unregister()` as shown here.  [oai_citation:3‡Nil Coalescing](https://nilcoalescing.com/blog/LaunchAtLoginSetting)  
Apple also documents that `SMAppService` is the modern mechanism to manage login/background items.  [oai_citation:4‡Apple Support](https://support.apple.com/en-gu/guide/deployment/depdca572563/web)

### C) Avoid “downloading/executing new code to change functionality”
Guideline 2.5.2 is the one you must respect if your “skills” or gateway can download plugins/scripts/browsers at runtime.  [oai_citation:5‡Apple Developer](https://developer.apple.com/app-store/review/guidelines/)

**Implication for your agent stack:**  
For the App Store build, you should strongly consider:
- **No auto-downloading** of MCP servers, browsers, interpreters, “skill packs,” etc.
- If you support skills, make them **data-driven** (configs/prompts) or require the user to import local content explicitly and keep it transparent (developer-tool positioning).

---

## 4) Recommended product split (keeps App Store viable)

You said you want both:
- CLI distribution
- Mac App Store app

A workable split is:

### 1) CLI (“Full power”)
- Distributed via Homebrew / GitHub releases / notarized pkg
- Can enable all OpenClaw-like capabilities (unrestricted shell, broader filesystem access, optional tool downloads, etc.)

### 2) Mac App Store app (“Sandboxed companion”)
- Same core engine, but:
  - File access limited to **user-selected workspace(s)**
  - “Dangerous” tools gated behind explicit UI approvals
  - No runtime code downloads
  - Scheduler runs while the app is running (menu bar), with user-controlled launch-at-login

This doesn’t prevent you from reaching OpenClaw parity; it just makes parity **policy-aware**.

---

## 5) Step-by-step checklist to adopt this starter in your repo

### Step 0 — Place the project
- Put the folder somewhere like:
  - `apps/macos/Clawdex`

### Step 1 — Generate/open the Xcode project
This starter uses XcodeGen for reproducibility:
- Run `xcodegen generate`
- Open `Clawdex.xcodeproj`

(If you don’t want XcodeGen, create a new SwiftUI macOS app in Xcode and copy `Sources/`, `Resources/`, and the build script.)

### Step 2 — Set signing + bundle IDs
Edit `project.yml`:
- `DEVELOPMENT_TEAM: YOURTEAMID`
- `PRODUCT_BUNDLE_IDENTIFIER: com.yourcompany.Clawdex`

### Step 3 — Make sure Rust can build universal2
Install targets:
- `rustup target add aarch64-apple-darwin x86_64-apple-darwin`

### Step 4 — Hook in your actual Cargo workspace layout
In `Scripts/build_and_embed_rust.sh`, update:
- `REPO_ROOT` (where `Cargo.toml` lives)
- `RUST_PACKAGES` (Cargo package names)
- `RUST_BINARIES` (produced binary names)

### Step 5 — Implement `clawdex ui-bridge --stdio`
- Parse stdin JSONL `user_message`
- Run a Codex “turn” (however you’re already doing it)
- Emit stdout JSONL events:
  - assistant message
  - error
- Keep non-JSON logs on stderr (or remove them)

### Step 6 — Ensure scheduler lifecycle is App Store-safe
- If user quits the app: stop the scheduler + child processes  
- If user closes windows: keep app alive as menu bar app (so cron/heartbeat can keep running)

This maps to Apple’s “no background processes after quit without consent” expectation.  [oai_citation:6‡Apple Developer](https://developer.apple.com/app-store/review/guidelines/)

### Step 7 — Workspace access
- Use the built-in folder picker (already in Settings)
- Store bookmark
- On startup, resolve bookmark and start accessing the security-scoped resource before spawning your toolchain

(That pattern is the standard way to keep sandbox access across launches.  [oai_citation:7‡Timac](https://blog.timac.org/2021/0516-mac-app-store-embedding-a-command-line-tool-using-paths-as-arguments/))

### Step 8 — Privacy manifest + App Store submission hygiene
Even for macOS App Store apps, you should include `PrivacyInfo.xcprivacy`. Apple’s policy is enforced for App Store submissions; macOS has a slightly different bar for “required reason APIs,” but the manifest is still relevant for App Store distribution.  [oai_citation:8‡Unity Documentation](https://docs.unity3d.com/6000.3/Documentation/Manual/apple-privacy-manifest-policy.html)  
Starter includes a minimal manifest at `Resources/PrivacyInfo.xcprivacy` (you must fill it out based on what you use).

---

## 6) How to bundle “all binaries” (beyond codex + clawdex)

If you have additional helpers (gateway, extra MCP servers, etc.):

1. Add them to the build script:
   - build them (Cargo / Go / etc.)
   - create universal2 if needed
   - copy into `Resources/bin/`
   - codesign them (same identity + inherit entitlements)

2. Add a small “tool registry” in `clawdex`:
   - discover tool paths under `Application Support/<bid>/tools/`
   - spawn them on-demand

3. For App Store build: avoid helpers that **download executable content** or behave like “installers,” which risks 2.5.2.  [oai_citation:9‡Apple Developer](https://developer.apple.com/app-store/review/guidelines/)

--