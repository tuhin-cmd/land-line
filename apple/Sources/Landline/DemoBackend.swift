import Foundation
import LandlineKit

/// Scripted devices for looking at the UI without a network (`--demo`).
/// Nothing here touches the core or the data directory.
@MainActor
final class DemoBackend: DeviceBackend {
    let events: AsyncStream<CoreEvent>
    private let continuation: AsyncStream<CoreEvent>.Continuation
    private let scenario: DemoScenario
    private var list: [Device]
    private let storage = DemoStorage()
    private var nextTransferId: UInt64 = 100
    private var cancelledTransfers: Set<UInt64> = []

    init(_ scenario: DemoScenario) throws {
        if scenario == .error {
            throw LandlineError.Startup(message: "The data directory is not writable (demo).")
        }
        (events, continuation) = AsyncStream.makeStream(of: CoreEvent.self)
        self.scenario = scenario
        list = Self.devices(for: scenario)
    }

    func start() {
        let status: NetworkStatus = switch scenario {
        case .blocked, .blockedConnected: .blocked
        case .offline: .offline
        default: .ready
        }
        continuation.yield(.networkStatusChanged(status))
        continuation.yield(.devicesChanged(list))
        if scenario == .failure {
            continuation.yield(.pairingFailed(deviceId: Self.tvID, reason: .timedOut))
        }
        if scenario == .ping {
            Task { [weak self] in
                while let self {
                    try? await Task.sleep(for: .seconds(3))
                    self.continuation.yield(.pingReceived(deviceId: Self.pixel.id))
                }
            }
        }
        if scenario == .notifications || scenario == .system || scenario == .crowded {
            playPhoneScript()
        }
        if scenario == .files || scenario == .crowded {
            playFileScript()
        }
    }

    /// Transfers in every state, then shared text and a link.
    private func playFileScript() {
        let phone = Self.pixel.id
        let mb: UInt64 = 1_000_000
        after(.seconds(0.5)) {
            await $0.receive("IMG_20261110_0912.jpg", from: phone, size: 6_200_000, seconds: 45)
        }
        after(.seconds(0.8)) {
            _ = await $0.play(
                $0.newTransfer("Quarterly report.key", phone, .outgoing, total: 24 * mb), seconds: 180
            ) { .finished(path: "/tmp/Quarterly report.key") }
        }
        after(.seconds(1)) {
            // Two files close together: one notification for both.
            await $0.receive("Boarding pass.pdf", from: phone, size: 210_000, seconds: 1)
            await $0.receive("Hotel booking.pdf", from: phone, size: 380_000, seconds: 1)
        }
        after(.seconds(1.2)) {
            _ = await $0.play($0.newTransfer("Holiday video.mp4", phone, .incoming, total: 1_420 * mb), seconds: 1.5) {
                .failed(reason: "The phone stopped sending the file.")
            }
        }
        after(.seconds(1.4)) {
            let transfer = $0.newTransfer("Archive.zip", phone, .outgoing, total: 64 * mb)
            $0.after(.seconds(1)) { $0.cancelledTransfers.insert(transfer.id) }
            _ = await $0.play(transfer, seconds: 20) { .finished(path: "/tmp/Archive.zip") }
        }
        after(.seconds(3)) {
            $0.continuation.yield(.textShared(deviceId: phone, text: "Gate B12 · Boarding 14:35\nSeat 23A"))
        }
        after(.seconds(5)) {
            $0.continuation.yield(.urlShared(deviceId: phone, url: "https://example.com/recipes/lentil-soup"))
        }
    }

    /// Plays the phone sharing a file: progress, then a stand-in saved in
    /// the demo's temporary folder.
    private func receive(_ name: String, from deviceId: String, size: UInt64, seconds: Double) async {
        let folder = DemoFiles.receivedFolder
        try? FileManager.default.createDirectory(at: folder, withIntermediateDirectories: true)
        let saved = UniqueName.pick(name) { FileManager.default.fileExists(atPath: folder.appendingPathComponent($0).path) }
        let local = folder.appendingPathComponent(saved)
        _ = await play(newTransfer(saved, deviceId, .incoming, total: size), seconds: seconds) {
            try DemoFiles.writeSample(named: saved, to: local)
            return .finished(path: local.path)
        }
    }

