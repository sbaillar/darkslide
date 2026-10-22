#!/usr/bin/env bash
# One command: Rust FFI → Swift bindings → SwiftPM release build → dist/Darkslide.app → sign.
#
#   scripts/build-app.sh            build (incremental; ~1 s when nothing changed)
#   scripts/build-app.sh --run      build, then open the app
#   scripts/build-app.sh --test     build, then run `Darkslide --self-test`
#   DS_SKIP_MODELS=1 scripts/...    fast dev build without the AI Denoise model
#   DS_BUNDLE_UPSCALE=1 scripts/... also bundle the AI Upscale model (by default the app
#                                   downloads it on first use; docs/UPSCALE_MODEL.md)
#
# Signing (default: the local "Darkslide Dev" identity if it is valid, else ad hoc). Every
# signature has the Hardened Runtime and app/Resources/Darkslide.entitlements, so dev builds
# run under the same restrictions as a release:
#   DS_SIGN_IDENTITY="Developer ID Application: Name (TEAMID)" scripts/...
#       opt-in distribution signing: Hardened Runtime + app/Resources/Darkslide.entitlements
#       + secure timestamp. Needs the owner's Apple Developer account (docs/RELEASING.md);
#       not exercised by any automated gate.
#   DS_NOTARY_PROFILE=<keychain profile>   with DS_SIGN_IDENTITY: also notarize and staple
#       (profile created once with `xcrun notarytool store-credentials`; nothing secret is
#       read from the environment or the repository).
#
# Needs only the Xcode Command Line Tools (no Xcode.app): shaders compile at
# runtime, and the bundle is assembled by hand.
set -euo pipefail
cd "$(dirname "$0")/.."
ROOT="$PWD"
APP="$ROOT/dist/Darkslide.app"
BUNDLE_ID="casa.baillargeon.darkslide"
VERSION="$(/usr/bin/sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)"
BUILD_NUMBER="$(git rev-list --count HEAD 2>/dev/null || echo 1)"

log() { printf '\033[1;34m==>\033[0m %s\n' "$*"; }

scripts/ffi.sh

# SwiftPM doesn't track libds_ffi.a as a link input, so a Rust-only change
# wouldn't relink. Drop the linked binary whenever the archive's hash changes.
BIN="$(swift build -c release --package-path app --show-bin-path)/Darkslide"
LIB_HASH="$(shasum -a 256 app/.rust/lib/libds_ffi.a | awk '{print $1}')"
LINK_STAMP="$(dirname "$BIN")/.libds_ffi.linked"
if [[ "$(cat "$LINK_STAMP" 2>/dev/null)" != "$LIB_HASH" ]]; then
  /bin/rm -f "$BIN"
fi

log "swift build -c release"
swift build -c release --package-path app --quiet
echo "$LIB_HASH" >"$LINK_STAMP"

log "assemble ${APP#"$ROOT"/}"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
ICON="$ROOT/app/Resources/AppIcon.icns"

# AI Denoise: the fp16 model, converted once from the pinned upstream weights with the
# hash-locked dependencies and cached (scripts/denoise-model/build-model.sh). Without
# Python or network the app is built without it (AI Denoise then shows as unavailable).
MODEL_PKG=""
MODEL_ID="none"
if MODEL_PKG="$(scripts/denoise-model/build-model.sh)"; then
  MODEL_ID="$(shasum -a 256 "$MODEL_PKG/Data/com.apple.CoreML/weights/weight.bin" | awk '{print $1}')"
else
  MODEL_PKG=""
  if [[ "${DS_SKIP_MODELS:-}" == 1 ]]; then
    log "AI Denoise model skipped (DS_SKIP_MODELS=1)"
  else
    printf '\033[1;33mwarning:\033[0m AI Denoise model not built (see above): the app is built without AI Denoise.\n'
  fi
fi
# AI Upscale: not bundled unless DS_BUNDLE_UPSCALE=1 (its training data is licensed for
# non-commercial research, so public builds download it on first use instead). Then it is
# built the same way (scripts/upscale-model/build-model.sh); without it the app offers the
# download.
UPSCALE_PKG=""
UPSCALE_ID="none"
if [[ "${DS_BUNDLE_UPSCALE:-}" != 1 ]]; then
  log "AI Upscale model not bundled (downloaded on first use; DS_BUNDLE_UPSCALE=1 bundles it)"
elif UPSCALE_PKG="$(scripts/upscale-model/build-model.sh)"; then
  UPSCALE_ID="$(shasum -a 256 "$UPSCALE_PKG/Data/com.apple.CoreML/weights/weight.bin" | awk '{print $1}')"
