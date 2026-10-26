#!/usr/bin/env bash
# Luxtable's side of the benchmark (bench/README.md).
#
#   bench/run-luxtable.sh <staged-raws> <out-dir> [--runs R] [--count N] [--screen]
#
# In-app timings: R fresh launches of `Luxtable --bench` (headless), each with its own
# scratch support folder and preferences, the OS file cache purged before each when
# `sudo -n purge` works (else the first pass is labelled warm). Results go to
# <out-dir>/results/luxtable-inapp-r<k>.csv. Extra `--bench` options (e.g. "--jumps 3"
# for a small set) can be passed in $LT_BENCH_ARGS.
#
# --screen also launches the app R times (this checkout's build, private support folder,
# never the installed app) on a fresh copy of the RAWs and times the viewer from a
# screen recording with the plan bench/plans/luxtable.json (needs Screen Recording and
# Accessibility for the terminal, and an idle Mac).
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"
source bench/lib.sh

STAGE="${1:?staged RAW folder}"
OUT="${2:?output folder}"
shift 2
RUNS=3 COUNT=0 SCREEN=0
while [[ $# -gt 0 ]]; do
  case "$1" in
    --runs) RUNS="$2"; shift 2 ;;
    --count) COUNT="$2"; shift 2 ;;
    --screen) SCREEN=1; shift ;;
    *) die "unknown option $1" ;;
  esac
done

BIN="$ROOT/dist/Luxtable.app/Contents/MacOS/Luxtable"
[[ -x "$BIN" ]] || die "build first: scripts/build-app.sh"
RESULTS="$OUT/results"
WORK="$OUT/work"
mkdir -p "$RESULTS" "$WORK"

# A scratch support folder whose preferences skip onboarding.
new_support() {
  local s
  s="$(mktemp -d -t luxtable-bench-support)"
  defaults write "$s/preferences" onboardingComplete -bool true
  echo "$s"
}

for r in $(seq 1 "$RUNS"); do
  first="cold"
  purge_cache || { first="warm"; warn "OS file cache not purged (run 'sudo -v' first): pass 1 is labelled warm"; }
  support="$(new_support)"
  log "Luxtable in-app run $r/$RUNS ($first start)"
  flags=()
  [[ "$first" == warm ]] && flags+=(--warm-start)
  # shellcheck disable=SC2086 # LT_BENCH_ARGS is split into options on purpose
  LT_SUPPORT_ROOT="$support" "$BIN" --bench "$STAGE" --work "$WORK/inapp-r$r" --out "$RESULTS/luxtable-inapp-r$r.csv" \
    --count "$COUNT" --run "$r" ${flags[@]+"${flags[@]}"} ${LT_BENCH_ARGS:-}
  /bin/rm -rf "$WORK/inapp-r$r" "$support"
done

if [[ "$SCREEN" == 1 ]]; then
  for r in $(seq 1 "$RUNS"); do
    folder="$WORK/screen-r$r"
    n="$(stage "$STAGE" "$folder" "$COUNT")"
    mode=cold
    purge_cache || mode=warm
    support="$(new_support)"
    log "Luxtable on-screen run $r/$RUNS ($mode)"
    LT_SUPPORT_ROOT="$support" "$BIN" --open "$folder" &
    pid=$!
    # The folder opens and its previews build before any timing (more photos, longer).
    sleep "${LT_BENCH_OPEN_WAIT:-$((20 + n / 5))}"
    status=0
    lt_tool "$BIN" --bench-drive bench/plans/luxtable.json --out "$RESULTS/luxtable-screen-r$r.csv" \
      --run "$r" --mode "$mode" --pid "$pid" || status=$?
    kill "$pid" 2>/dev/null || true
    wait "$pid" 2>/dev/null || true
    /bin/rm -rf "$folder" "$support"
    [[ "$status" == 0 ]] || die "screen run $r failed"
  done
fi
log "Luxtable results in $RESULTS"
