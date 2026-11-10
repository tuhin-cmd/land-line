import AppKit
import LandlineKit
import SwiftUI

/// The last one-time code, at the top of the panel, so a missed banner is
/// one click away.
struct CodeCard: View {
    let code: ReceivedCode
    let onCopy: () -> Void
    let onDismiss: () -> Void

    var body: some View {
        InsetCard {
            HStack(alignment: .top) {
                Text(code.source.isEmpty ? "One-time code" : "Code from \(code.source)")
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .lineLimit(1)
                Spacer(minLength: 8)
                Button(action: onDismiss) {
                    Image(systemName: "xmark")
                        .font(.system(size: 10, weight: .semibold))
                        .frame(width: 16, height: 16)
                }
                .buttonStyle(.borderless)
                .foregroundStyle(.secondary)
                .help("Dismiss")
                .accessibilityLabel("Dismiss code")
            }
            HStack(alignment: .center) {
                Text(code.code)
                    .font(.system(size: 22, weight: .semibold, design: .monospaced))
                    .tracking(2)
                    .textSelection(.enabled)
                    .accessibilityLabel(code.code.map(String.init).joined(separator: " "))
                Spacer()
                Button(action: onCopy) {
                    Label(code.copied ? "Copied" : "Copy", systemImage: code.copied ? "checkmark" : "doc.on.doc")
                        .contentTransition(.symbolEffect(.replace))
                }
                .controlSize(.small)
                .help(code.copied ? "On the clipboard. Click to copy again." : "Copy the code")
            }
        }
    }
}

/// Notifications showing on connected phones, newest first.
struct NotificationList: View {
    let items: [ListedNotification]
    /// Rows beyond this collapse into a count, to keep the panel short.
    static let limit = 4

    @Environment(AppModel.self) private var model

    var body: some View {
        let showDevice = Set(items.map(\.deviceId)).count > 1
        VStack(alignment: .leading, spacing: 2) {
            Text("Notifications")
                .font(.subheadline.weight(.semibold))
                .foregroundStyle(.secondary)
                .padding(.horizontal, 6)
                .padding(.bottom, 2)
            ForEach(items.prefix(Self.limit)) { item in
                NotificationRow(item: item, showDevice: showDevice) {
                    model.dismiss(item.notification.id, on: item.deviceId)
                }
            }
            if items.count > Self.limit {
                Text("\(items.count - Self.limit) more")
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 6)
                    .padding(.top, 2)
            }
        }
    }
}

private struct NotificationRow: View {
    let item: ListedNotification
    let showDevice: Bool
    let onDismiss: () -> Void
    @State private var hovering = false

    private var notification: PhoneNotification { item.notification }

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            AppIcon(name: notification.appName, iconPath: notification.iconPath)
            VStack(alignment: .leading, spacing: 1) {
                HStack(spacing: 4) {
                    Text(showDevice ? "\(notification.appName) · \(item.deviceName)" : notification.appName)
                        .font(.caption.weight(.medium))
                        .foregroundStyle(.secondary)
                        .lineLimit(1)
                    Spacer(minLength: 4)
                    trailing
                }
                if !notification.title.isEmpty {
                    Text(notification.title)
                        .font(.subheadline.weight(.medium))
                        .lineLimit(1)
                }
                if !notification.text.isEmpty {
                    Text(notification.text)
                        .font(.subheadline)
                        .foregroundStyle(.secondary)
                        .lineLimit(2)
                        .fixedSize(horizontal: false, vertical: true)
                }
            }
            .accessibilityElement(children: .combine)
        }
        .padding(.vertical, 4)
        .padding(.horizontal, 6)
        .background(
            hovering ? AnyShapeStyle(.quinary) : AnyShapeStyle(.clear),
            in: RoundedRectangle(cornerRadius: 6, style: .continuous))
        .onHover { hovering = $0 }
    }

    /// The time, swapped for a clear button on hover, as Notification
    /// Center does.
    @ViewBuilder
    private var trailing: some View {
        if hovering && notification.dismissable {
            Button(action: onDismiss) {
                Image(systemName: "xmark.circle.fill")
                    .font(.system(size: 12))
            }
            .buttonStyle(.borderless)
            .foregroundStyle(.secondary)
            .help("Clear on the phone")
            .accessibilityLabel("Clear \(notification.appName) notification")
        } else {
            Text(NotificationTime.short(notification.timeMs))
                .font(.caption)
                .foregroundStyle(.tertiary)
                .monospacedDigit()
        }
    }
}

/// The phone app's icon if it sent one, otherwise its initial on a tile.
private struct AppIcon: View {
    let name: String
    let iconPath: String?

    var body: some View {
        Group {
            if let iconPath, let image = NSImage(contentsOfFile: iconPath) {
                Image(nsImage: image)
                    .resizable()
                    .interpolation(.high)
            } else {
                Text(name.first.map { String($0).uppercased() } ?? "•")
                    .font(.system(size: 11, weight: .semibold))
                    .foregroundStyle(.secondary)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
                    .background(.quaternary)
            }
        }
        .frame(width: 22, height: 22)
        .clipShape(RoundedRectangle(cornerRadius: 5, style: .continuous))
        .padding(.top, 2)
        .accessibilityHidden(true)
    }
}

enum NotificationTime {
    /// "now", "5m", "3h", "2d": the compact form Android's shade uses, which
    /// fits at the end of a row.
    static func short(_ timeMs: Int64, now: Date = .now) -> String {
        let seconds = max(0, now.timeIntervalSince1970 - Double(timeMs) / 1000)
        switch seconds {
        case ..<60: return "now"
        case ..<3600: return "\(Int(seconds / 60))m"
        case ..<86_400: return "\(Int(seconds / 3600))h"
        default: return "\(Int(seconds / 86_400))d"
        }
    }
}
