import Foundation

struct ChatMessage: Identifiable, Codable, Equatable {
    enum Role: String, Codable {
        case user
        case assistant
        case system
    }

    let id: UUID
    let role: Role
    let text: String
    let date: Date

    init(role: Role, text: String, date: Date = Date()) {
        self.id = UUID()
        self.role = role
        self.text = text
        self.date = date
    }
}
