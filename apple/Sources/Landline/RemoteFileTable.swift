import AppKit
import LandlineKit
import SwiftUI
import UniformTypeIdentifiers

/// The browser's file list. An `NSTableView` rather than SwiftUI's `Table`:
/// dragging a row to Finder needs a file promise (the file is still on the
/// phone when the drag starts), which only AppKit's drag source offers, and
/// the table also gives Finder's keys and drop behaviour for free.
struct RemoteFileTable: NSViewRepresentable {
    let entries: [RemoteEntry]
    @Binding var selection: Set<String>
    @Binding var sort: BrowserSort
    let actions: Actions

    @MainActor
    struct Actions {
        /// Double-click: enters a folder or downloads a file.
        var open: (RemoteEntry) -> Void
        /// Return: the same for the selection.
        var openSelection: () -> Void
        var download: ([RemoteEntry]) -> Void
        var downloadTo: ([RemoteEntry]) -> Void
        /// Space.
        var togglePreview: () -> Void
        /// Escape.
        var cancelPreview: () -> Void
        var preview: (RemoteEntry) -> Void
        /// Fetches a file dragged to Finder into the folder Finder asked for.
        var deliver: (RemoteEntry, URL) async throws -> URL
        /// Files dropped on the list.
        var upload: ([URL]) -> Void
    }

    func makeCoordinator() -> Coordinator { Coordinator(self) }

    func makeNSView(context: Context) -> NSScrollView {
        let table = BrowserTableView()
        table.style = .fullWidth
        table.usesAlternatingRowBackgroundColors = true
        table.allowsMultipleSelection = true
        table.allowsColumnReordering = true
        // Name takes up any change in width, as in Finder's list view.
        table.columnAutoresizingStyle = .firstColumnOnlyAutoresizingStyle
        table.rowSizeStyle = .default
        table.intercellSpacing = NSSize(width: 6, height: 2)
        for column in Column.allCases {
            let tableColumn = NSTableColumn(identifier: column.identifier)
            tableColumn.title = column.title
            tableColumn.width = column.width
            tableColumn.minWidth = column.minWidth
            if let key = column.sortKey {
                tableColumn.sortDescriptorPrototype = NSSortDescriptor(key: key.rawValue, ascending: key != .modified)
            }
            if column == .size { tableColumn.headerCell.alignment = .right }
            table.addTableColumn(tableColumn)
        }
        table.dataSource = context.coordinator
        table.delegate = context.coordinator
        table.target = context.coordinator
        table.doubleAction = #selector(Coordinator.doubleClicked(_:))
        table.registerForDraggedTypes([.fileURL])
        table.setDraggingSourceOperationMask(.copy, forLocal: false)
        table.setDraggingSourceOperationMask([], forLocal: true)
        let menu = NSMenu()
        menu.delegate = context.coordinator
        table.menu = menu
        table.onKey = { [weak coordinator = context.coordinator] key in coordinator?.handle(key) ?? false }

        let scroll = NSScrollView()
        scroll.documentView = table
        scroll.hasVerticalScroller = true
        scroll.autohidesScrollers = true
        scroll.borderType = .noBorder
        context.coordinator.table = table
        context.coordinator.apply(self, force: true)
        return scroll
    }

    func updateNSView(_ scroll: NSScrollView, context: Context) {
        context.coordinator.apply(self, force: false)
    }

    enum Column: String, CaseIterable {
        case name, modified, size, kind

        var identifier: NSUserInterfaceItemIdentifier { NSUserInterfaceItemIdentifier(rawValue) }
        var title: String {
            switch self {
            case .name: "Name"
            case .modified: "Date Modified"
            case .size: "Size"
            case .kind: "Kind"
            }
        }
        var width: CGFloat {
            switch self {
            case .name: 250
            case .modified: 170
            case .size: 72
            case .kind: 120
            }
        }
        var minWidth: CGFloat {
            switch self {
            case .name: 140
            case .modified: 110
            case .size: 60
            case .kind: 70
            }
        }
        var sortKey: BrowserSort.Key? {
            switch self {
            case .name: .name
            case .modified: .modified
            case .size: .size
            case .kind: nil
            }
        }
    }

