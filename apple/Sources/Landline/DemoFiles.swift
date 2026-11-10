import AppKit
import LandlineKit

/// Where demo mode keeps files. Everything it writes goes under one
/// temporary folder, so a demo can never touch the user's own.
enum DemoFiles {
    static let root = FileManager.default.temporaryDirectory
        .appendingPathComponent("io.github.tuhin-cmd.landline.demo", isDirectory: true)
    static let receivedFolder = root.appendingPathComponent("Received", isDirectory: true)

    static func clean() {
        try? FileManager.default.removeItem(at: root)
    }

    /// True if writing to `url` keeps the demo inside its temporary folder.
    static func isWritable(_ url: URL) -> Bool {
        let path = url.standardizedFileURL.resolvingSymlinksInPath().path
        let temp = FileManager.default.temporaryDirectory.standardizedFileURL.resolvingSymlinksInPath().path
        return path.hasPrefix(temp + "/")
    }

    /// Writes a stand-in for a phone file: a picture for images, a page for
    /// PDFs, text for anything else, so Quick Look has something to show.
    static func writeSample(named name: String, to url: URL) throws {
        guard isWritable(url) else {
            throw LandlineError.Storage(message: "Demo mode saves files only inside its temporary folder.")
        }
        try FileManager.default.createDirectory(
            at: url.deletingLastPathComponent(), withIntermediateDirectories: true)
        let ext = (name as NSString).pathExtension.lowercased()
        let data: Data
        switch ext {
        case "jpg", "jpeg", "png", "heic":
            data = picture(titled: name, png: ext == "png")
        case "pdf":
            data = page(titled: name)
        default:
            data = Data("A demo file standing in for “\(name)”. Nothing here came from a phone.\n".utf8)
        }
        try data.write(to: url, options: .atomic)
    }

    private static func picture(titled title: String, png: Bool) -> Data {
        let size = NSSize(width: 1200, height: 900)
        let image = NSImage(size: size, flipped: false) { rect in
            let hue = CGFloat(abs(title.hashValue % 360)) / 360
            NSGradient(
                starting: NSColor(hue: hue, saturation: 0.55, brightness: 0.85, alpha: 1),
                ending: NSColor(hue: (hue + 0.15).truncatingRemainder(dividingBy: 1), saturation: 0.65, brightness: 0.45, alpha: 1)
            )?.draw(in: rect, angle: -60)
            let attributes: [NSAttributedString.Key: Any] = [
                .font: NSFont.systemFont(ofSize: 56, weight: .semibold),
                .foregroundColor: NSColor.white,
            ]
            let text = NSAttributedString(string: title, attributes: attributes)
            let textSize = text.size()
            text.draw(at: NSPoint(x: (rect.width - textSize.width) / 2, y: (rect.height - textSize.height) / 2))
            return true
        }
        guard let tiff = image.tiffRepresentation, let bitmap = NSBitmapImageRep(data: tiff) else { return Data() }
        return bitmap.representation(using: png ? .png : .jpeg, properties: [.compressionFactor: 0.85]) ?? Data()
    }

    private static func page(titled title: String) -> Data {
        let data = NSMutableData()
        var box = CGRect(x: 0, y: 0, width: 595, height: 842)
        guard let consumer = CGDataConsumer(data: data), let context = CGContext(consumer: consumer, mediaBox: &box, nil)
        else { return Data() }
        context.beginPDFPage(nil)
        NSGraphicsContext.saveGraphicsState()
        NSGraphicsContext.current = NSGraphicsContext(cgContext: context, flipped: false)
        NSAttributedString(string: title, attributes: [.font: NSFont.systemFont(ofSize: 28, weight: .bold)])
            .draw(at: NSPoint(x: 60, y: 740))
        NSAttributedString(
            string: "A demo document. Nothing here came from a phone.",
            attributes: [.font: NSFont.systemFont(ofSize: 14), .foregroundColor: NSColor.darkGray]
        ).draw(at: NSPoint(x: 60, y: 700))
        NSGraphicsContext.restoreGraphicsState()
        context.endPDFPage()
        context.closePDF()
        return data as Data
    }
}

/// A made-up phone file system for `--demo`: folders, files of many types
/// and sizes, hidden files, a folder the phone refuses to open, and one that
/// takes a few seconds to list.
@MainActor
final class DemoStorage {
    static let internalRoot = StorageRoot(name: "Internal storage", path: "/storage/emulated/0")
    static let cardRoot = StorageRoot(name: "SD card", path: "/storage/3A1F-0E2B")
    static let deniedFolder = "/storage/emulated/0/Android/data"
    static let slowFolder = "/storage/emulated/0/Slow Folder"

    private var folders: [String: [RemoteEntry]] = [:]

