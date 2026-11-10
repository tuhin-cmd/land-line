import Foundation
import LandlineKit
import Observation

/// The app's state, mirrored from the backend's callbacks.
@MainActor @Observable
final class AppModel {
    private(set) var devices: [Device] = []
    /// `nil` until the core first reports, which it does as soon as it starts.
    private(set) var networkStatus: NetworkStatus?
    /// Why the core could not start; the popover shows it instead of the list.
    private(set) var startError: String?
    /// The latest pairing failure or command error, until dismissed.
    var notice: Notice?
    /// Devices whose ping arrived in the last few seconds.
    private(set) var recentPings: Set<String> = []
    /// Devices we pinged in the last moment, to acknowledge the click.
    private(set) var sentPings: Set<String> = []
    /// The device whose row is asking "Unpair?", if any.
    var confirmingUnpair: String?
    /// Devices whose clipboard arrived in the last few seconds.
    private(set) var recentClipboard: Set<String> = []
    /// Notifications showing on each device, newest first. A device's list
    /// is dropped when it disconnects; the phone sends them again, marked
    /// silent, when it comes back.
    private(set) var phoneNotifications: [String: [PhoneNotification]] = [:]
    /// The last one-time code, until it is too old to be of use.
    private(set) var latestCode: ReceivedCode?
    /// `nil` until first checked.
    private(set) var notificationPermission: NotificationPermission?
    private(set) var loginItemState: LoginItemState
    /// Why the last launch-at-login change failed, until the next attempt.
    private(set) var loginItemError: String?
    /// Running transfers, and ended ones until they expire.
    private(set) var transferList = TransferList()
    /// The name the phone sees for this Mac.
    let macName: String
    let isDemo: Bool
    let settings: AppSettings

    @ObservationIgnored private let makeBackend: @MainActor () throws -> any DeviceBackend
    @ObservationIgnored private var backend: (any DeviceBackend)?
    @ObservationIgnored private var eventTask: Task<Void, Never>?
    @ObservationIgnored private var flashTokens: [FlashKey: UUID] = [:]
    @ObservationIgnored private let notificationCenter: any NotificationCenterClient
    @ObservationIgnored private let loginItem: any LoginItem
    @ObservationIgnored let workspace: any Workspace
    @ObservationIgnored let defaultDownloadDir: String
    /// Storage calls whose transfer has not been reported yet.
    @ObservationIgnored private var expectedTransfers: [ExpectedTransfer] = []
    @ObservationIgnored private var batcher = ReceivedBatcher()
    @ObservationIgnored private var batchCheck: Task<Void, Never>?
    @ObservationIgnored private var expiryCheck: Task<Void, Never>?
    @ObservationIgnored private var bannerSerial = 0
    /// Open storage browser windows, by device.
    @ObservationIgnored private var browsers: [String: StorageBrowserModel] = [:]
    @ObservationIgnored private var clipboard: ClipboardSync!
    /// Codes reported lately, to recognise the phone notification that
    /// carries the same code.
    @ObservationIgnored private var recentCodes: [ReceivedCode] = []
    @ObservationIgnored private var codeSerial = 0
    @ObservationIgnored private var connectedIds: Set<String> = []
    @ObservationIgnored private var askedForPermission = false
    /// Banners from the last run would be stale; only the live app clears them.
    @ObservationIgnored private let clearsBannersAtStart: Bool

    /// How long "Ping received" stays on a row.
    static let pingDisplayTime: Duration = .seconds(4)
    /// How long a code is kept for the panel and for matching duplicates.
    /// Codes usually expire within minutes.
    nonisolated static let codeLifetime: TimeInterval = 10 * 60

    convenience init(mode: LaunchMode) {
        switch mode {
        case .live:
            let services = AppServices.live()
            let downloadDir = services.settings.downloadDir ?? services.defaultDownloadDir
            self.init(
                macName: CoreBackend.computerName(), isDemo: false, services: services,
                clearsBannersAtStart: true
            ) { try CoreBackend(config: CoreBackend.defaultConfig(downloadDir: downloadDir)) }
        case .demo(let scenario):
            self.init(
                macName: "Demo Mac", isDemo: true, services: .demo(scenario),
                clearsBannersAtStart: false
            ) { try DemoBackend(scenario) }
        }
    }

