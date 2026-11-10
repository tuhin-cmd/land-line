import Foundation
import LandlineKit
import Observation

/// What the storage browser needs from the rest of the app. `AppModel`
/// provides it, adding the bookkeeping that tells storage transfers apart
/// from shared files.
@MainActor
protocol StorageService: AnyObject {
    func storageRoots(of deviceId: String) async throws -> [StorageRoot]
    func listDir(_ path: String, on deviceId: String) async throws -> [RemoteEntry]
    /// `onTransfer` gets the ID the core reports the download's progress
    /// under, once it starts.
    func download(
        _ remote: String, to local: URL, from deviceId: String, origin: TransferOrigin,
        onTransfer: @escaping @MainActor (UInt64) -> Void) async throws
    func upload(
        _ local: URL, to remote: String, on deviceId: String,
        onTransfer: @escaping @MainActor (UInt64) -> Void) async throws
    /// Stops a running download or upload. Its call then throws
    /// `LandlineError.Cancelled`.
    func cancelStorageTransfer(_ transferId: UInt64, on deviceId: String)
}

/// A folder on the phone, inside one of its storage roots.
struct RemoteLocation: Hashable, Sendable {
    let root: StorageRoot
    let path: String
}

/// Back and forward, as in Finder: visiting a new folder drops the forward
/// list.
struct NavigationHistory: Equatable {
    private(set) var back: [RemoteLocation] = []
    private(set) var current: RemoteLocation?
    private(set) var forward: [RemoteLocation] = []

    var canGoBack: Bool { !back.isEmpty }
    var canGoForward: Bool { !forward.isEmpty }

    mutating func visit(_ location: RemoteLocation) {
        guard location != current else { return }
        if let current { back.append(current) }
        current = location
        forward = []
    }

    mutating func goBack() -> RemoteLocation? {
        guard let previous = back.popLast() else { return nil }
        if let current { forward.append(current) }
        current = previous
        return previous
    }

    mutating func goForward() -> RemoteLocation? {
        guard let next = forward.popLast() else { return nil }
        if let current { back.append(current) }
        current = next
        return next
    }
}

/// Paths on the phone, which always use "/".
enum RemotePath {
    static func normalized(_ path: String) -> String {
        var path = path
        while path.count > 1, path.hasSuffix("/") { path.removeLast() }
        return path.isEmpty ? "/" : path
    }

    static func join(_ folder: String, _ name: String) -> String {
        let folder = normalized(folder)
        return folder == "/" ? "/" + name : folder + "/" + name
    }

    static func name(of path: String) -> String {
        let path = normalized(path)
        return path.split(separator: "/").last.map(String.init) ?? "/"
    }

    /// The enclosing folder, or `nil` at the top of the root: the phone only
    /// exposes what is inside its roots.
    static func parent(of path: String, within root: String) -> String? {
        let path = normalized(path)
        let root = normalized(root)
        guard path != root, isInside(path, root) else { return nil }
        guard let slash = path.lastIndex(of: "/") else { return nil }
        let parent = slash == path.startIndex ? "/" : String(path[..<slash])
        return isInside(parent, root) || parent == root ? parent : nil
    }

    /// The folders from the root down to `path`, for the path bar. The root
    /// is shown by its name rather than its path.
    static func trail(to path: String, in root: StorageRoot) -> [(name: String, path: String)] {
        let path = normalized(path)
        let rootPath = normalized(root.path)
        var trail = [(name: root.name, path: rootPath)]
        guard path != rootPath, isInside(path, rootPath) else { return trail }
        let rest = path.dropFirst(rootPath == "/" ? 1 : rootPath.count + 1)
        var current = rootPath
        for component in rest.split(separator: "/") {
            current = join(current, String(component))
            trail.append((name: String(component), path: current))
        }
        return trail
    }

    private static func isInside(_ path: String, _ root: String) -> Bool {
        root == "/" ? path.hasPrefix("/") : path == root || path.hasPrefix(root + "/")
    }
}

/// How the file list is ordered. Folders always come first, as the phone
/// lists them.
struct BrowserSort: Equatable, Sendable {
    enum Key: String, Sendable { case name, size, modified }

