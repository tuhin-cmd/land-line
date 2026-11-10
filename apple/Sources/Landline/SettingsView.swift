import AppKit
import SwiftUI

struct SettingsView: View {
    @Environment(AppModel.self) private var model

    var body: some View {
        Form {
            general
            notifications
            mutedApps
            clipboard
            files
        }
        .formStyle(.grouped)
        .frame(width: 480)
        .fixedSize(horizontal: false, vertical: true)
        .task { await refresh() }
        // Coming back from System Settings, where either may have changed.
        .onReceive(NotificationCenter.default.publisher(for: NSApplication.didBecomeActiveNotification)) { _ in
            Task { await refresh() }
        }
    }

    private func refresh() async {
        model.refreshLoginItem()
        await model.refreshNotificationPermission()
    }

    // MARK: Sections

    private var general: some View {
        Section {
            Toggle("Open at login", isOn: Binding(
                get: { [.enabled, .needsApproval].contains(model.loginItemState) },
                set: { model.setLaunchAtLogin($0) }))
            if let explanation = model.loginItemState.explanation {
                Explanation(text: explanation) {
                    if model.loginItemState == .needsApproval {
                        Button("Open Login Items Settings…") { SystemLoginItem.openLoginItemsSettings() }
                    }
                }
            }
            if let error = model.loginItemError {
                Explanation(text: error, symbol: "exclamationmark.triangle.fill", tint: .orange)
            }
        }
    }

    private var notifications: some View {
        Section {
            Toggle(isOn: Binding(
                get: { model.settings.showNotifications },
                set: { model.setShowNotifications($0) })
            ) {
                Text("Show phone notifications")
                Text("Closing one on this Mac also clears it on the phone.")
            }
            Toggle(isOn: Binding(
                get: { model.settings.copyCodesAutomatically },
                set: { model.settings.copyCodesAutomatically = $0 })
            ) {
                Text("Copy codes automatically")
                Text("One-time codes go straight to the clipboard, hidden from clipboard managers.")
            }
            LabeledContent("Mac notifications") {
                permission
            }
        } header: {
            Text("Notifications")
        }
    }

    @ViewBuilder
    private var permission: some View {
        switch model.notificationPermission {
        case .allowed:
            Label("Allowed", systemImage: "checkmark.circle.fill")
                .foregroundStyle(.secondary)
        case .denied:
            HStack {
                Text("Off")
                    .foregroundStyle(.secondary)
                Button("Open System Settings…", action: SystemSettings.openNotifications)
            }
        case .notAsked:
            Button("Allow…") {
                Task { await model.requestNotificationPermission() }
            }
        case nil:
            ProgressView()
                .controlSize(.small)
        }
    }

    private var mutedApps: some View {
        Section {
            if model.settings.mutedApps.isEmpty {
                Text("None. To mute an app, choose Mute in the options of one of its notifications.")
                    .foregroundStyle(.secondary)
            }
            ForEach(model.settings.mutedApps, id: \.self) { app in
                LabeledContent(app) {
                    Button("Unmute") { model.unmute(app) }
                }
            }
        } header: {
            Text("Muted apps")
        }
    }

    private var clipboard: some View {
        Section {
            Toggle(isOn: Binding(
                get: { model.settings.syncClipboard },
                set: { model.setSyncClipboard($0) })
            ) {
                Text("Sync clipboard")
                Text("Text you copy here is sent to your phone, except passwords and other text apps mark as private. Text copied on the phone arrives when you send it from KDE Connect.")
            }
        } header: {
            Text("Clipboard")
        }
    }

    private var files: some View {
        Section {
            LabeledContent("Save received files to") {
                HStack(spacing: 8) {
                    FolderLabel(path: model.settings.downloadDir ?? AppModel.defaultDownloadDir)
                    Button("Choose…", action: chooseFolder)
                }
            }
            if model.settings.downloadDir != nil {
                HStack {
                    Spacer()
                    Button("Use Downloads") { model.setDownloadDir(nil) }
                }
            }
        } header: {
            Text("Files")
        }
    }

    private func chooseFolder() {
        let panel = NSOpenPanel()
        panel.canChooseFiles = false
        panel.canChooseDirectories = true
        panel.canCreateDirectories = true
        panel.allowsMultipleSelection = false
        panel.prompt = "Choose"
        panel.message = "Choose where files sent from your phone are saved."
        panel.directoryURL = URL(fileURLWithPath: model.settings.downloadDir ?? AppModel.defaultDownloadDir)
        guard panel.runModal() == .OK, let url = panel.url else { return }
        model.setDownloadDir(url.path)
    }
}

/// A folder as Finder shows it: its icon and its name, with the full path on
/// hover.
private struct FolderLabel: View {
    let path: String

    var body: some View {
        HStack(spacing: 4) {
            Image(nsImage: NSWorkspace.shared.icon(forFile: path))
                .resizable()
                .frame(width: 16, height: 16)
            Text(FileManager.default.displayName(atPath: path))
                .lineLimit(1)
                .truncationMode(.middle)
        }
        .help(path)
    }
}

/// A note under a control, with an optional button.
private struct Explanation<Action: View>: View {
    let text: String
    var symbol = "info.circle"
    var tint: Color = .secondary
    @ViewBuilder var action: Action

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            Label {
                Text(text)
                    .foregroundStyle(.secondary)
                    .fixedSize(horizontal: false, vertical: true)
            } icon: {
                Image(systemName: symbol)
                    .foregroundStyle(tint)
            }
            .font(.callout)
            action
        }
    }
}

private extension Explanation where Action == EmptyView {
    init(text: String, symbol: String = "info.circle", tint: Color = .secondary) {
        self.init(text: text, symbol: symbol, tint: tint) { EmptyView() }
    }
}
