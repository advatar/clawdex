import Foundation

struct TaskSummary: Identifiable, Hashable {
    let id: String
    let title: String
    let createdAtMs: Int64
    let lastRunAtMs: Int64?
}

struct TaskRunInfo: Hashable {
    let id: String
    let taskId: String
    let status: String
}

struct TaskEvent: Identifiable, Hashable {
    let id: String
    let tsMs: Int64
    let kind: String
    let payload: String
}

struct PendingApproval: Identifiable, Hashable {
    let id: String
    let runId: String
    let kind: String
    let createdAtMs: Int64
    let requestJson: String
}

struct UserInputOption: Hashable {
    let label: String
    let description: String
}

struct UserInputQuestion: Identifiable, Hashable {
    let id: String
    let header: String
    let prompt: String
    let options: [UserInputOption]
    let isOther: Bool
    let isSecret: Bool
}

struct PendingUserInput: Identifiable, Hashable {
    let id: String
    let runId: String
    let createdAtMs: Int64
    let questions: [UserInputQuestion]
}

@MainActor
final class DaemonClient {
    private let baseURL: URL

    init(baseURL: URL) {
        self.baseURL = baseURL
    }

    func fetchTasks() async throws -> [TaskSummary] {
        let url = baseURL.appendingPathComponent("/v1/tasks")
        let (data, _) = try await URLSession.shared.data(from: url)
        let obj = try parseObject(data: data)
        let items = obj["tasks"] as? [[String: Any]] ?? []
        return items.compactMap { item -> TaskSummary? in
            guard let id = item["id"] as? String,
                  let title = item["title"] as? String else { return nil }
            let createdAtMs = int64Value(item["created_at_ms"]) ?? 0
            let lastRunAtMs = int64Value(item["last_run_at_ms"])
            return TaskSummary(
                id: id,
                title: title,
                createdAtMs: createdAtMs,
                lastRunAtMs: lastRunAtMs
            )
        }
    }

    func createTask(title: String) async throws -> TaskSummary {
        var request = URLRequest(url: baseURL.appendingPathComponent("/v1/tasks"))
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        let payload: [String: Any] = ["title": title]
        request.httpBody = try JSONSerialization.data(withJSONObject: payload)

        let (data, _) = try await URLSession.shared.data(for: request)
        let obj = try parseObject(data: data)
        let task = obj["task"] as? [String: Any]
        guard let id = task?["id"] as? String,
              let title = task?["title"] as? String else {
            throw NSError(domain: "Clawdex", code: 3, userInfo: [NSLocalizedDescriptionKey: "Invalid task response"])
        }
        let createdAtMs = int64Value(task?["created_at_ms"]) ?? 0
        let lastRunAtMs = int64Value(task?["last_run_at_ms"])
        return TaskSummary(
            id: id,
            title: title,
            createdAtMs: createdAtMs,
            lastRunAtMs: lastRunAtMs
        )
    }

    func startRun(taskId: String?, title: String?, prompt: String) async throws -> TaskRunInfo {
        var request = URLRequest(url: baseURL.appendingPathComponent("/v1/runs"))
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        var payload: [String: Any] = [
            "prompt": prompt,
            "autoApprove": true
        ]
        if let taskId {
            payload["taskId"] = taskId
        }
        if let title {
            payload["title"] = title
        }
        request.httpBody = try JSONSerialization.data(withJSONObject: payload)

        let (data, _) = try await URLSession.shared.data(for: request)
        let obj = try parseObject(data: data)
        let run = obj["run"] as? [String: Any]
        guard let id = run?["id"] as? String,
              let taskId = run?["task_id"] as? String,
              let status = run?["status"] as? String else {
            throw NSError(domain: "Clawdex", code: 5, userInfo: [NSLocalizedDescriptionKey: "Invalid run response"])
        }
        return TaskRunInfo(id: id, taskId: taskId, status: status)
    }

    func fetchEvents(runId: String, after: Int64, waitMs: Int64) async throws -> [TaskEvent] {
        var urlComponents = URLComponents(url: baseURL.appendingPathComponent("/v1/runs/\(runId)/events"), resolvingAgainstBaseURL: false)
        urlComponents?.queryItems = [
            URLQueryItem(name: "after", value: "\(after)"),
            URLQueryItem(name: "wait", value: "\(waitMs)")
        ]
        guard let url = urlComponents?.url else {
            throw NSError(domain: "Clawdex", code: 6, userInfo: [NSLocalizedDescriptionKey: "Invalid URL"])
        }
        var request = URLRequest(url: url)
        request.timeoutInterval = TimeInterval(max(1, waitMs / 1000))

        let (data, _) = try await URLSession.shared.data(for: request)
        let obj = try parseObject(data: data)
        let items = obj["events"] as? [[String: Any]] ?? []
        return items.compactMap { item -> TaskEvent? in
            guard let id = item["id"] as? String,
                  let kind = item["kind"] as? String else { return nil }
            let tsMs = int64Value(item["ts_ms"]) ?? 0
            let payloadObj = item["payload"]
            let payloadData = try? JSONSerialization.data(withJSONObject: payloadObj ?? [:], options: [])
            let payloadString = payloadData.flatMap { String(data: $0, encoding: .utf8) } ?? ""
            return TaskEvent(id: id, tsMs: tsMs, kind: kind, payload: payloadString)
        }
    }

