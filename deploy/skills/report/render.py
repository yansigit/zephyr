"""Markdown to PDF with the image's Chromium: python render.py IN.md OUT.pdf [--slides]

Markdown (headings, tables, code, strikethrough, local images) becomes HTML with markdown-it-py
and this folder's style.css, then Chromium prints it. With --slides, each part between lines
that are exactly `---` is one 16:9 page.

The Markdown is not trusted (it may hold text from a web page): raw HTML in it is escaped, never
passed through, and the page runs no script and Chromium has no network, so a `<script>` does not
run and a remote image is not fetched. Local images still print.
"""

import html
import os
import pathlib
import subprocess
import sys
import tempfile

from markdown_it import MarkdownIt

HERE = pathlib.Path(__file__).resolve().parent


def slides_of(text):
    parts, current = [], []
    for line in text.splitlines():
        if line.strip() == "---":
            parts.append("\n".join(current))
            current = []
        else:
            current.append(line)
    parts.append("\n".join(current))
    return [p for p in parts if p.strip()]


def page(source, slides):
    md = MarkdownIt("commonmark", {"html": False}).enable(["table", "strikethrough"])
    text = source.read_text(encoding="utf-8")
    title = html.escape(source.stem)
    if slides:
        body = "\n".join(f"<section>{md.render(s)}</section>" for s in slides_of(text))
        kind = "slides"
    else:
        body = md.render(text)
        kind = "report"
    style = (HERE / "style.css").read_text(encoding="utf-8")
    if slides:
        style += "\n@page { size: 13.333in 7.5in; margin: 0; }\n"
    # Relative image paths resolve against the markdown file's folder.
    base = source.parent.resolve().as_uri() + "/"
    # No script runs and nothing is fetched but local files: the page's own policy, since
    # Chromium's headless print needs scripting on (`--blink-settings=scriptEnabled=false`
    # makes it write no PDF).
    policy = (
        "default-src 'none'; img-src file: data:; style-src 'unsafe-inline'; font-src file: data:"
    )
    return (
        f'<!doctype html><html><head><meta charset="utf-8">'
        f'<meta http-equiv="Content-Security-Policy" content="{policy}"><base href="{base}">'
        f"<title>{title}</title><style>{style}</style></head>"
        f'<body class="{kind}">{body}</body></html>'
    )


def main(argv):
    args = [a for a in argv if a != "--slides"]
    if len(args) != 2:
        sys.exit("usage: python render.py IN.md OUT.pdf [--slides]")
    source, out = pathlib.Path(args[0]), pathlib.Path(args[1]).resolve()
    chrome = os.environ.get("AGENT_BROWSER_EXECUTABLE_PATH")
    if not chrome:
        sys.exit("AGENT_BROWSER_EXECUTABLE_PATH is not set: the image's Chromium is not known")
    with tempfile.TemporaryDirectory() as tmp:
        doc = pathlib.Path(tmp) / "page.html"
        doc.write_text(page(source, "--slides" in argv), encoding="utf-8")
        subprocess.run(
            [
                chrome,
                "--headless",
                "--no-sandbox",
                "--disable-gpu",
                f"--user-data-dir={tmp}/profile",
                "--no-pdf-header-footer",
                # No network at all: a proxy that refuses, and no name resolves.
                "--proxy-server=127.0.0.1:9",
                "--proxy-bypass-list=<-loopback>",
                "--host-resolver-rules=MAP * ~NOTFOUND",
                "--disable-background-networking",
                f"--print-to-pdf={out}",
                doc.as_uri(),
            ],
            check=True,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.DEVNULL,
            timeout=120,
        )
    if not out.exists():
        sys.exit("Chromium wrote no PDF")
    print(out)


if __name__ == "__main__":
    main(sys.argv[1:])
