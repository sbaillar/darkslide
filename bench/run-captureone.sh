#!/usr/bin/env bash
# Capture One's side of the benchmark (bench/README.md). Drives Capture One: run it
# only on an idle Mac, with Capture One quit beforehand and nothing else open in it.
#
#   bench/run-captureone.sh <staged-raws> <out-dir> --session <empty-session-folder> [--runs R] [--count N]
#
# <empty-session-folder> is a session you created once (File ▸ New Session, nothing
# imported) and quit. It is copied per run and never written to. Each run:
#   1. purge the OS file cache, launch Capture One on a fresh copy of the session;
#   2. import: browse a fresh copy of the RAWs in place (like Lightroom's Add and
#      Luxtable's Open Folder) until Capture One has built a preview for every one;
#   3. open: close the session, open it again and browse the folder until every
#      image is listed (previews already built);
#   4. select, held →, exposure: the screen driver with bench/plans/captureone.json;
#   5. export: a full-size sRGB JPEG recipe for the first 100 images, until every file is
#      written and the queue is empty;
#   6. quit Capture One.
# Timings by polling over AppleScript (about 0.1 s resolution: fine for operations of
# seconds) and from the screen recording for the viewer.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"
source bench/lib.sh

STAGE="${1:?staged RAW folder}"
OUT="${2:?output folder}"
shift 2
RUNS=3 COUNT=0 TEMPLATE=""
JPEG_QUALITY="${BENCH_JPEG_QUALITY:-90}"
# The preview size set in Capture One (Preferences ▸ Image), recorded with the import.
C1_PREVIEW_PX="${C1_PREVIEW_PX:-2560}"
while [[ $# -gt 0 ]]; do
  case "$1" in
    --runs) RUNS="$2"; shift 2 ;;
    --count) COUNT="$2"; shift 2 ;;
    --session) TEMPLATE="$2"; shift 2 ;;
    *) die "unknown option $1" ;;
  esac
done
[[ -d "$TEMPLATE" ]] || die "--session: an empty Capture One session folder"
TEMPLATE_DB="$(find "$TEMPLATE" -maxdepth 1 -name '*.cosessiondb' | head -1)"
[[ -n "$TEMPLATE_DB" ]] || die "no .cosessiondb in $TEMPLATE"
C1="$(c1_app)" # $C1_APP, or the one Capture One installed
# Any Capture One running (this version or another) could receive the scripts: none may be.
if pgrep -fq "/Applications/Capture One"; then
  die "quit Capture One first (every version): each run starts it fresh on a bench copy"
fi

BIN="$ROOT/dist/Luxtable.app/Contents/MacOS/Luxtable"
[[ -x "$BIN" ]] || die "build first: scripts/build-app.sh (the screen driver is part of it)"
RESULTS="$OUT/results"
WORK="$OUT/work"
mkdir -p "$RESULTS" "$WORK"

