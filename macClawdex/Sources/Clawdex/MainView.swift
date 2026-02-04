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
        }
    }
}
