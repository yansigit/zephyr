"""A chart from a CSV: uv run --with matplotlib==3.11.2 python chart.py IN.csv OUT.png|OUT.svg
    [--kind line|bar|barh|scatter|pie] [--title TEXT] [--x COLUMN] [--y COLUMN[,COLUMN..]]

The first column is x and the rest are series unless --x and --y say otherwise. Labels in Korean
print in NanumGothic. PNG for chat (150 dpi), SVG for a report.
"""

import argparse
import csv
import pathlib

import matplotlib

matplotlib.use("Agg")
import matplotlib.pyplot as plt  # noqa: E402
from matplotlib import font_manager  # noqa: E402


def number(v):
    try:
        return float(v.replace(",", ""))
    except ValueError:
        return None


def main():
    p = argparse.ArgumentParser()
    p.add_argument("source")
    p.add_argument("out")
    p.add_argument("--kind", default="line", choices=["line", "bar", "barh", "scatter", "pie"])
    p.add_argument("--title", default="")
    p.add_argument("--x")
    p.add_argument("--y")
    a = p.parse_args()

    names = {f.name for f in font_manager.fontManager.ttflist}
    family = [f for f in ("NanumGothic", "DejaVu Sans") if f in names]
    plt.rcParams["font.family"] = family or ["sans-serif"]
    plt.rcParams["axes.unicode_minus"] = False

    with open(a.source, newline="", encoding="utf-8-sig") as f:
        rows = list(csv.DictReader(f))
    if not rows:
        raise SystemExit("the CSV has no rows")
    columns = list(rows[0].keys())
    x = a.x or columns[0]
    ys = a.y.split(",") if a.y else [c for c in columns if c != x]
    labels = [r[x] for r in rows]

    fig, ax = plt.subplots(figsize=(9, 5), layout="constrained")
    if a.kind == "pie":
        ax.pie([number(r[ys[0]]) or 0 for r in rows], labels=labels, autopct="%1.0f%%")
        ax.axis("equal")
    else:
        width = 0.8 / max(len(ys), 1)
        for i, y in enumerate(ys):
            values = [number(r[y]) for r in rows]
            if a.kind == "line":
                ax.plot(labels, values, marker="o", label=y)
            elif a.kind == "scatter":
                ax.scatter([number(v) for v in labels], values, label=y)
            elif a.kind == "bar":
                ax.bar([j + i * width for j in range(len(rows))], values, width, label=y)
                ax.set_xticks([j + width * (len(ys) - 1) / 2 for j in range(len(rows))], labels)
            else:
                ax.barh([j + i * width for j in range(len(rows))], values, width, label=y)
                ax.set_yticks([j + width * (len(ys) - 1) / 2 for j in range(len(rows))], labels)
        if len(ys) > 1:
            ax.legend()
        ax.grid(alpha=0.3)
    if a.title:
        ax.set_title(a.title)
    out = pathlib.Path(a.out)
    fig.savefig(out, dpi=150)
    print(out.resolve())


if __name__ == "__main__":
    main()
