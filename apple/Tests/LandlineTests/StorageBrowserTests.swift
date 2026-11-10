import Foundation
import LandlineKit
import Testing

@testable import Landline

/// A phone's storage in memory, answering at once.
@MainActor
final class FakeStorage: StorageService {
    var roots = [StorageRoot(name: "Internal storage", path: "/storage/emulated/0")]
    var folders: [String: [RemoteEntry]] = [:]
    var failures: [String: any Error] = [:]
    var rootsFailure: (any Error)?
    private(set) var listed: [String] = []
    private(set) var downloads: [(remote: String, local: URL, origin: TransferOrigin)] = []
    private(set) var uploads: [(local: URL, remote: String)] = []
    /// Downloads wait for this before finishing, when set.
    var gate: AsyncStream<Void>?
    var downloadFailure: (any Error)?

    func storageRoots(of deviceId: String) async throws -> [StorageRoot] {
        if let rootsFailure { throw rootsFailure }
        return roots
    }

    func listDir(_ path: String, on deviceId: String) async throws -> [RemoteEntry] {
        listed.append(path)
        if let failure = failures[path] { throw failure }
        return folders[path] ?? []
    }

    func download(
        _ remote: String, to local: URL, from deviceId: String, origin: TransferOrigin,
        onTransfer: @escaping @MainActor (UInt64) -> Void
    ) async throws {
        downloads.append((remote, local, origin))
        onTransfer(UInt64(downloads.count))
        if let gate { for await _ in gate { break } }
        if let downloadFailure { throw downloadFailure }
        FileManager.default.createFile(atPath: local.path, contents: Data("x".utf8))
    }

    func upload(
        _ local: URL, to remote: String, on deviceId: String, onTransfer: @escaping @MainActor (UInt64) -> Void
    ) async throws {
        uploads.append((local, remote))
    }
}

@MainActor @Suite struct StorageBrowserTests {
    let storage = FakeStorage()
    let folder = TemporaryFolder()
    let root = "/storage/emulated/0"

    final class Box { var revealed: [[URL]] = []; var blocked = false }
    let box = Box()

    init() {
        storage.folders[root] = [
            entry(root + "/DCIM", dir: true), entry(root + "/.thumbnails", dir: true),
            entry(root + "/notes.txt", size: 5), entry(root + "/.nomedia", size: 0),
            entry(root + "/big.mov", size: 2_000_000_000),
        ]
        storage.folders[root + "/DCIM"] = [entry(root + "/DCIM/Camera", dir: true)]
        storage.folders[root + "/DCIM/Camera"] = [entry(root + "/DCIM/Camera/a.jpg", size: 2_000_000)]
    }

    func browser(connected: Bool = true) -> StorageBrowserModel {
        let box = box
        let downloads = folder.url
        return StorageBrowserModel(
            deviceId: "p", deviceName: "Pixel", connected: connected, service: storage,
            downloadFolder: { downloads }, reveal: { box.revealed.append($0) },
            networkBlocked: { box.blocked },
            cacheFolder: folder.url.appendingPathComponent("cache", isDirectory: true))
    }

    func started(connected: Bool = true) async throws -> StorageBrowserModel {
        let browser = browser(connected: connected)
        browser.start()
        try await eventually { browser.phase != .loading }
        return browser
    }

    // MARK: Navigation

    @Test func startsInTheFirstRootWithHiddenFilesHidden() async throws {
        let browser = try await started()
        #expect(browser.phase == .loaded)
        #expect(browser.folderName == "Internal storage")
        #expect(browser.visibleEntries.map(\.name) == ["DCIM", "big.mov", "notes.txt"])
        #expect(browser.hiddenCount == 2)
        #expect(!browser.canGoUp)
    }

