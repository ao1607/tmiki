"""Write tmiki's logo lockups (mark + wordmark) as SVG, light and dark.

    python make_logo.py OUTDIR

The wordmark is drawn with strokes, not a font, so it looks the same
everywhere and carries no font licence.
"""
import sys
from pathlib import Path

out = Path(sys.argv[1])
out.mkdir(parents=True, exist_ok=True)

WOOD, BLUE, RED, YELLOW = "#E2B77A", "#6C9BD2", "#D9735B", "#EBC55A"
INK = "#4A3B32"


def mark(outline):
    return f"""<g stroke="{outline}" stroke-width="4" stroke-linejoin="round">
    <rect x="6" y="56" width="32" height="24" rx="2" fill="{WOOD}"/>
    <rect x="42" y="56" width="32" height="24" rx="2" fill="{BLUE}"/>
    <rect x="16" y="36" width="48" height="20" rx="2" fill="{RED}"/>
    <path d="M16 36 L40 12 L64 36 Z" fill="{YELLOW}"/>
  </g>"""


def wordmark(color, x0):
    # baseline 80, x-height 46, ascender 26, stroke 9
    t, m, i1, k, i2 = x0 + 8, x0 + 34, x0 + 96, x0 + 118, x0 + 160
    return f"""<g fill="none" stroke="{color}" stroke-width="9" stroke-linecap="round" stroke-linejoin="round">
    <path d="M{t} 28 V68 Q{t} 80 {t + 12} 80"/>
    <path d="M{t - 7} 47 H{t + 13}"/>
    <path d="M{m} 47 V80"/>
    <path d="M{m} 58 A12 12 0 0 1 {m + 24} 58 V80"/>
    <path d="M{m + 24} 58 A12 12 0 0 1 {m + 48} 58 V80"/>
    <path d="M{i1} 50 V80"/>
    <path d="M{k} 26 V80"/>
    <path d="M{k + 22} 48 L{k + 1} 66"/>
    <path d="M{k + 8} 60 L{k + 24} 80"/>
    <path d="M{i2} 50 V80"/>
  </g>
  <g fill="{color}"><circle cx="{i1}" cy="32" r="5.5"/><circle cx="{i2}" cy="32" r="5.5"/></g>"""


def lockup(text_color, outline):
    w, h = 268, 92
    return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {w} {h}" width="{w * 2}" height="{h * 2}" role="img" aria-label="tmiki">
  <g transform="translate(0 4)">
  {mark(outline)}
  {wordmark(text_color, 92)}
  </g>
</svg>
"""


def mark_only():
    return f"""<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 80 88" width="160" height="176" role="img" aria-label="tmiki">
  <g transform="translate(0 2)">{mark(INK)}</g>
</svg>
"""


(out / "tmiki-lockup-light.svg").write_text(lockup("#2B2420", INK), encoding="utf-8")
(out / "tmiki-lockup-dark.svg").write_text(lockup("#EDE6DA", INK), encoding="utf-8")
(out / "tmiki-mark.svg").write_text(mark_only(), encoding="utf-8")
print("wrote", sorted(p.name for p in out.glob("*.svg")))
