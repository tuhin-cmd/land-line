import Foundation
import LandlineKit
import Testing

@testable import Landline

/// Receiving, sending and sharing, through the model.
@MainActor @Suite struct FileTests {
    let backend = FakeBackend()
    let services = TestServices()

    func startedModel() -> AppModel {
        let backend = backend
        let model = AppModel(services: services.services) { backend }
        model.start()
        model.handle(.devicesChanged([device("Pixel", id: "p", reachable: true, .paired)]))
        return model
    }

    var shown: [Banner] { services.center.shown }

    @Test func receivedFilesAreAnnouncedOnceTheBatchIsComplete() {
        let model = startedModel()
        model.handle(.transferUpdated(transfer(1, "a.pdf")))
        model.handle(.transferUpdated(transfer(1, "a.pdf", .finished(path: "/d/a.pdf"))))
        model.handle(.transferUpdated(transfer(2, "b.pdf")))
        model.handle(.transferUpdated(transfer(2, "b.pdf", .finished(path: "/d/b.pdf"))))
        model.postDueBatches(now: .now)
        #expect(shown.isEmpty)

        model.postDueBatches(now: .now.addingTimeInterval(ReceivedBatcher.quietPeriod + 1))
        #expect(shown.map(\.title) == ["Received 2 files"])
        #expect(shown.first?.body == "a.pdf and b.pdf from Pixel")
    }

    @Test func aFileStillArrivingHoldsBackTheNotification() {
        let model = startedModel()
        model.handle(.transferUpdated(transfer(1, "a.pdf", .finished(path: "/d/a.pdf"))))
        model.handle(.transferUpdated(transfer(2, "big.mov")))
        model.postDueBatches(now: .now.addingTimeInterval(60))
        #expect(shown.isEmpty)
    }

    @Test func failuresAreQuiet() {
        let model = startedModel()
        model.handle(.transferUpdated(transfer(1, "a.pdf", .failed(reason: "The phone stopped sending the file."))))
        #expect(shown.count == 1)
        #expect(shown.first?.title == "Couldn’t receive a.pdf")
        #expect(shown.first?.body == "The phone stopped sending the file.")
        #expect(shown.first?.quiet == true)
        #expect(shown.first?.category == .info)
    }

    @Test func storageDownloadsAreRecognisedAndNotAnnounced() async throws {
        let model = startedModel()
        let local = services.folder.url.appendingPathComponent("photo.jpg")
        var reported: UInt64?
        backend.onStorageCall = { [model] path in
            let name = (path as NSString).lastPathComponent
            model.handle(.transferUpdated(transfer(7, name)))
            model.handle(.transferUpdated(transfer(7, name, .finished(path: path))))
        }
        try await model.download("/sdcard/photo.jpg", to: local, from: "p", origin: .storage) { reported = $0 }
        model.postDueBatches(now: .now.addingTimeInterval(60))
        #expect(reported == 7)
        #expect(shown.isEmpty)
        #expect(model.transfer(7)?.origin == .storage)
    }

    @Test func aTransferReportedAfterTheCallReturnedIsStillRecognised() async throws {
        let model = startedModel()
        let local = services.folder.url.appendingPathComponent("late.jpg")
        try await model.download("/sdcard/late.jpg", to: local, from: "p", origin: .preview) { _ in }
        model.handle(.transferUpdated(transfer(9, "late.jpg", .finished(path: local.path))))
        #expect(model.transfer(9)?.origin == .preview)
        #expect(model.listedTransfers.isEmpty)
    }

    @Test func cancelGoesToTheCoreForSharedFilesOnly() {
        let model = startedModel()
        model.handle(.transferUpdated(transfer(1, "a.pdf")))
        model.cancelTransfer(model.transfer(1)!)
        #expect(backend.cancelled == [1])
    }

    @Test func clearRemovesEndedTransfersOnly() {
        let model = startedModel()
        model.handle(.transferUpdated(transfer(1)))
        model.handle(.transferUpdated(transfer(2, .cancelled)))
        model.clearEndedTransfers()
        #expect(model.listedTransfers.map(\.id) == [1])
    }

    // MARK: Shared text and links

    @Test func sharedTextGoesToTheClipboardAndIsNotSentBack() {
        let model = startedModel()
        model.handle(.textShared(deviceId: "p", text: "Gate B12\nSeat 23A"))
        #expect(services.pasteboard.string() == "Gate B12\nSeat 23A")
        #expect(shown.first?.title == "Copied text from Pixel")
        #expect(shown.first?.body == "Gate B12")
        model.pollClipboard()
        #expect(backend.clipboardSent.isEmpty)
    }

    @Test func linksWaitForTheUserByDefault() {
        let model = startedModel()
        #expect(!services.settings.openLinksAutomatically)
        model.handle(.urlShared(deviceId: "p", url: "https://example.com/soup"))
        #expect(services.workspace.opened.isEmpty)
        let banner = try! #require(shown.first)
        #expect(banner.category == .link)
        #expect(banner.title == "Link from Pixel")

        services.center.respond(.openLink, to: banner)
        #expect(services.workspace.opened == [URL(string: "https://example.com/soup")!])
    }

    @Test func linksOpenAutomaticallyWhenAllowed() {
        let model = startedModel()
        services.settings.openLinksAutomatically = true
        model.handle(.urlShared(deviceId: "p", url: "mailto:sam@example.com"))
        #expect(services.workspace.opened == [URL(string: "mailto:sam@example.com")!])
        #expect(shown.first?.title == "Opened a link from Pixel")
        #expect(shown.first?.category == .info)
    }

