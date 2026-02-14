import Foundation

final class AppState: ObservableObject {
    // User-configurable settings
    @Published var workspaceDisplayPath: String = "Not set"
    @Published var launchAtLoginEnabled: Bool = false
    @Published var agentAutoStart: Bool = true
    @Published var parallelPrepassEnabled: Bool = false
    @Published var internetEnabled: Bool = true
    @Published var workspaceReadOnly: Bool = false
    @Published var mcpAllowlist: String = ""
    @Published var mcpDenylist: String = ""

    // Peer assist (Antenna)
    @Published var peerAssistEnabled: Bool = false
    @Published var peerRelayURL: String = ""
    @Published var peerCategoryENS: String = "clawdex.peers"
    @Published var peerAnonKey: String = ""
    @Published var peerAutoHelpEnabled: Bool = true
    @Published var peerDiscussionEnabled: Bool = true
    @Published var peerDiscussionIntervalMinutes: Int = 45

    // Memory (config-backed)
    @Published var memoryEnabled: Bool = true
    @Published var memoryCitations: String = "auto" // auto | on | off
    @Published var memorySessionMemory: Bool = false
    @Published var memoryExtraPaths: String = "" // comma-separated
    @Published var memoryChunkTokens: Int = 400
    @Published var memoryChunkOverlap: Int = 80
    @Published var memorySyncIntervalMinutes: Int = 0 // 0 disables periodic sync

    // Embeddings (config-backed)
    @Published var embeddingsEnabled: Bool = true
    @Published var embeddingsProvider: String = "" // empty = use default
    @Published var embeddingsModel: String = "" // empty = use default
    @Published var embeddingsApiBase: String = ""
    @Published var embeddingsApiKeyEnv: String = ""
    @Published var embeddingsBatchSize: Int = 0 // 0 = use default

    // API key status
    @Published var hasOpenAIKey: Bool = false

    // Status / errors
    @Published var lastError: String? = nil
}
