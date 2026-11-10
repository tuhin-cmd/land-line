import AppKit
import LandlineKit

/// Lets files be dropped on the menu bar icon.
///
/// `MenuBarExtra` does not expose its status item, but the item's button
/// sits in a window of its own, and a window registered for dragged types
/// passes the drag messages to its delegate. That is public API; the only
/// assumption is finding the window, which is the app's one status bar
/// window. If SwiftUI ever gives that window a delegate of its own, this
/// leaves it alone and the icon simply does not take drops.
@MainActor
final class StatusItemDropTarget: NSObject, NSWindowDelegate, NSDraggingDestination {
    private weak var model: AppModel?
    private weak var window: NSWindow?
    private var attempts = 0
    private var springTimer: Timer?

    init(model: AppModel) {
        self.model = model
        super.init()
    }

    /// The status item appears after launch, so this looks for it a few
    /// times, and again when screens change, which can rebuild it.
    func install() {
        attempts = 0
        tryAttach()
        NotificationCenter.default.addObserver(
            forName: NSApplication.didChangeScreenParametersNotification, object: nil, queue: .main
        ) { [weak self] _ in
            MainActor.assumeIsolated { self?.install() }
        }
    }

    private func tryAttach() {
        if attach() { return }
        attempts += 1
        guard attempts < 20 else {
            Log.app.info("No status item window found; dropping on the menu bar icon is off")
            return
        }
        DispatchQueue.main.asyncAfter(deadline: .now() + 0.5) { [weak self] in self?.tryAttach() }
    }

    private func attach() -> Bool {
        if let window, window.isVisible, window.delegate === self { return true }
        guard let window = NSApplication.shared.windows.first(where: { Self.button(in: $0) != nil }) else {
            return false
        }
        guard window.delegate == nil || window.delegate === self else {
            Log.app.info("The status item window has a delegate already; not taking drops on the icon")
            return true
        }
        window.registerForDraggedTypes([.fileURL])
        window.delegate = self
        self.window = window
        return true
    }

    /// The app's status item button, wherever SwiftUI put it.
    static func statusButton() -> NSStatusBarButton? {
        NSApplication.shared.windows.lazy.compactMap(button(in:)).first
    }

    private static func button(in window: NSWindow) -> NSStatusBarButton? {
        guard window.className.contains("StatusBarWindow") else { return nil }
        return find(in: window.contentView)
    }

    private static func find(in view: NSView?) -> NSStatusBarButton? {
        guard let view else { return nil }
        if let button = view as? NSStatusBarButton { return button }
        for subview in view.subviews {
            if let button = find(in: subview) { return button }
        }
        return nil
    }

    private var button: NSStatusBarButton? { window.flatMap(Self.button) }

    // MARK: Dragging

    func draggingEntered(_ sender: any NSDraggingInfo) -> NSDragOperation {
        guard Self.hasFiles(sender), let model else { return [] }
        button?.highlight(true)
        if model.sendableDevices.count != 1 {
            // Spring-loaded, like a folder in Finder: holding the files
            // over the icon opens the panel, so they can be dropped on the
            // device they are for.
            springTimer?.invalidate()
            springTimer = Timer.scheduledTimer(withTimeInterval: 0.6, repeats: false) { [weak self] _ in
                MainActor.assumeIsolated { self?.openPanel() }
            }
        }
        return .copy
    }

    func draggingUpdated(_ sender: any NSDraggingInfo) -> NSDragOperation {
        Self.hasFiles(sender) ? .copy : []
    }

    func draggingExited(_ sender: (any NSDraggingInfo)?) {
        springTimer?.invalidate()
        button?.highlight(false)
    }

    func prepareForDragOperation(_ sender: any NSDraggingInfo) -> Bool {
        Self.hasFiles(sender)
    }

    func performDragOperation(_ sender: any NSDraggingInfo) -> Bool {
        springTimer?.invalidate()
        button?.highlight(false)
        guard let model else { return false }
        let urls = sender.draggingPasteboard.readObjects(
            forClasses: [NSURL.self], options: [.urlReadingFileURLsOnly: true]) as? [URL] ?? []
        let devices = model.sendableDevices
        guard devices.count == 1, let device = devices.first, !urls.isEmpty else {
            // No single place to send to: show the panel, which lists the
            // devices (or says none is connected).
            if devices.isEmpty {
                model.notice = Notice(
                    title: "No phone is connected",
                    message: "Files can be sent once a paired phone is connected.")
            }
            openPanel()
            return false
        }
        model.sendFiles(urls, to: device.id)
        return true
    }

    func concludeDragOperation(_ sender: (any NSDraggingInfo)?) {
        button?.highlight(false)
    }

    private func openPanel() {
        guard !MenuBarPanel.isOpen else { return }
        button?.performClick(nil)
    }

    private static func hasFiles(_ info: any NSDraggingInfo) -> Bool {
        info.draggingPasteboard.canReadObject(forClasses: [NSURL.self], options: [.urlReadingFileURLsOnly: true])
    }
}
