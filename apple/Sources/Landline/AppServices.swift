import Foundation
import LandlineKit

/// What the model uses from the OS, gathered so tests and demo mode can
/// swap in versions that touch nothing real.
@MainActor
struct AppServices {
    let settings: AppSettings
    let notificationCenter: any NotificationCenterClient
    let pasteboard: any Pasteboard
    let loginItem: any LoginItem

    static func live() -> AppServices {
        AppServices(
            settings: AppSettings(defaults: .standard),
            notificationCenter: SystemNotificationCenter.isAvailable
                ? SystemNotificationCenter() : InertNotificationCenter(permission: .denied),
            pasteboard: SystemPasteboard(),
            loginItem: SystemLoginItem())
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
            loginItem: real ? SystemLoginItem() : MemoryLoginItem())
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

/// Text or a link the phone shared.
struct SharedItem: Equatable, Sendable {
    enum Kind: Equatable, Sendable { case text, url }

    let deviceId: String
    let kind: Kind
    let value: String
}