    @MainActor
    final class Coordinator: NSObject, NSTableViewDataSource, NSTableViewDelegate, NSMenuDelegate,
        NSFilePromiseProviderDelegate
    {
        var parent: RemoteFileTable {
            didSet { delivery.deliver = parent.actions.deliver }
        }
        weak var table: BrowserTableView?
        /// Reachable from the promise callback, which AppKit declares
        /// nonisolated.
        private nonisolated let delivery = Delivery()
        private var rows: [RemoteEntry] = []
        private var applying = false

        init(_ parent: RemoteFileTable) {
            self.parent = parent
            delivery.deliver = parent.actions.deliver
        }

        func apply(_ parent: RemoteFileTable, force: Bool) {
            self.parent = parent
            guard let table else { return }
            applying = true
            defer { applying = false }
            let descriptor = NSSortDescriptor(key: parent.sort.key.rawValue, ascending: parent.sort.ascending)
            if table.sortDescriptors.first?.key != descriptor.key
                || table.sortDescriptors.first?.ascending != descriptor.ascending
            {
                table.sortDescriptors = [descriptor]
            }
            if force || rows != parent.entries {
                let scrollToTop = rows.first?.path != parent.entries.first?.path || rows.isEmpty
                rows = parent.entries
                table.reloadData()
                if scrollToTop { table.scrollRowToVisible(0) }
            }
            let wanted = IndexSet(rows.indices.filter { parent.selection.contains(rows[$0].path) })
            if wanted != table.selectedRowIndexes {
                table.selectRowIndexes(wanted, byExtendingSelection: false)
            }
        }

        private var selectedRows: [RemoteEntry] {
            table?.selectedRowIndexes.compactMap { rows.indices.contains($0) ? rows[$0] : nil } ?? []
        }

        // MARK: Data

        func numberOfRows(in tableView: NSTableView) -> Int { rows.count }

        func tableView(_ tableView: NSTableView, viewFor tableColumn: NSTableColumn?, row: Int) -> NSView? {
            guard let tableColumn, let column = Column(rawValue: tableColumn.identifier.rawValue) else { return nil }
            let entry = rows[row]
            let cell = tableView.makeView(withIdentifier: tableColumn.identifier, owner: nil) as? NSTableCellView
                ?? makeCell(column)
            switch column {
            case .name:
                cell.textField?.stringValue = entry.name
                cell.imageView?.image = FileIcons.icon(forName: entry.name, isFolder: entry.isDir)
            case .modified:
                cell.textField?.stringValue = entry.modifiedSecs.map {
                    Self.dateFormatter.string(from: Date(timeIntervalSince1970: TimeInterval($0)))
                } ?? "--"
            case .size:
                cell.textField?.stringValue = entry.isDir ? "--" : ByteFormat.size(entry.size)
            case .kind:
                cell.textField?.stringValue = FileIcons.kind(forName: entry.name, isFolder: entry.isDir)
            }
            let hidden = BrowserSort.isHidden(entry)
            cell.textField?.textColor = column == .name && !hidden ? .labelColor : .secondaryLabelColor
            cell.imageView?.alphaValue = hidden ? 0.5 : 1
            return cell
        }

        private func makeCell(_ column: Column) -> NSTableCellView {
            let cell = NSTableCellView()
            cell.identifier = column.identifier
            let text = NSTextField(labelWithString: "")
            text.lineBreakMode = column == .name ? .byTruncatingMiddle : .byTruncatingTail
            text.translatesAutoresizingMaskIntoConstraints = false
            text.font = .systemFont(ofSize: NSFont.systemFontSize)
            text.alignment = column == .size ? .right : .left
            cell.addSubview(text)
            cell.textField = text
            var constraints = [
                text.centerYAnchor.constraint(equalTo: cell.centerYAnchor),
                text.trailingAnchor.constraint(equalTo: cell.trailingAnchor, constant: -2),
            ]
            if column == .name {
                let image = NSImageView()
                image.translatesAutoresizingMaskIntoConstraints = false
                image.imageScaling = .scaleProportionallyUpOrDown
                cell.addSubview(image)
                cell.imageView = image
                constraints += [
                    image.leadingAnchor.constraint(equalTo: cell.leadingAnchor, constant: 2),
                    image.centerYAnchor.constraint(equalTo: cell.centerYAnchor),
                    image.widthAnchor.constraint(equalToConstant: 16),
                    image.heightAnchor.constraint(equalToConstant: 16),
                    text.leadingAnchor.constraint(equalTo: image.trailingAnchor, constant: 6),
                ]
            } else {
                constraints.append(text.leadingAnchor.constraint(equalTo: cell.leadingAnchor, constant: 2))
            }
            NSLayoutConstraint.activate(constraints)
            return cell
        }

        /// "Today at 14:32", "10 Nov 2026 at 09:15": Finder's list view.
        private static let dateFormatter: DateFormatter = {
            let formatter = DateFormatter()
            formatter.dateStyle = .medium
            formatter.timeStyle = .short
            formatter.doesRelativeDateFormatting = true
            return formatter
        }()

        // MARK: Selection, sorting, keys

        func tableViewSelectionDidChange(_ notification: Notification) {
            guard !applying else { return }
            parent.selection = Set(selectedRows.map(\.path))
        }

        func tableView(_ tableView: NSTableView, sortDescriptorsDidChange oldDescriptors: [NSSortDescriptor]) {
            guard !applying, let descriptor = tableView.sortDescriptors.first, let key = descriptor.key,
                let sortKey = BrowserSort.Key(rawValue: key)
            else { return }
            parent.sort = BrowserSort(key: sortKey, ascending: descriptor.ascending)
        }

        @objc func doubleClicked(_ sender: NSTableView) {
            let row = sender.clickedRow
            guard rows.indices.contains(row) else { return }
            parent.actions.open(rows[row])
        }

        func handle(_ key: BrowserTableView.Key) -> Bool {
            switch key {
            case .open:
                guard !selectedRows.isEmpty else { return false }
                parent.actions.openSelection()
            case .preview:
                parent.actions.togglePreview()
            case .escape:
                parent.actions.cancelPreview()
            }
            return true
        }

        // MARK: Context menu

        func menuNeedsUpdate(_ menu: NSMenu) {
            menu.removeAllItems()
            guard let table else { return }
            // As in Finder: right-clicking inside the selection acts on all
            // of it, outside it on the clicked row alone. A menu opened from
            // the keyboard or VoiceOver has no clicked row and acts on the
            // selection.
            let clickedRow = table.clickedRow
            let targets: [RemoteEntry]
            if rows.indices.contains(clickedRow) {
                targets = table.selectedRowIndexes.contains(clickedRow) ? selectedRows : [rows[clickedRow]]
            } else {
                targets = selectedRows
            }
            let actions = parent.actions
            if targets.count == 1, let folder = targets.first, folder.isDir {
                menu.addItem(ClosureMenuItem("Open") { actions.open(folder) })
                return
            }
            let files = targets.filter { !$0.isDir }
            guard !files.isEmpty else { return }
            menu.addItem(ClosureMenuItem("Download") { actions.download(files) })
            menu.addItem(ClosureMenuItem("Download To…") { actions.downloadTo(files) })
            if files.count == 1, let file = files.first {
                menu.addItem(.separator())
                menu.addItem(ClosureMenuItem("Quick Look “\(file.name)”") { actions.preview(file) })
            }
        }

        // MARK: Drag out

        func tableView(_ tableView: NSTableView, pasteboardWriterForRow row: Int) -> (any NSPasteboardWriting)? {
            let entry = rows[row]
            guard !entry.isDir else { return nil }
            let ext = (entry.name as NSString).pathExtension
            let type = UTType(filenameExtension: ext) ?? .data
            let provider = NSFilePromiseProvider(fileType: type.identifier, delegate: self)
            provider.userInfo = entry
            return provider
        }

        func filePromiseProvider(_ provider: NSFilePromiseProvider, fileNameForType fileType: String) -> String {
            (provider.userInfo as? RemoteEntry)?.name ?? "File"
        }

        /// Runs on the main queue (see `operationQueue(for:)`). Finder waits
        /// for the completion handler, which is called once the download
        /// has landed.
        nonisolated func filePromiseProvider(
            _ provider: NSFilePromiseProvider, writePromiseTo url: URL,
            completionHandler: @escaping @Sendable ((any Error)?) -> Void
        ) {
            guard let entry = provider.userInfo as? RemoteEntry else {
                completionHandler(CocoaError(.fileWriteUnknown))
                return
            }
            let delivery = delivery
            Task { @MainActor in
                do {
                    guard let deliver = delivery.deliver else { throw CocoaError(.fileWriteUnknown) }
                    _ = try await deliver(entry, url)
                    completionHandler(nil)
                } catch {
                    completionHandler(error)
                }
            }
        }

        nonisolated func operationQueue(for filePromiseProvider: NSFilePromiseProvider) -> OperationQueue {
            .main
        }

        // MARK: Drop in

        func tableView(
            _ tableView: NSTableView, validateDrop info: any NSDraggingInfo, proposedRow row: Int,
            proposedDropOperation dropOperation: NSTableView.DropOperation
        ) -> NSDragOperation {
            // Rows dragged within the window would be uploaded back into
            // the folder they came from.
            if (info.draggingSource as? NSTableView) === tableView { return [] }
            guard info.draggingPasteboard.canReadObject(forClasses: [NSURL.self], options: Self.fileURLsOnly)
            else { return [] }
            // Drops land in the folder being shown, not on a row.
            tableView.setDropRow(-1, dropOperation: .on)
            return .copy
        }

        func tableView(
            _ tableView: NSTableView, acceptDrop info: any NSDraggingInfo, row: Int,
            dropOperation: NSTableView.DropOperation
        ) -> Bool {
            let urls = info.draggingPasteboard.readObjects(forClasses: [NSURL.self], options: Self.fileURLsOnly)
                as? [URL] ?? []
            guard !urls.isEmpty else { return false }
            parent.actions.upload(urls)
            return true
        }

        private static let fileURLsOnly: [NSPasteboard.ReadingOptionKey: Any] = [.urlReadingFileURLsOnly: true]
    }
}

