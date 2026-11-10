import LandlineKit
import Testing

@testable import Landline

/// Records commands and lets the test play the core's callbacks.
@MainActor
final class FakeBackend: DeviceBackend {
    let events: AsyncStream<CoreEvent>
    let continuation: AsyncStream<CoreEvent>.Continuation
    var sent: [(DeviceCommand, String)] = []
    var clipboardSent: [String] = []
    var downloadDirs: [String] = []
    var initial: [Device] = []
    var failure: (any Error)?

    init() {
        (events, continuation) = AsyncStream.makeStream(of: CoreEvent.self)
    }

    func start() {}
    func stop() {}
    func devices() -> [Device] { initial }

    func send(_ command: DeviceCommand, to deviceId: String) throws {
        if let failure { throw failure }
        sent.append((command, deviceId))
    }

    func sendClipboard(_ text: String) { clipboardSent.append(text) }
    func setDownloadDir(_ path: String) { downloadDirs.append(path) }

    var filesSent: [([String], String)] = []
    var cancelled: [UInt64] = []
    /// Plays the core during a storage call, such as reporting a transfer.
    var onStorageCall: (@MainActor (String) -> Void)?

    func sendFiles(_ paths: [String], to deviceId: String) throws {
        if let failure { throw failure }
        filesSent.append((paths, deviceId))
    }

    func cancelTransfer(_ transferId: UInt64, on deviceId: String) throws { cancelled.append(transferId) }

    func storageRoots(of deviceId: String) async throws -> [StorageRoot] { [] }
    func listDir(_ path: String, on deviceId: String) async throws -> [RemoteEntry] { [] }

    func download(_ remote: String, to local: String, from deviceId: String) async throws {
        onStorageCall?(local)
        if let failure { throw failure }
    }

    func upload(_ local: String, to remote: String, on deviceId: String) async throws {
        onStorageCall?(local)
        if let failure { throw failure }
    }
}

@MainActor @Suite struct AppModelTests {
    let backend = FakeBackend()
    let services = TestServices()

    func startedModel() -> AppModel {
        let backend = backend
        let model = AppModel(services: services.services) { backend }
        model.start()
        return model
    }

    @Test func seedsDevicesFromTheBackendOnStart() {
        backend.initial = [device("Phone", reachable: false, .paired)]
        let model = startedModel()
        #expect(model.devices.map(\.name) == ["Phone"])
    }

    @Test func startFailureIsShownNotThrown() {
        let model = AppModel(services: services.services) { throw LandlineError.Startup(message: "disk full") }
        model.start()
        #expect(model.startError == "disk full")
        #expect(model.menuBarSymbol == "iphone.gen3.slash")
    }

    @Test func mirrorsCallbacks() {
        let model = startedModel()
        model.handle(.networkStatusChanged(.blocked))
        model.handle(.devicesChanged([device("Phone", .requestedByPeer, key: "AAAABBBB")]))
        #expect(model.networkStatus == .blocked)
        #expect(model.sections.requests.count == 1)
    }

    @Test func pairingFailureBecomesANoticeNamingTheDevice() {
        let model = startedModel()
        model.handle(.devicesChanged([device("Pixel", id: "p")]))
        model.handle(.pairingFailed(deviceId: "p", reason: .notReachable))
        #expect(model.notice?.title == "Pixel isn’t reachable")
    }

    @Test func actionsMapToCoreCommands() {
        let model = startedModel()
        model.perform(.pair, on: "a")
        model.perform(.accept, on: "b")
        model.perform(.reject, on: "c")
        model.perform(.cancel, on: "d")
        model.perform(.ping, on: "e")
        let sent = backend.sent.map { "\($0.0):\($0.1)" }
        #expect(sent == ["requestPair:a", "acceptPair:b", "rejectPair:c", "rejectPair:d", "sendPing:e"])
        #expect(model.sentPings == ["e"])
    }

    @Test func unpairAsksFirst() {
        let model = startedModel()
        model.handle(.devicesChanged([device("Phone", id: "p", .paired)]))
        model.perform(.unpair, on: "p")
        #expect(backend.sent.isEmpty)
        #expect(model.confirmingUnpair == "p")
        model.confirmUnpair()
        #expect(backend.sent.map { "\($0.0):\($0.1)" } == ["unpair:p"])
        #expect(model.confirmingUnpair == nil)
    }

    @Test func confirmationClosesWhenTheDeviceIsNoLongerPaired() {
        let model = startedModel()
        model.handle(.devicesChanged([device("Phone", id: "p", .paired)]))
        model.perform(.unpair, on: "p")
        model.handle(.devicesChanged([device("Phone", id: "p", .notPaired)]))
        #expect(model.confirmingUnpair == nil)
    }

    @Test func vanishedDeviceGivesANotice() {
        let model = startedModel()
        model.handle(.devicesChanged([device("Pixel", id: "p")]))
        backend.failure = LandlineError.UnknownDevice
        model.perform(.pair, on: "p")
        #expect(model.notice == .deviceGone("Pixel"))
    }

    @Test func pairingAgainClearsTheOldFailure() {
        let model = startedModel()
        model.handle(.pairingFailed(deviceId: "p", reason: .timedOut))
        #expect(model.notice != nil)
        model.perform(.pair, on: "p")
        #expect(model.notice == nil)
    }

    @Test func receivedPingShowsThenClears() async throws {
        let model = startedModel()
        model.handle(.pingReceived(deviceId: "p"))
        #expect(model.recentPings == ["p"])
        try await Task.sleep(for: AppModel.pingDisplayTime + .milliseconds(300))
        #expect(model.recentPings.isEmpty)
    }

    @Test func eventsFromTheStreamReachTheModel() async throws {
        let model = startedModel()
        backend.continuation.yield(.devicesChanged([device("Phone")]))
        for _ in 0..<50 where model.devices.isEmpty {
            try await Task.sleep(for: .milliseconds(10))
        }
        #expect(model.devices.map(\.name) == ["Phone"])
    }
}