else
  UPSCALE_PKG=""
  if [[ "${DS_SKIP_MODELS:-}" == 1 ]]; then
    log "AI Upscale model skipped (DS_SKIP_MODELS=1)"
  else
    printf '\033[1;33mwarning:\033[0m AI Upscale model not built (see above): the app is built without AI Upscale.\n'
  fi
fi
ACK="$ROOT/app/Resources/Acknowledgements.md"
LICENSES="$ROOT/THIRD_PARTY_LICENSES.md" # scripts/licenses.sh
# Bundled plug-ins (plugins/builtin/*.dsplugin): Darkslide's own looks and styles…
PLUGINS_SRC="$ROOT/plugins/builtin"
PLUGINS_ID="$(cd "$PLUGINS_SRC" && find . -type f ! -name '.*' -print0 | sort -z | xargs -0 shasum -a 256 | shasum -a 256 | awk '{print $1}')"

# Google Photos export: the OAuth client built into the app, never committed. From
# DS_GOOGLE_CLIENT_ID / DS_GOOGLE_CLIENT_SECRET, else the git-ignored
# secrets/google-oauth.json (the JSON Google Cloud Console downloads for a Desktop
# client, or {"client_id": …, "client_secret": …}). Without one, users set up their
# own client in Settings ▸ Google Photos (docs/GOOGLE_PHOTOS.md).
GOOGLE_ID="${DS_GOOGLE_CLIENT_ID:-}"
GOOGLE_SECRET="${DS_GOOGLE_CLIENT_SECRET:-}"
GOOGLE_JSON="$ROOT/secrets/google-oauth.json"
if [[ -z "$GOOGLE_ID" && -f "$GOOGLE_JSON" ]]; then
  json_value() { /usr/bin/plutil -extract "installed.$1" raw -o - "$GOOGLE_JSON" 2>/dev/null \
    || /usr/bin/plutil -extract "$1" raw -o - "$GOOGLE_JSON" 2>/dev/null || true; }
  GOOGLE_ID="$(json_value client_id)"
  GOOGLE_SECRET="$(json_value client_secret)"
fi
if [[ -n "$GOOGLE_ID" ]]; then
  for v in "$GOOGLE_ID" "$GOOGLE_SECRET"; do
    [[ "$v" =~ ^[A-Za-z0-9._-]*$ ]] || { echo "error: the Google OAuth client ID/secret has unexpected characters" >&2; exit 1; }
  done
  log "Google Photos: built-in OAuth client ${GOOGLE_ID%%-*}-…"
else
  log "Google Photos: no built-in OAuth client (users add their own in Settings)"
fi

PLIST_IN="$ROOT/app/Resources/Info.plist.in"
ENTITLEMENTS="$ROOT/app/Resources/Darkslide.entitlements"
PLIST="$(mktemp)"
trap '/bin/rm -f "$PLIST"' EXIT
/usr/bin/sed -e "s|@BUNDLE_ID@|${BUNDLE_ID}|" -e "s|@VERSION@|${VERSION}|" \
  -e "s|@BUILD_NUMBER@|${BUILD_NUMBER}|" "$PLIST_IN" >"$PLIST"
if [[ -n "$GOOGLE_ID" ]]; then
  plutil -insert DSGoogleClientID -string "$GOOGLE_ID" "$PLIST"
  if [[ -n "$GOOGLE_SECRET" ]]; then plutil -insert DSGoogleClientSecret -string "$GOOGLE_SECRET" "$PLIST"; fi
