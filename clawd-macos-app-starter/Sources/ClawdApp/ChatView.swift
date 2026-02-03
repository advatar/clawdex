import SwiftUI

struct ChatView: View {
    @EnvironmentObject var appState: AppState
    @EnvironmentObject var runtime: RuntimeManager

    @State private var input: String = ""
    @State private var messages: [ChatMessage] = [
        ChatMessage(role: .system, text: "Clawd (Codex-powered) — macOS app shell. Configure API key + workspace in Settings.")
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
    }

    private var header: some View {
        HStack {
            VStack(alignment: .leading) {
                Text("Clawd")
                    .font(.headline)
                Text(runtime.isRunning ? "Agent running" : "Agent stopped")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
            Spacer()
            Button(runtime.isRunning ? "Stop" : "Start") {
                if runtime.isRunning { runtime.stop() } else { runtime.start() }
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
        HStack {
            TextField("Message…", text: $input, axis: .vertical)
                .lineLimit(1...6)
            Button("Send") {
                send()
            }
            .keyboardShortcut(.return, modifiers: [.command])
            .disabled(input.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)
        }
        .padding()
    }

    private func send() {
        let text = input.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !text.isEmpty else { return }
        input = ""

        messages.append(ChatMessage(role: .user, text: text))

        if !runtime.isRunning {
            runtime.start()
        }
        runtime.sendUserMessage(text)
    }

    private func label(for role: ChatMessage.Role) -> String {
        switch role {
        case .user: return "You"
        case .assistant: return "Clawd"
        case .system: return "System"
        }
    }
}