    /// For tests: services and a backend built by the caller.
    convenience init(
        macName: String = "Test Mac", services: AppServices,
        backend: @escaping @MainActor () throws -> any DeviceBackend
    ) {
        self.init(macName: macName, isDemo: false, services: services, clearsBannersAtStart: false, backend: backend)
    }

    private init(
        macName: String, isDemo: Bool, services: AppServices, clearsBannersAtStart: Bool,
        backend: @escaping @MainActor () throws -> any DeviceBackend
    ) {
        self.macName = macName
        self.isDemo = isDemo
        settings = services.settings
        notificationCenter = services.notificationCenter
        loginItem = services.loginItem
        workspace = services.workspace
        defaultDownloadDir = services.defaultDownloadDir
        loginItemState = services.loginItem.state
        self.clearsBannersAtStart = clearsBannersAtStart
        makeBackend = backend
        clipboard = ClipboardSync(pasteboard: services.pasteboard) { [weak self] text in
            self?.backend?.sendClipboard(text)
        }
        notificationCenter.onResponse = { [weak self] response in
            self?.handle(response)
        }
    }

    var sections: DeviceSections { DeviceSections(devices) }

    var menuBarSymbol: String {
        MenuBarIcon.symbol(devices: devices, network: networkStatus, failedToStart: startError != nil)
    }

    func start() {
        guard backend == nil else { return }
        let backend: any DeviceBackend
        do {
            backend = try makeBackend()
        } catch {
            startError = Self.describe(error)
            return
        }
        startError = nil
        self.backend = backend
        if clearsBannersAtStart {
            notificationCenter.removeAll()
        }
        let events = backend.events
        eventTask = Task { [weak self] in
            for await event in events {
                self?.handle(event)
            }
        }
        backend.start()
        // The core sends a list on change only; a known paired device that is
        // offline would not show until something else changed.
        handleDevices(backend.devices())
        Task { await refreshNotificationPermission() }
    }

    func stop() {
        clipboard.setPolling(false)
        backend?.stop()
        eventTask?.cancel()
        backend = nil
    }

    func handle(_ event: CoreEvent) {
        switch event {
        case .devicesChanged(let devices):
            handleDevices(devices)
        case .pairingFailed(let deviceId, let reason):
            notice = .pairingFailed(reason, deviceName: name(of: deviceId))
        case .pingReceived(let deviceId):
            flash(deviceId, in: \.recentPings, for: Self.pingDisplayTime)
        case .networkStatusChanged(let status):
            networkStatus = status
        case .notificationPosted(let deviceId, let notification):
            notificationPosted(notification, on: deviceId)
        case .notificationRemoved(let deviceId, let notificationId):
            removeNotification(notificationId, on: deviceId)
        case .smsReceived:
            // The messaging app's own notification already shows the message,
            // and a code in it arrives separately as `otpDetected`.
            break
        case .otpDetected(let deviceId, let otp):
            codeDetected(otp, on: deviceId)
        case .clipboardReceived(let deviceId, let text):
            guard settings.syncClipboard else { return }
            clipboard.receive(text)
            flash(deviceId, in: \.recentClipboard, for: Self.pingDisplayTime)
        case .transferUpdated(let transfer):
            transferUpdated(transfer)
        case .textShared(let deviceId, let text):
            textShared(text, from: deviceId)
        case .urlShared(let deviceId, let url):
            urlShared(url, from: deviceId)
        }
    }

    private func handleDevices(_ devices: [Device]) {
        self.devices = devices
        if let id = confirmingUnpair, !devices.contains(where: { $0.id == id && $0.pairState == .paired }) {
            confirmingUnpair = nil
        }
        let connected = Set(devices.filter { RowState($0) == .connected }.map(\.id))
        for deviceId in phoneNotifications.keys where !connected.contains(deviceId) {
            clearNotifications(of: deviceId)
        }
        let newlyConnected = !connected.subtracting(connectedIds).isEmpty
        connectedIds = connected
        for (deviceId, browser) in browsers {
            if let name = name(of: deviceId) { browser.deviceName = name }
            browser.setConnected(connected.contains(deviceId))
        }
        updateClipboardPolling()
        if newlyConnected {
            // The first connected phone is when banners start to matter, so
            // that is when to ask, rather than at a first launch with nothing
            // to show yet.
            Task { await askForNotificationPermissionIfNeeded() }
        }
    }

