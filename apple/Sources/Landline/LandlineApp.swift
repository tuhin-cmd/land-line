import AppKit
import SwiftUI

@main
struct LandlineApp: App {
    @State private var model: AppModel

    init() {
        let model = AppModel(mode: LaunchMode(arguments: CommandLine.arguments))
        // Started here rather than when the panel first opens: the phone
        // should be able to find the Mac as soon as the app is running.
        model.start()
        NotificationCenter.default.addObserver(
            forName: NSApplication.willTerminateNotification, object: nil, queue: .main
        ) { _ in
            // Lets the phone see the link close instead of timing out.
            MainActor.assumeIsolated { model.stop() }
        }
        _model = State(initialValue: model)
    }

    var body: some Scene {
        MenuBarExtra {
            PopoverView()
                .environment(model)
        } label: {
            Image(systemName: model.menuBarSymbol)
                .accessibilityLabel("Landline")
        }
        .menuBarExtraStyle(.window)

        Settings {
            SettingsView()
                .environment(model)
        }
    }
}

