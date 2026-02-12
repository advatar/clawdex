 Right now the mac app is a thin shell around clawdex ui-bridge (macClawdex/Sources/Clawdex/RuntimeManager.swift), so
  parallel-agent orchestration is not in the protocol yet. But you can integrate ParallellVibe in the app and run N
  candidate agents per prompt.

  Best practical pattern:

  1. Use ParallellVibe in-app for parallel draft/verification.
  2. Send only the selected winner to clawdex for tool-using execution and workspace edits.
  3. Keep app UI unchanged except a toggle like “Parallel mode”.

  Important caveats:

  - ParallellVibe is currently tied to macOS/iOS 26 and an absolute local dependency path (/Volumes/XCode/Braid/
    Packages/OpenAppleAPI) in DeepThink/Package.swift; that must be made portable first.
  - If you run everything in Swift package directly, you bypass Codex/clawdex thread state, approvals, and tool
    sandboxing.

  If you want true multi-agent execution against the same workspace/task, implementing it in clawdex (Rust) and exposing
  one new UI-bridge message type is cleaner long-term.

