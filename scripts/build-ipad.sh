#!/usr/bin/env bash
# Luxtable for iPad (docs/IPAD_PLAN.md, ipad/README.md). A stub until the iPad app target
# exists (phase 0): today it checks the toolchain, builds the Rust XCFramework and compiles
# the shared targets (LuxtableCore, LuxtableUI) for the iOS simulator.
#
#   scripts/build-ipad.sh            check tools, XCFramework, shared targets for the simulator
#   scripts/build-ipad.sh --check    only check the tools (prints what to install)
#
# Needs Xcode.app and rustup with the iOS targets (scripts/ffi.sh --xcframework explains).
set -euo pipefail
cd "$(dirname "$0")/.."

log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }

if ! xcrun --sdk iphonesimulator --show-sdk-path >/dev/null 2>&1; then
  echo "error: no iOS SDK. Install Xcode from the App Store, then" >&2
  echo "       sudo xcode-select -s /Applications/Xcode.app/Contents/Developer && xcodebuild -runFirstLaunch" >&2
  exit 1
fi
[[ "${1:-}" == --check ]] && { log "Xcode: $(xcodebuild -version | head -1)"; exit 0; }

# Rust for macOS, iOS and the simulator (fails with install hints when rustup is missing).
scripts/ffi.sh --xcframework

# The shared targets on the iOS simulator. Until the package consumes the XCFramework on
# iOS (ipad/README.md, step 2) this stops at the FFI link; compile errors in Core/UI show first.
for scheme in LuxtableCore LuxtableUI; do
  log "xcodebuild $scheme (iOS Simulator)"
  (cd app && xcodebuild build -scheme "$scheme" -destination 'generic/platform=iOS Simulator' -quiet)
done

log "next: the LuxtableTouch app target (ipad/README.md, phase 0)"
