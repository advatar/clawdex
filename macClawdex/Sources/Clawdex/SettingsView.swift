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
                        WorkspaceAccess.pickFolderAndPersistBookmark { result in
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
        }
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