    func perform(_ action: RowAction, on deviceId: String) {
        switch action {
        case .pair:
            notice = nil
            send(.requestPair, to: deviceId)
        case .accept:
            send(.acceptPair, to: deviceId)
        case .reject, .cancel:
            send(.rejectPair, to: deviceId)
        case .ping:
            send(.sendPing, to: deviceId)
            flash(deviceId, in: \.sentPings, for: .seconds(1.5))
        case .unpair:
            confirmingUnpair = deviceId
        }
    }

    func confirmUnpair() {
        guard let id = confirmingUnpair else { return }
        confirmingUnpair = nil
        send(.unpair, to: id)
    }

    private func send(_ command: DeviceCommand, to deviceId: String) {
        guard let backend else { return }
        do {
            try backend.send(command, to: deviceId)
        } catch LandlineError.UnknownDevice {
            notice = .deviceGone(name(of: deviceId))
        } catch {
            notice = Notice(title: "Something went wrong", message: Self.describe(error))
        }
    }

    // MARK: Phone notifications

    /// Phone notifications for the panel: from connected devices, newest
    /// first, without muted apps.
    var listedNotifications: [ListedNotification] {
        guard settings.showNotifications else { return [] }
        return devices.filter { RowState($0) == .connected }
            .flatMap { device in
                (phoneNotifications[device.id] ?? [])
                    .filter { !settings.isMuted($0.appName) }
                    .map { ListedNotification(deviceId: device.id, deviceName: device.name, notification: $0) }
            }
            .sorted { $0.notification.timeMs > $1.notification.timeMs }
    }

    private func notificationPosted(_ notification: PhoneNotification, on deviceId: String) {
        var list = phoneNotifications[deviceId] ?? []
        let previous = list.first { $0.id == notification.id }
        list.removeAll { $0.id == notification.id }
        list.insert(notification, at: 0)
        phoneNotifications[deviceId] = list

        let id = Banner.phoneId(deviceId: deviceId, notificationId: notification.id)
        if carriesRecentCode(notification, on: deviceId) {
            // The code's own banner already says this.
            notificationCenter.remove(ids: [id])
            return
        }
        guard !notification.silent, settings.showNotifications, !settings.isMuted(notification.appName) else {
            return
        }
        // Apps repost unchanged notifications (a progress tick, a re-sort);
        // the banner should not come back for each.
        if let previous, previous.appName == notification.appName, previous.title == notification.title,
            previous.text == notification.text
        {
            return
        }
        notificationCenter.show(.phone(notification, deviceId: deviceId))
    }

    private func removeNotification(_ notificationId: String, on deviceId: String) {
        phoneNotifications[deviceId]?.removeAll { $0.id == notificationId }
        notificationCenter.remove(ids: [Banner.phoneId(deviceId: deviceId, notificationId: notificationId)])
    }

    private func clearNotifications(of deviceId: String) {
        let ids = (phoneNotifications[deviceId] ?? []).map {
            Banner.phoneId(deviceId: deviceId, notificationId: $0.id)
        }
        phoneNotifications[deviceId] = nil
        notificationCenter.remove(ids: ids)
    }

    /// Clears a notification on the phone, from the panel.
    func dismiss(_ notificationId: String, on deviceId: String) {
        guard let notification = phoneNotifications[deviceId]?.first(where: { $0.id == notificationId }),
            notification.dismissable
        else { return }
        dismissOnPhone(notificationId, on: deviceId)
        // Not waiting for the phone to confirm keeps the panel responsive;
        // its removal, when it comes, finds nothing left to do.
        removeNotification(notificationId, on: deviceId)
    }

    private func dismissOnPhone(_ notificationId: String, on deviceId: String) {
        // A failure means the phone is gone, and the notification with it
        // as far as the Mac is concerned; nothing worth interrupting for.
        try? backend?.send(.dismissNotification(id: notificationId), to: deviceId)
    }

    func mute(_ appName: String) {
        settings.mute(appName)
        let ids = phoneNotifications.flatMap { deviceId, list in
            list.filter { $0.appName == appName }.map { Banner.phoneId(deviceId: deviceId, notificationId: $0.id) }
        }
        notificationCenter.remove(ids: ids)
    }

    func unmute(_ appName: String) {
        settings.unmute(appName)
    }

