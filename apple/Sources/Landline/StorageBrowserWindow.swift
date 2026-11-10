import AppKit
import LandlineKit
import QuickLook
import SwiftUI
// SwiftUI's Quick Look modifier lives in this cross-import overlay, which
// scripts/test-swift.sh switches off; importing it by name works either way.
import _QuickLook_SwiftUI

/// A window onto one device's storage, laid out like a small Finder window:
/// the storage roots in a sidebar, the folder's files in a list, and a path
/// bar at the bottom. One window per device.
struct StorageBrowserWindow: View {
    static let id = "storage-browser"

    let deviceId: String
    @Environment(AppModel.self) private var model

    var body: some View {
        StorageBrowserView(browser: model.storageBrowser(for: deviceId))
            .background(WindowReader { window in
                BrowserWindows.register(window, for: deviceId)
            })
            .onDisappear {
                model.closeStorageBrowser(for: deviceId)
            }
    }
}

private struct StorageBrowserView: View {
    @Bindable var browser: StorageBrowserModel
    @Environment(AppModel.self) private var model

    var body: some View {
        NavigationSplitView {
            sidebar
                .navigationSplitViewColumnWidth(min: 150, ideal: 180, max: 260)
        } detail: {
            detail
                .frame(maxWidth: .infinity, maxHeight: .infinity)
                .safeAreaInset(edge: .bottom, spacing: 0) { bottomBar }
                .toolbar { toolbar }
                .navigationTitle(browser.folderName)
                .navigationSubtitle(browser.deviceName)
                .toolbarTitleMenu {
                    ForEach(browser.trail.reversed(), id: \.path) { step in
                        Button(step.name) { browser.go(toPath: step.path) }
                    }
                }
        }
        .frame(minWidth: 560, minHeight: 300)
        .background(shortcuts)
        .quickLookPreview($browser.previewURL)
        .alert(
            tooLargeTitle,
            isPresented: Binding(
                get: { if case .tooLarge = browser.previewState { true } else { false } },
                set: { if !$0 { browser.cancelPreview() } })
        ) {
            Button("Download") { browser.downloadInsteadOfPreview() }
            Button("Cancel", role: .cancel) { browser.cancelPreview() }
        } message: {
            Text("Quick Look needs the whole file on this Mac first. Files over \(ByteFormat.size(PreviewPolicy.sizeLimit)) are not fetched just to preview them; download it instead.")
        }
        .task { browser.start() }
    }

    private var tooLargeTitle: String {
        guard case .tooLarge(let entry) = browser.previewState else { return "" }
        return "“\(entry.name)” is \(ByteFormat.size(entry.size))"
    }

    // MARK: Sidebar

    private var sidebar: some View {
        List(selection: Binding(
            get: { browser.current?.root },
            set: { if let root = $0 { browser.select(root: root) } })
        ) {
            Section(browser.deviceName) {
                ForEach(browser.roots, id: \.self) { root in
                    Label(root.name, systemImage: Self.symbol(for: root))
                        .tag(root)
                }
            }
        }
        .listStyle(.sidebar)
    }

    private static func symbol(for root: StorageRoot) -> String {
        let name = root.name.lowercased()
        return name.contains("sd") || name.contains("card") || name.contains("usb") ? "sdcard" : "internaldrive"
    }

    // MARK: Content

