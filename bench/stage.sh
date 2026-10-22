#!/usr/bin/env bash
# Stage RAWs for a benchmark run (read-only on the source; APFS clones):
#
#   bench/stage.sh <source-folder> <new-dest-folder> [<count>] [cycle]
#
# Copies the first <count> RAWs by name (0 = all). With `cycle`, a small set (a test
# pack) is repeated to <count> files with unique names. Prints the number staged.
set -euo pipefail
if [[ $# -lt 2 ]]; then
  echo "usage: bench/stage.sh <source> <dest> [count] [cycle]" >&2
  exit 2
fi
CALLER="$PWD"
abs() { case "$1" in /*) echo "$1" ;; *) echo "$CALLER/$1" ;; esac; }
SRC="$(abs "$1")" DEST="$(abs "$2")"
cd "$(dirname "$0")/.."
source bench/lib.sh
stage "$SRC" "$DEST" "${3:-0}" "${4:-}"
