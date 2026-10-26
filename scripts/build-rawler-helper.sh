#!/usr/bin/env bash
# Build luxtable-rawler, the separate program that runs the LGPL-2.1 rawler decoder for
# the formats and cameras Luxtable's own decoders don't cover (docs/RAWDEC.md,
# crates/lt-rawler/README.md). Luxtable.app doesn't contain it: users download it from
# Settings ▸ RAW (a GitHub release asset whose SHA-256 the app pins), or build it here.
#
#   scripts/build-rawler-helper.sh            build dist/rawler-helper/luxtable-rawler-macos-arm64
#                                             and print its SHA-256 and size (for RawlerHelper.swift)
#   scripts/build-rawler-helper.sh --install  also install it where Luxtable looks:
#                                             ~/Library/Application Support/Luxtable/Helpers/
#   scripts/build-rawler-helper.sh --quiet    build only, print the path (test-app.sh)
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"

MODE="${1:-}"
case "$MODE" in
  ""|--install|--quiet) ;;
  *) echo "usage: scripts/build-rawler-helper.sh [--install|--quiet]" >&2; exit 2 ;;
esac

export MACOSX_DEPLOYMENT_TARGET=26.0
# The same neutral source prefixes as scripts/ffi.sh (and so the same build cache).
REMAP="--remap-path-prefix=$HOME=/home --remap-path-prefix=$HOME/.cargo/registry/src=/registry"
REMAP="$REMAP --remap-path-prefix=$HOME/.rustup=/rustup --remap-path-prefix=$ROOT=/src"
RUSTFLAGS="${RUSTFLAGS:-} $REMAP" cargo build --locked --release -p lt-rawler --quiet
BIN="$ROOT/target/release/luxtable-rawler"
if [[ "$MODE" == --quiet ]]; then
  echo "$BIN"
  exit 0
fi

OUT="$ROOT/dist/rawler-helper"
ASSET="$OUT/luxtable-rawler-macos-$(uname -m)"
mkdir -p "$OUT"
/bin/cp -f "$BIN" "$ASSET"
strip -x "$ASSET"
codesign --force --sign - --timestamp=none "$ASSET"
if strings -a "$ASSET" | grep -qF "$HOME/"; then
  echo "error: $ASSET still contains \$HOME paths" >&2
  exit 1
fi
/bin/cp -f crates/lt-rawler/README.md "$OUT/README.md"
SHA="$(shasum -a 256 "$ASSET" | awk '{print $1}')"
SIZE="$(stat -f %z "$ASSET")"
echo "$ASSET"
echo "  sha256 $SHA"
echo "  size   $SIZE"

if [[ "$MODE" == --install ]]; then
  DEST="$HOME/Library/Application Support/Luxtable/Helpers"
  mkdir -p "$DEST"
  /bin/cp -f "$ASSET" "$DEST/luxtable-rawler.new"
  /bin/mv -f "$DEST/luxtable-rawler.new" "$DEST/luxtable-rawler"
  echo "installed $DEST/luxtable-rawler (Luxtable uses it from its next launch)"
fi
