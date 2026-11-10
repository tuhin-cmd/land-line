import Foundation
import LandlineKit
import Testing

@testable import Landline

@Suite struct TransferListTests {
    let start = Date(timeIntervalSince1970: 1_000_000)

    @Test func reportsWhatEachUpdateDid() {
        var list = TransferList()
        #expect(list.apply(transfer(1), origin: .shared, now: start) == .started)
        #expect(list.apply(transfer(1, done: 50), origin: .shared, now: start) == .progressed)
        #expect(list.apply(transfer(1, done: 100, .finished(path: "/tmp/a.jpg")), origin: .shared, now: start) == .ended)
        #expect(list.apply(transfer(1, .failed(reason: "late")), origin: .shared, now: start) == .ignored)
        #expect(list.entry(1)?.finishedPath == "/tmp/a.jpg")
    }

    @Test func aTransferFirstSeenEndedStillCounts() {
        var list = TransferList()
        #expect(list.apply(transfer(1, .cancelled), origin: .shared, now: start) == .ended)
        #expect(list.entry(1)?.endedAt == start)
    }

    @Test func listsRunningFirstThenEndedNewestFirstWithoutPreviews() {
        var list = TransferList()
        list.apply(transfer(1), origin: .shared, now: start)
        list.apply(transfer(2), origin: .storage, now: start)
        list.apply(transfer(3), origin: .preview, now: start)
        list.apply(transfer(4), origin: .shared, now: start)
        list.apply(transfer(1, .finished(path: "/a")), origin: .shared, now: start.addingTimeInterval(1))
        list.apply(transfer(4, .cancelled), origin: .shared, now: start.addingTimeInterval(2))
        #expect(list.listed.map(\.id) == [2, 4, 1])
    }

    @Test func onlyRunningSharedTransfersCanBeCancelled() {
        var list = TransferList()
        list.apply(transfer(1), origin: .shared, now: start)
        list.apply(transfer(2), origin: .storage, now: start)
        list.apply(transfer(3, .finished(path: "/a")), origin: .shared, now: start)
        #expect(list.entry(1)?.canCancel == true)
        #expect(list.entry(2)?.canCancel == false)
        #expect(list.entry(3)?.canCancel == false)
    }

    @Test func endedEntriesExpireAndClear() {
        var list = TransferList()
        list.apply(transfer(1), origin: .shared, now: start)
        list.apply(transfer(2, .finished(path: "/a")), origin: .shared, now: start)
        list.apply(transfer(3, .failed(reason: "x")), origin: .shared, now: start.addingTimeInterval(60))
        #expect(list.nextExpiry == start.addingTimeInterval(TransferList.endedLifetime))

        list.expire(now: start.addingTimeInterval(TransferList.endedLifetime))
        #expect(list.entries.map(\.id) == [1, 3])
        list.clearEnded()
        #expect(list.entries.map(\.id) == [1])
        #expect(list.nextExpiry == nil)
        #expect(!list.hasEnded)
    }

    @Test func knowsWhichDevicesAreStillSending() {
        var list = TransferList()
        list.apply(transfer(1, device: "a"), origin: .shared, now: start)
        list.apply(transfer(2, device: "b", direction: .outgoing), origin: .shared, now: start)
        list.apply(transfer(3, device: "c"), origin: .storage, now: start)
        #expect(list.devicesReceiving == ["a"])
    }
}

@Suite struct ByteFormatTests {
    @Test func progressReadsLikeFinder() {
        let done = ByteFormat.size(4_200_000)
        let total = ByteFormat.size(10_000_000)
        #expect(ByteFormat.progress(done: 4_200_000, total: 10_000_000) == "\(done) of \(total)")
        #expect(ByteFormat.progress(done: 4_200_000, total: nil) == done)
        // A sender that undercounts never shows more done than total.
        #expect(ByteFormat.progress(done: 20, total: 10) == "\(ByteFormat.size(10)) of \(ByteFormat.size(10))")
    }

    @Test func sizesUseDecimalUnitsAndNoWords() {
        #expect(ByteFormat.size(0).contains("0"))
        #expect(ByteFormat.size(1_000_000).contains("1"))
        #expect(ByteFormat.size(100_000_000) == ByteFormat.size(PreviewPolicy.sizeLimit))
    }

    @Test func fractionNeedsATotal() {
        #expect(ByteFormat.fraction(done: 25, total: 100) == 0.25)
        #expect(ByteFormat.fraction(done: 25, total: nil) == nil)
        #expect(ByteFormat.fraction(done: 25, total: 0) == nil)
        #expect(ByteFormat.fraction(done: 200, total: 100) == 1)
    }
}

@Suite struct TransferRowTests {
    func row(_ transfer: Transfer, _ origin: TransferOrigin = .shared) -> TransferRowContent {
        var list = TransferList()
        list.apply(transfer, origin: origin)
        return TransferRowContent(list.entry(transfer.id)!, deviceName: "Pixel")
    }

    @Test func runningShowsProgress() {
        let content = row(transfer(1, done: 25, total: 100))
        #expect(content.showsProgress)
        #expect(content.fraction == 0.25)
        #expect(content.symbol == "arrow.down.circle")
    }

    @Test func endedSaysWhatHappened() {
        #expect(row(transfer(1, total: 10, .finished(path: "/a"))).detail.hasPrefix("Received from Pixel"))
        #expect(row(transfer(1, direction: .outgoing, total: 10, .finished(path: "/a"))).detail.hasPrefix("Sent to Pixel"))
        #expect(row(transfer(1, total: 10, .finished(path: "/a")), .storage).detail.hasPrefix("Downloaded"))
        #expect(row(transfer(1, direction: .outgoing, total: 10, .finished(path: "/a")), .storage).detail.hasPrefix("Uploaded"))
        let failed = row(transfer(1, .failed(reason: "Disk full")))
        #expect(failed.detail == "Disk full")
        #expect(failed.failed)
        #expect(row(transfer(1, .cancelled)).detail == "Cancelled")
    }
}

