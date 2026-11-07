from __future__ import annotations

import base64
import html
from datetime import datetime
from pathlib import Path
from string import Template
from typing import Any


def render_report(
    results: list[dict[str, Any]],
    out_path: Path,
    *,
    title: str | None = None,
) -> Path:
    out_path.parent.mkdir(parents=True, exist_ok=True)
    ts = title or datetime.now().strftime("%Y-%m-%d %H:%M:%S")
    counts = _summary_counts(results)
    sections = "\n".join(_render_file_section(idx, fr) for idx, fr in enumerate(results))
    body = _PAGE_TEMPLATE.safe_substitute(
        title=html.escape(f"tmiki diff @ {ts}"),
        css=_CSS,
        js=_JS,
        ts=html.escape(ts),
        **counts,
        sections=sections or _EMPTY_SECTION,
    )
    out_path.write_text(body, encoding="utf-8")
    return out_path


def _b64(data: bytes) -> str:
    return base64.b64encode(data).decode("ascii")


def _data_uri(data: bytes) -> str:
    # Previews are WebP, JPEG or PNG (see tmi.diff._preview_bytes).
    if data[:4] == b"RIFF" and data[8:12] == b"WEBP":
        mime = "image/webp"
    elif data[:3] == b"\xff\xd8\xff":
        mime = "image/jpeg"
    else:
        mime = "image/png"
    return f"data:{mime};base64,{_b64(data)}"


def _summary_counts(results: list[dict[str, Any]]) -> dict[str, int]:
    """Files and layers, counted apart (issue #24: one changed layer used to
    read "changed: 2", the file's composite counted as a second change). A
    PNG/JPEG's single image is not a layer."""
    counts = {f"{unit}_{state}": 0 for unit in ("files", "layers") for state in ("added", "changed", "deleted")}
    for fr in results:
        state = fr.get("state")
        counts["files_" + ("added" if state == "added" else "deleted" if state == "deleted" else "changed")] += 1
        if fr.get("raster"):
            continue
        for layer in fr.get("layers", []):
            key = f"layers_{layer.get('state')}"
            if key in counts:
                counts[key] += 1
    return counts


def _file_states(file_result: dict[str, Any]) -> list[str]:
    states: set[str] = set()
    if file_result.get("state") == "deleted":
        states.add("deleted")
    if file_result.get("bbox"):
        states.add("changed")
    for layer in file_result.get("layers", []):
        s = layer.get("state")
        if s:
            states.add(s)
    return sorted(states)


def _render_file_section(idx: int, fr: dict[str, Any]) -> str:
    path = html.escape(fr.get("path", ""))
    states = _file_states(fr)
    states_attr = ",".join(states) if states else "changed"

    if fr.get("state") == "deleted":
        return Template(
            '<section class="file" data-states="$states">\n'
            '  <h2><span class="state deleted">deleted</span> $path</h2>\n'
            '  <p class="note">file removed from working tree</p>\n'
            "</section>"
        ).safe_substitute(states=states_attr, path=path)

    composite = _render_composite(idx, fr)
    # A PNG/JPEG's one "layer" is the image the composite already shows.
    layers_table = "" if fr.get("raster") else _render_layers_table(fr.get("layers", []))
    bbox = fr.get("bbox")
    bbox_label = "changed pixels bbox" if fr.get("raster") else "composite bbox"
    bbox_line = (
        f'<p class="bbox">{bbox_label}: {html.escape(str(bbox))}</p>' if bbox else ""
    )

    return Template(
        '<section class="file" data-states="$states">\n'
        "  <h2>$path</h2>\n"
        "  $bbox_line\n"
        "  $composite\n"
        "  $layers\n"
        "</section>"
    ).safe_substitute(
        states=states_attr,
        path=path,
        bbox_line=bbox_line,
        composite=composite,
        layers=layers_table,
    )


