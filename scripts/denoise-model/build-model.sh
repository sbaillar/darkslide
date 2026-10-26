#!/usr/bin/env bash
# Build the AI Denoise model for the app bundle (once; cached) and print the path of
# LuxtableDenoise.mlpackage. Exits non-zero, printing nothing on stdout, when it can't
# (no Python 3.13, no network, a check failed): the app is then built without AI Denoise.
#
#   scripts/denoise-model/build-model.sh                     convert if needed
#   LT_SKIP_MODELS=1 scripts/denoise-model/build-model.sh    never convert (exits 1)
#
# Cache: ${LT_MODEL_CACHE:-~/.cache/luxtable/models}/<upstream weights SHA-256>/<stamp>/,
# the stamp hashing this script, convert.py, nafnet.py and requirements.txt. A finished
# folder is never modified (a new stamp gets a new folder), and concurrent builds take
# turns on a lock (lockf) for the venv, the download and the conversion.
set -euo pipefail
source "$(dirname "$0")/../lib/env-compat.sh"
[[ "${LT_SKIP_MODELS:-}" == 1 ]] && exit 1
HERE="$(cd "$(dirname "$0")" && pwd)"
CACHE="${LT_MODEL_CACHE:-$HOME/.cache/luxtable/models}"
mkdir -p "$CACHE"
if [[ -z "${LT_MODEL_LOCKED:-}" ]]; then
  exec /usr/bin/lockf -t 3600 "$CACHE/.lock" /usr/bin/env LT_MODEL_LOCKED=1 "$0" "$@"
fi

WEIGHTS_SHA="$(/usr/bin/sed -n 's/^WEIGHTS_SHA256 = "\(.*\)"/\1/p' "$HERE/convert.py")"
STAMP="$(cat "$HERE/build-model.sh" "$HERE/convert.py" "$HERE/nafnet.py" "$HERE/requirements.txt" | shasum -a 256 | cut -c1-16)"
OUT="$CACHE/$WEIGHTS_SHA/$STAMP"
PKG="$OUT/LuxtableDenoise.mlpackage"
if [[ -d "$PKG" ]]; then
  echo "$PKG"
  exit 0
fi

log() { printf '\033[1;34m==>\033[0m %s\n' "$*" >&2; }
PY="$(command -v python3.13 || true)"
[[ -n "$PY" ]] || { echo "build-model: python3.13 not found" >&2; exit 1; }

VENV="$CACHE/venv-py313"
if [[ ! -x "$VENV/bin/python" ]]; then
  log "AI Denoise model: creating a venv in $VENV"
  "$PY" -m venv "$VENV" >&2
fi
"$VENV/bin/pip" install --quiet --disable-pip-version-check --require-hashes --only-binary=:all: \
  -r "$HERE/requirements.txt" >&2

log "AI Denoise model: converting NAFNet-SIDD-width32 (one time)"
TMP="$CACHE/.build-$$"
/bin/rm -rf "$TMP"
trap '/bin/rm -rf "$TMP"' EXIT
mkdir -p "$TMP"
# The weights download is kept across rebuilds (convert.py verifies it, and fetches it
# again when it doesn't match).
[[ -f "$CACHE/NAFNet-SIDD-width32.pth" ]] && /bin/cp -f "$CACHE/NAFNet-SIDD-width32.pth" "$TMP/"
(cd "$HERE" && PYTHONDONTWRITEBYTECODE=1 "$VENV/bin/python" convert.py "$TMP" >&2)
/bin/cp -f "$TMP/NAFNet-SIDD-width32.pth" "$CACHE/.weights.part"
mv -f "$CACHE/.weights.part" "$CACHE/NAFNet-SIDD-width32.pth"
/bin/rm -f "$TMP/NAFNet-SIDD-width32.pth"
mkdir -p "$(dirname "$OUT")"
mv "$TMP" "$OUT"
trap - EXIT
echo "$PKG"
