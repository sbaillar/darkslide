#!/usr/bin/env bash
# Package the app as the usual drag-to-install disk image: a window with Luxtable.app, an
# arrow and the Applications folder to drop it on, the app icon as the volume's icon.
#
#   scripts/make-dmg.sh                   dist/Luxtable.app → dist/Luxtable-<version>-macos-arm64.dmg
#   scripts/make-dmg.sh <app> <out.dmg>
#
# The window is laid out by Finder (AppleScript), so this needs a logged-in session; the first
# run asks to let the terminal control Finder. The background is drawn by
# `scripts/make-icon.swift dmg-background`. The image is signed like the app: with
# LT_SIGN_IDENTITY (and a secure timestamp) when set, else with the app's own certificate;
# an ad-hoc signed app gives an unsigned image. The in-app updater doesn't use the image (it
# downloads the zip; docs/RELEASING.md).
set -euo pipefail
cd "$(dirname "$0")/.."

APP="${1:-dist/Luxtable.app}"
[ -d "$APP" ] || { echo "make-dmg: no app at $APP (scripts/build-app.sh builds it)" >&2; exit 1; }
version="$(/usr/libexec/PlistBuddy -c "Print CFBundleShortVersionString" "$APP/Contents/Info.plist")"
OUT="${2:-dist/Luxtable-$version-macos-arm64.dmg}"
VOL=Luxtable
MNT="/Volumes/$VOL"
[ -e "$MNT" ] && { echo "make-dmg: a volume named $VOL is mounted; eject it first" >&2; exit 1; }

work="$(mktemp -d)"
dev=""
cleanup() {
  [ -z "$dev" ] || hdiutil detach -quiet -force "$dev" 2>/dev/null || true
  rm -rf "$work"
}
trap cleanup EXIT

stage="$work/stage"
mkdir -p "$stage/.background"
ditto "$APP" "$stage/Luxtable.app"
ln -s /Applications "$stage/Applications"
swift scripts/make-icon.swift dmg-background 1 "$work/bg.png"
swift scripts/make-icon.swift dmg-background 2 "$work/bg@2x.png"
tiffutil -cathidpicheck "$work/bg.png" "$work/bg@2x.png" -out "$stage/.background/background.tiff" 2>/dev/null
cp app/Resources/AppIcon.icns "$stage/.VolumeIcon.icns"

mb=$(( $(/usr/bin/du -sm "$stage" | cut -f1) + 40 ))
hdiutil create -quiet -srcfolder "$stage" -volname "$VOL" -fs HFS+ -format UDRW -size "${mb}m" "$work/rw.dmg"
# `hdiutil attach` is deprecated from macOS 27, where `diskutil image attach` replaces it.
if diskutil image attach --help >/dev/null 2>&1; then
  dev="$(diskutil image attach "$work/rw.dmg" | awk '/Apple_HFS/ { print $1 }')"
else
  dev="$(hdiutil attach -readwrite -noverify -noautoopen "$work/rw.dmg" | awk '/Apple_HFS/ { print $1 }')"
fi
[ -d "$MNT" ] || { echo "make-dmg: the image didn't mount at $MNT" >&2; exit 1; }
SetFile -a C "$MNT"

# Window 660x400 like the background; icon centres as the background's arrow expects. The
# icons are placed in a later open than the window's options: Finder moves icons placed
# while those change by the toolbar's height when the window next opens.
osascript <<EOF
tell application "Finder"
  tell disk "$VOL"
    open
    set current view of container window to icon view
    set toolbar visible of container window to false
    set statusbar visible of container window to false
    set the bounds of container window to {200, 120, 860, 520}
    set opts to the icon view options of container window
    set arrangement of opts to not arranged
    set icon size of opts to 128
    set text size of opts to 13
    set background picture of opts to file ".background:background.tiff"
    close
    open
    set position of item "Luxtable.app" of container window to {170, 200}
    set position of item "Applications" of container window to {490, 200}
    close
    open
    update without registering applications
    delay 1
    close
  end tell
end tell
EOF

rm -rf "$MNT/.fseventsd"
sync
hdiutil detach -quiet "$dev"
dev=""
mkdir -p "$(dirname "$OUT")"
hdiutil convert -quiet "$work/rw.dmg" -format ULFO -ov -o "$OUT"

if [ -n "${LT_SIGN_IDENTITY:-}" ]; then
  codesign --force --timestamp --sign "$LT_SIGN_IDENTITY" "$OUT"
else
  signer="$(codesign -dvv "$APP" 2>&1 | /usr/bin/sed -n 's/^Authority=//p' | head -1)"
  [ -z "$signer" ] || codesign --force --timestamp=none --sign "$signer" "$OUT"
fi
hdiutil verify -quiet "$OUT"
echo "built $OUT"
