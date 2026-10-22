// swift-tools-version: 6.2
// Darkslide macOS app. Build via `scripts/build-app.sh` (it builds the Rust
// static library and the generated bindings first); test via `scripts/test-app.sh`.
import Foundation
import PackageDescription

let appDir = URL(fileURLWithPath: #filePath).deletingLastPathComponent().path
let rustLibDir = "\(appDir)/.rust/lib"

// With only the Command Line Tools installed, swift-testing lives outside the
// default framework search path, and its Foundation cross-import overlay
// (_Testing_Foundation) ships without a module, so overlays are disabled.
// Harmless when Xcode.app is present.
let cltFrameworks = "/Library/Developer/CommandLineTools/Library/Developer/Frameworks"
let testingFlags: [SwiftSetting] = FileManager.default.fileExists(atPath: cltFrameworks)
    ? [.unsafeFlags(["-F", cltFrameworks, "-Xfrontend", "-disable-cross-import-overlays"])] : []
let testingLinker: [LinkerSetting] = FileManager.default.fileExists(atPath: cltFrameworks)
    ? [.unsafeFlags(["-F", cltFrameworks, "-Xlinker", "-rpath", "-Xlinker", cltFrameworks])] : []

let package = Package(
    name: "Darkslide",
    platforms: [.macOS(.v26)],
    products: [.executable(name: "Darkslide", targets: ["Darkslide"])],
    targets: [
        // Generated C header + module map for the Rust static library (scripts/ffi.sh).
        .systemLibrary(name: "ds_ffiFFI", path: "Sources/ds_ffiFFI"),
        // Generated UniFFI Swift bindings.
        .target(
            name: "DarkslideFFI",
            dependencies: ["ds_ffiFFI"],
            swiftSettings: [.swiftLanguageMode(.v5)],
            linkerSettings: [.unsafeFlags(["-L", rustLibDir]), .linkedLibrary("ds_ffi")]
        ),
        // On-device AI on Apple frameworks (Vision, FoundationModels). No UI, no FFI.
        .target(name: "DarkslideAI"),
        // View models, services, mocks, and SwiftUI views.
        .target(
            name: "DarkslideKit",
            dependencies: ["DarkslideFFI", "DarkslideAI"]
        ),
        .executableTarget(name: "Darkslide", dependencies: ["DarkslideKit"]),
        .testTarget(
            name: "DarkslideKitTests",
            dependencies: ["DarkslideKit"],
            // Tiny identity Core ML models for the AI Denoise engine (no network in tests).
            resources: [.copy("Fixtures")],
            swiftSettings: testingFlags,
            linkerSettings: testingLinker
        ),
        .testTarget(
            name: "DarkslideAITests",
            dependencies: ["DarkslideAI"],
            swiftSettings: testingFlags,
            linkerSettings: testingLinker
        ),
    ]
)
