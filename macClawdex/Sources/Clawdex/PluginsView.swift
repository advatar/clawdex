import SwiftUI
import AppKit

struct PluginsView: View {
    @EnvironmentObject var runtime: RuntimeManager

    @State private var selectedPluginId: String?
    @State private var pluginSearchQuery: String = ""
    @State private var openClawHubQuery: String = ""
    @State private var npmSpec: String = ""
    @State private var linkInstall: Bool = false
    @State private var removePluginId: String?
    @State private var showRemoveConfirm: Bool = false

    var body: some View {
        HStack(spacing: 0) {
            sidebar
            Divider()
            detail
        }
        .onAppear {
            runtime.refreshPluginsSnapshot()
            runtime.searchOpenClawHubSkills(query: openClawHubQuery)
        }
        .alert("Remove plugin?", isPresented: $showRemoveConfirm) {
            Button("Remove", role: .destructive) {
                guard let removePluginId else { return }
                runtime.removePlugin(id: removePluginId, keepFiles: false)
                self.removePluginId = nil
            }
            Button("Cancel", role: .cancel) {
                removePluginId = nil
            }
        } message: {
            if let removePluginId,
               let plugin = runtime.plugins.first(where: { $0.id == removePluginId }) {
                Text("This will remove \"\(plugin.name)\" from Clawdex.")
            } else {
                Text("This will remove the plugin from Clawdex.")
            }
        }
    }

