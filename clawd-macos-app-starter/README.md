# ClawdApp (macOS starter)

This is a starter macOS (SwiftUI) app shell that bundles and runs:
- `codex` (Codex CLI/app-server binary)
- `codex-clawd` (your daemon / MCP compatibility runtime)

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

3) Open `ClawdApp.xcodeproj` in Xcode
4) In `project.yml`:
   - set `DEVELOPMENT_TEAM`
   - set bundle identifiers under `PRODUCT_BUNDLE_IDENTIFIER`
5) Build & Run

## Runtime contract
See `Docs/RUNTIME_PROTOCOL.md`.

## Where tools are embedded
- Build step copies universal2 binaries into `ClawdApp.app/Contents/Resources/bin/`
- On first run, the app copies them into its Application Support container and runs them from there.

## Notes
- This project intentionally leaves the `codex-clawd ui-bridge --stdio` implementation to your Rust side.
- Keep the App Store version "self-contained" and avoid downloading/executing new code at runtime.
