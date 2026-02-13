import Foundation
import Combine
import AppKit
import ParallellVibe

final class RuntimeManager: ObservableObject {
    @Published private(set) var isRunning: Bool = false
    @Published private(set) var logs: [String] = []
    @Published private(set) var plugins: [PluginInfo] = []
    @Published private(set) var pluginCommands: [PluginCommand] = []
    @Published private(set) var daemonRunning: Bool = false
    @Published private(set) var gatewayRunning: Bool = false
    @Published private(set) var pluginOperationInFlight: Bool = false
    @Published private(set) var pluginOperationStatus: String = ""

    private var appState: AppState?
    private var process: Process?
    private var daemonProcess: Process?
    private var gatewayProcess: Process?
    private var stdinPipe: Pipe?
    private var stdoutPipe: Pipe?
    private var stderrPipe: Pipe?
    private var daemonStdoutPipe: Pipe?
    private var daemonStderrPipe: Pipe?
    private var gatewayStdoutPipe: Pipe?
    private var gatewayStderrPipe: Pipe?
    private var workspaceURL: URL?

    private var cancellables = Set<AnyCancellable>()
    private let toolsInstallLock = NSLock()

    let assistantMessagePublisher = PassthroughSubject<String, Never>()
    let errorPublisher = PassthroughSubject<String, Never>()

    // Increment this whenever you change embedded tool wiring.
    private let toolsVersion = "0.3.0"
    private let openclawPluginsVersion = "2"
    private let openclawPluginsSourceLabel = "bundled-openclaw"
    private let blockedBundledOpenClawPlugins: [String: String] = [
        "matrix": "Blocked due to GHSA-p8p7-x288-28g6 (`request` SSRF) in transitive dependency chain."
    ]

    func bootstrap(appState: AppState) {
        self.appState = appState

        // Load persisted settings
        if UserDefaults.standard.object(forKey: DefaultsKeys.agentAutoStart) == nil {
            UserDefaults.standard.set(true, forKey: DefaultsKeys.agentAutoStart)
        }
        if UserDefaults.standard.object(forKey: DefaultsKeys.parallelPrepassEnabled) == nil {
            UserDefaults.standard.set(false, forKey: DefaultsKeys.parallelPrepassEnabled)
        }
        if UserDefaults.standard.object(forKey: DefaultsKeys.peerAssistEnabled) == nil {
            UserDefaults.standard.set(false, forKey: DefaultsKeys.peerAssistEnabled)
        }
        if UserDefaults.standard.object(forKey: DefaultsKeys.peerCategoryENS) == nil {
            UserDefaults.standard.set("clawdex.peers", forKey: DefaultsKeys.peerCategoryENS)
        }

        appState.agentAutoStart = UserDefaults.standard.bool(forKey: DefaultsKeys.agentAutoStart)
        appState.parallelPrepassEnabled = UserDefaults.standard.bool(forKey: DefaultsKeys.parallelPrepassEnabled)
        appState.peerAssistEnabled = UserDefaults.standard.bool(forKey: DefaultsKeys.peerAssistEnabled)
        appState.peerRelayURL = UserDefaults.standard.string(forKey: DefaultsKeys.peerRelayURL) ?? ""
        appState.peerCategoryENS = UserDefaults.standard.string(forKey: DefaultsKeys.peerCategoryENS) ?? "clawdex.peers"
        appState.peerAnonKey = UserDefaults.standard.string(forKey: DefaultsKeys.peerAnonKey) ?? ""

        appState.launchAtLoginEnabled = LaunchAtLoginController.isEnabled()
        appState.hasOpenAIKey = (try? Keychain.loadOpenAIKey()) != nil

        preinstallOpenClawPluginsIfNeeded()

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
            try startGatewayProcess(
                clawdexURL: clawdexURL,
                stateDir: stateDir
            )

            // Load API key from Keychain
            let openAIKey = (try? Keychain.loadOpenAIKey()) ?? ""
            appState.hasOpenAIKey = !openAIKey.isEmpty
            if openAIKey.isEmpty {
                errorPublisher.send("No OpenAI API key found. Set it in Settings.")
                return
            }

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
            requestEventSubscription()

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
        if let p = gatewayProcess {
            appendLog("[app] Stopping clawdex gateway (pid \(p.processIdentifier))…")
            p.terminate()
        }
        process = nil
        daemonProcess = nil
        gatewayProcess = nil
        stdinPipe = nil
        stdoutPipe = nil
        stderrPipe = nil
        daemonStdoutPipe = nil
        daemonStderrPipe = nil
        gatewayStdoutPipe = nil
        gatewayStderrPipe = nil
        isRunning = false
        daemonRunning = false
        gatewayRunning = false
    }

