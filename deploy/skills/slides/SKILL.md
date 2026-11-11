---
name: slides
description: "Use when the owner asks for a presentation or slide deck: write Markdown slides, render a 16:9 PDF (or a .pptx when asked), send it with send_file."
---

# Slides: Markdown to a 16:9 deck

1. Write the deck as Markdown in your work folder (`~/work/<topic>/slides.md`), one slide per
   part, parts separated by a line that is exactly `---`. Start each slide with a `#` or `##`
   heading; keep it to a few short bullets. Images by a path relative to the file.
2. Render a PDF, one 16:9 page per slide, with the report skill's renderer (the image's
   Chromium, no network):

   ```sh
   python ~/.approved/skills/report/render.py ~/work/<topic>/slides.md ~/work/<topic>/slides.pdf --slides
   ```

3. Only when the owner asks for PowerPoint, make a `.pptx` (python-pptx is fetched on first use,
   so this needs the network once):

   ```sh
   uv run --with python-pptx==1.0.2 python ~/.approved/skills/slides/make_pptx.py ~/work/<topic>/slides.md ~/work/<topic>/slides.pptx
   ```

4. Check the PDF (`pdfinfo slides.pdf` gives the page count; `pdftoppm -png -r 50 -f 2 -l 2
   slides.pdf /tmp/s` then `read` `/tmp/s-2.png`), then deliver with `send_file`.

Never upload the result anywhere to share it (Google Slides, a cloud drive, a paste site): that
is a post, which goes past the owner. `send_file` is how it reaches them.