    var key: Key = .name
    var ascending = true

    func arrange(_ entries: [RemoteEntry], showHidden: Bool) -> [RemoteEntry] {
        let shown = showHidden ? entries : entries.filter { !BrowserSort.isHidden($0) }
        return shown.sorted { lhs, rhs in
            if lhs.isDir != rhs.isDir { return lhs.isDir }
            let order = compare(lhs, rhs)
            if order == .orderedSame {
                return lhs.name.localizedStandardCompare(rhs.name) == .orderedAscending
            }
            return ascending ? order == .orderedAscending : order == .orderedDescending
        }
    }

    private func compare(_ lhs: RemoteEntry, _ rhs: RemoteEntry) -> ComparisonResult {
        switch key {
        case .name:
            return lhs.name.localizedStandardCompare(rhs.name)
        case .size:
            return lhs.size == rhs.size ? .orderedSame : (lhs.size < rhs.size ? .orderedAscending : .orderedDescending)
        case .modified:
            let left = lhs.modifiedSecs ?? .min
            let right = rhs.modifiedSecs ?? .min
            return left == right ? .orderedSame : (left < right ? .orderedAscending : .orderedDescending)
        }
    }

    /// Dot-files, as on any Unix system; Android uses them for caches and
    /// markers such as ".nomedia".
    static func isHidden(_ entry: RemoteEntry) -> Bool {
        entry.name.hasPrefix(".")
    }
}

/// Which files a download writes where, decided before anything starts.
struct DownloadPlan: Equatable {
    struct Item: Equatable {
        let remote: String
        let name: String
        let local: URL
    }

    var items: [Item] = []
    /// Folders in the selection; the browser downloads files only.
    var skippedFolders: [String] = []

    /// `taken` says whether a name in `folder` is in use, on disk or by a
    /// download that has not finished yet.
    init(_ entries: [RemoteEntry], into folder: URL, taken: (String) -> Bool) {
        var chosen: Set<String> = []
        for entry in entries {
            if entry.isDir {
                skippedFolders.append(entry.name)
                continue
            }
            let name = UniqueName.pick(entry.name) { taken($0) || chosen.contains($0.lowercased()) }
            chosen.insert(name.lowercased())
            items.append(Item(remote: entry.path, name: name, local: folder.appendingPathComponent(name)))
        }
    }
}

/// Quick Look needs the whole file on the Mac, so large files are not
/// fetched just to look at them.
enum PreviewPolicy {
    // Decimal, as Finder counts, so the prompt can say "100 MB".
    static let sizeLimit: UInt64 = 100_000_000

    enum Decision: Equatable { case preview, tooLarge, notAFile }

    static func decide(_ entry: RemoteEntry) -> Decision {
        if entry.isDir { return .notAFile }
        return entry.size > sizeLimit ? .tooLarge : .preview
    }
}

/// Error text for the browser. The core's storage messages are written for
/// the user and are shown as they are.
enum StorageErrorText {
    static func message(for error: any Error, deviceName: String, networkBlocked: Bool) -> String {
        switch error {
        case LandlineError.Storage(let message):
            return message
        case LandlineError.NotReachable:
            if networkBlocked {
                return "macOS isn’t letting Landline connect to devices on the local network, so the phone’s files can’t be opened. Turn on Landline in System Settings > Privacy & Security > Local Network."
            }
            return "\(deviceName) isn’t reachable. Check that it’s on the same Wi-Fi network as this Mac, then try again."
        case LandlineError.UnknownDevice:
            return "\(deviceName) is no longer paired with this Mac."
        default:
            return AppModel.describe(error)
        }
    }
}

/// A download or upload started from the browser window.
struct BrowserOperation: Identifiable, Equatable {
    enum Kind: Equatable { case download, upload }
    enum State: Equatable { case waiting, running, failed(String), done }

    let id = UUID()
    let kind: Kind
    let name: String
    var transferId: UInt64?
    var state: State = .waiting

    var isPending: Bool { state == .waiting || state == .running }
}