    func setShowNotifications(_ on: Bool) {
        settings.showNotifications = on
        guard !on else { return }
        let ids = phoneNotifications.flatMap { deviceId, list in
            list.map { Banner.phoneId(deviceId: deviceId, notificationId: $0.id) }
        }
        notificationCenter.remove(ids: ids)
    }

    func handle(_ response: BannerResponse) {
        let info = response.userInfo
        switch response.action {
        case .opened:
            break
        case .dismissed:
            // Only a notification the phone lets go of; closing the banner of
            // an ongoing one (a call, navigation) just closes the banner.
            guard info[BannerInfo.dismissable] == "1", let deviceId = info[BannerInfo.deviceId],
                let notificationId = info[BannerInfo.notificationId]
            else { return }
            dismissOnPhone(notificationId, on: deviceId)
        case .muteApp:
            if let appName = info[BannerInfo.appName] { mute(appName) }
        case .copyCode:
            if let code = info[BannerInfo.code] { copyCode(code) }
        case .showInFinder:
            workspace.reveal(Self.existingFiles(info[BannerInfo.paths]))
        case .openFile:
            for url in Self.existingFiles(info[BannerInfo.paths]) { workspace.open(url) }
        case .openLink:
            if let link = info[BannerInfo.url].flatMap(SharedLink.openable) { workspace.open(link) }
        }
        // Clicking the banner itself does the obvious thing for files and
        // links; other banners just close.
        if response.action == .opened {
            if info[BannerInfo.paths] != nil {
                workspace.reveal(Self.existingFiles(info[BannerInfo.paths]))
            } else if let link = info[BannerInfo.url].flatMap(SharedLink.openable) {
                workspace.open(link)
            }
        }
    }

    /// Files named in a banner that are still there; the user may have
    /// moved them since.
    private static func existingFiles(_ paths: String?) -> [URL] {
        (paths ?? "").split(separator: "\n").map { URL(fileURLWithPath: String($0)) }
            .filter { FileManager.default.fileExists(atPath: $0.path) }
    }

    func refreshNotificationPermission() async {
        notificationPermission = await notificationCenter.permission()
    }

    /// Asks once per run at most, and never again once answered: a denial
    /// is shown in Settings, not asked about again.
    func askForNotificationPermissionIfNeeded() async {
        guard !askedForPermission else { return }
        askedForPermission = true
        let current = await notificationCenter.permission()
        notificationPermission = current == .notAsked ? await notificationCenter.requestPermission() : current
    }

    /// From Settings, when the user has not been asked yet.
    func requestNotificationPermission() async {
        notificationPermission = await notificationCenter.requestPermission()
    }

    // MARK: One-time codes

    private func codeDetected(_ otp: OtpCode, on deviceId: String) {
        let copied = settings.copyCodesAutomatically
        if copied {
            clipboard.writeCode(otp.code)
        }
        let code = ReceivedCode(code: otp.code, source: otp.source, deviceId: deviceId, date: .now, copied: copied)
        latestCode = code
        recentCodes = recentCodes.filter { !$0.isExpired } + [code]
        Task { [weak self] in
            try? await Task.sleep(for: .seconds(Self.codeLifetime))
            if self?.latestCode?.date == code.date { self?.latestCode = nil }
        }

        // The message the code came from may already be showing as a phone
        // notification; the code's banner replaces it.
        let duplicates = (phoneNotifications[deviceId] ?? []).filter { code.appears(in: $0) }
        notificationCenter.remove(ids: duplicates.map { Banner.phoneId(deviceId: deviceId, notificationId: $0.id) })

        codeSerial += 1
        notificationCenter.show(.code(otp, deviceId: deviceId, copied: copied, serial: codeSerial))
    }

    private func carriesRecentCode(_ notification: PhoneNotification, on deviceId: String) -> Bool {
        recentCodes.contains { $0.deviceId == deviceId && !$0.isExpired && $0.appears(in: notification) }
    }

    /// Copies the code, marked so clipboard managers do not keep it.
    func copyCode(_ code: String) {
        clipboard.writeCode(code)
        if latestCode?.code == code {
            latestCode?.copied = true
        }
    }

    func clearLatestCode() {
        latestCode = nil
    }

    // MARK: Clipboard

    func setSyncClipboard(_ on: Bool) {
        settings.syncClipboard = on
        updateClipboardPolling()
    }