    func sendUserMessage(_ text: String) {
        sendUserMessage(text, localImagePaths: [])
    }

    func sendUserMessage(_ text: String, localImagePaths: [String]) {
        guard isRunning else {
            errorPublisher.send("Agent is not running.")
            return
        }

        let cleaned = localImagePaths
            .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
            .filter { !$0.isEmpty }

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

        if shouldRunParallelPrepass(prompt: text, localImagePaths: cleaned) {
            runParallelPrepassAndSend(prompt: text, localImagePaths: cleaned)
            return
        }

        sendUserPayload(text: text, localImagePaths: cleaned)
    }

    @MainActor
    func publishPeerHelpRequest(_ question: String) async throws -> PeerHelpPublishResult {
        guard let appState else {
            throw NSError(
                domain: "Clawdex",
                code: 3001,
                userInfo: [NSLocalizedDescriptionKey: "App state unavailable."]
            )
        }

        guard appState.peerAssistEnabled else {
            throw NSError(
                domain: "Clawdex",
                code: 3002,
                userInfo: [NSLocalizedDescriptionKey: "Peer assist is disabled in Settings."]
            )
        }

        let relayRaw = appState.peerRelayURL.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !relayRaw.isEmpty, let relayURL = URL(string: relayRaw) else {
            throw NSError(
                domain: "Clawdex",
                code: 3003,
                userInfo: [NSLocalizedDescriptionKey: "Peer relay URL is missing or invalid."]
            )
        }

        let category = appState.peerCategoryENS.trimmingCharacters(in: .whitespacesAndNewlines)
        let anonKey = appState.peerAnonKey.trimmingCharacters(in: .whitespacesAndNewlines)

        let appVersion = Bundle.main.infoDictionary?["CFBundleShortVersionString"] as? String ?? "dev"
        let sourceLabel = "clawdex-mac/\(appVersion)"
        let capabilities = ["codex-runtime", "gateway-attachments", "plugins", "memory-hybrid-search"]

        let result = try await AntennaPeerAssist.publishHelpRequest(
            question: question,
            relayURL: relayURL,
            categoryENS: category,
            anonKey: anonKey,
            sourceLabel: sourceLabel,
            capabilities: capabilities
        )

        appendLog("[app] Peer assist published event \(result.eventID) to \(result.topic) (replies: \(result.repliesTopic))")
        return result
    }

    private func shouldRunParallelPrepass(prompt: String, localImagePaths: [String]) -> Bool {
        guard appState?.parallelPrepassEnabled == true else { return false }
        guard localImagePaths.isEmpty else { return false }
        return !prompt.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
    }

    private func sendUserPayload(text: String, localImagePaths: [String]) {
        var payload: [String: Any] = [
            "type": "user_message",
            "text": text
        ]
        if !localImagePaths.isEmpty {
            payload["localImages"] = localImagePaths
        }
        sendControlMessage(payload)
    }

    private func runParallelPrepassAndSend(prompt: String, localImagePaths: [String]) {
        let weakSelf = WeakRuntimeManager(self)
        appendLog("[app] DeepThink prepass started.")

        DispatchQueue.global(qos: .userInitiated).async {
            Task {
                guard let strongSelf = weakSelf.value else { return }

                let finalPrompt: String
                do {
                    finalPrompt = try await strongSelf.buildParallelPrepassPrompt(for: prompt)
                    weakSelf.value?.appendLog("[app] DeepThink prepass selected a candidate.")
                } catch {
                    finalPrompt = prompt
                    weakSelf.value?.appendLog("[app] DeepThink prepass failed: \(error.localizedDescription). Using original prompt.")
                }

                Task { @MainActor in
                    weakSelf.value?.sendUserPayload(text: finalPrompt, localImagePaths: localImagePaths)
                }
            }
        }
    }