    @Test func entersFoldersAndComesBack() async throws {
        let browser = try await started()
        browser.open(entry(root + "/DCIM", dir: true))
        try await eventually { browser.entriesLocation?.path == root + "/DCIM" }
        browser.open(entry(root + "/DCIM/Camera", dir: true))
        try await eventually { browser.entriesLocation?.path == root + "/DCIM/Camera" }
        #expect(browser.trail.map(\.name) == ["Internal storage", "DCIM", "Camera"])
        #expect(browser.canGoUp)

        browser.goUp()
        try await eventually { browser.entriesLocation?.path == root + "/DCIM" }
        browser.goBack()
        try await eventually { browser.entriesLocation?.path == root + "/DCIM/Camera" }
        #expect(browser.history.canGoForward)
        browser.goForward()
        try await eventually { browser.entriesLocation?.path == root + "/DCIM" }
        #expect(!browser.history.canGoForward)
    }

    @Test func historyDropsForwardOnANewVisit() {
        let r = StorageRoot(name: "R", path: "/r")
        var history = NavigationHistory()
        history.visit(RemoteLocation(root: r, path: "/r"))
        history.visit(RemoteLocation(root: r, path: "/r/a"))
        history.visit(RemoteLocation(root: r, path: "/r/a"))
        #expect(history.back.count == 1)
        #expect(history.goBack()?.path == "/r")
        #expect(history.canGoForward)
        history.visit(RemoteLocation(root: r, path: "/r/b"))
        #expect(!history.canGoForward)
        #expect(history.back.map(\.path) == ["/r"])
        #expect(history.goBack()?.path == "/r")
        #expect(history.goBack() == nil)
    }

