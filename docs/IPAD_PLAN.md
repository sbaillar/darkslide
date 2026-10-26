# Luxtable for iPad: plan

Status: proposed (2026-10-07; look-and-feel and Mac ↔ iPad link added 2026-10-10). Phase R (the Mac side of the link) is built. Phase 1 (the shared core) is done on the Mac side (2026-10-21): the split, the seams and the Rust gates are built and verified on macOS; compiling it for iOS waits for Xcode (see Phase 1 below and [ipad/README.md](../ipad/README.md)). Phase 0 waits for the installs listed under Phase 0.

## Goal

The first version is an **iPad companion for culling and light editing**. It opens a Luxtable session, lets you cull it with touch, applies looks, presets and basic adjustments, and writes everything to the same sidecars. The Mac then picks the work up with no import or export step.

Not in the first version: card import, layers and masks, export, archiving. These are covered under phases 4–5 and "Not planned" below.

## What we start from

| Part | On iPad | Work needed |
|---|---|---|
| Rust core: RAW decode, pipeline (wgpu/Metal), sessions, sidecars, SQLite index, search, AI scoring | Runs as is | Build for `aarch64-apple-ios` and the simulator; guard the macOS-only calls (below) |
| Rust `Command::new` uses: `sysctl` (cache, scheduler), `hdiutil`, `diskutil`, `rsync`, `mkfifo` (tests) | iOS can't start processes | Use `sysctlbyname`/`available_parallelism` instead; gate disk images, `diskutil` and rsync to macOS |
| Swift models and services (≈ 200 of 259 files) | Mostly platform-neutral | Move into a shared target. 35 files use AppKit (`NSEvent`, `NSImage`, panels, pasteboard…) and need small `#if os(macOS)` seams |
| SwiftUI views (57 files) | Built for a Mac window with keyboard and hover | New touch layouts; reuse the leaf controls (sliders, rows, thumbnails) |
| On-device AI: Vision, Core ML (denoise, upscale), speech, Foundation Models | Available on iPadOS | Use as is; check memory on 24–60 MP images |
| Card watching (DiskArbitration), rsync archive, Amazon Photos hand-off | Not available | Mac-only; iPad uses ImageCaptureCore and the Files app instead |

## Prerequisites (you)

1. **Install Xcode** from the App Store. iOS apps can't be built with the Command Line Tools alone, which is all this Mac has today.
2. **Install rustup** next to Homebrew Rust, with the targets `aarch64-apple-ios` and `aarch64-apple-ios-sim`. Homebrew Rust only ships the macOS standard library.
3. **Choose signing:** a paid Apple Developer account ($99/yr; installs last a year, TestFlight possible) or a free account (installs expire every 7 days). Recommended: the paid account.
4. **Target device:** an M-series iPad on iPadOS 26. Older A-series iPads lack the memory for 24 MP RAW editing with AI.

## Architecture

- **Rust:** `scripts/ffi.sh` builds an **XCFramework** (macOS arm64, iOS arm64, iOS-simulator arm64) instead of the single macOS `.a`. The UniFFI bindings stay the same for both apps. Platform-only code goes behind `#[cfg(target_os = "macos")]` with a working fallback, and the tests run on both.
- **Swift:** split `LuxtableKit` into:
  - `LuxtableCore`: models, services, AppModel logic, codecs, AI. Platform-neutral, and holds all the logic tests.
  - `LuxtableMac`: today's views and AppKit glue.
  - `LuxtableTouch`: the iPad views.

  The Mac app depends on Core + Mac, the iPad app on Core + Touch.
- **iPad app target:** a small Xcode project, or `xcodebuild` on the SwiftPM package with an iOS app product, built by `scripts/build-ipad.sh`. Simulator tests run through `xcodebuild test`.
- **Session access:** sessions are opened through the Files app (USB-C SSD, iCloud Drive, or SMB to the NAS). The app keeps security-scoped bookmarks for Open Recent. File coordination (`NSFileCoordinator`) is used when the Mac may be editing the same session.
- **Mac and iPad on the same session:** sidecars are per photo, and edits are written atomically.
  - If both devices edit the same photo, the newer write wins.
  - The other version is kept as a variant ("Edited on iPad"), never dropped.
  - The SQLite index is per device and rebuilt from the sidecars. This already works today.