    private func updateClipboardPolling() {
        clipboard.setPolling(settings.syncClipboard && !connectedIds.isEmpty)
    }

    /// Exposed for tests, which cannot wait on the timer.
    var isWatchingClipboard: Bool { clipboard.isPolling }

    func pollClipboard() { clipboard.poll() }

    // MARK: Files and login

    /// `nil` goes back to Downloads.
    func setDownloadDir(_ path: String?) {
        settings.downloadDir = path
        backend?.setDownloadDir(path ?? defaultDownloadDir)
    }

    /// Where received files go, and where the storage browser downloads to.
    var downloadFolder: URL {
        URL(fileURLWithPath: settings.downloadDir ?? defaultDownloadDir, isDirectory: true)
    }

    func setLaunchAtLogin(_ on: Bool) {
        do {
            try loginItem.setEnabled(on)
            loginItemError = nil
        } catch {
            loginItemError = LoginItemState.failureMessage(enabling: on, error: error)
        }
        loginItemState = loginItem.state
        // Registering can succeed without the app being listed; say so
        // rather than leaving a switch that flips back without a word.
        if loginItemError == nil, on, loginItemState == .disabled {
            loginItemError = LoginItemState.failureMessage(enabling: true, error: nil)
        }
    }

    func refreshLoginItem() {
        loginItemState = loginItem.state
    }

    private func name(of deviceId: String) -> String? {
        devices.first { $0.id == deviceId }?.name
    }

    /// Adds the ID to the set and takes it out again after `duration`, unless
    /// a newer flash for the same device has started meanwhile.
    private func flash(_ deviceId: String, in set: ReferenceWritableKeyPath<AppModel, Set<String>>, for duration: Duration) {
        let token = UUID()
        let key = FlashKey(set: set, deviceId: deviceId)
        flashTokens[key] = token
        self[keyPath: set].insert(deviceId)
        Task { [weak self] in
            try? await Task.sleep(for: duration)
            guard let self, self.flashTokens[key] == token else { return }
            self.flashTokens[key] = nil
            self[keyPath: set].remove(deviceId)
        }
    }

    private struct FlashKey: Hashable {
        let set: ReferenceWritableKeyPath<AppModel, Set<String>>
        let deviceId: String
    }

    /// UniFFI errors describe themselves as `Startup(message: "...")`; the
    /// message is what a person should read.
    nonisolated static func describe(_ error: any Error) -> String {
        if case LandlineError.Startup(let message) = error { return message }
        return String(describing: error)
    }
}

// MARK: - Logs

extension AppModel {
    func logReport(date: Date = .now) -> String {
        LogReport.make(
            appVersion: Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "dev",
            coreVersion: coreVersion(),
            osVersion: ProcessInfo.processInfo.operatingSystemVersionString,
            state: reportState,
            logs: LogReport.read([LogFiles.previous, LogFiles.current]),
            date: date)
    }

    /// The app's state for a log export, with nothing that identifies the
    /// user, their devices or their folders.
    var reportState: [(String, String)] {
        func onOff(_ on: Bool) -> String { on ? "on" : "off" }
        let paired = devices.filter { $0.pairState == .paired }
        return [
            ("Mode", isDemo ? "demo" : "live"),
            ("Network", networkStatus.map { "\($0)" } ?? "unknown"),
            ("Paired devices", "\(paired.count), \(paired.filter(\.reachable).count) connected"),
            ("Other devices nearby", "\(devices.count - paired.count)"),
            ("Mac notifications", notificationPermission.map { "\($0)" } ?? "unknown"),
            ("Open at login", "\(loginItemState)"),
            ("Show phone notifications", onOff(settings.showNotifications)),
            ("Copy codes automatically", onOff(settings.copyCodesAutomatically)),
            ("Sync clipboard", onOff(settings.syncClipboard)),
            ("Open links automatically", onOff(settings.openLinksAutomatically)),
            ("Received files folder", settings.downloadDir == nil ? "Downloads" : "custom"),
            ("Muted apps", "\(settings.mutedApps.count)"),
        ]
    }

    func showLogsInFinder() {
        let existing = [LogFiles.current, LogFiles.previous].filter { FileManager.default.fileExists(atPath: $0.path) }
        if existing.isEmpty {
            workspace.open(FileManager.default.homeDirectoryForCurrentUser.appendingPathComponent("Library/Logs"))
        } else {
            workspace.reveal(existing)
        }
    }
}

