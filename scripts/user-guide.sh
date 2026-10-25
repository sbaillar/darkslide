#!/usr/bin/env bash
# Build docs/Darkslide-User-Guide.pdf from the user guide's Markdown chapters.
#
#   scripts/user-guide.sh              the PDF (diagrams from the committed SVGs)
#   scripts/user-guide.sh --diagrams   also re-render docs/user-guide/diagrams/*.mmd to img/*.svg
#
# The chapters are docs/user-guide/NN-*.md, taken in file-name order. pandoc joins them into
# one HTML page (with a contents list and a section per chapter) styled by guide.css, and
# WeasyPrint turns that into the PDF. Needs both: brew install pandoc weasyprint.
#
# --diagrams also needs Google Chrome and the network: Mermaid is loaded from a pinned CDN
# version and run in headless Chrome, with plain SVG text labels (WeasyPrint doesn't draw
# the HTML labels Mermaid uses by default).
#
# Screenshots come from docs/screenshots (scripts/screenshots.sh, demo mode).
set -euo pipefail
cd "$(dirname "$0")/.."
SRC="docs/user-guide"
OUT="docs/Darkslide-User-Guide.pdf"
MERMAID="https://cdn.jsdelivr.net/npm/mermaid@11.4.1/dist/mermaid.min.js"
CHROME="${CHROME:-/Applications/Google Chrome.app/Contents/MacOS/Google Chrome}"

die() { echo "error: $*" >&2; exit 1; }
command -v pandoc >/dev/null || die "pandoc not found (brew install pandoc)"
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

chapters=("$SRC"/[0-9][0-9]-*.md)
[[ -e "${chapters[0]}" ]] || die "no chapters in $SRC"
filter="$PWD/scripts/user-guide/kbd.lua"

# The cover page goes before the contents (its paths are relative to $SRC, like the chapters').
cat >"$tmp/cover.html" <<'HTML'
<section class="cover">
  <img class="logo" src="../brand/darkslide-logo.png" alt="Darkslide">
  <div class="title">User Guide</div>
  <div class="subtitle">Import, cull, develop, export and archive your RAW photos on a Mac</div>
  <img class="hero" src="../screenshots/grid.png" alt="">
  <div class="foot">Darkslide is free software under the Apache License 2.0 · The Darkslide Authors</div>
</section>
HTML

# --file-scope reads each chapter on its own, so the same heading in two chapters keeps two
# ids, and links such as 06-develop.md#looks-styles-and-presets become links inside the page
# (pandoc runs in $SRC so those file names match). The build copy sits in $SRC too, so its
# relative paths resolve.
build="$SRC/.guide.build.html"
(cd "$SRC" && pandoc "${chapters[@]#"$SRC"/}" \
  --from gfm+implicit_figures --to html5 --standalone \
  --file-scope --section-divs --toc --toc-depth=2 \
  --syntax-highlighting=none \
  --lua-filter "$filter" \
  --css guide.css \
  --include-before-body "$tmp/cover.html" \
  --metadata pagetitle="Darkslide User Guide" \
  --metadata toc-title="Contents" \
  --metadata lang=en \
  --metadata author-meta="The Darkslide Authors" \
  --metadata description-meta="How to import, cull, develop, export and archive photos with Darkslide for macOS.") |
  python3 -c '
import re, sys
# Screenshots: the JPEG copies. pandoc puts a text variation selector (U+FE0E) after
# symbols such as ↔, which WeasyPrint draws as a missing glyph: drop it.
page = sys.stdin.read().replace("︎", "")
sys.stdout.write(re.sub(r"\.\./screenshots/([\w-]+)\.png", lambda m: "file://" + sys.argv[1] + "/" + m.group(1) + ".jpg", page))
' "$tmp/shots" >"$build"
weasyprint --quiet "$build" "$OUT" || { /bin/rm -f "$build"; die "weasyprint failed"; }
/bin/rm -f "$build"
echo "wrote $OUT ($(/usr/bin/du -h "$OUT" | cut -f1))"