    private func newTransfer(_ name: String, _ deviceId: String, _ direction: TransferDirection, total: UInt64?) -> Transfer {
        nextTransferId += 1
        return Transfer(
            id: nextTransferId, deviceId: deviceId, fileName: name, direction: direction, bytesDone: 0,
            bytesTotal: total, state: .inProgress)
    }

    /// Reports a transfer the way the core does: at the start, four times a
    /// second while it runs, and once at the end.
    private func play(
        _ transfer: Transfer, seconds: Double, ending: @MainActor () throws -> TransferState
    ) async -> TransferState {
        var transfer = transfer
        continuation.yield(.transferUpdated(transfer))
        let ticks = max(1, Int(seconds * 4))
        for tick in 1...ticks {
            try? await Task.sleep(for: .milliseconds(250))
            if cancelledTransfers.contains(transfer.id) {
                transfer.state = .cancelled
                continuation.yield(.transferUpdated(transfer))
                return .cancelled
            }
            if tick < ticks {
                transfer.bytesDone = transfer.bytesTotal.map { $0 * UInt64(tick) / UInt64(ticks) } ?? UInt64(tick) * 300_000
                continuation.yield(.transferUpdated(transfer))
            }
        }
        do {
            transfer.state = try ending()
        } catch {
            transfer.state = .failed(reason: AppModel.describe(error))
        }
        if case .finished = transfer.state { transfer.bytesDone = transfer.bytesTotal ?? transfer.bytesDone }
        continuation.yield(.transferUpdated(transfer))
        return transfer.state
    }

    /// A few seconds of a phone's life: what was already showing when it
    /// connected, then new messages, a code, a clipboard and a removal.
    private func playPhoneScript() {
        let phone = Self.pixel.id
        for notification in Self.alreadyShowing {
            continuation.yield(.notificationPosted(deviceId: phone, notification: notification))
        }
        let script: [(Double, CoreEvent)] = [
            (2, .notificationPosted(deviceId: phone, notification: Self.message(text: "Are we still on for lunch?"))),
            // Same ID: replaces the banner rather than adding one.
            (5, .notificationPosted(deviceId: phone, notification: Self.message(text: "I can do 1 pm instead."))),
            // The code arrives both as the SMS app's notification and as a
            // detected code, the way the core reports it.
            (8, .notificationPosted(deviceId: phone, notification: Self.codeMessage)),
            (8.3, .otpDetected(deviceId: phone, otp: OtpCode(code: "482913", source: "Example Bank"))),
            (11, .clipboardReceived(deviceId: phone, text: "https://example.com/recipes/lentil-soup")),
            (14, .notificationRemoved(deviceId: phone, notificationId: "demo-chat")),
        ]
        for (seconds, event) in script {
            after(.seconds(seconds)) { $0.continuation.yield(event) }
        }
    }

    func stop() { continuation.finish() }

    func devices() -> [Device] { list }

    func send(_ command: DeviceCommand, to deviceId: String) throws {
        guard let index = list.firstIndex(where: { $0.id == deviceId }) else {
            throw LandlineError.UnknownDevice
        }
        switch command {
        case .requestPair:
            update(index) { $0.pairState = .requested; $0.verificationKey = "B71D4E20" }
            // Plays the phone accepting a few seconds later.
            after(.seconds(4)) { $0.update(deviceId) { $0.pairState = .paired; $0.verificationKey = nil } }
        case .acceptPair:
            update(index) { $0.pairState = .paired; $0.verificationKey = nil }
        case .rejectPair:
            update(index) { $0.pairState = .notPaired; $0.verificationKey = nil }
        case .unpair:
            if list[index].reachable {
                update(index) { $0.pairState = .notPaired }
            } else {
                list.remove(at: index)
                continuation.yield(.devicesChanged(list))
            }
        case .sendPing:
            // The phone pinging back stands in for the real round trip.
            after(.seconds(1)) { $0.continuation.yield(.pingReceived(deviceId: deviceId)) }
        case .dismissNotification(let id):
            // The phone confirms by removing it, as the real one does.
            after(.seconds(0.3)) {
                $0.continuation.yield(.notificationRemoved(deviceId: deviceId, notificationId: id))
            }
        }
    }

