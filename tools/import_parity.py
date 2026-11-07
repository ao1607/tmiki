"""Rust import (tmi-import, RUST_MIGRATION.md 3b-2) against Python's.

    python tools/import_parity.py                     # example/ and .test-images/
    python tools/import_parity.py --quick             # files under 30 MB only
    python tools/import_parity.py FILE...             # just these
    python tools/import_parity.py --conform PATH --json report.json

Every file is imported by ``tmi.importers.import_file`` and by
``tmi-conform import-dump`` twice: as ``add`` does (composite, every layer
blob) and as ``status`` does (no composite, no blobs, the first import's
decodes reused). The layer records, canvas_meta, source_meta and the
sha256 of every blob, the composite PNG and the thumbnail must be equal,
and so must the warnings Python prints. Every file under example/ and
.test-images/ is imported, whatever its extension: a file is an image when
tmi's own decoders read it, else an opaque blob (PLUGINS.md P0e), the same
verdict on both sides, so no raster or opaque file is declined (forwarded to
Python); the report counts what was recorded as an image, per format, and
as an opaque blob ("binary"). Only layered files (CLIP / MDP2) that tmi-import
does not reproduce are declined, and the front door hands those commands to
Python.

Build first: ``cargo build --release -p tmi-conform`` in rust/.
"""
from __future__ import annotations

import argparse
import contextlib
import hashlib
import io
import json
import os
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
EXE = ".exe" if os.name == "nt" else ""
LAYERED = {".clip", ".mdp"}


def _sha(b: bytes) -> str | None:
    return hashlib.sha256(b).hexdigest() if b else None


def _layer(layer) -> dict:
    return {
        "layer_id": layer.layer_id,
        "stable_source_id": layer.stable_source_id,
        "name": layer.name,
        "kind": layer.kind,
        "visible": layer.visible,
        "opacity": layer.opacity,
        "blend_mode": layer.blend_mode,
        "clip_to_below": layer.clip_to_below,
        "offset_x": layer.offset_x,
        "offset_y": layer.offset_y,
        "mask_ref": layer.mask_ref,
        "pixel_bbox": layer.pixel_bbox,
        "pixel_sha256": layer.pixel_sha256,
        "source_meta": layer.source_meta,
        "blob": _sha(layer.blob_bytes) if layer.blob_bytes is not None else None,
        "children": [_layer(c) for c in layer.children],
    }


def _file(imported) -> dict:
    return {
        "source_format": imported.source_format,
        "source_digest": imported.source_digest,
        "canvas_meta": imported.canvas_meta,
        "source_meta": imported.source_meta,
        "composite": _sha(imported.composite_png),
        "thumbnail": _sha(imported.thumbnail_png),
        "layers": [_layer(layer) for layer in imported.layers],
    }


def _priors(layers, out: dict) -> None:
    for layer in layers:
        source = (layer.source_meta or {}).get("pixel_source")
        if layer.pixel_sha256 is not None and source:
            out[layer.stable_source_id] = {
                "pixel_source": source, "pixel_sha256": layer.pixel_sha256, "pixel_bbox": layer.pixel_bbox,
            }
        _priors(layer.children, out)


def _reset_warnings() -> None:
    from tmi.formats import clip as C

    for name in ("_warned_offscreens", "_warned_tile_sizes", "_warned_composites", "_warned_filters"):
        getattr(C, name).clear()


def python_dump(path: Path) -> dict:
    from tmi.importers import import_file

    _reset_warnings()
    err = io.StringIO()
    out: dict = {"file": str(path)}
    with contextlib.redirect_stderr(err):
        try:
            imported = import_file(path)
        except BaseException as exc:  # noqa: BLE001 - a report, not a fallback
            out["error"] = f"{type(exc).__name__}: {exc}"
            return out
    out["add"] = _file(imported)
    out["warnings"] = err.getvalue().splitlines()
    prior: dict = {}
    _priors(imported.layers, prior)
    with contextlib.redirect_stderr(io.StringIO()):
        status = import_file(path, render_composite=False, reuse_decoded=prior or None, layer_blobs=frozenset())
    out["status"] = _file(status)
    return out


def canonical(value) -> str:
    return json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))


def corpus(quick: bool) -> list[Path]:
    files = [p for d in ("example", ".test-images") for p in sorted((ROOT / d).rglob("*")) if p.is_file()]
    if quick:
        files = [p for p in files if p.stat().st_size < 30 << 20]
    return files


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("files", nargs="*", type=Path)
    parser.add_argument("--conform", type=Path, default=ROOT / "rust" / "target" / "release" / f"tmi-conform{EXE}")
    parser.add_argument("--quick", action="store_true", help="skip files of 30 MB or more")
    parser.add_argument("--json", type=Path, help="write the full report here")
    args = parser.parse_args()
    files = [p.resolve() for p in args.files] or corpus(args.quick)
    started = time.perf_counter()
    rust_lines: dict[str, dict] = {}
    batch = 40
    for i in range(0, len(files), batch):
        chunk = [str(p) for p in files[i:i + batch]]
        out = subprocess.run([str(args.conform), "import-dump", *chunk], capture_output=True, check=True)
        for line in out.stdout.decode("utf-8").splitlines():
            row = json.loads(line)
            rust_lines[row["file"]] = row
    rust_time = time.perf_counter() - started
    report: dict = {"files": len(files), "same": 0, "declined": [], "differ": [], "python_errors": [], "recorded": {}}
    for path in files:
        rust = rust_lines[str(path)]
        py = python_dump(path)
        rel = str(path.relative_to(ROOT))
        if "error" in py:
            report["python_errors"].append({"file": rel, "error": py["error"], "rust": rust.get("declined", "imported")})
            continue
        if "declined" in rust:
            report["declined"].append({"file": rel, "why": rust["declined"]})
            continue
        bad = [part for part in ("add", "warnings", "status") if canonical(py.get(part)) != canonical(rust.get(part))]
        if bad:
            report["differ"].append({"file": rel, "parts": bad, "python": {k: py.get(k) for k in bad}, "rust": {k: rust.get(k) for k in bad}})
            print(f"DIFF {rel}: {bad}")
        else:
            report["same"] += 1
            fmt = py["add"]["source_format"]
            report["recorded"][fmt] = report["recorded"].get(fmt, 0) + 1
    report["seconds"] = round(time.perf_counter() - started, 1)
    report["rust_seconds"] = round(rust_time, 1)
    print(f"import parity: {report['files']} files, {report['same']} identical, {len(report['declined'])} declined "
          f"(Python imports them), {len(report['differ'])} differ, {len(report['python_errors'])} Python errors "
          f"({report['seconds']}s)")
    print("  recorded as: " + ", ".join(f"{k} {v}" for k, v in sorted(report["recorded"].items())))
    forwarded = [d for d in report["declined"] if Path(d["file"]).suffix.lower() not in LAYERED]
    if forwarded:
        print(f"FAIL: {len(forwarded)} raster / opaque files declined (forwarded to Python); the verdict is tmi's own now")
    by_reason: dict[str, int] = {}
    for d in report["declined"]:
        key = d["why"].split("(")[0].strip()[:70]
        by_reason[key] = by_reason.get(key, 0) + 1
    for why, n in sorted(by_reason.items(), key=lambda kv: -kv[1]):
        print(f"  declined {n:4d}  {why}")
    if args.json:
        args.json.write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
    return 1 if report["differ"] or forwarded else 0


if __name__ == "__main__":
    raise SystemExit(main())
