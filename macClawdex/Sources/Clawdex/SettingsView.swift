import SwiftUI

struct SettingsView: View {
    @EnvironmentObject var appState: AppState
    @EnvironmentObject var runtime: RuntimeManager

    @State private var openAIKey: String = ""
    @State private var keyStatus: String = ""
    @State private var workspaceStatus: String = ""

    var body: some View {
        Form {
            Section("Agent") {
                Toggle("Start agent automatically", isOn: $appState.agentAutoStart)
                    .onChange(of: appState.agentAutoStart) { _, newValue in
                        UserDefaults.standard.set(newValue, forKey: DefaultsKeys.agentAutoStart)
                    }

                Toggle("Enable DeepThink parallel prepass", isOn: $appState.parallelPrepassEnabled)
                    .onChange(of: appState.parallelPrepassEnabled) { _, newValue in
                        UserDefaults.standard.set(newValue, forKey: DefaultsKeys.parallelPrepassEnabled)
                    }

                Toggle("Launch at login", isOn: $appState.launchAtLoginEnabled)
                    .onChange(of: appState.launchAtLoginEnabled) { _, newValue in
                        LaunchAtLoginController.setEnabled(newValue)
                        refreshLaunchAtLoginStatus()
                    }

                HStack {
                    Button(runtime.isRunning ? "Stop" : "Start") {
                        runtime.isRunning ? runtime.stop() : runtime.start()
                    }
                    Text(runtime.isRunning ? "Running" : "Stopped")
                        .foregroundStyle(.secondary)
                }
            }

            Section("OpenAI") {
                SecureField("API key", text: $openAIKey)
                    .textContentType(.password)

                HStack {
                    Button("Save key") {
                        do {
                            try Keychain.saveOpenAIKey(openAIKey)
                            keyStatus = "Saved."
                        } catch {
                            keyStatus = "Save failed: \(error.localizedDescription)"
                        }
                        refreshKeyStatus()
                    }
                    Button("Clear key") {
                        do {
                            try Keychain.deleteOpenAIKey()
                            openAIKey = ""
                            keyStatus = "Cleared."
                        } catch {
                            keyStatus = "Clear failed: \(error.localizedDescription)"
                        }
                        refreshKeyStatus()
                    }
                    Text(keyStatus).foregroundStyle(.secondary)
                }

                Text("The agent reads the key from Keychain and passes it to the embedded runtime.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            Section("Workspace") {
                HStack {
                    Button("Choose workspace folder…") {
                        Task { @MainActor in
                            let result = await WorkspaceAccess.pickFolderAndPersistBookmark()
                            switch result {
                            case .success(let url):
                                workspaceStatus = "Workspace set: \(url.path)"
                                appState.workspaceDisplayPath = url.path
                            case .failure(let err):
                                workspaceStatus = "Workspace error: \(err.localizedDescription)"
                            }
                        }
                    }
                    Button("Clear workspace") {
                        WorkspaceAccess.clearWorkspaceBookmark()
                        workspaceStatus = "Workspace cleared."
                        appState.workspaceDisplayPath = "Not set"
                    }
                }

                Text(appState.workspaceDisplayPath)
                    .font(.caption)
                    .foregroundStyle(.secondary)

                Text(workspaceStatus)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            Section("Peer Assist (Antenna)") {
                Toggle("Enable peer assist", isOn: $appState.peerAssistEnabled)
                    .onChange(of: appState.peerAssistEnabled) { _, _ in
                        persistPeerSettings()
                    }

                TextField("Relay URL (POST endpoint)", text: $appState.peerRelayURL)
                    .onChange(of: appState.peerRelayURL) { _, _ in
                        persistPeerSettings()
                    }

                TextField("Category ENS", text: $appState.peerCategoryENS)
                    .onChange(of: appState.peerCategoryENS) { _, _ in
                        persistPeerSettings()
                    }

                TextField("Anonymous key (optional)", text: $appState.peerAnonKey)
                    .onChange(of: appState.peerAnonKey) { _, _ in
                        persistPeerSettings()
                    }

                Toggle("Auto-ask peers when stuck / for second opinion", isOn: $appState.peerAutoHelpEnabled)
                    .onChange(of: appState.peerAutoHelpEnabled) { _, _ in
                        persistPeerSettings()
                    }

                Toggle("Join peer discussions automatically", isOn: $appState.peerDiscussionEnabled)
                    .onChange(of: appState.peerDiscussionEnabled) { _, _ in
                        persistPeerSettings()
                    }

                Stepper(value: $appState.peerDiscussionIntervalMinutes, in: 5...720) {
                    Text("Discussion cadence (minutes): \(appState.peerDiscussionIntervalMinutes)")
                }
                .onChange(of: appState.peerDiscussionIntervalMinutes) { _, _ in
                    persistPeerSettings()
                }

                Text("Use `/peers <question>` for manual posts. Auto modes can also publish peer requests when you ask for a second opinion, when Clawdex appears stuck, or at the discussion cadence.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            Section("Permissions") {
                Toggle("Allow internet access", isOn: $appState.internetEnabled)
                Toggle("Read-only workspace", isOn: $appState.workspaceReadOnly)

                TextField("MCP allowlist (comma-separated)", text: $appState.mcpAllowlist)
                TextField("MCP denylist (comma-separated)", text: $appState.mcpDenylist)

                Button("Apply permissions") {
                    runtime.updatePermissions()
                }
                .disabled(!runtime.isRunning)

                Text("Changes are applied via the running Clawdex runtime.")
                    .font(.caption)
                    .foregroundStyle(.secondary)

                Divider()

                HStack {
                    Text("Per-plugin MCP")
                        .font(.subheadline)
                    Spacer()
                    Button("Refresh") {
                        runtime.requestPlugins()
                    }
                }

                if runtime.plugins.isEmpty {
                    Text("No plugins installed.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                } else {
                    ForEach(runtime.plugins.filter { $0.hasMcp }) { plugin in
                        Toggle("MCP: \(plugin.name)", isOn: Binding(
                            get: { plugin.mcpEnabled },
                            set: { runtime.setPluginMcpEnabled(pluginId: plugin.id, enabled: $0) }
                        ))
                    }
                }
            }

            Section("Memory") {
                Toggle("Enable memory", isOn: $appState.memoryEnabled)

                Picker("Citations", selection: $appState.memoryCitations) {
                    Text("Auto").tag("auto")
                    Text("On").tag("on")
                    Text("Off").tag("off")
                }
                .pickerStyle(.segmented)

                Toggle("Include session memory", isOn: $appState.memorySessionMemory)

                Divider()

                Toggle("Enable embeddings", isOn: $appState.embeddingsEnabled)
                    .disabled(!appState.memoryEnabled)

                TextField("Embeddings provider (optional)", text: $appState.embeddingsProvider)
                    .disabled(!appState.memoryEnabled)
                TextField("Embeddings model (optional)", text: $appState.embeddingsModel)
                    .disabled(!appState.memoryEnabled)
                TextField("Embeddings API base (optional)", text: $appState.embeddingsApiBase)
                    .disabled(!appState.memoryEnabled)
                TextField("Embeddings API key env (optional)", text: $appState.embeddingsApiKeyEnv)
                    .disabled(!appState.memoryEnabled)

                Stepper(value: $appState.embeddingsBatchSize, in: 0...256) {
                    let label = appState.embeddingsBatchSize == 0 ? "Default" : "\(appState.embeddingsBatchSize)"
                    Text("Embeddings batch size: \(label)")
                }
                .disabled(!appState.memoryEnabled)

                Divider()

                TextField("Extra paths (comma-separated)", text: $appState.memoryExtraPaths)
                    .disabled(!appState.memoryEnabled)

                Stepper(value: $appState.memoryChunkTokens, in: 1...2000) {
                    Text("Chunk tokens: \(appState.memoryChunkTokens)")
                }
                .disabled(!appState.memoryEnabled)
                .onChange(of: appState.memoryChunkTokens) { _, newValue in
                    if appState.memoryChunkOverlap > newValue {
                        appState.memoryChunkOverlap = newValue
                    }
                }

                Stepper(value: $appState.memoryChunkOverlap, in: 0...2000) {
                    Text("Chunk overlap: \(appState.memoryChunkOverlap)")
                }
                .disabled(!appState.memoryEnabled)

                Stepper(value: $appState.memorySyncIntervalMinutes, in: 0...1440) {
                    Text("Index sync (minutes): \(appState.memorySyncIntervalMinutes)")
                }
                .disabled(!appState.memoryEnabled)

                Button("Apply memory settings") {
                    runtime.updateMemorySettings()
                }
                .disabled(!runtime.isRunning)

                Text("Changes are applied via the running Clawdex runtime.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            Section("Embedded runtime") {
                Text("This app expects `codex` and `clawdex` to be embedded in Resources/bin by the build script.")
                    .font(.caption)
                    .foregroundStyle(.secondary)

                Button("Reinstall embedded tools into Application Support") {
                    do {
                        try runtime.installToolsIfNeeded(force: true)
                    } catch {
                        appState.lastError = error.localizedDescription
                    }
                }

                if let err = appState.lastError {
                    Text(err).foregroundStyle(.red)
                }
            }
        }
        .padding()
        .onAppear {
            refreshKeyStatus()
            refreshWorkspaceStatus()
            refreshLaunchAtLoginStatus()
            if runtime.isRunning {
                runtime.requestConfig()
                runtime.requestPlugins()
            }
        }
    }

    private func persistPeerSettings() {
        let defaults = UserDefaults.standard
        defaults.set(appState.peerAssistEnabled, forKey: DefaultsKeys.peerAssistEnabled)
        defaults.set(appState.peerRelayURL.trimmingCharacters(in: .whitespacesAndNewlines), forKey: DefaultsKeys.peerRelayURL)
        defaults.set(appState.peerCategoryENS.trimmingCharacters(in: .whitespacesAndNewlines), forKey: DefaultsKeys.peerCategoryENS)
        defaults.set(appState.peerAnonKey.trimmingCharacters(in: .whitespacesAndNewlines), forKey: DefaultsKeys.peerAnonKey)
        defaults.set(appState.peerAutoHelpEnabled, forKey: DefaultsKeys.peerAutoHelpEnabled)
        defaults.set(appState.peerDiscussionEnabled, forKey: DefaultsKeys.peerDiscussionEnabled)
        defaults.set(max(5, appState.peerDiscussionIntervalMinutes), forKey: DefaultsKeys.peerDiscussionIntervalMinutes)
    }

    private func refreshKeyStatus() {
        appState.hasOpenAIKey = (try? Keychain.loadOpenAIKey()) != nil
        if appState.hasOpenAIKey {
            keyStatus = "Key present in Keychain."
        } else {
            keyStatus = "No key saved."
        }
    }

    private func refreshWorkspaceStatus() {
        if let url = WorkspaceAccess.resolveWorkspaceURL() {
            appState.workspaceDisplayPath = url.path
        } else {
            appState.workspaceDisplayPath = "Not set"
        }
    }

    private func refreshLaunchAtLoginStatus() {
        appState.launchAtLoginEnabled = LaunchAtLoginController.isEnabled()
    }
}
