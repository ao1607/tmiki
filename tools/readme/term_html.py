"""Turn the recorded session (session.txt) into a terminal-looking HTML page.

    python term_html.py session.txt out.html "commit -a" "merge" "log --graph"

Only the blocks whose command starts with one of the given prefixes are kept,
in the session's order; the output text itself is not changed.
"""
import html
import re
import sys
from pathlib import Path

src, out, *keep = sys.argv[1:]
blocks, cur = [], None
for line in Path(src).read_text(encoding="utf-8").splitlines():
    if line.startswith("> tmiki "):
        cur = [line[len("> tmiki "):], []]
        blocks.append(cur)
    elif cur is not None:
        cur[1].append(line)
blocks = [b for b in blocks if any(b[0].startswith(k) for k in keep)]

HASH = re.compile(r"\b([0-9a-f]{12})\b")


def fmt_out(text):
    t = html.escape(text)
    t = HASH.sub(r'<span class="h">\1</span>', t)
    t = re.sub(r"^([*|\\/ ]+)(?=\s|<)", lambda m: f'<span class="g">{m.group(1)}</span>', t, flags=re.M)
    return t


body = []
for cmd, lines in blocks:
    while lines and not lines[-1].strip():
        lines.pop()
    body.append(f'<div class="cmd"><span class="p">PS D:\\tsumiki&gt;</span> tmiki {html.escape(cmd)}</div>')
    body.append(f'<pre>{fmt_out(chr(10).join(lines))}</pre>')

page = f"""<!doctype html><html><head><meta charset="utf-8"><style>
html,body{{margin:0;background:transparent}}
.win{{margin:24px;width:820px;background:#0d1117;border:1px solid #30363d;border-radius:10px;overflow:hidden}}
.bar{{height:34px;display:flex;align-items:center;padding:0 14px;background:#161b22;border-bottom:1px solid #30363d;
  color:#8b949e;font:13px "Segoe UI","Yu Gothic UI",sans-serif}}
.term{{padding:16px 20px 18px;font:15px/1.55 "Cascadia Mono",Consolas,"BIZ UDGothic","MS Gothic",monospace;color:#c9d1d9}}
.cmd{{color:#e6edf3;font-weight:600;margin-top:10px}}
.cmd:first-child{{margin-top:0}}
.p{{color:#7d8590;font-weight:400}}
pre{{margin:2px 0 0;font:inherit;white-space:pre}}
.h{{color:#d29922}}
.g{{color:#7d8590}}
</style></head><body><div class="win"><div class="bar">Windows PowerShell</div><div class="term">
{''.join(body)}
</div></div></body></html>"""
Path(out).write_text(page, encoding="utf-8")
print("wrote", out, len(blocks), "blocks")
