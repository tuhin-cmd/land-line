import Foundation
import Observation

/// The user's choices, kept in `UserDefaults`. Each property writes through
/// as soon as it changes, so a crash never loses a setting.
@MainActor @Observable
final class AppSettings {
    /// Mirror the phone's notifications as Mac banners.
    var showNotifications: Bool {
        didSet { defaults.set(showNotifications, forKey: Key.showNotifications) }
    }
    /// Put a one-time code on the pasteboard as soon as it arrives.
    var copyCodesAutomatically: Bool {
        didSet { defaults.set(copyCodesAutomatically, forKey: Key.copyCodes) }
    }
    /// Send Mac copies to the phone and accept the phone's clipboard.
    var syncClipboard: Bool {
        didSet { defaults.set(syncClipboard, forKey: Key.syncClipboard) }
    }
    /// Phone apps whose notifications are never shown, sorted for display.
    private(set) var mutedApps: [String] {
        didSet { defaults.set(mutedApps, forKey: Key.mutedApps) }
    }
    /// Where received files go; `nil` leaves it to the core, which uses
    /// Downloads.
    var downloadDir: String? {
        didSet { defaults.set(downloadDir, forKey: Key.downloadDir) }
    }

    @ObservationIgnored private let defaults: UserDefaults

    init(defaults: UserDefaults) {
        self.defaults = defaults
        // Defaults for anything not yet chosen. `register(defaults:)` is not
        // used: its domain is process-wide, so every suite would share it.
        showNotifications = defaults.object(forKey: Key.showNotifications) as? Bool ?? true
        copyCodesAutomatically = defaults.object(forKey: Key.copyCodes) as? Bool ?? true
        syncClipboard = defaults.object(forKey: Key.syncClipboard) as? Bool ?? true
        mutedApps = (defaults.stringArray(forKey: Key.mutedApps) ?? []).sorted(by: Self.finderOrder)
        downloadDir = defaults.string(forKey: Key.downloadDir)
    }

    func isMuted(_ appName: String) -> Bool {
        mutedApps.contains(appName)
    }

    func mute(_ appName: String) {
        guard !appName.isEmpty, !isMuted(appName) else { return }
        mutedApps = (mutedApps + [appName]).sorted(by: Self.finderOrder)
    }

    func unmute(_ appName: String) {
        mutedApps.removeAll { $0 == appName }
    }

    private static func finderOrder(_ lhs: String, _ rhs: String) -> Bool {
        lhs.localizedStandardCompare(rhs) == .orderedAscending
    }

    private enum Key {
        static let showNotifications = "showNotifications"
        static let copyCodes = "copyCodesAutomatically"
        static let syncClipboard = "syncClipboard"
        static let mutedApps = "mutedApps"
        static let downloadDir = "downloadDir"
    }
}
