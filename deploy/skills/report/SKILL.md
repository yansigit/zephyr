---
name: report
description: "Use when the owner asks for a report, summary or document as a PDF: write Markdown, render it to PDF, send it with send_file."
---

# Report: Markdown to PDF

1. Write the report as Markdown in your work folder (`~/work/<topic>/report.md`): headings,
   paragraphs, lists, tables (`| a | b |`), code blocks, `~~struck~~` text, and images by a path
   relative to the file (`![chart](chart.png)`; make charts with the `charts` skill). Korean and
   English both print correctly (NanumGothic).
2. Render it:

   ```sh
   python ~/.approved/skills/report/render.py ~/work/<topic>/report.md ~/work/<topic>/report.pdf
   ```

   It prints the PDF's path. It uses the image's Chromium, headless, and needs no network.
3. Check it before sending: `pdftotext -layout report.pdf - | head -40` for the text, and
   `pdftoppm -png -r 60 -f 1 -l 1 report.pdf /tmp/page` then `read` `/tmp/page-1.png` to look at
   the first page.
4. Deliver it with `send_file` (`path`, and a one-line `caption`).

Never upload the result anywhere to share it (a paste site, a cloud drive, a gist): that is a
post, which goes past the owner. `send_file` is how it reaches them.
