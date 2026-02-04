import Foundation

struct PluginInfo: Identifiable, Hashable {
    let id: String
    let name: String
    let hasMcp: Bool
    var mcpEnabled: Bool
}

struct PluginCommand: Identifiable, Hashable {
    let id: String
    let pluginId: String
    let pluginName: String
    let command: String
    let description: String?
}
