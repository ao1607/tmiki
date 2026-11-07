"""Performance benchmark + golden-hash parity harness for tmi.

Usage:
    python tools/bench.py                 # run benchmarks, print a table
    python tools/bench.py --json          # machine-readable benchmark output
    python tools/bench.py --verify        # compare render output against tools/golden_hashes.json
    python tools/bench.py --update-golden # regenerate tools/golden_hashes.json from current code
    python tools/bench.py --memory        # only the peak working set per file (Windows)
    python tools/bench.py --no-memory     # benchmarks without the peak-memory rows

Peak memory: each layered composite and each import with composite runs in
a child process, which reports the peak working set (GetProcessMemoryInfo)
in MiB and, per canvas pixel, how far the peak rose above the working set
the operation started from (B/px).

The golden hashes pin two things per example file:
  - the full layered-composite render (sha256 of the RGBA bytes), and
  - every leaf layer's decoded full-size pixels (sha256 over "WxH:" + RGBA
    bytes of the layer blob expanded by ``image_from_bytes``).

Any optimisation (numpy rewrite, Rust kernel, encoder change) must keep
``--verify`` green: the stored PNG bytes may change, the pixels may not.
"""
from __future__ import annotations

import argparse
import hashlib
import io
import json
import subprocess
import sys
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT))

GOLDEN_PATH = ROOT / "tools" / "golden_hashes.json"

CLIP_FILES = sorted((ROOT / "example").glob("*.clip"))
MDP_FILES = sorted((ROOT / "example").glob("*.mdp"))


def pixel_digest(image) -> str:
    """sha256 over "WxH:" + raw RGBA bytes. Must match ImportedLayer.pixel_sha256."""
    rgba = image if image.mode == "RGBA" else image.convert("RGBA")
    h = hashlib.sha256()
    h.update(f"{rgba.width}x{rgba.height}:".encode("ascii"))
    h.update(rgba.tobytes())
    return h.hexdigest()


def _leaf_pixel_hashes(imported) -> dict[str, str]:
    # Hash the *full-size* decoded layer, independent of how blobs are stored
    # (cropped layer blobs expand back through image_from_bytes), so a storage
    # change can never move the goldens.
    from tmi.image_ops import image_from_bytes
    from tmi.importers import walk_imported_layers

    out: dict[str, str] = {}
    for layer in walk_imported_layers(imported.layers):
        if layer.blob_bytes is not None:
            out[layer.stable_source_id] = pixel_digest(image_from_bytes(layer.blob_bytes))
    return out


def collect_hashes() -> dict:
    """Render everything and return the golden-hash structure."""
    from tmi import importers
    from tmi.formats import clip as clip_mod
    from tmi.formats import mdp2 as mdp2_mod

    files: dict[str, dict] = {}
    for path in CLIP_FILES:
        clip = clip_mod.open_clip(path)
        composite = clip_mod.render_layered_composite_rgba(clip)
        imported = importers.import_file(path, render_composite=False)
        files[path.relative_to(ROOT).as_posix()] = {
            "layered_composite": pixel_digest(composite),
            "leaf_pixels": _leaf_pixel_hashes(imported),
        }
    for path in MDP_FILES:
        mdp = mdp2_mod.open_mdp2(path)
        composite = mdp2_mod.render_composite_rgba(mdp)
        imported = importers.import_file(path, render_composite=False)
        files[path.relative_to(ROOT).as_posix()] = {
            "layered_composite": pixel_digest(composite),
            "leaf_pixels": _leaf_pixel_hashes(imported),
        }
    return {"version": 1, "files": files}


def verify() -> int:
    if not GOLDEN_PATH.exists():
        print(f"missing {GOLDEN_PATH}; run --update-golden first", file=sys.stderr)
        return 2
    golden = json.loads(GOLDEN_PATH.read_text(encoding="utf-8"))
    current = collect_hashes()
    failures: list[str] = []
    for rel, want in golden["files"].items():
        got = current["files"].get(rel)
        if got is None:
            failures.append(f"{rel}: example file missing")
            continue
        if want["layered_composite"] != got["layered_composite"]:
            failures.append(
                f"{rel}: layered composite changed "
                f"{want['layered_composite'][:16]} -> {got['layered_composite'][:16]}"
            )
        for sid, digest in want["leaf_pixels"].items():
            now = got["leaf_pixels"].get(sid)
            if now != digest:
                failures.append(f"{rel}: leaf {sid} pixels changed {digest[:16]} -> {now and now[:16]}")
        for sid in got["leaf_pixels"].keys() - want["leaf_pixels"].keys():
            failures.append(f"{rel}: unexpected new leaf {sid}")
    if failures:
        print("GOLDEN MISMATCH:")
        for line in failures:
            print(f"  {line}")
        return 1
    print(f"golden hashes OK ({len(golden['files'])} files)")
    return 0


