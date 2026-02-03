import Foundation
import Combine
import AppKit

final class RuntimeManager: ObservableObject {
    @Published private(set) var isRunning: Bool = false
    @Published private(set) var logs: [String] = []

    private var appState: AppState?
    private var process: Process?
    private var stdinPipe: Pipe?
    private var stdoutPipe: Pipe?
    private var stderrPipe: Pipe?
    private var workspaceURL: URL?

    private var cancellables = Set<AnyCancellable>()

    let assistantMessagePublisher = PassthroughSubject<String, Never>()
    let errorPublisher = PassthroughSubject<String, Never>()

    // Increment this whenever you change embedded tool wiring.
    private let toolsVersion = "0.1.0"

    func bootstrap(appState: AppState) {
        self.appState = appState

        // Load persisted settings
        if UserDefaults.standard.object(forKey: DefaultsKeys.agentAutoStart) == nil {
            UserDefaults.standard.set(true, forKey: DefaultsKeys.agentAutoStart)
        }
        appState.agentAutoStart = UserDefaults.standard.bool(forKey: DefaultsKeys.agentAutoStart)
        appState.launchAtLoginEnabled = LaunchAtLoginController.isEnabled()
        appState.hasOpenAIKey = (try? Keychain.loadOpenAIKey()) != nil

        // Optional: auto-start agent
        if appState.agentAutoStart {
            start()
        }

        // Stop child process on app termination
        NotificationCenter.default.publisher(for: NSApplication.willTerminateNotification)
            .sink { [weak self] _ in
                self?.stop()
            }
            .store(in: &cancellables)
    }

    func start() {
        guard !isRunning else { return }
        guard let appState else { return }

        do {
            try installToolsIfNeeded(force: false)
        } catch {
            appState.lastError = "Tool install failed: \(error.localizedDescription)"
            errorPublisher.send(appState.lastError ?? "Tool install failed")
            return
        }

        // Workspace access (optional but recommended)
        workspaceURL = WorkspaceAccess.resolveWorkspaceURL()

        // Load API key from Keychain
        let openAIKey = (try? Keychain.loadOpenAIKey()) ?? ""
        appState.hasOpenAIKey = !openAIKey.isEmpty
        if openAIKey.isEmpty {
            errorPublisher.send("No OpenAI API key found. Set it in Settings.")
            return
        }

        do {
            let toolPaths = try toolInstallPaths()
            let clawdexURL = toolPaths.clawdex
            let codexURL = toolPaths.codex

            let stateDir = try ensureStateDir()

            let p = Process()
            p.executableURL = clawdexURL

            // NOTE: These arguments are a suggested contract. Implement them in clawdex.
            // You can change them, but keep the app and clawdex in sync.
            var args: [String] = []
            args += ["ui-bridge", "--stdio"]  // recommended: JSONL over stdin/stdout
            args += ["--codex-path", codexURL.path]
            args += ["--state-dir", stateDir.path]

            if let workspaceURL {
                args += ["--workspace", workspaceURL.path]
            }

            p.arguments = args

            // Environment: pass API key and any other required vars.
            var env = ProcessInfo.processInfo.environment
            env["OPENAI_API_KEY"] = openAIKey
            env["CLAWDEX_APP"] = "1"
            p.environment = env

            // Pipes
            let inPipe = Pipe()
            let outPipe = Pipe()
            let errPipe = Pipe()

            p.standardInput = inPipe
            p.standardOutput = outPipe
            p.standardError = errPipe

            self.stdinPipe = inPipe
            self.stdoutPipe = outPipe
            self.stderrPipe = errPipe

            // Stream logs
            attachReaders(stdout: outPipe, stderr: errPipe)

            try p.run()
            self.process = p
            self.isRunning = true
            appendLog("[app] Started clawdex (pid \(p.processIdentifier))")

        } catch {
            appState.lastError = error.localizedDescription
            errorPublisher.send(error.localizedDescription)
        }
    }

    func stop() {
        workspaceURL.map { WorkspaceAccess.stopAccessing($0) }
        workspaceURL = nil

        if let p = process {
            appendLog("[app] Stopping clawdex (pid \(p.processIdentifier))…")
            p.terminate()
        }
        process = nil
        stdinPipe = nil
        stdoutPipe = nil
        stderrPipe = nil
        isRunning = false
    }

    func sendUserMessage(_ text: String) {
        guard isRunning else {
            errorPublisher.send("Agent is not running.")
            return
        }
        guard let stdin = stdinPipe else {
            errorPublisher.send("No stdin pipe.")
            return
        }

        // Simple JSONL protocol (implement in clawdex):
        // {"type":"user_message","text":"..."}
        let payload: [String: Any] = [
            "type": "user_message",
            "text": text
        ]
        do {
            let data = try JSONSerialization.data(withJSONObject: payload, options: [])
            if let line = String(data: data, encoding: .utf8) {
                stdin.fileHandleForWriting.write((line + "\n").data(using: .utf8)!)
            }
        } catch {
            errorPublisher.send("Failed to encode message: \(error.localizedDescription)")
        }
    }

    // MARK: - Tool installation

