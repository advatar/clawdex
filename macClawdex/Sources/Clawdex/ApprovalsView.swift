import SwiftUI

enum ApprovalDecision: String {
    case accept
    case decline
    case cancel
}

@MainActor
final class ApprovalsViewModel: ObservableObject {
    @Published var approvals: [PendingApproval] = []
    @Published var inputs: [PendingUserInput] = []
    @Published var statusMessage: String = ""

    private let client = DaemonClient(baseURL: URL(string: "http://127.0.0.1:18791")!)
    private var pollTask: Task<Void, Never>?

    func refresh() async {
        do {
            let result = try await client.fetchApprovals()
            approvals = result.approvals.sorted { $0.createdAtMs > $1.createdAtMs }
            inputs = result.inputs.sorted { $0.createdAtMs > $1.createdAtMs }
        } catch {
            statusMessage = "Failed to load approvals: \(error.localizedDescription)"
        }
    }

    func startPolling() {
        stopPolling()
        pollTask = Task { [weak self] in
            guard let self else { return }
            while !Task.isCancelled {
                await self.refresh()
                try? await Task.sleep(nanoseconds: 2_000_000_000)
            }
        }
    }

    func stopPolling() {
        pollTask?.cancel()
        pollTask = nil
    }

    func decide(id: String, decision: ApprovalDecision) async -> Bool {
        do {
            let ok = try await client.resolveApproval(id: id, decision: decision.rawValue)
            if ok {
                await refresh()
            }
            return ok
        } catch {
            statusMessage = "Approval failed: \(error.localizedDescription)"
            return false
        }
    }

    func submit(inputId: String, answers: [String: [String]]) async -> Bool {
        do {
            let ok = try await client.submitUserInput(id: inputId, answers: answers)
            if ok {
                await refresh()
            }
            return ok
        } catch {
            statusMessage = "Submit failed: \(error.localizedDescription)"
            return false
        }
    }
}

struct ApprovalsView: View {
    @EnvironmentObject var runtime: RuntimeManager
    @StateObject private var viewModel = ApprovalsViewModel()

    @State private var selections: [String: [String: String]] = [:]
    @State private var textInputs: [String: [String: String]] = [:]

    private let otherOptionKey = "__other__"

