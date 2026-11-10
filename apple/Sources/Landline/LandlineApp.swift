import AppKit
import SwiftUI

@main
struct LandlineApp: App {
    @State private var model: AppModel

    init() {
        let mode = LaunchMode(arguments: CommandLine.arguments)
        if mode != .live, let appearance = DemoAppearance(arguments: CommandLine.arguments) {
            NSApplication.shared.appearance = appearance.nsAppearance
        }
        let model = AppModel(mode: mode)
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

/// `--appearance=light` or `--appearance=dark` in demo mode, to look at the
/// app in either without changing the whole system's appearance.
enum DemoAppearance: String {
    case light, dark

    init?(arguments: [String]) {
        guard let argument = arguments.first(where: { $0.hasPrefix("--appearance=") }) else { return nil }
        self.init(rawValue: String(argument.dropFirst("--appearance=".count)))
    }

    var nsAppearance: NSAppearance? {
        NSAppearance(named: self == .dark ? .darkAqua : .aqua)
    }
}
