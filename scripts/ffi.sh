#!/usr/bin/env bash
# Build the ds-ffi static library and refresh the generated Swift bindings.
#
# Idempotent and incremental: cargo only rebuilds what changed, and generated
# files are copied into app/ only when their content differs, so SwiftPM does
# not recompile the bindings on every run.
#
# Outputs (all git-ignored):
#   app/.rust/lib/libds_ffi.a                   static lib linked by SwiftPM
#   app/Sources/ds_ffiFFI/{ds_ffiFFI.h,module.modulemap}   C module for the FFI
#   app/Sources/DarkslideFFI/ds_ffi.swift       generated Swift bindings
#
#   scripts/ffi.sh                  the macOS build above (what build-app.sh and test-app.sh use)
#   scripts/ffi.sh --xcframework    also app/.rust/DarkslideFFI.xcframework: macOS arm64, iOS
#                                   arm64 and iOS-simulator arm64 slices for the iPad app
#                                   (docs/IPAD_PLAN.md). Needs Xcode.app and rustup with the
#                                   iOS targets; it says what to install when they are missing.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"

log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }

XCFRAMEWORK=0
case "${1:-}" in
  "") ;;
  --xcframework) XCFRAMEWORK=1 ;;
  *) echo "usage: scripts/ffi.sh [--xcframework]" >&2; exit 2 ;;
esac

# The XCFramework needs Xcode's iOS SDKs and rustup's iOS standard libraries; check both
# before building anything so a missing tool costs seconds, not a release build.
IOS_TARGETS=(aarch64-apple-ios aarch64-apple-ios-sim)
if [[ "$XCFRAMEWORK" == 1 ]]; then
  missing=()
  if ! xcrun --sdk iphoneos --show-sdk-path >/dev/null 2>&1; then
    missing+=("Xcode.app (the Command Line Tools have no iOS SDK): install Xcode from the App Store, then
       sudo xcode-select -s /Applications/Xcode.app/Contents/Developer && xcodebuild -runFirstLaunch")
  fi
  # rustup next to Homebrew Rust: installed with --no-modify-path, so `cargo` stays Homebrew's.
  RUSTUP="$(command -v rustup || echo "$HOME/.cargo/bin/rustup")"
  if [[ ! -x "$RUSTUP" ]]; then
    missing+=("rustup (Homebrew Rust ships only the macOS standard library), without changing PATH:
       curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path
       ~/.cargo/bin/rustup target add ${IOS_TARGETS[*]} aarch64-apple-darwin")
  else
    installed="$("$RUSTUP" target list --installed 2>/dev/null || true)"
    for t in "${IOS_TARGETS[@]}" aarch64-apple-darwin; do
      grep -qx "$t" <<<"$installed" || missing+=("the Rust target $t: $RUSTUP target add $t")
    done
  fi
  if ((${#missing[@]})); then
    echo "error: scripts/ffi.sh --xcframework needs:" >&2
    for m in "${missing[@]}"; do printf '  - %s\n' "$m" >&2; done
    exit 1
  fi
fi

# C code compiled by build scripts (sqlite, blake3) must target the app's minimum macOS.
export MACOSX_DEPLOYMENT_TARGET=26.0

# The shipped library must not carry the builder's home directory (panic locations,
# assert messages, debug info): map every source root to a neutral prefix. Later flags win,
# so the specific roots come after $HOME. build-app.sh fails the build if `$HOME` remains.
REMAP="--remap-path-prefix=$HOME=/home --remap-path-prefix=$HOME/.cargo/registry/src=/registry"
REMAP="$REMAP --remap-path-prefix=$HOME/.rustup=/rustup --remap-path-prefix=$ROOT=/src"

log "cargo build --release -p ds-ffi"
RUSTFLAGS="${RUSTFLAGS:-} $REMAP" cargo build --locked --release -p ds-ffi --quiet

# Keep the bindgen binary in its own target dir: building it enables uniffi's
# `cli` feature, which would otherwise invalidate the release library each time.
cargo build --locked -p ds-ffi --features bindgen --bin uniffi-bindgen \
  --target-dir "$ROOT/target/bindgen" --quiet

GEN="$(mktemp -d)"
trap '/bin/rm -rf "$GEN"' EXIT
"$ROOT/target/bindgen/debug/uniffi-bindgen" generate \
  --library "$ROOT/target/release/libds_ffi.dylib" \
  --language swift --out-dir "$GEN" >/dev/null

# Copy $1 to $2 only when the content changed (preserves mtimes → no rebuild).
sync_file() {
  mkdir -p "$(dirname "$2")"
  if ! cmp -s "$1" "$2"; then
    /bin/cp -f "$1" "$2"
    echo "   updated ${2#"$ROOT"/}"
  fi
}

sync_file "$GEN/ds_ffi.swift" "$ROOT/app/Sources/DarkslideFFI/ds_ffi.swift"
sync_file "$GEN/ds_ffiFFI.h" "$ROOT/app/Sources/ds_ffiFFI/ds_ffiFFI.h"
sync_file "$GEN/ds_ffiFFI.modulemap" "$ROOT/app/Sources/ds_ffiFFI/module.modulemap"
# Only the static archive goes in this dir so the linker cannot pick the dylib.
sync_file "$ROOT/target/release/libds_ffi.a" "$ROOT/app/.rust/lib/libds_ffi.a"

[[ "$XCFRAMEWORK" == 1 ]] || exit 0

# One static library per platform, through rustup's toolchain (Homebrew's cargo has no iOS
# std). The bindings above are target-independent, so the same header serves every slice.
export IPHONEOS_DEPLOYMENT_TARGET=26.0
XC_ARGS=()
for t in aarch64-apple-darwin "${IOS_TARGETS[@]}"; do
  log "cargo build --release -p ds-ffi --target $t"
  RUSTFLAGS="${RUSTFLAGS:-} $REMAP" "$RUSTUP" run stable cargo build --locked --release -p ds-ffi \
    --target "$t" --quiet
  XC_ARGS+=(-library "$ROOT/target/$t/release/libds_ffi.a" -headers "$GEN/headers")
done
mkdir -p "$GEN/headers"
/bin/cp -f "$GEN/ds_ffiFFI.h" "$GEN/headers/ds_ffiFFI.h"
/bin/cp -f "$GEN/ds_ffiFFI.modulemap" "$GEN/headers/module.modulemap"
OUT="$ROOT/app/.rust/DarkslideFFI.xcframework"
/bin/rm -rf "$OUT"
xcodebuild -create-xcframework "${XC_ARGS[@]}" -output "$OUT" >/dev/null
log "built ${OUT#"$ROOT"/}"