    @Test func otherSchemesAreNeverOpened() {
        let model = startedModel()
        services.settings.openLinksAutomatically = true
        model.handle(.urlShared(deviceId: "p", url: "file:///etc/hosts"))
        #expect(services.workspace.opened.isEmpty)
        #expect(services.pasteboard.string() == "file:///etc/hosts")
        #expect(shown.first?.category == .info)
    }

    @Test func receivedBannersShowAndOpenFilesThatStillExist() {
        let model = startedModel()
        let kept = services.folder.file("kept.pdf")
        let banner = Banner.received(
            ReceivedBatch(deviceId: "p", files: [
                ReceivedFile(name: "kept.pdf", path: kept.path),
                ReceivedFile(name: "moved.pdf", path: services.folder.url.appendingPathComponent("moved.pdf").path),
            ]), deviceName: "Pixel", serial: 1)
        services.center.respond(.showInFinder, to: banner)
        services.center.respond(.openFile, to: banner)
        services.center.respond(.opened, to: banner)
        #expect(services.workspace.revealed == [[kept], [kept]])
        #expect(services.workspace.opened == [kept])
        _ = model
    }

    // MARK: Sending

    @Test func sendsFilesAndSkipsFoldersWithANotice() {
        let model = startedModel()
        let file = services.folder.file("notes.txt")
        let folder = services.folder.folder("Photos")
        model.sendFiles([file, folder], to: "p")
        #expect(backend.filesSent.map(\.0) == [[file.path]])
        #expect(backend.filesSent.map(\.1) == ["p"])
        #expect(model.notice == .foldersNotSent(["Photos"]))
    }

    @Test func onlyFoldersSendsNothing() {
        let model = startedModel()
        model.sendFiles([services.folder.folder("A"), services.folder.folder("B")], to: "p")
        #expect(backend.filesSent.isEmpty)
        #expect(model.notice?.title == "2 folders weren’t sent")
    }

    @Test func sendableDevicesArePairedAndConnected() {
        let model = startedModel()
        model.handle(.devicesChanged([
            device("Pixel", id: "p", reachable: true, .paired),
            device("Tablet", id: "t", reachable: false, .paired),
            device("TV", id: "v", reachable: true, .notPaired),
        ]))
        #expect(model.sendableDevices.map(\.id) == ["p"])
    }

    // MARK: Browsers

    @Test func browsersFollowTheirDevicesConnection() {
        let model = startedModel()
        let browser = model.storageBrowser(for: "p")
        #expect(browser.isConnected)
        #expect(model.storageBrowser(for: "p") === browser)
        model.handle(.devicesChanged([device("Pixel 8", id: "p", reachable: false, .paired)]))
        #expect(!browser.isConnected)
        #expect(browser.deviceName == "Pixel 8")
        model.closeStorageBrowser(for: "p")
        #expect(model.storageBrowser(for: "p") !== browser)
    }
}

@MainActor @Suite struct LogReportTests {
    @Test func reportHasVersionsStateAndLogsInOrder() {
        let report = LogReport.make(
            appVersion: "0.1.0", coreVersion: "0.1.0", osVersion: "Version 26.6", state: [("Sync clipboard", "on")],
            logs: [LogReport.Log(name: "landline.log.old", contents: nil), LogReport.Log(name: "landline.log", contents: "linked\n")],
            date: Date(timeIntervalSince1970: 0))
        #expect(report.hasPrefix("Landline log export\nExported: 1970-01-01T00:00:00Z\nApp: 0.1.0\nCore: 0.1.0\nmacOS: Version 26.6\n"))
        #expect(report.contains("\nSync clipboard: on\n"))
        let old = try! #require(report.range(of: "===== landline.log.old =====\n(none)"))
        let current = try! #require(report.range(of: "===== landline.log =====\nlinked"))
        #expect(old.lowerBound < current.lowerBound)
    }

    @Test func modelStateSaysNothingPrivate() {
        let services = TestServices()
        let backend = FakeBackend()
        let model = AppModel(services: services.services) { backend }
        model.start()
        services.settings.downloadDir = "/Users/someone/Secret Folder"
        services.settings.mute("Bank of Somewhere")
        model.handle(.devicesChanged([device("Alex’s Phone", id: "p", reachable: true, .paired)]))
        let text = model.reportState.map { "\($0): \($1)" }.joined(separator: "\n")
        #expect(text.contains("Received files folder: custom"))
        #expect(text.contains("Muted apps: 1"))
        #expect(text.contains("Paired devices: 1, 1 connected"))
        #expect(text.contains("Open links automatically: off"))
        for secret in ["someone", "Secret", "Bank", "Alex"] {
            #expect(!text.contains(secret))
        }
    }

    @Test func readsMissingFilesAsNone() {
        let folder = TemporaryFolder()
        let present = folder.file("landline.log", contents: "hello")
        let logs = LogReport.read([folder.url.appendingPathComponent("landline.log.old"), present])
        #expect(logs == [
            LogReport.Log(name: "landline.log.old", contents: nil), LogReport.Log(name: "landline.log", contents: "hello"),
        ])
    }

    @Test func suggestsADatedName() {
        #expect(LogReport.suggestedFileName(date: Date(timeIntervalSince1970: 0)) == "Landline log 1970-01-01.txt")
    }
}
