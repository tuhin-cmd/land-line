import Foundation
import LandlineKit

/// What a device row is showing, decided from the core's view of the device.
/// Kept apart from the views so every state can be tested without rendering.
enum RowState: Equatable, Sendable {
    case connected
    case offline
    case available
    /// Not paired and not reachable. The core drops such devices from its
    /// list, but a stale snapshot can still hold one for a moment.
    case unavailable
    /// We asked to pair and are waiting for the phone.
    case waitingForPeer
    /// The phone asked to pair and is waiting for us.
    case askingUs

    init(_ device: Device) {
        switch device.pairState {
        case .paired: self = device.reachable ? .connected : .offline
        case .notPaired: self = device.reachable ? .available : .unavailable
        case .requested: self = .waitingForPeer
        case .requestedByPeer: self = .askingUs
        }
    }
}

enum RowAction: Hashable, Sendable {
    case pair, cancel, accept, reject, ping, unpair
}

struct DeviceRowContent: Equatable {
    let id: String
    let name: String
    let symbol: String
    let state: RowState
    let status: String
    let actions: [RowAction]
    /// The verification key in blocks of four, present while pairing is pending.
    let keyGroups: [String]?

    init(_ device: Device, pingReceived: Bool = false) {
        let state = RowState(device)
        id = device.id
        name = device.name
        symbol = device.kind.symbol
        self.state = state
        keyGroups = device.verificationKey.map(VerificationKey.groups)

        switch state {
        case .connected:
            status = pingReceived ? "Ping received" : "Connected"
            actions = [.ping, .unpair]
        case .offline:
            status = "Offline"
            actions = [.unpair]
        case .available:
            status = "Available to pair"
            actions = [.pair]
        case .unavailable:
            status = "Not reachable"
            actions = []
        case .waitingForPeer:
            status = "Waiting for approval…"
            actions = [.cancel]
        case .askingUs:
            status = "Wants to pair"
            actions = [.reject, .accept]
        }
    }
}

extension DeviceKind {
    var symbol: String {
        switch self {
        case .phone: "smartphone"
        case .tablet: "ipad.landscape"
        case .desktop: "desktopcomputer"
        case .laptop: "laptopcomputer"
        case .tv: "tv"
        }
    }
}

enum VerificationKey {
    /// Splits the key into blocks of four so it can be compared a block at a
    /// time, the way people read a phone number.
    static func groups(_ key: String) -> [String] {
        let characters = Array(key.uppercased().filter { !$0.isWhitespace })
        return stride(from: 0, to: characters.count, by: 4).map {
            String(characters[$0..<min($0 + 4, characters.count)])
        }
    }

    /// VoiceOver reads a run like "3F9A" as a word; spelling it out keeps it
    /// comparable by ear.
    static func spokenForm(_ key: String) -> String {
        groups(key).map { $0.map(String.init).joined(separator: " ") }.joined(separator: ", ")
    }
}

/// The device list split the way the popover shows it.
struct DeviceSections: Equatable {
    /// Pairing requests from phones, shown first because they expire.
    var requests: [Device] = []
    /// Paired devices, connected ones first.
    var mine: [Device] = []
    /// Everything else, including devices we are asking to pair with.
    var nearby: [Device] = []

    init(_ devices: [Device]) {
        for device in devices {
            switch device.pairState {
            case .requestedByPeer: requests.append(device)
            case .paired: mine.append(device)
            case .notPaired, .requested: nearby.append(device)
            }
        }
        requests.sort(by: Self.byName)
        mine.sort { lhs, rhs in
            lhs.reachable != rhs.reachable ? lhs.reachable : Self.byName(lhs, rhs)
        }
        nearby.sort(by: Self.byName)
    }

    var isEmpty: Bool { requests.isEmpty && mine.isEmpty && nearby.isEmpty }

    /// Finder order ("Pixel 2" before "Pixel 10"), with the ID as a tiebreak
    /// so two phones with the same name do not swap places on every update.
    private static func byName(_ lhs: Device, _ rhs: Device) -> Bool {
        switch lhs.name.localizedStandardCompare(rhs.name) {
        case .orderedAscending: true
        case .orderedDescending: false
        case .orderedSame: lhs.id < rhs.id
        }
    }
}

/// A message shown at the top of the popover until dismissed.
struct Notice: Equatable {
    let title: String
    let message: String

    static func pairingFailed(_ reason: PairFailure, deviceName: String?) -> Notice {
        let name = deviceName ?? "The device"
        switch reason {
        case .timedOut:
            return Notice(
                title: "Pairing timed out",
                message: "No answer came in time. Keep KDE Connect open on \(deviceName ?? "the phone") and try again."
            )
        case .cancelledByPeer:
            return Notice(
                title: "Pairing declined",
                message: "\(name) declined or cancelled the request."
            )
        case .clocksDoNotMatch:
            return Notice(
                title: "Clocks don’t match",
                message: "The clocks on this Mac and \(deviceName ?? "the phone") differ by more than 30 minutes. Set both to the correct time, then try again."
            )
        case .notReachable:
            return Notice(
                title: "\(name) isn’t reachable",
                message: "Check that it’s on the same Wi-Fi network as this Mac, then try again."
            )
        }
    }

    static func deviceGone(_ deviceName: String?) -> Notice {
        Notice(
            title: "\(deviceName ?? "The device") is no longer available",
            message: "It may have disconnected. Wait for it to reappear, then try again."
        )
    }
}

/// The menu bar symbol, chosen so the states that need the user show up
/// without opening the popover.
enum MenuBarIcon {
    static func symbol(devices: [Device], network: NetworkStatus?, failedToStart: Bool) -> String {
        if failedToStart || network == .blocked { return "iphone.gen3.slash" }
        if devices.contains(where: { $0.pairState == .requestedByPeer }) {
            return "iphone.gen3.badge.exclamationmark"
        }
        if devices.contains(where: { $0.pairState == .paired && $0.reachable }) {
            return "iphone.gen3.radiowaves.left.and.right"
        }
        return "iphone.gen3"
    }
}