def update_golden() -> int:
    GOLDEN_PATH.write_text(
        json.dumps(collect_hashes(), indent=2, sort_keys=True) + "\n", encoding="utf-8"
    )
    print(f"wrote {GOLDEN_PATH}")
    return 0


def _timed(fn) -> float:
    t0 = time.perf_counter()
    fn()
    return time.perf_counter() - t0


# ───────────────────────────── peak memory ─────────────────────────────
#
# The peak working set comes from GetProcessMemoryInfo, which only reports
# a process-lifetime peak; so each measured operation runs in a child
# process (``--peak-child``) that reports its working set just before the
# operation and its peak just after.


def _memory_counters():
    """(working set, peak working set) of this process in bytes, or None
    where GetProcessMemoryInfo is unavailable (not Windows)."""
    if sys.platform != "win32":
        return None
    import ctypes
    from ctypes import wintypes

    class PROCESS_MEMORY_COUNTERS(ctypes.Structure):
        _fields_ = [
            ("cb", wintypes.DWORD),
            ("PageFaultCount", wintypes.DWORD),
            ("PeakWorkingSetSize", ctypes.c_size_t),
            ("WorkingSetSize", ctypes.c_size_t),
            ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
            ("QuotaPagedPoolUsage", ctypes.c_size_t),
            ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
            ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
            ("PagefileUsage", ctypes.c_size_t),
            ("PeakPagefileUsage", ctypes.c_size_t),
        ]

    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    # Both need explicit types: without a restype the pseudo-handle comes back
    # as a C int, and without argtypes a HANDLE is passed as a 32-bit int.
    get_current_process = kernel32.GetCurrentProcess
    get_current_process.argtypes = []
    get_current_process.restype = wintypes.HANDLE
    get_info = kernel32.K32GetProcessMemoryInfo
    get_info.argtypes = [wintypes.HANDLE, ctypes.POINTER(PROCESS_MEMORY_COUNTERS), wintypes.DWORD]
    get_info.restype = wintypes.BOOL
    counters = PROCESS_MEMORY_COUNTERS()
    counters.cb = ctypes.sizeof(counters)
    if not get_info(get_current_process(), ctypes.byref(counters), counters.cb):
        raise ctypes.WinError(ctypes.get_last_error())
    return counters.WorkingSetSize, counters.PeakWorkingSetSize


def _peak_child(kind: str, path: Path) -> int:
    """Run one operation and print {"before", "peak", "pixels"} as JSON."""
    from tmi import importers
    from tmi.formats import clip as clip_mod
    from tmi.formats import mdp2 as mdp2_mod

    is_clip = path.suffix.lower() == ".clip"
    if kind == "composite":
        doc = clip_mod.open_clip(path) if is_clip else mdp2_mod.open_mdp2(path)
        render = clip_mod.render_layered_composite_rgba if is_clip else mdp2_mod.render_composite_rgba
        before = _memory_counters()
        image = render(doc)
        size = image.size
    elif kind == "import":
        before = _memory_counters()
        importers.import_file(path, render_composite=True)
    else:
        raise SystemExit(f"unknown --peak-child kind {kind!r}")
    after = _memory_counters()
    if kind == "import":
        doc = clip_mod.open_clip(path) if is_clip else mdp2_mod.open_mdp2(path)
        size = (doc.canvas.width, doc.canvas.height) if is_clip else (doc.canvas_width, doc.canvas_height)
    print(json.dumps({"before": before[0], "peak": after[1], "pixels": size[0] * size[1]}))
    return 0


def _measure_peak(kind: str, path: Path) -> dict | None:
    proc = subprocess.run(
        [sys.executable, str(Path(__file__).resolve()), "--peak-child", kind, str(path)],
        cwd=ROOT, capture_output=True, text=True,
    )
    if proc.returncode != 0:
        print(f"peak memory of {kind} {path.name} failed:\n{proc.stderr}", file=sys.stderr)
        return None
    return json.loads(proc.stdout.strip().splitlines()[-1])


