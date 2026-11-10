import LandlineKit
import Testing

@testable import Landline

@MainActor @Suite struct PhoneNotificationTests {
    let backend = FakeBackend()
    let services = TestServices()
    var center: FakeNotificationCenter { services.center }

    func connectedModel() -> AppModel {
        let backend = backend
        let model = AppModel(services: services.services) { backend }
        model.start()
        model.handle(.devicesChanged([device("Pixel", id: "p", .paired)]))
        return model
    }

    @Test func postedNotificationShowsABanner() {
        let model = connectedModel()
        model.handle(.notificationPosted(deviceId: "p", notification: phoneNotification("1")))
        #expect(center.shown.map(\.id) == [Banner.phoneId(deviceId: "p", notificationId: "1")])
        #expect(model.listedNotifications.map(\.notification.id) == ["1"])
    }

    @Test func updateReplacesTheBannerAndTheListEntry() {
        let model = connectedModel()
        model.handle(.notificationPosted(deviceId: "p", notification: phoneNotification("1", text: "one")))
        model.handle(.notificationPosted(deviceId: "p", notification: phoneNotification("1", text: "two")))
        #expect(center.shown.count == 2)
        #expect(center.onScreen.count == 1)
        #expect(center.onScreen.values.first?.body == "two")
        #expect(model.listedNotifications.map(\.notification.text) == ["two"])
    }

    @Test func unchangedRepostDoesNotAnnounceAgain() {
        let model = connectedModel()
        model.handle(.notificationPosted(deviceId: "p", notification: phoneNotification("1", time: 1)))
        model.handle(.notificationPosted(deviceId: "p", notification: phoneNotification("1", time: 2)))
        #expect(center.shown.count == 1)
    }

    @Test func removalTakesTheBannerDown() {
        let model = connectedModel()
        model.handle(.notificationPosted(deviceId: "p", notification: phoneNotification("1")))
        model.handle(.notificationRemoved(deviceId: "p", notificationId: "1"))
        #expect(center.onScreen.isEmpty)
        #expect(model.listedNotifications.isEmpty)
    }

    @Test func silentNotificationsAreListedButNotAnnounced() {
        let model = connectedModel()
        model.handle(.notificationPosted(deviceId: "p", notification: phoneNotification("1", silent: true)))
        #expect(center.shown.isEmpty)
        #expect(model.listedNotifications.count == 1)
    }

    @Test func listIsNewestFirstAcrossDevices() {
        let model = connectedModel()
        model.handle(.devicesChanged([device("Pixel", id: "p", .paired), device("Tab", id: "t", .paired)]))
        model.handle(.notificationPosted(deviceId: "p", notification: phoneNotification("old", time: 1)))
        model.handle(.notificationPosted(deviceId: "t", notification: phoneNotification("new", time: 5)))
        model.handle(.notificationPosted(deviceId: "p", notification: phoneNotification("mid", time: 3)))
        #expect(model.listedNotifications.map(\.notification.id) == ["new", "mid", "old"])
        #expect(model.listedNotifications.first?.deviceName == "Tab")
    }

    @Test func disconnectClearsTheDevicesBanners() {
        let model = connectedModel()
        model.handle(.notificationPosted(deviceId: "p", notification: phoneNotification("1")))
        model.handle(.notificationPosted(deviceId: "p", notification: phoneNotification("2", app: "Gmail")))
        model.handle(.devicesChanged([device("Pixel", id: "p", reachable: false, .paired)]))
        #expect(center.onScreen.isEmpty)
        #expect(model.phoneNotifications["p"] == nil)
    }

    @Test func mutedAppsAreNeitherShownNorListed() {
        let model = connectedModel()
        model.mute("WhatsApp")
        model.handle(.notificationPosted(deviceId: "p", notification: phoneNotification("1", app: "WhatsApp")))
        model.handle(.notificationPosted(deviceId: "p", notification: phoneNotification("2", app: "Gmail")))
        #expect(center.shown.map(\.title) == ["Gmail"])
        #expect(model.listedNotifications.map(\.notification.appName) == ["Gmail"])
        model.unmute("WhatsApp")
        #expect(model.listedNotifications.count == 2)
    }