## Look and feel: the iPad app should feel like Luxtable

The aim is that a Mac user picks up the iPad and recognises everything: the same dark chrome, the same tool tabs and panels, the same slider feel and the same filmstrip. Only the input changes, from mouse and keyboard to touch, Pencil and an optional Magic Keyboard.

### What carries over as is

A census of `app/Sources/LuxtableKit` (2026-10-10): 28 of its files `import AppKit`, but only 15 touch `NSView`, `NSEvent` or `NSWindow`, and most of those touch one or two lines. The model and logic layers (`Model/`, `Tools/`, `Browser/`, `Presets/`, `Viewer/` except one file, `AI/`, `Live/`, `Rust/`) have no AppKit at all.

| Piece | Files | AppKit today | On iPad |
|---|---|---|---|
| Design tokens `DS.*` (spacing, radii, sizes, colours, fonts) | `Views/Theme.swift` | `NSColor(name:)` for light/dark adaptive colours (4 lines) | Reuse. Swap in `UIColor { traits in … }` behind `#if`. Bump `DS.Size.sliderRow` (22 pt) and the 11 pt fonts by one step on touch: a `DS.Metrics` value picked per platform, not a second theme |
| Tool tabs and panels: the top tab strip (`feat/toolstrip`: `ToolStrip.split`, `ToolStripControl`), the tab model and workspace | `Model/ToolTab.swift`, `Tools/ToolLayout.swift`, `Tools/ToolWorkspace*.swift`, `Tools/ToolStrip.swift`, `Views/ToolSidebar.swift`, `Views/Tools/*Panels.swift` | Logic: none. Views: `onHover`, `.help`, context menus | Reuse the logic unchanged, so the iPad shows the user's own tab layout. The views reuse with the context menu moved onto the panel's ⋯ menu (which the toolstrip branch already adds), so long-press and ⋯ do the same thing |
| Sliders | `Tools/SliderSpec.swift`, `SliderInput.swift`, `ScrubMath.swift` (no AppKit); `Views/Controls/ToolSlider.swift`, `ScrubField.swift` | `NSEvent.modifierFlags` for Option = fine (3 lines); `ScrollWheelCatcher` (an `NSView`) | Reuse. Fine control on touch = drag a second finger or drag vertically away from the track (the slider slows down, like iOS video scrubbing); double-tap resets, as on the Mac. The scroll-wheel catcher becomes a no-op on iPad (trackpad scroll goes through SwiftUI) |
| Curves, levels, colour wheels, histogram | `Views/Controls/*.swift`, `Tools/CurveModel.swift`, `LevelsMath.swift`, `WheelMath.swift`, `Histogram*.swift` | 1–7 lines each, mostly hover readouts | Reuse; hover readouts show while dragging instead |
| Filmstrip and grid | `Views/Browser/BrowserViews.swift` (`Filmstrip`, `BrowserGrid`), `Browser/*.swift` | One `NSEvent.modifierFlags` (shift/cmd-click selection) | Reuse. Multi-select becomes Select mode or a two-finger drag, as in Photos |
| Metal viewer | `Viewer/*` (renderer, draw plan, geometry, `ViewTransform`), `Views/Viewer/MetalImageView.swift` | Only `MetalImageView`: an `NSViewRepresentable` around `MTKView`, with `scrollWheel`/`magnify` overrides | Reuse the renderer and all of `Viewer/`. Add a `UIViewRepresentable` twin: the same `Coordinator`, with pinch, pan and double-tap gesture recognisers feeding the same `ViewTransform` calls. EDR works the same way (`CAMetalLayer.wantsExtendedDynamicRangeContent`) on XDR iPads |
| Looks, styles, presets, history, copy/apply, AI panels | `Views/Presets/StylesPanel.swift`, `Views/Tools/HistoryPanel.swift`, `Views/CopyApplyButtons.swift`, `Views/Optimize/*` | Hover previews, `NSImage` for thumbnails, menus | Reuse; hover preview becomes press-and-hold preview |