enum BrowserPhase: Equatable {
    case loading
    case loaded
    case failed(String)
    /// The device went away. The window stays open and reloads when it
    /// comes back.
    case disconnected
}

enum PreviewState: Equatable {
    /// Downloading into the preview cache.
    case preparing(RemoteEntry, transferId: UInt64?)
    /// Over the size limit; the window offers a download instead.
    case tooLarge(RemoteEntry)
}

/// One storage browser window's state.
@MainActor @Observable
final class StorageBrowserModel {
    let deviceId: String
    var deviceName: String
    private(set) var isConnected: Bool
    private(set) var roots: [StorageRoot] = []
    private(set) var history = NavigationHistory()
    private(set) var phase: BrowserPhase = .loading
    /// Unfiltered and unsorted, as the phone listed them.
    private(set) var entries: [RemoteEntry] = []
    /// The folder `entries` belong to; differs from the current one while
    /// a new folder loads.
    private(set) var entriesLocation: RemoteLocation?
    var sort = BrowserSort()
    var showHidden = false {
        didSet {
            if !showHidden { selection = selection.filter { !RemotePath.name(of: $0).hasPrefix(".") } }
        }
    }
    /// Paths of the selected rows.
    var selection: Set<String> = []
    private(set) var operations: [BrowserOperation] = []
    var notice: String?
    private(set) var previewState: PreviewState?
    /// Set when the preview file is ready; the window shows it in Quick
    /// Look and clears this when Quick Look closes.
    var previewURL: URL?

    @ObservationIgnored private let service: any StorageService
    @ObservationIgnored private let downloadFolder: @MainActor () -> URL
    @ObservationIgnored private let reveal: @MainActor ([URL]) -> Void
    @ObservationIgnored private let networkBlocked: @MainActor () -> Bool
    @ObservationIgnored private let fileExists: (URL) -> Bool
    @ObservationIgnored private let cacheFolder: URL
    @ObservationIgnored private var loadToken: UUID?
    @ObservationIgnored private var previewToken: UUID?
    @ObservationIgnored private var previewCache: [String: (entry: RemoteEntry, url: URL)] = [:]
    @ObservationIgnored private var previewSerial = 0
    @ObservationIgnored private var tail: Task<Void, Never>?
    /// Local files that queued downloads will create, so a second download
    /// of the same name picks another before the first has landed.
    @ObservationIgnored private var reservedLocal: Set<String> = []
    @ObservationIgnored private var started = false

    init(
        deviceId: String, deviceName: String, connected: Bool, service: any StorageService,
        downloadFolder: @escaping @MainActor () -> URL,
        reveal: @escaping @MainActor ([URL]) -> Void,
        networkBlocked: @escaping @MainActor () -> Bool = { false },
        cacheFolder: URL,
        fileExists: @escaping (URL) -> Bool = { FileManager.default.fileExists(atPath: $0.path) }
    ) {
        self.deviceId = deviceId
        self.deviceName = deviceName
        isConnected = connected
        self.service = service
        self.downloadFolder = downloadFolder
        self.reveal = reveal
        self.networkBlocked = networkBlocked
        self.cacheFolder = cacheFolder
        self.fileExists = fileExists
        if !connected { phase = .disconnected }
    }

    // MARK: Reading

    var current: RemoteLocation? { history.current }

    var visibleEntries: [RemoteEntry] {
        sort.arrange(entries, showHidden: showHidden)
    }

    var hiddenCount: Int { entries.filter(BrowserSort.isHidden).count }

    var selectedEntries: [RemoteEntry] {
        visibleEntries.filter { selection.contains($0.path) }
    }

    /// Whether the table should show `entries`: they belong to the current
    /// folder, even if it is being reloaded.
    var showsEntries: Bool {
        entriesLocation != nil && entriesLocation == current && (phase == .loaded || phase == .loading)
    }

    var folderName: String {
        guard let current else { return deviceName }
        return RemotePath.normalized(current.path) == RemotePath.normalized(current.root.path)
            ? current.root.name : RemotePath.name(of: current.path)
    }

