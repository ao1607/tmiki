"""Render a CLIP file under two configurations and show where they part.

    python tools/render_diff.py example/tama.clip                      # tmi (Rust) vs the reference
    python tools/render_diff.py example/tama.clip --region 0 0 512 512
    python tools/render_diff.py page.clip --folder clip:377            # one folder (render_folder)
    python tools/render_diff.py page.clip --a python --b python,TMI_X=1
    python tools/render_diff.py --traces a.jsonl b.jsonl               # compare two existing traces

A side is a renderer, ``rust`` (tmi's own: ``tmi.formats.clip``, tmi_accel)
or ``python`` (the reference package ``tmiref.clip``), optionally followed by
``,KEY=VALUE[,KEY=VALUE...]`` environment settings. Each side renders in its
own process with ``TMI_RENDER_TRACE`` set (see tmi/formats/render_trace.py).
The traces are walked in step; the first record whose phase, band, node,
window, bounds or hash differ is printed with its neighbours, then the extent
of the pixels that differ in the two final images (bounding box, count,
largest channel difference). Exit status: 0 when both agree, 1 when they
differ.
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
RENDERERS = ("rust", "python")
_KEY_FIELDS = ("phase", "band", "main_id", "window")


def parse_config(text: str) -> tuple[str, dict[str, str]]:
    """``(renderer, environment)`` of a side: ``rust`` or ``python``, then
    optional ``KEY=VALUE`` items, comma separated."""
    parts = [part.strip() for part in text.split(",")]
    renderer, items = parts[0], [p for p in parts[1:] if p]
    if renderer not in RENDERERS:
        raise argparse.ArgumentTypeError(f"a side starts with {' or '.join(RENDERERS)}, not {text!r}")
    env: dict[str, str] = {}
    for item in items:
        key, sep, value = item.partition("=")
        if not sep or not key:
            raise argparse.ArgumentTypeError(f"not a KEY=VALUE item: {item!r} in {text!r}")
        env[key] = value
    return renderer, env


# ───────────────────────────── child: one render ─────────────────────────────


def child(path: Path, out: Path, region: list[int] | None, folder: str | None, renderer: str) -> int:
    sys.path.insert(0, str(ROOT))
    from tmi.formats import clip as clip_mod

    if renderer == "python":
        from tmiref import clip as renderer_mod
    else:
        renderer_mod = clip_mod
    clip = clip_mod.open_clip(path)
    if folder is not None:
        node = clip_mod.find_layer_node(clip, folder)
        if node is None or not node.folder:
            raise SystemExit(f"not a folder: {folder}")
        image = renderer_mod.render_folder(clip, node.main_id)
    else:
        image = renderer_mod.render_layered_composite_rgba(clip, region=tuple(region) if region else None)
    rgba = image if image.mode == "RGBA" else image.convert("RGBA")
    out.write_bytes(rgba.tobytes())
    out.with_suffix(".json").write_text(json.dumps({"size": list(rgba.size)}), encoding="utf-8")
    return 0


def render_side(name: str, side: tuple[str, dict[str, str]], args, workdir: Path) -> tuple[Path, Path]:
    renderer, env_extra = side
    trace = workdir / f"{name}.jsonl"
    image = workdir / f"{name}.rgba"
    trace.unlink(missing_ok=True)
    env = os.environ.copy()
    env.pop("TMI_RENDER_TRACE", None)
    env.update(env_extra)
    env["TMI_RENDER_TRACE"] = str(trace)
    cmd = [sys.executable, str(Path(__file__).resolve()), "--child", str(image), "--renderer", renderer, str(args.file)]
    if args.region:
        cmd += ["--region", *map(str, args.region)]
    if args.folder:
        cmd += ["--folder", args.folder]
    label = ",".join([renderer] + [f"{k}={v}" for k, v in env_extra.items()])
    print(f"[{name}] {label}", flush=True)
    proc = subprocess.run(cmd, cwd=ROOT, env=env, capture_output=True, text=True)
    for line in proc.stderr.splitlines():
        print(f"  [{name}] {line}")
    if proc.returncode != 0:
        raise SystemExit(f"[{name}] render failed (exit {proc.returncode})")
    if not trace.exists():
        trace.write_text("", encoding="utf-8")
    return trace, image


# ───────────────────────────── comparing ─────────────────────────────


def load_trace(path: Path) -> list[dict]:
    return [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines() if line.strip()]


def _fmt(rec: dict | None) -> str:
    if rec is None:
        return "(no record)"
    sha = rec.get("sha256")
    sha = sha[:16] if isinstance(sha, str) else sha
    return (f"{rec.get('phase'):9} main_id={rec.get('main_id')} band={rec.get('band')} "
            f"window={rec.get('window')} bounds={rec.get('bounds')} sha256={sha}")


def compare_traces(a: list[dict], b: list[dict], names=("a", "b"), context: int = 2) -> int | None:
    """Print the first diverging record; its index, or None when equal."""
    for i in range(max(len(a), len(b))):
        ra = a[i] if i < len(a) else None
        rb = b[i] if i < len(b) else None
        if ra == rb:
            continue
        structural = ra is None or rb is None or any(ra.get(k) != rb.get(k) for k in _KEY_FIELDS)
        kind = "structure" if structural else ("bounds" if ra.get("bounds") != rb.get("bounds") else "pixels")
        print(f"first divergence at record {i} of {len(a)} / {len(b)} ({kind}):")
        for j in range(max(0, i - context), i):
            print(f"   same    {_fmt(a[j])}")
        print(f"   {names[0]:7} {_fmt(ra)}")
        print(f"   {names[1]:7} {_fmt(rb)}")
        later = sum(1 for x, y in zip(a[i + 1:], b[i + 1:]) if x != y)
        print(f"   ({later} later record(s) differ as well)")
        return i
    print(f"traces agree ({len(a)} records)")
    return None


def pixel_diff(path_a: Path, path_b: Path) -> bool:
    """Print the extent of the differing pixels; True when the images agree."""
    import numpy as np

    size_a = json.loads(path_a.with_suffix(".json").read_text(encoding="utf-8"))["size"]
    size_b = json.loads(path_b.with_suffix(".json").read_text(encoding="utf-8"))["size"]
    if size_a != size_b:
        print(f"image sizes differ: {size_a} vs {size_b}")
        return False
    w, h = size_a
    a = np.frombuffer(path_a.read_bytes(), np.uint8).reshape(h, w, 4)
    b = np.frombuffer(path_b.read_bytes(), np.uint8).reshape(h, w, 4)
    delta = np.abs(a.astype(np.int16) - b.astype(np.int16))
    differ = delta.any(axis=2)
    count = int(differ.sum())
    if not count:
        print(f"images agree ({w}x{h})")
        return True
    ys, xs = np.nonzero(differ)
    box = [int(xs.min()), int(ys.min()), int(xs.max()) + 1, int(ys.max()) + 1]
    worst = delta.reshape(-1, 4).max(axis=0)
    print(f"images differ: {count} of {w * h} pixels, extent {box} (image coordinates), "
          f"largest difference R{worst[0]} G{worst[1]} B{worst[2]} A{worst[3]}")
    y, x = int(ys[0]), int(xs[0])
    print(f"   first differing pixel ({x}, {y}): {tuple(a[y, x])} vs {tuple(b[y, x])}")
    return False


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("file", nargs="?", type=Path, help="the .clip file")
    parser.add_argument("--region", nargs=4, type=int, metavar=("X0", "Y0", "X1", "Y1"),
                        help="render only this window (canvas pixels)")
    parser.add_argument("--folder", help="render one folder alone (selector, e.g. clip:<main_id>)")
    parser.add_argument("--a", dest="a", default="rust", help="first side: rust|python[,KEY=VALUE...]")
    parser.add_argument("--b", dest="b", default="python", help="second side: rust|python[,KEY=VALUE...]")
    parser.add_argument("--traces", nargs=2, type=Path, metavar=("A", "B"), help="compare two trace files only")
    parser.add_argument("--keep", type=Path, help="keep the traces and images in this directory")
    parser.add_argument("--child", type=Path, help=argparse.SUPPRESS)
    parser.add_argument("--renderer", choices=RENDERERS, default="rust", help=argparse.SUPPRESS)
    args = parser.parse_args(argv)

    if args.child is not None:
        return child(args.file, args.child, args.region, args.folder, args.renderer)
    if args.traces:
        return 0 if compare_traces(load_trace(args.traces[0]), load_trace(args.traces[1]),
                                   names=("A", "B")) is None else 1
    if args.file is None:
        parser.error("a .clip file (or --traces A B) is required")
    if not args.file.is_file():
        parser.error(f"no such file: {args.file}")
    try:
        env_a, env_b = parse_config(args.a), parse_config(args.b)
    except argparse.ArgumentTypeError as exc:
        parser.error(str(exc))
    if args.region and args.folder:
        parser.error("--region and --folder do not combine")

    if args.keep:
        args.keep.mkdir(parents=True, exist_ok=True)
        workdir = args.keep
    else:
        workdir = Path(tempfile.mkdtemp(prefix="tmi-render-diff-"))
    try:
        trace_a, image_a = render_side("a", env_a, args, workdir)
        trace_b, image_b = render_side("b", env_b, args, workdir)
        first = compare_traces(load_trace(trace_a), load_trace(trace_b))
        same_pixels = pixel_diff(image_a, image_b)
    finally:
        if not args.keep:
            shutil.rmtree(workdir, ignore_errors=True)
    if args.keep:
        print(f"kept traces and images in {workdir}")
    return 0 if first is None and same_pixels else 1


if __name__ == "__main__":
    raise SystemExit(main())
