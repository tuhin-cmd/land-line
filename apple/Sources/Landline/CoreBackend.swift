import AppKit
import LandlineKit
import Network
import os
import SystemConfiguration

/// The real backend: the Rust core, plus the OS signals it cannot see itself.
@MainActor
final class CoreBackend: DeviceBackend {
    let events: AsyncStream<CoreEvent>
    private let core: LandlineCore
    private let pathMonitor = NWPathMonitor()
    private var wakeObserver: (any NSObjectProtocol)?

    init(config: CoreConfig) throws {
        let (events, continuation) = AsyncStream.makeStream(of: CoreEvent.self)
        self.events = events
        // The delegate holds only the stream's continuation, never this
        // object or the core: the core keeps its delegate for life, so a
        // reference back would be a cycle.
        core = try LandlineCore(config: config, delegate: DelegateAdapter(continuation))
    }

    func start() {
        core.start()

        let core = core
        // The monitor reports the current path as soon as it starts; the core
        // has just announced itself, so only later changes matter.
        let isFirstUpdate = OSAllocatedUnfairLock(initialState: true)
        pathMonitor.pathUpdateHandler = { _ in
            let skip = isFirstUpdate.withLock { first in
                defer { first = false }
                return first
            }
            if !skip { core.networkChanged() }
        }
        pathMonitor.start(queue: DispatchQueue(label: "io.github.tuhin-cmd.landline.path"))

        // Sleep can drop every link without the interface ever going down,
        // so the path monitor alone would miss it.
        wakeObserver = NSWorkspace.shared.notificationCenter.addObserver(
            forName: NSWorkspace.didWakeNotification, object: nil, queue: nil
        ) { _ in
            core.networkChanged()
        }
    }

    func stop() {
        pathMonitor.cancel()
        if let wakeObserver {
            NSWorkspace.shared.notificationCenter.removeObserver(wakeObserver)
        }
        wakeObserver = nil
        core.stop()
    }

    func devices() -> [Device] { core.devices() }

    func send(_ command: DeviceCommand, to deviceId: String) throws {
        switch command {
        case .requestPair: try core.requestPair(deviceId: deviceId)
        case .acceptPair: try core.acceptPair(deviceId: deviceId)
        case .rejectPair: try core.rejectPair(deviceId: deviceId)
        case .unpair: try core.unpair(deviceId: deviceId)
        case .sendPing: try core.sendPing(deviceId: deviceId)
        }
    }

    /// The core's identity lives here, shared with `landline-cli` so both
    /// appear to the phone as the same device.
    static func defaultConfig() throws -> CoreConfig {
        let support = try FileManager.default.url(
            for: .applicationSupportDirectory, in: .userDomainMask, appropriateFor: nil, create: true)
        let dataDir = support.appendingPathComponent("io.github.tuhin-cmd.landline", isDirectory: true)
        return CoreConfig(dataDir: dataDir.path, deviceName: computerName(), downloadDir: nil)
    }

    /// The name set in System Settings > General > Sharing, which is what the
    /// user will look for on the phone. `Host.current()` would also give it,
    /// but it can block on DNS at launch.
    static func computerName() -> String {
        SCDynamicStoreCopyComputerName(nil, nil) as String? ?? ProcessInfo.processInfo.hostName
    }
}

/// Called on core threads. Funnels everything into one stream so the model
/// sees callbacks in the order the core made them; a task per callback would
/// not keep that order.
private final class DelegateAdapter: CoreDelegate {
    private let continuation: AsyncStream<CoreEvent>.Continuation

    init(_ continuation: AsyncStream<CoreEvent>.Continuation) {
        self.continuation = continuation
    }

    func devicesChanged(devices: [Device]) {
        continuation.yield(.devicesChanged(devices))
    }

    func pairingFailed(deviceId: String, reason: PairFailure) {
        continuation.yield(.pairingFailed(deviceId: deviceId, reason: reason))
    }

    func pingReceived(deviceId: String) {
        continuation.yield(.pingReceived(deviceId: deviceId))
    }

    func networkStatusChanged(status: NetworkStatus) {
        continuation.yield(.networkStatusChanged(status))
    }

    // Not shown in the app yet.
    func notificationPosted(deviceId: String, notification: PhoneNotification) {}
    func notificationRemoved(deviceId: String, notificationId: String) {}
    func smsReceived(deviceId: String, message: SmsMessage) {}
    func otpDetected(deviceId: String, otp: OtpCode) {}
    func clipboardReceived(deviceId: String, text: String) {}
    func transferUpdated(transfer: Transfer) {}
    func textShared(deviceId: String, text: String) {}
    func urlShared(deviceId: String, url: String) {}
}
