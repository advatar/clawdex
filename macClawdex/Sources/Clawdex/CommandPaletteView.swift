import SwiftUI

struct CommandPaletteView: View {
    @EnvironmentObject var runtime: RuntimeManager
    @Binding var isPresented: Bool

    @State private var searchText: String = ""
    @State private var inputText: String = ""
    @State private var selectedId: PluginCommand.ID?

    var body: some View {
        VStack(spacing: 12) {
            header
            searchField
            commandList
            inputField
            actions
        }
        .padding()
        .frame(minWidth: 640, minHeight: 420)
        .onAppear {
            runtime.requestPluginCommands()
        }
    }

    private var header: some View {
        HStack {
            Text("Plugin Commands")
                .font(.headline)
            Spacer()
            Button("Reload") {
                runtime.requestPluginCommands()
            }
        }
    }

    private var searchField: some View {
        TextField("Search commands…", text: $searchText)
            .textFieldStyle(.roundedBorder)
    }

    private var commandList: some View {
        List(filteredCommands, selection: $selectedId) { cmd in
            VStack(alignment: .leading, spacing: 4) {
                Text("\(cmd.pluginName) / \(cmd.command)")
                    .font(.body)
                if let description = cmd.description, !description.isEmpty {
                    Text(description)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
            .tag(cmd.id)
        }
    }

    private var inputField: some View {
        VStack(alignment: .leading, spacing: 6) {
            Text("Input (optional)")
                .font(.caption)
                .foregroundStyle(.secondary)
            TextField("Additional input for the command…", text: $inputText, axis: .vertical)
                .lineLimit(1...4)
        }
    }

    private var actions: some View {
        HStack {
            Button("Cancel") {
                isPresented = false
            }
            Spacer()
            Button("Run") {
                runSelected()
            }
            .disabled(selectedCommand == nil)
            .keyboardShortcut(.return, modifiers: [.command])
        }
    }

    private var filteredCommands: [PluginCommand] {
        let trimmed = searchText.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty else { return runtime.pluginCommands }
        let needle = trimmed.lowercased()
        return runtime.pluginCommands.filter { cmd in
            cmd.pluginName.lowercased().contains(needle)
                || cmd.command.lowercased().contains(needle)
                || (cmd.description?.lowercased().contains(needle) ?? false)
        }
    }

    private var selectedCommand: PluginCommand? {
        guard let selectedId else { return nil }
        return runtime.pluginCommands.first { $0.id == selectedId }
    }

    private func runSelected() {
        guard let command = selectedCommand else { return }
        let input = inputText.trimmingCharacters(in: .whitespacesAndNewlines)
        runtime.runPluginCommand(command, input: input.isEmpty ? nil : input)
        inputText = ""
        isPresented = false
    }
}
