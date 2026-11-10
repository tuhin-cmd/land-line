import Foundation
import Testing

@testable import Landline

@MainActor @Suite struct SettingsTests {
    let store = TestDefaults()

    @Test func defaultsAreOn() {
        let settings = AppSettings(defaults: store.defaults)
        #expect(settings.showNotifications)
        #expect(settings.copyCodesAutomatically)
        #expect(settings.syncClipboard)
        #expect(settings.mutedApps.isEmpty)
        #expect(settings.downloadDir == nil)
        #expect(!settings.openLinksAutomatically)
    }

    @Test func openingLinksAutomaticallyIsRemembered() {
        AppSettings(defaults: store.defaults).openLinksAutomatically = true
        #expect(AppSettings(defaults: store.defaults).openLinksAutomatically)
    }

    @Test func choicesSurviveARelaunch() {
        let settings = AppSettings(defaults: store.defaults)
        settings.showNotifications = false
        settings.copyCodesAutomatically = false
        settings.syncClipboard = false
        settings.downloadDir = "/Users/test/Received"
        settings.mute("WhatsApp")

        let again = AppSettings(defaults: store.defaults)
        #expect(!again.showNotifications)
        #expect(!again.copyCodesAutomatically)
        #expect(!again.syncClipboard)
        #expect(again.downloadDir == "/Users/test/Received")
        #expect(again.mutedApps == ["WhatsApp"])
    }

    @Test func muteListIsSortedAndHasNoRepeats() {
        let settings = AppSettings(defaults: store.defaults)
        settings.mute("Zoom")
        settings.mute("gmail")
        settings.mute("Zoom")
        settings.mute("")
        #expect(settings.mutedApps == ["gmail", "Zoom"])
        #expect(settings.isMuted("Zoom"))
        settings.unmute("Zoom")
        #expect(settings.mutedApps == ["gmail"])
        #expect(AppSettings(defaults: store.defaults).mutedApps == ["gmail"])
    }

    @Test func resettingTheFolderForgetsIt() {
        let settings = AppSettings(defaults: store.defaults)
        settings.downloadDir = "/tmp/x"
        settings.downloadDir = nil
        #expect(AppSettings(defaults: store.defaults).downloadDir == nil)
    }
}
