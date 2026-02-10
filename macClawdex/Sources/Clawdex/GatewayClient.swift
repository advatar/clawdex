import Foundation

struct GatewayAttachment: Identifiable, Hashable {
    let id: String
    let fileName: String?
    let mimeType: String?
    let sizeBytes: Int64?
    let sha256: String?
    let createdAtMs: Int64?
}

struct GatewayReceipt: Identifiable, Hashable {
    let id: String
    let tsMs: Int64
    let status: String
    let direction: String
    let sessionKey: String?
    let channel: String?
    let to: String?
    let from: String?
    let error: String?
}

struct GatewaySession: Identifiable, Hashable {
    let id: String // sessionKey
    let channel: String
    let to: String
    let accountId: String?
    let updatedAtMs: Int64
}

struct GatewayClient: Sendable {
    let baseURL: URL
    let token: String?

    init(baseURL: URL, token: String? = nil) {
        self.baseURL = baseURL
        let trimmed = token?.trimmingCharacters(in: .whitespacesAndNewlines)
        self.token = (trimmed?.isEmpty ?? true) ? nil : trimmed
    }

    func listAttachments(after: Int64? = nil, limit: Int? = nil) async throws -> [GatewayAttachment] {
        var components = URLComponents(url: baseURL.appendingPathComponent("/v1/attachments"), resolvingAgainstBaseURL: false)
        var items: [URLQueryItem] = []
        if let after { items.append(URLQueryItem(name: "after", value: "\(after)")) }
        if let limit { items.append(URLQueryItem(name: "limit", value: "\(limit)")) }
        if !items.isEmpty { components?.queryItems = items }
        guard let url = components?.url else { return [] }

        var request = URLRequest(url: url)
        applyAuth(&request)
        let (data, _) = try await URLSession.shared.data(for: request)
        let obj = try parseObject(data: data)
        let entries = obj["attachments"] as? [[String: Any]] ?? []
        return entries.compactMap(parseAttachment(entry:))
    }

    func uploadAttachment(data: Data, fileName: String?, mimeType: String?) async throws -> GatewayAttachment? {
        var request = URLRequest(url: baseURL.appendingPathComponent("/v1/attachments"))
        request.httpMethod = "POST"
        request.setValue("application/json", forHTTPHeaderField: "Content-Type")
        applyAuth(&request)

        var payload: [String: Any] = [
            "content": data.base64EncodedString()
        ]
        if let fileName, !fileName.isEmpty {
            payload["fileName"] = fileName
        }
        if let mimeType, !mimeType.isEmpty {
            payload["mimeType"] = mimeType
        }

        request.httpBody = try JSONSerialization.data(withJSONObject: payload)
        let (respData, _) = try await URLSession.shared.data(for: request)
        let obj = try parseObject(data: respData)
        let entries = obj["attachments"] as? [[String: Any]] ?? []
        return entries.compactMap(parseAttachment(entry:)).first
    }

    func downloadAttachmentData(id: String) async throws -> Data {
        let url = baseURL.appendingPathComponent("/v1/attachments/\(id)/data")
        var request = URLRequest(url: url)
        request.httpMethod = "GET"
        applyAuth(&request)
        let (data, _) = try await URLSession.shared.data(for: request)
        return data
    }

    func listReceipts(after: Int64? = nil, limit: Int? = nil) async throws -> [GatewayReceipt] {
        var components = URLComponents(url: baseURL.appendingPathComponent("/v1/receipts"), resolvingAgainstBaseURL: false)
        var items: [URLQueryItem] = []
        if let after { items.append(URLQueryItem(name: "after", value: "\(after)")) }
        if let limit { items.append(URLQueryItem(name: "limit", value: "\(limit)")) }
        if !items.isEmpty { components?.queryItems = items }
        guard let url = components?.url else { return [] }

        var request = URLRequest(url: url)
        applyAuth(&request)
        let (data, _) = try await URLSession.shared.data(for: request)
        let obj = try parseObject(data: data)
        let entries = obj["receipts"] as? [[String: Any]] ?? []
        return entries.compactMap(parseReceipt(entry:))
    }

    private func applyAuth(_ request: inout URLRequest) {
        guard let token else { return }
        request.setValue("Bearer \(token)", forHTTPHeaderField: "Authorization")
        request.setValue(token, forHTTPHeaderField: "X-Clawdex-Token")
    }
}

private func parseObject(data: Data) throws -> [String: Any] {
    let obj = try JSONSerialization.jsonObject(with: data) as? [String: Any]
    return obj ?? [:]
}

private func int64FromAny(_ any: Any?) -> Int64? {
    if let value = any as? Int64 { return value }
    if let value = any as? Int { return Int64(value) }
    if let value = any as? NSNumber { return value.int64Value }
    if let value = any as? String { return Int64(value.trimmingCharacters(in: .whitespacesAndNewlines)) }
    return nil
}

private func parseAttachment(entry: [String: Any]) -> GatewayAttachment? {
    guard let id = entry["id"] as? String else { return nil }
    return GatewayAttachment(
        id: id,
        fileName: entry["fileName"] as? String,
        mimeType: entry["mimeType"] as? String,
        sizeBytes: int64FromAny(entry["sizeBytes"]),
        sha256: entry["sha256"] as? String,
        createdAtMs: int64FromAny(entry["createdAtMs"])
    )
}

private func parseReceipt(entry: [String: Any]) -> GatewayReceipt? {
    guard let id = entry["id"] as? String else { return nil }
    let tsMs = int64FromAny(entry["tsMs"]) ?? 0
    let status = entry["status"] as? String ?? "unknown"
    let direction = entry["direction"] as? String ?? "unknown"
    return GatewayReceipt(
        id: id,
        tsMs: tsMs,
        status: status,
        direction: direction,
        sessionKey: entry["sessionKey"] as? String,
        channel: entry["channel"] as? String,
        to: entry["to"] as? String,
        from: entry["from"] as? String,
        error: entry["error"] as? String
    )
}