    var trail: [(name: String, path: String)] {
        guard let current else { return [] }
        return RemotePath.trail(to: current.path, in: current.root)
    }

    var canGoUp: Bool {
        guard let current else { return false }
        return RemotePath.parent(of: current.path, within: current.root.path) != nil
    }

    var pendingOperations: [BrowserOperation] { operations.filter(\.isPending) }
    var failedOperations: [BrowserOperation] {
        operations.filter { if case .failed = $0.state { true } else { false } }
    }

    // MARK: Navigation

    /// Loads the roots and the first of them. Does nothing the second time.
    func start() {
        guard !started else { return }
        started = true
        if isConnected { loadRoots() }
    }

    func setConnected(_ connected: Bool) {
        guard connected != isConnected else { return }
        isConnected = connected
        if connected {
            reload()
        } else {
            loadToken = nil
            phase = .disconnected
        }
    }

    func reload() {
        guard isConnected else { return }
        if roots.isEmpty || current == nil {
            loadRoots()
        } else if let current {
            load(current)
        }
    }

    func select(root: StorageRoot) {
        go(to: RemoteLocation(root: root, path: root.path))
    }

    func go(toPath path: String) {
        guard let current else { return }
        go(to: RemoteLocation(root: current.root, path: path))
    }

    func goBack() {
        if let location = history.goBack() { load(location) }
    }

    func goForward() {
        if let location = history.goForward() { load(location) }
    }

    func goUp() {
        guard let current, let parent = RemotePath.parent(of: current.path, within: current.root.path) else { return }
        go(to: RemoteLocation(root: current.root, path: parent))
    }

    /// Double-click or Return: enters a folder, downloads a file.
    func open(_ entry: RemoteEntry) {
        if entry.isDir {
            go(toPath: entry.path)
        } else {
            download([entry])
        }
    }

    func openSelection() {
        let selected = selectedEntries
        if selected.count == 1, let entry = selected.first {
            open(entry)
        } else if !selected.isEmpty {
            download(selected)
        }
    }

    private func go(to location: RemoteLocation) {
        history.visit(location)
        load(location)
    }

    private func loadRoots() {
        let token = UUID()
        loadToken = token
        phase = .loading
        Task {
            do {
                let roots = try await service.storageRoots(of: deviceId)
                guard loadToken == token else { return }
                self.roots = roots
                guard let first = roots.first else {
                    phase = .failed("\(deviceName) isn’t sharing any folders. Check the Filesystem expose plugin’s settings in KDE Connect.")
                    return
                }
                let location = current.flatMap { current in
                    roots.contains(current.root) ? current : nil
                } ?? RemoteLocation(root: first, path: first.path)
                history.visit(location)
                load(location)
            } catch {
                guard loadToken == token else { return }
                fail(error)
            }
        }
    }

    private func load(_ location: RemoteLocation) {
        let token = UUID()
        loadToken = token
        if entriesLocation != location { selection = [] }
        guard isConnected else {
            phase = .disconnected
            return
        }
        phase = .loading
        Task {
            do {
                let list = try await service.listDir(location.path, on: deviceId)
                guard loadToken == token else { return }
                entries = list
                entriesLocation = location
                let paths = Set(list.map(\.path))
                selection = selection.intersection(paths)
                phase = .loaded
            } catch {
                guard loadToken == token else { return }
                fail(error)
            }
        }
    }

    private func fail(_ error: any Error) {
        // A device that has just gone makes every call fail; the
        // disconnected state says it better than the error would.
        guard isConnected else {
            phase = .disconnected
            return
        }
        phase = .failed(message(for: error))
    }

    private func message(for error: any Error) -> String {
        StorageErrorText.message(for: error, deviceName: deviceName, networkBlocked: networkBlocked())
    }

    // MARK: Downloads and uploads

