import SwiftUI
import AppKit
import UniformTypeIdentifiers

struct GatewayRoute: Identifiable, Hashable {
    let id: String // sessionKey
    let channel: String
    let to: String
    let accountId: String?
    let updatedAtMs: Int64
}

@MainActor
final class GatewayViewModel: ObservableObject {
    @Published var routes: [GatewayRoute] = []
    @Published var receipts: [GatewayReceipt] = []
    @Published var attachments: [GatewayAttachment] = []
    @Published var statusMessage: String = ""

    @Published var token: String = UserDefaults.standard.string(forKey: DefaultsKeys.gatewayToken) ?? ""

    private let baseURL = URL(string: "http://127.0.0.1:18789")!

    func saveToken() {
        let trimmed = token.trimmingCharacters(in: .whitespacesAndNewlines)
        UserDefaults.standard.set(trimmed, forKey: DefaultsKeys.gatewayToken)
        token = trimmed
    }

    func refreshRoutes() async {
        do {
            let url = try routesFileURL()
            guard FileManager.default.fileExists(atPath: url.path) else {
                routes = []
                statusMessage = "No routes yet."
                return
            }
            let data = try Data(contentsOf: url)
            let parsed = try parseRoutes(data: data)
            routes = parsed.sorted { $0.updatedAtMs > $1.updatedAtMs }
            statusMessage = ""
        } catch {
            statusMessage = "Failed to load routes: \(error.localizedDescription)"
        }
    }

    func refreshReceipts(limit: Int = 200) async {
        do {
            let client = GatewayClient(baseURL: baseURL, token: token)
            let list = try await client.listReceipts(limit: limit)
            receipts = list.sorted { $0.tsMs > $1.tsMs }
            statusMessage = ""
        } catch {
            statusMessage = "Failed to load receipts: \(error.localizedDescription)"
        }
    }

    func refreshAttachments(limit: Int = 200) async {
        do {
            let client = GatewayClient(baseURL: baseURL, token: token)
            let list = try await client.listAttachments(limit: limit)
            attachments = list.sorted { ($0.createdAtMs ?? 0) > ($1.createdAtMs ?? 0) }
            statusMessage = ""
        } catch {
            statusMessage = "Failed to load attachments: \(error.localizedDescription)"
        }
    }

    func uploadFile(url: URL) async {
        saveToken()
        let path = url.path
        let fileName = url.lastPathComponent
        let mimeType = mimeTypeForFile(url)
        let token = self.token
        let base = baseURL

        do {
            let data = try await Task.detached(priority: .utility) {
                try Data(contentsOf: URL(fileURLWithPath: path))
            }.value
            let client = GatewayClient(baseURL: base, token: token)
            _ = try await client.uploadAttachment(data: data, fileName: fileName, mimeType: mimeType)
            statusMessage = "Uploaded \(fileName)."
            await refreshAttachments()
        } catch {
            statusMessage = "Upload failed: \(error.localizedDescription)"
        }
    }