def _render_composite(idx: int, fr: dict[str, Any]) -> str:
    h = fr.get("html")
    if not h:
        return ""
    sbs = _data_uri(h["side_by_side_png"])
    ovl = _data_uri(h["overlay_png"])
    pl = _data_uri(h["diff_preview_png"])
    old_uri = _data_uri(h["old_png"])
    new_uri = _data_uri(h["new_png"])
    w, hgt = h.get("size", [1, 1])
    aspect = f"{max(w, 1)} / {max(hgt, 1)}"
    full = ""
    if h.get("full_old") and h.get("full_new"):
        full = (
            f'<p class="note">full size: <a href="{html.escape(h["full_old"])}">old</a> / '
            f'<a href="{html.escape(h["full_new"])}">new</a> ({w}&times;{hgt})</p>\n'
        )
    return full + Template(
        '<div class="composite">\n'
        '  <div class="tabs" role="tablist">\n'
        '    <button data-tab="sbs" aria-selected="true">side-by-side</button>\n'
        '    <button data-tab="ovl" aria-selected="false">overlay</button>\n'
        '    <button data-tab="pl" aria-selected="false">per-layer</button>\n'
        '    <button data-tab="sw" aria-selected="false">swipe</button>\n'
        "  </div>\n"
        '  <div class="panels">\n'
        '    <div data-panel="sbs" class="panel active"><img src="$sbs" alt="side-by-side"></div>\n'
        '    <div data-panel="ovl" class="panel"><img src="$ovl" alt="overlay"></div>\n'
        '    <div data-panel="pl" class="panel"><img src="$pl" alt="per-layer"></div>\n'
        '    <div data-panel="sw" class="panel swipe">\n'
        '      <div class="swipe-wrap" style="aspect-ratio: $aspect;">\n'
        '        <img class="swipe-old" src="$old" alt="old">\n'
        '        <img class="swipe-new" src="$new" alt="new" style="clip-path: inset(0 0 0 50%);">\n'
        '        <div class="swipe-handle" style="left:50%;"></div>\n'
        "      </div>\n"
        '      <input type="range" min="0" max="100" value="50" class="swipe-range" aria-label="swipe position">\n'
        "    </div>\n"
        "  </div>\n"
        "</div>"
    ).safe_substitute(sbs=sbs, ovl=ovl, pl=pl, old=old_uri, new=new_uri, aspect=aspect)


def _render_layers_table(layers: list[dict[str, Any]]) -> str:
    if not layers:
        return ""
    rows = "\n".join(_render_layer_row(l) for l in layers)
    return (
        '<div class="table-wrap">\n'
        '<table class="layers">\n'
        "  <thead><tr>"
        '<th>state</th><th>layer</th><th>property changes</th><th class="bbox-col">bbox</th><th>preview</th>'
        "</tr></thead>\n"
        f"  <tbody>\n{rows}\n  </tbody>\n"
        "</table>\n"
        "</div>"
    )


def _render_layer_row(layer: dict[str, Any]) -> str:
    state = layer.get("state", "changed")
    name = html.escape(layer.get("name", ""))
    bbox = layer.get("bbox")
    bbox_cell = html.escape(str(bbox)) if bbox else "&mdash;"
    props = _render_prop_changes(layer.get("prop_changes", []))
    preview = _render_layer_preview(layer)
    return Template(
        '    <tr class="layer" data-state="$state">\n'
        '      <td><span class="state $state">$state</span></td>\n'
        "      <td>$name</td>\n"
        "      <td>$props</td>\n"
        '      <td class="bbox-col">$bbox</td>\n'
        "      <td>$preview</td>\n"
        "    </tr>"
    ).safe_substitute(state=state, name=name, props=props, bbox=bbox_cell, preview=preview)


def _render_prop_changes(changes: list[dict[str, Any]]) -> str:
    if not changes:
        return "&mdash;"
    parts = []
    for c in changes:
        key = html.escape(str(c.get("key", "")))
        if c.get("old") is None and c.get("new") is None:
            parts.append(f'<div class="prop"><code>{key}</code> changed</div>')
            continue
        old = html.escape(str(c.get("old")))
        new = html.escape(str(c.get("new")))
        parts.append(f'<div class="prop"><code>{key}</code>: {old} &rarr; {new}</div>')
    return "\n".join(parts)


