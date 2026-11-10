import AppKit
import LandlineKit

/// What the model uses from the OS, gathered so tests and demo mode can
/// swap in versions that touch nothing real.
@MainActor
struct AppServices {
    let settings: AppSettings
    let notificationCenter: any NotificationCenterClient
    let pasteboard: any Pasteboard
    let loginItem: any LoginItem
    let workspace: any Workspace
    /// Where received and downloaded files go unless the user picked a
    /// folder: Downloads, or a temporary folder in demo mode and tests.
    let defaultDownloadDir: String

    static func live() -> AppServices {
        AppServices(
            settings: AppSettings(defaults: .standard),
            notificationCenter: SystemNotificationCenter.isAvailable
                ? SystemNotificationCenter() : InertNotificationCenter(permission: .denied),
            pasteboard: SystemPasteboard(),
            loginItem: SystemLoginItem(),
            workspace: SystemWorkspace(),
            defaultDownloadDir: FileManager.default.urls(for: .downloadsDirectory, in: .userDomainMask).first?.path
                ?? NSHomeDirectory() + "/Downloads")
    }

    /// Demo settings live in their own suite so trying things in a demo
    /// (muting an app, turning sync off) never changes the real app's.
    /// Only the `system` scenario reaches the real notification center,
    /// pasteboard and login items.
    static func demo(_ scenario: DemoScenario) -> AppServices {
        let defaults = UserDefaults(suiteName: "io.github.tuhin-cmd.landline.demo") ?? .standard
        let real = scenario == .system && SystemNotificationCenter.isAvailable
        return AppServices(
            settings: AppSettings(defaults: defaults),
            notificationCenter: real ? SystemNotificationCenter() : InertNotificationCenter(permission: .allowed),
            pasteboard: real ? SystemPasteboard() : MemoryPasteboard(),
            loginItem: real ? SystemLoginItem() : MemoryLoginItem(),
            workspace: DemoWorkspace(opensLinks: real),
            defaultDownloadDir: DemoFiles.receivedFolder.path)
    }
}

/// Opening and revealing files and links, so tests and demo mode can see
/// what would have opened without anything opening.
@MainActor
protocol Workspace: AnyObject {
    /// Opens a file in its default app, or a link in the browser or mail.
    func open(_ url: URL)
    /// Selects the files in a Finder window.
    func reveal(_ urls: [URL])
}

@MainActor
final class SystemWorkspace: Workspace {
    func open(_ url: URL) {
        NSWorkspace.shared.open(url)
    }

    func reveal(_ urls: [URL]) {
        guard !urls.isEmpty else { return }
        NSWorkspace.shared.activateFileViewerSelecting(urls)
    }
}

/// Demo files live in a temporary folder, so revealing and opening them is
/// harmless and shows the feature working. Web links are only recorded,
/// except in the `system` scenario.
@MainActor
final class DemoWorkspace: Workspace {
    private let opensLinks: Bool
    private(set) var openedLinks: [URL] = []

    init(opensLinks: Bool) {
        self.opensLinks = opensLinks
    }

    func open(_ url: URL) {
        if url.isFileURL || opensLinks {
            NSWorkspace.shared.open(url)
        } else {
            openedLinks.append(url)
            Log.app.info("Demo: would open a shared link")
        }
    }

    func reveal(_ urls: [URL]) {
        guard !urls.isEmpty else { return }
        NSWorkspace.shared.activateFileViewerSelecting(urls)
    }
}

/// A one-time code as the panel shows it.
struct ReceivedCode: Equatable, Sendable {
    let code: String
    let source: String
    let deviceId: String
    let date: Date
    var copied: Bool

    var isExpired: Bool { Date.now.timeIntervalSince(date) > AppModel.codeLifetime }

    /// Whether the phone notification carries this code, which makes it the
    /// message the code was read from, or another copy of it.
    func appears(in notification: PhoneNotification) -> Bool {
        CodeMatch.text(notification.title + "\n" + notification.text, contains: code)
    }
}

/// A phone notification in the panel's list.
struct ListedNotification: Identifiable, Equatable {
    let deviceId: String
    let deviceName: String
    let notification: PhoneNotification

    var id: String { Banner.phoneId(deviceId: deviceId, notificationId: notification.id) }
}

