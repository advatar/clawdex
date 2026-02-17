# Clawdex 

This is a starter macOS (SwiftUI) app shell that bundles and runs:
- `codex` (Codex CLI/app-server binary)
- `clawdex` (your daemon / MCP compatibility runtime)

## Prereqs
- Xcode 15+
- Rust toolchain (stable) with targets:
  - aarch64-apple-darwin
  - x86_64-apple-darwin
- XcodeGen (optional, recommended)

## Quick start (XcodeGen)
1) Install XcodeGen:
   - `brew install xcodegen`

2) From this folder:
   - `xcodegen generate`

3) Open `Clawdex.xcodeproj` in Xcode
4) Signing + bundle IDs:
   - create `Configs/Signing.local.xcconfig` and set `DEVELOPMENT_TEAM = YOURTEAMID`
   - set bundle identifiers under `PRODUCT_BUNDLE_IDENTIFIER` in `project.yml`
5) Build & Run

## Runtime contract
See `Docs/RUNTIME_PROTOCOL.md`.

## Where tools are embedded
- Build step copies universal2 binaries into `Clawdex.app/Contents/Resources/bin/`
- On first run, the app copies them into its Application Support container and runs them from there.
- The build step also embeds bundled plugin content in app resources:
  - `Contents/Resources/openclaw-extensions/`
  - `Contents/Resources/claude-plugins/`
- Runtime exports `CLAWDEX_BUNDLED_CLAUDE_PLUGINS_DIR` for all spawned `clawdex` commands/processes so bundled Claude plugins are auto-installed on first run.

## Plugin Manager
- Search installed plugins and local skills/commands from the sidebar.
- Discover community skills/plugins from OpenClawHub directly in the Plugin Manager.
- Copy OpenClawHub install commands (`npx clawhub@latest install <slug>`) or open result pages in the browser.

## Notes
- This project intentionally leaves the `codex-cl ui-bridge --stdio` implementation to your Rust side.
- Keep the App Store version "self-contained" and avoid downloading/executing new code at runtime.
