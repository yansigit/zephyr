---
name: charts
description: "Use when the owner asks for a chart or graph of some numbers: a PNG for chat or an SVG for a report, made with matplotlib, sent with send_file."
---

# Charts: numbers to a PNG or SVG

1. Put the data in a CSV in your work folder (`~/work/<topic>/data.csv`), a header row first:
   the x values in the first column, one column per series.
2. Draw it (matplotlib is fetched on first use, so this needs the network once):

   ```sh
   uv run --with matplotlib==3.11.2 python ~/.approved/skills/charts/chart.py data.csv chart.png --kind line --title "월별 매출"
   ```

   `--kind` is `line`, `bar`, `barh`, `scatter` or `pie`; `--x COLUMN` and `--y A,B` pick
   columns. Korean labels print in NanumGothic. A `.svg` output is for a report (the `report`
   skill takes `![chart](chart.svg)`); a `.png` is for chat.
3. For anything the script does not cover, write your own matplotlib script and run it the same
   way (`uv run --with matplotlib==3.11.2 python my_chart.py`), setting
   `plt.rcParams["font.family"] = ["NanumGothic"]` for Korean text.
4. `read` the PNG to check it, then deliver with `send_file`.

Never upload a chart anywhere to share it (an image host, a chart service): that is a post,
which goes past the owner. `send_file` is how it reaches them.
