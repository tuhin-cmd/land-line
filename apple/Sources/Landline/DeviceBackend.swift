import LandlineKit

/// Something the model can drive: the real core, or the scripted demo.
@MainActor
protocol DeviceBackend: AnyObject {
    /// Every delegate callback, in the order the core made them.
    var events: AsyncStream<CoreEvent> { get }
    func start()
    func stop()
    func devices() -> [Device]
    func send(_ command: DeviceCommand, to deviceId: String) throws
}

enum CoreEvent: Sendable {
    case devicesChanged([Device])
    case pairingFailed(deviceId: String, reason: PairFailure)
    case pingReceived(deviceId: String)
    case networkStatusChanged(NetworkStatus)
}

enum DeviceCommand: Sendable {
    case requestPair, acceptPair, rejectPair, unpair, sendPing
}

/// How the app was launched. `--demo` skips the core so the UI can be looked
/// at without a network and without a second instance using the same
/// identity as a running one; `--demo=<scenario>` picks a starting state.
enum LaunchMode: Equatable {
    case live
    case demo(DemoScenario)

    init(arguments: [String]) {
        self = .live
        for argument in arguments.dropFirst() {
            if argument == "--demo" {
                self = .demo(.all)
            } else if argument.hasPrefix("--demo=") {
                let name = argument.dropFirst("--demo=".count)
                self = .demo(DemoScenario(rawValue: String(name)) ?? .all)
            }
        }
    }
}

enum DemoScenario: String, CaseIterable, Sendable {
    /// One device in every state.
    case all
    /// No devices yet.
    case searching
    /// Local Network permission missing.
    case blocked
    /// No usable network interface.
    case offline
    /// Every state plus a pairing failure banner.
    case failure
    /// The core failed to start.
    case error
    /// A paired phone that pings the Mac every few seconds.
    case ping
}
