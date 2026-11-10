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
    let workspace = FakeWorkspace()
    let folder = TemporaryFolder()
    lazy var settings = AppSettings(defaults: defaults.defaults)

    var services: AppServices {
        AppServices(
            settings: settings, notificationCenter: center, pasteboard: pasteboard, loginItem: loginItem,
            workspace: workspace, defaultDownloadDir: folder.url.path)
    }
}

/// Records what would have been opened or shown in Finder.
@MainActor
final class FakeWorkspace: Workspace {
    private(set) var opened: [URL] = []
    private(set) var revealed: [[URL]] = []

    func open(_ url: URL) { opened.append(url) }
    func reveal(_ urls: [URL]) { revealed.append(urls) }
}

/// A folder of its own under the temporary directory, deleted with the
/// test, so no test writes to the user's folders.
final class TemporaryFolder: @unchecked Sendable {
    let url: URL

    init() {
        url = FileManager.default.temporaryDirectory
            .appendingPathComponent("landline-tests-\(UUID().uuidString)", isDirectory: true)
            .resolvingSymlinksInPath()
        try? FileManager.default.createDirectory(at: url, withIntermediateDirectories: true)
    }

    deinit {
        try? FileManager.default.removeItem(at: url)
    }

    @discardableResult
    func file(_ name: String, contents: String = "x") -> URL {
        let file = url.appendingPathComponent(name)
        try? FileManager.default.createDirectory(at: file.deletingLastPathComponent(), withIntermediateDirectories: true)
        FileManager.default.createFile(atPath: file.path, contents: Data(contents.utf8))
        return file
    }

    @discardableResult
    func folder(_ name: String) -> URL {
        let folder = url.appendingPathComponent(name, isDirectory: true)
        try? FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        return folder
    }
}

func transfer(
    _ id: UInt64, device: String = "p", direction: TransferDirection = .incoming, done: UInt64 = 0,
    total: UInt64? = 100, _ state: TransferState = .inProgress
) -> Transfer {
    transfer(id, "a.jpg", device: device, direction: direction, done: done, total: total, state)
}

func transfer(
    _ id: UInt64, _ name: String, device: String = "p", direction: TransferDirection = .incoming,
    done: UInt64 = 0, total: UInt64? = 100, _ state: TransferState = .inProgress
) -> Transfer {
    Transfer(id: id, deviceId: device, fileName: name, direction: direction, bytesDone: done, bytesTotal: total, state: state)
}

func entry(_ path: String, dir: Bool = false, size: UInt64 = 10, modified: Int64? = nil) -> RemoteEntry {
    RemoteEntry(
        name: (path as NSString).lastPathComponent, path: path, isDir: dir, size: size, modifiedSecs: modified)
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
