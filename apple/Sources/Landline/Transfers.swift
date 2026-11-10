import Foundation
import LandlineKit

/// Where a transfer came from, which decides what the app does with it.
/// The core reports all of them the same way, so the app tells them apart
/// by remembering which storage calls it made.
enum TransferOrigin: Equatable, Sendable {
    /// Shared from the phone, or sent from the Mac.
    case shared
    /// A storage browser download or upload.
    case storage
    /// A download into the preview cache. Not listed: the browser window
    /// shows its progress, and the file is thrown away afterwards.
    case preview
}

/// One transfer as the app tracks it.
struct TransferEntry: Identifiable, Equatable {
    var transfer: Transfer
    let origin: TransferOrigin
    let startedAt: Date
    var endedAt: Date?

    var id: UInt64 { transfer.id }
    var isActive: Bool { transfer.state == .inProgress }
    var canCancel: Bool { isActive }

    /// The saved file, once an incoming transfer has finished.
    var finishedPath: String? {
        if case .finished(let path) = transfer.state { return path }
        return nil
    }
}

/// The transfers the app knows about: running ones, and finished ones until
/// they expire or are cleared.
struct TransferList: Equatable {
    /// How long a finished, failed or cancelled transfer stays listed.
    static let endedLifetime: TimeInterval = 5 * 60

    private(set) var entries: [TransferEntry] = []

    enum Change: Equatable {
        case started
        case progressed
        /// The transfer reached its final state with this update.
        case ended
        /// An update for a transfer that had already ended.
        case ignored
    }

    @discardableResult
    mutating func apply(_ transfer: Transfer, origin: TransferOrigin, now: Date = .now) -> Change {
        let ended = transfer.state != .inProgress
        guard let index = entries.firstIndex(where: { $0.id == transfer.id }) else {
            entries.append(
                TransferEntry(transfer: transfer, origin: origin, startedAt: now, endedAt: ended ? now : nil))
            return ended ? .ended : .started
        }
        guard entries[index].isActive else { return .ignored }
        entries[index].transfer = transfer
        if ended {
            entries[index].endedAt = now
            return .ended
        }
        return .progressed
    }

    func entry(_ id: UInt64) -> TransferEntry? {
        entries.first { $0.id == id }
    }

    /// What the panel lists: running transfers in the order they started,
    /// then ended ones, newest first. Preview downloads are left out.
    var listed: [TransferEntry] {
        let shown = entries.filter { $0.origin != .preview }
        let active = shown.filter(\.isActive)
        let ended = shown.filter { !$0.isActive }.sorted { ($0.endedAt ?? .distantPast) > ($1.endedAt ?? .distantPast) }
        return active + ended
    }

    var hasEnded: Bool { entries.contains { !$0.isActive && $0.origin != .preview } }

    /// Running incoming transfers from the phone, by device.
    var devicesReceiving: Set<String> {
        Set(entries.filter { $0.isActive && $0.origin == .shared && $0.transfer.direction == .incoming }
            .map(\.transfer.deviceId))
    }

    mutating func clearEnded() {
        entries.removeAll { !$0.isActive }
    }

    mutating func expire(now: Date = .now) {
        entries.removeAll { entry in
            guard let endedAt = entry.endedAt else { return false }
            return now.timeIntervalSince(endedAt) >= Self.endedLifetime
        }
    }

    /// When the next ended entry is due to go.
    var nextExpiry: Date? {
        entries.compactMap(\.endedAt).min().map { $0.addingTimeInterval(Self.endedLifetime) }
    }
}

/// Sizes and progress as Finder writes them.
enum ByteFormat {
    static func size(_ bytes: UInt64) -> String {
        let formatter = ByteCountFormatter()
        formatter.countStyle = .file
        formatter.allowsNonnumericFormatting = false
        return formatter.string(fromByteCount: Int64(clamping: bytes))
    }

    /// "4.2 MB of 10 MB", or just the bytes so far when the total is unknown.
    static func progress(done: UInt64, total: UInt64?) -> String {
        guard let total else { return size(done) }
        return "\(size(min(done, total))) of \(size(total))"
    }

    /// Between 0 and 1, or `nil` when there is nothing to measure against.
    static func fraction(done: UInt64, total: UInt64?) -> Double? {
        guard let total, total > 0 else { return nil }
        return min(1, Double(done) / Double(total))
    }
}

/// What a transfer row in the panel shows.
struct TransferRowContent: Equatable {
    let name: String
    let symbol: String
    let detail: String
    /// `nil` when the row shows no bar, or the total is unknown while
    /// running (an indeterminate bar).
    let fraction: Double?
    let showsProgress: Bool
    let failed: Bool

