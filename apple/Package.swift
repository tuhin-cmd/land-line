// swift-tools-version: 6.0
import PackageDescription

// Wraps the Rust core. Both the xcframework and Sources/LandlineKit/Generated
// are build outputs of scripts/build-xcframework.sh; run it before building.
let package = Package(
    name: "LandlineKit",
    platforms: [.macOS(.v14)],
    products: [
        .library(name: "LandlineKit", targets: ["LandlineKit"]),
    ],
    targets: [
        .binaryTarget(name: "LandlineFFI", path: "LandlineFFI.xcframework"),
        .target(name: "LandlineKit", dependencies: ["LandlineFFI"]),
        .testTarget(name: "LandlineKitTests", dependencies: ["LandlineKit"]),
    ]
)