    init() {
        let now = Int64(Date.now.timeIntervalSince1970)
        let day: Int64 = 86_400
        func file(_ folder: String, _ name: String, _ size: UInt64, daysAgo: Double) -> RemoteEntry {
            RemoteEntry(
                name: name, path: RemotePath.join(folder, name), isDir: false, size: size,
                modifiedSecs: now - Int64(daysAgo * Double(day)))
        }
        func folder(_ parent: String, _ name: String, daysAgo: Double) -> RemoteEntry {
            RemoteEntry(
                name: name, path: RemotePath.join(parent, name), isDir: true, size: 0,
                modifiedSecs: now - Int64(daysAgo * Double(day)))
        }
        let mb: UInt64 = 1_000_000
        let home = Self.internalRoot.path
        let camera = home + "/DCIM/Camera"
        folders[home] = [
            folder(home, ".thumbnails", daysAgo: 0.2),
            folder(home, "Alarms", daysAgo: 400),
            folder(home, "Android", daysAgo: 30),
            folder(home, "DCIM", daysAgo: 0.3),
            folder(home, "Documents", daysAgo: 2),
            folder(home, "Download", daysAgo: 0.05),
            folder(home, "Music", daysAgo: 60),
            folder(home, "Slow Folder", daysAgo: 9),
            file(home, ".nomedia", 0, daysAgo: 400),
        ]
        folders[home + "/Alarms"] = []
        folders[home + "/Android"] = [
            folder(home + "/Android", "data", daysAgo: 1), folder(home + "/Android", "media", daysAgo: 1),
        ]
        folders[home + "/Android/media"] = []
        folders[home + "/DCIM"] = [
            folder(home + "/DCIM", "Camera", daysAgo: 0.3), folder(home + "/DCIM", "Screenshots", daysAgo: 1.5),
        ]
        var photos: [RemoteEntry] = []
        for index in 1...12 {
            let name = String(format: "IMG_202611%02d_%04d.jpg", index, 1830 + index * 7)
            let size = UInt64(2_100_000 + index * 310_000)
            photos.append(file(camera, name, size, daysAgo: Double(13 - index) * 0.9))
        }
        photos.append(file(camera, "VID_20261104_0915.mp4", 184 * mb, daysAgo: 6))
        photos.append(file(camera, "VID_20261108_1742.mp4", 1_420 * mb, daysAgo: 2))
        folders[camera] = photos
        let shots = home + "/DCIM/Screenshots"
        folders[shots] = [
            file(shots, "Screenshot_20261109-081512.png", 1_240_000, daysAgo: 1.5),
            file(shots, "Screenshot_20261109-221047.png", 980_000, daysAgo: 1.1),
        ]
        let documents = home + "/Documents"
        folders[documents] = [
            file(documents, "Lease agreement.pdf", 2_380_000, daysAgo: 40),
            file(documents, "Budget 2026.xlsx", 84_000, daysAgo: 3),
            file(documents, "Notes.txt", 2_400, daysAgo: 2),
            file(documents, "Recipes.md", 11_200, daysAgo: 12),
            file(documents, "Slides.pptx", 6_800_000, daysAgo: 18),
        ]
        let download = home + "/Download"
        folders[download] = [
            file(download, "Boarding pass.pdf", 210_000, daysAgo: 0.05),
            file(download, "app-release.apk", 48 * mb, daysAgo: 4),
            file(download, "Trip photos.zip", 312 * mb, daysAgo: 7),
            file(download, "Podcast episode 41.mp3", 58 * mb, daysAgo: 9),
            file(download, "contacts.vcf", 18_400, daysAgo: 21),
        ]
        let music = home + "/Music"
        folders[music] = [
            file(music, "Morning Walk.mp3", 7_200_000, daysAgo: 60),
            file(music, "Night Train.flac", 31_000_000, daysAgo: 61),
        ]
        folders[Self.slowFolder] = [
            file(Self.slowFolder, "Archive 2019.zip", 920 * mb, daysAgo: 300),
            file(Self.slowFolder, "Old notes.txt", 5_100, daysAgo: 280),
        ]
        folders[home + "/.thumbnails"] = [file(home + "/.thumbnails", "1730000000.jpg", 24_000, daysAgo: 0.2)]

        let card = Self.cardRoot.path
        folders[card] = [folder(card, "Movies", daysAgo: 90), folder(card, "Music", daysAgo: 120)]
        folders[card + "/Movies"] = [file(card + "/Movies", "Holiday 2025.mp4", 2_100 * mb, daysAgo: 90)]
        folders[card + "/Music"] = []
    }

    var roots: [StorageRoot] { [Self.internalRoot, Self.cardRoot] }

    func list(_ path: String) async throws -> [RemoteEntry] {
        let path = RemotePath.normalized(path)
        try await Task.sleep(for: .milliseconds(path == Self.slowFolder ? 4_000 : 250))
        if path == Self.deniedFolder || path.hasPrefix(Self.deniedFolder + "/") {
            throw LandlineError.Storage(
                message: "The phone refused to open “data”. Android keeps other apps’ data private, even from KDE Connect.")
        }
        guard let entries = folders[path] else {
            throw LandlineError.Storage(message: "“\(RemotePath.name(of: path))” no longer exists on the phone.")
        }
        return entries
    }

    func entry(at path: String) -> RemoteEntry? {
        let parent = (path as NSString).deletingLastPathComponent
        return folders[parent]?.first { $0.path == path }
    }

    func add(_ entry: RemoteEntry, to folder: String) {
        var list = folders[folder] ?? []
        list.removeAll { $0.path == entry.path }
        list.append(entry)
        folders[folder] = list
    }
}
