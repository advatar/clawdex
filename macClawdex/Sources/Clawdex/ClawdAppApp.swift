import SwiftUI

@main
struct ClawdexApp: App {
    @StateObject private var appState = AppState()
    @StateObject private var runtime = RuntimeManager()

    var body: some Scene {
        WindowGroup("Clawdex") {
            MainView()
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
        MenuBarExtra("Clawdex", systemImage: "bolt.horizontal.circle") {
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

            Button("Open Clawdex") {
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
