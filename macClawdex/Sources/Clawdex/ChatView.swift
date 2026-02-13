import SwiftUI
import AppKit
import UniformTypeIdentifiers

struct ChatView: View {
    @EnvironmentObject var appState: AppState
    @EnvironmentObject var runtime: RuntimeManager

    @State private var input: String = ""
    @State private var showCommandPalette: Bool = false
    @State private var attachments: [ChatImageAttachment] = []
    @State private var messages: [ChatMessage] = [
        ChatMessage(role: .system, text: "Clawdex (Codex-powered) — macOS app shell. Configure API key + workspace in Settings. Plugin commands: /plugin <id> <command> [input]. Peer assist: /peers <question>.")
    ]

    var body: some View {
        VStack(spacing: 0) {
            header
            Divider()
            conversation
            Divider()
            composer
        }
        .frame(minWidth: 760, minHeight: 520)
        .onReceive(runtime.assistantMessagePublisher) { text in
            messages.append(ChatMessage(role: .assistant, text: text))
        }
        .onReceive(runtime.errorPublisher) { err in
            messages.append(ChatMessage(role: .system, text: "Error: \(err)"))
        }
        .sheet(isPresented: $showCommandPalette) {
            CommandPaletteView(isPresented: $showCommandPalette)
                .environmentObject(runtime)
        }
    }

    private var header: some View {
        HStack {
            VStack(alignment: .leading) {
                Text("Clawdex")
                    .font(.headline)
                Text(runtime.isRunning ? "Agent running" : "Agent stopped")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer()
            Button(runtime.isRunning ? "Stop" : "Start") {
                if runtime.isRunning { runtime.stop() } else { runtime.start() }
            }
            Button("Commands") {
                showCommandPalette = true
            }
            Button("Settings") {
                NSApp.sendAction(Selector(("showSettingsWindow:")), to: nil, from: nil)
            }
        }
        .padding()
    }

    private var conversation: some View {
        ScrollViewReader { proxy in
            ScrollView {
                LazyVStack(alignment: .leading, spacing: 10) {
                    ForEach(messages) { m in
                        HStack(alignment: .top) {
                            Text(label(for: m.role))
                                .font(.caption)
                                .foregroundStyle(.secondary)
                                .frame(width: 70, alignment: .leading)

                            Text(m.text)
                                .textSelection(.enabled)

                            Spacer(minLength: 0)
                        }
                        .id(m.id)
                    }
                }
                .padding()
            }
            .onChange(of: messages.count) { _, _ in
                if let last = messages.last {
                    proxy.scrollTo(last.id, anchor: .bottom)
                }
            }
        }
    }

    private var composer: some View {
        VStack(alignment: .leading, spacing: 8) {
            if !attachments.isEmpty {
                attachmentsPreview
            }

            HStack {
                TextField("Message…", text: $input, axis: .vertical)
                    .lineLimit(1...6)

                Button("Attach…") {
                    chooseImages()
                }

                Button("Send") {
                    send()
                }
                .keyboardShortcut(.return, modifiers: [.command])
                .disabled(!canSend)
            }
        }
        .padding()
    }

    private func send() {
        let text = input.trimmingCharacters(in: .whitespacesAndNewlines)
        let localImages = attachments.map { $0.url.path }
        guard !text.isEmpty || !localImages.isEmpty else { return }

        input = ""
        attachments = []

        if let peerPrompt = parsePeerAssistCommand(from: text) {
            if peerPrompt.isEmpty {
                messages.append(ChatMessage(role: .system, text: "Peer assist usage: /peers <question>"))
                return
            }
            if !localImages.isEmpty {
                messages.append(ChatMessage(role: .system, text: "Peer assist does not support image attachments yet."))
                return
            }

            messages.append(ChatMessage(role: .user, text: "/peers \(peerPrompt)"))
            Task { @MainActor in
                do {
                    let published = try await runtime.publishPeerHelpRequest(peerPrompt)
                    messages.append(
                        ChatMessage(
                            role: .system,
                            text: "Peer request published. event=\(published.eventID) topic=\(published.topic) replies=\(published.repliesTopic) relay=\(published.relayURL.absoluteString)"
                        )
                    )
                } catch {
                    messages.append(ChatMessage(role: .system, text: "Peer assist failed: \(error.localizedDescription)"))
                }
            }
            return
        }

        let displayText: String
        if text.isEmpty {
            displayText = "Sent \(localImages.count) image(s)."
        } else if localImages.isEmpty {
            displayText = text
        } else {
            displayText = "\(text) [\(localImages.count) image(s)]"
        }
        messages.append(ChatMessage(role: .user, text: displayText))

        if !runtime.isRunning {
            runtime.start()
        }
        runtime.sendUserMessage(text, localImagePaths: localImages)
    }

    private func parsePeerAssistCommand(from text: String) -> String? {
        let trimmed = text.trimmingCharacters(in: .whitespacesAndNewlines)
        if trimmed == "/peers" {
            return ""
        }
        guard trimmed.hasPrefix("/peers ") else {
            return nil
        }
        return String(trimmed.dropFirst("/peers".count)).trimmingCharacters(in: .whitespacesAndNewlines)
    }

    private func label(for role: ChatMessage.Role) -> String {
        switch role {
        case .user: return "You"
        case .assistant: return "Clawdex"
        case .system: return "System"
        }
    }

    private var canSend: Bool {
        !input.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty || !attachments.isEmpty
    }

    private var attachmentsPreview: some View {
        ScrollView(.horizontal) {
            HStack(spacing: 10) {
                ForEach(attachments) { att in
                    ZStack(alignment: .topTrailing) {
                        if let image = NSImage(contentsOf: att.url) {
                            Image(nsImage: image)
                                .resizable()
                                .aspectRatio(contentMode: .fill)
                                .frame(width: 64, height: 64)
                                .clipped()
                                .cornerRadius(8)
                        } else {
                            RoundedRectangle(cornerRadius: 8)
                                .fill(Color.gray.opacity(0.12))
                                .frame(width: 64, height: 64)
                                .overlay(
                                    Image(systemName: "photo")
                                        .foregroundStyle(.secondary)
                                )
                        }

                        Button {
                            attachments.removeAll { $0.id == att.id }
                        } label: {
                            Image(systemName: "xmark.circle.fill")
                                .foregroundStyle(.secondary)
                        }
                        .buttonStyle(.plain)
                        .padding(4)
                    }
                }
            }
            .padding(.vertical, 2)
        }
        .frame(height: 72)
    }

    private func chooseImages() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = true
        panel.allowedContentTypes = [UTType.image]
        panel.prompt = "Attach"
        panel.message = "Choose image(s) to attach."

        if panel.runModal() == .OK {
            let existing = Set(attachments.map { $0.url.path })
            for url in panel.urls {
                if existing.contains(url.path) {
                    continue
                }
                attachments.append(ChatImageAttachment(url: url))
            }
        }
    }
}

private struct ChatImageAttachment: Identifiable, Hashable {
    let id: UUID
    let url: URL

    init(url: URL) {
        self.id = UUID()
        self.url = url
    }
}
