import os

/// Log categories. Nothing a phone sends (notification text, codes,
/// clipboard contents) is ever interpolated into these.
enum Log {
    static let notifications = Logger(subsystem: "io.github.tuhin-cmd.landline", category: "notifications")
    static let clipboard = Logger(subsystem: "io.github.tuhin-cmd.landline", category: "clipboard")
    static let app = Logger(subsystem: "io.github.tuhin-cmd.landline", category: "app")
}
