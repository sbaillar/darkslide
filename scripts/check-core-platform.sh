#!/usr/bin/env bash
# Keeps the shared Swift targets buildable for iPadOS (docs/IPAD_PLAN.md, phase 1):
#
#   LuxtableCore  never imports SwiftUI.
#   LuxtableCore, LuxtableUI
#                  import AppKit (or UIKit) and name AppKit types only in seam files
#                  (a `Platform/` folder: LuxtableCore/Platform/, LuxtableUI/Views/Platform/),
#                  and there only inside `#if os(macOS)` (UIKit: `#else` / `#if os(iOS)`).
#                  The same goes for macOS-only Foundation API (home folder, mounts, Host).
#                  Other macOS-only frameworks (DiskArbitration, ScreenCaptureKit, Quartz,
#                  ColorSync, IOKit) are imported only inside `#if os(macOS)` anywhere.
#
# LuxtableMac and the Luxtable executable are Mac-only and are not checked.
# Run by scripts/test-app.sh; exits non-zero with one line per violation.
set -euo pipefail
cd "$(dirname "$0")/../app/Sources"

fail=0
report() { echo "check-core-platform: $*" >&2; fail=1; }

# AppKit type names that SwiftUI re-exports on macOS (so they compile without an import).
APPKIT_TYPES='\bNS(View|ViewController|ViewRepresentable|Event|Window|WindowController|App|Application|ApplicationDelegate|Image|Color|Font|FontManager|Screen|Workspace|OpenPanel|SavePanel|Pasteboard|Alert|Menu|MenuItem|Cursor|Responder|Control|TextField|TextView|Text|BezierPath|GraphicsContext|BitmapImageRep|Appearance|Sound|Accessibility|AccessibilityPriorityLevel|Point|Rect|Size|HostingView|HostingController|Animation)\b'
# Foundation API that exists only on macOS.
MAC_FOUNDATION='\b(homeDirectoryForCurrentUser|mountedVolumeURLs|Host\.current|Process\(\)|NSUserName|NSFullUserName|DistributedNotificationCenter|NSAppleScript)\b'
MAC_ONLY_MODULES='AppKit|DiskArbitration|ScreenCaptureKit|Quartz|ColorSync|IOKit|Carbon'

# Prints "line:module" for every import of a macOS-only module that is not inside an
# `#if os(macOS)` / `#if canImport(<module>)` branch.
unguarded_imports() {
  awk -v mods="$MAC_ONLY_MODULES" '
    function guarded(   i) { for (i = 1; i <= depth; i++) if (mac[i]) return 1; return 0 }
    /^[[:space:]]*#if/      { depth++; mac[depth] = ($0 ~ /os\(macOS\)|canImport\((AppKit|DiskArbitration|ScreenCaptureKit|Quartz|ColorSync|IOKit)\)/) && $0 !~ /!/; next }
    /^[[:space:]]*#elseif/  { mac[depth] = ($0 ~ /os\(macOS\)/) && $0 !~ /!/; next }
    /^[[:space:]]*#else/    { mac[depth] = 0; next }
    /^[[:space:]]*#endif/   { depth--; next }
    {
      line = $0; sub(/^[[:space:]]*@[A-Za-z_]+[[:space:]]+/, "", line)
      if (match(line, "^[[:space:]]*import[[:space:]]+(" mods ")([[:space:]]|$)") && !guarded()) {
        split(line, w, /[[:space:]]+/); m = (w[1] == "import") ? w[2] : w[3]
        print FNR ":" m
      }
    }' "$1"
}

is_seam() { [[ "$1" == */Platform/* ]]; }

for target in LuxtableCore LuxtableUI; do
  [[ -d "$target" ]] || { report "missing target app/Sources/$target"; continue; }
  while IFS= read -r -d '' f; do
    rel="app/Sources/$f"
    if [[ "$target" == LuxtableCore ]] && grep -nE '^[[:space:]]*(@[A-Za-z_]+[[:space:]]+)?import[[:space:]]+SwiftUI\b' "$f" >/dev/null; then
      report "$rel imports SwiftUI (LuxtableCore has no views; move it to LuxtableUI)"
    fi
    while IFS=: read -r line mod; do
      [[ -n "$line" ]] || continue
      report "$rel:$line imports $mod outside #if os(macOS)"
    done < <(unguarded_imports "$f")
    if ! is_seam "$f"; then
      if grep -nE '^[[:space:]]*(@[A-Za-z_]+[[:space:]]+)?import[[:space:]]+(AppKit|UIKit)\b' "$f" >/dev/null; then
        report "$rel imports AppKit/UIKit outside a Platform/ seam file"
      fi
      # Comments may name AppKit types; code may not.
      hits="$(grep -nE "$APPKIT_TYPES|$MAC_FOUNDATION" "$f" | grep -vE '^[0-9]+:[[:space:]]*//' || true)"
      if [[ -n "$hits" ]]; then
        while IFS= read -r h; do report "$rel:${h%%:*} uses macOS-only API (${h#*:}) outside a Platform/ seam file"; done <<<"$hits"
      fi
    fi
  done < <(find "$target" -name '*.swift' -print0)
done

if [[ "$fail" == 0 ]]; then
  echo "check-core-platform: LuxtableCore and LuxtableUI keep AppKit behind Platform/ seams"
fi
exit "$fail"
