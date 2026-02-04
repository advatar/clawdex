import SwiftUI

@MainActor
final class TasksViewModel: ObservableObject {
    @Published var tasks: [TaskSummary] = []
    @Published var events: [TaskEvent] = []
    @Published var selectedTaskId: String? = nil
    @Published var currentRunId: String? = nil
    @Published var statusMessage: String = ""

    private let client = DaemonClient(baseURL: URL(string: "http://127.0.0.1:18791")!)
    private var pollTask: Task<Void, Never>?
    private var lastEventTs: Int64 = 0

    func refreshTasks() async {
        do {
            let tasks = try await client.fetchTasks()
            self.tasks = tasks.sorted { $0.createdAtMs > $1.createdAtMs }
            if selectedTaskId == nil {
                selectedTaskId = tasks.first?.id
            }
        } catch {
            statusMessage = "Failed to load tasks: \(error.localizedDescription)"
        }
    }

    func createTask(title: String) async {
        do {
            let task = try await client.createTask(title: title)
            tasks.insert(task, at: 0)
            selectedTaskId = task.id
        } catch {
            statusMessage = "Create failed: \(error.localizedDescription)"
        }
    }

    func startRun(title: String?, prompt: String) async {
        do {
            let run = try await client.startRun(taskId: selectedTaskId, title: title, prompt: prompt)
            currentRunId = run.id
            lastEventTs = 0
            events = []
            statusMessage = "Run started: \(run.id)"
            startPolling()
        } catch {
            statusMessage = "Run failed: \(error.localizedDescription)"
        }
    }

    func startPolling() {
        stopPolling()
        pollTask = Task { [weak self] in
            guard let self else { return }
            while !Task.isCancelled {
                await self.fetchEvents()
                try? await Task.sleep(nanoseconds: 1_000_000_000)
            }
        }
    }

    func stopPolling() {
        pollTask?.cancel()
        pollTask = nil
    }

    func fetchEvents() async {
        guard let runId = currentRunId else { return }
        do {
            let events = try await client.fetchEvents(runId: runId, after: lastEventTs, waitMs: 1000)
            if !events.isEmpty {
                self.events.append(contentsOf: events)
                self.lastEventTs = max(self.lastEventTs, events.map { $0.tsMs }.max() ?? self.lastEventTs)
            }
        } catch {
            // Ignore polling failures.
        }
    }
}

struct TasksView: View {
    @EnvironmentObject var runtime: RuntimeManager
    @StateObject private var viewModel = TasksViewModel()

    @State private var newTitle: String = ""
    @State private var prompt: String = ""

    var body: some View {
        HStack(spacing: 0) {
            sidebar
            Divider()
            detail
        }
        .onAppear {
            Task {
                await viewModel.refreshTasks()
            }
        }
        .onDisappear {
            viewModel.stopPolling()
        }
    }

    private var sidebar: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text("Tasks")
                    .font(.headline)
                Spacer()
                Button("Refresh") {
                    Task {
                        await viewModel.refreshTasks()
                    }
                }
            }

            List(selection: $viewModel.selectedTaskId) {
                ForEach(viewModel.tasks) { task in
                    VStack(alignment: .leading, spacing: 4) {
                        Text(task.title).font(.body)
                        if let last = task.lastRunAtMs {
                            Text("Last run: \(formatMs(last))")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        }
                    }
                    .tag(task.id)
                }
            }

            TextField("New task title", text: $newTitle)
                .textFieldStyle(.roundedBorder)
            Button("Create Task") {
                let title = newTitle.trimmingCharacters(in: .whitespacesAndNewlines)
                guard !title.isEmpty else { return }
                newTitle = ""
                Task {
                    await viewModel.createTask(title: title)
                }
            }
            .disabled(newTitle.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)

            Spacer()
            if !runtime.daemonRunning {
                Text("Daemon not running.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .padding()
        .frame(minWidth: 260, idealWidth: 300, maxWidth: 320)
    }

    private var detail: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Run Task")
                .font(.headline)

            TextEditor(text: $prompt)
                .frame(minHeight: 120)
                .overlay(RoundedRectangle(cornerRadius: 6).stroke(Color.secondary.opacity(0.2)))

            HStack {
                Button("Run") {
                    let trimmed = prompt.trimmingCharacters(in: .whitespacesAndNewlines)
                    guard !trimmed.isEmpty else { return }
                    Task {
                        await viewModel.startRun(title: nil, prompt: trimmed)
                    }
                }
                .disabled(prompt.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)

                Spacer()
                if !viewModel.statusMessage.isEmpty {
                    Text(viewModel.statusMessage)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }

            Divider()

            Text("Events")
                .font(.headline)

            ScrollView {
                LazyVStack(alignment: .leading, spacing: 8) {
                    ForEach(viewModel.events) { event in
                        VStack(alignment: .leading, spacing: 4) {
                            Text("\(event.kind) • \(formatMs(event.tsMs))")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                            Text(event.payload)
                                .font(.system(.body, design: .monospaced))
                                .textSelection(.enabled)
                        }
                        .padding(8)
                        .background(Color.gray.opacity(0.08))
                        .cornerRadius(6)
                    }
                }
                .padding(.vertical, 4)
            }
        }
        .padding()
    }

    private func formatMs(_ ms: Int64) -> String {
        let date = Date(timeIntervalSince1970: TimeInterval(ms) / 1000.0)
        return date.formatted(date: .abbreviated, time: .shortened)
    }
}
