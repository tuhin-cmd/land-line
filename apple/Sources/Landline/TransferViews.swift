import AppKit
import LandlineKit
import SwiftUI
import UniformTypeIdentifiers

/// Running and recently ended transfers, kept short: the panel is not a
/// download manager.
struct TransferSection: View {
    let entries: [TransferEntry]
    /// Rows beyond this collapse into a count.
    static let limit = 4

    @Environment(AppModel.self) private var model

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            HStack(alignment: .firstTextBaseline) {
                Text("Transfers")
                    .font(.subheadline.weight(.semibold))
                    .foregroundStyle(.secondary)
                Spacer()
                if entries.contains(where: { !$0.isActive }) {
                    Button("Clear") { model.clearEndedTransfers() }
                        .buttonStyle(.borderless)
                        .font(.subheadline)
                        .help("Remove finished transfers from the list")
                }
            }
            .padding(.horizontal, 6)
            .padding(.bottom, 2)
            ForEach(entries.prefix(Self.limit)) { entry in
                TransferRow(entry: entry)
            }
            if entries.count > Self.limit {
                Text("\(entries.count - Self.limit) more")
                    .font(.subheadline)
                    .foregroundStyle(.secondary)
                    .padding(.horizontal, 6)
                    .padding(.top, 2)
            }
        }
    }
}

private struct TransferRow: View {
    let entry: TransferEntry
    @Environment(AppModel.self) private var model
    @State private var hovering = false

    var body: some View {
        let deviceName = model.devices.first { $0.id == entry.transfer.deviceId }?.name
        let content = TransferRowContent(entry, deviceName: deviceName)
        HStack(spacing: 8) {
            FileIcon(name: entry.transfer.fileName, size: 26)
                .overlay(alignment: .bottomTrailing) {
                    Image(systemName: content.symbol)
                        .symbolRenderingMode(.palette)
                        .foregroundStyle(.white, Color.accentColor)
                        .font(.system(size: 11, weight: .semibold))
                        .background(Circle().fill(.background).padding(1))
                        .offset(x: 3, y: 3)
                }
                .accessibilityHidden(true)
            VStack(alignment: .leading, spacing: 2) {
                Text(content.name)
                    .font(.subheadline)
                    .lineLimit(1)
                    .truncationMode(.middle)
                if content.showsProgress {
                    // Two views rather than an optional value: a bar that
                    // starts indeterminate does not redraw its fill later.
                    Group {
                        if let fraction = content.fraction {
                            ProgressView(value: fraction)
                        } else {
                            ProgressView()
                        }
                    }
                        .progressViewStyle(.linear)
                        .controlSize(.small)
                        .accessibilityLabel(entry.transfer.direction == .incoming ? "Receiving" : "Sending")
                }
                HStack(spacing: 3) {
                    if content.failed {
                        Image(systemName: "exclamationmark.triangle.fill")
                            .foregroundStyle(.orange)
                    }
                    Text(content.detail)
                        .lineLimit(2)
                        .fixedSize(horizontal: false, vertical: true)
                }
                .font(.caption)
                .foregroundStyle(.secondary)
                .monospacedDigit()
            }
            .accessibilityElement(children: .combine)
            Spacer(minLength: 4)
            trailing
        }
        .padding(.vertical, 4)
        .padding(.horizontal, 6)
        .background(
            hovering ? AnyShapeStyle(.quinary) : AnyShapeStyle(.clear),
            in: RoundedRectangle(cornerRadius: 6, style: .continuous))
        .onHover { hovering = $0 }
    }

    @ViewBuilder
    private var trailing: some View {
        if entry.canCancel {
            Button {
                model.cancelTransfer(entry)
            } label: {
                Image(systemName: "xmark.circle.fill")
                    .font(.system(size: 14))
            }
            .buttonStyle(.borderless)
            .foregroundStyle(.secondary)
            .help("Stop")
            .accessibilityLabel("Stop \(entry.transfer.fileName)")
        } else if let path = entry.finishedPath, FileManager.default.fileExists(atPath: path) {
            Button {
                model.workspace.reveal([URL(fileURLWithPath: path)])
            } label: {
                Image(systemName: "magnifyingglass.circle.fill")
                    .font(.system(size: 14))
            }
            .buttonStyle(.borderless)
            .foregroundStyle(.secondary)
            .help("Show in Finder")
            .accessibilityLabel("Show \(entry.transfer.fileName) in Finder")
        }
    }
}

/// The icon Finder shows for a file of this name's type.
struct FileIcon: View {
    let name: String
    var isFolder = false
    let size: CGFloat

    var body: some View {
        Image(nsImage: FileIcons.icon(forName: name, isFolder: isFolder))
            .resizable()
            .interpolation(.high)
            .frame(width: size, height: size)
    }
}

@MainActor
enum FileIcons {
    private static var cache: [String: NSImage] = [:]

    /// By extension, since the file itself is on the phone or not yet here.
    static func icon(forName name: String, isFolder: Bool) -> NSImage {
        let ext = isFolder ? "/folder" : (name as NSString).pathExtension.lowercased()
        if let cached = cache[ext] { return cached }
        let type: UTType = isFolder ? .folder : (UTType(filenameExtension: ext) ?? .data)
        let icon = NSWorkspace.shared.icon(for: type)
        cache[ext] = icon
        return icon
    }

    /// "JPEG image", "Folder": Finder's Kind column.
    static func kind(forName name: String, isFolder: Bool) -> String {
        // UTType calls it "folder"; Finder's Kind column says "Folder".
        if isFolder { return "Folder" }
        let ext = (name as NSString).pathExtension
        guard !ext.isEmpty, let type = UTType(filenameExtension: ext) else { return "Document" }
        return type.localizedDescription ?? "Document"
    }
}
