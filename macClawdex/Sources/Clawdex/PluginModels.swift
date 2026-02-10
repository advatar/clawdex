import Foundation

struct PluginInstallInfo: Hashable {
    let source: String
    let spec: String?
    let sourcePath: String?
    let installPath: String?
    let version: String?
    let installedAtMs: Int64?
    let updatedAtMs: Int64?
}

struct PluginInfo: Identifiable, Hashable {
    let id: String
    let name: String
    let version: String?
    let description: String?
    let source: String?
    let path: String?
    var enabled: Bool
    let installedAtMs: Int64?
    let updatedAtMs: Int64?
    let skills: Int
    let commands: Int
    let hasMcp: Bool
    var mcpEnabled: Bool
    let manifestType: String?
    let manifestPath: String?
    let install: PluginInstallInfo?
}

struct PluginCommand: Identifiable, Hashable {
    let id: String
    let pluginId: String
    let pluginName: String
    let command: String
    let description: String?
}
