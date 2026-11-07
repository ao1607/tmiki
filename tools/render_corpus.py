"""Render corpus: pinned CLIP and MDP2 renders for checking a renderer change or a second renderer.

    python tools/render_corpus.py --update                 # render every file (reference), write the corpus
    python tools/render_corpus.py --verify                 # render with tmi (tmi_accel) and compare
    python tools/render_corpus.py --verify --quick         # without the comic pages (run_tests.py --all)
    python tools/render_corpus.py --verify --render python # the reference renderer (tmiref) instead
    python tools/render_corpus.py --coverage               # which CLIP features the corpus exercises
    python tools/render_corpus.py --verify --only tama -j 4 --examples D:/illust_git/example

Files (relative to the example directory, all gitignored except tama.clip):
    *.clip                                  example     the files bench.py's goldens use
    *.mdp                                   mdp         MDP2 files (RUST_MIGRATION.md 2c)
    csp515/*.clip                           csp515      CSP 5.1.5 feature probes
    additional_examples/*.clip              probe       correction / material / vector probes
    additional_examples/extracted/**/*.clip comic       comic pages (tools/extract_examples.py)

Per file, ``tools/render_corpus.json`` keeps the sha256 of
  * the whole layered composite (white background; "WxH:" + RGBA bytes, as
    bench.py's goldens),
  * three windows of it (``region=``): the top-left quarter, a band across a
    tile-row boundary and a 1-pixel column,
  * each top-level folder rendered alone (``native.render_folder``),
plus the canvas size, the file's own sha256 (a different file is skipped,
not a mismatch), the ``tmi:`` warnings the renders printed, and the
features the file exercises (``--coverage``). An MDP2 file has no windows:
its record holds the composite over white, the composite over transparent
(as the region "transparent"), each folder alone and each renderable
layer's own pixels (the ``LayerCrop`` digest the importer stores); the
reference is ``tmiref.mdp2``.

Files run in parallel worker processes, one fresh process per file (the
warning de-duplication is per process), largest-first by earlier run times
(``.render-corpus-durations.json``, gitignored). Files the corpus lists but
this machine lacks are skipped and reported; ``--update`` keeps their
records unless ``--prune``. Renderers: ``rust`` is tmi's own
(``tmi.formats.clip``, tmi_accel), ``python`` the reference package
``tmiref.clip`` (RUST_MIGRATION.md 1e); ``--update`` always pins the
reference's output.
"""
from __future__ import annotations

import argparse
import contextlib
import hashlib
import io
import json
import multiprocessing as mp
import os
import re
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
TOOLS = ROOT / "tools"
CORPUS_PATH = TOOLS / "render_corpus.json"
DURATIONS = ROOT / ".render-corpus-durations.json"
RENDERERS = ("rust", "python")
TILE = 256


# ───────────────────────────── discovery ─────────────────────────────


def discover(examples: Path) -> dict[str, tuple[Path, str]]:
    """{key: (path, group)}; keys are "example/<path relative to examples>"."""
    found: dict[str, tuple[Path, str]] = {}

    def add(paths, group: str) -> None:
        for path in sorted(paths):
            if path.is_file():
                found[group_key(examples, path)] = (path, group)

    add(examples.glob("*.clip"), "example")
    add(examples.glob("*.mdp"), "mdp")
    add((examples / "csp515").glob("*.clip"), "csp515")
    add((examples / "additional_examples").glob("*.clip"), "probe")
    add((examples / "additional_examples" / "extracted").rglob("*.clip"), "comic")
    return found


def group_key(examples: Path, path: Path) -> str:
    return "example/" + path.relative_to(examples).as_posix()