def _render_layer_preview(layer: dict[str, Any]) -> str:
    h = layer.get("html")
    if not h:
        return "&mdash;"
    sbs = _data_uri(h["side_by_side_png"])
    ovl = _data_uri(h["overlay_png"])
    return Template(
        "<details>\n"
        "  <summary>show</summary>\n"
        '  <div class="layer-preview">\n'
        '    <figure><figcaption>side-by-side</figcaption><img src="$sbs" alt="side-by-side"></figure>\n'
        '    <figure><figcaption>overlay</figcaption><img src="$ovl" alt="overlay"></figure>\n'
        "  </div>\n"
        "</details>"
    ).safe_substitute(sbs=sbs, ovl=ovl)


_EMPTY_SECTION = '<section class="file"><p class="note">no differences</p></section>'


_CSS = """
:root { color-scheme: dark; }
* { box-sizing: border-box; }
body {
  margin: 0;
  font-family: -apple-system, "Segoe UI", Roboto, "Helvetica Neue", sans-serif;
  background: #111;
  color: #ddd;
  line-height: 1.5;
}
header {
  position: sticky;
  top: 0;
  z-index: 10;
  background: #181818;
  border-bottom: 1px solid #2a2a2a;
  padding: 12px 24px;
  display: flex;
  flex-wrap: wrap;
  align-items: center;
  gap: 16px;
}
header h1 { font-size: 16px; margin: 0; font-weight: 600; }
.summary { font-size: 13px; color: #bbb; display: flex; flex-wrap: wrap; gap: 4px 16px; }
.summary .count-group { display: inline-flex; flex-wrap: wrap; gap: 0 8px; }
.summary .unit { color: #888; }
.summary .added { color: #6ec46e; }
.summary .changed { color: #d6a542; }
.summary .deleted { color: #d96b6b; }
.filters { display: flex; gap: 12px; font-size: 13px; }
.filters label { cursor: pointer; user-select: none; }
main { padding: 24px; max-width: 1400px; margin: 0 auto; }
section.file {
  background: #161616;
  border: 1px solid #2a2a2a;
  border-radius: 6px;
  padding: 16px 20px;
  margin-bottom: 24px;
}
section.file h2 {
  font-size: 15px;
  margin: 0 0 12px;
  font-family: ui-monospace, SFMono-Regular, Menlo, monospace;
  word-break: break-all;
}
.note { color: #888; font-style: italic; }
.bbox { color: #888; font-size: 12px; margin: 0 0 12px; }
.state {
  display: inline-block;
  padding: 1px 8px;
  border-radius: 3px;
  font-size: 11px;
  font-weight: 600;
  letter-spacing: 0.04em;
  text-transform: uppercase;
}
.state.added { background: #1f6f1f; color: #fff; }
.state.changed { background: #a07a00; color: #fff; }
.state.deleted { background: #8a2a2a; color: #fff; }
.composite { margin-bottom: 16px; }
.tabs {
  display: flex;
  gap: 2px;
  border-bottom: 1px solid #2a2a2a;
  margin-bottom: 12px;
}
.tabs button {
  background: transparent;
  border: none;
  color: #888;
  padding: 8px 14px;
  cursor: pointer;
  font-size: 13px;
  border-bottom: 2px solid transparent;
}
.tabs button:hover { color: #ddd; }
.tabs button[aria-selected="true"] {
  color: #fff;
  border-bottom-color: #6ec4e8;
}
.panel { display: none; }
.panel.active { display: block; }
.panel img { max-width: 100%; height: auto; display: block; background: #0a0a0a; }
.swipe-wrap {
  position: relative;
  width: 100%;
  max-width: 100%;
  background: #0a0a0a;
  overflow: hidden;
  user-select: none;
}
.swipe-wrap img {
  position: absolute;
  inset: 0;
  width: 100%;
  height: 100%;
  object-fit: contain;
}
.swipe-handle {
  position: absolute;
  top: 0;
  bottom: 0;
  width: 2px;
  background: #fff;
  mix-blend-mode: difference;
  pointer-events: none;
}
.swipe-range {
  width: 100%;
  margin-top: 8px;
}
.table-wrap { overflow-x: auto; max-width: 100%; }
table.layers {
  width: 100%;
  border-collapse: collapse;
  font-size: 13px;
}
table.layers th, table.layers td {
  text-align: left;
  padding: 6px 10px;
  border-bottom: 1px solid #232323;
  vertical-align: top;
}
table.layers th {
  color: #888;
  font-weight: 500;
  font-size: 11px;
  text-transform: uppercase;
  letter-spacing: 0.04em;
}
.prop code {
  background: #222;
  padding: 1px 5px;
  border-radius: 3px;
  font-size: 12px;
}
.layer-preview {
  display: flex;
  gap: 12px;
  flex-wrap: wrap;
  margin-top: 8px;
}
.layer-preview figure {
  margin: 0;
  background: #0a0a0a;
  padding: 8px;
  border-radius: 4px;
}
.layer-preview figcaption {
  font-size: 11px;
  color: #888;
  margin-bottom: 4px;
}
.layer-preview img { max-width: 320px; height: auto; display: block; }
details summary { cursor: pointer; color: #6ec4e8; font-size: 12px; }
@media (max-width: 600px) {
  header { padding: 10px 12px; gap: 8px 16px; }
  main { padding: 12px; }
  section.file { padding: 12px; }
  table.layers .bbox-col { display: none; }
  table.layers th, table.layers td { padding: 6px; }
  .layer-preview img { max-width: 100%; }
}
"""