    private func buildParallelPrepassPrompt(for prompt: String) async throws -> String {
        let trimmedKey = try Keychain.loadOpenAIKey()?
            .trimmingCharacters(in: .whitespacesAndNewlines) ?? ""
        guard !trimmedKey.isEmpty else {
            throw NSError(
                domain: "Clawdex",
                code: 1001,
                userInfo: [NSLocalizedDescriptionKey: "OpenAI API key is missing."]
            )
        }

        let environment = ProcessInfo.processInfo.environment
        let endpointRaw = environment["CLAWDEX_PARALLEL_API_URL"] ?? "https://api.openai.com/v1/chat/completions"
        guard let endpoint = URL(string: endpointRaw) else {
            throw NSError(
                domain: "Clawdex",
                code: 1002,
                userInfo: [NSLocalizedDescriptionKey: "Invalid parallel API URL: \(endpointRaw)"]
            )
        }
        let model = resolveParallelModel(environment: environment)

        let provider = APILLMProvider(
            config: .openAICompatible(endpoint: endpoint, apiKey: trimmedKey, model: model)
        )
        var configuration = ParallelVibeConfiguration.default
        configuration.candidateCount = resolveParallelCandidateCount(environment: environment)
        configuration.refineRounds = resolveParallelRefineRounds(environment: environment)
        configuration.allowParallelGeneration = true
        configuration.allowParallelVerification = true

        let result = try await ParallelVibeEngine(
            provider: provider,
            configuration: configuration
        ).run(prompt: prompt)

        let selected = result.selected.candidate.output
        return renderParallelPrompt(originalPrompt: prompt, selected: selected)
    }

    private func resolveParallelModel(environment: [String: String]) -> String {
        let configured = environment["CLAWDEX_PARALLEL_MODEL"]?
            .trimmingCharacters(in: .whitespacesAndNewlines)
        if let configured, !configured.isEmpty {
            return configured
        }
        return "gpt-4.1-mini"
    }

    private func resolveParallelCandidateCount(environment: [String: String]) -> Int {
        if let parsed = parseParallelInt(environment["CLAWDEX_PARALLEL_CANDIDATES"]) {
            return min(max(parsed, 1), 8)
        }
        return 3
    }

    private func resolveParallelRefineRounds(environment: [String: String]) -> Int {
        if let parsed = parseParallelInt(environment["CLAWDEX_PARALLEL_REFINE_ROUNDS"]) {
            return min(max(parsed, 0), 4)
        }
        return 1
    }

    private func parseParallelInt(_ raw: String?) -> Int? {
        guard let raw else { return nil }
        return Int(raw.trimmingCharacters(in: .whitespacesAndNewlines))
    }

    private func renderParallelPrompt(originalPrompt: String, selected: CandidateOutput) -> String {
        var lines: [String] = []
        lines.append("The user request appears below.")
        lines.append("A parallel prepass generated a candidate answer. Use it as a starting point, but verify independently and use tools as needed.")
        lines.append("")
        lines.append("PREPASS_FINAL_ANSWER:")
        lines.append(selected.finalAnswer)

        if !selected.keySteps.isEmpty {
            lines.append("")
            lines.append("PREPASS_KEY_STEPS:")
            for step in selected.keySteps {
                lines.append("- \(step)")
            }
        }

        if !selected.failureModes.isEmpty {
            lines.append("")
            lines.append("PREPASS_FAILURE_MODES:")
            for mode in selected.failureModes {
                lines.append("- \(mode)")
            }
        }

        lines.append("")
        lines.append("ORIGINAL_USER_REQUEST:")
        lines.append(originalPrompt)
        return lines.joined(separator: "\n")
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

    func requestEventSubscription() {
        let payload: [String: Any] = [
            "type": "subscribe_events",
            "subscriptionId": "mac-ui",
            "kinds": [
                "turn_started",
                "turn_completed",
                "item_started",
                "item_completed",
                "agent_message_delta",
                "error",
            ],
        ]
        sendControlMessage(payload)
    }

    func refreshPluginsSnapshot() {
        if isRunning {
            requestPlugins()
            return
        }
        refreshPluginsViaCli()
    }

    func ensureGatewayRunning() {
        guard gatewayProcess == nil else { return }
        do {
            try installToolsIfNeeded(force: false)
            let toolPaths = try toolInstallPaths()
            let stateDir = try ensureStateDir()
            try startGatewayProcess(clawdexURL: toolPaths.clawdex, stateDir: stateDir)
        } catch {
            appendLog("[app] Gateway start failed: \(error.localizedDescription)")
            errorPublisher.send("Gateway start failed: \(error.localizedDescription)")
        }
    }

    func installPluginFromFolder(_ url: URL, link: Bool) {
        let args = [
            "plugins",
            "add",
            "--path",
            url.path,
        ] + (link ? ["--link"] : []) + [
            "--source",
            "mac-app",
        ]
        runPluginManagerCommand(args: args, label: "Installing plugin from folder")
    }

    func installPluginFromNpm(spec: String) {
        let trimmed = spec.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return }
        let args = [
            "plugins",
            "add",
            "--npm",
            trimmed,
            "--source",
            "mac-app",
        ]
        runPluginManagerCommand(args: args, label: "Installing plugin from npm")
    }