def regions(width: int, height: int) -> list[tuple[str, list[int]]]:
    """The three windows rendered per file."""
    quarter = [0, 0, max(1, width // 2), max(1, height // 2)]
    # A band straddling the tile-row boundary nearest the middle.
    row = max(TILE, (height // 2) // TILE * TILE) if height > TILE else height // 2
    band = [0, max(0, row - 40), width, min(height, row + 40)]
    if band[1] >= band[3]:
        band = [0, 0, width, height]
    x = min(width - 1, width * 2 // 3 + 1)
    column = [x, 0, x + 1, height]
    return [("quarter", quarter), ("tile_row_band", band), ("column", column)]


# ───────────────────────────── one file (worker) ─────────────────────────────


def pixel_digest(image) -> str:
    rgba = image if image.mode == "RGBA" else image.convert("RGBA")
    h = hashlib.sha256(f"{rgba.width}x{rgba.height}:".encode("ascii"))
    h.update(rgba.tobytes())
    return h.hexdigest()


def _error(exc: BaseException) -> str:
    return f"{type(exc).__name__}: {exc}"


def features(clip) -> list[str]:
    """The CLIP features a file's visible layers exercise."""
    from tmiref import clip as C
    from tmiref import clip_style, clip_tone, clip_vector

    out: set[str] = set()

    def check(name: str, fn) -> None:
        try:
            if fn():
                out.add(name)
        except Exception:
            out.add(f"{name}?")  # the probe itself failed

    def walk(nodes) -> None:
        for node in nodes:
            if not getattr(node, "visible", True):
                out.add("hidden")
                continue
            style = clip_style.layer_style(clip, node.main_id)
            mode = C._composite_name(node.composite or 0)
            if (node.opacity or 0) < 256:
                out.add("opacity")
            if node.clip:
                out.add("clipping")
            check("mask", lambda: C._mask_is_enabled(node))
            check("edge", lambda: clip_style.parse_edge_effect(style))
            check("tone", lambda: clip_tone.parse_tone_effect(style))
            check("layer-colour", lambda: clip_style.layer_colour(style))
            colour_type = style.get("LayerColorTypeIndex") or 0
            if colour_type in (1, 2):
                out.add("expression-colour:" + ("mono" if colour_type == 1 else "grey"))
            if node.folder:
                if mode == C.BLEND_PASS_THROUGH:
                    out.add("pass-through")
                else:
                    out.add("folder")
                    out.add(f"blend:{mode}")
                if getattr(node, "comic_frame", False):
                    out.add("comic-frame")
                walk(node.children)
                continue
            if getattr(node, "type", 0) == 4098:
                blob = C._safe_layer_blob(clip, node.main_id, "FilterLayerInfo")
                if blob and len(blob) >= 8:
                    kind = int.from_bytes(bytes(blob[:4]), "big")
                    builder = C._FILTER_BUILDERS.get(kind)
                    name = builder.__name__.removeprefix("_filter_") if builder else f"unsupported-{kind}"
                    out.add(f"adjustment:{name}")
                continue
            out.add(f"blend:{mode}")
            if getattr(node, "type", 0) == 1584:
                out.add("paper")
                continue
            info = C.layer_offscreen_info(clip, node.render_mipmap)
            stored = bool(info and info[2] in clip.extas)
            if stored:
                out.add("raster")
            if style.get("ResizableOriginalMipmap"):
                out.add("material")
            elif not stored:
                check("vector", lambda: clip_vector.has_vector_strokes(clip, node))
            blob = C._safe_layer_blob(clip, node.main_id, "GradationFillInfo")
            if blob:
                try:
                    fill = C._parse_gradation_fill(bytes(blob))
                    out.add("fill-solid" if fill["solid"] is not None else "gradient")
                except Exception:
                    out.add("gradient?")

    walk(clip.root.children)
    return sorted(out)


def _memory_peak_mib() -> float | None:
    try:
        sys.path.insert(0, str(TOOLS))
        from bench import _memory_counters

        counters = _memory_counters()
    except Exception:
        return None
    return None if counters is None else counters[1] / 2**20


def mdp_features(mdp) -> list[str]:
    """The MDP2 features a file exercises (layer types, blend modes, ...)."""
    from tmiref import mdp2 as M

    out: set[str] = set()
    for layer in mdp.layers:
        if layer.children:
            out.add("mdp:folder")
            continue
        if not M.is_renderable_raster(mdp, layer):
            continue
        out.add(f"mdp:{layer.type}")
        out.add(f"blend:{M._MDP_BLEND_TO_TMI.get(layer.mode, 'normal')}")
        if layer.clipping:
            out.add("clipping")
        if layer.masking:
            out.add("mask")
        if layer.alpha < 255:
            out.add("opacity")
        if not M._effectively_visible(mdp, layer):
            out.add("hidden")
    return sorted(out)


def render_mdp_file(key: str, path: Path, group: str, renderer: str) -> dict:
    """The MDP2 record: the composite over white and over transparent, each
    folder alone (``native.render_folder``) and each layer's own pixels (the
    ``LayerCrop`` digest the importer stores)."""
    import dataclasses

    from tmi import native

    if renderer == "python":
        from tmiref import mdp2 as M
    else:
        from tmi.formats import mdp2 as M

    def folder_subset(mdp, folder):
        by_id = {layer.layer_id: layer for layer in mdp.layers}

        def inside(layer) -> bool:
            seen = set()
            cur = by_id.get(layer.parent_id)
            while cur is not None and cur.layer_id not in seen:
                if cur.layer_id == folder.layer_id:
                    return True
                seen.add(cur.layer_id)
                cur = by_id.get(cur.parent_id)
            return False

        return dataclasses.replace(mdp, layers=[layer for layer in mdp.layers if inside(layer)])

    raw = path.read_bytes()
    record: dict = {"group": group, "file_sha256": hashlib.sha256(raw).hexdigest(), "file_size": len(raw)}
    stderr = io.StringIO()
    with contextlib.redirect_stderr(stderr):
        mdp = M.parse_mdp2(raw)
        record["canvas"] = [mdp.canvas_width, mdp.canvas_height]
        try:
            record["composite"] = pixel_digest(M.render_composite_rgba(mdp))
        except Exception as exc:
            record["composite"] = {"error": _error(exc)}
        record["regions"] = []
        entry: dict = {"name": "transparent", "rect": [0, 0, mdp.canvas_width, mdp.canvas_height]}
        try:
            entry["sha256"] = pixel_digest(M.render_composite_rgba(mdp, (0, 0, 0, 0)))
        except Exception as exc:
            entry["error"] = _error(exc)
        record["regions"].append(entry)
        record["folders"] = []
        for layer in mdp.layers:
            if not layer.children:
                continue
            entry = {"main_id": layer.layer_id, "name": layer.name}
            try:
                if renderer == "python":
                    image = M.render_composite_rgba(folder_subset(mdp, layer), background=(0, 0, 0, 0))
                else:
                    image = native.render_folder(path, "mdp2", f"mdp2:{layer.layer_id}")
                entry["sha256"] = pixel_digest(image)
            except Exception as exc:
                entry["error"] = _error(exc)
            record["folders"].append(entry)
        record["layers"] = []
        for layer in mdp.layers:
            if not M.is_renderable_raster(mdp, layer):
                continue
            entry = {"id": layer.layer_id, "name": layer.name}
            try:
                entry["digest"] = M.render_layer_crop(mdp, layer).digest()
            except Exception as exc:
                entry["error"] = _error(exc)
            record["layers"].append(entry)
        from tmiref import mdp2 as ref

        record["features"] = mdp_features(ref.parse_mdp2(raw))
    record["warnings"] = sorted({line for line in stderr.getvalue().splitlines() if line.startswith("tmi:")})
    return record


def render_file(job: tuple[str, str, str, str]) -> dict:
    """Everything the corpus records for one file (plus timing and checks),
    rendered by ``renderer``."""
    key, path_text, group, renderer = job
    path = Path(path_text)
    start = time.perf_counter()
    if path.suffix.lower() == ".mdp":
        record = render_mdp_file(key, path, group, renderer)
        return {
            "key": key,
            "renderer": renderer,
            "record": record,
            "seconds": time.perf_counter() - start,
            "peak_mib": _memory_peak_mib(),
            "problems": [],
        }
    from tmi import native
    from tmi.formats import clip as C

    if renderer == "python":
        from tmiref import clip as R

        render = R.render_layered_composite_rgba

        def render_folder(clip, node):
            return R.render_folder(clip, node.main_id)
    else:
        render = C.render_layered_composite_rgba

        def render_folder(clip, node):
            return native.render_folder(path, "clip", f"clip:{node.main_id}")

    raw = path.read_bytes()
    record: dict = {"group": group, "file_sha256": hashlib.sha256(raw).hexdigest(), "file_size": len(raw)}
    problems: list[str] = []
    stderr = io.StringIO()
    with contextlib.redirect_stderr(stderr):
        clip = C.open_clip_bytes(raw)
        del raw
        width, height = clip.canvas.width, clip.canvas.height
        record["canvas"] = [width, height]
        full = None
        try:
            full = render(clip)
            record["composite"] = pixel_digest(full)
        except Exception as exc:
            record["composite"] = {"error": _error(exc)}
        record["regions"] = []
        for name, rect in regions(width, height):
            entry: dict = {"name": name, "rect": rect}
            try:
                image = render(clip, region=tuple(rect))
                entry["sha256"] = pixel_digest(image)
                if full is not None and pixel_digest(full.crop(tuple(rect))) != entry["sha256"]:
                    problems.append(f"region {name} {rect} differs from that crop of the composite")
            except Exception as exc:
                entry["error"] = _error(exc)
            record["regions"].append(entry)
        del full
        record["folders"] = []
        for node in clip.root.children:
            if not node.folder:
                continue
            entry = {"main_id": node.main_id, "name": node.name}
            try:
                entry["sha256"] = pixel_digest(render_folder(clip, node))
            except Exception as exc:
                entry["error"] = _error(exc)
            record["folders"].append(entry)
        record["features"] = features(clip)
        clip.db.close()
    record["warnings"] = sorted({line for line in stderr.getvalue().splitlines() if line.startswith("tmi:")})
    return {
        "key": key,
        "renderer": renderer,
        "record": record,
        "seconds": time.perf_counter() - start,
        "peak_mib": _memory_peak_mib(),
        "problems": problems,
    }


def _worker_init(env: dict[str, str]) -> None:
    os.environ.update(env)
    os.chdir(ROOT)
    sys.path.insert(0, str(ROOT))


# ───────────────────────────── running ─────────────────────────────


def count_records(record: dict) -> int:
    return 1 + len(record.get("regions", [])) + len(record.get("folders", [])) + len(record.get("layers", []))


def load_corpus() -> dict:
    if CORPUS_PATH.exists():
        return json.loads(CORPUS_PATH.read_text(encoding="utf-8"))
    return {"version": 1, "files": {}}


def write_corpus(corpus: dict) -> None:
    corpus["files"] = dict(sorted(corpus["files"].items()))
    text = json.dumps(corpus, indent=1, ensure_ascii=False)
    # Number lists (canvas sizes, rectangles) on one line each.
    text = re.sub(
        r"\[\s+(-?\d+(?:,\s+-?\d+)*)\s+\]",
        lambda m: "[" + ", ".join(v.strip() for v in m.group(1).split(",")) + "]",
        text,
    )
    CORPUS_PATH.write_text(text + "\n", encoding="utf-8", newline="\n")


def check_renderer(name: str) -> str | None:
    """None when ``name`` can render here, else why not."""
    sys.path.insert(0, str(ROOT))
    try:
        if name == "python":
            import tmiref.clip  # noqa: F401  (numpy, Pillow)
        from tmi import accel

        accel.native()  # tmi (and so the corpus' file reading) needs it
    except BaseException as exc:  # AccelUnavailableError is not an Exception
        if isinstance(exc, (KeyboardInterrupt, SystemExit)):
            raise
        return f"{type(exc).__name__}: {exc}"
    return None


def _duration_key(key: str, renderer: str) -> str:
    return key if renderer == "rust" else f"{renderer}:{key}"


def run_jobs(jobs: list[tuple[str, str, str, str]], workers: int, env: dict[str, str]) -> list[dict]:
    try:
        durations = json.loads(DURATIONS.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        durations = {}
    jobs = sorted(jobs, key=lambda j: (durations.get(_duration_key(j[0], j[3]), 0.0), Path(j[1]).stat().st_size), reverse=True)
    results: list[dict] = []
    ctx = mp.get_context("spawn")
    with ctx.Pool(min(workers, len(jobs)) or 1, initializer=_worker_init, initargs=(env,), maxtasksperchild=1) as pool:
        for done, result in enumerate(pool.imap_unordered(render_file, jobs), 1):
            results.append(result)
            durations[_duration_key(result["key"], result["renderer"])] = round(result["seconds"], 2)
            if sys.stdout.isatty():
                print(f"\r  {done}/{len(jobs)} {result['key'][-60:]:60}", end="", flush=True)
    if sys.stdout.isatty():
        print("\r" + " " * 70 + "\r", end="")
    try:
        DURATIONS.write_text(json.dumps(durations, indent=0, sort_keys=True), encoding="utf-8")
    except OSError:
        pass
    return results


def compare(key: str, want: dict, got: dict) -> list[str]:
    """The differences between a recorded and a fresh record."""
    out: list[str] = []
    if want.get("canvas") != got.get("canvas"):
        out.append(f"canvas {want.get('canvas')} -> {got.get('canvas')}")

    def short(value) -> str:
        return value[:16] if isinstance(value, str) else json.dumps(value, ensure_ascii=False)

    if want.get("composite") != got.get("composite"):
        out.append(f"composite {short(want.get('composite'))} -> {short(got.get('composite'))}")
    old_regions = {r["name"]: r for r in want.get("regions", [])}
    for region in got.get("regions", []):
        old = old_regions.pop(region["name"], None)
        if old is None:
            out.append(f"region {region['name']}: not in the corpus")
        elif old != region:
            what = old.get("sha256", old.get("error"))
            now = region.get("sha256", region.get("error"))
            out.append(f"region {region['name']} {region['rect']}: {short(what)} -> {short(now)}")
    out.extend(f"region {name}: no longer rendered" for name in old_regions)
    old_folders = {f["main_id"]: f for f in want.get("folders", [])}
    for folder in got.get("folders", []):
        old = old_folders.pop(folder["main_id"], None)
        label = f"folder {folder['main_id']} {folder['name']!r}"
        if old is None:
            out.append(f"{label}: not in the corpus")
        elif old.get("sha256") != folder.get("sha256") or old.get("error") != folder.get("error"):
            what = old.get("sha256", old.get("error"))
            now = folder.get("sha256", folder.get("error"))
            out.append(f"{label}: {short(what)} -> {short(now)}")
    out.extend(f"folder {main_id}: no longer a top-level folder" for main_id in old_folders)
    old_layers = {entry["id"]: entry for entry in want.get("layers", [])}
    for entry in got.get("layers", []):
        old = old_layers.pop(entry["id"], None)
        if old is None:
            out.append(f"layer {entry['id']}: not in the corpus")
        elif old != entry:
            out.append(f"layer {entry['id']} {entry['name']!r}: {short(old.get('digest', old.get('error')))} -> "
                       f"{short(entry.get('digest', entry.get('error')))}")
    out.extend(f"layer {layer_id}: no longer decoded" for layer_id in old_layers)
    if want.get("warnings", []) != got.get("warnings", []):
        lost = sorted(set(want.get("warnings", [])) - set(got.get("warnings", [])))
        new = sorted(set(got.get("warnings", [])) - set(want.get("warnings", [])))
        out.extend(f"warning gone: {w}" for w in lost)
        out.extend(f"new warning: {w}" for w in new)
    return out


def _names(keys: list[str], limit: int = 5) -> str:
    shown = ", ".join(Path(k).name for k in keys[:limit])
    return shown + (f" and {len(keys) - limit} more" if len(keys) > limit else "")


def _summary_lines(results: list[dict], wall: float) -> list[str]:
    lines = []
    slow = sorted(results, key=lambda r: r["seconds"], reverse=True)[:5]
    if slow:
        lines.append("slowest: " + ", ".join(f"{Path(r['key']).name} {r['seconds']:.1f}s" for r in slow))
    peaks = [r for r in results if r.get("peak_mib")]
    if peaks:
        top = max(peaks, key=lambda r: r["peak_mib"])
        lines.append(f"largest peak working set: {Path(top['key']).name} {top['peak_mib']:.0f} MiB")
    total = sum(r["seconds"] for r in results)
    lines.append(f"{len(results)} files, {total:.1f}s of work in {wall:.1f}s")
    return lines


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    mode = parser.add_mutually_exclusive_group(required=True)
    mode.add_argument("--update", action="store_true", help="render with Python and write the corpus")
    mode.add_argument("--verify", action="store_true", help="render and compare with the corpus")
    mode.add_argument("--coverage", action="store_true", help="report the CLIP features the corpus exercises")
    parser.add_argument("--render", choices=RENDERERS, default="rust",
                        help="renderer for --verify: tmi's (rust) or the reference tmiref (python)")
    parser.add_argument("--quick", action="store_true", help="leave out the comic pages")
    parser.add_argument("--only", action="append", default=[], metavar="TEXT",
                        help="only files whose key contains TEXT (repeatable)")
    parser.add_argument("--examples", type=Path, default=ROOT / "example", help="the example directory")
    parser.add_argument("--prune", action="store_true", help="--update: drop records of files not found")
    parser.add_argument("-j", "--jobs", type=int, default=max(2, min(8, (os.cpu_count() or 4) // 2)))
    args = parser.parse_args(argv)

    examples = args.examples.resolve()
    corpus = load_corpus()
    present = discover(examples)

    def selected(key: str, group: str) -> bool:
        if args.quick and group == "comic":
            return False
        return not args.only or any(text in key for text in args.only)

    present = {k: v for k, v in present.items() if selected(k, v[1])}
    recorded = {k: v for k, v in corpus["files"].items() if selected(k, v.get("group", ""))}
    missing = sorted(recorded.keys() - present.keys())

    if args.coverage:
        return report_coverage(present, recorded, missing, args.jobs)

    renderer = "python" if args.update else args.render
    why_not = check_renderer(renderer)
    if why_not:
        print(f"--render {renderer}: {why_not}", file=sys.stderr)
        return 2
    if args.verify:
        unlisted = sorted(present.keys() - recorded.keys())
        jobs = [(k, str(p), g, renderer) for k, (p, g) in sorted(present.items()) if k in recorded]
    else:
        unlisted = []
        jobs = [(k, str(p), g, renderer) for k, (p, g) in sorted(present.items())]
    if not jobs:
        print("no corpus files found" + (f" under {examples}" if not present else ""))
        for key in missing:
            print(f"  missing: {key}")
        return 0 if args.verify else 1

    start = time.perf_counter()
    results = run_jobs(jobs, args.jobs, {})
    wall = time.perf_counter() - start
    problems = [(r["key"], p) for r in results for p in r["problems"]]

    if args.update:
        files = corpus["files"]
        if args.prune:
            for key in missing:
                files.pop(key, None)
        for result in results:
            files[result["key"]] = result["record"]
        corpus["version"] = 1
        write_corpus(corpus)
        n_records = sum(count_records(r["record"]) for r in results)
        for key, problem in problems:
            print(f"  CHECK {key}: {problem}")
        for line in _summary_lines(results, wall):
            print(line)
        if missing:
            verb = "dropped" if args.prune else "kept the records of"
            print(f"{verb} {len(missing)} file(s) not found: {_names(missing)}")
        print(f"wrote {CORPUS_PATH.relative_to(ROOT).as_posix()}: {len(results)} files, {n_records} records rendered")
        return 1 if problems else 0

    failures: list[tuple[str, str]] = []
    changed_files: list[str] = []
    n_records = 0
    for result in sorted(results, key=lambda r: r["key"]):
        key, got = result["key"], result["record"]
        want = recorded[key]
        if want.get("file_sha256") != got["file_sha256"]:
            changed_files.append(key)
            continue
        n_records += count_records(want)
        failures.extend((key, line) for line in compare(key, want, got))
    for key, problem in problems:
        failures.append((key, problem))
    for key, line in failures:
        print(f"  MISMATCH {key}: {line}")
    for line in _summary_lines(results, wall):
        print(line)
    if changed_files:
        print(f"skipped {len(changed_files)} file(s) that differ from the corpus's: {_names(changed_files, 20)}")
    if missing:
        print(f"skipped {len(missing)} missing file(s): {_names(missing)}")
    if unlisted:
        print(f"{len(unlisted)} file(s) not in the corpus (run --update): {_names(unlisted)}")
    checked = len(results) - len(changed_files)
    if failures:
        print(f"render corpus MISMATCH: {len(failures)} difference(s) in {len({k for k, _ in failures})} file(s) "
              f"({checked} files checked, --render {renderer})")
        return 1
    print(f"render corpus OK ({checked} files, {n_records} records, --render {renderer}"
          f"{f', {len(missing)} missing' if missing else ''})")
    return 0


# ───────────────────────────── coverage ─────────────────────────────


def _features_of(job: tuple[str, str, str]) -> tuple[str, list[str]]:
    from tmi.formats import clip as C

    key, path_text, _group = job[:3]
    if path_text.lower().endswith(".mdp"):
        from tmiref import mdp2

        return key, mdp_features(mdp2.open_mdp2(path_text))
    with contextlib.redirect_stderr(io.StringIO()):
        clip = C.open_clip(path_text)
        try:
            return key, features(clip)
        finally:
            clip.db.close()


def _known_features() -> list[str]:
    sys.path.insert(0, str(ROOT))
    from tmiref import clip as C

    names = [
        "raster", "folder", "pass-through", "clipping", "mask", "opacity", "hidden", "paper",
        "fill-solid", "gradient", "tone", "edge", "material", "vector", "comic-frame", "layer-colour",
        "expression-colour:mono", "expression-colour:grey",
    ]
    names += [f"blend:{mode}" for mode in sorted(set(C._COMPOSITE_MAP.values())) if mode != C.BLEND_PASS_THROUGH]
    names += [f"adjustment:{fn.__name__.removeprefix('_filter_')}" for fn in C._FILTER_BUILDERS.values()]
    return names


def report_coverage(present: dict, recorded: dict, missing: list[str], workers: int) -> int:
    jobs = [(k, str(p), g) for k, (p, g) in sorted(present.items())]
    by_file: dict[str, list[str]] = {}
    if jobs:
        ctx = mp.get_context("spawn")
        with ctx.Pool(min(workers, len(jobs)), initializer=_worker_init, initargs=({},)) as pool:
            by_file.update(pool.imap_unordered(_features_of, jobs))
    for key in missing:
        by_file[key] = recorded[key].get("features", [])
    counts: dict[str, list[str]] = {}
    for key, names in by_file.items():
        for name in names:
            counts.setdefault(name, []).append(key)
    known = _known_features()
    width = max(len(n) for n in [*known, *counts])
    print(f"{'feature':{width}}  files  (examples)")
    for name in sorted(counts, key=lambda n: (n not in known, -len(counts[n]), n)):
        keys = sorted(counts[name])
        sample = ", ".join(Path(k).name for k in keys[:3]) + (" ..." if len(keys) > 3 else "")
        print(f"{name:{width}}  {len(keys):5}  {sample}")
    uncovered = [n for n in known if n not in counts]
    print(f"\n{len(by_file)} files ({len(missing)} from recorded features of missing files), "
          f"{sum(count_records(r) for r in recorded.values())} records in the corpus")
    print("not covered: " + (", ".join(uncovered) if uncovered else "nothing"))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