    func downloadAndOpen(_ attachment: GatewayAttachment) async {
        saveToken()
        let token = self.token
        let base = baseURL
        do {
            let client = GatewayClient(baseURL: base, token: token)
            let data = try await client.downloadAttachmentData(id: attachment.id)
            let dir = FileManager.default.temporaryDirectory.appendingPathComponent("clawdex-attachments", isDirectory: true)
            try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)

            let preferredName = attachment.fileName?.trimmingCharacters(in: .whitespacesAndNewlines)
            let fileName = (preferredName?.isEmpty ?? true) ? attachment.id : preferredName!
            let dst = dir.appendingPathComponent(fileName)
            try data.write(to: dst, options: [.atomic])

            NSWorkspace.shared.open(dst)
            statusMessage = ""
        } catch {
            statusMessage = "Download failed: \(error.localizedDescription)"
        }
    }

    private func routesFileURL() throws -> URL {
        let state = try stateDirURL()
        return state.appendingPathComponent("gateway/routes.json")
    }

    private func stateDirURL() throws -> URL {
        let fm = FileManager.default
        guard let base = fm.urls(for: .applicationSupportDirectory, in: .userDomainMask).first else {
            throw NSError(domain: "Clawdex", code: 1, userInfo: [NSLocalizedDescriptionKey: "No Application Support directory"])
        }
        let bid = Bundle.main.bundleIdentifier ?? "Clawdex"
        let dir = base.appendingPathComponent(bid, isDirectory: true).appendingPathComponent("state", isDirectory: true)
        return dir
    }

    private func parseRoutes(data: Data) throws -> [GatewayRoute] {
        let rootAny = try JSONSerialization.jsonObject(with: data)
        guard let root = rootAny as? [String: Any] else { return [] }
        guard let routesAny = root["routes"] as? [String: Any] else { return [] }
        var out: [GatewayRoute] = []
        for (sessionKey, entryAny) in routesAny {
            guard let entry = entryAny as? [String: Any] else { continue }
            let channel = entry["channel"] as? String ?? ""
            let to = entry["to"] as? String ?? ""
            let accountId = (entry["accountId"] as? String) ?? (entry["account_id"] as? String)
            let updatedAtMs = int64FromAny(entry["updatedAtMs"] ?? entry["updated_at_ms"]) ?? 0
            out.append(GatewayRoute(id: sessionKey, channel: channel, to: to, accountId: accountId, updatedAtMs: updatedAtMs))
        }
        return out
    }

    private func mimeTypeForFile(_ url: URL) -> String? {
        if let type = try? url.resourceValues(forKeys: [.contentTypeKey]).contentType {
            if let mime = type.preferredMIMEType {
                return mime
            }
        }
        if !url.pathExtension.isEmpty {
            return UTType(filenameExtension: url.pathExtension)?.preferredMIMEType
        }
        return nil
    }

    private func int64FromAny(_ any: Any?) -> Int64? {
        if let value = any as? Int64 { return value }
        if let value = any as? Int { return Int64(value) }
        if let value = any as? NSNumber { return value.int64Value }
        if let value = any as? String { return Int64(value.trimmingCharacters(in: .whitespacesAndNewlines)) }
        return nil
    }
}

struct GatewayView: View {
    @EnvironmentObject var runtime: RuntimeManager
    @StateObject private var viewModel = GatewayViewModel()

    @State private var section: GatewaySection = .routes

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            header
            picker
            Divider()
            content

