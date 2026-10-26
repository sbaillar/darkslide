#!/usr/bin/env bash
# Swift tests with coverage for the app (links the real Rust library).
#
#   scripts/test-app.sh                 all tests + coverage report (non-View code)
#   scripts/test-app.sh --filter X      pass-through to `swift test`
#
# Coverage counts view models, services, formatters, and glue. It excludes
# SwiftUI views (app/Sources/*/Views/), the generated bindings, and the
# executable's main.swift. Fails under COVERAGE_MIN (default 90).
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/lib/env-compat.sh
ROOT="$PWD"
SCRATCH="${SCRATCH:-$ROOT/app/.build}"

# The shared targets stay iPad-ready: no SwiftUI in Core, AppKit only in Platform/ seams.
scripts/check-core-platform.sh

scripts/ffi.sh >/dev/null
# Luxtable.app runs the rawler helper only when the user installs it; the tests use the
# one built here (sample-free tests decode synthetic Sony files, whose colour matrix is
# rawler's camera data).
export LT_RAWLER_HELPER="${LT_RAWLER_HELPER:-$(scripts/build-rawler-helper.sh --quiet)}"

# With only the Command Line Tools, SwiftPM runs swift-testing tests only when
# the Testing framework dir is passed on the command line (Package.swift flags
# alone compile the tests but the runner then silently finds none).
CLT_FW=/Library/Developer/CommandLineTools/Library/Developer/Frameworks
TEST_FLAGS=()
if [[ -d "$CLT_FW" ]]; then
  TEST_FLAGS=(-Xswiftc -F -Xswiftc "$CLT_FW" -Xlinker -rpath -Xlinker "$CLT_FW")
fi

# Force a relink of the test bundle when the Rust archive changed.
BIN_DIR="$(swift build --package-path app --scratch-path "$SCRATCH" --show-bin-path)"
LIB_HASH="$(shasum -a 256 app/.rust/lib/liblt_ffi.a | awk '{print $1}')"
if [[ "$(cat "$BIN_DIR/.liblt_ffi.linked" 2>/dev/null)" != "$LIB_HASH" ]]; then
  /bin/rm -rf "$BIN_DIR/LuxtablePackageTests.xctest"
fi

# Tests never touch the real ~/Library/Application Support/Luxtable (AppPaths).
export LT_SUPPORT_ROOT="${LT_SUPPORT_ROOT:-$(mktemp -d -t luxtable-test-support)}"
swift test --package-path app --scratch-path "$SCRATCH" --enable-code-coverage "${TEST_FLAGS[@]}" "$@"
mkdir -p "$BIN_DIR" && echo "$LIB_HASH" >"$BIN_DIR/.liblt_ffi.linked"

PROFDATA="$BIN_DIR/codecov/default.profdata"
XCTEST="$BIN_DIR/LuxtablePackageTests.xctest/Contents/MacOS/LuxtablePackageTests"
IGNORE='(/\.build|/Tests/|/Views/|/LuxtableFFI/|/Sources/Luxtable/main\.swift|/App/LuxtableApp\.swift)'

echo
xcrun llvm-cov report "$XCTEST" -instr-profile "$PROFDATA" -ignore-filename-regex "$IGNORE" \
  | /usr/bin/sed "s#$ROOT/app/Sources/##"
TOTAL="$(xcrun llvm-cov export -summary-only "$XCTEST" -instr-profile "$PROFDATA" -ignore-filename-regex "$IGNORE" \
  | python3 -c 'import json,sys; print("%.2f" % json.load(sys.stdin)["data"][0]["totals"]["lines"]["percent"])')"
echo "Swift line coverage (non-View): ${TOTAL}%"
awk -v t="$TOTAL" -v m="${COVERAGE_MIN:-90}" 'BEGIN { exit (t + 0 >= m) ? 0 : 1 }' \
  || { echo "coverage ${TOTAL}% < ${COVERAGE_MIN:-90}%"; exit 1; }
