#!/usr/bin/env bash
# The full comparison, one command (bench/README.md, docs/BENCHMARKS.md):
#
#   bench/compare.sh (--pack [<id>] | --raws <folder>) --c1-session <empty-session-folder> [--count 150]
#                    [--runs 3] [--out <dir>] [--camera "<model>"] [--no-captureone] [--no-screen]
#                    [--no-build] [--force]
#
# --pack fetches a test pack (bench/packs/<id>.json; `standard` or no id: standard-v1,
# verified by hash) and repeats it to --count photos: results comparable across Macs.
# --raws stages the first --count RAWs of your own <folder> (read only): a "custom"
# result, never ranked. Either way the photos go to a scratch folder on the internal
# SSD. It checks the machine state, runs Luxtable (in-app and on-screen) and Capture
# One with the same files and settings, and writes <out>/report.md plus the shareable
# <out>/result.json (docs/BENCHMARKS.md, Sharing results).
# Lightroom Classic has no automated runner: see bench/lightroom-checklist.md.
#
# Before: quit Capture One and every other app you can, plug in power, turn off Low
# Power Mode, wait for Spotlight to finish, and run `sudo -v` so cold runs can purge
# the file cache. The terminal needs Screen Recording and Accessibility (screen runs)
# and Automation for Capture One. Don't touch the Mac while it runs.
set -euo pipefail
CALLER="$PWD" # relative paths in the options are the caller's
cd "$(dirname "$0")/.."
source bench/lib.sh
abs() { case "$1" in /*) echo "$1" ;; "") echo "" ;; *) echo "$CALLER/$1" ;; esac; }

RAWS="" PACK="" SESSION="" COUNT=150 RUNS=3 OUT="" CAMERA="" C1=1 SCREEN=1 BUILD=1 FORCE=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --raws) RAWS="$(abs "$2")"; shift 2 ;;
    --pack)
      PACK=standard-v1
      if [[ $# -gt 1 && "$2" != --* ]]; then PACK="$2"; shift; fi
      shift ;;
    --c1-session) SESSION="$(abs "$2")"; shift 2 ;;
    --count) COUNT="$2"; shift 2 ;;
    --runs) RUNS="$2"; shift 2 ;;
    --out) OUT="$(abs "$2")"; shift 2 ;;
    --camera) CAMERA="$2"; shift 2 ;;
    --no-captureone) C1=0; shift ;;
    --no-screen) SCREEN=0; shift ;;
    --no-build) BUILD=0; shift ;;
    --force) FORCE=1; shift ;;
    *) die "unknown option $1 (see the header of $0)" ;;
  esac
done
[[ -n "$PACK" || -d "$RAWS" ]] || die "--pack, or --raws <a folder of RAW files>"
[[ -z "$PACK" || -z "$RAWS" ]] || die "--pack or --raws, not both"
MANIFEST=""
if [[ -n "$PACK" ]]; then
  [[ "$PACK" == standard ]] && PACK=standard-v1 # the current standard pack
  MANIFEST="bench/packs/$PACK.json"
  [[ -f "$MANIFEST" ]] || die "no test pack $MANIFEST"
  log "fetch test pack $PACK"
  RAWS="$(bench/fetch-pack.sh "$MANIFEST" | tail -1)"
fi
[[ "$C1" == 0 || -d "$SESSION" ]] || die "--c1-session: an empty Capture One session folder (or --no-captureone)"
# Viewer operations are compared only when timed on screen: without it there is no
# cross-app comparison to make.
[[ "$C1" == 0 || "$SCREEN" == 1 ]] || die "--no-screen gives no cross-app comparison: add --no-captureone (Luxtable only)"
[[ "$C1" == 0 ]] || C1_NAME="$(c1_app)"
TMP="${TMPDIR:-/tmp}"
OUT="${OUT:-${TMP%/}/luxtable-bench/$(date +%Y%m%d-%H%M%S)}"
[[ ! -e "$OUT" ]] || die "$OUT already exists"
mkdir -p "$OUT"
# Nothing is ever written into the source folder.
case "$(cd "$OUT" && pwd -P)/" in "$(cd "$RAWS" && pwd -P)/"*) die "--out must not be inside --raws" ;; esac

if [[ "$BUILD" == 1 ]]; then
  log "build Luxtable (this checkout)"
  LT_SKIP_MODELS=1 scripts/build-app.sh
fi

log "stage RAWs into $OUT/raws"
if [[ -n "$PACK" ]]; then
  N="$(stage "$RAWS" "$OUT/raws" "$COUNT" cycle)"
  PHOTOS="$RAWS" # the pack's distinct files
else
  N="$(stage "$RAWS" "$OUT/raws" "$COUNT")"
  PHOTOS="$OUT/raws"
fi
log "$N photos staged"

PREFLIGHT="$(preflight "$OUT")"
echo "$PREFLIGHT"
if grep -q '^WARN' <<<"$PREFLIGHT" && [[ "$FORCE" == 0 ]]; then
  die "the machine isn't in benchmark state (above); fix it, or pass --force to record it as is"
fi
PURGED=true
sudo -n true 2>/dev/null || { PURGED=false; PREFLIGHT+=$'\n'"WARN: no sudo: file cache not purged, first passes labelled warm"; }

mkdir -p "$OUT/results"
LT_VERSION="$(/usr/bin/sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
LT_BUILD="$(git rev-list --count HEAD 2>/dev/null || echo "")"
C1_VERSION="" C1_BUILD=""
C1_INFO="/Applications/${C1_NAME:-none}.app/Contents/Info"
if [[ -f "$C1_INFO.plist" ]]; then
  C1_VERSION="$(defaults read "$C1_INFO" CFBundleShortVersionString 2>/dev/null || true)"
  C1_BUILD="$(defaults read "$C1_INFO" CFBundleVersion 2>/dev/null || true)"
fi
LR_RAN=false LR_NOTE="not installed on this Mac: bench/lightroom-checklist.md"
[[ -d "/Applications/Adobe Lightroom Classic" ]] && LR_NOTE="manual: bench/lightroom-checklist.md"
C1_RAN=$([[ "$C1" == 1 ]] && echo true || echo false)
{
  echo "{"
  echo "  \"date\": $(json_string "$(date '+%Y-%m-%d %H:%M')"),"
  echo "  \"machine\": $(json_string "$(sysctl -n hw.model), $(sysctl -n machdep.cpu.brand_string), $(($(sysctl -n hw.memsize) / 1073741824)) GB"),"
  echo "  \"os\": $(json_string "$(sw_vers -productVersion) ($(sw_vers -buildVersion))"),"
  echo "  \"photos\": $N,"
  echo "  \"camera\": $(json_string "$CAMERA"),"
  echo "  \"runs\": $RUNS,"
  echo "  \"fileCachePurged\": $PURGED,"
  echo "  \"jpegQuality\": ${BENCH_JPEG_QUALITY:-90},"
  echo "  \"apps\": ["
  echo "    {\"name\": \"Luxtable\", \"version\": $(json_string "$LT_VERSION"), \"build\": $(json_string "$LT_BUILD"), \"ran\": true, \"note\": \"\"},"
  echo "    {\"name\": \"Capture One\", \"version\": $(json_string "$C1_VERSION"), \"build\": $(json_string "$C1_BUILD"), \"ran\": $C1_RAN, \"note\": \"\"},"
  echo "    {\"name\": \"Lightroom Classic\", \"version\": \"\", \"ran\": $LR_RAN, \"note\": $(json_string "$LR_NOTE")}"
  echo "  ],"
  echo "  \"preflight\": ["
  sep=""
  while IFS= read -r line; do printf '%s    %s' "$sep" "$(json_string "$line")"; sep=$',\n'; done <<<"$PREFLIGHT"
  echo
  echo "  ]"
  echo "}"
} >"$OUT/results/meta.json"

screen_flag=()
[[ "$SCREEN" == 1 ]] && screen_flag=(--screen)
bench/run-luxtable.sh "$OUT/raws" "$OUT" --runs "$RUNS" --count "$N" ${screen_flag[@]+"${screen_flag[@]}"}
if [[ "$C1" == 1 ]]; then
  C1_APP="$C1_NAME" bench/run-captureone.sh "$OUT/raws" "$OUT" --session "$SESSION" --runs "$RUNS" --count "$N"
fi

BIN=dist/Luxtable.app/Contents/MacOS/Luxtable
lt_tool "$BIN" --bench-report "$OUT/results" --out "$OUT/report.md"
pack_flag=()
[[ -n "$MANIFEST" ]] && pack_flag=(--pack "$MANIFEST")
lt_tool "$BIN" --bench-share "$OUT/results" --photos "$PHOTOS" \
  ${pack_flag[@]+"${pack_flag[@]}"} --out "$OUT/result.json"
/bin/rm -rf "$OUT/work"
log "report: $OUT/report.md, shareable result: $OUT/result.json (raw samples: $OUT/results)"
