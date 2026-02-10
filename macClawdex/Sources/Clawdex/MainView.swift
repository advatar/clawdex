import SwiftUI

struct MainView: View {
    var body: some View {
        TabView {
            ChatView()
                .tabItem {
                    Label("Chat", systemImage: "message")
                }
            TasksView()
                .tabItem {
                    Label("Tasks", systemImage: "checklist")
                }
            ApprovalsView()
                .tabItem {
                    Label("Approvals", systemImage: "hand.raised")
                }
            CronView()
                .tabItem {
                    Label("Schedule", systemImage: "calendar")
                }
            GatewayView()
                .tabItem {
                    Label("Gateway", systemImage: "antenna.radiowaves.left.and.right")
                }
            PluginsView()
                .tabItem {
                    Label("Plugins", systemImage: "puzzlepiece.extension")
                }
        }
    }
}