// MARK: - Files

/// A storage call waiting for the core to report its transfer. The core
/// gives storage calls no transfer ID, so the first matching report is
/// taken to be theirs.
private struct ExpectedTransfer {
    let deviceId: String
    let direction: TransferDirection
    let fileName: String
    let origin: TransferOrigin
    let token: UUID
    let onMatch: @MainActor (UInt64) -> Void

    func matches(_ transfer: Transfer) -> Bool {
        transfer.deviceId == deviceId && transfer.direction == direction && transfer.fileName == fileName
    }
}

extension AppModel {
    /// Transfers for the panel.
    var listedTransfers: [TransferEntry] { transferList.listed }

    func transfer(_ id: UInt64) -> TransferEntry? { transferList.entry(id) }

    private func transferUpdated(_ transfer: Transfer) {
        // Only a transfer seen for the first time can be a storage call's.
        let origin = transferList.entry(transfer.id)?.origin ?? claimOrigin(for: transfer)
        let change = transferList.apply(transfer, origin: origin)
        guard change == .ended else { return }
        scheduleExpiry()
        guard origin == .shared else { return }
        switch transfer.state {
        case .finished(let path) where transfer.direction == .incoming:
            batcher.add(ReceivedFile(name: transfer.fileName, path: path), from: transfer.deviceId, at: .now)
        case .failed(let reason):
            if transfer.direction == .incoming { batcher.touch(transfer.deviceId, at: .now) }
            notificationCenter.show(.transferFailed(transfer, reason: reason, deviceName: name(of: transfer.deviceId)))
        default:
            break
        }
        scheduleBatchCheck()
    }

    private func claimOrigin(for transfer: Transfer) -> TransferOrigin {
        guard let index = expectedTransfers.firstIndex(where: { $0.matches(transfer) }) else { return .shared }
        let expected = expectedTransfers.remove(at: index)
        expected.onMatch(transfer.id)
        return expected.origin
    }

    /// Announces received files once their batch is complete.
    private func scheduleBatchCheck() {
        batchCheck?.cancel()
        guard let next = batcher.nextCheck else { return }
        batchCheck = Task { [weak self] in
            try? await Task.sleep(for: .seconds(max(0.05, next.timeIntervalSinceNow)))
            guard !Task.isCancelled, let self else { return }
            self.postDueBatches(now: .now)
        }
    }

    /// Exposed for tests, which pass their own clock.
    func postDueBatches(now: Date) {
        for batch in batcher.takeDue(now: now, receiving: transferList.devicesReceiving) {
            bannerSerial += 1
            notificationCenter.show(.received(batch, deviceName: name(of: batch.deviceId), serial: bannerSerial))
        }
        if !batcher.isEmpty { scheduleBatchCheck() }
    }

    private func scheduleExpiry() {
        expiryCheck?.cancel()
        guard let next = transferList.nextExpiry else { return }
        expiryCheck = Task { [weak self] in
            try? await Task.sleep(for: .seconds(max(1, next.timeIntervalSinceNow)))
            guard !Task.isCancelled, let self else { return }
            self.transferList.expire()
            self.scheduleExpiry()
        }
    }

    func clearEndedTransfers() {
        transferList.clearEnded()
    }

    func cancelTransfer(_ entry: TransferEntry) {
        guard entry.canCancel else { return }
        do {
            try backend?.cancelTransfer(entry.id, on: entry.transfer.deviceId)
        } catch {
            notice = Notice(title: "Couldn’t cancel \(entry.transfer.fileName)", message: Self.describe(error))
        }
    }

    // MARK: Shared text and links

    private func textShared(_ text: String, from deviceId: String) {
        clipboard.receive(text)
        bannerSerial += 1
        notificationCenter.show(.sharedText(text, deviceId: deviceId, deviceName: name(of: deviceId), serial: bannerSerial))
    }

    private func urlShared(_ string: String, from deviceId: String) {
        guard let url = SharedLink.openable(string) else {
            // Not something to hand to the system; it is still the user's,
            // so it goes on the clipboard like shared text.
            textShared(string, from: deviceId)
            return
        }
        let open = settings.openLinksAutomatically
        if open { workspace.open(url) }
        bannerSerial += 1
        notificationCenter.show(
            .sharedLink(url, deviceId: deviceId, deviceName: name(of: deviceId), opened: open, serial: bannerSerial))
    }