    /// Downloads files into `folder`, the received-files folder by default,
    /// without replacing anything there, then shows them in Finder.
    func download(_ entries: [RemoteEntry], into folder: URL? = nil) {
        let folder = folder ?? downloadFolder()
        let plan = DownloadPlan(entries, into: folder) { name in
            let url = folder.appendingPathComponent(name)
            return fileExists(url) || reservedLocal.contains(url.path.lowercased())
        }
        if !plan.skippedFolders.isEmpty {
            notice = plan.skippedFolders.count == 1
                ? "“\(plan.skippedFolders[0])” is a folder. Only files can be downloaded."
                : "Folders were skipped. Only files can be downloaded."
        }
        guard !plan.items.isEmpty else { return }
        let jobs = plan.items.map { item in
            reservedLocal.insert(item.local.path.lowercased())
            let operation = BrowserOperation(kind: .download, name: item.name)
            operations.append(operation)
            return (operation.id, item)
        }
        enqueue { [weak self] in
            var saved: [URL] = []
            for (id, item) in jobs {
                guard let self else { return }
                let ok = await self.run(id) { onTransfer in
                    try await self.service.download(
                        item.remote, to: item.local, from: self.deviceId, origin: .storage, onTransfer: onTransfer)
                }
                self.reservedLocal.remove(item.local.path.lowercased())
                if ok { saved.append(item.local) }
            }
            if !saved.isEmpty { self?.reveal(saved) }
        }
    }

    /// Uploads files into the current folder under names not already used
    /// there, then reloads it.
    func upload(_ urls: [URL]) {
        guard let location = current else { return }
        var folders: [String] = []
        var files: [URL] = []
        for url in urls {
            var isDirectory: ObjCBool = false
            if FileManager.default.fileExists(atPath: url.path, isDirectory: &isDirectory), isDirectory.boolValue {
                folders.append(url.lastPathComponent)
            } else {
                files.append(url)
            }
        }
        if !folders.isEmpty {
            notice = folders.count == 1
                ? "“\(folders[0])” is a folder. Only files can be uploaded."
                : "Folders were skipped. Only files can be uploaded."
        }
        guard !files.isEmpty else { return }
        let existing = entriesLocation == location ? Set(entries.map(\.name)) : []
        let names = UniqueName.pick(files.map(\.lastPathComponent), existing: existing)
        let jobs = zip(files, names).map { file, name in
            let operation = BrowserOperation(kind: .upload, name: name)
            operations.append(operation)
            return (operation.id, file, RemotePath.join(location.path, name))
        }
        enqueue { [weak self] in
            for (id, file, remote) in jobs {
                guard let self else { return }
                _ = await self.run(id) { onTransfer in
                    try await self.service.upload(file, to: remote, on: self.deviceId, onTransfer: onTransfer)
                }
            }
            if let self, self.current == location { self.load(location) }
        }
    }

    /// Runs one operation, tracking its state. Returns whether it worked.
    private func run(
        _ id: UUID, _ work: (@escaping @MainActor (UInt64) -> Void) async throws -> Void
    ) async -> Bool {
        update(id) { $0.state = .running }
        do {
            try await work { [weak self] transferId in
                self?.update(id) { $0.transferId = transferId }
            }
            operations.removeAll { $0.id == id }
            return true
        } catch LandlineError.Cancelled {
            // The user stopped it; that is not a failure to report.
            operations.removeAll { $0.id == id }
            return false
        } catch {
            update(id) { $0.state = .failed(message(for: error)) }
            return false
        }
    }

    /// Stops the download or upload that is running now. The ones queued
    /// behind it carry on.
    func stopRunningOperation() {
        guard let running = operations.first(where: { $0.state == .running }),
            let transferId = running.transferId
        else { return }
        service.cancelStorageTransfer(transferId, on: deviceId)
    }

    private func update(_ id: UUID, _ change: (inout BrowserOperation) -> Void) {
        if let index = operations.firstIndex(where: { $0.id == id }) { change(&operations[index]) }
    }

    func dismissFailures() {
        operations.removeAll { if case .failed = $0.state { true } else { false } }
    }

    /// One at a time, in the order asked: the phone serves them over one
    /// connection anyway, and the window can say "2 of 5".
    private func enqueue(_ work: @escaping @MainActor () async -> Void) {
        let previous = tail
        tail = Task {
            await previous?.value
            await work()
        }
    }