@Suite struct ReceivedBatcherTests {
    let start = Date(timeIntervalSince1970: 1_000_000)

    @Test func filesCloseTogetherFormOneBatch() {
        var batcher = ReceivedBatcher()
        batcher.add(ReceivedFile(name: "a.pdf", path: "/d/a.pdf"), from: "p", at: start)
        batcher.add(ReceivedFile(name: "b.pdf", path: "/d/b.pdf"), from: "p", at: start.addingTimeInterval(1))
        #expect(batcher.takeDue(now: start.addingTimeInterval(2), receiving: []).isEmpty)
        let due = batcher.takeDue(now: start.addingTimeInterval(3), receiving: [])
        #expect(due.map(\.files.count) == [2])
        #expect(batcher.isEmpty)
    }

    @Test func aBatchWaitsWhileItsDeviceIsStillSending() {
        var batcher = ReceivedBatcher()
        batcher.add(ReceivedFile(name: "a", path: "/a"), from: "p", at: start)
        #expect(batcher.takeDue(now: start.addingTimeInterval(60), receiving: ["p"]).isEmpty)
        #expect(batcher.takeDue(now: start.addingTimeInterval(60), receiving: []).count == 1)
    }

    @Test func devicesAreBatchedApart() {
        var batcher = ReceivedBatcher()
        batcher.add(ReceivedFile(name: "a", path: "/a"), from: "p", at: start)
        batcher.add(ReceivedFile(name: "b", path: "/b"), from: "q", at: start.addingTimeInterval(1.5))
        #expect(batcher.nextCheck == start.addingTimeInterval(ReceivedBatcher.quietPeriod))
        #expect(batcher.takeDue(now: start.addingTimeInterval(2), receiving: []).map(\.deviceId) == ["p"])
        #expect(batcher.takeDue(now: start.addingTimeInterval(4), receiving: []).map(\.deviceId) == ["q"])
    }

    @Test func aFailureKeepsTheBatchOpen() {
        var batcher = ReceivedBatcher()
        batcher.add(ReceivedFile(name: "a", path: "/a"), from: "p", at: start)
        batcher.touch("p", at: start.addingTimeInterval(1.5))
        #expect(batcher.takeDue(now: start.addingTimeInterval(2.5), receiving: []).isEmpty)
    }

    @Test func bannersNameOneFileOrCountSeveral() {
        let one = Banner.received(
            ReceivedBatch(deviceId: "p", files: [ReceivedFile(name: "a.pdf", path: "/d/a.pdf")]),
            deviceName: "Pixel", serial: 1)
        #expect(one.title == "Received a.pdf")
        #expect(one.body == "From Pixel")
        #expect(one.category == .receivedFile)
        #expect(one.userInfo[BannerInfo.paths] == "/d/a.pdf")

        let files = ["a", "b", "c", "d"].map { ReceivedFile(name: $0, path: "/d/\($0)") }
        let many = Banner.received(ReceivedBatch(deviceId: "p", files: files), deviceName: "Pixel", serial: 2)
        #expect(many.title == "Received 4 files")
        #expect(many.body == "a, b and 2 more from Pixel")
        #expect(many.category == .receivedFiles)
        #expect(many.userInfo[BannerInfo.paths] == "/d/a\n/d/b\n/d/c\n/d/d")
    }
}

@Suite struct SharedLinkTests {
    @Test func webAndMailLinksCanBeOpened() {
        #expect(SharedLink.openable("https://example.com/a?b=c") != nil)
        #expect(SharedLink.openable("  HTTP://example.com  ") != nil)
        #expect(SharedLink.openable("mailto:someone@example.com") != nil)
    }

    @Test func everythingElseIsText() {
        for link in [
            "file:///etc/passwd", "javascript:alert(1)", "intent://scan/#Intent;end", "ssh://host",
            "example.com", "https://", "https:///path", "https://example.com\nhttps://other.example",
            "x-apple.systempreferences:com.apple.preference.security", "",
        ] {
            #expect(SharedLink.openable(link) == nil, "\(link)")
        }
    }
}

@Suite struct UniqueNameTests {
    @Test func keepsAFreeName() {
        #expect(UniqueName.pick("photo.jpg") { _ in false } == "photo.jpg")
    }

    @Test func numbersLikeFinder() {
        let taken: Set = ["photo.jpg", "photo 2.jpg", "notes", ".nomedia", "a.tar.gz", "Report 2.pdf"]
        #expect(UniqueName.pick("photo.jpg", isTaken: taken.contains) == "photo 3.jpg")
        #expect(UniqueName.pick("notes", isTaken: taken.contains) == "notes 2")
        #expect(UniqueName.pick(".nomedia", isTaken: taken.contains) == ".nomedia 2")
        #expect(UniqueName.pick("a.tar.gz", isTaken: taken.contains) == "a.tar 2.gz")
        // A copy of "Report 2" is "Report 3", not "Report 2 2".
        #expect(UniqueName.pick("Report 2.pdf", isTaken: taken.contains) == "Report 3.pdf")
    }

    @Test func severalFilesGetDistinctNamesIgnoringCase() {
        let names = UniqueName.pick(["IMG.jpg", "img.jpg", "other.png"], existing: ["Img.JPG"])
        #expect(names == ["IMG 2.jpg", "img 3.jpg", "other.png"])
    }
}
