import Foundation
import Combine
import AppKit

final class RuntimeManager: ObservableObject {
    @Published private(set) var isRunning: Bool = false
    @Published private(set) var logs: [String] = []
    @Published private(set) var plugins: [PluginInfo] = []
    @Published private(set) var pluginCommands: [PluginCommand] = []
    @Published private(set) var daemonRunning: Bool = false

    private var appState: AppState?
    private var process: Process?
    private var daemonProcess: Process?
    private var stdinPipe: Pipe?
    private var stdoutPipe: Pipe?
    private var stderrPipe: Pipe?
    private var daemonStdoutPipe: Pipe?
    private var daemonStderrPipe: Pipe?
    private var workspaceURL: URL?

    private var cancellables = Set<AnyCancellable>()

    let assistantMessagePublisher = PassthroughSubject<String, Never>()
    let errorPublisher = PassthroughSubject<String, Never>()

    // Increment this whenever you change embedded tool wiring.
    private let toolsVersion = "0.3.0"

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
            let clawdexdURL = toolPaths.clawdexd

            let stateDir = try ensureStateDir()

            try startDaemonProcess(
                clawdexdURL: clawdexdURL,
                codexURL: codexURL,
                stateDir: stateDir
            )

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
            requestConfig()
            requestPlugins()

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
        if let p = daemonProcess {
            appendLog("[app] Stopping clawdexd (pid \(p.processIdentifier))…")
            p.terminate()
        }
        process = nil
        daemonProcess = nil
        stdinPipe = nil
        stdoutPipe = nil
        stderrPipe = nil
        daemonStdoutPipe = nil
        daemonStderrPipe = nil
        isRunning = false
        daemonRunning = false
    }

    func sendUserMessage(_ text: String) {
        guard isRunning else {
            errorPublisher.send("Agent is not running.")
            return
        }

        if let command = parsePluginCommand(text) {
            let payload: [String: Any] = [
                "type": "plugin_command",
                "pluginId": command.pluginId,
                "command": command.command,
                "input": command.input as Any
            ]
            sendControlMessage(payload)
            return
        }

        let payload: [String: Any] = [
            "type": "user_message",
            "text": text
        ]
        sendControlMessage(payload)
    }

    func requestConfig() {
        let payload: [String: Any] = ["type": "get_config"]
        sendControlMessage(payload)
    }

    func requestPlugins() {
        let payload: [String: Any] = [
            "type": "list_plugins",
            "includeDisabled": true
        ]
        sendControlMessage(payload)
    }

    func requestPluginCommands() {
        let payload: [String: Any] = [
            "type": "list_plugin_commands"
        ]
        sendControlMessage(payload)
    }

    func updatePermissions() {
        let allow = appState?.mcpAllowlist
            .split(separator: ",")
            .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
            .filter { !$0.isEmpty } ?? []
        let deny = appState?.mcpDenylist
            .split(separator: ",")
            .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
            .filter { !$0.isEmpty } ?? []

        let payload: [String: Any] = [
            "type": "update_config",
            "config": [
                "permissions": [
                    "internet": appState?.internetEnabled ?? true,
                    "mcp": [
                        "allow": allow,
                        "deny": deny
                    ]
                ],
                "workspace_policy": [
                    "read_only": appState?.workspaceReadOnly ?? false
                ]
            ]
        ]
        sendControlMessage(payload)
    }

    func setPluginMcpEnabled(pluginId: String, enabled: Bool) {
        guard isRunning else { return }
        let payload: [String: Any] = [
            "type": "update_config",
            "config": [
                "permissions": [
                    "mcp": [
                        "plugins": [
                            pluginId: enabled
                        ]
                    ]
                ]
            ]
        ]
        sendControlMessage(payload)
        updateLocalPluginMcpState(pluginId: pluginId, enabled: enabled)
    }

    func runPluginCommand(_ command: PluginCommand, input: String?) {
        let payload: [String: Any] = [
            "type": "plugin_command",
            "pluginId": command.pluginId,
            "command": command.command,
            "input": input ?? ""
        ]
        sendControlMessage(payload)
    }

    private func sendControlMessage(_ payload: [String: Any]) {
        guard let stdin = stdinPipe else {
            errorPublisher.send("No stdin pipe.")
            return
        }
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
        if fm.fileExists(atPath: destDir.path) {
            try fm.removeItem(at: destDir)
        }
        try fm.createDirectory(at: destDir, withIntermediateDirectories: true)

        // Embedded tool source dir (populated by Xcode build script)
        guard let srcRoot = Bundle.main.resourceURL?.appendingPathComponent("bin", isDirectory: true) else {
            throw NSError(domain: "Clawdex", code: 1, userInfo: [NSLocalizedDescriptionKey: "Missing app resources dir"])
        }

        let items = try fm.contentsOfDirectory(
            at: srcRoot,
            includingPropertiesForKeys: [.isDirectoryKey],
            options: [.skipsHiddenFiles]
        )

        for src in items {
            let dst = destDir.appendingPathComponent(src.lastPathComponent)
            if fm.fileExists(atPath: dst.path) {
                try fm.removeItem(at: dst)
            }
            try fm.copyItem(at: src, to: dst)

            let isDir = (try? src.resourceValues(forKeys: [.isDirectoryKey]).isDirectory) ?? false
            if !isDir {
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
        }

        UserDefaults.standard.set(toolsVersion, forKey: DefaultsKeys.toolsVersion)
        appendLog("[app] Installed tools into \(destDir.path) (version \(toolsVersion))")
    }

    private func toolInstallPaths() throws -> (codex: URL, clawdex: URL, clawdexd: URL) {
        let dir = try toolsDir()
        return (
            codex: dir.appendingPathComponent("codex"),
            clawdex: dir.appendingPathComponent("clawdex"),
            clawdexd: dir.appendingPathComponent("clawdexd")
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

    private func attachDaemonReaders(stdout: Pipe, stderr: Pipe) {
        let weakSelf = WeakRuntimeManager(self)
        stdout.fileHandleForReading.readabilityHandler = { h in
            let data = h.availableData
            if data.isEmpty { return }
            Task { @MainActor in
                weakSelf.value?.handleDaemonOutput(data: data, stream: "stdout")
            }
        }
        stderr.fileHandleForReading.readabilityHandler = { h in
            let data = h.availableData
            if data.isEmpty { return }
            Task { @MainActor in
                weakSelf.value?.handleDaemonOutput(data: data, stream: "stderr")
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
            } else if let config = parseConfig(from: text) {
                applyConfig(config)
            } else if let list = parsePlugins(from: text) {
                applyPlugins(list)
            } else if let commands = parsePluginCommands(from: text) {
                applyPluginCommands(commands)
            }
        }
    }

    private func handleDaemonOutput(data: Data, stream: String) {
        guard let s = String(data: data, encoding: .utf8) else { return }
        for line in s.split(separator: "\n", omittingEmptySubsequences: true) {
            let text = String(line)
            appendLog("[clawdexd][\(stream)] \(text)")
        }
    }

    private func startDaemonProcess(clawdexdURL: URL, codexURL: URL, stateDir: URL) throws {
        guard daemonProcess == nil else { return }
        let p = Process()
        p.executableURL = clawdexdURL

        var args: [String] = []
        args += ["--bind", "127.0.0.1:18791"]
        args += ["--codex-path", codexURL.path]
        args += ["--state-dir", stateDir.path]
        if let workspaceURL {
            args += ["--workspace", workspaceURL.path]
        }
        p.arguments = args

        var env = ProcessInfo.processInfo.environment
        if let openAIKey = try? Keychain.loadOpenAIKey(), !openAIKey.isEmpty {
            env["OPENAI_API_KEY"] = openAIKey
        }
        env["CLAWDEX_APP"] = "1"
        p.environment = env

        let outPipe = Pipe()
        let errPipe = Pipe()
        p.standardOutput = outPipe
        p.standardError = errPipe
        daemonStdoutPipe = outPipe
        daemonStderrPipe = errPipe
        attachDaemonReaders(stdout: outPipe, stderr: errPipe)

        try p.run()
        daemonProcess = p
        daemonRunning = true
        appendLog("[app] Started clawdexd (pid \(p.processIdentifier))")
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

    private func parseConfig(from line: String) -> [String: Any]? {
        guard line.first == "{" else { return nil }
        guard let data = line.data(using: .utf8) else { return nil }
        guard let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return nil }
        let kind = obj["type"] as? String
        guard kind == "config" || kind == "config_updated" else { return nil }
        return obj["config"] as? [String: Any]
    }

    private func parsePlugins(from line: String) -> [[String: Any]]? {
        guard line.first == "{" else { return nil }
        guard let data = line.data(using: .utf8) else { return nil }
        guard let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return nil }
        guard obj["type"] as? String == "plugins_list" else { return nil }
        return obj["plugins"] as? [[String: Any]]
    }

    private func parsePluginCommands(from line: String) -> [[String: Any]]? {
        guard line.first == "{" else { return nil }
        guard let data = line.data(using: .utf8) else { return nil }
        guard let obj = try? JSONSerialization.jsonObject(with: data) as? [String: Any] else { return nil }
        guard obj["type"] as? String == "plugin_commands" else { return nil }
        return obj["commands"] as? [[String: Any]]
    }

    private func applyConfig(_ config: [String: Any]) {
        if let permissions = config["permissions"] as? [String: Any] {
            if let internet = permissions["internet"] as? Bool {
                appState?.internetEnabled = internet
            }
            if let mcp = permissions["mcp"] as? [String: Any] {
                if let allow = mcp["allow"] as? [String] {
                    appState?.mcpAllowlist = allow.joined(separator: ", ")
                }
                if let deny = mcp["deny"] as? [String] {
                    appState?.mcpDenylist = deny.joined(separator: ", ")
                }
            }
        }
        if let workspacePolicy = config["workspace_policy"] as? [String: Any],
           let readOnly = workspacePolicy["read_only"] as? Bool {
            appState?.workspaceReadOnly = readOnly
        }
        requestPlugins()
    }

    private func applyPlugins(_ entries: [[String: Any]]) {
        let mapped = entries.compactMap { entry -> PluginInfo? in
            guard let id = entry["id"] as? String,
                  let name = entry["name"] as? String else { return nil }
            let hasMcp = entry["hasMcp"] as? Bool ?? false
            let mcpEnabled = entry["mcpEnabled"] as? Bool ?? false
            return PluginInfo(id: id, name: name, hasMcp: hasMcp, mcpEnabled: mcpEnabled)
        }
        plugins = mapped.sorted { $0.name.lowercased() < $1.name.lowercased() }
    }

    private func applyPluginCommands(_ entries: [[String: Any]]) {
        let mapped = entries.compactMap { entry -> PluginCommand? in
            let pluginId = (entry["plugin_id"] as? String) ?? (entry["pluginId"] as? String)
            let pluginName = (entry["plugin_name"] as? String) ?? (entry["pluginName"] as? String)
            guard let pluginId, let pluginName,
                  let command = entry["command"] as? String else { return nil }
            let description = entry["description"] as? String
            let id = "\(pluginId):\(command)"
            return PluginCommand(
                id: id,
                pluginId: pluginId,
                pluginName: pluginName,
                command: command,
                description: description
            )
        }
        pluginCommands = mapped.sorted { lhs, rhs in
            if lhs.pluginName.lowercased() == rhs.pluginName.lowercased() {
                return lhs.command.lowercased() < rhs.command.lowercased()
            }
            return lhs.pluginName.lowercased() < rhs.pluginName.lowercased()
        }
    }

    private func updateLocalPluginMcpState(pluginId: String, enabled: Bool) {
        if let idx = plugins.firstIndex(where: { $0.id == pluginId }) {
            plugins[idx].mcpEnabled = enabled
        }
    }

    private func parsePluginCommand(_ text: String) -> (pluginId: String, command: String, input: String?)? {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        guard trimmed.hasPrefix("/plugin") || trimmed.hasPrefix("/cmd") else { return nil }

        let parts = trimmed.split(separator: " ")
        guard parts.count >= 3 else { return nil }
        let pluginId = String(parts[1])
        let command = String(parts[2])
        let input = parts.count > 3 ? parts.dropFirst(3).joined(separator: " ") : nil
        return (pluginId: pluginId, command: command, input: input)
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
