import Foundation

struct PeerHelpPublishResult: Sendable {
    let eventID: String
    let topic: String
    let repliesTopic: String
    let relayURL: URL
}

enum AntennaPeerAssistError: LocalizedError {
    case invalidRelayURL
    case invalidCategory
    case emptyQuestion
    case relayRejected(status: Int, body: String)
    case antennaUnavailable

    var errorDescription: String? {
        switch self {
        case .invalidRelayURL:
            return "Peer relay URL must use http or https."
        case .invalidCategory:
            return "Peer category (ENS) is required."
        case .emptyQuestion:
            return "Peer assist question is empty."
        case .relayRejected(let status, let body):
            if body.isEmpty {
                return "Peer relay rejected request (HTTP \(status))."
            }
            return "Peer relay rejected request (HTTP \(status)): \(body)"
        case .antennaUnavailable:
            return "AntennaProtocol is unavailable in this build."
        }
    }
}

#if canImport(AntennaProtocol)
import AntennaProtocol

enum AntennaPeerAssist {
    static func publishHelpRequest(
        question: String,
        relayURL: URL,
        bootstrapRelays: [URL],
        categoryENS: String,
        anonKey: String,
        sourceLabel: String,
        capabilities: [String]
    ) async throws -> PeerHelpPublishResult {
        guard let scheme = relayURL.scheme?.lowercased(), scheme == "http" || scheme == "https" else {
            throw AntennaPeerAssistError.invalidRelayURL
        }

        let trimmedQuestion = question.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmedQuestion.isEmpty else {
            throw AntennaPeerAssistError.emptyQuestion
        }

        let trimmedCategory = categoryENS.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmedCategory.isEmpty else {
            throw AntennaPeerAssistError.invalidCategory
        }

        let trimmedAnonKey = anonKey.trimmingCharacters(in: .whitespacesAndNewlines)
        let author = MBAuthor(
            type: "anon",
            agentRegistry: nil,
            agentId: nil,
            ens: nil,
            anonKey: trimmedAnonKey.isEmpty ? nil : trimmedAnonKey
        )

        var event = MBEvent(
            kind: "helpRequest",
            category: trimmedCategory,
            thread: nil,
            parents: [],
            author: author,
            createdAt: iso8601Now(),
            parts: [MBPart(kind: "text", text: trimmedQuestion)],
            extensions: ["clawdex.peer_assist.v1"],
            metadata: nil,
            auth: nil
        )

        let eventID = try event.computeEventId()
        event.id = eventID

        let repliesTopic = MBTopics.helpRepliesTopic(eventID)
        event.metadata = buildMetadata(
            repliesTopic: repliesTopic,
            sourceLabel: sourceLabel,
            capabilities: capabilities
        )

        let topic = MBTopics.helpTopic(trimmedCategory)
        let envelope = MBEnvelope(topic: topic, event: event)
        let publishOutcome = try await MBRelayClient.publishEnvelope(
            envelope,
            primaryRelay: relayURL,
            bootstrapRelays: bootstrapRelays,
            discover: true
        )

        return PeerHelpPublishResult(
            eventID: eventID,
            topic: topic,
            repliesTopic: repliesTopic,
            relayURL: publishOutcome.relayURL
        )
    }

    private static func buildMetadata(
        repliesTopic: String,
        sourceLabel: String,
        capabilities: [String]
    ) -> CodableValue {
        let caps = capabilities
            .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
            .filter { !$0.isEmpty }
            .map(CodableValue.string)

        return .object([
            "replyTopic": .string(repliesTopic),
            "source": .string(sourceLabel),
            "requestedAt": .string(iso8601Now()),
            "capabilities": .array(caps)
        ])
    }

    private static func iso8601Now() -> String {
        let formatter = ISO8601DateFormatter()
        formatter.formatOptions = [.withInternetDateTime, .withFractionalSeconds]
        return formatter.string(from: Date())
    }
}
#else
enum AntennaPeerAssist {
    static func publishHelpRequest(
        question _: String,
        relayURL _: URL,
        bootstrapRelays _: [URL],
        categoryENS _: String,
        anonKey _: String,
        sourceLabel _: String,
        capabilities _: [String]
    ) async throws -> PeerHelpPublishResult {
        throw AntennaPeerAssistError.antennaUnavailable
    }
}
#endif
