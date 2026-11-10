import AppKit
import LandlineKit

/// Open and save panels. A menu bar app has no Dock icon and is never
/// frontmost by itself, so each activates the app first; otherwise the panel
/// would open behind the app the user is working in.
@MainActor
enum FilePanels {
    static func chooseFilesToSend(to device: Device, model: AppModel) {
        let panel = NSOpenPanel()
        panel.allowsMultipleSelection = true
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.resolvesAliases = true
        panel.prompt = "Send"
        panel.message = "Choose files to send to \(device.name)."
        guard run(panel) else { return }
        model.sendFiles(panel.urls, to: device.id)
    }

    static func chooseFilesToUpload(into folderName: String) -> [URL]? {
        let panel = NSOpenPanel()
        panel.allowsMultipleSelection = true
        panel.canChooseFiles = true
        panel.canChooseDirectories = false
        panel.prompt = "Upload"
        panel.message = "Choose files to upload to “\(folderName)”."
        return run(panel) ? panel.urls : nil
    }

    static func chooseDownloadFolder(for count: Int) -> URL? {
        let panel = NSOpenPanel()
        panel.canChooseFiles = false
        panel.canChooseDirectories = true
        panel.canCreateDirectories = true
        panel.allowsMultipleSelection = false
        panel.prompt = "Download"
        panel.message = count == 1 ? "Choose where to save the file." : "Choose where to save the \(count) files."
        return run(panel) ? panel.url : nil
    }

    static func chooseLogDestination(suggestedName: String) -> URL? {
        let panel = NSSavePanel()
        panel.nameFieldStringValue = suggestedName
        panel.allowedContentTypes = [.plainText]
        panel.canCreateDirectories = true
        panel.message = "The log holds connection and pairing events, never message contents."
        return run(panel) ? panel.url : nil
    }

    private static func run(_ panel: NSSavePanel) -> Bool {
        MenuBarPanel.close()
        NSApplication.shared.activate()
        panel.level = .modalPanel
        return panel.runModal() == .OK
    }
}

/// File URLs from a SwiftUI drop.
enum FileDrop {
    @MainActor
    static func urls(from providers: [NSItemProvider]) async -> [URL] {
        var urls: [URL] = []
        for provider in providers where provider.canLoadObject(ofClass: URL.self) {
            let url: URL? = await withCheckedContinuation { continuation in
                _ = provider.loadObject(ofClass: URL.self) { url, _ in
                    continuation.resume(returning: url)
                }
            }
            if let url, url.isFileURL { urls.append(url) }
        }
        return urls
    }
}
