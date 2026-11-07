"""Compare tmi's CLIP render with CLIP STUDIO PAINT's full-size PNG export.

CSP saves no full-size composite in a .clip, only the downscaled
CanvasPreview (built from per-layer mipmaps, so it cannot match exactly).
This tool compares against PNGs exported from CSP itself (File → 画像を統合して
書き出し → .png at 100 %), kept in ``example/additional_examples/csp_export/``
with the clip's stem as file name (settings: research/fidelity/README.md).

    python tools/fidelity.py                       # every exported PNG
    python tools/fidelity.py ricardo_ja tama       # some of them
    python tools/fidelity.py tama --loo            # + leave-one-out per layer
    python tools/fidelity.py tama --renderer python  # tmiref instead of tmi

Both images are flattened onto white before comparing (the export keeps
alpha). The error of a pixel is the largest of its three channel
differences. Draft layers (DraftLayer = 1) are hidden before rendering, since
the export leaves them out (下描き unchecked).

Writes research/fidelity/<renderer>/summary.{json,md} and, per file, a
heatmap (``<stem>_heat.png``, downscaled with max pooling to ≤ 1024 px, not
committed) and the leave-one-out table.
"""

from __future__ import annotations

import argparse
import contextlib
import io
import json
import sqlite3
import struct
import sys
import time
from pathlib import Path

import numpy as np
from PIL import Image

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

EXPORT_DIR = ROOT / "example" / "additional_examples" / "csp_export"
OUT_DIR = ROOT / "research" / "fidelity"


# ───────────────────────────── inputs ─────────────────────────────


def find_clip(stem: str) -> Path:
    hits = sorted(p for p in (ROOT / "example").rglob(f"{stem}.clip") if "csp_export" not in p.parts)
    if not hits:
        raise SystemExit(f"no {stem}.clip under example/")
    return hits[0]


def load_export(stem: str) -> np.ndarray:
    im = Image.open(EXPORT_DIR / f"{stem}.png")
    return np.asarray(im.convert("RGBA"))


def flatten_white(rgba: np.ndarray) -> np.ndarray:
    """RGB of ``rgba`` (straight alpha) over white, rounded to u8."""
    a = rgba[..., 3:4].astype(np.float64) / 255.0
    rgb = rgba[..., :3].astype(np.float64)
    return np.rint(rgb * a + 255.0 * (1.0 - a)).astype(np.int16)


# ─────────────────────── editing the embedded database ───────────────────────


def edit_db(raw: bytes, edit) -> bytes:
    """``raw`` with ``edit(con)`` applied to its embedded SQLite (``edit``
    returns False to leave the file as it is). The SQLi chunk must be the
    last data chunk, as in every CSP file: the Exta offsets stay valid."""
    from tmi.formats import clip as C

    container = C.parse_container(raw)
    sqli = [c for c in container.chunks if c.cid == b"SQLi"]
    assert len(sqli) == 1
    sqli = sqli[0]
    after = [c for c in container.chunks if c.file_off > sqli.file_off]
    assert all(c.cid == b"Foot" for c in after), "SQLi is not the last data chunk"
    con = sqlite3.connect(":memory:")
    con.deserialize(raw[sqli.payload_off:sqli.payload_off + sqli.payload_size])
    if edit(con) is False:
        con.close()
        return raw
    con.commit()
    payload = con.serialize()
    con.close()
    head = raw[:sqli.file_off] + b"CHNK" + b"SQLi" + struct.pack(">Q", len(payload)) + payload
    tail = raw[sqli.payload_off + sqli.payload_size:]
    out = bytearray(head + tail)
    out[8:16] = struct.pack(">Q", len(out))
    return bytes(out)


