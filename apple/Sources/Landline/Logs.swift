import Foundation
import LandlineKit

/// The core's log file, which it writes once told where at launch.
enum LogFiles {
    static let folder = FileManager.default.homeDirectoryForCurrentUser
        .appendingPathComponent("Library/Logs/Landline", isDirectory: true)
    static let current = folder.appendingPathComponent("landline.log")
    /// Where the core moves a log that grew too large.
    static let previous = folder.appendingPathComponent("landline.log.old")

    /// Starts the core's file log. Live mode only, and before the core is
    /// created, since the core installs its logger once.
    static func start() {
        do {
            try startLogging(path: current.path)
        } catch {
            Log.app.error("Could not start the core's log: \(AppModel.describe(error), privacy: .public)")
        }
    }
}

/// The text file Export Logs saves: what someone helping needs to know about
/// this Mac and app, then the core's log, oldest first. Settings are
/// described without anything private: the received-files folder only as
/// default or custom, muted apps only as a count, devices only as counts.
enum LogReport {
    struct Log: Equatable {
        let name: String
        /// `nil` if the file does not exist.
        let contents: String?
    }

    static func make(
        appVersion: String, coreVersion: String, osVersion: String, state: [(String, String)], logs: [Log],
        date: Date
    ) -> String {
        var lines = [
            "Landline log export",
            "Exported: \(date.formatted(.iso8601))",
            "App: \(appVersion)",
            "Core: \(coreVersion)",
            "macOS: \(osVersion)",
            "",
        ]
        lines += state.map { "\($0): \($1)" }
        for log in logs {
            lines += ["", "===== \(log.name) =====", log.contents ?? "(none)"]
        }
        return lines.joined(separator: "\n") + "\n"
    }

    static func read(_ urls: [URL]) -> [Log] {
        urls.map { url in
            let data = try? Data(contentsOf: url)
            // Lossy on purpose: a log cut mid-character should still export.
            return Log(name: url.lastPathComponent, contents: data.map { String(decoding: $0, as: UTF8.self) })
        }
    }

    static func suggestedFileName(date: Date) -> String {
        let day = date.formatted(.iso8601.year().month().day())
        return "Landline log \(day).txt"
    }
}
