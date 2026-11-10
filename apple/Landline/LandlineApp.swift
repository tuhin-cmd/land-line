import LandlineKit
import SwiftUI

@main
struct LandlineApp: App {
    var body: some Scene {
        MenuBarExtra("Landline", systemImage: "phone.connection") {
            Text("Landline core \(coreVersion())")
            Divider()
            Button("Quit Landline") {
                NSApplication.shared.terminate(nil)
            }
            .keyboardShortcut("q")
        }
    }
}
