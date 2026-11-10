// swift-tools-version: 6.0
import PackageDescription

// The app and its Swift wrapper around the Rust core. Builds with the Xcode
// command line tools alone; scripts/build-app.sh turns the executable into
// Landline.app.
//
// LandlineFFI.xcframework and Sources/LandlineKit/Generated are build outputs
// of scripts/build-xcframework.sh; run it before building this package.
let package = Package(
    name: "Landline",
    platforms: [.macOS(.v14)],
    targets: [
        .binaryTarget(name: "LandlineFFI", path: "LandlineFFI.xcframework"),
        .target(name: "LandlineKit", dependencies: ["LandlineFFI"]),
        .executableTarget(name: "Landline", dependencies: ["LandlineKit"]),
        .testTarget(name: "LandlineKitTests", dependencies: ["LandlineKit"]),
        // The app's model and presentation logic, tested through the
        // executable target so they need no library of their own.
        .testTarget(name: "LandlineTests", dependencies: ["Landline", "LandlineKit"]),
    ]
)