def with_layers_hidden(raw: bytes, main_ids: set[int], *, hide_drafts: bool) -> tuple[bytes, list[int]]:
    """``raw`` with the eye bit of ``main_ids`` (and of draft layers) cleared
    in the embedded SQLite. Returns the new bytes and the drafts it hid."""
    drafts: list[int] = []

    def edit(con) -> bool:
        if hide_drafts:
            cols = {r[1] for r in con.execute("PRAGMA table_info(Layer)")}
            if "DraftLayer" in cols:
                drafts.extend(r[0] for r in con.execute(
                    "SELECT MainId FROM Layer WHERE DraftLayer = 1 AND (LayerVisibility & 1) = 1"))
        hide = set(main_ids) | set(drafts)
        if not hide:
            return False
        con.executemany("UPDATE Layer SET LayerVisibility = LayerVisibility & ~1 WHERE MainId = ?",
                        [(i,) for i in sorted(hide)])
        return True

    return edit_db(raw, edit), drafts


def isolated(raw: bytes, keep: int, *, plain: bool = True) -> bytes:
    """``raw`` with every layer hidden except ``keep`` (with its folders and,
    for a folder, its contents); with ``plain`` the kept node is drawn in
    normal mode at full opacity. Export the result from CSP to get that
    layer's own ground truth (``make-isolated`` below)."""
    from tmi.formats import clip as C

    with contextlib.redirect_stderr(io.StringIO()):
        clip = C.open_clip_bytes(raw)
    parents: dict[int, int] = {}
    nodes = {}

    def walk(children, parent):
        for n in children:
            nodes[n.main_id] = n
            parents[n.main_id] = parent
            if n.folder:
                walk(n.children, n.main_id)

    walk(clip.root.children, None)
    clip.db.close()
    show = {keep}
    p = parents.get(keep)
    while p is not None:
        show.add(p)
        p = parents.get(p)
    stack = [nodes[keep]]
    while stack:
        n = stack.pop()
        show.add(n.main_id)
        stack.extend(n.children if n.folder else [])

    def edit(con) -> bool:
        con.executemany("UPDATE Layer SET LayerVisibility = LayerVisibility & ~1 WHERE MainId = ?",
                        [(i,) for i in nodes if i not in show])
        if plain:
            con.execute("UPDATE Layer SET LayerComposite = 0, LayerOpacity = 256 WHERE MainId = ?", (keep,))
        return True

    return edit_db(raw, edit)


# ───────────────────────────── rendering ─────────────────────────────


def render(raw: bytes, renderer: str) -> np.ndarray:
    from tmi.formats import clip as C

    with contextlib.redirect_stderr(io.StringIO()):
        clip = C.open_clip_bytes(raw)
        if renderer == "python":
            from tmiref import clip as R

            img = R.render_layered_composite_rgba(clip, (0, 0, 0, 0))
            arr = np.asarray(img.convert("RGBA"))
        else:
            img = C.render_layered_composite_rgba(clip, (0, 0, 0, 0))
            arr = np.frombuffer(bytes(img.data), np.uint8).reshape(img.height, img.width, 4)
        clip.db.close()
    return arr


# ───────────────────────────── metrics ─────────────────────────────


def error_map(ours: np.ndarray, theirs: np.ndarray) -> np.ndarray:
    return np.abs(flatten_white(ours) - flatten_white(theirs)).max(axis=2).astype(np.uint8)


def metrics(err: np.ndarray, ours: np.ndarray, theirs: np.ndarray) -> dict:
    flat = err.ravel()
    n = flat.size
    hist = np.bincount(flat, minlength=256)
    cum = np.cumsum(hist)
    chan = np.abs(flatten_white(ours) - flatten_white(theirs))
    return {
        "pixels": int(n),
        "mean": float(chan.mean()),          # mean over all RGB channel values
        "mean_px": float(flat.mean()),       # mean of the per-pixel max error
        "exact": float(hist[0] / n),
        "le1": float(cum[1] / n),
        "le2": float(cum[2] / n),
        "p99": int(np.searchsorted(cum, 0.99 * n)),
        "max": int(flat.max()),
        "alpha_mismatch": float(np.mean(ours[..., 3] != theirs[..., 3])),
    }


