#!/usr/bin/env bash
# Build the AI Upscale model for the app bundle (once; cached) and print the path of
# DarkslideUpscale.mlpackage. Exits non-zero, printing nothing on stdout, when it can't
# (no Python 3.13, no network, a check failed): the app is then built without AI Upscale.
#
#   scripts/upscale-model/build-model.sh                     convert if needed
#   DS_SKIP_MODELS=1 scripts/upscale-model/build-model.sh    never convert (exits 1)
#
# Cache: ${DS_MODEL_CACHE:-~/.cache/darkslide/models}/<upstream weights SHA-256>/<stamp>/,
# the stamp hashing this script, convert.py, rrdbnet.py and the requirements. It shares the
# AI Denoise model's venv and hash-locked requirements (scripts/denoise-model/), and its
# lock: concurrent builds take turns (lockf) for the venv, the download and the conversion.
set -euo pipefail
[[ "${DS_SKIP_MODELS:-}" == 1 ]] && exit 1
HERE="$(cd "$(dirname "$0")" && pwd)"
REQS="$HERE/../denoise-model/requirements.txt"
CACHE="${DS_MODEL_CACHE:-$HOME/.cache/darkslide/models}"
mkdir -p "$CACHE"
if [[ -z "${DS_MODEL_LOCKED:-}" ]]; then
  exec /usr/bin/lockf -t 3600 "$CACHE/.lock" /usr/bin/env DS_MODEL_LOCKED=1 "$0" "$@"
fi

WEIGHTS_SHA="$(/usr/bin/sed -n 's/^WEIGHTS_SHA256 = "\(.*\)"/\1/p' "$HERE/convert.py")"
STAMP="$(cat "$HERE/build-model.sh" "$HERE/convert.py" "$HERE/rrdbnet.py" "$REQS" | shasum -a 256 | cut -c1-16)"
OUT="$CACHE/$WEIGHTS_SHA/$STAMP"
PKG="$OUT/DarkslideUpscale.mlpackage"
if [[ -d "$PKG" ]]; then
  echo "$PKG"
  exit 0
fi

log() { printf '\033[1;34m==>\033[0m %s\n' "$*" >&2; }
PY="$(command -v python3.13 || true)"
[[ -n "$PY" ]] || { echo "build-model: python3.13 not found" >&2; exit 1; }

VENV="$CACHE/venv-py313"
if [[ ! -x "$VENV/bin/python" ]]; then
  log "AI Upscale model: creating a venv in $VENV"
  "$PY" -m venv "$VENV" >&2
fi
"$VENV/bin/pip" install --quiet --disable-pip-version-check --require-hashes --only-binary=:all: \
  -r "$REQS" >&2

log "AI Upscale model: converting RealESRGAN_x2plus (one time)"
TMP="$CACHE/.build-$$"
/bin/rm -rf "$TMP"
trap '/bin/rm -rf "$TMP"' EXIT
mkdir -p "$TMP"
# The weights download is kept across rebuilds (convert.py verifies it, and fetches it
# again when it doesn't match).
[[ -f "$CACHE/RealESRGAN_x2plus.pth" ]] && /bin/cp -f "$CACHE/RealESRGAN_x2plus.pth" "$TMP/"
(cd "$HERE" && PYTHONDONTWRITEBYTECODE=1 "$VENV/bin/python" convert.py "$TMP" >&2)
/bin/cp -f "$TMP/RealESRGAN_x2plus.pth" "$CACHE/.weights.part"
mv -f "$CACHE/.weights.part" "$CACHE/RealESRGAN_x2plus.pth"
/bin/rm -f "$TMP/RealESRGAN_x2plus.pth"
mkdir -p "$(dirname "$OUT")"
mv "$TMP" "$OUT"
trap - EXIT
echo "$PKG"
