//! The page template of `diff --html` (its CSS, script and HTML).

pub const CSS: &str = r#"
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
"#;

pub const JS: &str = r#"
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
"#;

pub const PAGE: &str = r#"<!doctype html>
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
"#;

pub const EMPTY_SECTION: &str = r#"<section class="file"><p class="note">no differences</p></section>"#;