    func updatePlugin(id: String) {
        let trimmed = id.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return }
        let args = [
            "plugins",
            "update",
            "--id",
            trimmed,
        ]
        runPluginManagerCommand(args: args, label: "Updating plugin \(trimmed)")
    }

    func updateAllPlugins() {
        let args = [
            "plugins",
            "update",
            "--all",
        ]
        runPluginManagerCommand(args: args, label: "Updating all plugins")
    }

    func setPluginEnabled(id: String, enabled: Bool) {
        let trimmed = id.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return }
        let args = [
            "plugins",
            enabled ? "enable" : "disable",
            "--id",
            trimmed,
        ]
        runPluginManagerCommand(
            args: args,
            label: enabled ? "Enabling plugin \(trimmed)" : "Disabling plugin \(trimmed)"
        )
    }

    func removePlugin(id: String, keepFiles: Bool) {
        let trimmed = id.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return }
        let args = [
            "plugins",
            "remove",
            "--id",
            trimmed,
        ] + (keepFiles ? ["--keep-files"] : [])
        runPluginManagerCommand(args: args, label: "Removing plugin \(trimmed)")
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

    func updateMemorySettings() {
        guard isRunning else { return }
        let citations = normalizeCitationsMode(appState?.memoryCitations ?? "auto")

        let provider = (appState?.embeddingsProvider ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
        let model = (appState?.embeddingsModel ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
        let apiBase = (appState?.embeddingsApiBase ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
        let apiKeyEnv = (appState?.embeddingsApiKeyEnv ?? "").trimmingCharacters(in: .whitespacesAndNewlines)
        let batchSize = max(0, appState?.embeddingsBatchSize ?? 0)

        let extraPaths = splitList(appState?.memoryExtraPaths ?? "")
        let extraPathsValue: Any = extraPaths.isEmpty ? NSNull() : extraPaths

        let chunkTokens = max(1, appState?.memoryChunkTokens ?? 400)
        let rawOverlap = max(0, appState?.memoryChunkOverlap ?? 80)
        let chunkOverlap = min(rawOverlap, chunkTokens)

        let syncMinutes = max(0, appState?.memorySyncIntervalMinutes ?? 0)
        let syncValue: Any = syncMinutes > 0 ? ["interval_minutes": syncMinutes] : NSNull()

        let providerValue: Any = provider.isEmpty ? NSNull() : provider
        let modelValue: Any = model.isEmpty ? NSNull() : model
        let apiBaseValue: Any = apiBase.isEmpty ? NSNull() : apiBase
        let apiKeyEnvValue: Any = apiKeyEnv.isEmpty ? NSNull() : apiKeyEnv
        let batchSizeValue: Any = batchSize > 0 ? batchSize : NSNull()

        let memory: [String: Any] = [
            "enabled": appState?.memoryEnabled ?? true,
            "citations": citations,
            "session_memory": appState?.memorySessionMemory ?? false,
            "extra_paths": extraPathsValue,
            "chunk_tokens": chunkTokens,
            "chunk_overlap": chunkOverlap,
            "sync": syncValue,
            "embeddings": [
                "enabled": appState?.embeddingsEnabled ?? true,
                "provider": providerValue,
                "model": modelValue,
                "api_base": apiBaseValue,
                "api_key_env": apiKeyEnvValue,
                "batch_size": batchSizeValue,
            ]
        ]

        let payload: [String: Any] = [
            "type": "update_config",
            "config": [
                "memory": memory
            ]
        ]
        sendControlMessage(payload)
    }

    private func refreshPluginsViaCli() {
        let weakSelf = WeakRuntimeManager(self)
        DispatchQueue.global(qos: .utility).async {
            guard let self = weakSelf.value else { return }
            do {
                try self.installToolsIfNeeded(force: false)
                let toolPaths = try self.toolInstallPaths()
                let stateDir = try self.ensureStateDir()
                var args = [
                    "plugins",
                    "list",
                    "--include-disabled",
                    "--state-dir",
                    stateDir.path,
                ]
                if let workspaceURL = self.workspaceURL {
                    args += ["--workspace", workspaceURL.path]
                }
                let result = try self.runClawdexCommand(clawdexURL: toolPaths.clawdex, args: args)
                guard let data = result.stdout.data(using: .utf8),
                      let obj = try JSONSerialization.jsonObject(with: data) as? [String: Any],
                      let plugins = obj["plugins"] as? [[String: Any]] else {
                    return
                }
                Task { @MainActor in
                    weakSelf.value?.applyPlugins(plugins)
                }
            } catch {
                Task { @MainActor in
                    weakSelf.value?.appendLog("[app] Plugin list failed: \(error.localizedDescription)")
                }
            }
        }
    }

    private func runPluginManagerCommand(args: [String], label: String) {
        guard !pluginOperationInFlight else { return }
        pluginOperationInFlight = true
        pluginOperationStatus = "\(label)…"

        let weakSelf = WeakRuntimeManager(self)
        DispatchQueue.global(qos: .userInitiated).async {
            guard let self = weakSelf.value else { return }
            do {
                try self.installToolsIfNeeded(force: false)
                let toolPaths = try self.toolInstallPaths()
                let stateDir = try self.ensureStateDir()

                var fullArgs = args
                fullArgs += ["--state-dir", stateDir.path]
                if let workspaceURL = self.workspaceURL {
                    fullArgs += ["--workspace", workspaceURL.path]
                }

                let result = try self.runClawdexCommand(clawdexURL: toolPaths.clawdex, args: fullArgs)
                if !result.stdout.isEmpty { self.appendLog(result.stdout) }
                if !result.stderr.isEmpty { self.appendLog(result.stderr) }

                Task { @MainActor in
                    weakSelf.value?.pluginOperationInFlight = false
                    weakSelf.value?.pluginOperationStatus = "\(label) complete."
                    weakSelf.value?.refreshPluginsSnapshot()
                    if weakSelf.value?.isRunning == true {
                        weakSelf.value?.requestPluginCommands()
                    }
                }
            } catch {
                Task { @MainActor in
                    weakSelf.value?.pluginOperationInFlight = false
                    weakSelf.value?.pluginOperationStatus = "\(label) failed."
                    weakSelf.value?.appendLog("[app] \(label) failed: \(error.localizedDescription)")
                }
            }
        }
    }

    private func normalizeCitationsMode(_ raw: String) -> String {
        let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines).lowercased()
        if trimmed == "on" {
            return "on"
        }
        if trimmed == "off" {
            return "off"
        }
        return "auto"
    }

    private func splitList(_ raw: String) -> [String] {
        raw.split { $0 == "," || $0 == "\n" }
            .map { String($0).trimmingCharacters(in: .whitespacesAndNewlines) }
            .filter { !$0.isEmpty }
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
        toolsInstallLock.lock()
        defer { toolsInstallLock.unlock() }
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

    private func preinstallOpenClawPluginsIfNeeded() {
        let defaults = UserDefaults.standard
        if defaults.string(forKey: DefaultsKeys.openclawPluginsVersion) == openclawPluginsVersion {
            return
        }

        let weakSelf = WeakRuntimeManager(self)
        DispatchQueue.global(qos: .utility).async {
            guard let self = weakSelf.value else { return }
            let defaults = UserDefaults.standard
            do {
                try self.installToolsIfNeeded(force: false)
                let toolPaths = try self.toolInstallPaths()
                let stateDir = try self.ensureStateDir()
                let pluginRoots = try self.bundledOpenClawPluginRoots()
                if pluginRoots.isEmpty {
                    self.appendLog("[app] No bundled OpenClaw plugins found.")
                    defaults.set(self.openclawPluginsVersion, forKey: DefaultsKeys.openclawPluginsVersion)
                    return
                }

                self.appendLog("[app] Preinstalling bundled OpenClaw plugins...")
                var failures = 0
                for root in pluginRoots {
                    if let pluginID = self.bundledOpenClawPluginID(for: root),
                       let reason = self.blockedBundledOpenClawPlugins[pluginID]
                    {
                        self.appendLog("[app] Skipping bundled plugin \(pluginID): \(reason)")
                        do {
                            try self.removeBundledOpenClawPluginIfInstalled(
                                clawdexURL: toolPaths.clawdex,
                                stateDir: stateDir,
                                pluginID: pluginID
                            )
                        } catch {
                            self.appendLog("[app] Failed to remove blocked plugin \(pluginID): \(error.localizedDescription)")
                        }
                        continue
                    }
                    do {
                        try self.installBundledOpenClawPlugin(
                            clawdexURL: toolPaths.clawdex,
                            stateDir: stateDir,
                            pluginDir: root
                        )
                    } catch {
                        failures += 1
                        self.appendLog("[app] Failed to install \(root.lastPathComponent): \(error.localizedDescription)")
                    }
                }

                if failures == 0 {
                    defaults.set(self.openclawPluginsVersion, forKey: DefaultsKeys.openclawPluginsVersion)
                    self.appendLog("[app] OpenClaw plugin preinstall complete.")
                } else {
                    self.appendLog("[app] OpenClaw plugin preinstall finished with \(failures) failures.")
                }
            } catch {
                self.appendLog("[app] OpenClaw plugin preinstall error: \(error.localizedDescription)")
            }
        }
    }

    private func bundledOpenClawPluginRoots() throws -> [URL] {
        guard let root = Bundle.main.resourceURL?.appendingPathComponent("openclaw-extensions", isDirectory: true) else {
            return []
        }
        let fm = FileManager.default
        guard fm.fileExists(atPath: root.path) else {
            return []
        }
        let entries = try fm.contentsOfDirectory(
            at: root,
            includingPropertiesForKeys: [.isDirectoryKey],
            options: [.skipsHiddenFiles]
        )
        var plugins: [URL] = []
        for entry in entries {
            let isDir = (try? entry.resourceValues(forKeys: [.isDirectoryKey]).isDirectory) ?? false
            if !isDir { continue }
            let manifest = entry.appendingPathComponent("openclaw.plugin.json")
            if fm.fileExists(atPath: manifest.path) {
                plugins.append(entry)
            }
        }
        return plugins.sorted { $0.lastPathComponent.lowercased() < $1.lastPathComponent.lowercased() }
    }

    private func bundledOpenClawPluginID(for pluginDir: URL) -> String? {
        let manifestURL = pluginDir.appendingPathComponent("openclaw.plugin.json")
        guard let data = try? Data(contentsOf: manifestURL),
              let json = try? JSONSerialization.jsonObject(with: data) as? [String: Any],
              let pluginID = json["id"] as? String
        else {
            return nil
        }
        let trimmed = pluginID.trimmingCharacters(in: .whitespacesAndNewlines)
        return trimmed.isEmpty ? nil : trimmed
    }

    private func removeBundledOpenClawPluginIfInstalled(
        clawdexURL: URL,
        stateDir: URL,
        pluginID: String
    ) throws {
        var args = [
            "plugins",
            "remove",
            "--id",
            pluginID,
            "--state-dir",
            stateDir.path
        ]
        if let workspaceURL {
            args += ["--workspace", workspaceURL.path]
        }
        _ = try runClawdexCommand(clawdexURL: clawdexURL, args: args)
        appendLog("[app] Removed blocked plugin \(pluginID).")
    }

    private func installBundledOpenClawPlugin(
        clawdexURL: URL,
        stateDir: URL,
        pluginDir: URL
    ) throws {
        var args = [
            "plugins",
            "add",
            "--path",
            pluginDir.path,
            "--source",
            openclawPluginsSourceLabel,
            "--state-dir",
            stateDir.path
        ]
        if let workspaceURL {
            args += ["--workspace", workspaceURL.path]
        }
        let result = try runClawdexCommand(clawdexURL: clawdexURL, args: args)
        if !result.stdout.isEmpty {
            self.appendLog(result.stdout)
        }
        if !result.stderr.isEmpty {
            self.appendLog(result.stderr)
        }
    }

    private func runClawdexCommand(clawdexURL: URL, args: [String]) throws -> (stdout: String, stderr: String) {
        let process = Process()
        process.executableURL = clawdexURL
        process.arguments = args

        var env = ProcessInfo.processInfo.environment
        env["CLAWDEX_APP"] = "1"
        process.environment = env

        let outPipe = Pipe()
        let errPipe = Pipe()
        process.standardOutput = outPipe
        process.standardError = errPipe

        try process.run()
        process.waitUntilExit()

        let outData = outPipe.fileHandleForReading.readDataToEndOfFile()
        let errData = errPipe.fileHandleForReading.readDataToEndOfFile()
        let stdout = String(data: outData, encoding: .utf8) ?? ""
        let stderr = String(data: errData, encoding: .utf8) ?? ""

        if process.terminationStatus != 0 {
            let detail = stderr.isEmpty ? stdout : stderr
            throw NSError(
                domain: "Clawdex",
                code: Int(process.terminationStatus),
                userInfo: [NSLocalizedDescriptionKey: detail.isEmpty ? "clawdex command failed" : detail]
            )
        }
        return (stdout.trimmingCharacters(in: .whitespacesAndNewlines),
                stderr.trimmingCharacters(in: .whitespacesAndNewlines))
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

    private func attachGatewayReaders(stdout: Pipe, stderr: Pipe) {
        let weakSelf = WeakRuntimeManager(self)
        stdout.fileHandleForReading.readabilityHandler = { h in
            let data = h.availableData
            if data.isEmpty { return }
            Task { @MainActor in
                weakSelf.value?.handleGatewayOutput(data: data, stream: "stdout")
            }
        }
        stderr.fileHandleForReading.readabilityHandler = { h in
            let data = h.availableData
            if data.isEmpty { return }
            Task { @MainActor in
                weakSelf.value?.handleGatewayOutput(data: data, stream: "stderr")
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

    private func handleGatewayOutput(data: Data, stream: String) {
        guard let s = String(data: data, encoding: .utf8) else { return }
        for line in s.split(separator: "\n", omittingEmptySubsequences: true) {
            let text = String(line)
            appendLog("[clawdex-gateway][\(stream)] \(text)")
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

    private func startGatewayProcess(clawdexURL: URL, stateDir: URL) throws {
        guard gatewayProcess == nil else { return }
        let p = Process()
        p.executableURL = clawdexURL

        var args: [String] = []
        args += ["gateway"]
        args += ["--bind", "127.0.0.1:18789"]
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
        gatewayStdoutPipe = outPipe
        gatewayStderrPipe = errPipe
        attachGatewayReaders(stdout: outPipe, stderr: errPipe)

        try p.run()
        gatewayProcess = p
        gatewayRunning = true
        appendLog("[app] Started clawdex gateway (pid \(p.processIdentifier))")
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

        if let memory = config["memory"] as? [String: Any] {
            if let enabled = memory["enabled"] as? Bool {
                appState?.memoryEnabled = enabled
            }

            let citationsAny = memory["citations"]
            if let citations = citationsAny as? String {
                appState?.memoryCitations = normalizeCitationsMode(citations)
            } else if citationsAny is NSNull {
                appState?.memoryCitations = "auto"
            }

            let sessionAny = memory["session_memory"] ?? memory["sessionMemory"]
            if let enabled = sessionAny as? Bool {
                appState?.memorySessionMemory = enabled
            } else if sessionAny is NSNull {
                appState?.memorySessionMemory = false
            }

            let extraAny = memory["extra_paths"] ?? memory["extraPaths"]
            if let list = extraAny as? [String] {
                appState?.memoryExtraPaths = list.joined(separator: ", ")
            } else if let list = extraAny as? [Any] {
                let strings = list.compactMap { $0 as? String }
                appState?.memoryExtraPaths = strings.joined(separator: ", ")
            } else if extraAny is NSNull {
                appState?.memoryExtraPaths = ""
            }

            let chunkTokensAny = memory["chunk_tokens"] ?? memory["chunkTokens"]
            if let tokens = intFromAny(chunkTokensAny) {
                appState?.memoryChunkTokens = tokens
            } else if chunkTokensAny is NSNull {
                appState?.memoryChunkTokens = 400
            }

            let chunkOverlapAny = memory["chunk_overlap"] ?? memory["chunkOverlap"]
            if let overlap = intFromAny(chunkOverlapAny) {
                appState?.memoryChunkOverlap = overlap
            } else if chunkOverlapAny is NSNull {
                appState?.memoryChunkOverlap = 80
            }

            let syncAny = memory["sync"]
            if let sync = syncAny as? [String: Any] {
                let intervalAny = sync["interval_minutes"] ?? sync["intervalMinutes"]
                if let interval = intFromAny(intervalAny) {
                    appState?.memorySyncIntervalMinutes = max(0, interval)
                } else if intervalAny is NSNull {
                    appState?.memorySyncIntervalMinutes = 0
                }
            } else if syncAny is NSNull {
                appState?.memorySyncIntervalMinutes = 0
            }

            let embeddingsAny = memory["embeddings"]
            if let embeddings = embeddingsAny as? [String: Any] {
                if let enabled = embeddings["enabled"] as? Bool {
                    appState?.embeddingsEnabled = enabled
                }

                let providerAny = embeddings["provider"]
                if let provider = providerAny as? String {
                    appState?.embeddingsProvider = provider
                } else if providerAny is NSNull {
                    appState?.embeddingsProvider = ""
                }

                let modelAny = embeddings["model"]
                if let model = modelAny as? String {
                    appState?.embeddingsModel = model
                } else if modelAny is NSNull {
                    appState?.embeddingsModel = ""
                }

                let apiBaseAny = embeddings["api_base"] ?? embeddings["apiBase"]
                if let apiBase = apiBaseAny as? String {
                    appState?.embeddingsApiBase = apiBase
                } else if apiBaseAny is NSNull {
                    appState?.embeddingsApiBase = ""
                }

                let apiKeyEnvAny = embeddings["api_key_env"] ?? embeddings["apiKeyEnv"]
                if let apiKeyEnv = apiKeyEnvAny as? String {
                    appState?.embeddingsApiKeyEnv = apiKeyEnv
                } else if apiKeyEnvAny is NSNull {
                    appState?.embeddingsApiKeyEnv = ""
                }

                let batchAny = embeddings["batch_size"] ?? embeddings["batchSize"]
                if let batch = intFromAny(batchAny) {
                    appState?.embeddingsBatchSize = max(0, batch)
                } else if batchAny is NSNull {
                    appState?.embeddingsBatchSize = 0
                }
            } else if embeddingsAny is NSNull {
                appState?.embeddingsEnabled = true
                appState?.embeddingsProvider = ""
                appState?.embeddingsModel = ""
                appState?.embeddingsApiBase = ""
                appState?.embeddingsApiKeyEnv = ""
                appState?.embeddingsBatchSize = 0
            }
        }
        requestPlugins()
    }

    private func applyPlugins(_ entries: [[String: Any]]) {
        let mapped = entries.compactMap { entry -> PluginInfo? in
            guard let id = entry["id"] as? String,
                  let name = entry["name"] as? String else { return nil }
            let version = entry["version"] as? String
            let description = entry["description"] as? String
            let source = entry["source"] as? String
            let path = entry["path"] as? String
            let enabled = entry["enabled"] as? Bool ?? true
            let installedAtMs = int64FromAny(entry["installedAtMs"])
            let updatedAtMs = int64FromAny(entry["updatedAtMs"])
            let skills = intFromAny(entry["skills"]) ?? 0
            let commands = intFromAny(entry["commands"]) ?? 0
            let hasMcp = entry["hasMcp"] as? Bool ?? false
            let mcpEnabled = entry["mcpEnabled"] as? Bool ?? false
            let manifestType = entry["manifestType"] as? String
            let manifestPath = entry["manifestPath"] as? String
            let install = parseInstallInfo(entry["install"])

            return PluginInfo(
                id: id,
                name: name,
                version: version,
                description: description,
                source: source,
                path: path,
                enabled: enabled,
                installedAtMs: installedAtMs,
                updatedAtMs: updatedAtMs,
                skills: skills,
                commands: commands,
                hasMcp: hasMcp,
                mcpEnabled: mcpEnabled,
                manifestType: manifestType,
                manifestPath: manifestPath,
                install: install
            )
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

    private func int64FromAny(_ any: Any?) -> Int64? {
        if let value = any as? Int64 {
            return value
        }
        if let value = any as? Int {
            return Int64(value)
        }
        if let value = any as? NSNumber {
            return value.int64Value
        }
        if let value = any as? String {
            return Int64(value.trimmingCharacters(in: .whitespacesAndNewlines))
        }
        return nil
    }

    private func intFromAny(_ any: Any?) -> Int? {
        if let value = any as? Int {
            return value
        }
        if let value = any as? Int64 {
            return Int(value)
        }
        if let value = any as? NSNumber {
            return value.intValue
        }
        if let value = any as? String {
            return Int(value.trimmingCharacters(in: .whitespacesAndNewlines))
        }
        return nil
    }

    private func parseInstallInfo(_ any: Any?) -> PluginInstallInfo? {
        guard let dict = any as? [String: Any] else { return nil }
        guard let source = dict["source"] as? String else { return nil }
        let spec = dict["spec"] as? String
        let sourcePath = (dict["sourcePath"] as? String) ?? (dict["source_path"] as? String)
        let installPath = (dict["installPath"] as? String) ?? (dict["install_path"] as? String)
        let version = dict["version"] as? String
        let installedAtMs = int64FromAny(dict["installedAtMs"] ?? dict["installed_at_ms"])
        let updatedAtMs = int64FromAny(dict["updatedAtMs"] ?? dict["updated_at_ms"])

        return PluginInstallInfo(
            source: source,
            spec: spec,
            sourcePath: sourcePath,
            installPath: installPath,
            version: version,
            installedAtMs: installedAtMs,
            updatedAtMs: updatedAtMs
        )
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
