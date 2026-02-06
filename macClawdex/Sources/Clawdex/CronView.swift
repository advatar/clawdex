import SwiftUI

@MainActor
final class CronViewModel: ObservableObject {
    @Published var jobs: [CronJob] = []
    @Published var statusMessage: String = ""

    private let client = DaemonClient(baseURL: URL(string: "http://127.0.0.1:18791")!)

    func refresh() async {
        do {
            let jobs = try await client.fetchCronJobs(includeDisabled: true)
            self.jobs = jobs.sorted { $0.name.lowercased() < $1.name.lowercased() }
        } catch {
            statusMessage = "Failed to load cron jobs: \(error.localizedDescription)"
        }
    }

    func createJob(payload: [String: Any]) async -> CronJob? {
        do {
            let job = try await client.createCronJob(payload: payload)
            await refresh()
            return job
        } catch {
            statusMessage = "Create failed: \(error.localizedDescription)"
            return nil
        }
    }

    func updateJob(id: String, patch: [String: Any]) async -> CronJob? {
        do {
            let job = try await client.updateCronJob(id: id, patch: patch)
            await refresh()
            return job
        } catch {
            statusMessage = "Update failed: \(error.localizedDescription)"
            return nil
        }
    }
}

struct CronFormState {
    var name: String = ""
    var enabled: Bool = true
    var scheduleKind: String = "cron"
    var cronExpr: String = ""
    var timezone: String = "UTC"
    var everyMinutes: Int = 60
    var atDate: Date = Date().addingTimeInterval(3600)
    var sessionTarget: String = "isolated"
    var message: String = ""
}

struct CronView: View {
    @EnvironmentObject var runtime: RuntimeManager
    @StateObject private var viewModel = CronViewModel()

    @State private var selectedJobId: String?
    @State private var form = CronFormState()

    private let scheduleKinds = ["cron", "every", "at"]
    private let sessionTargets = ["isolated", "main"]

    var body: some View {
        HStack(spacing: 0) {
            sidebar
            Divider()
            editor
        }
        .onAppear {
            handleDaemonStateChange(running: runtime.daemonRunning)
        }
        .onChange(of: runtime.daemonRunning) { _, running in
            handleDaemonStateChange(running: running)
        }
    }

    private var sidebar: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack {
                Text("Schedule")
                    .font(.headline)
                Spacer()
                Button("Refresh") {
                    Task {
                        await viewModel.refresh()
                    }
                }
            }

            List(selection: $selectedJobId) {
                ForEach(viewModel.jobs) { job in
                    VStack(alignment: .leading, spacing: 4) {
                        Text(job.name)
                            .font(.body)
                        Text(jobSummary(job))
                            .font(.caption)
                            .foregroundStyle(.secondary)
                    }
                    .tag(job.id)
                    .onTapGesture {
                        loadForm(job)
                    }
                }
            }

            Button("New Job") {
                selectedJobId = nil
                form = CronFormState()
            }

            if !runtime.daemonRunning {
                Text("Daemon not running.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .padding()
        .frame(minWidth: 260, idealWidth: 300, maxWidth: 320)
    }

    private var editor: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text(selectedJobId == nil ? "Create Job" : "Edit Job")
                .font(.headline)

            Form {
                TextField("Name", text: $form.name)
                Toggle("Enabled", isOn: $form.enabled)

                Picker("Session Target", selection: $form.sessionTarget) {
                    ForEach(sessionTargets, id: \.self) { target in
                        Text(target).tag(target)
                    }
                }

                TextEditor(text: $form.message)
                    .frame(minHeight: 100)
                    .overlay(RoundedRectangle(cornerRadius: 6).stroke(Color.secondary.opacity(0.2)))

                Picker("Schedule", selection: $form.scheduleKind) {
                    ForEach(scheduleKinds, id: \.self) { kind in
                        Text(kind).tag(kind)
                    }
                }

                scheduleFields
            }

            HStack {
                Button(selectedJobId == nil ? "Create" : "Update") {
                    Task {
                        await saveJob()
                    }
                }
                .disabled(form.message.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty)

                Spacer()
                if !viewModel.statusMessage.isEmpty {
                    Text(viewModel.statusMessage)
                        .font(.caption)
                        .foregroundStyle(.secondary)
                }
            }
        }
        .padding()
    }

    @ViewBuilder
    private var scheduleFields: some View {
        switch form.scheduleKind {
        case "every":
            Stepper(value: $form.everyMinutes, in: 1...1440, step: 5) {
                Text("Every \(form.everyMinutes) minutes")
            }
        case "at":
            DatePicker("Run At", selection: $form.atDate)
        default:
            TextField("Cron expression", text: $form.cronExpr)
            TextField("Timezone", text: $form.timezone)
        }
    }

    private func loadForm(_ job: CronJob) {
        selectedJobId = job.id
        form.name = job.name
        form.enabled = job.enabled
        form.scheduleKind = job.scheduleKind
        form.cronExpr = job.cronExpr
        form.timezone = job.timezone
        form.everyMinutes = Int((job.everyMs ?? 3_600_000) / 60_000)
        form.atDate = Date(timeIntervalSince1970: TimeInterval(job.atMs ?? Int64(Date().timeIntervalSince1970 * 1000)) / 1000.0)
        form.sessionTarget = job.sessionTarget
        form.message = job.message
    }

    private func saveJob() async {
        let payloadKind = form.sessionTarget == "main" ? "systemEvent" : "agentTurn"
        var schedule: [String: Any] = ["kind": form.scheduleKind]
        switch form.scheduleKind {
        case "every":
            schedule["everyMs"] = Int64(form.everyMinutes * 60 * 1000)
        case "at":
            schedule["atMs"] = Int64(form.atDate.timeIntervalSince1970 * 1000)
        default:
            schedule["cron"] = form.cronExpr
            schedule["timezone"] = form.timezone
        }

        let jobPayload: [String: Any] = [
            "kind": payloadKind,
            "message": form.message
        ]

        var body: [String: Any] = [
            "name": form.name.isEmpty ? "Untitled Job" : form.name,
            "enabled": form.enabled,
            "sessionTarget": form.sessionTarget,
            "schedule": schedule,
            "payload": jobPayload
        ]

        if let jobId = selectedJobId {
            _ = await viewModel.updateJob(id: jobId, patch: body)
        } else {
            _ = await viewModel.createJob(payload: body)
        }
    }

    private func handleDaemonStateChange(running: Bool) {
        if running {
            viewModel.statusMessage = ""
            Task {
                await viewModel.refresh()
            }
        } else {
            viewModel.statusMessage = "Daemon not running."
        }
    }

    private func jobSummary(_ job: CronJob) -> String {
        switch job.scheduleKind {
        case "every":
            let minutes = (job.everyMs ?? 0) / 60_000
            return "Every \(minutes) min • \(job.sessionTarget)"
        case "at":
            if let atMs = job.atMs {
                return "At \(formatMs(atMs)) • \(job.sessionTarget)"
            }
            return "At (unset) • \(job.sessionTarget)"
        default:
            return "\(job.cronExpr.isEmpty ? "cron" : job.cronExpr) • \(job.sessionTarget)"
        }
    }

    private func formatMs(_ ms: Int64) -> String {
        let date = Date(timeIntervalSince1970: TimeInterval(ms) / 1000.0)
        return date.formatted(date: .abbreviated, time: .shortened)
    }
}
