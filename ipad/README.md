# Luxtable for iPad

The iPad app is planned in [docs/IPAD_PLAN.md](../docs/IPAD_PLAN.md). This folder will hold
its app target (`LuxtableTouch`: the app shell, scenes, gestures, the bottom-sheet layout,
Pencil and file pickers). Nothing here builds yet: iOS apps need Xcode.app, and the
development Mac so far has only the Command Line Tools.

## Where things stand (phase 1, shared core)

The Swift package in `app/` is split so the iPad app can reuse everything but the Mac shell:

| Target | Holds | Platforms |
|---|---|---|
| `LuxtableCore` | Models, services, `AppModel` logic, codecs, AI, the Metal viewer renderer, the Rust wrappers | macOS, iPadOS |
| `LuxtableUI` | `DS` tokens, panels, sliders, curves, the filmstrip and grid, the tool strip, the viewer views, `MetalImageView` | macOS, iPadOS |
| `LuxtableMac` | The Mac app shell, menus, `KeyMonitor`, sheets with `NSOpenPanel`, Settings, archive, cards, `--snapshot`, the benchmark kit | macOS only |
| `Luxtable` | The Mac executable (`main.swift`) | macOS only |

- `LuxtableCore` never imports SwiftUI. `LuxtableCore` and `LuxtableUI` name AppKit only in
  **seam files**, one per platform concern, in `LuxtableCore/Platform/` and
  `LuxtableUI/Views/Platform/`, each with an `#if os(macOS)` branch and an `#else` (UIKit)
  branch. `scripts/check-core-platform.sh` enforces this and runs with `scripts/test-app.sh`.
- The `#else` branches have **never been compiled**. They are kept minimal and marked
  `iOS (unverified)`; several are placeholders for phase 2 (they say what replaces them).
- SwiftPM has no per-target platforms, so `Package.swift` lists `.iOS(.v26)` for the whole
  package. Only `LuxtableCore` and `LuxtableUI` (and `LuxtableAI`, `LuxtableFFI`) are meant
  to be built for iOS; the iPad app target depends on those two and never on `LuxtableMac`.
- Rust: the macOS-only process calls (`sysctl`, `diskutil`, `hdiutil`, `rsync`) are behind
  `#[cfg(target_os = "macos")]` with fallbacks (`available_parallelism`, an 8 GiB memory
  budget, "not supported" errors). `scripts/ffi.sh --xcframework` builds
  `app/.rust/LuxtableFFI.xcframework` (macOS arm64, iOS arm64, iOS-simulator arm64).

## What to install (once)

1. **Xcode** from the App Store, then
   `sudo xcode-select -s /Applications/Xcode.app/Contents/Developer && xcodebuild -runFirstLaunch`,
   and the iOS platform (Xcode ▸ Settings ▸ Components, or `xcodebuild -downloadPlatform iOS`).
2. **rustup next to Homebrew Rust**, without touching `PATH` (so `cargo` stays Homebrew's):
   `curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path`,
   then `~/.cargo/bin/rustup target add aarch64-apple-ios aarch64-apple-ios-sim aarch64-apple-darwin`.
3. **Signing:** an Apple Developer account (paid recommended: installs last a year and
   TestFlight works; a free account's installs expire after 7 days). Add it in Xcode ▸ Settings
   ▸ Accounts. Team IDs and provisioning profiles stay out of the repository (read from a
   git-ignored file or the environment, like the Google OAuth client).
4. An M-series iPad on iPadOS 26, paired with the Mac in Xcode (Window ▸ Devices).

`scripts/build-ipad.sh --check` confirms step 1; `scripts/ffi.sh --xcframework` lists anything
missing from step 2.

## Remaining steps once Xcode exists

1. **Compile the shared targets for iOS:** `scripts/build-ipad.sh`. Fix what the compiler finds
   in the unverified `#else` seam branches and any macOS-only SwiftUI modifiers still in
   `LuxtableUI` (for example `.onExitCommand`, `.pickerStyle(.radioGroup)`,
   `.toggleStyle(.checkbox)`, `.textFieldStyle(.squareBorder)`, `.menuStyle(.borderlessButton)`,
   `HSplitView`): each becomes a seam or a `LuxtableMac`-only view.
2. **Link Rust on iOS:** in `app/Package.swift`, make the `lt_ffiFFI` system library and the
   `-L app/.rust/lib` linker flags macOS-only (`.when(platforms: [.macOS])`) and add the
   XCFramework as a `.binaryTarget(name: "LuxtableFFIBinary", path: ".rust/LuxtableFFI.xcframework")`
   that `LuxtableFFI` depends on for iOS. (Or switch the Mac to the XCFramework too.)
3. **Run the Core tests on the simulator:** split `LuxtableKitTests` (it imports all three
   targets today) into a Core/UI test target and a Mac one, then
   `xcodebuild test -scheme <the Core test scheme> -destination 'platform=iOS Simulator,name=<an iPad simulator>'`.
   Tests that drive Mac views or AppKit stay in the Mac target; the rest must pass on both
   (phase 1's exit gate).
4. **Generate the app target:** an Xcode project here (`ipad/Luxtable.xcodeproj`, or an
   XcodeGen/Tuist spec checked in instead of the project) with one iOS app target,
   `LuxtableTouch`, depending on the local package products `LuxtableCore` and
   `LuxtableUI`. `scripts/build-ipad.sh` then grows an
   `xcodebuild -project ipad/Luxtable.xcodeproj -scheme LuxtableTouch` step.
5. **Phase 0 spike** (docs/IPAD_PLAN.md): an app that opens a session read-only (Files app,
   security-scoped bookmark) and shows one RAW rendered on the GPU through `MetalImageView`'s
   iOS branch. Measure on the iPad: first render (< 1.5 s), next photo (< 50 ms), peak memory,
   and the time to fill the browser from embedded previews. That is the go/no-go for phases 2–5.