    @ViewBuilder
    private var detail: some View {
        if browser.showsEntries {
            let entries = browser.visibleEntries
            RemoteFileTable(
                entries: entries, selection: $browser.selection, sort: $browser.sort, actions: tableActions
            )
            .overlay {
                if entries.isEmpty, browser.phase == .loaded {
                    Text(browser.entries.isEmpty ? "This folder is empty" : "Only hidden files")
                        .font(.title3)
                        .foregroundStyle(.secondary)
                        .allowsHitTesting(false)
                }
            }
        } else {
            switch browser.phase {
            case .loading, .loaded:
                ProgressView()
                    .controlSize(.regular)
                    .frame(maxWidth: .infinity, maxHeight: .infinity)
            case .failed(let message):
                ContentUnavailableView {
                    Label("Can’t open this folder", systemImage: "exclamationmark.triangle")
                } description: {
                    Text(message)
                        .textSelection(.enabled)
                } actions: {
                    HStack {
                        if model.networkStatus == .blocked {
                            Button("Open System Settings", action: SystemSettings.openLocalNetworkPrivacy)
                        }
                        Button("Try Again") { browser.reload() }
                            .keyboardShortcut(.defaultAction)
                    }
                }
            case .disconnected:
                ContentUnavailableView {
                    Label("\(browser.deviceName) isn’t connected", systemImage: "iphone.slash")
                } description: {
                    Text("This window will load again when it reconnects. Keep KDE Connect open on the phone and on the same Wi-Fi network as this Mac.")
                }
            }
        }
    }

    private var tableActions: RemoteFileTable.Actions {
        let browser = browser
        return RemoteFileTable.Actions(
            open: { browser.open($0) },
            openSelection: { browser.openSelection() },
            download: { browser.download($0) },
            downloadTo: { entries in
                if let folder = FilePanels.chooseDownloadFolder(for: entries.count) {
                    browser.download(entries, into: folder)
                }
            },
            togglePreview: { browser.togglePreview() },
            cancelPreview: { browser.cancelPreview() },
            preview: { browser.preview($0) },
            deliver: { entry, url in try await browser.deliver(entry, to: url) },
            upload: { browser.upload($0) })
    }

    // MARK: Toolbar

    @ToolbarContentBuilder
    private var toolbar: some ToolbarContent {
        ToolbarItemGroup(placement: .navigation) {
            ControlGroup {
                Button {
                    browser.goBack()
                } label: {
                    Label("Back", systemImage: "chevron.left")
                }
                .disabled(!browser.history.canGoBack || !browser.isConnected)
                .keyboardShortcut("[", modifiers: .command)
                .help("See folders you viewed previously")
                Button {
                    browser.goForward()
                } label: {
                    Label("Forward", systemImage: "chevron.right")
                }
                .disabled(!browser.history.canGoForward || !browser.isConnected)
                .keyboardShortcut("]", modifiers: .command)
                .help("See folders you viewed next")
            }
            .controlGroupStyle(.navigation)
        }
        ToolbarItemGroup(placement: .primaryAction) {
            Toggle(isOn: $browser.showHidden) {
                Label("Show Hidden Files", systemImage: browser.showHidden ? "eye" : "eye.slash")
            }
            .keyboardShortcut(".", modifiers: [.command, .shift])
            .help(browser.showHidden ? "Hide hidden files" : "Show hidden files")
            Button {
                if let urls = FilePanels.chooseFilesToUpload(into: browser.folderName) { browser.upload(urls) }
            } label: {
                Label("Upload", systemImage: "square.and.arrow.up")
            }
            .disabled(!browser.showsEntries)
            .help("Upload files to this folder")
            let files = browser.selectedEntries.filter { !$0.isDir }
            Menu {
                Button("Download To…") {
                    if let folder = FilePanels.chooseDownloadFolder(for: files.count) {
                        browser.download(files, into: folder)
                    }
                }
            } label: {
                Label("Download", systemImage: "square.and.arrow.down")
            } primaryAction: {
                browser.download(files)
            }
            .disabled(files.isEmpty || !browser.isConnected)
            .help("Download the selected files to \(FileManager.default.displayName(atPath: model.downloadFolder.path))")
        }
    }

    /// Keys with no visible control. Zero-sized and transparent rather
    /// than hidden, since a hidden button's shortcut does not fire.
    private var shortcuts: some View {
        Group {
            Button("Enclosing Folder") { browser.goUp() }
                .keyboardShortcut(.upArrow, modifiers: .command)
                .disabled(!browser.canGoUp || !browser.isConnected)
            Button("Reload") { browser.reload() }
                .keyboardShortcut("r", modifiers: .command)
        }
        .frame(width: 0, height: 0)
        .opacity(0)
        .accessibilityHidden(true)
    }