### What needs touch or iPadOS rework

| Piece | Files | Why | iPad version |
|---|---|---|---|
| App shell, windows and menus | `App/LuxtableApp.swift` (`NSApplicationDelegate`, `NSApp.windows`, `NSMenu` notifications), `App/MenuKeys.swift`, `App/AppWindows.swift`, `Keyboard/MenuActions.swift` | AppKit app lifecycle and the menu bar | A new `LuxtableTouchApp` with one `WindowGroup` per session (Stage Manager can show two sessions side by side). The SwiftUI `.commands` that already exist carry over and become the iPadOS 26 menu bar and the ⌘-hold shortcut overlay |
| Keyboard | `Views/KeyMonitor.swift` (`NSEvent.addLocalMonitorForEvents`), `Keyboard/KeyInput+AppKit.swift` | No global key monitor on iOS | `KeyboardRouter` and `KeyBindings` (platform-neutral) stay; feed them from `.onKeyPress` plus `UIKeyCommand`s. Same bindings file, so Magic Keyboard shortcuts match the Mac 1:1 (P/X/U, 0–5, arrows, ⌘Z, the tool-tab keys) |
| Hover | `onHover` in `ToolSidebar`, `ToolSlider`, `HistoryPanel`, `StylesPanel`, `BrowserViews` | No hover without a pointer | Keep `onHover` (it works with a trackpad and Pencil hover on M-series iPads); every hover-only affordance gets a touch path (long-press, or always-visible on compact widths) |
| Sheets and panels | `Views/Import/ImportSheet.swift`, `Views/Export/ExportSheet.swift`, `Views/NewSessionSheet.swift`, `Views/OpenSessionSheet.swift`, `Views/PreferencesView.swift`, `Views/OnboardingView.swift`, `Views/Archive/*` | `NSOpenPanel`/`NSSavePanel`, `NSWorkspace`, Preferences window | `fileImporter`/`fileExporter`, the share sheet, and a Settings sheet. Archive stays Mac-only |
| Voice and snapshot plumbing | `Views/VoiceViews.swift`, `Views/Snapshot.swift` | `NSWindow`/`NSView` used to place the HUD and to render screenshots | Small `#if` seams; `--snapshot` stays Mac-only at first |
| Card import and volumes | `Import/CardWatcher.swift`, `DiskArbitrationEvents.swift`, `App/AppModel+Volumes.swift` | DiskArbitration | Mac-only; iPad uses ImageCaptureCore (phase 4) |

### Layout on iPad