    @Test func muteActionMutesTheAppAndClearsItsBanners() throws {
        let model = connectedModel()
        model.handle(.notificationPosted(deviceId: "p", notification: phoneNotification("1", app: "WhatsApp")))
        model.handle(.notificationPosted(deviceId: "p", notification: phoneNotification("2", app: "WhatsApp")))
        model.handle(.notificationPosted(deviceId: "p", notification: phoneNotification("3", app: "Gmail")))
        let banner = try #require(center.shown.first)
        center.respond(.muteApp, to: banner)
        #expect(services.settings.mutedApps == ["WhatsApp"])
        #expect(center.onScreen.values.map(\.title) == ["Gmail"])
        #expect(AppSettings(defaults: services.defaults.defaults).isMuted("WhatsApp"))
    }

    @Test func closingABannerDismissesOnThePhone() throws {
        let model = connectedModel()
        model.handle(.notificationPosted(deviceId: "p", notification: phoneNotification("1", dismissable: true)))
        center.respond(.dismissed, to: try #require(center.shown.last))
        #expect(backend.sent.map(\.0) == [.dismissNotification(id: "1")])
        #expect(backend.sent.map(\.1) == ["p"])
    }

    @Test func closingTheBannerOfAnOngoingNotificationLeavesThePhoneAlone() throws {
        let model = connectedModel()
        model.handle(.notificationPosted(deviceId: "p", notification: phoneNotification("1", dismissable: false)))
        center.respond(.dismissed, to: try #require(center.shown.last))
        center.respond(.opened, to: try #require(center.shown.last))
        #expect(backend.sent.isEmpty)
    }

    @Test func dismissingFromThePanel() {
        let model = connectedModel()
        model.handle(.notificationPosted(deviceId: "p", notification: phoneNotification("1")))
        model.handle(.notificationPosted(deviceId: "p", notification: phoneNotification("2", dismissable: false)))
        model.dismiss("1", on: "p")
        model.dismiss("2", on: "p")
        #expect(backend.sent.map(\.0) == [.dismissNotification(id: "1")])
        #expect(model.listedNotifications.map(\.notification.id) == ["2"])
        #expect(center.onScreen.keys.sorted() == [Banner.phoneId(deviceId: "p", notificationId: "2")])
    }

    @Test func turningNotificationsOffStopsAndClearsBanners() {
        let model = connectedModel()
        model.handle(.notificationPosted(deviceId: "p", notification: phoneNotification("1")))
        model.setShowNotifications(false)
        model.handle(.notificationPosted(deviceId: "p", notification: phoneNotification("2")))
        #expect(center.onScreen.isEmpty)
        #expect(center.shown.count == 1)
        #expect(model.listedNotifications.isEmpty)
        #expect(!services.settings.showNotifications)
    }

    @Test func asksForPermissionWhenAPhoneFirstConnects() async throws {
        center.currentPermission = .notAsked
        center.answer = .denied
        let model = AppModel(services: services.services) { [backend] in backend }
        model.start()
        try await eventually { model.notificationPermission != nil }
        #expect(center.requests == 0)

        model.handle(.devicesChanged([device("Pixel", id: "p", .paired)]))
        try await eventually { center.requests > 0 }
        try await eventually { model.notificationPermission == .denied }
        #expect(model.notificationPermission == .denied)

        // Never asked again, even when the phone reconnects.
        model.handle(.devicesChanged([device("Pixel", id: "p", reachable: false, .paired)]))
        model.handle(.devicesChanged([device("Pixel", id: "p", .paired)]))
        try await Task.sleep(for: .milliseconds(50))
        #expect(center.requests == 1)
    }

    @Test func doesNotAskWhenAlreadyAnswered() async throws {
        center.currentPermission = .denied
        let model = connectedModel()
        try await eventually { model.notificationPermission == .denied }
        try await Task.sleep(for: .milliseconds(50))
        #expect(center.requests == 0)
    }

    @Test func demoAndTestModelsDoNotClearRealBannersAtStart() {
        _ = connectedModel()
        #expect(center.removedAll == 0)
    }
}

@MainActor @Suite struct CodeTests {
    let backend = FakeBackend()
    let services = TestServices()
    var center: FakeNotificationCenter { services.center }
    var pasteboard: MemoryPasteboard { services.pasteboard }

    func connectedModel() -> AppModel {
        let backend = backend
        let model = AppModel(services: services.services) { backend }
        model.start()
        model.handle(.devicesChanged([device("Pixel", id: "p", .paired)]))
        return model
    }

    let otp = OtpCode(code: "482913", source: "Example Bank")

    @Test func autoCopyPutsTheCodeOnThePasteboardConcealed() throws {
        let model = connectedModel()
        model.handle(.otpDetected(deviceId: "p", otp: otp))
        #expect(pasteboard.string() == "482913")
        #expect(pasteboard.types.contains(PasteboardMarker.concealed))
        let banner = try #require(center.shown.last)
        #expect(banner.body == "482913 · Copied")
        #expect(model.latestCode?.copied == true)
        // A concealed code is never sent on to the phone.
        model.pollClipboard()
        #expect(backend.clipboardSent.isEmpty)
    }

    @Test func withoutAutoCopyTheBannerOffersCopy() throws {
        services.settings.copyCodesAutomatically = false
        let model = connectedModel()
        model.handle(.otpDetected(deviceId: "p", otp: otp))
        #expect(pasteboard.string() == nil)
        let banner = try #require(center.shown.last)
        #expect(banner.body == "482913")
        #expect(model.latestCode?.copied == false)

        center.respond(.copyCode, to: banner)
        #expect(pasteboard.string() == "482913")
        #expect(pasteboard.types.contains(PasteboardMarker.concealed))
        #expect(model.latestCode?.copied == true)
    }

    @Test func messageShownBeforeTheCodeIsReplacedByIt() {
        let model = connectedModel()
        let sms = phoneNotification("sms", app: "Messages", title: "Example Bank", text: "Your code is 482 913")
        model.handle(.notificationPosted(deviceId: "p", notification: sms))
        #expect(center.onScreen.count == 1)
        model.handle(.otpDetected(deviceId: "p", otp: otp))
        #expect(center.onScreen.values.map(\.category) == [.code])
        // Still listed in the panel; only the second banner goes.
        #expect(model.listedNotifications.map(\.notification.id) == ["sms"])
    }

    @Test func messageArrivingAfterTheCodeIsNotAnnounced() {
        let model = connectedModel()
        model.handle(.otpDetected(deviceId: "p", otp: otp))
        model.handle(.notificationPosted(
            deviceId: "p", notification: phoneNotification("sms", app: "Messages", text: "482913 is your code")))
        model.handle(.notificationPosted(
            deviceId: "p", notification: phoneNotification("other", app: "Messages", text: "See you at 4")))
        #expect(center.shown.map(\.category) == [.code, .phoneNotification(appName: "Messages")])
        #expect(center.onScreen.count == 2)
    }

    @Test func codeFromAnotherDeviceDoesNotSuppress() {
        let model = connectedModel()
        model.handle(.devicesChanged([device("Pixel", id: "p", .paired), device("Tab", id: "t", .paired)]))
        model.handle(.otpDetected(deviceId: "t", otp: otp))
        model.handle(.notificationPosted(deviceId: "p", notification: phoneNotification("x", text: "482913")))
        #expect(center.onScreen.count == 2)
    }

    @Test func codesAreAnnouncedEvenWithPhoneNotificationsOff() {
        services.settings.showNotifications = false
        let model = connectedModel()
        model.handle(.otpDetected(deviceId: "p", otp: otp))
        #expect(center.shown.map(\.category) == [.code])
    }

    @Test func copyFromThePanelMarksItCopied() {
        services.settings.copyCodesAutomatically = false
        let model = connectedModel()
        model.handle(.otpDetected(deviceId: "p", otp: otp))
        model.copyCode("482913")
        #expect(model.latestCode?.copied == true)
        #expect(pasteboard.string() == "482913")
    }
}
