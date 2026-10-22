#!/usr/bin/env bash
# Workspace coverage gate: fails under 90% line coverage.
# Homebrew rust has no llvm-tools component, so point cargo-llvm-cov at Homebrew LLVM.
set -euo pipefail
cd "$(dirname "$0")/.."
LLVM_BIN="${LLVM_BIN:-/opt/homebrew/opt/llvm@23/bin}"
export LLVM_COV="${LLVM_COV:-$LLVM_BIN/llvm-cov}"
export LLVM_PROFDATA="${LLVM_PROFDATA:-$LLVM_BIN/llvm-profdata}"
cargo llvm-cov --workspace --all-features \
  --ignore-filename-regex '(benches/|tests/support/|ds-ffi/src/generated|ds-ffi/src/bin/)' \
  --fail-under-lines "${COVERAGE_MIN:-90}" "$@"
