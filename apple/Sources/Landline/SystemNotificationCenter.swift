import Foundation
import os
import UniformTypeIdentifiers
import UserNotifications

/// Banners through `UNUserNotificationCenter`. Create only inside the app
/// bundle; see `NotificationCenterClient`.
@MainActor
final class SystemNotificationCenter: NSObject, NotificationCenterClient {
    var onResponse: (@MainActor (BannerResponse) -> Void)?

    private let center = UNUserNotificationCenter.current()
    private var categories: [String: UNNotificationCategory] = [:]
    /// The previous operation. Each one waits for it, so a removal never
    /// overtakes the banner it removes while that is still being added.
    private var tail: Task<Void, Never>?
    private let iconDirectory = FileManager.default.temporaryDirectory
        .appendingPathComponent("io.github.tuhin-cmd.landline.icons", isDirectory: true)

    /// True when this process is an app bundle, where the real center works.
    static var isAvailable: Bool {
        Bundle.main.bundleURL.pathExtension == "app" && Bundle.main.bundleIdentifier != nil
    }

    override init() {
        super.init()
        center.delegate = self
        let copy = UNNotificationAction(identifier: ActionId.copyCode, title: "Copy Code")
        let reveal = UNNotificationAction(identifier: ActionId.showInFinder, title: "Show in Finder")
        let openFile = UNNotificationAction(identifier: ActionId.openFile, title: "Open")
        let openLink = UNNotificationAction(identifier: ActionId.openLink, title: "Open")
        let fixed: [(BannerCategory, [UNNotificationAction])] = [
            (.code, [copy]),
            (.receivedFile, [reveal, openFile]),
            (.receivedFiles, [reveal]),
            (.link, [openLink]),
            (.info, []),
        ]
        for (category, actions) in fixed {
            categories[category.identifier] = UNNotificationCategory(
                identifier: category.identifier, actions: actions, intentIdentifiers: [])
        }
        center.setNotificationCategories(Set(categories.values))
    }

    func permission() async -> NotificationPermission {
        Self.permission(await center.notificationSettings().authorizationStatus)
    }

    func requestPermission() async -> NotificationPermission {
        do {
            _ = try await center.requestAuthorization(options: [.alert, .sound])
        } catch {
            Log.notifications.error("Notification permission request failed: \(error.localizedDescription, privacy: .public)")
        }
        return await permission()
    }

    func show(_ banner: Banner) {
        enqueue { [weak self] in await self?.add(banner) }
    }

    func remove(ids: [String]) {
        guard !ids.isEmpty else { return }
        enqueue { [center] in
            center.removeDeliveredNotifications(withIdentifiers: ids)
            center.removePendingNotificationRequests(withIdentifiers: ids)
        }
    }

    func removeAll() {
        enqueue { [center] in
            center.removeAllDeliveredNotifications()
            center.removeAllPendingNotificationRequests()
        }
    }

    private func enqueue(_ operation: @escaping @MainActor () async -> Void) {
        let previous = tail
        tail = Task {
            await previous?.value
            await operation()
        }
    }

    private func add(_ banner: Banner) async {
        await registerCategory(for: banner.category)

        let content = UNMutableNotificationContent()
        content.title = banner.title
        content.subtitle = banner.subtitle
        content.body = banner.body
        content.threadIdentifier = banner.threadId
        content.categoryIdentifier = banner.category.identifier
        content.userInfo = banner.userInfo
        content.sound = banner.quiet ? nil : .default
        if let iconPath = banner.iconPath, let attachment = attachment(for: iconPath) {
            content.attachments = [attachment]
        }

        let request = UNNotificationRequest(identifier: banner.id, content: content, trigger: nil)
        do {
            try await center.add(request)
        } catch {
            // Usually means notifications are off for the app.
            Log.notifications.info("Banner not shown: \(error.localizedDescription, privacy: .public)")
        }
    }

    /// Action titles are fixed per category, so each phone app gets its own
    /// category to be able to say "Mute WhatsApp" rather than "Mute App".
    private func registerCategory(for category: BannerCategory) async {
        guard case .phoneNotification(let appName) = category, categories[category.identifier] == nil else {
            return
        }
        let title = appName.isEmpty ? "Mute This App" : "Mute \(appName)"
        let mute = UNNotificationAction(identifier: ActionId.muteApp, title: title)
        categories[category.identifier] = UNNotificationCategory(
            identifier: category.identifier, actions: [mute], intentIdentifiers: [],
            // Without this the app is not told when a banner is closed, and
            // could not clear the notification on the phone.
            options: [.customDismissAction])
        center.setNotificationCategories(Set(categories.values))
        // Setting categories returns before the daemon has them; reading them
        // back waits for that, so the banner added next gets its actions.
        _ = await center.notificationCategories()
    }

    /// The system moves an attachment's file into its own store, so it gets a
    /// copy rather than the core's icon cache entry.
    private func attachment(for iconPath: String) -> UNNotificationAttachment? {
        do {
            let copy = try IconStaging.copy(iconPath, into: iconDirectory)
            return try UNNotificationAttachment(
                identifier: "icon", url: copy,
                options: [UNNotificationAttachmentOptionsTypeHintKey: UTType.png.identifier])
        } catch {
            Log.notifications.info("Icon not attached: \(error.localizedDescription, privacy: .public)")
            return nil
        }
    }

    nonisolated static func permission(_ status: UNAuthorizationStatus) -> NotificationPermission {
        switch status {
        case .notDetermined: .notAsked
        case .denied: .denied
        case .authorized, .provisional, .ephemeral: .allowed
        @unknown default: .allowed
        }
    }

    fileprivate enum ActionId {
        static let copyCode = "copy-code"
        static let muteApp = "mute-app"
        static let showInFinder = "show-in-finder"
        static let openFile = "open-file"
        static let openLink = "open-link"
    }
}

extension SystemNotificationCenter: UNUserNotificationCenterDelegate {
    /// A menu bar app counts as frontmost while its panel is open; without
    /// this, banners arriving then would go straight to Notification Center.
    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter, willPresent notification: UNNotification
    ) async -> UNNotificationPresentationOptions {
        [.banner, .list, .sound]
    }

    nonisolated func userNotificationCenter(
        _ center: UNUserNotificationCenter, didReceive response: UNNotificationResponse
    ) async {
        let action: BannerResponse.Action? = switch response.actionIdentifier {
        case UNNotificationDefaultActionIdentifier: .opened
        case UNNotificationDismissActionIdentifier: .dismissed
        case ActionId.copyCode: .copyCode
        case ActionId.muteApp: .muteApp
        case ActionId.showInFinder: .showInFinder
        case ActionId.openFile: .openFile
        case ActionId.openLink: .openLink
        default: nil
        }
        guard let action else { return }
        let userInfo = response.notification.request.content.userInfo as? [String: String] ?? [:]
        let banner = BannerResponse(action: action, userInfo: userInfo)
        await MainActor.run { self.onResponse?(banner) }
    }
}

enum IconStaging {
    /// Copies the file to a new, uniquely named PNG in `directory`.
    static func copy(_ path: String, into directory: URL) throws -> URL {
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        let destination = directory.appendingPathComponent(UUID().uuidString).appendingPathExtension("png")
        try FileManager.default.copyItem(at: URL(fileURLWithPath: path), to: destination)
        return destination
    }
}
