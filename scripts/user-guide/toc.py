"""Print guide.html with its table of contents filled in (scripts/user-guide.sh).

Chapters are the h1 headings with an id (the "Chapter N" kicker before one gives the number),
sections the h2 headings with an id. WeasyPrint adds the page numbers
(target-counter in guide.css).
"""

import re
import sys

page = open(sys.argv[1], encoding="utf-8").read()
items = []
pattern = r'(?:<div class="num">(?:Chapter (\d+))?[^<]*</div>\s*)?<h([12]) id="([^"]+)">(.*?)</h\2>'
for n, level, ident, text in re.findall(pattern, page, re.S):
    text = re.sub(r"<[^>]+>", "", text).strip()
    if level == "1":
        items.append(f'<li class="ch"><a href="#{ident}"><span class="n">{n}</span>{text}</a></li>')
    else:
        items.append(f'<li class="sec"><a href="#{ident}">{text}</a></li>')
toc = "<ol>\n" + "\n".join(items) + "\n</ol>"
sys.stdout.write(page.replace("<!-- TOC -->", toc))
