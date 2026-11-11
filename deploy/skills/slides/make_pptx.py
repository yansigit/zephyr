"""Markdown slides to .pptx: uv run --with python-pptx==1.0.2 python make_pptx.py IN.md OUT.pptx

Slides are separated by lines that are exactly `---`. In each, the first `#` or `##` heading is
the title; `-`/`*` list items (indented by two spaces for a second level) and plain lines are the
body; a `![alt](path)` line puts that picture on the slide.
"""

import pathlib
import re
import sys

from pptx import Presentation
from pptx.util import Inches, Pt

IMAGE = re.compile(r"^!\[[^\]]*\]\(([^)]+)\)$")


def slides_of(text):
    parts, current = [], []
    for line in text.splitlines():
        if line.strip() == "---":
            parts.append(current)
            current = []
        else:
            current.append(line)
    parts.append(current)
    return [p for p in parts if any(l.strip() for l in p)]


def main(argv):
    if len(argv) != 2:
        sys.exit("usage: uv run --with python-pptx==1.0.2 python make_pptx.py IN.md OUT.pptx")
    source, out = pathlib.Path(argv[0]), pathlib.Path(argv[1])
    deck = Presentation()
    deck.slide_width, deck.slide_height = Inches(13.333), Inches(7.5)
    for lines in slides_of(source.read_text(encoding="utf-8")):
        slide = deck.slides.add_slide(deck.slide_layouts[1])
        title, body = slide.shapes.title, slide.placeholders[1]
        body.text_frame.clear()
        first = True
        for line in lines:
            if not line.strip():
                continue
            heading = re.match(r"^#{1,2}\s+(.*)$", line)
            if heading and not title.text:
                title.text = heading.group(1)
                continue
            image = IMAGE.match(line.strip())
            if image:
                path = (source.parent / image.group(1)).resolve()
                slide.shapes.add_picture(str(path), Inches(7.2), Inches(1.6), height=Inches(5))
                continue
            item = re.match(r"^(\s*)[-*]\s+(.*)$", line)
            text, level = (item.group(2), min(len(item.group(1)) // 2, 4)) if item else (line.strip(), 0)
            para = body.text_frame.paragraphs[0] if first else body.text_frame.add_paragraph()
            para.text, para.level = text, level
            para.font.size = Pt(24 if level == 0 else 20)
            first = False
    deck.save(out)
    print(out.resolve())


if __name__ == "__main__":
    main(sys.argv[1:])