    private var sidebar: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text("Plugins")
                    .font(.headline)
                Spacer()
                Button("Refresh") {
                    runtime.refreshPluginsSnapshot()
                }
            }

            TextField("Search installed plugins/skills", text: $pluginSearchQuery)
                .textFieldStyle(.roundedBorder)

            List(selection: $selectedPluginId) {
                if filteredPlugins.isEmpty {
                    Text("No matching plugins.")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                } else {
                    ForEach(filteredPlugins) { plugin in
                        VStack(alignment: .leading, spacing: 4) {
                            Text(plugin.name)
                                .font(.body)
                            HStack(spacing: 6) {
                                if let version = plugin.version, !version.isEmpty {
                                    Text(version)
                                        .font(.caption)
                                        .foregroundStyle(.secondary)
                                }
                                if !plugin.enabled {
                                    Text("Disabled")
                                        .font(.caption)
                                        .foregroundStyle(.secondary)
                                }
                            }
                        }
                        .tag(plugin.id)
                    }
                }
            }

            Spacer()
        }
        .padding()
        .frame(minWidth: 280, idealWidth: 320, maxWidth: 360)
    }

    private var detail: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 14) {
                header
                installControls
                discoveryControls
                localSkillMatches
                Divider()
                pluginDetails
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            .padding()
        }
        .frame(minWidth: 520)
    }

    private var header: some View {
        HStack(alignment: .center) {
            VStack(alignment: .leading, spacing: 2) {
                Text("Plugin Manager")
                    .font(.headline)
                if runtime.pluginOperationInFlight {
                    Text(runtime.pluginOperationStatus.isEmpty ? "Working…" : runtime.pluginOperationStatus)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                } else if !runtime.pluginOperationStatus.isEmpty {
                    Text(runtime.pluginOperationStatus)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
            Spacer()
            if runtime.pluginOperationInFlight {
                ProgressView()
            }
            Button("Update All") {
                runtime.updateAllPlugins()
            }
            .disabled(runtime.pluginOperationInFlight)
        }
    }

    private var installControls: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("Install")
                .font(.headline)

            HStack(spacing: 8) {
                TextField("npm spec (e.g. @scope/plugin@latest)", text: $npmSpec)
                    .textFieldStyle(.roundedBorder)
                Button("Install from npm") {
                    let spec = npmSpec.trimmingCharacters(in: .whitespacesAndNewlines)
                    guard !spec.isEmpty else { return }
                    runtime.installPluginFromNpm(spec: spec)
                    npmSpec = ""
                }
                .disabled(runtime.pluginOperationInFlight)
            }

            HStack(spacing: 10) {
                Button("Install from folder…") {
                    choosePluginFolder()
                }
                .disabled(runtime.pluginOperationInFlight)

                Toggle("Link (dev)", isOn: $linkInstall)
                    .toggleStyle(.switch)
                    .help("Link keeps the plugin directory in place instead of copying it.")

                Spacer()
            }
        }
    }

    private var discoveryControls: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("Discover (OpenClawHub)")
                .font(.headline)

            HStack(spacing: 8) {
                TextField("Search OpenClawHub skills/plugins", text: $openClawHubQuery)
                    .textFieldStyle(.roundedBorder)
                    .onSubmit {
                        runtime.searchOpenClawHubSkills(query: openClawHubQuery)
                    }
                Button("Search") {
                    runtime.searchOpenClawHubSkills(query: openClawHubQuery)
                }
                .disabled(runtime.openClawHubSearchInFlight)
                Button("Popular") {
                    openClawHubQuery = ""
                    runtime.searchOpenClawHubSkills(query: "")
                }
                .disabled(runtime.openClawHubSearchInFlight)
            }

            if runtime.openClawHubSearchInFlight {
                HStack(spacing: 8) {
                    ProgressView()
                        .controlSize(.small)
                    Text("Loading OpenClawHub results…")
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            } else if !runtime.openClawHubSearchStatus.isEmpty {
                Text(runtime.openClawHubSearchStatus)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            if runtime.openClawHubSkills.isEmpty {
                Text("Search to discover community skills and plugin bundles.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else {
                VStack(alignment: .leading, spacing: 8) {
                    ForEach(Array(runtime.openClawHubSkills.prefix(8))) { skill in
                        VStack(alignment: .leading, spacing: 6) {
                            HStack(alignment: .firstTextBaseline) {
                                Text(skill.displayName)
                                    .font(.subheadline)
                                Spacer()
                                Text("↓\(skill.downloads) ★\(skill.stars)")
                                    .font(.caption2)
                                    .foregroundStyle(.secondary)
                            }
                            Text("/\(skill.slug)\(skill.latestVersion.map { " • \($0)" } ?? "")")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                            if !skill.summary.isEmpty {
                                Text(skill.summary)
                                    .font(.caption)
                                    .foregroundStyle(.secondary)
                                    .lineLimit(2)
                            }
                            HStack(spacing: 8) {
                                Button("Copy Install Cmd") {
                                    runtime.copyOpenClawHubInstallCommand(slug: skill.slug)
                                }
                                .buttonStyle(.bordered)
                                Button("Open") {
                                    runtime.openOpenClawHubSkillPage(slug: skill.slug)
                                }
                                .buttonStyle(.bordered)
                            }
                        }
                        .padding(.vertical, 4)
                    }
                }
            }
        }
    }

    private var localSkillMatches: some View {
        Group {
            let matches = filteredPluginCommands
            if !pluginSearchQuery.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                VStack(alignment: .leading, spacing: 8) {
                    Text("Local Skills / Commands")
                        .font(.headline)
                    if matches.isEmpty {
                        Text("No local skills/commands match \"\(pluginSearchQuery)\".")
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    } else {
                        ForEach(Array(matches.prefix(12))) { command in
                            VStack(alignment: .leading, spacing: 2) {
                                Text("\(command.pluginId):\(command.command)")
                                    .font(.caption)
                                if let description = command.description, !description.isEmpty {
                                    Text(description)
                                        .font(.caption2)
                                        .foregroundStyle(.secondary)
                                        .lineLimit(2)
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    private var pluginDetails: some View {
        VStack(alignment: .leading, spacing: 10) {
            Text("Selected Plugin")
                .font(.headline)

            if let plugin = selectedPlugin {
                HStack {
                    VStack(alignment: .leading, spacing: 4) {
                        Text(plugin.name)
                            .font(.title3)
                        Text(plugin.id)
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                    Spacer()
                    Button("Update") {
                        runtime.updatePlugin(id: plugin.id)
                    }
                    .disabled(runtime.pluginOperationInFlight || !pluginSupportsUpdate(plugin))

                    Button(plugin.enabled ? "Disable" : "Enable") {
                        runtime.setPluginEnabled(id: plugin.id, enabled: !plugin.enabled)
                    }
                    .disabled(runtime.pluginOperationInFlight)

                    Button("Remove") {
                        removePluginId = plugin.id
                        showRemoveConfirm = true
                    }
                    .disabled(runtime.pluginOperationInFlight)
                }

                Group {
                    if let version = plugin.version, !version.isEmpty {
                        infoRow("Version", version)
                    }
                    infoRow("Enabled", plugin.enabled ? "yes" : "no")
                    if let source = plugin.source, !source.isEmpty {
                        infoRow("Label", source)
                    }
                    infoRow("Assets", "\(plugin.skills) skills • \(plugin.commands) commands • MCP: \(plugin.hasMcp ? "yes" : "no")")
                    if let manifestType = plugin.manifestType {
                        infoRow("Manifest", manifestType)
                    }
                    if let install = plugin.install {
                        infoRow("Install Source", install.source)
                        if let spec = install.spec, !spec.isEmpty {
                            infoRow("Spec", spec)
                        }
                    }
                    if let path = plugin.path {
                        infoRow("Path", path)
                    }
                    if let desc = plugin.description, !desc.isEmpty {
                        Text(desc)
                            .font(.body)
                            .foregroundStyle(.secondary)
                            .padding(.top, 6)
                    }
                }
            } else {
                Text("Select a plugin from the list.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
    }

    private var selectedPlugin: PluginInfo? {
        guard let selectedPluginId else { return nil }
        return runtime.plugins.first { $0.id == selectedPluginId }
    }

    private var filteredPlugins: [PluginInfo] {
        let query = pluginSearchQuery
            .trimmingCharacters(in: .whitespacesAndNewlines)
            .lowercased()
        guard !query.isEmpty else {
            return runtime.plugins
        }
        return runtime.plugins.filter { plugin in
            let fields = [
                plugin.id,
                plugin.name,
                plugin.version ?? "",
                plugin.description ?? "",
                plugin.source ?? "",
                plugin.path ?? "",
            ].joined(separator: " ").lowercased()
            if fields.contains(query) {
                return true
            }
            return runtime.pluginCommands.contains { command in
                guard command.pluginId == plugin.id else { return false }
                let commandFields = [
                    command.command,
                    command.description ?? "",
                ].joined(separator: " ").lowercased()
                return commandFields.contains(query)
            }
        }
    }

    private var filteredPluginCommands: [PluginCommand] {
        let query = pluginSearchQuery
            .trimmingCharacters(in: .whitespacesAndNewlines)
            .lowercased()
        guard !query.isEmpty else {
            return runtime.pluginCommands
        }
        return runtime.pluginCommands.filter { command in
            let fields = [
                command.pluginId,
                command.pluginName,
                command.command,
                command.description ?? "",
            ].joined(separator: " ").lowercased()
            return fields.contains(query)
        }
    }

    private func choosePluginFolder() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = false
        panel.canChooseDirectories = true
        panel.allowsMultipleSelection = false
        panel.prompt = "Choose"
        panel.message = "Select a plugin folder (containing openclaw.plugin.json or plugin.json)."
        if panel.runModal() == .OK, let url = panel.url {
            runtime.installPluginFromFolder(url, link: linkInstall)
        }
    }

    private func pluginSupportsUpdate(_ plugin: PluginInfo) -> Bool {
        plugin.install?.source.lowercased() == "npm"
    }

    @ViewBuilder
    private func infoRow(_ key: String, _ value: String) -> some View {
        HStack(alignment: .top, spacing: 8) {
            Text(key)
                .font(.caption)
                .foregroundStyle(.secondary)
                .frame(width: 110, alignment: .leading)
            Text(value)
                .font(.caption)
                .textSelection(.enabled)
            Spacer(minLength: 0)
        }
    }
}