    // MARK: Sending

    /// Paired devices that are connected now: the ones files can go to.
    var sendableDevices: [Device] {
        sections.mine.filter { RowState($0) == .connected }
    }

    /// Sends the files to the device. Folders are left out with a notice:
    /// the protocol carries files only.
    func sendFiles(_ urls: [URL], to deviceId: String) {
        var folders: [String] = []
        var files: [String] = []
        for url in urls {
            var isDirectory: ObjCBool = false
            guard FileManager.default.fileExists(atPath: url.path, isDirectory: &isDirectory) else { continue }
            if isDirectory.boolValue {
                folders.append(url.lastPathComponent)
            } else {
                files.append(url.path)
            }
        }
        if !folders.isEmpty {
            notice = .foldersNotSent(folders)
        }
        guard !files.isEmpty, let backend else { return }
        do {
            try backend.sendFiles(files, to: deviceId)
        } catch LandlineError.UnknownDevice {
            notice = .deviceGone(name(of: deviceId))
        } catch {
            notice = Notice(title: "Couldn’t send files", message: Self.describe(error))
        }
    }

    // MARK: Storage

    /// The browser for a device's window, made on first use.
    func storageBrowser(for deviceId: String) -> StorageBrowserModel {
        if let browser = browsers[deviceId] { return browser }
        let browser = StorageBrowserModel(
            deviceId: deviceId, deviceName: name(of: deviceId) ?? "Phone",
            connected: connectedIds.contains(deviceId), service: self,
            downloadFolder: { [weak self] in self?.downloadFolder ?? FileManager.default.temporaryDirectory },
            reveal: { [weak self] urls in self?.workspace.reveal(urls) },
            networkBlocked: { [weak self] in self?.networkStatus == .blocked },
            cacheFolder: PreviewCache.folder(for: deviceId))
        browsers[deviceId] = browser
        return browser
    }

    /// The window closed.
    func closeStorageBrowser(for deviceId: String) {
        browsers.removeValue(forKey: deviceId)?.close()
    }

    /// Notes a storage call before making it, so its transfer is recognised
    /// when reported, and forgets it a little after the call returns. Not at
    /// once: the transfer's reports travel through the event stream and can
    /// reach the model after the call itself has returned.
    private func expectTransfer<T>(
        _ deviceId: String, _ direction: TransferDirection, fileName: String, origin: TransferOrigin,
        onTransfer: @escaping @MainActor (UInt64) -> Void, during call: () async throws -> T
    ) async throws -> T {
        let token = UUID()
        expectedTransfers.append(ExpectedTransfer(
            deviceId: deviceId, direction: direction, fileName: fileName, origin: origin, token: token,
            onMatch: onTransfer))
        defer {
            Task { [weak self] in
                try? await Task.sleep(for: .seconds(5))
                self?.expectedTransfers.removeAll { $0.token == token }
            }
        }
        return try await call()
    }

    private var storageBackend: any DeviceBackend {
        get throws {
            guard let backend else { throw LandlineError.NotReachable }
            return backend
        }
    }
}

extension AppModel: StorageService {
    func storageRoots(of deviceId: String) async throws -> [StorageRoot] {
        try await storageBackend.storageRoots(of: deviceId)
    }

    func listDir(_ path: String, on deviceId: String) async throws -> [RemoteEntry] {
        try await storageBackend.listDir(path, on: deviceId)
    }

    func download(
        _ remote: String, to local: URL, from deviceId: String, origin: TransferOrigin,
        onTransfer: @escaping @MainActor (UInt64) -> Void
    ) async throws {
        let backend = try storageBackend
        try await expectTransfer(
            deviceId, .incoming, fileName: local.lastPathComponent, origin: origin, onTransfer: onTransfer
        ) {
            try await backend.download(remote, to: local.path, from: deviceId)
        }
    }

    func upload(
        _ local: URL, to remote: String, on deviceId: String, onTransfer: @escaping @MainActor (UInt64) -> Void
    ) async throws {
        let backend = try storageBackend
        try await expectTransfer(
            deviceId, .outgoing, fileName: local.lastPathComponent, origin: .storage, onTransfer: onTransfer
        ) {
            try await backend.upload(local.path, to: remote, on: deviceId)
        }
    }
}
