import Foundation
import LandlineKit
import Testing

/// Checks that the Rust core is reachable from Swift through the generated
/// bindings.
@Suite struct BridgeTests {
    @Test func versionIsReported() {
        #expect(coreVersion().split(separator: ".").count == 3)
    }

    @Test func coreStartsWithNoDevices() throws {
        let dataDir = FileManager.default.temporaryDirectory
            .appendingPathComponent("landline-tests-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: dataDir) }

        let config = CoreConfig(dataDir: dataDir.path, deviceName: "Test Mac", downloadDir: nil)
        let core = try LandlineCore(config: config, delegate: SilentDelegate())
        #expect(core.devices().isEmpty)
    }

    @Test func commandsOnUnknownDevicesThrow() throws {
        let config = CoreConfig(dataDir: NSTemporaryDirectory(), deviceName: "Test Mac", downloadDir: nil)
        let core = try LandlineCore(config: config, delegate: SilentDelegate())
        #expect(throws: LandlineError.UnknownDevice) {
            try core.requestPair(deviceId: "0123456789abcdef0123456789abcdef")
        }
    }
}

private final class SilentDelegate: CoreDelegate {
    func devicesChanged(devices: [Device]) {}
    func pairingFailed(deviceId: String, reason: PairFailure) {}
    func pingReceived(deviceId: String) {}
    func networkStatusChanged(status: NetworkStatus) {}
    func notificationPosted(deviceId: String, notification: PhoneNotification) {}
    func notificationRemoved(deviceId: String, notificationId: String) {}
    func smsReceived(deviceId: String, message: SmsMessage) {}
    func otpDetected(deviceId: String, otp: OtpCode) {}
    func clipboardReceived(deviceId: String, text: String) {}
    func transferUpdated(transfer: Transfer) {}
    func textShared(deviceId: String, text: String) {}
    func urlShared(deviceId: String, url: String) {}
}
