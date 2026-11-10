import AppKit
import LandlineKit
import SwiftUI

/// The menu bar panel, laid out like the system's Wi-Fi and Bluetooth panels.
struct PopoverView: View {
    @Environment(AppModel.self) private var model
    @State private var contentHeight: CGFloat = 200

    /// Leaves room for the header and footer below the menu bar on a small
    /// screen, and stops short of a full-height panel on a large one.
    static var maxContentHeight: CGFloat {
        let screen = NSScreen.main?.visibleFrame.height ?? 800
        return max(240, min(560, screen - 140))
    }

    var body: some View {
        let sections = model.sections
        VStack(alignment: .leading, spacing: 0) {
            header
                .padding(.horizontal, 6)
                .padding(.bottom, 8)

            ScrollView(.vertical) {
                content(sections)
                    .onGeometryChange(for: CGFloat.self) { $0.size.height } action: { contentHeight = $0 }
            }
            .scrollBounceBehavior(.basedOnSize)
            // As tall as the content up to a limit, then scrolling: a list
            // of many devices or notifications must not run off the screen.
            .frame(height: min(contentHeight, Self.maxContentHeight))

            Divider()
                .padding(.vertical, 8)
            footer
                .padding(.horizontal, 6)
        }
        .padding(.horizontal, 8)
        .padding(.vertical, 10)
        .frame(width: 340)
    }

    private func content(_ sections: DeviceSections) -> some View {
        VStack(alignment: .leading, spacing: 10) {
            if let notice = model.notice {
                NoticeBanner(notice: notice) { model.notice = nil }
            }
            if let code = model.latestCode {
                CodeCard(
                    code: code,
                    onCopy: { model.copyCode(code.code) },
                    onDismiss: { model.clearLatestCode() })
            }
            if let error = model.startError {
                ProblemView(
                    symbol: "exclamationmark.triangle",
                    title: "Landline couldn’t start",
                    message: error,
                    buttonTitle: "Try Again"
                ) { model.start() }
            } else {
                deviceContent(sections)
                let transfers = model.listedTransfers
                if !transfers.isEmpty {
                    TransferSection(entries: transfers)
                }
                let notifications = model.listedNotifications
                if !notifications.isEmpty {
                    NotificationList(items: notifications)
                }
            }
        }
        .animation(.default, value: model.devices)
        .animation(.default, value: model.notice)
        .animation(.default, value: model.latestCode)
        .animation(.default, value: model.listedNotifications)
        .animation(.default, value: model.confirmingUnpair)
        .animation(.default, value: model.listedTransfers.map(\.id))
    }

    private var header: some View {
        HStack(alignment: .firstTextBaseline) {
            Text("Landline")
                .font(.headline)
            Spacer()
            if model.startError == nil {
                visibleAs
            }
        }
    }

    private var visibleAs: some View {
        Text("Visible as “\(model.macName)”")
            .font(.subheadline)
            .foregroundStyle(.secondary)
            .lineLimit(1)
            .truncationMode(.middle)
            .help("The name your phone shows for this Mac")
    }

    @ViewBuilder
    private func deviceContent(_ sections: DeviceSections) -> some View {
        ForEach(sections.requests, id: \.id) { device in
            IncomingPairingCard(
                device: device,
                onReject: { model.perform(.reject, on: device.id) },
                onAccept: { model.perform(.accept, on: device.id) })
        }

        switch model.networkStatus {
        case .blocked where sections.isEmpty:
            ProblemView(
                symbol: "lock.shield",
                title: "Local Network access is off",
                message: "Landline finds your phone over Wi-Fi, which macOS allows only with permission. Turn on Landline in Privacy & Security > Local Network.",
                buttonTitle: "Open System Settings",
                action: SystemSettings.openLocalNetworkPrivacy)
        case .blocked:
            // The phone can still connect to the Mac, so the list can look
            // healthy while everything the Mac starts fails.
            BlockedWarning()
        case .offline:
            ProblemView(
                symbol: "wifi.slash",
                title: "No network",
                message: "Connect this Mac to the same Wi-Fi network as your phone.")
        case .ready, nil:
            if sections.isEmpty {
                SearchingView()
            }
        }

        if !sections.mine.isEmpty {
            DeviceSection(title: "My Devices", devices: sections.mine)
        }
        if model.networkStatus != .blocked, model.networkStatus != .offline, !sections.isEmpty {
            DeviceSection(title: "Nearby", devices: sections.nearby) {
                HStack(spacing: 6) {
                    ProgressView()
                        .controlSize(.mini)
                    Text("Looking for devices…")
                }
                .font(.subheadline)
                .foregroundStyle(.secondary)
                .padding(.horizontal, 6)
                .padding(.vertical, 2)
            }
        }
    }

    private var footer: some View {
        HStack {
            Text(versionLine)
                .font(.caption)
                .foregroundStyle(.secondary)
                .help(model.isDemo ? "Demo mode: sample devices, no network" : "")
            Spacer()
            SettingsButton()
            Button("Quit Landline") { NSApplication.shared.terminate(nil) }
                .keyboardShortcut("q")
                .controlSize(.small)
        }
    }

    private var versionLine: String {
        let app = Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "dev"
        let demo = model.isDemo ? " · Demo" : ""
        return "Version \(app) · core \(coreVersion())\(demo)"
    }
}