    init(_ entry: TransferEntry, deviceName: String?) {
        let transfer = entry.transfer
        let incoming = transfer.direction == .incoming
        let device = deviceName ?? "the phone"
        name = transfer.fileName.isEmpty ? "Untitled" : transfer.fileName
        symbol = incoming ? "arrow.down.circle" : "arrow.up.circle"
        failed = if case .failed = transfer.state { true } else { false }
        switch transfer.state {
        case .inProgress:
            showsProgress = true
            fraction = ByteFormat.fraction(done: transfer.bytesDone, total: transfer.bytesTotal)
            detail = ByteFormat.progress(done: transfer.bytesDone, total: transfer.bytesTotal)
        case .finished:
            showsProgress = false
            fraction = nil
            let size = ByteFormat.size(transfer.bytesTotal ?? transfer.bytesDone)
            let what = switch (entry.origin, incoming) {
            case (.shared, true): "Received from \(device)"
            case (.shared, false): "Sent to \(device)"
            case (_, true): "Downloaded"
            case (_, false): "Uploaded"
            }
            detail = "\(what) · \(size)"
        case .failed(let reason):
            showsProgress = false
            fraction = nil
            detail = reason.isEmpty ? "Failed" : reason
        case .cancelled:
            showsProgress = false
            fraction = nil
            detail = "Cancelled"
        }
    }
}

/// Files received from one device close together, announced as one.
struct ReceivedBatch: Equatable {
    let deviceId: String
    var files: [ReceivedFile]
}

struct ReceivedFile: Equatable {
    let name: String
    let path: String
}

/// Groups finished incoming files by device and timing, so a batch from the
/// phone gets one Mac notification instead of one per file.
///
/// The phone sends a batch one file at a time and starts the next within a
/// second of the last ending, so a batch is complete once nothing from that
/// device is running and nothing has finished for a short quiet period.
struct ReceivedBatcher {
    static let quietPeriod: TimeInterval = 2

    private var pending: [String: (batch: ReceivedBatch, last: Date)] = [:]

    var isEmpty: Bool { pending.isEmpty }

    mutating func add(_ file: ReceivedFile, from deviceId: String, at date: Date) {
        var entry = pending[deviceId] ?? (ReceivedBatch(deviceId: deviceId, files: []), date)
        entry.batch.files.append(file)
        entry.last = date
        pending[deviceId] = entry
    }

    /// Notes activity that keeps a batch open without adding to it, such as
    /// a file that failed partway through a batch.
    mutating func touch(_ deviceId: String, at date: Date) {
        pending[deviceId]?.last = date
    }

    /// Takes the batches that are complete: their device is not receiving
    /// anything and has been quiet long enough.
    mutating func takeDue(now: Date, receiving: Set<String>) -> [ReceivedBatch] {
        let due = pending.filter { deviceId, entry in
            !receiving.contains(deviceId) && now.timeIntervalSince(entry.last) >= Self.quietPeriod
        }
        for deviceId in due.keys { pending[deviceId] = nil }
        return due.values.map(\.batch).sorted { $0.deviceId < $1.deviceId }
    }

    /// When the earliest pending batch could next be due.
    var nextCheck: Date? {
        pending.values.map { $0.last.addingTimeInterval(Self.quietPeriod) }.min()
    }
}

/// Which shared links the app may hand to the system. Anything else (a
/// `file:` URL, a custom app scheme) could open something on the Mac the
/// user did not choose, so it is treated as text.
enum SharedLink {
    static let openableSchemes: Set<String> = ["http", "https", "mailto"]

    static func openable(_ string: String) -> URL? {
        let trimmed = string.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.contains(where: \.isNewline), let url = URL(string: trimmed),
            let scheme = url.scheme?.lowercased(), openableSchemes.contains(scheme)
        else { return nil }
        if scheme == "mailto" { return url }
        guard let host = url.host, !host.isEmpty else { return nil }
        return url
    }
}

/// Names that do not overwrite: "photo.jpg", then "photo 2.jpg",
/// "photo 3.jpg", as Finder does.
enum UniqueName {
    static func pick(_ name: String, isTaken: (String) -> Bool) -> String {
        guard isTaken(name) else { return name }
        let (base, ext) = split(name)
        var (stem, number) = numbered(base)
        if number == nil { stem = base }
        var next = (number ?? 1) + 1
        while true {
            let candidate = ext.isEmpty ? "\(stem) \(next)" : "\(stem) \(next).\(ext)"
            if !isTaken(candidate) { return candidate }
            next += 1
        }
    }

    /// Picks names for several files going into one folder, unique against
    /// the folder and against each other. Comparison ignores case, as the
    /// Mac's and the phone's file systems usually do.
    static func pick(_ names: [String], existing: Set<String>) -> [String] {
        var taken = Set(existing.map { $0.lowercased() })
        return names.map { name in
            let chosen = pick(name) { taken.contains($0.lowercased()) }
            taken.insert(chosen.lowercased())
            return chosen
        }
    }

    /// A leading dot is part of the name, not an extension (".nomedia").
    private static func split(_ name: String) -> (String, String) {
        guard let dot = name.lastIndex(of: "."), dot != name.startIndex else { return (name, "") }
        let ext = String(name[name.index(after: dot)...])
        guard !ext.isEmpty, !ext.contains(" ") else { return (name, "") }
        return (String(name[..<dot]), ext)
    }

    /// "photo 2" is ("photo", 2), so its copy becomes "photo 3" rather than
    /// "photo 2 2".
    private static func numbered(_ base: String) -> (String, Int?) {
        guard let space = base.lastIndex(of: " ") else { return (base, nil) }
        let digits = base[base.index(after: space)...]
        guard !digits.isEmpty, digits.allSatisfy(\.isASCII), let number = Int(digits), number >= 2 else {
            return (base, nil)
        }
        return (String(base[..<space]), number)
    }
}