- **Full screen, landscape:** the Mac layout as is: browser or viewer in the middle, the tool tabs and panels on the right at `DS.Size.toolPanel` (320 pt), the filmstrip along the bottom at `DS.Size.filmstrip`. On a 13″ iPad this is close to a 13″ MacBook window.
- **Portrait and compact widths** (Split View, Slide Over, small Stage Manager windows): the tool pane becomes a bottom sheet with detents (the tab strip stays visible as the sheet's header), and the filmstrip folds into a scrubber. This is one `ViewerLayout`-style pure function from size class to layout, with tests, like the existing `ViewerLayout.swift`.
- **Stage Manager and external display:** each session is its own window. On an external display the iPad can show the image full screen on the monitor and the tools on the iPad (a second `WindowGroup` scene for "viewer only"), which is the nicest thing an iPad can do that the Mac can't.
- **Magic Keyboard and trackpad:** the same shortcuts as the Mac (shared `KeyBindings`), pointer hover effects on the tool tabs, trackpad scroll and pinch on the viewer through the same `ViewTransform` calls.
- **Apple Pencil:** the brush, heal, clone and remove tools in phase 5, with pressure, and Pencil hover to show the brush outline before touching. Squeeze or double-tap on the Pencil toggles brush and eraser. In the culling loupe the Pencil can just draw a tick or a cross (optional, later).

### Concrete approach: a shared `LuxtableUI` target

This refines the three-target split in "Architecture":

```
LuxtableCore   models, services, AppModel logic, codecs, AI, Viewer/ renderer   (no SwiftUI views, no AppKit)
LuxtableUI     DS tokens, panels, sliders, curves, filmstrip, grid, tool strip,  (SwiftUI; AppKit/UIKit only behind #if)
                MetalImageView (two small representables)
LuxtableMac    app shell, menus, KeyMonitor, sheets with NSOpenPanel, archive, cards
LuxtableTouch  app shell, scenes, gestures, bottom-sheet layout, Pencil, file pickers
```

- **Seams, not forks:** each AppKit touch point in a shared view becomes one small platform helper, for example `Platform.fineAdjustActive` (Option on the Mac, a second finger on iPad) or `DS.adaptive` built on `NSColor` or `UIColor`. Views call the helper; the `#if os(macOS)` lives in one file per seam, not scattered through the views.
- **Order:** move `Views/Theme.swift`, `Views/Controls/*`, `Views/Tools/*Panels.swift`, the tool strip and the filmstrip first. They are the most visible and have the fewest AppKit lines.
- **Keeps the Mac safe:** the Mac app keeps building from the same sources, and `Luxtable --snapshot` (`Views/Snapshot.swift`) gives before-and-after screenshots of the Mac window to catch visual drift while the seams go in.
- **Sizing:** about 15 files need a real seam (the hard `NSView`/`NSEvent`/`NSWindow` list), about 13 more only drop `import AppKit` or swap `NSImage` for `CGImage`. That fits in phase 1 alongside the Core split.

## Mac ↔ iPad link (Tailscale or LAN)

**Short answer: yes.** While Luxtable is open on the Mac, the iPad can find it and send photos, previews and edits both ways, at home over the LAN and away over Tailscale. Tailscale does the VPN; Luxtable only needs a small authenticated server in the Mac app and a client in the iPad app.

### How it connects

1. **Both devices join the tailnet** with the normal Tailscale apps (Mac App Store or standalone on the Mac, App Store on the iPad). Nothing Tailscale-specific goes into Luxtable.
2. **The Mac app runs a small server** while it's open (Settings → "Allow iPad connections", off by default).
   - It listens on one TCP port on all interfaces but only accepts peers from the tailnet ranges (`100.64.0.0/10`, `fd7a:115c:a1e0::/48`) and the local subnet, and only after mutual TLS with a paired key (below). Binding to the Tailscale interface alone is fragile, since the interface comes and goes when Tailscale reconnects; the address filter plus pinned keys give the same exposure without that.
   - It advertises itself on Bonjour as `_luxtable._tcp`, so at home the iPad finds it with no setup.
3. **The iPad finds the Mac** by Bonjour on the LAN first, then by its MagicDNS name (for example `my-mac.<tailnet>.ts.net`), which pairing stores. Tailscale picks a direct path when it can, so at home the traffic often goes over the LAN even through the tailnet name.

**Embed Tailscale or use the system app?** Use the system app.

| | System Tailscale app (recommended) | Embedded (`tsnet` on the Mac, `libtailscale` on the iPad) |
|---|---|---|
| Setup | Already installed and logged in | Luxtable becomes its own tailnet node with its own login and key expiry |
| Size and build | Nothing added | A Go runtime in each app (tens of MB), a Go toolchain in the build, cgo bindings |
| iPad | Works with any app, including Luxtable in the background | Userspace networking only inside Luxtable; fine, but one more thing to maintain |
| When it's worth it | Now | Only if Luxtable is ever given to people without Tailscale |

One limit to know: iPadOS allows one VPN at a time, so a work VPN on the iPad would turn Tailscale off. The LAN path still works at home.

### Pairing and security

- **Pairing, once per device:** the Mac shows a QR code (and a 6-digit code to type when the camera is awkward). The QR holds the Mac's MagicDNS name, port, a one-time token and the fingerprint of the Mac's key. The iPad scans it, sends its own public key, and both sides pin each other.
- **Keys:** each device makes an Ed25519 key once, kept in its Keychain. Connections use TLS 1.3 with self-signed certificates for those keys and mutual authentication, with each side checking the other against its pinned fingerprint (rustls on the Mac; Network framework `sec_protocol_options` verify block on the iPad). No CA and no passwords. Noise would also work, but TLS gets us standard tools on both sides.
- **Exposure:** never reachable from the internet: Tailscale plus the address filter, and no port forwarding or UPnP ever. Unpaired peers get a TLS failure before any data. Paired devices are listed in Settings with "Forget".
- **Only while the app is open:** the server starts with the app and stops when it quits. A small menu-bar helper that keeps serving with the main window closed is a possible later step (decision 7).

### What moves, and how

The protocol is small, versioned request/response messages over that one TLS connection, plus file streams:

- **Catalogue:** list sessions and albums (smart folders); for one, list photos with their content hash (BLAKE3 of the original), sidecar version and modified time.
- **Files:** originals, 2560 px previews (the existing `lt-cache` previews) and sidecars, fetched or pushed by content hash.
- **Transfers are chunked and resumable:** 4 MB chunks, the receiver writes `.partial` files and asks for the missing ranges after a drop, then checks the whole file's BLAKE3 against the sender's hash before an atomic rename. This is the same scheme as `lt-import` (`crates/lt-import/src/run.rs`: chunked copy to `.partial`, BLAKE3 on a helper thread, `verify_hash`), so that code is reused and not rewritten. A file whose hash already exists on the receiver is skipped (the import duplicate check).

**Mac → iPad, "Send to iPad":** pick a session or album and choose:
- **Previews only** (about 1 MB a photo): culling, rating, looks and basic edits, rendered from the preview. This is the default for travel: 1 000 photos is about 1 GB.
- **With RAWs** (25–60 MB a photo): full-quality editing and zoom. Good on the LAN or a fast direct Tailscale path.
The iPad keeps a "linked copy" of the session: it knows which Mac and session it came from.

**iPad → Mac:**
- **Edits and ratings:** the iPad sends changed sidecars only. The Mac merges them with the rules already in this plan: per photo, the newer write wins, and the other version is kept as a variant ("Edited on iPad"), never dropped. Ratings, labels and keywords merge field by field, since they rarely conflict. Each sidecar carries the hash of the sidecar it was based on, so the Mac can tell a clean fast-forward from a real conflict (an additive optional field, so the sidecar schema stays at version 1).
- **New photos imported on the iPad** (card or Files, phase 4): sent with their sidecars and added to the session they were imported into, or to a "From iPad" session.
- **Offline:** everything the iPad does is written locally first and queued. When the Mac is next reachable, the queue drains; nothing waits on the network to edit.

### iPadOS limits, speed, battery, a sleeping Mac

- **Background:** iPadOS suspends apps soon after they leave the screen, so a sync can't count on running in the background. Transfers pause on suspend and resume on the next launch, which the resumable chunks make harmless. Two ways to keep long syncs going, both to try in phase 0: iPadOS 26 continued-processing background tasks (started from the app, with a progress Live Activity), and background `URLSession` tasks, which need the file stream to be plain HTTPS on the same pinned TLS identity rather than custom framing. A "Keep screen on while syncing" toggle covers the "sync 30 GB tonight on the charger" case regardless.
- **Speed:** on the LAN it's limited by Wi-Fi and the disks: on Wi-Fi 6 roughly 50–100 MB/s, so 1 000 previews in under a minute and 1 000 RAWs (40 GB) in roughly 10 minutes. Over Tailscale with a **direct** connection it's the internet path (home upload is usually the limit). If Tailscale falls back to a **DERP relay** it's much slower (often a few MB/s), so "with RAWs" should warn when the path is relayed (the Mac can see this from `tailscale status`, the iPad from the measured throughput) and suggest previews only.
- **Battery:** Wi-Fi transfer plus BLAKE3 hashing is cheap next to RAW editing; a 1 GB preview sync is a few percent. Large RAW syncs should prefer "only while charging" (a setting, default on for RAWs).
- **Mac asleep:** the iPad can't wake it reliably over Tailscale. With the Mac on the same LAN, macOS's "Wake for network access" plus Bonjour Sleep Proxy can wake it for a Bonjour lookup; away from home the iPad just shows "Mac not reachable — changes queued" and syncs later. The Mac app can hold a power assertion (`IOPMAssertion`, prevent idle sleep) while a transfer is running, so a sync that has started finishes.

### Where it fits in the plan

- **The Mac side can start now**, before the iPad app exists: a new `lt-remote` crate (see below), behind the "Allow iPad connections" setting in the Mac app.
- **The iPad client** belongs in **phase 4 (get photos in and out)**: "Send to iPad / Send to Mac" sits next to card import and Files import, and uses the same verification. Phase 2 (culling) can already use previews sent this way if the Mac side is ready by then; otherwise phase 2 uses Files as planned.

### `lt-remote`: buildable and testable today

A new Rust crate with:

- `server`: the listener, peer address filter, mutual TLS with pinned keys (rustls; tokio is already in the workspace), and the catalogue and file handlers over a `lt-session` session.
- `pairing`: key generation, the one-time pairing token and QR payload, the paired-device list (stored atomically next to the global settings).
- `transfer`: chunked, resumable send and receive with BLAKE3 verification, sharing the `lt-import` copy and hash code (factored into a small shared module rather than duplicated).
- `merge`: the sidecar merge rules (fast-forward, newer-wins + variant, field merge for ratings and labels), with property tests.
- `lt-remote` CLI test client: `lt-remote pair <code>`, `lt-remote ls`, `lt-remote pull <session> --previews`, `lt-remote push <dir>`. It stands in for the iPad until the iPad app exists, and drives the integration tests (two temp sessions on localhost, interrupted transfers, a corrupted chunk, a conflicting edit, an unpaired client refused).

Everything above is plain Rust, so it gets the usual gates and ≥ 90% coverage. Only the Bonjour advertising and the Settings toggle are Swift (Network framework `NWListener` service, in the Mac app).

### Alternatives

| Option | Good | Not so good |
|---|---|---|
| **Luxtable link over Tailscale/LAN** (above) | Works at home and away; sends only what changed; previews-only mode; merges edits with the Luxtable rules; no cloud | Mac must be on and Luxtable open; needs building |
| iCloud Drive (sessions in a synced folder) | No server; works when the Mac is off | Slow first sync of RAWs, storage cost, iCloud decides when files download; two devices writing SQLite-adjacent files is fragile (the index is per device, which helps). Fine for small selects, already in decision 3 |
| AirDrop | Built in, fast, no setup | Manual, one-way, no sidecar merge, nearby only |
| The NAS over SMB (via the Files app) | Already there; big storage; Mac can be off | iPadOS SMB is slow and drops on sleep; away from home it needs Tailscale anyway (the NAS on the tailnet); no merge logic beyond the per-photo sidecar rules |
| Network framework peer-to-peer (Bonjour + AWDL, no Wi-Fi network needed) | Works with no router at all, e.g. in the field | Nearby only; same server and protocol work as the link anyway |

**Recommendation:** build the Luxtable link, with Bonjour on the LAN and the system Tailscale app for away. The Network framework gives peer-to-peer (AWDL) for free once the iPad uses `NWConnection` with `includePeerToPeer`, so the field case comes along almost free. Keep iCloud Drive and the NAS as the "Mac is off" fallback through Files, which the plan supports anyway.

## Phases

Each phase is one agent batch: build, adversarial review, fix, merge. All gates stay as they are, and coverage stays ≥ 90% for Rust and for the shared Swift core.

### Phase 0: spike (1 agent)
- Build the XCFramework, plus an empty iPad app that opens a session read-only and shows one RAW rendered on the GPU.
- **Verify:** it runs on your iPad. Measure the decode-and-render time for one A9 file, peak memory, and the time to fill the browser from embedded previews.
- **Go/no-go:** under 1.5 s for the first render and under 50 ms to move to the next photo, the same budgets as the Mac.
- **What you install first** (nothing here can be done by an agent; details in [ipad/README.md](../ipad/README.md#what-to-install-once)):
  1. **Xcode** (App Store), then `sudo xcode-select -s /Applications/Xcode.app/Contents/Developer`, `xcodebuild -runFirstLaunch` and the iOS platform (`xcodebuild -downloadPlatform iOS`).
  2. **rustup** next to Homebrew Rust (`--no-modify-path`), then `~/.cargo/bin/rustup target add aarch64-apple-ios aarch64-apple-ios-sim` (plus `aarch64-apple-darwin` so the XCFramework's macOS slice uses the same toolchain).
  3. **An Apple Developer account** added in Xcode ▸ Settings ▸ Accounts (paid recommended; a free one's installs expire after 7 days).
  4. An M-series iPad on iPadOS 26, paired with the Mac in Xcode.

  `scripts/build-ipad.sh --check` confirms 1; `scripts/ffi.sh --xcframework` reports what is missing from 2.

### Phase 1: shared core (1–2 agents)
- Move the non-UI Swift into `LuxtableCore` and the reusable views into `LuxtableUI` (see "Look and feel"), and add the AppKit seams and Rust cfg gates.
- **Verify:** the Mac app works exactly as before. All Mac tests pass, the Core tests also pass on the iOS simulator, and coverage is unchanged.
- **Status (2026-10-21): done on macOS; the iOS half waits for Xcode.** The Mac app builds, all Swift tests pass with coverage unchanged, the Rust gates pass, and `--snapshot` screenshots match the pre-split build pixel for pixel.
  - `app/Package.swift`: `LuxtableKit` is split into `LuxtableCore` (no SwiftUI), `LuxtableUI` (the shared views) and `LuxtableMac` (shell, menus, `KeyMonitor`, sheets, Settings, archive, cards, `--snapshot`, the benchmark kit). No umbrella module: `@testable` through a re-exporting module would not reach the internals the tests use, so each test file imports the three targets instead (one mechanical line each). The test target keeps its name, `LuxtableKitTests`. Names the other targets use are `package`, not `public`.
  - Seams: `LuxtableCore/Platform/` (`PlatformWorkspace`: mounts, Finder, Launch Services, eject, the home folder and the computer's name; `PlatformAccessibility`; `VideoQuickLook`) and `LuxtableUI/Views/Platform/` (`PlatformInput.fineAdjustActive` and the selection modifiers, `Color.adaptive`, `ScrollWheelCatcher`, `PlatformDisplay` for P3/EDR, `PlatformPanels` for file pickers, confirmations and the pasteboard, and `MetalImageView`'s NSView/UIView representables). DiskArbitration and ColorSync's profile iteration are behind `#if os(macOS)`. `scripts/check-core-platform.sh` (run by `scripts/test-app.sh`) keeps it that way. The iOS branches have never been compiled.
  - Rust: `sysctl` (lt-cache, lt-pipeline), `rsync` (lt-archive) and the `hdiutil` test volumes are macOS-only with fallbacks; `diskutil` already was. `scripts/ffi.sh --xcframework` builds the XCFramework once Xcode and rustup are installed.
  - **Remaining:** compile `LuxtableCore`/`LuxtableUI` for iOS and fix what the compiler finds (macOS-only SwiftUI modifiers are listed in `ipad/README.md`), link the XCFramework on iOS, and run the Core tests on the simulator (they share one test target with the Mac views today, so the Mac-view tests need an `#if os(macOS)` or a target of their own).

### Phase 2: touch culling (1 agent)
- A grid and a full-screen loupe:
  - swipe up to pick, down to reject, left and right to move;
  - tap stars and colour labels.
- Smart folders (the exact rating folders), a filmstrip, and compare and zoom with pinch and double-tap.
- AI cull suggestions from the existing scoring.
- **Verify:** UI tests on the simulator for each gesture → sidecar, and a manual pass on the device.

### Phase 3: light editing (1–2 agents)
- A compact inspector:
  - Looks (tap replaces, long-press to "Add on Top"), styles and presets;
  - Light, Colour and White balance sliders;
  - crop and straighten;
  - Auto Adjust, AI Optimize, Edit with words;
  - undo and the History list.
- Copy and apply to a selection.
- **Verify:** an edit made on the iPad renders identically on the Mac (a pixel test through the shared pipeline), and the slider-drag budget holds on the device.

### Phase R: `lt-remote` on the Mac (1 agent, can start now)
- The `lt-remote` crate, the CLI test client, and "Allow iPad connections" with pairing in the Mac app (see "Mac ↔ iPad link").
- **Verify:** CLI integration tests on localhost (pair, pull previews, push sidecars, resume after a cut, refuse an unpaired peer), and one manual pull across the tailnet from another machine.
- **Status (2026-10-10): built**, as described in [REMOTE.md](REMOTE.md):
  - `crates/lt-remote`: server, client library and the `lt-remote` CLI (which can also serve, for trying the link between two machines). Blocking I/O with a thread per connection (no async runtime; the workspace has none), rustls TLS 1.3 with pinned Ed25519 keys, pairing by QR token or 6-digit code with SPAKE2 bound to both TLS keys, the address filter, resumable verified transfers on `lt-import`'s `.partial` and `verify_hash`, and the sidecar merge rules with property tests.
  - The app: Settings ▸ Devices (off by default; QR code and code; paired devices with write access and Revoke), Bonjour as `_luxtable._tcp` through the DNS-SD API, a toolbar indicator while a device is connected or transferring, and a reload of the open session after a device changes it. FFI contract 30.
  - Loopback end-to-end tests cover every case above, including a corrupted chunk and a revoked device.
- **Remaining:**
  - The manual pull across a tailnet from another machine, and Bonjour discovery from another device: not possible on one machine.
  - The app serves the open session only; listing every recent session needs each one's store root (an in-place folder's store can live in Application Support).
  - The device key is a file in Application Support (mode 0600), not in the Keychain.
  - "With RAWs" relayed-path warnings, the power assertion during transfers, previews from `lt-cache` (today: the RAW's embedded JPEG), and the iPad side (phase 4).

### Phase 4: get photos in and out (1–2 agents)
- Import from a card or camera with ImageCaptureCore, or from the Files app, with the same BLAKE3 verification and duplicate check as the Mac.
- The iPad side of the link: pair by QR, Send to iPad (previews or RAWs), Send to Mac, the offline queue.
- Export recipes to Files or the share sheet.
- Mac-and-iPad conflict handling as described above.

### Phase 5: Pencil and the rest (later)
- Brush masks and Heal/Clone/Remove with Apple Pencil (pressure and hover).
- Voice culling, Find and People, the Map view.

## Not planned for iPad
- **rsync archive to the NAS:** stays a Mac job. A built-in SMB copy is possible later if it's wanted.
- **Tethered shooting, disk-image tests, the Amazon Photos hand-off:** Mac-only.

## Risks
- **Memory:** iPadOS kills apps that use too much. Large panoramas, HDR merges and AI on 60 MP files may need smaller tiles on iPad. Phase 0 measures this.
- **Files over SMB/iCloud:** the first open of a big session is slow. Embedded previews keep the browser fast, and a "Keep offline" option downloads the session.
- **Two UIs to maintain:** keeping all logic in `LuxtableCore`, with the views thin, limits the cost.

## Decisions for you
1. Install Xcode and rustup. Needed before Phase 0. Recommended: yes, now.
2. Paid developer account or free 7-day installs. Recommended: paid.
3. How sessions reach the iPad: the Luxtable link, a USB-C SSD, iCloud Drive, or the NAS over SMB. Recommended: the link (previews only by default) for day-to-day, the SSD for whole shoots with RAWs, iCloud Drive or the NAS only when the Mac is off.
4. Which iPad you'll test on. Recommended: an M-series iPad Pro or Air on iPadOS 26.
5. Look and feel: the same Mac layout on iPad with touch-sized metrics, or a separate touch-first design. Recommended: the same layout through `LuxtableUI`, with a bottom-sheet tool pane only on compact widths.
6. Tailscale: rely on the system Tailscale app or embed it. Recommended: the system app.
7. When the Mac serves the iPad: only while Luxtable is open, or also from a menu-bar helper. Recommended: only while open for now; revisit the helper after phase 4.
8. Start `lt-remote` (phase R) now, before the iPad app. Recommended: yes; it is plain Rust, testable with the CLI client, and unblocks phase 4.
