import Foundation
import LandlineKit

@testable import Landline

/// A `UserDefaults` suite of its own, so no test reads or writes the real
/// app's settings. A suite named by an absolute path is stored at that path,
/// which keeps it in a temporary folder deleted with the test, rather than
/// leaving a file per test in ~/Library/Preferences.
final class TestDefaults {
    private let directory = FileManager.default.temporaryDirectory
        .appendingPathComponent("landline-tests-\(UUID().uuidString)", isDirectory: true)
    let defaults: UserDefaults

    init() {
        try? FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        defaults = UserDefaults(suiteName: directory.appendingPathComponent("settings").path)!
    }

    deinit {
        try? FileManager.default.removeItem(at: directory)
    }
}

/// Records banners instead of showing them.
@MainActor
final class FakeNotificationCenter: NotificationCenterClient {
    var onResponse: (@MainActor (BannerResponse) -> Void)?
    var currentPermission: NotificationPermission = .allowed
    /// What the user answers when asked.
    var answer: NotificationPermission = .allowed
    private(set) var requests = 0
    private(set) var shown: [Banner] = []
    private(set) var removed: [String] = []
    private(set) var removedAll = 0
    /// Banners shown and not removed since, by ID; a later one replaces.
    private(set) var onScreen: [String: Banner] = [:]

    func permission() async -> NotificationPermission { currentPermission }

    func requestPermission() async -> NotificationPermission {
        requests += 1
        if currentPermission == .notAsked { currentPermission = answer }
        return currentPermission
    }

    func show(_ banner: Banner) {
        shown.append(banner)
        onScreen[banner.id] = banner
    }

    func remove(ids: [String]) {
        removed += ids
        for id in ids { onScreen[id] = nil }
    }

    func removeAll() {
        removedAll += 1
        onScreen = [:]
    }

    /// Plays the user acting on a banner, through the model's handler.
    func respond(_ action: BannerResponse.Action, to banner: Banner) {
        onResponse?(BannerResponse(action: action, userInfo: banner.userInfo))
    }
}

/// Fakes for everything the model takes from the OS.
@MainActor
final class TestServices {
    let defaults = TestDefaults()
    let center = FakeNotificationCenter()
    let pasteboard = MemoryPasteboard()
    let loginItem = MemoryLoginItem()
    lazy var settings = AppSettings(defaults: defaults.defaults)

    var services: AppServices {
        AppServices(settings: settings, notificationCenter: center, pasteboard: pasteboard, loginItem: loginItem)
    }
}

func phoneNotification(
    _ id: String = "n1",
    app: String = "WhatsApp",
    title: String = "Maya",
    text: String = "Lunch?",
    time: Int64 = 1_000,
    dismissable: Bool = true,
    silent: Bool = false,
    icon: String? = nil
) -> PhoneNotification {
    PhoneNotification(
        id: id, appName: app, title: title, text: text, timeMs: time, dismissable: dismissable, silent: silent,
        iconPath: icon)
}

/// Waits for work the model started in a task of its own.
@MainActor
func eventually(_ condition: () -> Bool) async throws {
    for _ in 0..<100 where !condition() {
        try await Task.sleep(for: .milliseconds(10))
    }
}
