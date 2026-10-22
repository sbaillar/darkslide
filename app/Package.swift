// swift-tools-version: 6.2
// Darkslide macOS app. Build via `scripts/build-app.sh` (it builds the Rust
// static library and the generated bindings first); test via `scripts/test-app.sh`.
//
// DarkslideCore and DarkslideUI are shared with the iPad app (docs/IPAD_PLAN.md,
// ipad/README.md); DarkslideMac and the Darkslide executable are Mac-only.
import Foundation
import PackageDescription

let appDir = URL(fileURLWithPath: #filePath).deletingLastPathComponent().path
let rustLibDir = "\(appDir)/.rust/lib"

// With only the Command Line Tools installed, swift-testing lives outside the
// default framework search path, and its Foundation cross-import overlay
// (_Testing_Foundation) ships without a module, so overlays are disabled.
// Only when the Command Line Tools are the active developer directory: with Xcode
// selected (CI), Xcode's own swift-testing must win, or its macros and the CLT
// framework disagree.
let cltFrameworks = "/Library/Developer/CommandLineTools/Library/Developer/Frameworks"
let developerDir = ProcessInfo.processInfo.environment["DEVELOPER_DIR"]
    ?? (try? FileManager.default.destinationOfSymbolicLink(atPath: "/var/db/xcode_select_link"))
    ?? "/Library/Developer/CommandLineTools"
let useCLTTesting = developerDir.hasPrefix("/Library/Developer/CommandLineTools")
    && FileManager.default.fileExists(atPath: cltFrameworks)
let testingFlags: [SwiftSetting] = useCLTTesting
    ? [.unsafeFlags(["-F", cltFrameworks, "-Xfrontend", "-disable-cross-import-overlays"])] : []
let testingLinker: [LinkerSetting] = useCLTTesting
    ? [.unsafeFlags(["-F", cltFrameworks, "-Xlinker", "-rpath", "-Xlinker", cltFrameworks])] : []

let package = Package(
    name: "Darkslide",
    // SwiftPM has no per-target platforms: iOS is listed for DarkslideCore and DarkslideUI
    // (and the AI/FFI targets they use). DarkslideMac and Darkslide are never built for it.
    platforms: [.macOS(.v26), .iOS(.v26)],
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
        // Models, services, AppModel logic, codecs, AI, the viewer renderer, mocks.
        // No SwiftUI; AppKit only in Platform/ seams (scripts/check-core-platform.sh).
        .target(
            name: "DarkslideCore",
            dependencies: ["DarkslideFFI", "DarkslideAI"]
        ),
        // Shared SwiftUI views: DS tokens, panels, controls, browser, viewer.
        // AppKit only in Views/Platform/ seams.
        .target(
            name: "DarkslideUI",
            dependencies: ["DarkslideCore", "DarkslideFFI", "DarkslideAI"]
        ),
        // The Mac app: shell, menus, keyboard monitor, sheets, settings, archive, cards.
        .target(
            name: "DarkslideMac",
            dependencies: ["DarkslideCore", "DarkslideUI", "DarkslideFFI", "DarkslideAI"]
        ),
        .executableTarget(name: "Darkslide", dependencies: ["DarkslideCore", "DarkslideMac"]),
        .testTarget(
            name: "DarkslideKitTests",
            dependencies: ["DarkslideCore", "DarkslideUI", "DarkslideMac"],
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
