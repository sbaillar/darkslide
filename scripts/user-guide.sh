#!/usr/bin/env bash
# Build docs/Darkslide-User-Guide.pdf from docs/user-guide/guide.html with WeasyPrint.
#
#   scripts/user-guide.sh              the PDF (diagrams from the committed SVGs)
#   scripts/user-guide.sh --diagrams   also re-render docs/user-guide/diagrams/*.mmd to img/*.svg
#
# Needs WeasyPrint (brew install weasyprint). --diagrams also needs Google Chrome and the
# network: Mermaid is loaded from a pinned CDN version and run in headless Chrome, with
# plain SVG text labels (WeasyPrint doesn't draw the HTML labels Mermaid uses by default).
#
# Screenshots come from docs/screenshots (scripts/screenshots.sh, demo mode).
set -euo pipefail
cd "$(dirname "$0")/.."
SRC="docs/user-guide"
OUT="docs/Darkslide-User-Guide.pdf"
MERMAID="https://cdn.jsdelivr.net/npm/mermaid@11.4.1/dist/mermaid.min.js"
CHROME="${CHROME:-/Applications/Google Chrome.app/Contents/MacOS/Google Chrome}"

die() { echo "error: $*" >&2; exit 1; }
command -v weasyprint >/dev/null || die "weasyprint not found (brew install weasyprint)"
tmp="$(mktemp -d)"
trap '/bin/rm -rf "$tmp"' EXIT

if [[ "${1:-}" == "--diagrams" ]]; then
  [[ -x "$CHROME" ]] || die "Google Chrome not found (set CHROME)"
  for mmd in "$SRC"/diagrams/*.mmd; do
    name="$(basename "$mmd" .mmd)"
    python3 "scripts/user-guide/mermaid_page.py" "$mmd" "$MERMAID" >"$tmp/$name.html"
    "$CHROME" --headless=new --disable-gpu --no-first-run --user-data-dir="$tmp/chrome" \
      --virtual-time-budget=15000 --dump-dom "file://$tmp/$name.html" 2>/dev/null >"$tmp/$name.dom" || true
    python3 "scripts/user-guide/mermaid_svg.py" "$tmp/$name.dom" "$SRC/img/$name.svg"
    echo "rendered $SRC/img/$name.svg"
  done
fi

# Screenshots go in as JPEG copies (WeasyPrint embeds JPEG as is; the palette PNGs would be
# re-encoded several times larger).
mkdir -p "$tmp/shots"
for png in docs/screenshots/*.png; do
  sips -s format jpeg -s formatOptions 82 "$png" --out "$tmp/shots/$(basename "$png" .png).jpg" >/dev/null
done
# The table of contents goes where guide.html says <!-- TOC -->. The build copy sits next
# to guide.html so its other relative paths resolve.
build="$SRC/.guide.build.html"
python3 "scripts/user-guide/toc.py" "$SRC/guide.html" | python3 -c '
import re, sys
sys.stdout.write(re.sub(r"\.\./screenshots/([\w-]+)\.png", lambda m: "file://" + sys.argv[1] + "/" + m.group(1) + ".jpg", sys.stdin.read()))
' "$tmp/shots" >"$build"
weasyprint --quiet "$build" "$OUT" || { /bin/rm -f "$build"; die "weasyprint failed"; }
/bin/rm -f "$build"
echo "wrote $OUT ($(/usr/bin/du -h "$OUT" | cut -f1))"