    func fetchApprovals() async throws -> (approvals: [PendingApproval], inputs: [PendingUserInput]) {
        let url = baseURL.appendingPathComponent("/v1/approvals")
        let (data, _) = try await URLSession.shared.data(from: url)
        let obj = try parseObject(data: data)

        let approvalsArray = obj["approvals"] as? [[String: Any]] ?? []
        let approvals = approvalsArray.compactMap { item -> PendingApproval? in
            guard let id = item["id"] as? String,
                  let runId = item["run_id"] as? String,
                  let kind = item["kind"] as? String else { return nil }
            let createdAtMs = int64Value(item["created_at_ms"]) ?? 0
            let requestObj = item["request"] ?? [:]
            let requestData = try? JSONSerialization.data(withJSONObject: requestObj, options: [.prettyPrinted])
            let requestJson = requestData.flatMap { String(data: $0, encoding: .utf8) } ?? ""
            return PendingApproval(
                id: id,
                runId: runId,
                kind: kind,
                createdAtMs: createdAtMs,
                requestJson: requestJson
            )
        }

        let inputsArray = obj["userInputs"] as? [[String: Any]] ?? []
        let inputs = inputsArray.compactMap { item -> PendingUserInput? in
            guard let id = item["id"] as? String,
                  let runId = item["run_id"] as? String else { return nil }
            let createdAtMs = int64Value(item["created_at_ms"]) ?? 0
            let params = item["params"] as? [String: Any]
            let questionItems = params?["questions"] as? [[String: Any]] ?? []
            let questions = questionItems.compactMap { question -> UserInputQuestion? in
                guard let qid = question["id"] as? String,
                      let header = question["header"] as? String,
                      let prompt = question["question"] as? String else { return nil }
                let isOther = question["isOther"] as? Bool ?? false
                let isSecret = question["isSecret"] as? Bool ?? false
                let optionItems = question["options"] as? [[String: Any]] ?? []
                let options = optionItems.compactMap { option -> UserInputOption? in
                    guard let label = option["label"] as? String,
                          let description = option["description"] as? String else { return nil }
                    return UserInputOption(label: label, description: description)
                }
                return UserInputQuestion(
                    id: qid,
                    header: header,
                    prompt: prompt,
                    options: options,
                    isOther: isOther,
                    isSecret: isSecret
                )
            }
            return PendingUserInput(id: id, runId: runId, createdAtMs: createdAtMs, questions: questions)
        }

        return (approvals: approvals, inputs: inputs)
    }

    func resolveApproval(id: String, decision: String) async throws -> Bool {
        var request = URLRequest(url: baseURL.appendingPathComponent("/v1/approvals/\(id)"))
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        request.httpBody = try JSONSerialization.data(withJSONObject: ["decision": decision])
        let (data, _) = try await URLSession.shared.data(for: request)
        let obj = try parseObject(data: data)
        return obj["ok"] as? Bool ?? false
    }

    func submitUserInput(id: String, answers: [String: [String]]) async throws -> Bool {
        var request = URLRequest(url: baseURL.appendingPathComponent("/v1/user-input/\(id)"))
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        var payloadAnswers: [String: Any] = [:]
        for (key, values) in answers {
            payloadAnswers[key] = ["answers": values]
        }
        request.httpBody = try JSONSerialization.data(withJSONObject: ["answers": payloadAnswers])
        let (data, _) = try await URLSession.shared.data(for: request)
        let obj = try parseObject(data: data)
        return obj["ok"] as? Bool ?? false
    }
}

private func parseObject(data: Data) throws -> [String: Any] {
    let obj = try JSONSerialization.jsonObject(with: data) as? [String: Any]
    return obj ?? [:]
}

private func int64Value(_ any: Any?) -> Int64? {
    if let value = any as? Int64 {
        return value
    }
    if let value = any as? Int {
        return Int64(value)
    }
    if let value = any as? NSNumber {
        return value.int64Value
    }
    return nil
}
