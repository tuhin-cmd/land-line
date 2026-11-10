import Foundation
import LandlineKit

/// A Mac notification the app wants shown. Plain values, so the rules that
/// build banners can be tested where UserNotifications cannot run.
struct Banner: Equatable, Sendable {
    /// Showing a banner with an ID already on screen replaces it.
    var id: String
    var title: String
    var subtitle: String = ""
    var body: String
    /// Banners with the same thread are grouped in Notification Center.
    var threadId: String
    var category: BannerCategory
    var userInfo: [String: String] = [:]
    /// A PNG to attach, such as the phone app's icon.
    var iconPath: String?
}

/// Decides which actions a banner offers.
enum BannerCategory: Hashable, Sendable {
    /// A phone notification. Offers "Mute <app>", and reports dismissal so
    /// the notification can be removed on the phone too.
    case phoneNotification(appName: String)
    /// A one-time code. Offers "Copy Code".
    case code

    var identifier: String {
        switch self {
        case .phoneNotification(let appName): "phone/\(appName)"
        case .code: "code"
        }
    }
}

/// What the user did with a banner.
struct BannerResponse: Equatable, Sendable {
    enum Action: Equatable, Sendable {
        /// Clicked the banner itself.
        case opened
        /// Closed it, or cleared it from Notification Center.
        case dismissed
        case muteApp
        case copyCode
    }

    let action: Action
    let userInfo: [String: String]
}

enum NotificationPermission: Equatable, Sendable {
    case notAsked, allowed, denied
}

/// The parts of the system notification center the app uses.
/// `UNUserNotificationCenter.current()` crashes in a process without a
/// bundle, so only the app itself creates the real one.
@MainActor
protocol NotificationCenterClient: AnyObject {
    /// Called on the main actor for each banner action.
    var onResponse: (@MainActor (BannerResponse) -> Void)? { get set }
    func permission() async -> NotificationPermission
    /// Shows the system prompt if the user has not been asked yet.
    func requestPermission() async -> NotificationPermission
    func show(_ banner: Banner)
    /// Takes banners off screen and out of Notification Center.
    func remove(ids: [String])
    /// Every banner this app has shown, including in earlier runs.
    func removeAll()
}

/// Banners that are never shown: for demo mode and for a process without a
/// bundle. The panel still lists phone notifications.
@MainActor
final class InertNotificationCenter: NotificationCenterClient {
    var onResponse: (@MainActor (BannerResponse) -> Void)?
    private let fixedPermission: NotificationPermission

    init(permission: NotificationPermission) {
        fixedPermission = permission
    }

    func permission() async -> NotificationPermission { fixedPermission }
    func requestPermission() async -> NotificationPermission { fixedPermission }
    func show(_ banner: Banner) {}
    func remove(ids: [String]) {}
    func removeAll() {}
}

/// Keys in `Banner.userInfo`.
enum BannerInfo {
    static let deviceId = "device"
    static let notificationId = "notification"
    static let appName = "app"
    static let dismissable = "dismissable"
    static let code = "code"
}

extension Banner {
    /// The banner for a phone notification. Arranged the way Android shows
    /// it: the app first, then the notification's own title, then its text.
    /// The Mac shows Landline's icon above every banner, so without the app
    /// name in the title nothing would say which phone app it came from.
    static func phone(_ notification: PhoneNotification, deviceId: String) -> Banner {
        let appName = notification.appName.trimmingCharacters(in: .whitespacesAndNewlines)
        let title = notification.title.trimmingCharacters(in: .whitespacesAndNewlines)
        let text = notification.text.trimmingCharacters(in: .whitespacesAndNewlines)
        // Many apps repeat their name as the title; once is enough.
        let subtitle = title.caseInsensitiveCompare(appName) == .orderedSame ? "" : title
        return Banner(
            id: phoneId(deviceId: deviceId, notificationId: notification.id),
            title: appName.isEmpty ? "Phone" : appName,
            subtitle: text.isEmpty ? "" : subtitle,
            // A banner with an empty body shows only its title; move the
            // notification title down so it is still read.
            body: text.isEmpty ? subtitle : text,
            threadId: "phone/\(deviceId)/\(appName)",
            category: .phoneNotification(appName: appName),
            userInfo: [
                BannerInfo.deviceId: deviceId,
                BannerInfo.notificationId: notification.id,
                BannerInfo.appName: notification.appName,
                BannerInfo.dismissable: notification.dismissable ? "1" : "0",
            ],
            iconPath: notification.iconPath)
    }

    /// Derived from the device and the phone's ID, so a later version of the
    /// same notification replaces the banner instead of stacking under it.
    static func phoneId(deviceId: String, notificationId: String) -> String {
        "phone/\(deviceId)/\(notificationId)"
    }

    /// The banner for a one-time code. Its ID carries a serial number, not
    /// the code, because the system logs notification IDs.
    static func code(_ otp: OtpCode, deviceId: String, copied: Bool, serial: Int) -> Banner {
        let source = otp.source.trimmingCharacters(in: .whitespacesAndNewlines)
        return Banner(
            id: "code/\(deviceId)/\(serial)",
            title: source.isEmpty ? "One-time code" : "Code from \(source)",
            body: copied ? "\(otp.code) · Copied" : otp.code,
            threadId: "codes",
            category: .code,
            userInfo: [BannerInfo.deviceId: deviceId, BannerInfo.code: otp.code])
    }
}

/// Finds a one-time code inside notification text, to tell whether a phone
/// notification is the same message the code was read from.
enum CodeMatch {
    /// True if `code` appears in `text` as a whole number: not inside a
    /// longer one, and allowing the "123 456" or "123-456" split that
    /// messages use. Digits from other scripts (such as Bangla) count.
    static func text(_ text: String, contains code: String) -> Bool {
        guard !code.isEmpty else { return false }
        return numbers(in: text).contains(code)
    }

    /// Every run of digits, both as written and with single spaces or
    /// hyphens between digit groups closed up.
    static func numbers(in text: String) -> Set<String> {
        var found: Set<String> = []
        var joined = ""
        var current = ""
        var pendingSeparator = false
        for character in text {
            if let digit = character.wholeNumberValue, character.isNumber, (0...9).contains(digit) {
                if pendingSeparator, !current.isEmpty {
                    found.insert(current)
                    current = ""
                }
                pendingSeparator = false
                current.append(String(digit))
                joined.append(String(digit))
            } else if (character == " " || character == "-" || character == "\u{00A0}"),
                !current.isEmpty, !pendingSeparator
            {
                pendingSeparator = true
            } else {
                if !current.isEmpty { found.insert(current) }
                if !joined.isEmpty { found.insert(joined) }
                current = ""
                joined = ""
                pendingSeparator = false
            }
        }
        if !current.isEmpty { found.insert(current) }
        if !joined.isEmpty { found.insert(joined) }
        return found
    }
}