def write_heatmap(err: np.ndarray, path: Path, limit: int = 1024) -> None:
    h, w = err.shape
    k = max(1, -(-max(h, w) // limit))
    hh, ww = -(-h // k), -(-w // k)
    pad = np.zeros((hh * k, ww * k), np.uint8)
    pad[:h, :w] = err
    pooled = pad.reshape(hh, k, ww, k).max(axis=(1, 3)).astype(np.float64)
    # 0 → black, 1–2 → dark blue, 3–15 → purple → red, ≥ 16 → yellow → white
    t = np.clip(np.log2(pooled + 1) / 8.0, 0, 1)
    rgb = np.stack([np.clip(t * 2.2 - 0.2, 0, 1), np.clip(t * 2.0 - 1.0, 0, 1), np.clip(0.5 - np.abs(t - 0.3) * 1.5, 0, 1) + np.clip(t * 3 - 2.4, 0, 1)], axis=2)
    rgb[pooled == 0] = 0
    path.parent.mkdir(parents=True, exist_ok=True)
    Image.fromarray(np.rint(rgb * 255).astype(np.uint8)).save(path)


# ─────────────────────────── leave one out ───────────────────────────


def loo_nodes(raw: bytes, depth: int) -> list[tuple[int, str, int]]:
    """(main_id, path name, depth) of the visible nodes down to ``depth``."""
    from tmi.formats import clip as C

    with contextlib.redirect_stderr(io.StringIO()):
        clip = C.open_clip_bytes(raw)
    out: list[tuple[int, str, int]] = []

    def walk(nodes, prefix: str, d: int) -> None:
        for node in nodes:
            if not node.visible:
                continue
            name = f"{prefix}{node.name}"
            out.append((node.main_id, name + ("/" if node.folder else ""), d))
            if node.folder and d < depth:
                walk(node.children, name + "/", d + 1)

    walk(clip.root.children, "", 0)
    clip.db.close()
    return out


def leave_one_out(raw: bytes, ours: np.ndarray, err: np.ndarray, renderer: str, depth: int) -> list[dict]:
    """For each node: the pixels its removal changes (its footprint), the
    share of the total error that lies in that footprint, and the mean error
    there. A node whose footprint holds much error is where to look."""
    total = float(err.astype(np.float64).sum()) or 1.0
    rows = []
    for main_id, name, d in loo_nodes(raw, depth):
        start = time.perf_counter()
        hidden, _ = with_layers_hidden(raw, {main_id}, hide_drafts=True)
        without = render(hidden, renderer)
        foot = np.any(flatten_white(without) != flatten_white(ours), axis=2)
        n = int(foot.sum())
        e = err[foot].astype(np.float64)
        rows.append({
            "main_id": main_id, "name": name, "depth": d,
            "footprint": n,
            "err_share": float(e.sum() / total),
            "mean_in_footprint": float(e.mean()) if n else 0.0,
            "gt2_in_footprint": int((e > 2).sum()),
            "seconds": round(time.perf_counter() - start, 1),
        })
        print(f"    {name[:60]:60s} foot={n:>9d} share={rows[-1]['err_share']:.3f} "
              f"mean={rows[-1]['mean_in_footprint']:.2f}", flush=True)
    rows.sort(key=lambda r: -r["err_share"])
    return rows


# ───────────────────────────── main ─────────────────────────────


def run_one(stem: str, args) -> dict:
    clip_path = find_clip(stem)
    theirs = load_export(stem)
    raw = clip_path.read_bytes()
    raw, drafts = with_layers_hidden(raw, set(), hide_drafts=not args.keep_drafts)
    start = time.perf_counter()
    ours = render(raw, args.renderer)
    seconds = time.perf_counter() - start
    if ours.shape != theirs.shape:
        return {"stem": stem, "error": f"size {ours.shape} vs export {theirs.shape}"}
    err = error_map(ours, theirs)
    out = OUT_DIR / args.renderer
    write_heatmap(err, out / f"{stem}_heat.png")
    rec = {"stem": stem, "clip": str(clip_path.relative_to(ROOT)), "size": [ours.shape[1], ours.shape[0]],
           "drafts_hidden": drafts, "render_seconds": round(seconds, 1), **metrics(err, ours, theirs)}
    if args.save_diff:
        np.save(out / f"{stem}_err.npy", err)
    if args.loo:
        rec["loo"] = leave_one_out(raw, ours, err, args.renderer, args.depth)
        with open(out / f"{stem}_loo.md", "w", encoding="utf-8") as f:
            f.write(f"# {stem}: leave-one-out ({args.renderer})\n\n")
            f.write("| node | footprint px | error share | mean err in footprint | px > 2 in footprint |\n|---|---:|---:|---:|---:|\n")
            for r in rec["loo"]:
                f.write(f"| {r['name']} | {r['footprint']} | {r['err_share']:.3f} | {r['mean_in_footprint']:.2f} | {r['gt2_in_footprint']} |\n")
    return rec


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.split("\n\n")[0])
    ap.add_argument("stems", nargs="*", help="export file stems (default: all)")
    ap.add_argument("--renderer", choices=["rust", "python"], default="rust")
    ap.add_argument("--loo", action="store_true", help="leave each layer out once")
    ap.add_argument("--depth", type=int, default=0, help="folder depth for --loo (0 = top level)")
    ap.add_argument("--keep-drafts", action="store_true", help="do not hide draft layers")
    ap.add_argument("--save-diff", action="store_true", help="also save the error map as .npy")
    ap.add_argument("--no-summary", action="store_true", help="do not rewrite summary.{json,md}")
    ap.add_argument("--isolate", type=int, metavar="MAIN_ID",
                    help="write <stem>_iso<MAIN_ID>.clip (only that layer, normal at 100 %%) to "
                         "example/additional_examples/iso/ for exporting from CSP, then exit")
    ap.add_argument("--keep-mode", action="store_true", help="with --isolate: keep the layer's blend mode and opacity")
    args = ap.parse_args()
    if args.isolate is not None:
        for stem in args.stems:
            raw, _ = with_layers_hidden(find_clip(stem).read_bytes(), set(), hide_drafts=True)
            out = ROOT / "example" / "additional_examples" / "iso" / f"{stem}_iso{args.isolate}.clip"
            out.parent.mkdir(parents=True, exist_ok=True)
            out.write_bytes(isolated(raw, args.isolate, plain=not args.keep_mode))
            print(out.relative_to(ROOT))
        return
    stems = args.stems or sorted(p.stem for p in EXPORT_DIR.glob("*.png"))
    out = OUT_DIR / args.renderer
    out.mkdir(parents=True, exist_ok=True)
    summary_path = out / "summary.json"
    summary = json.loads(summary_path.read_text(encoding="utf-8")) if summary_path.exists() else {}
    for stem in stems:
        print(f"{stem} ...", flush=True)
        rec = run_one(stem, args)
        if "error" in rec:
            print(f"  {rec['error']}")
        else:
            print(f"  mean {rec['mean']:.3f}  exact {rec['exact']:.4f}  ±1 {rec['le1']:.4f}  ±2 {rec['le2']:.4f}  "
                  f"p99 {rec['p99']}  max {rec['max']}  ({rec['render_seconds']}s)", flush=True)
        rec.pop("loo", None)
        summary[stem] = rec
    if args.no_summary:
        return
    summary_path.write_text(json.dumps(summary, indent=1, ensure_ascii=False) + "\n", encoding="utf-8")
    with open(out / "summary.md", "w", encoding="utf-8") as f:
        f.write(f"# tmi ({args.renderer}) vs CSP full-size export\n\n")
        f.write("Error of a pixel = largest RGB channel difference after flattening onto white. "
                "mean = mean over all channel values.\n\n")
        f.write("| file | size | mean | exact | ≤1 | ≤2 | p99 | max | drafts hidden |\n|---|---|---:|---:|---:|---:|---:|---:|---:|\n")
        for stem in sorted(summary):
            r = summary[stem]
            if "error" in r:
                f.write(f"| {stem} | {r['error']} |||||||\n")
                continue
            f.write(f"| {stem} | {r['size'][0]}×{r['size'][1]} | {r['mean']:.3f} | {r['exact'] * 100:.2f}% | "
                    f"{r['le1'] * 100:.2f}% | {r['le2'] * 100:.2f}% | {r['p99']} | {r['max']} | {len(r['drafts_hidden'])} |\n")


if __name__ == "__main__":
    main()