    var body: some View {
        VStack(alignment: .leading, spacing: 16) {
            header

            ScrollView {
                VStack(alignment: .leading, spacing: 20) {
                    approvalsSection
                    Divider()
                    userInputsSection
                }
                .padding(.vertical, 8)
            }

            if !viewModel.statusMessage.isEmpty {
                Text(viewModel.statusMessage)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            if !runtime.daemonRunning {
                Text("Daemon not running.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .padding()
        .onAppear {
            viewModel.startPolling()
        }
        .onDisappear {
            viewModel.stopPolling()
        }
    }

    private var header: some View {
        HStack {
            Text("Approvals")
                .font(.headline)
            Spacer()
            Button("Refresh") {
                Task {
                    await viewModel.refresh()
                }
            }
        }
    }

    private var approvalsSection: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("Pending Approvals")
                .font(.headline)

            if viewModel.approvals.isEmpty {
                Text("No approvals pending.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else {
                ForEach(viewModel.approvals) { approval in
                    VStack(alignment: .leading, spacing: 8) {
                        HStack {
                            Text(approval.kind.capitalized)
                                .font(.subheadline)
                            Spacer()
                            Text(formatMs(approval.createdAtMs))
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        }
                        Text("Run: \(approval.runId)")
                            .font(.caption)
                            .foregroundStyle(.secondary)

                        if !approval.requestJson.isEmpty {
                            Text(approval.requestJson)
                                .font(.system(.caption, design: .monospaced))
                                .textSelection(.enabled)
                                .padding(8)
                                .background(Color.gray.opacity(0.08))
                                .cornerRadius(6)
                        }

                        HStack {
                            Button("Approve") {
                                Task {
                                    _ = await viewModel.decide(id: approval.id, decision: .accept)
                                }
                            }
                            Button("Decline") {
                                Task {
                                    _ = await viewModel.decide(id: approval.id, decision: .decline)
                                }
                            }
                            Button("Cancel") {
                                Task {
                                    _ = await viewModel.decide(id: approval.id, decision: .cancel)
                                }
                            }
                            Spacer()
                        }
                    }
                    .padding(12)
                    .background(Color.gray.opacity(0.06))
                    .cornerRadius(8)
                }
            }
        }
    }

    private var userInputsSection: some View {
        VStack(alignment: .leading, spacing: 12) {
            Text("User Inputs")
                .font(.headline)

            if viewModel.inputs.isEmpty {
                Text("No pending input requests.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else {
                ForEach(viewModel.inputs) { input in
                    VStack(alignment: .leading, spacing: 10) {
                        HStack {
                            Text("Input Request")
                                .font(.subheadline)
                            Spacer()
                            Text(formatMs(input.createdAtMs))
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        }
                        Text("Run: \(input.runId)")
                            .font(.caption)
                            .foregroundStyle(.secondary)

                        ForEach(input.questions) { question in
                            VStack(alignment: .leading, spacing: 6) {
                                Text(question.header)
                                    .font(.subheadline)
                                Text(question.prompt)
                                    .font(.caption)
                                    .foregroundStyle(.secondary)

                                if question.options.isEmpty {
                                    textField(for: question, inputId: input.id)
                                } else {
                                    optionPicker(for: question, inputId: input.id)
                                }
                            }
                            .padding(.vertical, 4)
                        }

                        Button("Submit Answers") {
                            let answers = buildAnswers(for: input)
                            let answeredCount = answers.count
                            if answeredCount < input.questions.count {
                                viewModel.statusMessage = "Please answer all questions before submitting."
                                return
                            }
                            Task {
                                let ok = await viewModel.submit(inputId: input.id, answers: answers)
                                if ok {
                                    selections[input.id] = nil
                                    textInputs[input.id] = nil
                                }
                            }
                        }
                    }
                    .padding(12)
                    .background(Color.gray.opacity(0.06))
                    .cornerRadius(8)
                }
            }
        }
    }

    @ViewBuilder
    private func textField(for question: UserInputQuestion, inputId: String) -> some View {
        let binding = textBinding(inputId: inputId, questionId: question.id)
        if question.isSecret {
            SecureField("Answer", text: binding)
                .textFieldStyle(.roundedBorder)
        } else {
            TextField("Answer", text: binding)
                .textFieldStyle(.roundedBorder)
        }
    }

    @ViewBuilder
    private func optionPicker(for question: UserInputQuestion, inputId: String) -> some View {
        let defaultValue = question.options.first?.label ?? ""
        let selection = selectionBinding(inputId: inputId, questionId: question.id, defaultValue: defaultValue)

        Picker("Select", selection: selection) {
            ForEach(question.options, id: \.label) { option in
                Text(option.label).tag(option.label)
            }
            if question.isOther {
                Text("Other").tag(otherOptionKey)
            }
        }
        .pickerStyle(.menu)

        if selection.wrappedValue == otherOptionKey {
            textField(for: question, inputId: inputId)
        } else if let option = question.options.first(where: { $0.label == selection.wrappedValue }),
                  !option.description.isEmpty {
            Text(option.description)
                .font(.caption)
                .foregroundStyle(.secondary)
        }
    }

    private func selectionBinding(inputId: String, questionId: String, defaultValue: String) -> Binding<String> {
        Binding<String>(
            get: {
                selections[inputId]?[questionId] ?? defaultValue
            },
            set: { newValue in
                var inputSelections = selections[inputId] ?? [:]
                inputSelections[questionId] = newValue
                selections[inputId] = inputSelections
            }
        )
    }

    private func textBinding(inputId: String, questionId: String) -> Binding<String> {
        Binding<String>(
            get: {
                textInputs[inputId]?[questionId] ?? ""
            },
            set: { newValue in
                var inputs = textInputs[inputId] ?? [:]
                inputs[questionId] = newValue
                textInputs[inputId] = inputs
            }
        )
    }

    private func buildAnswers(for input: PendingUserInput) -> [String: [String]] {
        var answers: [String: [String]] = [:]
        for question in input.questions {
            let value: String
            if question.options.isEmpty {
                value = textInputs[input.id]?[question.id] ?? ""
            } else {
                let selection = selections[input.id]?[question.id] ?? question.options.first?.label ?? ""
                if selection == otherOptionKey {
                    value = textInputs[input.id]?[question.id] ?? ""
                } else {
                    value = selection
                }
            }
            let trimmed = value.trimmingCharacters(in: .whitespacesAndNewlines)
            if !trimmed.isEmpty {
                answers[question.id] = [trimmed]
            }
        }
        return answers
    }

    private func formatMs(_ ms: Int64) -> String {
        let date = Date(timeIntervalSince1970: TimeInterval(ms) / 1000.0)
        return date.formatted(date: .abbreviated, time: .shortened)
    }
}