    // MARK: Bottom bar

    private var bottomBar: some View {
        VStack(spacing: 0) {
            if let notice = browser.notice {
                Divider()
                StatusRow(symbol: "info.circle", tint: .secondary, text: notice) {
                    Button("OK") { browser.notice = nil }
                }
            }
            if case .preparing(let entry, let transferId) = browser.previewState {
                Divider()
                ProgressRow(
                    title: "Preparing a preview of \(entry.name)…",
                    transfer: transferId.flatMap { model.transfer($0) }?.transfer,
                    total: entry.size
                ) {
                    Button("Close") { browser.cancelPreview() }
                        .help("Stop fetching the file for the preview.")
                }
            }
            operations
            Divider()
            pathBar
        }
        .background(.bar)
    }

    @ViewBuilder
    private var operations: some View {
        let pending = browser.pendingOperations
        if let running = pending.first {
            let verb = running.kind == .download ? "Downloading" : "Uploading"
            let count = pending.count > 1 ? " (1 of \(pending.count))" : ""
            Divider()
            ProgressRow(
                title: "\(verb) \(running.name)\(count)",
                transfer: running.transferId.flatMap { model.transfer($0) }?.transfer,
                total: nil)
            {
                if running.transferId != nil {
                    Button("Stop") { browser.stopRunningOperation() }
                }
            }
        }
        let failed = browser.failedOperations
        if let first = failed.first, case .failed(let message) = first.state {
            Divider()
            StatusRow(
                symbol: "exclamationmark.triangle.fill", tint: .orange,
                text: failed.count > 1
                    ? "\(failed.count) transfers failed. \(first.name): \(message)" : "\(first.name): \(message)"
            ) {
                Button("OK") { browser.dismissFailures() }
            }
        }
    }

    private var pathBar: some View {
        HStack(spacing: 2) {
            if let root = browser.current?.root {
                let trail = browser.trail
                ForEach(Array(trail.enumerated()), id: \.element.path) { index, step in
                    if index > 0 {
                        Image(systemName: "chevron.right")
                            .font(.system(size: 9, weight: .semibold))
                            .foregroundStyle(.tertiary)
                    }
                    Button {
                        browser.go(toPath: step.path)
                    } label: {
                        HStack(spacing: 3) {
                            if index == 0 {
                                Image(systemName: Self.symbol(for: root))
                            } else {
                                FileIcon(name: step.name, isFolder: true, size: 14)
                            }
                            Text(step.name)
                                .lineLimit(1)
                        }
                    }
                    .buttonStyle(.borderless)
                    .foregroundStyle(index == trail.count - 1 ? .primary : .secondary)
                    .disabled(!browser.isConnected)
                }
            }
            Spacer(minLength: 8)
            if browser.phase == .loading, browser.showsEntries {
                ProgressView()
                    .controlSize(.mini)
                    .padding(.trailing, 4)
            }
            Text(itemCount)
                .foregroundStyle(.secondary)
                .monospacedDigit()
        }
        .font(.caption)
        .lineLimit(1)
        .padding(.horizontal, 10)
        .frame(height: 24)
    }

    private var itemCount: String {
        guard browser.showsEntries else { return "" }
        let shown = browser.visibleEntries.count
        let hidden = browser.showHidden ? 0 : browser.hiddenCount
        let items = shown == 1 ? "1 item" : "\(shown) items"
        let selected = browser.selection.count
        let selection = selected > 0 ? "\(selected) of " : ""
        return hidden > 0 ? "\(selection)\(items), \(hidden) hidden" : "\(selection)\(items)"
    }
}

/// A line in the bottom bar with a message and a button.
private struct StatusRow<Action: View>: View {
    let symbol: String
    let tint: Color
    let text: String
    @ViewBuilder var action: Action