def memory_results() -> list[tuple[str, float]]:
    """Peak working set of a layered composite and of an import with
    composite, per example file: MiB, and bytes per canvas pixel above the
    working set the operation started from."""
    results: list[tuple[str, float]] = []
    if _memory_counters() is None:
        return results
    for kind, label in (("composite", "layered composite"), ("import", "import composite")):
        for path in CLIP_FILES + MDP_FILES:
            rel = path.relative_to(ROOT).as_posix()
            got = _measure_peak(kind, path)
            if got is None:
                continue
            results.append((f"peak {label:17} (MiB)  {rel}", got["peak"] / 2**20))
            results.append((f"peak {label:17} (B/px) {rel}", (got["peak"] - got["before"]) / max(1, got["pixels"])))
    return results


def _print_results(results: list[tuple[str, float]], as_json: bool) -> None:
    if as_json:
        print(json.dumps({name: round(value, 3) for name, value in results}, indent=2))
        return
    for name, value in results:
        unit = "" if any(u in name for u in ("(ms", "(MiB)", "(B/px)")) else "s"
        print(f"{name:55} {value:8.3f}{unit}")


def run_benchmarks(as_json: bool, memory: bool = True) -> int:
    from PIL import Image

    from tmi import importers
    from tmi.formats import clip as clip_mod
    from tmi.formats import mdp2 as mdp2_mod

    results: list[tuple[str, float]] = []

    for path in CLIP_FILES + MDP_FILES:
        rel = path.relative_to(ROOT).as_posix()
        results.append((
            f"import no-composite  {rel}",
            _timed(lambda: importers.import_file(path, render_composite=False)),
        ))
        results.append((
            f"import composite     {rel}",
            _timed(lambda: importers.import_file(path, render_composite=True)),
        ))

    for path in CLIP_FILES:
        rel = path.relative_to(ROOT).as_posix()
        clip = clip_mod.open_clip(path)
        results.append((
            f"layered composite    {rel}",
            _timed(lambda: clip_mod.render_layered_composite_rgba(clip)),
        ))

    for path in MDP_FILES:
        rel = path.relative_to(ROOT).as_posix()
        mdp = mdp2_mod.open_mdp2(path)
        results.append((
            f"layered composite    {rel}",
            _timed(lambda: mdp2_mod.render_composite_rgba(mdp)),
        ))

    # CLIP write-back tile encode (restore / merge hot path).
    # tmi_accel's (RUST_MIGRATION.md 2c), as clip_write_layer runs it.
    from tmi import accel

    tile = Image.new("RGBA", (clip_mod.TILE_SIZE, clip_mod.TILE_SIZE), (120, 80, 200, 255)).tobytes()
    n = 20
    dt = _timed(lambda: [accel.native()._test.clip_tile_record(0, tile) for _ in range(n)])
    results.append(("build_tile_record (ms/tile)", dt / n * 1000))

    # CLI import overhead over bare interpreter startup.
    bare = _timed(lambda: subprocess.run([sys.executable, "-c", "pass"], check=True))
    cli = _timed(lambda: subprocess.run([sys.executable, "-c", "import tmi.cli"], check=True, cwd=ROOT))
    results.append(("import tmi.cli overhead (ms)", (cli - bare) * 1000))

    if memory:
        results.extend(memory_results())
    _print_results(results, as_json)
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--json", action="store_true", help="machine-readable benchmark output")
    parser.add_argument("--verify", action="store_true", help="verify golden render hashes")
    parser.add_argument("--update-golden", action="store_true", help="regenerate golden hashes")
    parser.add_argument("--memory", action="store_true", help="only the peak-memory measurements")
    parser.add_argument("--no-memory", action="store_true", help="skip the peak-memory measurements")
    parser.add_argument("--peak-child", nargs=2, metavar=("KIND", "PATH"), help=argparse.SUPPRESS)
    args = parser.parse_args()
    if args.peak_child:
        return _peak_child(args.peak_child[0], Path(args.peak_child[1]))
    if args.verify:
        return verify()
    if args.update_golden:
        return update_golden()
    if args.memory:
        results = memory_results()
        if not results:
            print("peak memory is measured on Windows only", file=sys.stderr)
            return 1
        _print_results(results, args.json)
        return 0
    return run_benchmarks(args.json, memory=not args.no_memory)


if __name__ == "__main__":
    raise SystemExit(main())