    func sendClipboard(_ text: String) {}

    /// Received files always go to the demo's temporary folder.
    func setDownloadDir(_ path: String) {}

    /// Plays sending each file in turn. Only the files' sizes are read.
    func sendFiles(_ paths: [String], to deviceId: String) throws {
        guard list.contains(where: { $0.id == deviceId }) else { throw LandlineError.UnknownDevice }
        let transfers = paths.map { path in
            let size = (try? FileManager.default.attributesOfItem(atPath: path)[.size] as? UInt64) ?? nil
            return (newTransfer((path as NSString).lastPathComponent, deviceId, .outgoing, total: size ?? 0), path)
        }
        Task {
            for (transfer, path) in transfers {
                let seconds = min(8, max(1.5, Double(transfer.bytesTotal ?? 0) / 20_000_000))
                _ = await play(transfer, seconds: seconds) { .finished(path: path) }
            }
        }
    }

    func cancelTransfer(_ transferId: UInt64, on deviceId: String) throws {
        cancelledTransfers.insert(transferId)
    }

    // MARK: Storage

    private func checkStorage(_ deviceId: String) throws {
        // Like the real core without Local Network access: the phone can
        // reach the Mac, but every connection the Mac opens fails.
        if scenario == .blocked || scenario == .blockedConnected { throw LandlineError.NotReachable }
        guard let device = list.first(where: { $0.id == deviceId }) else { throw LandlineError.UnknownDevice }
        guard device.reachable, device.pairState == .paired else { throw LandlineError.NotReachable }
    }

    func storageRoots(of deviceId: String) async throws -> [StorageRoot] {
        try checkStorage(deviceId)
        try await Task.sleep(for: .milliseconds(400))
        return storage.roots
    }

    func listDir(_ path: String, on deviceId: String) async throws -> [RemoteEntry] {
        try checkStorage(deviceId)
        let entries = try await storage.list(path)
        try checkStorage(deviceId)
        return entries
    }

    func download(_ remote: String, to local: String, from deviceId: String) async throws {
        try checkStorage(deviceId)
        guard let entry = storage.entry(at: remote) else {
            throw LandlineError.Storage(message: "“\(RemotePath.name(of: remote))” no longer exists on the phone.")
        }
        let url = URL(fileURLWithPath: local)
        guard DemoFiles.isWritable(url) else {
            throw LandlineError.Storage(message: "Demo mode saves files only inside its temporary folder.")
        }
        let seconds = min(6, max(0.8, Double(entry.size) / 25_000_000))
        let state = await play(newTransfer(url.lastPathComponent, deviceId, .incoming, total: entry.size), seconds: seconds) {
            try DemoFiles.writeSample(named: entry.name, to: url)
            return .finished(path: local)
        }
        if case .failed(let reason) = state { throw LandlineError.Storage(message: reason) }
        if state == .cancelled { throw LandlineError.Cancelled }
    }

    func upload(_ local: String, to remote: String, on deviceId: String) async throws {
        try checkStorage(deviceId)
        let size = (try? FileManager.default.attributesOfItem(atPath: local)[.size] as? UInt64) ?? 0
        let name = (local as NSString).lastPathComponent
        let seconds = min(6, max(0.8, Double(size) / 25_000_000))
        let state = await play(newTransfer(name, deviceId, .outgoing, total: size), seconds: seconds) {
            .finished(path: local)
        }
        if state == .cancelled { throw LandlineError.Cancelled }
        let entry = RemoteEntry(
            name: RemotePath.name(of: remote), path: remote, isDir: false, size: size,
            modifiedSecs: Int64(Date.now.timeIntervalSince1970))
        storage.add(entry, to: (remote as NSString).deletingLastPathComponent)
    }

    private func update(_ index: Int, _ change: (inout Device) -> Void) {
        change(&list[index])
        continuation.yield(.devicesChanged(list))
    }