_JS = """
(function() {
  document.querySelectorAll('.tabs').forEach(function(tabs) {
    tabs.addEventListener('click', function(e) {
      var btn = e.target.closest('button[data-tab]');
      if (!btn) return;
      var section = tabs.parentElement;
      tabs.querySelectorAll('button').forEach(function(b) {
        b.setAttribute('aria-selected', b === btn ? 'true' : 'false');
      });
      section.querySelectorAll('[data-panel]').forEach(function(p) {
        p.classList.toggle('active', p.dataset.panel === btn.dataset.tab);
      });
    });
  });

  document.querySelectorAll('.swipe').forEach(function(sw) {
    var range = sw.querySelector('.swipe-range');
    var newImg = sw.querySelector('.swipe-new');
    var handle = sw.querySelector('.swipe-handle');
    var update = function() {
      var v = range.value;
      newImg.style.clipPath = 'inset(0 0 0 ' + v + '%)';
      handle.style.left = v + '%';
    };
    range.addEventListener('input', update);
    update();
  });

  var filters = document.querySelectorAll('.filters input[type=checkbox]');
  var apply = function() {
    var enabled = new Set();
    filters.forEach(function(c) { if (c.checked) enabled.add(c.value); });
    document.querySelectorAll('section.file').forEach(function(sec) {
      var states = (sec.dataset.states || '').split(',').filter(Boolean);
      var show = states.length === 0 || states.some(function(s) { return enabled.has(s); });
      sec.style.display = show ? '' : 'none';
    });
    document.querySelectorAll('tr.layer').forEach(function(tr) {
      tr.style.display = enabled.has(tr.dataset.state) ? '' : 'none';
    });
  };
  filters.forEach(function(c) { c.addEventListener('change', apply); });
})();
"""


_PAGE_TEMPLATE = Template(
    """<!doctype html>
<html lang="ja">
<head>
  <meta charset="utf-8">
  <meta name="viewport" content="width=device-width, initial-scale=1">
  <title>$title</title>
  <style>$css</style>
</head>
<body>
  <header>
    <h1>tmiki diff @ $ts</h1>
    <div class="summary">
      <span class="count-group"><span class="unit">files:</span>
        <span class="added">$files_added added</span>
        <span class="changed">$files_changed changed</span>
        <span class="deleted">$files_deleted deleted</span></span>
      <span class="count-group"><span class="unit">layers:</span>
        <span class="added">$layers_added added</span>
        <span class="changed">$layers_changed changed</span>
        <span class="deleted">$layers_deleted deleted</span></span>
    </div>
    <div class="filters">
      <label><input type="checkbox" value="added" checked> added</label>
      <label><input type="checkbox" value="changed" checked> changed</label>
      <label><input type="checkbox" value="deleted" checked> deleted</label>
    </div>
  </header>
  <main>
$sections
  </main>
  <script>$js</script>
</body>
</html>
"""
)