    // MARK: Quick Look

    /// Space: shows the file in Quick Look after fetching it into the
    /// preview cache, or closes Quick Look if it is open.
    func togglePreview() {
        if previewURL != nil || previewState != nil {
            cancelPreview()
            return
        }
        guard let entry = selectedEntries.first else { return }
        preview(entry)
    }

    func preview(_ entry: RemoteEntry) {
        switch PreviewPolicy.decide(entry) {
        case .notAFile:
            return
        case .tooLarge:
            previewState = .tooLarge(entry)
            return
        case .preview:
            break
        }
        if let cached = previewCache[entry.path], cached.entry == entry, fileExists(cached.url) {
            previewURL = cached.url
            return
        }
        previewSerial += 1
        let local = cacheFolder.appendingPathComponent("\(previewSerial)", isDirectory: true)
            .appendingPathComponent(entry.name)
        let token = UUID()
        previewToken = token
        previewState = .preparing(entry, transferId: nil)
        Task {
            do {
                try FileManager.default.createDirectory(
                    at: local.deletingLastPathComponent(), withIntermediateDirectories: true)
                try await service.download(entry.path, to: local, from: deviceId, origin: .preview) {
                    [weak self] transferId in
                    guard let self, self.previewToken == token else { return }
                    self.previewState = .preparing(entry, transferId: transferId)
                }
                previewCache[entry.path] = (entry, local)
                guard previewToken == token else { return }
                previewToken = nil
                previewState = nil
                previewURL = local
            } catch LandlineError.Cancelled {
                // Stopped by `cancelPreview`, which has already reset the state.
            } catch {
                guard previewToken == token else { return }
                previewToken = nil
                previewState = nil
                notice = message(for: error)
            }
        }
    }

    /// Escape, or closing the preview. Stops the download feeding it, if
    /// one is still running.
    func cancelPreview() {
        if case .preparing(_, let transferId?) = previewState {
            service.cancelStorageTransfer(transferId, on: deviceId)
        }
        previewToken = nil
        previewState = nil
        previewURL = nil
    }

    /// From the "too large" prompt.
    func downloadInsteadOfPreview() {
        guard case .tooLarge(let entry) = previewState else { return }
        previewState = nil
        download([entry])
    }

    /// The window closed: forgets the preview and deletes the cache.
    func close() {
        cancelPreview()
        loadToken = nil
        previewCache = [:]
        try? FileManager.default.removeItem(at: cacheFolder)
    }

    // MARK: Drag out

    /// Delivers a file dragged to Finder. `destination` is where Finder asked
    /// for it; if that name is taken there, the next free one is used.
    func deliver(_ entry: RemoteEntry, to destination: URL) async throws -> URL {
        let folder = destination.deletingLastPathComponent()
        let name = UniqueName.pick(destination.lastPathComponent) {
            fileExists(folder.appendingPathComponent($0))
        }
        let local = folder.appendingPathComponent(name)
        let operation = BrowserOperation(kind: .download, name: name)
        operations.append(operation)
        let ok = await run(operation.id) { onTransfer in
            try await service.download(entry.path, to: local, from: deviceId, origin: .storage, onTransfer: onTransfer)
        }
        guard ok else {
            throw CocoaError(.fileWriteUnknown, userInfo: [NSLocalizedDescriptionKey: "Couldn’t download \(entry.name)."])
        }
        return local
    }
}

/// The browser's temporary copies for Quick Look, one folder per window.
@MainActor
enum PreviewCache {
    /// Demo mode keeps its own, so a demo started next to the real app
    /// never deletes the real app's previews.
    static var root = FileManager.default.temporaryDirectory
        .appendingPathComponent("io.github.tuhin-cmd.landline.preview", isDirectory: true)

    static func folder(for deviceId: String) -> URL {
        root.appendingPathComponent("\(deviceId)-\(UUID().uuidString)", isDirectory: true)
    }

    /// At launch: whatever an earlier run left behind.
    static func clean() {
        try? FileManager.default.removeItem(at: root)
    }
}
