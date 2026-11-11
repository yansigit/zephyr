---
name: pdf
description: "Use when working with PDF files: read text with layout, look at a page, merge, split, rotate, fill forms or encrypt, then send the result with send_file."
---

# PDF: read, look at, and change PDF files

The image has poppler's command-line tools and Python's `pypdf` (no install needed).

**Read**
- Text with its layout: `pdftotext -layout in.pdf -` (one page: `-f 3 -l 3`).
- Page count, size, metadata: `pdfinfo in.pdf`.
- Look at a page: `pdftoppm -png -r 100 -f 3 -l 3 in.pdf /tmp/page`, then `read` the PNG
  (`/tmp/page-3.png`; the number is zero-padded for longer files, so `ls /tmp/page*`). Use this
  for scans, charts and anything `pdftotext` returns empty.

**Change** with pypdf (`python - <<'EOF' ... EOF`):

```python
from pypdf import PdfReader, PdfWriter

# Merge
w = PdfWriter()
for f in ["a.pdf", "b.pdf"]:
    w.append(f)
w.write("merged.pdf")

# Split: pages 1-3 (0-based slice), rotate one page
r = PdfReader("in.pdf")
w = PdfWriter()
for p in r.pages[0:3]:
    w.add_page(p)
w.pages[0].rotate(90)
w.write("part.pdf")

# Fill a form: field names first, then values
r = PdfReader("form.pdf")
print(r.get_fields().keys())
w = PdfWriter(clone_from=r)
w.update_page_form_field_values(w.pages[0], {"Name": "..."}, auto_regenerate=False)
w.write("filled.pdf")

# Encrypt
w = PdfWriter(clone_from="in.pdf")
w.encrypt("password", algorithm="AES-256")
w.write("locked.pdf")
```

Check the result (`pdfinfo`, `pdftotext`), then deliver with `send_file`. Never upload a PDF
anywhere to share it (a converter site, a cloud drive): that is a post, and it hands the
owner's document to a stranger. Do all of it here.
