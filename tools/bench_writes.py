"""Time add / commit -a / status: `python -m tmi` against the Rust `tmi` (RUST_MIGRATION.md 3b-2).

    python tools/bench_writes.py [--work DIR] [-n 3] [--files tama.clip ...]

For each file, template repositories are built once (in --work) with
`python -m tmi`; every timed run works on a fresh copy (the copy is not
timed):

- `add <file>` in a new repository;
- `commit -a` after a pixel edit made with tmi's own write-back (CLIP: the
  second edit, so the composite is the incremental one on the first edit's
  layered render; MDP2: one layer's tiles; PNG: a rectangle);
- `status` (long format) with that edit in the working tree.

Prints the minimum (median) wall time of n runs as a Markdown table. Build
the executable first: `cargo build --release -p tmi-cli` in rust/.
"""
from __future__ import annotations

import argparse
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
sys.path.insert(0, str(ROOT / "tools"))
EXE = ".exe" if os.name == "nt" else ""
DEFAULT_FILES = [
    "tama.clip",
    "kakenhi-kaguya.mdp",
    "additional_examples/extracted/lolita-ja_EX/lolita-ja_EX/page0001.clip",
    "tama.png",
]


def _make_png(path: Path) -> None:
    """tama.clip's CanvasPreview as a PNG (tmi's encoder)."""
    from tmi import raster
    from tmi.formats import clip as C

    clip = C.open_clip(ROOT / "example" / "tama.clip")
    image = C.canvas_preview(clip)
    clip.db.close()
    path.write_bytes(raster.png(image))


def _edit(path: Path, k: int) -> None:
    """The k-th pixel edit of a working file."""
    from tmi import raster

    suffix = path.suffix.lower()
    colour = [(200, 30, 30, 255), (30, 30, 200, 255)][k % 2]
    if suffix == ".clip":
        from conformance import _clip_leaves, _edit_clip_layer

        leaves = _clip_leaves(path)
        _edit_clip_layer(path, leaves[k % len(leaves)], colour, (40 + 300 * k, 40, 140 + 300 * k, 120))
    elif suffix == ".mdp":
        from tmi.formats import mdp2

        mdp = mdp2.parse_mdp2(path.read_bytes())
        layer = next(l for l in mdp.layers if l.renderable and l.type == "32bpp")
        image = mdp2.render_layer_rgba(mdp, layer, apply_opacity=False)
        data = bytearray(image.tobytes())
        for y in range(10, min(image.height, 90)):
            for x in range(10, min(image.width, 110)):
                i = (y * image.width + x) * 4
                data[i:i + 4] = bytes(colour)
        png = raster.png(raster.Rgba(image.width, image.height, bytes(data)))
        path.write_bytes(mdp2.replace_layer_pixels(path, f"mdp2:{layer.layer_id}", png))
    else:
        image = raster.decode_rgba(path.read_bytes())
        data = bytearray(image.tobytes())
        for y in range(100, 400):
            for x in range(100 + 50 * k, 500 + 50 * k):
                i = (y * image.width + x) * 4
                data[i:i + 4] = bytes(colour)
        path.write_bytes(raster.png(raster.Rgba(image.width, image.height, bytes(data))))


def _tmi(repo: Path, env: dict[str, str], *args: str) -> None:
    subprocess.run([sys.executable, "-m", "tmi", *args], cwd=repo, env=env, check=True, capture_output=True)


def build_templates(work: Path, src: Path, env: dict[str, str]) -> dict[str, Path]:
    name = src.name
    base = work / src.name.replace(".", "_")
    if base.exists():
        return {k: base / k for k in ("add", "commit", "status")}
    add = base / "add"
    add.mkdir(parents=True)
    _tmi(add, env, "init")
    shutil.copyfile(src, add / name)
    commit = base / "commit"
    shutil.copytree(add, commit)
    _tmi(commit, env, "add", name)
    _tmi(commit, env, "commit", "-m", "v1")
    if src.suffix.lower() == ".clip":
        _edit(commit / name, 0)  # first edit: a whole layered render, tagged
        _tmi(commit, env, "commit", "-a", "-m", "v2")
        _edit(commit / name, 1)
    else:
        _edit(commit / name, 0)
    status = base / "status"
    shutil.copytree(commit, status)
    time.sleep(1)
    return {"add": add, "commit": commit, "status": status}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--work", type=Path, help="where the templates are built (kept; reused)")
    parser.add_argument("-n", type=int, default=3, help="runs per command and side")
    parser.add_argument("--bin", type=Path, default=ROOT / "rust" / "target" / "release" / f"tmi{EXE}")
    parser.add_argument("--files", nargs="*", default=DEFAULT_FILES)
    parser.add_argument("--rust-only", action="store_true", help="time only the executable (tools/bench_plugins.py)")
    args = parser.parse_args()
    env = dict(os.environ, TMI_AUTHOR_NAME="T", TMI_AUTHOR_EMAIL="t@example.com", TMI_PYTHON=sys.executable,
               PYTHONPATH=str(ROOT) + os.pathsep + os.environ.get("PYTHONPATH", ""))
    env.pop("TMI_PROGRESS", None)
    work = args.work or Path(tempfile.mkdtemp(prefix="tmi-bench-writes-"))
    work.mkdir(parents=True, exist_ok=True)
    print("| | `python -m tmi` | Rust `tmi` |")
    print("|---|---:|---:|")
    for rel in args.files:
        src = ROOT / "example" / rel
        if rel == "tama.png":
            src = work / "tama.png"
            if not src.exists():
                _make_png(src)
        if not src.exists():
            print(f"| {rel} | (missing) | |")
            continue
        templates = build_templates(work, src, env)
        cases = [
            (f"`add {src.name}`", "add", ["add", src.name]),
            (f"`commit -a` ({src.name})", "commit", ["commit", "-a", "-m", "edit"]),
            (f"`status` ({src.name} modified)", "status", ["status"]),
        ]
        for label, template, cmd in cases:
            row = []
            sides = ([str(args.bin)],) if args.rust_only else ([sys.executable, "-m", "tmi"], [str(args.bin)])
            for side in sides:
                times = []
                for _ in range(args.n):
                    run_dir = work / "run"
                    if run_dir.exists():
                        shutil.rmtree(run_dir)
                    shutil.copytree(templates[template], run_dir)
                    start = time.perf_counter()
                    result = subprocess.run([*side, *cmd], cwd=run_dir, env=env, capture_output=True)
                    times.append(time.perf_counter() - start)
                    if result.returncode:
                        raise SystemExit(f"{side} {cmd}: {result.stderr.decode(errors='replace')}")
                row.append((min(times), statistics.median(times)))
            if args.rust_only:
                row.insert(0, (float("nan"), float("nan")))
            print(f"| {label} | {row[0][0]:.2f}s ({row[0][1]:.2f}) | {row[1][0]:.3f}s ({row[1][1]:.3f}) |", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