            if !viewModel.statusMessage.isEmpty {
                Text(viewModel.statusMessage)
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }
        }
        .padding()
        .onAppear {
            runtime.ensureGatewayRunning()
            Task {
                await refreshCurrentSection()
            }
        }
        .onChange(of: section) { _, _ in
            Task {
                await refreshCurrentSection()
            }
        }
    }

    private var header: some View {
        HStack(alignment: .center, spacing: 12) {
            VStack(alignment: .leading, spacing: 2) {
                Text("Gateway")
                    .font(.headline)
                Text(runtime.gatewayRunning ? "Running" : "Stopped")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            }

            if !runtime.gatewayRunning {
                Button("Start") {
                    runtime.ensureGatewayRunning()
                }
            }

            Spacer()

            SecureField("Token (optional)", text: $viewModel.token)
                .textFieldStyle(.roundedBorder)
                .frame(width: 260)
                .onSubmit {
                    viewModel.saveToken()
                }

            Button("Save") {
                viewModel.saveToken()
            }
        }
    }

    private var picker: some View {
        Picker("Section", selection: $section) {
            Text("Sessions").tag(GatewaySection.routes)
            Text("Receipts").tag(GatewaySection.receipts)
            Text("Attachments").tag(GatewaySection.attachments)
        }
        .pickerStyle(.segmented)
        .frame(maxWidth: 540)
    }

    @ViewBuilder
    private var content: some View {
        switch section {
        case .routes:
            routesView
        case .receipts:
            receiptsView
        case .attachments:
            attachmentsView
        }
    }

    private var routesView: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                Text("Sessions (Routes)")
                    .font(.headline)
                Spacer()
                Button("Refresh") {
                    Task { await viewModel.refreshRoutes() }
                }
            }

            if viewModel.routes.isEmpty {
                Text("No routes yet. Send/receive a message through the gateway to create a session route.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else {
                List(viewModel.routes) { route in
                    HStack(alignment: .top) {
                        VStack(alignment: .leading, spacing: 4) {
                            Text(route.id)
                                .font(.subheadline)
                                .textSelection(.enabled)
                            Text("\(route.channel) → \(route.to)")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                            if let accountId = route.accountId, !accountId.isEmpty {
                                Text("accountId: \(accountId)")
                                    .font(.caption)
                                    .foregroundStyle(.secondary)
                            }
                        }
                        Spacer()
                        VStack(alignment: .trailing, spacing: 6) {
                            Text(formatMs(route.updatedAtMs))
                                .font(.caption)
                                .foregroundStyle(.secondary)
                            Button("Copy") {
                                NSPasteboard.general.clearContents()
                                NSPasteboard.general.setString(route.id, forType: .string)
                            }
                        }
                    }
                }
                .frame(minHeight: 360)
            }
        }
    }

    private var receiptsView: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                Text("Receipts")
                    .font(.headline)
                Spacer()
                Button("Refresh") {
                    Task { await viewModel.refreshReceipts() }
                }
            }

            if viewModel.receipts.isEmpty {
                Text("No receipts recorded yet.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else {
                List(viewModel.receipts) { receipt in
                    VStack(alignment: .leading, spacing: 6) {
                        HStack {
                            Text(receipt.status)
                                .font(.subheadline)
                            Text(receipt.direction)
                                .font(.caption)
                                .foregroundStyle(.secondary)
                            Spacer()
                            Text(formatMs(receipt.tsMs))
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        }

                        if let sessionKey = receipt.sessionKey, !sessionKey.isEmpty {
                            Text("session: \(sessionKey)")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        }

                        let channel = receipt.channel ?? ""
                        let to = receipt.to ?? ""
                        let from = receipt.from ?? ""
                        if !channel.isEmpty || !to.isEmpty || !from.isEmpty {
                            Text("channel: \(channel)  to: \(to)  from: \(from)")
                                .font(.caption)
                                .foregroundStyle(.secondary)
                        }

                        if let err = receipt.error, !err.isEmpty {
                            Text(err)
                                .font(.caption)
                                .foregroundStyle(.red)
                        }
                    }
                    .padding(.vertical, 4)
                }
                .frame(minHeight: 360)
            }
        }
    }

    private var attachmentsView: some View {
        VStack(alignment: .leading, spacing: 10) {
            HStack {
                Text("Attachments")
                    .font(.headline)
                Spacer()
                Button("Upload…") {
                    chooseAttachmentFile()
                }
                Button("Refresh") {
                    Task { await viewModel.refreshAttachments() }
                }
            }

            if viewModel.attachments.isEmpty {
                Text("No attachments uploaded yet.")
                    .font(.caption)
                    .foregroundStyle(.secondary)
            } else {
                List(viewModel.attachments) { att in
                    HStack(alignment: .top) {
                        VStack(alignment: .leading, spacing: 4) {
                            Text(att.fileName ?? att.id)
                                .font(.subheadline)
                                .textSelection(.enabled)
                            Text(att.id)
                                .font(.caption)
                                .foregroundStyle(.secondary)
                                .textSelection(.enabled)
                            if let mime = att.mimeType, !mime.isEmpty {
                                Text(mime)
                                    .font(.caption)
                                    .foregroundStyle(.secondary)
                            }
                            if let size = att.sizeBytes, size > 0 {
                                Text(formatBytes(size))
                                    .font(.caption)
                                    .foregroundStyle(.secondary)
                            }
                        }
                        Spacer()
                        VStack(alignment: .trailing, spacing: 6) {
                            if let created = att.createdAtMs {
                                Text(formatMs(created))
                                    .font(.caption)
                                    .foregroundStyle(.secondary)
                            }
                            Button("Open") {
                                Task { await viewModel.downloadAndOpen(att) }
                            }
                        }
                    }
                    .padding(.vertical, 4)
                }
                .frame(minHeight: 360)
            }
        }
    }

    private func chooseAttachmentFile() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.allowsMultipleSelection = false
        panel.prompt = "Upload"
        panel.message = "Choose a file to upload as a gateway attachment."
        if panel.runModal() == .OK, let url = panel.url {
            Task { await viewModel.uploadFile(url: url) }
        }
    }

    private func refreshCurrentSection() async {
        switch section {
        case .routes:
            await viewModel.refreshRoutes()
        case .receipts:
            await viewModel.refreshReceipts()
        case .attachments:
            await viewModel.refreshAttachments()
        }
    }

    private func formatMs(_ ms: Int64) -> String {
        let d = Date(timeIntervalSince1970: TimeInterval(ms) / 1000.0)
        return d.formatted(date: .numeric, time: .standard)
    }

    private func formatBytes(_ bytes: Int64) -> String {
        let formatter = ByteCountFormatter()
        formatter.allowedUnits = [.useAll]
        formatter.countStyle = .file
        return formatter.string(fromByteCount: bytes)
    }
}

private enum GatewaySection: String, CaseIterable {
    case routes
    case receipts
    case attachments
}