fi
plutil -lint "$PLIST" >/dev/null
# Signing rewrites the bundled binary, so compare inputs by hash, not the bundle.
STAMP="$ROOT/dist/.Darkslide.inputs"
# -v: only valid identities (an expired or untrusted one is listed without it and would make codesign fail).
SIGNER="$(security find-identity -v -p codesigning 2>/dev/null | grep -c '"Darkslide Dev"' || true)"
SIGN_ID="${DS_SIGN_IDENTITY:-}"
INPUTS="$(shasum -a 256 "$BIN" "$ICON" "$PLIST" "$ENTITLEMENTS" "$ACK" "$LICENSES" app/.rust/lib/libds_ffi.a | awk '{print $1}') signer=$SIGNER runtime=1 dist=$SIGN_ID notary=${DS_NOTARY_PROFILE:-} model=$MODEL_ID upscale=$UPSCALE_ID plugins=$PLUGINS_ID"
if [[ "$(cat "$STAMP" 2>/dev/null)" != "$INPUTS" ]] || ! codesign --verify "$APP" 2>/dev/null; then
  /bin/cp -f "$BIN" "$APP/Contents/MacOS/Darkslide"
  /bin/cp -f "$ICON" "$APP/Contents/Resources/AppIcon.icns"
  /bin/cp -f "$PLIST" "$APP/Contents/Info.plist"
  /bin/cp -f "$ACK" "$APP/Contents/Resources/Acknowledgements.md"
  /bin/cp -f "$LICENSES" "$APP/Contents/Resources/THIRD_PARTY_LICENSES.md"
  /bin/rm -rf "$APP/Contents/Resources/Plugins"
  mkdir -p "$APP/Contents/Resources/Plugins"
  for p in "$PLUGINS_SRC"/*.dsplugin; do /bin/cp -R "$p" "$APP/Contents/Resources/Plugins/"; done
  /bin/rm -rf "$APP/Contents/Resources/Models"
  if [[ -n "$MODEL_PKG" ]]; then
    mkdir -p "$APP/Contents/Resources/Models"
    /bin/cp -R "$MODEL_PKG" "$APP/Contents/Resources/Models/DarkslideDenoise.mlpackage"
  fi
  if [[ -n "$UPSCALE_PKG" ]]; then
    mkdir -p "$APP/Contents/Resources/Models"
    /bin/cp -R "$UPSCALE_PKG" "$APP/Contents/Resources/Models/DarkslideUpscale.mlpackage"
  fi
  # Strip debug info (the Rust archive carries DWARF and the linker's debug map, both with
  # build-machine paths), then refuse to ship a binary that still names the builder's home
  # directory. (Not a bare `/Users/`: the app legitimately references /Users/Shared.)
  strip -S "$APP/Contents/MacOS/Darkslide"
  # Not `grep -q`: it exits at the first match, strings dies of SIGPIPE, and pipefail turns the hit into a miss.
  LEAKS="$(strings -a "$APP/Contents/MacOS/Darkslide" | grep -F "$HOME/" || true)"
  if [[ -n "$LEAKS" ]]; then
    echo "error: the binary still contains \$HOME paths (see scripts/ffi.sh --remap-path-prefix):" >&2
    printf '%s\n' "$LEAKS" | /usr/bin/sed -n 1,5p >&2
    /bin/rm -f "$STAMP"
    exit 1
  fi
  if [[ -n "$SIGN_ID" ]]; then
    # Opt-in distribution signing. Hardened Runtime + a secure timestamp are what notarization requires.
    log "codesign ($SIGN_ID, Hardened Runtime)"
    codesign --force --options runtime --timestamp --entitlements "$ENTITLEMENTS" --sign "$SIGN_ID" "$APP"
    if [[ -n "${DS_NOTARY_PROFILE:-}" ]]; then
      log "notarize (notarytool, profile $DS_NOTARY_PROFILE)"
      ZIP="$(mktemp -d)/Darkslide.zip"
      ditto -c -k --keepParent "$APP" "$ZIP"
      xcrun notarytool submit "$ZIP" --keychain-profile "$DS_NOTARY_PROFILE" --wait
      xcrun stapler staple "$APP"
      spctl --assess --type execute --verbose=2 "$APP"
    fi
  elif security find-identity -v -p codesigning 2>/dev/null | grep -q '"Darkslide Dev"'; then
    # A stable identity keeps privacy (TCC) grants across rebuilds; ad hoc changes the cdhash each build.
    log "codesign (Darkslide Dev, Hardened Runtime)"
    codesign --force --options runtime --entitlements "$ENTITLEMENTS" --sign "Darkslide Dev" --timestamp=none "$APP"
  else
    log "codesign (ad-hoc, Hardened Runtime)"
    printf '\033[1;33mwarning:\033[0m ad-hoc signature: privacy grants reset on every build. Run scripts/make-signing-identity.sh once.\n'
    codesign --force --options runtime --entitlements "$ENTITLEMENTS" --sign - --timestamp=none "$APP"
  fi
  echo "$INPUTS" >"$STAMP"
else
  log "bundle up to date"
fi
codesign --verify --strict "$APP"
log "built ${APP#"$ROOT"/} (v${VERSION} build ${BUILD_NUMBER})"

case "${1:-}" in
  --run) open "$APP" ;;
  --test) "$APP/Contents/MacOS/Darkslide" --self-test ;;
esac
