// Draws the app icon and writes assets/AppIcon.icns.
//
//   swift scripts/make-icon.swift
//
// Run from the repository root after changing the drawing. The .icns is
// committed, so building the app does not need this.
import AppKit

/// The icon at `size` pixels: a blue rounded square with a white telephone
/// handset, laid out on Apple's 1024-point grid (824-point shape, centred).
func drawIcon(size: Int) -> NSBitmapImageRep {
    let rep = NSBitmapImageRep(
        bitmapDataPlanes: nil, pixelsWide: size, pixelsHigh: size, bitsPerSample: 8,
        samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
        bytesPerRow: 0, bitsPerPixel: 0)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
    let context = NSGraphicsContext.current!.cgContext
    context.scaleBy(x: CGFloat(size) / 1024, y: CGFloat(size) / 1024)

    // Background, with the soft shadow macOS icons sit on.
    let shape = NSBezierPath(
        roundedRect: NSRect(x: 100, y: 100, width: 824, height: 824), xRadius: 185, yRadius: 185)
    context.saveGState()
    context.setShadow(
        offset: CGSize(width: 0, height: -12), blur: 24,
        color: NSColor.black.withAlphaComponent(0.28).cgColor)
    NSColor(srgbRed: 0.01, green: 0.52, blue: 0.78, alpha: 1).setFill()
    shape.fill()
    context.restoreGState()
    NSGradient(
        starting: NSColor(srgbRed: 0.22, green: 0.74, blue: 0.97, alpha: 1),
        ending: NSColor(srgbRed: 0.01, green: 0.41, blue: 0.63, alpha: 1))!
        .draw(in: shape, angle: -90)

    // The handset: an arched grip with an earpiece and a mouthpiece, tilted.
    context.saveGState()
    context.translateBy(x: 512, y: 512)
    context.rotate(by: -35 * .pi / 180)
    context.scaleBy(x: 1.2, y: 1.2)
    context.translateBy(x: -512, y: -512)
    context.setShadow(
        offset: CGSize(width: 0, height: -8), blur: 18,
        color: NSColor.black.withAlphaComponent(0.22).cgColor)
    context.beginTransparencyLayer(auxiliaryInfo: nil)
    NSColor.white.set()

    // A shallow arch for the grip, with a block hanging from each end.
    let centre = NSPoint(x: 512, y: 205)
    let radius: CGFloat = 380
    let grip = NSBezierPath()
    grip.appendArc(withCenter: centre, radius: radius, startAngle: 62, endAngle: 118)
    grip.lineWidth = 104
    grip.lineCapStyle = .butt
    grip.stroke()

    for (angle, outward) in [(62.0, 1.0), (118.0, -1.0)] {
        let radians = angle * .pi / 180
        let end = NSPoint(x: centre.x + radius * cos(radians), y: centre.y + radius * sin(radians))
        let pad = NSRect(x: end.x - 78 + outward * 22, y: end.y - 152, width: 156, height: 206)
        NSBezierPath(roundedRect: pad, xRadius: 52, yRadius: 52).fill()
    }
    context.endTransparencyLayer()
    context.restoreGState()

    NSGraphicsContext.restoreGraphicsState()
    return rep
}

let root = URL(fileURLWithPath: FileManager.default.currentDirectoryPath)
let iconset = FileManager.default.temporaryDirectory.appendingPathComponent("AppIcon.iconset")
try? FileManager.default.removeItem(at: iconset)
try FileManager.default.createDirectory(at: iconset, withIntermediateDirectories: true)

for points in [16, 32, 128, 256, 512] {
    for scale in [1, 2] {
        let suffix = scale == 2 ? "@2x" : ""
        let png = drawIcon(size: points * scale).representation(using: .png, properties: [:])!
        try png.write(to: iconset.appendingPathComponent("icon_\(points)x\(points)\(suffix).png"))
    }
}

let iconutil = Process()
iconutil.executableURL = URL(fileURLWithPath: "/usr/bin/iconutil")
iconutil.arguments = [
    "--convert", "icns", "--output", root.appendingPathComponent("assets/AppIcon.icns").path, iconset.path,
]
try iconutil.run()
iconutil.waitUntilExit()
guard iconutil.terminationStatus == 0 else { fatalError("iconutil failed") }

// A flat copy for the README.
try drawIcon(size: 256).representation(using: .png, properties: [:])!
    .write(to: root.appendingPathComponent("assets/icon.png"))
try? FileManager.default.removeItem(at: iconset)
print("Wrote assets/AppIcon.icns and assets/icon.png")
