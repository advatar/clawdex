import Foundation
import ServiceManagement

enum LaunchAtLoginController {
    static func isEnabled() -> Bool {
        return SMAppService.mainApp.status == .enabled
    }

    static func setEnabled(_ enabled: Bool) {
        do {
            if enabled {
                try SMAppService.mainApp.register()
            } else {
                try SMAppService.mainApp.unregister()
            }
        } catch {
            // In a production app, log this and/or surface to the user.
            NSLog("LaunchAtLoginController error: \(error)")
        }
    }
}