now_ms() { perl -MTime::HiRes=time -e 'printf "%.1f", time * 1000'; }
# c1 <script>: run a script in Capture One only if it is running: `tell` alone would
# relaunch a Capture One that quit or crashed, on its last (possibly the user's real)
# document. Not running is an error, never a launch.
c1() {
  osascript -e "if application \"$C1\" is running then" -e "tell application \"$C1\"" -e "$1" -e "end tell" \
    -e "else" -e "error \"$C1 is not running\" number 9001" -e "end if"
}
c1_alive() { [[ "$(osascript -e "application \"$C1\" is running")" == true ]]; }
# Set while this run expects Capture One to be open: a poll that finds it gone fails the run.
C1_EXPECTED=0

# wait_until <limit-seconds> <what> <command…>: poll every 0.1 s until the command
# succeeds.
wait_until() {
  local limit="$1" what="$2" start
  shift 2
  start="$(date +%s)"
  until "$@"; do
    if [[ "$C1_EXPECTED" == 1 ]] && ! c1_alive; then die "Capture One quit during the run ($what): run failed"; fi
    (($(date +%s) - start > limit)) && die "timed out after ${limit}s: $what"
    sleep 0.1
  done
}

images_listed() { [[ "$(c1 'count images of current document' 2>/dev/null)" == "$N" ]]; }
proxies_built() {
  local n
  n="$(find "$RAWS/CaptureOne/Cache/Proxies" -name '*.cop' 2>/dev/null | wc -l | tr -d ' ')"
  [[ "$n" -ge "$N" ]]
}
exported() {
  local n
  n="$(find "$EXPORT" -iname '*.jpg' 2>/dev/null | wc -l | tr -d ' ')"
  [[ "$n" -ge "$E" && "$(c1 'count jobs of current document' 2>/dev/null)" == "0" ]]
}
doc_open() { [[ "$(c1 'name of current document' 2>/dev/null)" == "$BENCH_NAME"* ]]; }
# c1_bench <script>: run a script that changes something, only while the front document
# is this run's bench copy (never the user's own catalog or session).
c1_bench() {
  doc_open || die "stopped: Capture One's front document is not the bench copy $BENCH_NAME"
  c1 "$1"
}

row() { # row <op> <mode> <ms> <items> <method>
  echo "Capture One,$1,$2,$r,1,$3,$4,$5,${6:-}" >>"$CSV"
}

TEMPLATE_NAME="$(basename "$TEMPLATE_DB" .cosessiondb)"
STAMP="$(date +%Y%m%d%H%M%S)"
for r in $(seq 1 "$RUNS"); do
  CSV="$RESULTS/captureone-r$r.csv"
  echo "app,operation,mode,run,sample,ms,items,method,previewPx" >"$CSV"
  # A uniquely named copy: no real document can have this name, and every script checks it.
  BENCH_NAME="LuxtableBench-$STAMP-r$r"
  SESSION="$WORK/$BENCH_NAME"
  RAWS="$WORK/c1-raws-r$r"
  EXPORT="$WORK/c1-export-r$r"
  /bin/cp -R "$TEMPLATE" "$SESSION"
  mv "$SESSION/$TEMPLATE_NAME.cosessiondb" "$SESSION/$BENCH_NAME.cosessiondb"
  N="$(stage "$STAGE" "$RAWS" "$COUNT")"
  mkdir -p "$EXPORT"
  mode=cold
  purge_cache || { mode=warm; warn "OS file cache not purged (run 'sudo -v' first): labelled warm"; }

  log "Capture One run $r/$RUNS ($mode, $N photos)"
  open -a "$C1" "$SESSION/$BENCH_NAME.cosessiondb"
  wait_until 180 "Capture One opens the session" doc_open
  C1_EXPECTED=1 # from here, Capture One quitting fails the run
  if [[ "$(c1 'processing acceleration')" == never ]]; then warn "Capture One hardware acceleration (processing) is off"; fi

  # Import in place: browse the folder, until every preview is built.
  t0="$(now_ms)"
  c1_bench "browse current document to path \"$RAWS\"" >/dev/null
  wait_until 1800 "import: $N images listed" images_listed
  wait_until 1800 "import: $N previews built" proxies_built
  row import "$mode" "$(echo "$(now_ms) - $t0" | bc)" "$N" applescript "$C1_PREVIEW_PX"

  # Open: close and reopen the session, browse the folder again.
  c1_bench 'close current document' >/dev/null
  sleep 2
  t0="$(now_ms)"
  open -a "$C1" "$SESSION/$BENCH_NAME.cosessiondb"
  wait_until 300 "open: session" doc_open
  c1_bench "browse current document to path \"$RAWS\"" >/dev/null
  wait_until 300 "open: $N images listed" images_listed
  row open "$mode" "$(echo "$(now_ms) - $t0" | bc)" "$N" applescript

  # Viewer: select, held →, exposure (screen recording).
  c1_bench 'select current document variant (first variant of current document)' >/dev/null || true
  sleep 3
  lt_tool "$BIN" --bench-drive bench/plans/captureone.json --out "$RESULTS/captureone-screen-r$r.csv" --run "$r" \
    --mode "$mode" --app-name "$C1" --expect-document "$BENCH_NAME"
  c1_bench 'set exposure of adjustments of primary variant to 0' >/dev/null || true

  # Export: full-size sRGB JPEG at the mapped quality, the first 100 images (like Luxtable).
  E=$((N < 100 ? N : 100))
  c1_bench "tell current document
        set rec to make new recipe with properties {name:\"Luxtable Bench\"}
        set output format of rec to JPEG
        set JPEG quality of rec to $JPEG_QUALITY
        set color profile of rec to \"sRGB Color Space Profile\"
        set scaling method of rec to Fixed
        set scaling unit of rec to percent
        set primary scaling value of rec to 100
        set sharpening of rec to no output sharpening
        set root folder type of rec to custom location
        set root folder location of rec to POSIX file \"$EXPORT\"
        set enabled of rec to true
      end tell" >/dev/null
  t0="$(now_ms)"
  c1_bench "repeat with v in (variants 1 thru $E of current document)
        process v recipe \"Luxtable Bench\"
      end repeat" >/dev/null
  wait_until 3600 "export: $E JPEGs" exported
  row export "$mode" "$(echo "$(now_ms) - $t0" | bc)" "$E" applescript
  bytes="$(find "$EXPORT" -iname '*.jpg' -exec stat -f %z {} + | awk '{s += $1} END {print s}')"
  log "Capture One export: $E JPEGs, $((bytes / E / 1024)) KB each on average"

  c1_bench 'delete recipe "Luxtable Bench" of current document' >/dev/null || true
  C1_EXPECTED=0
  c1 quit >/dev/null || true
  wait_until 120 "Capture One quits" bash -c "! pgrep -xq \"$C1\""
  /bin/rm -rf "$RAWS" "$EXPORT" "$SESSION"
done
log "Capture One results in $RESULTS"