/// Holds the delivery closure for the promise callback. Only read and
/// written on the main actor; AppKit just cannot say so.
private final class Delivery: @unchecked Sendable {
    @MainActor var deliver: ((RemoteEntry, URL) async throws -> URL)?

    nonisolated init() {}
}

/// Passes Finder's list keys to the browser: Return opens, Space previews,
/// Escape closes the preview.
final class BrowserTableView: NSTableView {
    enum Key { case open, preview, escape }

    var onKey: ((Key) -> Bool)?

    override func keyDown(with event: NSEvent) {
        let modifiers = event.modifierFlags.intersection(.deviceIndependentFlagsMask)
        let key: Key? = switch (event.keyCode, modifiers.subtracting([.numericPad, .function])) {
        case (36, []), (76, []): .open
        case (125, .command): .open
        case (49, []): .preview
        case (53, []): .escape
        default: nil
        }
        if let key, onKey?(key) == true { return }
        super.keyDown(with: event)
    }
}

/// A menu item that runs a closure.
final class ClosureMenuItem: NSMenuItem {
    private let handler: @MainActor @Sendable () -> Void

    init(_ title: String, handler: @escaping @MainActor @Sendable () -> Void) {
        self.handler = handler
        super.init(title: title, action: #selector(run), keyEquivalent: "")
        target = self
    }

    @available(*, unavailable)
    required init(coder: NSCoder) { fatalError("not used") }

    /// Menu actions arrive on the main thread.
    @objc private func run() {
        let handler = handler
        MainActor.assumeIsolated { handler() }
    }
}
