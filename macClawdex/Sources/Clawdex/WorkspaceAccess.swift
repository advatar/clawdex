import Foundation
import AppKit

enum WorkspaceAccessError: Error {
    case userCancelled
    case bookmarkCreationFailed
    case bookmarkResolutionFailed
}

enum WorkspaceAccess {
    /// Prompts user to pick a folder. Stores an app-scoped security-scoped bookmark in UserDefaults.
    @MainActor
    static func pickFolderAndPersistBookmark() async -> Result<URL, Error> {
        let panel = NSOpenPanel()
        panel.title = "Choose workspace folder"
        panel.canChooseFiles = false
        panel.canChooseDirectories = true
        panel.allowsMultipleSelection = false

        let response = panel.runModal()
        guard response == .OK, let url = panel.url else {
            return .failure(WorkspaceAccessError.userCancelled)
        }

        do {
            let bookmark = try url.bookmarkData(
                options: [.withSecurityScope],
                includingResourceValuesForKeys: nil,
                relativeTo: nil
            )
            UserDefaults.standard.set(bookmark, forKey: DefaultsKeys.workspaceBookmark)
            return .success(url)
        } catch {
            return .failure(error)
        }
    }

    /// Resolves the workspace bookmark, starts accessing it, and returns the URL.
    /// Call `stopAccessing(_:)` when you're done.
    static func resolveWorkspaceURL() -> URL? {
        guard let bookmark = UserDefaults.standard.data(forKey: DefaultsKeys.workspaceBookmark) else {
            return nil
        }

        var stale = false
        do {
            let url = try URL(
                resolvingBookmarkData: bookmark,
                options: [.withSecurityScope],
                relativeTo: nil,
                bookmarkDataIsStale: &stale
            )
            if stale {
                // Caller may repersist bookmark later; keep going for now.
                NSLog("Workspace bookmark is stale; consider repersisting.")
            }
            guard url.startAccessingSecurityScopedResource() else {
                NSLog("Failed to start accessing security scoped resource")
                return nil
            }
            return url
        } catch {
            NSLog("Bookmark resolve error: \(error)")
            return nil
        }
    }

    static func stopAccessing(_ url: URL) {
        url.stopAccessingSecurityScopedResource()
    }

    static func clearWorkspaceBookmark() {
        UserDefaults.standard.removeObject(forKey: DefaultsKeys.workspaceBookmark)
    }
}
