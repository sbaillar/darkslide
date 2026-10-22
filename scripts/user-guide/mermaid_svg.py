"""Write the SVG a mermaid_page.py page rendered, from Chrome's --dump-dom (scripts/user-guide.sh).

Exits with an error when Mermaid failed (a syntax error, or the script didn't load).
"""

import html
import re
import sys

dom = open(sys.argv[1], encoding="utf-8").read()
m = re.search(r'<div id="out">(.*?)</div>', dom, re.S)
svg = html.unescape(m.group(1)).strip() if m else ""
if not svg.startswith("<svg"):
    sys.exit(f"mermaid failed for {sys.argv[2]}: {svg[:300] or 'no output'}")
with open(sys.argv[2], "w", encoding="utf-8") as f:
    f.write(svg + "\n")
