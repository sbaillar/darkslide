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
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"

log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }

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
