import SwiftUI
import AppKit

struct PluginsView: View {
    @EnvironmentObject var runtime: RuntimeManager

    @State private var selectedPluginId: String?
    @State private var npmSpec: String = ""
    @State private var linkInstall: Bool = false

    var body: some View {
        HStack(spacing: 0) {
            sidebar
            Divider()
            detail
        }
        .onAppear {
            runtime.refreshPluginsSnapshot()
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

            List(selection: $selectedPluginId) {
                ForEach(runtime.plugins) { plugin in
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

            Spacer()
        }
        .padding()
        .frame(minWidth: 280, idealWidth: 320, maxWidth: 360)
    }

    private var detail: some View {
        VStack(alignment: .leading, spacing: 14) {
            header
            installControls
            Divider()
            pluginDetails
            Spacer()
        }
        .padding()
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