    var body: some View {
        HStack(spacing: 6) {
            Image(systemName: symbol)
                .foregroundStyle(tint)
            Text(text)
                .lineLimit(2)
                .textSelection(.enabled)
            Spacer(minLength: 8)
            action
                .controlSize(.small)
        }
        .font(.callout)
        .padding(.horizontal, 10)
        .padding(.vertical, 6)
    }
}

/// A running download or upload in the bottom bar.
private struct ProgressRow<Action: View>: View {
    let title: String
    let transfer: Transfer?
    /// The size the browser listed, used until the core reports one.
    let total: UInt64?
    @ViewBuilder var action: Action

    var body: some View {
        let done = transfer?.bytesDone ?? 0
        let total = transfer?.bytesTotal ?? total
        HStack(spacing: 10) {
            VStack(alignment: .leading, spacing: 3) {
                HStack {
                    Text(title)
                        .lineLimit(1)
                        .truncationMode(.middle)
                    Spacer(minLength: 8)
                    Text(ByteFormat.progress(done: done, total: total))
                        .foregroundStyle(.secondary)
                        .monospacedDigit()
                }
                // Two views rather than an optional value: a bar that starts
                // indeterminate does not redraw its fill later.
                Group {
                    if let fraction = ByteFormat.fraction(done: done, total: total) {
                        ProgressView(value: fraction)
                    } else {
                        ProgressView()
                    }
                }
                .progressViewStyle(.linear)
                .controlSize(.small)
            }
            action
                .controlSize(.small)
        }
        .font(.callout)
        .padding(.horizontal, 10)
        .padding(.vertical, 6)
    }
}

/// Hands over the `NSWindow` a view is in, once it has one.
struct WindowReader: NSViewRepresentable {
    let onWindow: @MainActor (NSWindow) -> Void

    func makeNSView(context: Context) -> NSView { ReaderView(onWindow) }
    func updateNSView(_ view: NSView, context: Context) {}

    private final class ReaderView: NSView {
        let onWindow: @MainActor (NSWindow) -> Void

        init(_ onWindow: @escaping @MainActor (NSWindow) -> Void) {
            self.onWindow = onWindow
            super.init(frame: .zero)
        }

        @available(*, unavailable)
        required init?(coder: NSCoder) { fatalError("not used") }

        override func viewDidMoveToWindow() {
            super.viewDidMoveToWindow()
            if let window { onWindow(window) }
        }
    }
}

/// The `MenuBarExtra` panel. It stays open over a window the app opens
/// from it unless closed, and would cover that window.
@MainActor
enum MenuBarPanel {
    static var window: NSWindow? {
        NSApplication.shared.windows.first { $0.isVisible && $0.className.contains("MenuBarExtraWindow") }
    }

    static var isOpen: Bool { window != nil }

    /// Closes it the way a click on the icon does. Closing the window
    /// directly leaves SwiftUI thinking it is open, and the next click on
    /// the icon then does nothing.
    static func close() {
        guard isOpen else { return }
        StatusItemDropTarget.statusButton()?.performClick(nil)
    }
}

/// The open browser windows, to bring the right one forward. A menu bar app
/// is never the active app by itself, so a window it opens would otherwise
/// stay behind the app the user is in.
@MainActor
enum BrowserWindows {
    private static var windows: [String: WeakWindow] = [:]

    private struct WeakWindow {
        weak var window: NSWindow?
    }

    static func register(_ window: NSWindow, for deviceId: String) {
        let isNew = windows[deviceId]?.window !== window
        windows[deviceId] = WeakWindow(window: window)
        if isNew { bringForward(window) }
    }

    /// Opens or focuses the browser for a device.
    static func show(_ deviceId: String, open: () -> Void) {
        MenuBarPanel.close()
        NSApplication.shared.activate()
        open()
        DispatchQueue.main.async {
            if let window = windows[deviceId]?.window { bringForward(window) }
        }
    }

    private static func bringForward(_ window: NSWindow) {
        NSApplication.shared.activate()
        window.makeKeyAndOrderFront(nil)
        // `activate()` is only a request since macOS 14 and may be turned
        // down; ordering the window front regardless covers that.
        window.orderFrontRegardless()
    }
}
