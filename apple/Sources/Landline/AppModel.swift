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
    /// The name the phone sees for this Mac.
    let macName: String
    let isDemo: Bool

    @ObservationIgnored private let makeBackend: @MainActor () throws -> any DeviceBackend
    @ObservationIgnored private var backend: (any DeviceBackend)?
    @ObservationIgnored private var eventTask: Task<Void, Never>?
    @ObservationIgnored private var flashTokens: [FlashKey: UUID] = [:]

    /// How long "Ping received" stays on a row.
    static let pingDisplayTime: Duration = .seconds(4)

    init(mode: LaunchMode) {
        switch mode {
        case .live:
            macName = CoreBackend.computerName()
            isDemo = false
            makeBackend = { try CoreBackend(config: CoreBackend.defaultConfig()) }
        case .demo(let scenario):
            macName = "Demo Mac"
            isDemo = true
            makeBackend = { try DemoBackend(scenario) }
        }
    }

    /// For tests: a backend built by the caller.
    init(macName: String = "Test Mac", backend: @escaping @MainActor () throws -> any DeviceBackend) {
        self.macName = macName
        isDemo = false
        makeBackend = backend
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
        let events = backend.events
        eventTask = Task { [weak self] in
            for await event in events {
                self?.handle(event)
            }
        }
        backend.start()
        // The core sends a list on change only; a known paired device that is
        // offline would not show until something else changed.
        devices = backend.devices()
    }

    func stop() {
        backend?.stop()
        eventTask?.cancel()
        backend = nil
    }

    func handle(_ event: CoreEvent) {
        switch event {
        case .devicesChanged(let devices):
            self.devices = devices
            if let id = confirmingUnpair, !devices.contains(where: { $0.id == id && $0.pairState == .paired }) {
                confirmingUnpair = nil
            }
        case .pairingFailed(let deviceId, let reason):
            notice = .pairingFailed(reason, deviceName: name(of: deviceId))
        case .pingReceived(let deviceId):
            flash(deviceId, in: \.recentPings, for: Self.pingDisplayTime)
        case .networkStatusChanged(let status):
            networkStatus = status
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
    static func describe(_ error: any Error) -> String {
        if case LandlineError.Startup(let message) = error { return message }
        return String(describing: error)
    }
}