/// Opens the Settings window and brings it forward. A menu bar app is never
/// the active app on its own, so without this the window would open behind
/// whatever the user was working in.
private struct SettingsButton: View {
    @Environment(\.openSettings) private var openSettings

    var body: some View {
        Button {
            NSApplication.shared.activate()
            openSettings()
            // `activate()` is only a request since macOS 14, and the system
            // may turn it down; ordering the window front regardless covers
            // that. The window exists only after this turn of the run loop.
            DispatchQueue.main.async {
                for window in NSApplication.shared.windows
                where window.identifier?.rawValue == "com_apple_SwiftUI_Settings_window" {
                    window.orderFrontRegardless()
                    window.makeKey()
                }
            }
        } label: {
            Image(systemName: "gearshape")
                .frame(width: 18, height: 18)
        }
        .buttonStyle(.borderless)
        .foregroundStyle(.secondary)
        .keyboardShortcut(",")
        .help("Settings")
        .accessibilityLabel("Settings")
    }
}

private struct DeviceSection<Empty: View>: View {
    let title: String
    let devices: [Device]
    @ViewBuilder var empty: Empty

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(title)
                .font(.subheadline.weight(.semibold))
                .foregroundStyle(.secondary)
                .padding(.horizontal, 6)
                .padding(.bottom, 2)
            if devices.isEmpty {
                empty
            }
            ForEach(devices, id: \.id) { device in
                DeviceRowView(device: device)
            }
        }
    }
}

private extension DeviceSection where Empty == EmptyView {
    init(title: String, devices: [Device]) {
        self.init(title: title, devices: devices) { EmptyView() }
    }
}

/// Before anything has been found. Tells the user what the phone needs,
/// since nothing on the Mac can make it appear.
private struct SearchingView: View {
    var body: some View {
        VStack(spacing: 6) {
            ProgressView()
                .controlSize(.small)
                .padding(.bottom, 4)
            Text("Looking for devices")
                .font(.headline)
            Text("Open the KDE Connect app on your phone and make sure it’s on the same Wi-Fi network as this Mac.")
                .font(.subheadline)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
        }
        .frame(maxWidth: .infinity)
        .padding(.horizontal, 16)
        .padding(.vertical, 18)
    }
}

private struct ProblemView: View {
    let symbol: String
    let title: String
    let message: String
    var buttonTitle: String?
    var action: () -> Void = {}

    var body: some View {
        VStack(spacing: 6) {
            Image(systemName: symbol)
                .font(.system(size: 24))
                .foregroundStyle(.secondary)
                .padding(.bottom, 2)
            Text(title)
                .font(.headline)
            Text(message)
                .font(.subheadline)
                .foregroundStyle(.secondary)
                .multilineTextAlignment(.center)
                .fixedSize(horizontal: false, vertical: true)
                .textSelection(.enabled)
            if let buttonTitle {
                Button(buttonTitle, action: action)
                    .controlSize(.regular)
                    .padding(.top, 4)
            }
        }
        .frame(maxWidth: .infinity)
        .padding(.horizontal, 16)
        .padding(.vertical, 14)
    }
}

/// Local Network permission is missing but devices are listed: say what
/// does not work, compactly, above the list.
private struct BlockedWarning: View {
    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            Image(systemName: "lock.shield.fill")
                .foregroundStyle(.orange)
            VStack(alignment: .leading, spacing: 2) {
                Text("Local Network access is off")
                    .font(.subheadline.weight(.semibold))
                Text("macOS doesn’t let this Mac connect to devices on your network. Until you turn on Landline in Local Network settings, files can’t be sent or browsed, and devices may not reconnect.")
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
                Button("Open System Settings", action: SystemSettings.openLocalNetworkPrivacy)
                    .controlSize(.small)
                    .padding(.top, 4)
            }
            Spacer(minLength: 0)
        }
        .padding(10)
        .background(.quinary, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
    }
}

private struct NoticeBanner: View {
    let notice: Notice
    let onDismiss: () -> Void

    var body: some View {
        HStack(alignment: .top, spacing: 8) {
            Image(systemName: "exclamationmark.triangle.fill")
                .foregroundStyle(.orange)
            VStack(alignment: .leading, spacing: 2) {
                Text(notice.title)
                    .font(.subheadline.weight(.semibold))
                Text(notice.message)
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            }
            Spacer(minLength: 0)
            Button(action: onDismiss) {
                Image(systemName: "xmark")
                    .font(.system(size: 10, weight: .semibold))
                    .frame(width: 16, height: 16)
            }
            .buttonStyle(.borderless)
            .foregroundStyle(.secondary)
            .help("Dismiss")
            .accessibilityLabel("Dismiss")
        }
        .padding(10)
        .background(.quinary, in: RoundedRectangle(cornerRadius: 10, style: .continuous))
    }
}

enum SystemSettings {
    /// The Local Network list under Privacy & Security. Older systems that
    /// do not know the anchor open Privacy & Security itself, which is close.
    static func openLocalNetworkPrivacy() {
        let url = URL(string: "x-apple.systempreferences:com.apple.preference.security?Privacy_LocalNetwork")!
        NSWorkspace.shared.open(url)
    }

    /// Landline's page under Notifications.
    static func openNotifications() {
        let id = Bundle.main.bundleIdentifier ?? "io.github.tuhin-cmd.landline"
        let url = URL(string: "x-apple.systempreferences:com.apple.Notifications-Settings.extension?id=\(id)")!
        NSWorkspace.shared.open(url)
    }
}
