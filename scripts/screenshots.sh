#!/usr/bin/env bash
# Retake docs/screenshots/ in --demo mode: synthetic photos, sample sessions, no people,
# no real file names or paths. Each run uses a fresh scratch support folder and the
# snapshot defaults suite, so it never reads or writes the user's Luxtable data.
#
#   scripts/screenshots.sh            all of them (build dist/Luxtable.app first)
#   scripts/screenshots.sh survey     one (by name)
#
# Look at every image before committing it.
#
# Launch safety: the app only ever runs with a fresh LT_SUPPORT_ROOT AND a private
# preferences suite inside it (`--defaults-suite`), and only a binary built from the
# current sources that knows those switches: an older binary would ignore an unknown
# flag and open the user's real library.
set -euo pipefail
cd "$(dirname "$0")/.."
source scripts/lib/env-compat.sh
APP="dist/Luxtable.app/Contents/MacOS/Luxtable"
OUT="docs/screenshots"
TIMEOUT=240 # seconds; the app's own snapshot watchdog gives up at 180

die() { echo "error: $*" >&2; exit 1; }
[[ -x "$APP" ]] || die "build first (LT_SKIP_MODELS=1 scripts/build-app.sh)"
stale="$(find app/Sources app/.rust/lib -newer "$APP" -type f -print -quit 2>/dev/null || true)"
[[ -z "$stale" ]] || die "$APP is older than $stale: rebuild it (LT_SKIP_MODELS=1 scripts/build-app.sh)"
# Strings of 15 bytes or fewer are inlined in Swift code, so check longer ones: the
# private-suite switch and the snapshot run's own suite and scratch folder.
for sentinel in --defaults-suite app.luxtable.Luxtable.snapshot luxtable-snapshot-support; do
  grep -aq -- "$sentinel" "$APP" || die "$APP lacks \"$sentinel\": refusing to launch it"
done

shots=(
  "shell|"
  "archive-popover|--archive-popover"
  "crop-tool|--crop-drag"
  "crop-ratios|--crop-ratios"      # the Crop button's ratio drop-down, 4:3 chosen
  "live-edit|--stage-edit"
  "zoom-1to1|--zoom-actual"
  "nl-command-bar|--nl|warmer and a bit brighter"
  "grid|--grid"
  "survey|--survey|4"   # four photos, one eliminated: "Survey · 3"
  "wb-loupe|--wb-loupe"
  "multiview|--select|4"           # four selected photos side by side, the second focused
  "ai-changes|--ai-changes"        # Auto Adjust's outlined tools and the AI Changes list
  "tools-custom|--tools-custom"
  "import-card|--import-card|/Volumes/DEMO_CARD" # no such volume: the demo invents a roll
  "settings-plugins|--settings|plugins"
  "settings-devices|--settings|devices"
)

for shot in "${shots[@]}"; do
  IFS='|' read -r name flag arg <<<"$shot"
  [[ $# -gt 0 && "$1" != "$name" ]] && continue
  root="$(mktemp -d)"
  log="$root/run.log"
  args=(--demo --defaults-suite "$root/preferences" --snapshot "$OUT/$name.png")
  if [[ -n "$flag" ]]; then args+=("$flag"); fi
  if [[ -n "$arg" ]]; then args+=("$arg"); fi
  echo "==> $name"
  LT_SUPPORT_ROOT="$root" "$APP" "${args[@]}" >"$log" 2>&1 &
  pid=$!
  for ((t = 0; t < TIMEOUT; t++)); do
    kill -0 "$pid" 2>/dev/null || break
    sleep 1
  done
  if kill -0 "$pid" 2>/dev/null; then
    kill "$pid" # only the process this script started
    die "$name: no snapshot after ${TIMEOUT}s (see $log)"
  fi
  wait "$pid" || die "$name: the app failed (see $log)"
  grep -q "^snapshot: wrote" "$log" || die "$name: no snapshot written (see $log)"
  grep "^snapshot: wrote" "$log"
  /bin/rm -rf "$root"
done
