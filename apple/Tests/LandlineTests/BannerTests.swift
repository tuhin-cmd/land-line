import Foundation
import LandlineKit
import Testing

@testable import Landline

@Suite struct BannerTests {
    @Test func phoneNotificationReadsAppThenTitleThenText() {
        let banner = Banner.phone(
            phoneNotification("42", app: "WhatsApp", title: "Maya", text: "Lunch?", icon: "/tmp/icon.png"),
            deviceId: "dev")
        #expect(banner.title == "WhatsApp")
        #expect(banner.subtitle == "Maya")
        #expect(banner.body == "Lunch?")
        #expect(banner.iconPath == "/tmp/icon.png")
        #expect(banner.category == .phoneNotification(appName: "WhatsApp"))
        #expect(banner.category.identifier == "phone/WhatsApp")
    }

    @Test func idComesFromDeviceAndNotification() {
        let first = Banner.phone(phoneNotification("42", text: "one"), deviceId: "dev")
        let update = Banner.phone(phoneNotification("42", text: "two"), deviceId: "dev")
        let otherDevice = Banner.phone(phoneNotification("42"), deviceId: "dev2")
        #expect(first.id == update.id)
        #expect(first.id != otherDevice.id)
        #expect(first.id == Banner.phoneId(deviceId: "dev", notificationId: "42"))
    }

    @Test func threadGroupsByPhoneApp() {
        let chat = Banner.phone(phoneNotification("1", app: "WhatsApp"), deviceId: "dev")
        let chat2 = Banner.phone(phoneNotification("2", app: "WhatsApp"), deviceId: "dev")
        let mail = Banner.phone(phoneNotification("3", app: "Gmail"), deviceId: "dev")
        #expect(chat.threadId == chat2.threadId)
        #expect(chat.threadId != mail.threadId)
    }

    @Test func titleRepeatingTheAppNameIsDropped() {
        let banner = Banner.phone(phoneNotification(app: "Gmail", title: "gmail", text: "2 new"), deviceId: "d")
        #expect(banner.title == "Gmail")
        #expect(banner.subtitle.isEmpty)
        #expect(banner.body == "2 new")
    }

    @Test func titleMovesIntoAnEmptyBody() {
        let banner = Banner.phone(phoneNotification(app: "Phone", title: "Missed call", text: " "), deviceId: "d")
        #expect(banner.subtitle.isEmpty)
        #expect(banner.body == "Missed call")
    }

    @Test func userInfoCarriesWhatTheActionsNeed() {
        let banner = Banner.phone(phoneNotification("7", app: "Chat", dismissable: false), deviceId: "dev")
        #expect(banner.userInfo[BannerInfo.deviceId] == "dev")
        #expect(banner.userInfo[BannerInfo.notificationId] == "7")
        #expect(banner.userInfo[BannerInfo.appName] == "Chat")
        #expect(banner.userInfo[BannerInfo.dismissable] == "0")
    }

    @Test func codeBanner() {
        let otp = OtpCode(code: "482913", source: "Example Bank")
        let copied = Banner.code(otp, deviceId: "dev", copied: true, serial: 3)
        #expect(copied.title == "Code from Example Bank")
        #expect(copied.body == "482913 · Copied")
        #expect(copied.category == .code)
        #expect(copied.userInfo[BannerInfo.code] == "482913")
        // The system logs request IDs, so the code must not be in one.
        #expect(!copied.id.contains("482913"))

        let notCopied = Banner.code(otp, deviceId: "dev", copied: false, serial: 4)
        #expect(notCopied.body == "482913")
        #expect(notCopied.id != copied.id)
        #expect(Banner.code(OtpCode(code: "1", source: ""), deviceId: "d", copied: false, serial: 1).title
            == "One-time code")
    }
}

@Suite struct CodeMatchTests {
    @Test(arguments: [
        "Your code is 482913.",
        "482913 is your verification code",
        "Code: 482 913",
        "Code: 482-913",
        "কোড ৪৮২৯১৩",
        "Use 482913 2 times",
    ])
    func findsTheCode(text: String) {
        #expect(CodeMatch.text(text, contains: "482913"))
    }

    @Test(arguments: [
        "Account 1234829130 credited",
        "Your code is 48291",
        "Balance Tk 482,913.00",
        "",
    ])
    func doesNotFindIt(text: String) {
        #expect(!CodeMatch.text(text, contains: "482913"))
    }

    @Test func emptyCodeNeverMatches() {
        #expect(!CodeMatch.text("123", contains: ""))
    }
}

@Suite struct IconStagingTests {
    @Test func copiesToAFreshPNG() throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: directory) }
        let source = directory.appendingPathComponent("source")
        try FileManager.default.createDirectory(at: directory, withIntermediateDirectories: true)
        try Data([1, 2, 3]).write(to: source)

        let first = try IconStaging.copy(source.path, into: directory.appendingPathComponent("staged"))
        let second = try IconStaging.copy(source.path, into: directory.appendingPathComponent("staged"))
        #expect(first.pathExtension == "png")
        #expect(first != second)
        #expect(try Data(contentsOf: first) == Data([1, 2, 3]))
        // The original stays where the core put it.
        #expect(FileManager.default.fileExists(atPath: source.path))
    }
}