    @Test func pathsStayInsideTheirRoot() {
        let r = StorageRoot(name: "Phone", path: "/storage/emulated/0/")
        #expect(RemotePath.parent(of: "/storage/emulated/0/DCIM/Camera", within: r.path) == "/storage/emulated/0/DCIM")
        #expect(RemotePath.parent(of: "/storage/emulated/0/DCIM", within: r.path) == "/storage/emulated/0")
        #expect(RemotePath.parent(of: "/storage/emulated/0", within: r.path) == nil)
        #expect(RemotePath.parent(of: "/storage/other", within: r.path) == nil)
        #expect(RemotePath.parent(of: "/a", within: "/") == "/")
        #expect(RemotePath.join("/storage/emulated/0/", "x.jpg") == "/storage/emulated/0/x.jpg")
        #expect(RemotePath.join("/", "x") == "/x")
        #expect(RemotePath.trail(to: "/storage/emulated/0/DCIM/Camera", in: r).map(\.path) == [
            "/storage/emulated/0", "/storage/emulated/0/DCIM", "/storage/emulated/0/DCIM/Camera",
        ])
        #expect(RemotePath.trail(to: "/a/b", in: StorageRoot(name: "Root", path: "/")).map(\.name) == ["Root", "a", "b"])
    }

    // MARK: Sorting and hidden files

    @Test func sortsFoldersFirstByAnyColumn() {
        let entries = [
            entry("/r/b.txt", size: 30, modified: 1), entry("/r/A.txt", size: 10, modified: 3),
            entry("/r/z", dir: true, size: 0, modified: 2), entry("/r/file10.txt", size: 20, modified: nil),
            entry("/r/file2.txt", size: 20, modified: 5),
        ]
        func names(_ sort: BrowserSort) -> [String] { sort.arrange(entries, showHidden: true).map(\.name) }
        #expect(names(BrowserSort()) == ["z", "A.txt", "b.txt", "file2.txt", "file10.txt"])
        #expect(names(BrowserSort(key: .name, ascending: false)) == ["z", "file10.txt", "file2.txt", "b.txt", "A.txt"])
        #expect(names(BrowserSort(key: .size)) == ["z", "A.txt", "file2.txt", "file10.txt", "b.txt"])
        #expect(names(BrowserSort(key: .modified, ascending: false)) == ["z", "file2.txt", "A.txt", "b.txt", "file10.txt"])
    }

    @Test func hidingHiddenFilesDropsThemFromTheSelection() async throws {
        let browser = try await started()
        browser.showHidden = true
        #expect(browser.visibleEntries.count == 5)
        browser.selection = [root + "/.nomedia", root + "/notes.txt"]
        browser.showHidden = false
        #expect(browser.selection == [root + "/notes.txt"])
    }

    // MARK: Errors and connection

    @Test func storageErrorsAreShownAsWritten() async throws {
        storage.failures[root + "/DCIM"] = LandlineError.Storage(message: "Allow All files access for KDE Connect.")
        let browser = try await started()
        browser.open(entry(root + "/DCIM", dir: true))
        try await eventually { browser.phase != .loading }
        #expect(browser.phase == .failed("Allow All files access for KDE Connect."))
        #expect(!browser.showsEntries)

        storage.failures = [:]
        browser.reload()
        try await eventually { browser.phase == .loaded }
        #expect(browser.entriesLocation?.path == root + "/DCIM")
    }

    @Test func unreachableExplainsLocalNetworkWhenBlocked() async throws {
        storage.rootsFailure = LandlineError.NotReachable
        let browser = try await started()
        guard case .failed(let reachable) = browser.phase else { Issue.record("not failed"); return }
        #expect(reachable.contains("Pixel isn’t reachable"))

        box.blocked = true
        browser.reload()
        try await eventually { browser.phase != .loading }
        guard case .failed(let blocked) = browser.phase else { Issue.record("not failed"); return }
        #expect(blocked.contains("Local Network"))
    }

    @Test func noRootsSaysSo() async throws {
        storage.roots = []
        let browser = try await started()
        guard case .failed(let message) = browser.phase else { Issue.record("not failed"); return }
        #expect(message.contains("isn’t sharing any folders"))
    }

    @Test func waitsWhileDisconnectedAndReloadsOnReconnect() async throws {
        let browser = try await started(connected: false)
        #expect(browser.phase == .disconnected)
        #expect(storage.listed.isEmpty)
        browser.setConnected(true)
        try await eventually { browser.phase == .loaded }
        #expect(storage.listed == [root])

        browser.setConnected(false)
        #expect(browser.phase == .disconnected)
        #expect(!browser.showsEntries)
        browser.setConnected(true)
        try await eventually { browser.phase == .loaded }
        #expect(storage.listed == [root, root])
    }

    @Test func aDisconnectMidLoadIsNotAnError() async throws {
        storage.failures[root] = LandlineError.NotReachable
        let browser = browser()
        browser.start()
        browser.setConnected(false)
        try await Task.sleep(for: .milliseconds(50))
        #expect(browser.phase == .disconnected)
    }

    // MARK: Downloads

    @Test func downloadPlanSkipsFoldersAndAvoidsTakenNames() {
        let into = URL(fileURLWithPath: "/tmp/in")
        let plan = DownloadPlan(
            [entry("/r/a.jpg"), entry("/r/Photos", dir: true), entry("/s/a.jpg"), entry("/r/b.txt")],
            into: into) { $0 == "b.txt" }
        #expect(plan.skippedFolders == ["Photos"])
        #expect(plan.items.map(\.name) == ["a.jpg", "a 2.jpg", "b 2.txt"])
        #expect(plan.items.map(\.local.path) == ["/tmp/in/a.jpg", "/tmp/in/a 2.jpg", "/tmp/in/b 2.txt"])
        #expect(plan.items.map(\.remote) == ["/r/a.jpg", "/s/a.jpg", "/r/b.txt"])
    }

    @Test func downloadsSelectionWithoutOverwritingThenReveals() async throws {
        folder.file("notes.txt")
        let browser = try await started()
        browser.selection = [root + "/notes.txt", root + "/DCIM"]
        browser.download(browser.selectedEntries)
        #expect(browser.notice == "“DCIM” is a folder. Only files can be downloaded.")
        try await eventually { browser.pendingOperations.isEmpty }
        let saved = folder.url.appendingPathComponent("notes 2.txt")
        #expect(storage.downloads.map(\.local) == [saved])
        #expect(storage.downloads.map(\.origin) == [.storage])
        #expect(box.revealed == [[saved]])
    }

    @Test func twoQuickDownloadsOfOneNameGetTwoNames() async throws {
        let browser = try await started()
        let notes = entry(root + "/notes.txt")
        browser.download([notes])
        browser.download([notes])
        try await eventually { box.revealed.count == 2 }
        #expect(storage.downloads.map(\.local.lastPathComponent) == ["notes.txt", "notes 2.txt"])
    }

    @Test func failedDownloadsAreKeptUntilDismissed() async throws {
        storage.downloadFailure = LandlineError.Storage(message: "No space left on the phone.")
        let browser = try await started()
        browser.download([entry(root + "/notes.txt")])
        try await eventually { !browser.failedOperations.isEmpty }
        #expect(browser.failedOperations.first?.state == .failed("No space left on the phone."))
        #expect(box.revealed.isEmpty)
        browser.dismissFailures()
        #expect(browser.operations.isEmpty)
    }

    @Test func uploadsUnderFreeNamesThenReloads() async throws {
        let browser = try await started()
        let local = folder.file("up/notes.txt")
        let other = folder.file("up/new.pdf")
        let dir = folder.folder("up/Folder")
        browser.upload([local, other, dir])
        #expect(browser.notice == "“Folder” is a folder. Only files can be uploaded.")
        try await eventually { storage.listed.count == 2 }
        #expect(storage.uploads.map(\.remote) == [root + "/notes 2.txt", root + "/new.pdf"])
    }

    // MARK: Quick Look

    @Test func previewLimitIsOneHundredMegabytes() {
        #expect(PreviewPolicy.decide(entry("/a", size: 100_000_000)) == .preview)
        #expect(PreviewPolicy.decide(entry("/a", size: 100_000_001)) == .tooLarge)
        #expect(PreviewPolicy.decide(entry("/a", dir: true)) == .notAFile)
    }

    @Test func largeFilesOfferADownloadInstead() async throws {
        let browser = try await started()
        let big = entry(root + "/big.mov", size: 2_000_000_000)
        browser.preview(big)
        #expect(browser.previewState == .tooLarge(big))
        #expect(storage.downloads.isEmpty)
        browser.downloadInsteadOfPreview()
        try await eventually { !box.revealed.isEmpty }
        #expect(storage.downloads.map(\.origin) == [.storage])
    }

    @Test func previewFetchesIntoTheCacheOnceAndClosingDeletesIt() async throws {
        let browser = try await started()
        browser.selection = [root + "/notes.txt"]
        browser.togglePreview()
        try await eventually { browser.previewURL != nil }
        let url = try #require(browser.previewURL)
        #expect(url.path.hasPrefix(folder.url.appendingPathComponent("cache").path))
        #expect(storage.downloads.map(\.origin) == [.preview])
        #expect(browser.previewState == nil)

        browser.togglePreview()
        #expect(browser.previewURL == nil)
        browser.togglePreview()
        #expect(browser.previewURL == url)
        #expect(storage.downloads.count == 1)

        browser.close()
        #expect(!FileManager.default.fileExists(atPath: url.path))
    }

    @Test func escapeWhilePreparingAbandonsThePreview() async throws {
        let (gate, open) = AsyncStream.makeStream(of: Void.self)
        storage.gate = gate
        let browser = try await started()
        let notes = entry(root + "/notes.txt")
        browser.preview(notes)
        try await eventually { browser.previewState != .preparing(notes, transferId: nil) }
        guard case .preparing(_, let transferId) = browser.previewState else { Issue.record("not preparing"); return }
        #expect(transferId == 1)
        browser.cancelPreview()
        open.yield()
        open.finish()
        try await Task.sleep(for: .milliseconds(50))
        #expect(browser.previewURL == nil)
        #expect(browser.previewState == nil)
    }

    @Test func dragOutDeliversUnderAFreeName() async throws {
        let browser = try await started()
        folder.file("notes.txt")
        let delivered = try await browser.deliver(entry(root + "/notes.txt"), to: folder.url.appendingPathComponent("notes.txt"))
        #expect(delivered.lastPathComponent == "notes 2.txt")
        #expect(FileManager.default.fileExists(atPath: delivered.path))
        #expect(box.revealed.isEmpty)
    }
}
