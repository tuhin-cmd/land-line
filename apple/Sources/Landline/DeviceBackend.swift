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
    /// Offers the text to every paired, connected device.
    func sendClipboard(_ text: String)
    /// Where files from a device are saved from now on.
    func setDownloadDir(_ path: String)
    /// Sends files, one after another. Progress arrives as `transferUpdated`.
    func sendFiles(_ paths: [String], to deviceId: String) throws
    /// Stops a shared file. Storage downloads and uploads cannot be stopped.
    func cancelTransfer(_ transferId: UInt64, on deviceId: String) throws

    // Storage. These run on the core's own threads; awaiting one does not
    // block the main actor. Cancelling the awaiting task does not stop the
    // work: the core finishes it regardless.
    func storageRoots(of deviceId: String) async throws -> [StorageRoot]
    func listDir(_ path: String, on deviceId: String) async throws -> [RemoteEntry]
    /// Copies a file from the device. The core writes to a hidden temporary
    /// file next to `local` and renames it, replacing anything there.
    func download(_ remote: String, to local: String, from deviceId: String) async throws
    func upload(_ local: String, to remote: String, on deviceId: String) async throws
}

enum CoreEvent: Sendable {
    case devicesChanged([Device])
    case pairingFailed(deviceId: String, reason: PairFailure)
    case pingReceived(deviceId: String)
    case networkStatusChanged(NetworkStatus)
    case notificationPosted(deviceId: String, notification: PhoneNotification)
    case notificationRemoved(deviceId: String, notificationId: String)
    case smsReceived(deviceId: String, message: SmsMessage)
    case otpDetected(deviceId: String, otp: OtpCode)
    case clipboardReceived(deviceId: String, text: String)
    case transferUpdated(Transfer)
    case textShared(deviceId: String, text: String)
    case urlShared(deviceId: String, url: String)
}

enum DeviceCommand: Equatable, Sendable {
    case requestPair, acceptPair, rejectPair, unpair, sendPing
    case dismissNotification(id: String)
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
    /// A paired phone that posts, updates and removes notifications, sends a
    /// one-time code and its clipboard. Banners and the pasteboard are
    /// simulated inside the app.
    case notifications
    /// The same script, but shown with real banners and written to the real
    /// pasteboard, to check the system side by hand.
    case system
    /// A connected phone that shares files, text and a link, with transfers
    /// in every state. Browse Files shows a made-up storage tree.
    case files
    /// Local Network permission missing while a phone is connected anyway:
    /// the phone can reach the Mac, but not the other way round.
    case blockedConnected = "blocked-connected"
    /// Enough devices and notifications to need scrolling.
    case crowded
}
