import SwiftUI

@main
struct ClawdAppApp: App {
    @StateObject private var appState = AppState()
    @StateObject private var runtime = RuntimeManager()

    var body: some Scene {
        WindowGroup("Clawd") {
            ChatView()
                .environmentObject(appState)
                .environmentObject(runtime)
                .onAppear {
                    runtime.bootstrap(appState: appState)
                }
        }
        Settings {
            SettingsView()
                .environmentObject(appState)
                .environmentObject(runtime)
        }
        // Optional: menu bar presence so the scheduler can keep running while windows are closed.
        MenuBarExtra("Clawd", systemImage: "bolt.horizontal.circle") {
            MenuBarView()
                .environmentObject(appState)
                .environmentObject(runtime)
        }
    }
}

struct MenuBarView: View {
    @EnvironmentObject var appState: AppState
    @EnvironmentObject var runtime: RuntimeManager

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            if runtime.isRunning {
                Text("Agent: Running")
            } else {
                Text("Agent: Stopped")
            }

            Button(runtime.isRunning ? "Stop Agent" : "Start Agent") {
                if runtime.isRunning {
                    runtime.stop()
                } else {
                    runtime.start()
                }
            }

            Divider()

            Button("Open Clawd") {
                NSApp.activate(ignoringOtherApps: true)
                for window in NSApp.windows {
                    window.makeKeyAndOrderFront(nil)
                }
            }

            Button("Quit") {
                runtime.stop()
                NSApp.terminate(nil)
            }
        }
        .padding(12)
        .frame(width: 220)
    }
}