    private func update(_ deviceId: String, _ change: (inout Device) -> Void) {
        if let index = list.firstIndex(where: { $0.id == deviceId }) { update(index, change) }
    }

    private func after(_ delay: Duration, _ action: @escaping @MainActor (DemoBackend) async -> Void) {
        Task { [weak self] in
            try? await Task.sleep(for: delay)
            if let self { await action(self) }
        }
    }

    // Made-up IDs, and models chosen not to match the owner’s real phone so
    // a demo pairing request is never mistaken for a real one.
    private static let pixel = Device(
        id: "demo0000000000000000000000000001", name: "Pixel 8a", kind: .phone,
        reachable: true, pairState: .paired, verificationKey: nil, battery: Battery(charge: 82, charging: false))

    private static let start = Int64(Date.now.timeIntervalSince1970 * 1000)

    private static let alreadyShowing = [
        PhoneNotification(
            id: "demo-calendar", appName: "Calendar", title: "Design review",
            text: "In 15 minutes · Room 4", timeMs: start - 20 * 60_000, dismissable: true, silent: true,
            iconPath: nil),
        PhoneNotification(
            id: "demo-mail", appName: "Gmail", title: "3 new messages",
            text: "Sam Rivera, Ana, Weekly digest", timeMs: start - 42 * 60_000, dismissable: true, silent: true,
            iconPath: nil),
    ]

    private static func message(text: String) -> PhoneNotification {
        PhoneNotification(
            id: "demo-chat", appName: "WhatsApp", title: "Maya", text: text,
            timeMs: Int64(Date.now.timeIntervalSince1970 * 1000), dismissable: true, silent: false, iconPath: nil)
    }

    private static var codeMessage: PhoneNotification {
        PhoneNotification(
            id: "demo-sms", appName: "Messages", title: "Example Bank",
            text: "482 913 is your Example Bank verification code. Do not share it.",
            timeMs: Int64(Date.now.timeIntervalSince1970 * 1000), dismissable: true, silent: false, iconPath: nil)
    }

    private static let tvID = "demo0000000000000000000000000005"

    private static func devices(for scenario: DemoScenario) -> [Device] {
        let tablet = Device(
            id: "demo0000000000000000000000000002", name: "Galaxy Tab S9", kind: .tablet,
            reachable: false, pairState: .paired, verificationKey: nil, battery: nil)
        switch scenario {
        case .searching, .error: return []
        case .blocked, .offline: return [Device(pixel, reachable: false), tablet]
        case .ping, .notifications, .system, .files, .blockedConnected: return [pixel]
        case .crowded:
            let names = ["Pixel 7", "Galaxy S23", "Moto G84", "Xperia 10 V", "Redmi Note 13"]
            let extra = names.enumerated().map { index, name in
                Device(
                    id: "demo00000000000000000000000001\(index + 10)", name: name, kind: .phone,
                    reachable: index % 2 == 0, pairState: index < 3 ? .paired : .notPaired, verificationKey: nil,
                    battery: index % 2 == 0 ? Battery(charge: UInt8(30 + index * 15), charging: index == 2) : nil)
            }
            return [pixel, tablet] + extra
        case .all, .failure:
            return [
                pixel, tablet,
                Device(
                    id: "demo0000000000000000000000000003", name: "Fairphone 5", kind: .phone,
                    reachable: true, pairState: .requestedByPeer, verificationKey: "3F9A0C7E", battery: nil),
                Device(
                    id: "demo0000000000000000000000000004", name: "Nokia G42", kind: .phone,
                    reachable: true, pairState: .requested, verificationKey: "B71D4E20", battery: nil),
                Device(
                    id: tvID, name: "Living Room TV", kind: .tv,
                    reachable: true, pairState: .notPaired, verificationKey: nil, battery: nil),
                Device(
                    id: "demo0000000000000000000000000006", name: "Work Laptop", kind: .laptop,
                    reachable: true, pairState: .notPaired, verificationKey: nil, battery: nil),
            ]
        }
    }
}

private extension Device {
    init(_ device: Device, reachable: Bool) {
        self = device
        self.reachable = reachable
    }
}
