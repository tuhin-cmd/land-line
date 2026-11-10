import LandlineKit

/// Scripted devices for looking at the UI without a network (`--demo`).
/// Nothing here touches the core or the data directory.
@MainActor
final class DemoBackend: DeviceBackend {
    let events: AsyncStream<CoreEvent>
    private let continuation: AsyncStream<CoreEvent>.Continuation
    private let scenario: DemoScenario
    private var list: [Device]

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
        case .blocked: .blocked
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
        }
    }

    private func update(_ index: Int, _ change: (inout Device) -> Void) {
        change(&list[index])
        continuation.yield(.devicesChanged(list))
    }

    private func update(_ deviceId: String, _ change: (inout Device) -> Void) {
        if let index = list.firstIndex(where: { $0.id == deviceId }) { update(index, change) }
    }

    private func after(_ delay: Duration, _ action: @escaping @MainActor (DemoBackend) -> Void) {
        Task { [weak self] in
            try? await Task.sleep(for: delay)
            if let self { action(self) }
        }
    }

    // Made-up IDs, and models chosen not to match the owner’s real phone so
    // a demo pairing request is never mistaken for a real one.
    private static let pixel = Device(
        id: "demo0000000000000000000000000001", name: "Pixel 8a", kind: .phone,
        reachable: true, pairState: .paired, verificationKey: nil, battery: nil)

    private static let tvID = "demo0000000000000000000000000005"

    private static func devices(for scenario: DemoScenario) -> [Device] {
        let tablet = Device(
            id: "demo0000000000000000000000000002", name: "Galaxy Tab S9", kind: .tablet,
            reachable: false, pairState: .paired, verificationKey: nil, battery: nil)
        switch scenario {
        case .searching, .error: return []
        case .blocked, .offline: return [Device(pixel, reachable: false), tablet]
        case .ping: return [pixel]
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