    func installToolsIfNeeded(force: Bool) throws {
        let installedVersion = UserDefaults.standard.string(forKey: DefaultsKeys.toolsVersion)
        if !force, installedVersion == toolsVersion {
            return
        }

        let fm = FileManager.default
        let destDir = try toolsDir()
        try fm.createDirectory(at: destDir, withIntermediateDirectories: true)

        // Embedded tool source dir (populated by Xcode build script)
        guard let srcRoot = Bundle.main.resourceURL?.appendingPathComponent("bin", isDirectory: true) else {
            throw NSError(domain: "Clawdex", code: 1, userInfo: [NSLocalizedDescriptionKey: "Missing app resources dir"])
        }

        let tools = ["codex", "clawdex"]
        for tool in tools {
            let src = srcRoot.appendingPathComponent(tool)
            let dst = destDir.appendingPathComponent(tool)

            guard fm.fileExists(atPath: src.path) else {
                throw NSError(domain: "Clawdex", code: 2, userInfo: [NSLocalizedDescriptionKey: "Missing embedded tool: \(src.path)"])
            }

            if fm.fileExists(atPath: dst.path) {
                try fm.removeItem(at: dst)
            }
            try fm.copyItem(at: src, to: dst)

            // Ensure executable bit
            var attrs = try fm.attributesOfItem(atPath: dst.path)
            if let p = attrs[.posixPermissions] as? NSNumber {
                let current = p.intValue
                // add u+x
                attrs[.posixPermissions] = NSNumber(value: current | 0o100)
                try fm.setAttributes(attrs, ofItemAtPath: dst.path)
            } else {
                try fm.setAttributes([.posixPermissions: 0o755], ofItemAtPath: dst.path)
            }
        }

        UserDefaults.standard.set(toolsVersion, forKey: DefaultsKeys.toolsVersion)
        appendLog("[app] Installed tools into \(destDir.path) (version \(toolsVersion))")
    }

    private func toolInstallPaths() throws -> (codex: URL, clawdex: URL) {
        let dir = try toolsDir()
        return (
            codex: dir.appendingPathComponent("codex"),
            clawdex: dir.appendingPathComponent("clawdex")
        )
    }

    private func toolsDir() throws -> URL {
        let base = try appSupportDir()
        return base.appendingPathComponent("tools", isDirectory: true)
    }

    private func ensureStateDir() throws -> URL {
        let base = try appSupportDir()
        let state = base.appendingPathComponent("state", isDirectory: true)
        try FileManager.default.createDirectory(at: state, withIntermediateDirectories: true)
        return state
    }

    private func appSupportDir() throws -> URL {
        let fm = FileManager.default
        guard let base = fm.urls(for: .applicationSupportDirectory, in: .userDomainMask).first else {
            throw NSError(domain: "Clawdex", code: 3, userInfo: [NSLocalizedDescriptionKey: "No Application Support directory"])
        }
        let bid = Bundle.main.bundleIdentifier ?? "Clawdex"
        let dir = base.appendingPathComponent(bid, isDirectory: true)
        try fm.createDirectory(at: dir, withIntermediateDirectories: true)
        return dir
    }

    // MARK: - Log streaming

    private final class WeakRuntimeManager: @unchecked Sendable {
        weak var value: RuntimeManager?
        init(_ value: RuntimeManager) {
            self.value = value
        }
    }

    private func attachReaders(stdout: Pipe, stderr: Pipe) {
        let weakSelf = WeakRuntimeManager(self)
        stdout.fileHandleForReading.readabilityHandler = { h in
            let data = h.availableData
            if data.isEmpty { return }
            Task { @MainActor in
                weakSelf.value?.handleOutput(data: data, stream: "stdout")
            }
        }
        stderr.fileHandleForReading.readabilityHandler = { h in
            let data = h.availableData
            if data.isEmpty { return }
            Task { @MainActor in
                weakSelf.value?.handleOutput(data: data, stream: "stderr")
            }
        }
    }

    private func handleOutput(data: Data, stream: String) {
        guard let s = String(data: data, encoding: .utf8) else { return }
        for line in s.split(separator: "\n", omittingEmptySubsequences: true) {
            let text = String(line)
            appendLog("[clawdex][\(stream)] \(text)")

            // Suggested convention: clawdex prints JSON lines for UI events.
            // Example:
            //   {"type":"assistant_message","text":"hello"}
            //   {"type":"error","message":"..."}
            if let msg = parseAssistantMessage(from: text) {
                assistantMessagePublisher.send(msg)
            } else if let err = parseError(from: text) {
                errorPublisher.send(err)
            }
        }
    }

    private func parseAssistantMessage(from line: String) -> String? {
        guard line.first == "{" else { return nil }
        guard let data = line.data(using: .utf8) else { return nil }
        guard let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return nil }
        guard obj["type"] as? String == "assistant_message" else { return nil }
        return obj["text"] as? String
    }

    private func parseError(from line: String) -> String? {
        guard line.first == "{" else { return nil }
        guard let data = line.data(using: .utf8) else { return nil }
        guard let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return nil }
        guard obj["type"] as? String == "error" else { return nil }
        return (obj["message"] as? String) ?? "Unknown error"
    }

    private func appendLog(_ line: String) {
        let weakSelf = WeakRuntimeManager(self)
        Task { @MainActor in
            guard let strongSelf = weakSelf.value else { return }
            strongSelf.logs.append(line)
            // keep memory bounded
            if strongSelf.logs.count > 2000 {
                strongSelf.logs.removeFirst(strongSelf.logs.count - 2000)
            }
        }
    }
}
