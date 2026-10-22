"""Print an HTML page that renders one Mermaid file to SVG text in #out (scripts/user-guide.sh).

Labels are plain SVG text (htmlLabels off): WeasyPrint doesn't draw foreignObject.
"""

import html
import sys

src = open(sys.argv[1], encoding="utf-8").read()
mermaid_url = sys.argv[2]
print(f"""<!doctype html><meta charset="utf-8"><body>
<pre id="src">{html.escape(src)}</pre><div id="out"></div>
<script src="{mermaid_url}"></script>
<script>
mermaid.initialize({{startOnLoad: false, theme: "neutral", htmlLabels: false,
  flowchart: {{htmlLabels: false, curve: "basis"}},
  themeVariables: {{fontFamily: "Avenir Next, Helvetica Neue, sans-serif", fontSize: "15px"}}}});
mermaid.render("d", document.getElementById("src").textContent)
  .then(r => {{ document.getElementById("out").textContent = r.svg; }})
  .catch(e => {{ document.getElementById("out").textContent = "ERROR " + e; }});
</script>""")
