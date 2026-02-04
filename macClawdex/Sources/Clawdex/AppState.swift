import Foundation

final class AppState: ObservableObject {
    // User-configurable settings
    @Published var workspaceDisplayPath: String = "Not set"
    @Published var launchAtLoginEnabled: Bool = false
    @Published var agentAutoStart: Bool = true
    @Published var internetEnabled: Bool = true
    @Published var workspaceReadOnly: Bool = false
    @Published var mcpAllowlist: String = ""
    @Published var mcpDenylist: String = ""

    // API key status
    @Published var hasOpenAIKey: Bool = false

    // Status / errors
    @Published var lastError: String? = nil
}
