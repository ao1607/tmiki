"""The Rust front door against the Python CLI (RUST_MIGRATION.md 3a).

    python tools/conformance.py                      # everything but --large
    python tools/conformance.py --large              # also re-chunk every example file
    python tools/conformance.py --bin PATH --conform PATH --work DIR --keep

1. Scenarios: `python -m tmi` builds repositories (init; add PNG, CLIP, MDP;
   commit; branch; tag; switch; merge, clean and conflicted; restore; stash;
   a detached HEAD; an empty repository), with commit times fixed by
   TMI_COMMIT_TIMESTAMP. The main scenario runs twice in two directories and
   both runs must give the same object ids, refs and index.
2. Read-only commands: each runs with `python -m tmi` and with the Rust
   executable (rust/target/release/tmiki.exe) in every repository; stdout
   bytes, stderr and the exit code must be equal. The executable's
   TMI_DEBUG_FORWARD line tells which ones it served itself.
3. `tmi-conform rehash` on every repository: each object's id, file bytes,
   canonical JSON and (for large blobs) chunking re-done by tmi-store.
4. Floats: repr, json.dumps, round(x, 6) and f"{x:.1f}" of random doubles.
5. --large: `write_large_blob` oids of every file under example/, Python
   (written to a scratch store) against tmi-store.

Build first: `cargo build --release -p tmi-cli -p tmi-conform` in rust/.
"""
from __future__ import annotations

import argparse
import json
import os
import random
import shutil
import struct
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
EXE = ".exe" if os.name == "nt" else ""
FORWARD_MARK = b"tmiki: forwarding to Python: "


def _png(path: Path, rgba: tuple[int, int, int, int], size: tuple[int, int] = (40, 30)) -> None:
    """A small RGBA PNG written by tmi's own encoder (no Pillow)."""
    from tmi import raster

    w, h = size
    pixels = bytes(rgba) * (w * h)
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(raster.png(raster.Rgba(w, h, pixels)))


class Runner:
    def __init__(self, env: dict[str, str]) -> None:
        self.env = env
        self.clock = 0

    def tmi(self, repo: Path, *args: str, check: bool = True, stamp: bool = False) -> subprocess.CompletedProcess:
        env = dict(self.env)
        if stamp:
            self.clock += 1
            env["TMI_COMMIT_TIMESTAMP"] = f"2026-01-{self.clock:02d}T09:00:00+00:00"
        result = subprocess.run([sys.executable, "-m", "tmi", *args], cwd=repo, env=env, capture_output=True)
        if check and result.returncode:
            raise SystemExit(
                f"scenario step failed: tmi {' '.join(args)} ({result.returncode})\n"
                f"{result.stdout.decode(errors='replace')}\n{result.stderr.decode(errors='replace')}"
            )
        return result


def scenario_main(run: Runner, repo: Path) -> None:
    ex = ROOT / "example"
    repo.mkdir(parents=True)
    run.tmi(repo, "init")
    run.tmi(repo, "config", "user.name", "Local Name")
    _png(repo / "a.png", (200, 40, 40, 255))
    _png(repo / "画像" / "空.png", (10, 120, 220, 128), (17, 9))
    (repo / "art").mkdir()
    shutil.copyfile(ex / "new.mdp", repo / "art" / "new.mdp")
    shutil.copyfile(ex / "blend_modes.clip", repo / "art" / "blend.clip")
    shutil.copyfile(ex / "layer_types.clip", repo / "art" / "big.clip")  # 2 MB: a chunked blob
    run.tmi(repo, "add", "a.png", "画像/空.png", "art/new.mdp", "art/blend.clip", "art/big.clip")
    run.tmi(repo, "commit", "-m", "first commit\n\nwith a body line", stamp=True)
    run.tmi(repo, "branch", "feature")
    run.tmi(repo, "switch", "feature")
    shutil.copyfile(ex / "new2.mdp", repo / "art" / "new.mdp")
    run.tmi(repo, "add", "art/new.mdp")
    run.tmi(repo, "commit", "-m", "feature: new2 の絵", stamp=True)
    run.tmi(repo, "tag", "v1")
    run.tmi(repo, "tag", "-a", "v1-ann", "-m", "annotated tag\nsecond line\nthird", stamp=True)
    run.tmi(repo, "switch", "main")
    _png(repo / "a.png", (30, 200, 60, 255))
    run.tmi(repo, "add", "a.png")
    run.tmi(repo, "commit", "-m", "main: recolor a", stamp=True)
    run.tmi(repo, "merge", "feature", "-m", "Merge feature", stamp=True)
    run.tmi(repo, "branch", "機能/テスト", "HEAD~1")
    run.tmi(repo, "restore", "--source", "v1", "a.png")
    run.tmi(repo, "add", "a.png")
    run.tmi(repo, "commit", "-m", "restore a from v1", stamp=True)
    shutil.copyfile(ex / "vector_simple.clip", repo / "art" / "blend.clip")
    run.tmi(repo, "stash", "push", "-m", "wip clip", stamp=True)
    run.tmi(repo, "stash", "pop")
    run.tmi(repo, "add", "art/blend.clip")
    run.tmi(repo, "commit", "-m", "swap clip", stamp=True)
    run.tmi(repo, "tag", "v2")
    _png(repo / "a.png", (1, 2, 3, 255))
    run.tmi(repo, "stash", "push", "-m", "kept stash", stamp=True)
    run.tmi(repo, "remote", "add", "origin", str(repo.parent / "nowhere"))
    # untracked and ignored files, staged and unstaged changes
    (repo / ".tmiignore").write_text("*.tmp\nbuild/*\n!build/keep.txt\nart/*.clip:下書き*\n", encoding="utf-8")
    for rel in ["notes.txt", "B.txt", "a_dir/c.txt", "X.TMP", "build/x.bin", "build/keep.txt", "Z/ü.txt"]:
        (repo / rel).parent.mkdir(parents=True, exist_ok=True)
        (repo / rel).write_text(rel, encoding="utf-8")
    shutil.copyfile(ex / "new2.mdp", repo / "art" / "staged.mdp")
    run.tmi(repo, "add", "art/staged.mdp")


def scenario_conflict(run: Runner, repo: Path) -> None:
    repo.mkdir(parents=True)
    run.tmi(repo, "init")
    _png(repo / "c.png", (100, 100, 100, 255))
    run.tmi(repo, "add", "c.png")
    run.tmi(repo, "commit", "-m", "base", stamp=True)
    run.tmi(repo, "switch", "-c", "other")
    _png(repo / "c.png", (255, 0, 0, 255))
    run.tmi(repo, "commit", "-a", "-m", "red", stamp=True)
    run.tmi(repo, "switch", "main")
    _png(repo / "c.png", (0, 0, 255, 255))
    run.tmi(repo, "commit", "-a", "-m", "blue", stamp=True)
    run.tmi(repo, "merge", "other", check=False, stamp=True)


def scenario_empty(run: Runner, repo: Path) -> None:
    repo.mkdir(parents=True)
    run.tmi(repo, "init")


# ───────────── write commands (RUST_MIGRATION.md 3b-2): add, commit, status ─────────────


def _clip_leaves(path: Path) -> list[int]:
    """Colour raster leaves of a CLIP file, as the tests pick them."""
    from tmi.formats import clip as C

    clip = C.open_clip(path)
    try:
        return [
            n.main_id for n in C.walk_layer_nodes(clip.root.children)
            if not n.folder and (info := C.layer_offscreen_info(clip, n.render_mipmap)) and info[2] in clip.extas
            and C.parse_offscreen_attribute(info[1]).channel_profile == "color-alpha-bgra"
        ]
    finally:
        clip.db.close()


def _edit_clip_layer(path: Path, main_id: int, colour: tuple[int, int, int, int], box: tuple[int, int, int, int]) -> None:
    """Paint ``box`` of layer ``main_id`` with tmi's own write-back (the
    tests' pixel edit, without Pillow)."""
    from tmi import raster
    from tmi.formats import clip as C

    clip = C.open_clip(path)
    node = next(n for n in C.walk_layer_nodes(clip.root.children) if n.main_id == main_id)
    image = C.render_layer_rgba(clip, node).image
    clip.db.close()
    data = bytearray(image.tobytes())
    x0, y0, x1, y1 = box
    for y in range(max(0, y0), min(image.height, y1)):
        for x in range(max(0, x0), min(image.width, x1)):
            i = (y * image.width + x) * 4
            data[i:i + 4] = bytes(colour)
    png = raster.png(raster.Rgba(image.width, image.height, bytes(data)))
    path.write_bytes(C.replace_layer_pixels(path, f"clip:{main_id}", png))


class WriteRunner(Runner):
    """Runs every step with Python (``python -m tmi``), with the Rust
    executable, or alternating between them (``cross-a``: Python first,
    ``cross-b``: Rust first), recording each step's output."""

    def __init__(self, env: dict[str, str], mode: str, binary: Path) -> None:
        super().__init__(env)
        self.mode = mode
        self.binary = binary
        self.step = 0
        self.log: list[tuple[str, int, bytes, bytes]] = []
        self.native: list[str] = []

    def tmi(self, repo: Path, *args: str, check: bool = True, stamp: bool = False) -> subprocess.CompletedProcess:
        env = dict(self.env, TMI_PYTHON=sys.executable, TMI_DEBUG_FORWARD="1")
        if stamp:
            self.clock += 1
            env["TMI_COMMIT_TIMESTAMP"] = f"2026-02-{self.clock:02d}T10:00:00+00:00"
        rust = self.mode == "rust" or (self.mode == "cross-a" and self.step % 2) or (self.mode == "cross-b" and not self.step % 2)
        self.step += 1
        cmd = [str(self.binary), *args] if rust else [sys.executable, "-m", "tmi", *args]
        result = subprocess.run(cmd, cwd=repo, env=env, capture_output=True)
        lines = result.stderr.splitlines(keepends=True)
        if rust and not any(ln.startswith(FORWARD_MARK) for ln in lines):
            self.native.append(" ".join(args[:1]))
        err = b"".join(ln for ln in lines if not ln.startswith(FORWARD_MARK))
        here = str(repo).encode("utf-8")  # each run has its own directory
        self.log.append((" ".join(args), result.returncode, result.stdout.replace(here, b"<repo>"), err.replace(here, b"<repo>")))
        if check and result.returncode:
            raise SystemExit(f"write scenario step failed ({self.mode}): tmi {' '.join(args)}\n{result.stdout!r}\n{err!r}")
        return result


def scenario_writes(run: Runner, repo: Path) -> None:
    """add / commit / commit -a / --amend / status (with layer changes) on a
    PNG, a CLIP (CanvasPreview, then whole and incremental layered renders
    after pixel edits) and MDP2 files."""
    ex = ROOT / "example"
    clip_src = next(p for p in (ex / "csp515" / "v2.clip", ex / "blend_modes.clip", ex / "tama.clip") if p.exists())
    repo.mkdir(parents=True)
    run.tmi(repo, "init")
    _png(repo / "a.png", (200, 40, 40, 255), (64, 48))
    shutil.copyfile(clip_src, repo / "art.clip")
    shutil.copyfile(ex / "new.mdp", repo / "art.mdp")
    (repo / "notes").mkdir()
    _png(repo / "notes" / "n.png", (1, 2, 3, 4), (5, 5))
    run.tmi(repo, "status", "-s")
    run.tmi(repo, "add", "a.png", "art.clip", "art.mdp")
    run.tmi(repo, "status")
    run.tmi(repo, "commit", "-m", "first\n\nbody", stamp=True)
    run.tmi(repo, "add", "art.clip")  # unchanged: nothing staged
    run.tmi(repo, "status")
    leaves = _clip_leaves(repo / "art.clip")
    a, b = leaves[0], leaves[-1]
    _png(repo / "a.png", (10, 200, 10, 255), (64, 48))
    _edit_clip_layer(repo / "art.clip", a, (200, 30, 30, 255), (10, 10, 60, 40))  # CanvasPreview cleared
    shutil.copyfile(ex / "new2.mdp", repo / "art.mdp")
    run.tmi(repo, "status")
    run.tmi(repo, "status", "--porcelain")
    run.tmi(repo, "commit", "-a", "-m", "edit all three", stamp=True)  # a whole layered render
    _edit_clip_layer(repo / "art.clip", b, (30, 30, 200, 255), (300, 200, 340, 260))
    run.tmi(repo, "status")
    run.tmi(repo, "commit", "-a", "-m", "incremental", stamp=True)  # on the tagged layered render
    _edit_clip_layer(repo / "art.clip", a, (90, 90, 90, 255), (0, 0, 8, 8))
    run.tmi(repo, "add", "art.clip")
    run.tmi(repo, "commit", "--amend", "-m", "incremental (amended)", stamp=True)
    run.tmi(repo, "commit", "--amend", stamp=True)
    (repo / "a.png").unlink()
    _png(repo / "sub" / "b.png", (9, 9, 9, 255), (7, 3))
    run.tmi(repo, "add", "-u")
    run.tmi(repo, "status")
    run.tmi(repo, "add", "-A")
    run.tmi(repo, "status", "-sb")
    run.tmi(repo, "commit", "-m", "add all", stamp=True)
    run.tmi(repo, "commit", "-m", "nothing", check=False, stamp=True)
    run.tmi(repo, "commit", "--allow-empty", "-m", "empty", stamp=True)
    run.tmi(repo, "add", "missing.png", check=False)
    run.tmi(repo, "add", check=False)
    shutil.copyfile(ex / "new.mdp", repo / "sub" / "c.mdp")
    run.tmi(repo, "add", "sub")
    run.tmi(repo, "add", "*.png", check=False)
    run.tmi(repo, "commit", "-a", "-m", "sub", stamp=True)
    run.tmi(repo, "status")


def compare_writes(work: Path, binary: Path, env: dict[str, str]) -> dict:
    """The write scenario four ways: Python, Rust, and alternating; every
    step's output Python against Rust, every repository's state."""
    runs: dict[str, WriteRunner] = {}
    states = {}
    for mode in ("python", "rust", "cross-a", "cross-b"):
        repo = work / f"writes-{mode}"
        if repo.exists():
            shutil.rmtree(repo)
        runner = WriteRunner(env, mode, binary)
        scenario_writes(runner, repo)
        runs[mode] = runner
        states[mode] = repo_state(repo)
    diffs = []
    for (label, code, out, err), (_, rcode, rout, rerr) in zip(runs["python"].log, runs["rust"].log):
        if (code, out, err) != (rcode, rout, rerr):
            diffs.append({"step": label, "python": [code, out.decode(errors="replace")[:1500], err.decode(errors="replace")[:600]],
                          "rust": [rcode, rout.decode(errors="replace")[:1500], rerr.decode(errors="replace")[:600]]})
    same_state = {mode: states[mode] == states["python"] for mode in states}
    return {
        "steps": len(runs["python"].log),
        "native_steps": runs["rust"].native,
        "output_diffs": diffs,
        "same_state": same_state,
        "objects": len(states["python"]["objects"]),
    }


def read_only_commands(repo: Path) -> list[list[str]]:
    oid = (repo / ".tmi" / "refs" / "heads" / "main").read_text(encoding="utf-8").strip() if (
        repo / ".tmi" / "refs" / "heads" / "main").exists() else "0" * 64
    cmds = [
        ["log"], ["log", "--oneline"], ["log", "--oneline", "--graph"], ["log", "--graph"],
        ["log", "--decorate"], ["log", "--decorate=full", "--oneline"], ["log", "--decorate", "--no-decorate"],
        ["log", "--all", "--oneline", "--decorate"], ["log", "--stat"], ["log", "--name-status"],
        ["log", "--name-only", "--oneline"], ["log", "--first-parent", "--oneline", "--graph"],
        ["log", "--merges"], ["log", "--no-merges", "--oneline"], ["log", "--merges", "--no-merges"],
        ["log", "-n", "2"], ["log", "-n2", "--skip", "1"], ["log", "--max-count=0"], ["log", "--reverse", "--oneline"],
        ["log", "--parents", "--oneline"], ["log", "--parents", "--abbrev", "6"], ["log", "--abbrev=2", "--oneline"],
        ["log", "--abbrev-commit"], ["log", "--abbrev-commit", "--no-abbrev-commit", "-n", "1"],
        ["log", "--format=%H %h %P %p|%s|%an <%ae> %ad%d%n--%%B--"], ["log", "--pretty=format:%h %D"],
        ["log", "--pretty=oneline"], ["log", "--pretty=short", "--oneline"], ["log", "--format", "%s", "--stat"],
        ["log", "--date", "short"], ["log", "--author", "tester"], ["log", "--author", "nobody"],
        ["log", "feature"], ["log", "HEAD~1", "--oneline"], ["log", "v1"], ["log", "v1-ann", "--oneline"],
        ["log", "機能/テスト", "--oneline"], ["log", oid[:10], "--oneline"], ["log", "nope"],
        ["log", "a.png", "--oneline"], ["log", "--oneline", "HEAD", "art"], ["log", "--oneline", "*.mdp"],
        ["log", "art/new.mdp", "--name-status"], ["log", "--graph", "--all", "--oneline", "--decorate"],
        ["log", "HEAD~99"], ["log", "--grep", "feature"], ["log", "--onel"],
        ["show"], ["show", "HEAD~1"], ["show", "v1"], ["show", "v1-ann"], ["show", "--stat"],
        ["show", "--name-only", "HEAD~2"], ["show", "--abbrev", "7", "feature"], ["show", "--date", "short"],
        ["show", "nope"], ["show", "HEAD:a.png"], ["show", "HEAD~1:art/new.mdp"], ["show", ":a.png"],
        ["show", "v1:art/big.clip"], ["show", "HEAD:missing.png"], ["show", oid],
        ["branch"], ["branch", "-v"], ["branch", "-vv"], ["branch", "-a"], ["branch", "-r"],
        ["branch", "--contains", "v1"], ["branch", "--merged"], ["branch", "--no-merged", "feature"],
        ["branch", "--sort", "-committerdate"], ["branch", "--sort=committerdate", "-v"], ["branch", "--sort", "-refname"],
        ["tag"], ["tag", "-l"], ["tag", "-l", "v1*"], ["tag", "-n"], ["tag", "-n", "2"], ["tag", "-n3", "-l"],
        ["tag", "--points-at", "v1"], ["tag", "--contains", "HEAD~3"], ["tag", "--sort", "-creatordate"],
        ["tag", "-l", "V1*"], ["tag", "--sort=-refname"], ["tag", "-l", "x", "y"],
        ["config", "--list"], ["config", "user.name"], ["config", "--global", "user.name"], ["config", "user.nope"],
        ["config", "core.maxImagePixels"], ["config"], ["config", "-l"],
        ["reflog"], ["recover"], ["recover", "--files"], ["remote"], ["remote", "-v"],
        ["status"], ["status", "-s"], ["status", "-sb"], ["status", "--porcelain"], ["status", "--short", "--branch"],
        ["stash", "list"], ["--version"],
    ]
    return cmds


def compare_commands(repo: Path, binary: Path, env: dict[str, str]) -> dict:
    rust_env = dict(env, TMI_DEBUG_FORWARD="1", TMI_PYTHON=sys.executable)
    py_cmd = [sys.executable, "-m", "tmi"]
    # warm the stat cache the way a user's earlier status would
    subprocess.run(py_cmd + ["status"], cwd=repo, env=env, capture_output=True)
    native, forwarded, failures = [], [], []
    for args in read_only_commands(repo):
        py = subprocess.run(py_cmd + args, cwd=repo, env=env, capture_output=True)
        rs = subprocess.run([str(binary), *args], cwd=repo, env=rust_env, capture_output=True)
        lines = rs.stderr.splitlines(keepends=True)
        marks = [ln for ln in lines if ln.startswith(FORWARD_MARK)]
        rs_err = b"".join(ln for ln in lines if not ln.startswith(FORWARD_MARK))
        label = " ".join(args)
        (forwarded if marks else native).append(label)
        if (py.returncode, py.stdout, py.stderr) != (rs.returncode, rs.stdout, rs_err):
            failures.append({
                "command": label,
                "python": [py.returncode, py.stdout.decode(errors="replace")[:2000], py.stderr.decode(errors="replace")[:500]],
                "rust": [rs.returncode, rs.stdout.decode(errors="replace")[:2000], rs_err.decode(errors="replace")[:500]],
            })
    return {"native": native, "forwarded": forwarded, "failures": failures}


# ---------------------------------------------------------------------------
# Write commands (RUST_MIGRATION.md 3b-1): the same steps run on copies of
# one base repository, in the same directory, all through Python
# (TMI_NATIVE=0), all through the executable, and alternating between the
# two (a write by one side continued by the other). After every step the
# outputs and the whole repository state must equal the all-Python run's.
# ---------------------------------------------------------------------------

_HOOK_PY = r'''import os, sys
from pathlib import Path
keys = ["TMI_HOOK", "TMI_BRANCH_FROM", "TMI_BRANCH_TO", "TMI_COMMIT", "TMI_COMMIT_MSG", "TMI_REPO"]
line = " ".join(f"{k}={os.environ.get(k, '-')}" for k in keys)
repo = Path(os.environ["TMI_REPO"])
with open(repo / ".tmi" / "hooks.log", "a", encoding="utf-8") as f:
    f.write(line + "\n")
print("hook out:", line)
print("hook err:", os.environ.get("TMI_HOOK"), file=sys.stderr)
sys.exit(3 if "blocked" in (os.environ.get("TMI_BRANCH_TO"), os.environ.get("TMI_COMMIT_MSG")) else 0)
'''


def _write(rel: str, data: bytes | str):
    def step(repo: Path) -> None:
        path = repo / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_bytes(data.encode("utf-8") if isinstance(data, str) else data)
    step.__name__ = f"write {rel}"
    return step


def _tiny_gif() -> bytes:
    """A 1x1 GIF89a (a red pixel): an image to tmi's own decoders."""
    return bytes.fromhex(
        "47494638396101000100800000ff000000000021f90401000000002c00000000010001000002024401003b")


def _tga() -> bytes:
    """A 2x2 uncompressed 24-bit TGA: Pillow opens it, tmi has no TGA decoder."""
    import struct

    header = struct.pack("<BBB5sHHHHBB", 0, 0, 2, bytes(5), 0, 0, 2, 2, 24, 0)
    return header + bytes((10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120))


def _lab_tiff() -> bytes:
    """A 2x2 TIFF in CIELab (PhotometricInterpretation 8): Pillow opens it as
    mode LAB, which it cannot convert to RGBA, so tmi declines it."""
    import struct

    pixels = bytes(range(10, 22))
    entries = [  # tag, type (3 short, 4 long), count, value
        (256, 3, 1, 2), (257, 3, 1, 2), (258, 3, 3, None), (259, 3, 1, 1), (262, 3, 1, 8), (273, 4, 1, None),
        (277, 3, 1, 3), (278, 3, 1, 2), (279, 4, 1, len(pixels)),
    ]
    ifd_at = 8
    ifd_len = 2 + 12 * len(entries) + 4
    bits_at = ifd_at + ifd_len
    data_at = bits_at + 6
    out = bytearray(b"II*\0" + struct.pack("<I", ifd_at) + struct.pack("<H", len(entries)))
    for tag, typ, count, value in entries:
        if tag == 258:
            value = bits_at
        elif tag == 273:
            value = data_at
        out += struct.pack("<HHI", tag, typ, count) + (struct.pack("<HH", value, 0) if typ == 3 and count == 1 else struct.pack("<I", value))
    out += struct.pack("<I", 0) + struct.pack("<3H", 8, 8, 8) + pixels
    return bytes(out)


def _truncated_jpeg() -> bytes:
    """A JPEG cut in the middle of its scan: libjpeg warns, tmi declines."""
    import tempfile

    with tempfile.TemporaryDirectory() as tmp:
        path = Path(tmp) / "j.jpg"
        _jpeg(path, (200, 60, 30, 255), (64, 48))
        data = path.read_bytes()
    return data[: len(data) * 2 // 3]


def _png_step(rel: str, rgba: tuple[int, int, int, int]):
    def step(repo: Path) -> None:
        _png(repo / rel, rgba)
    step.__name__ = f"png {rel}"
    return step


def _remove(rel: str):
    def step(repo: Path) -> None:
        path = repo / rel
        if path.is_dir():
            shutil.rmtree(path)
        else:
            path.unlink()
    step.__name__ = f"remove {rel}"
    return step


def _hooks(repo: Path) -> None:
    hooks = repo / ".tmi" / "hooks"
    hooks.mkdir(parents=True, exist_ok=True)
    for name in ("pre-switch", "post-switch", "post-commit"):
        (hooks / f"{name}.py").write_text(_HOOK_PY, encoding="utf-8")
    (hooks / "pre-commit.cmd" if os.name == "nt" else hooks / "pre-commit").write_text(
        "@echo pre-commit hook ran\r\n@exit /b 0\r\n" if os.name == "nt" else "#!/bin/sh\necho pre-commit hook ran\n",
        encoding="utf-8")
    if os.name != "nt":
        (hooks / "pre-commit").chmod(0o755)


def scenario_write_base(run: Runner, repo: Path) -> None:
    ex = ROOT / "example"
    repo.mkdir(parents=True)
    run.tmi(repo, "init")
    run.tmi(repo, "config", "user.name", "Local Name")
    _png(repo / "a.png", (200, 40, 40, 255))
    _png(repo / "b.png", (40, 40, 200, 255))
    _png(repo / "画像" / "空.png", (10, 120, 220, 128), (17, 9))
    (repo / "art").mkdir()
    shutil.copyfile(ex / "layer_types.clip", repo / "art" / "big.clip")  # 2 MB: chunked
    run.tmi(repo, "add", "a.png", "b.png", "画像/空.png", "art/big.clip")
    run.tmi(repo, "commit", "-m", "first", stamp=True)
    run.tmi(repo, "branch", "feature")
    run.tmi(repo, "switch", "feature")
    _png(repo / "a.png", (30, 200, 60, 255))
    _png(repo / "sub" / "deep" / "c.png", (1, 2, 3, 255))
    run.tmi(repo, "add", "a.png", "sub/deep/c.png")
    run.tmi(repo, "commit", "-m", "feature work", stamp=True)
    run.tmi(repo, "tag", "v1")
    run.tmi(repo, "tag", "-a", "v1-ann", "-m", "annotated\nbody", stamp=True)
    run.tmi(repo, "switch", "main")
    _png(repo / "b.png", (90, 90, 90, 255))
    run.tmi(repo, "add", "b.png")
    run.tmi(repo, "commit", "-m", "main work", stamp=True)


def scenario_write_conflict(run: Runner, repo: Path) -> None:
    scenario_conflict(run, repo)


# ───────────── diff and merge (RUST_MIGRATION.md 4a) ─────────────


def _jpeg(path: Path, rgba: tuple[int, int, int, int], size: tuple[int, int] = (48, 32), quality: int = 90) -> None:
    """A small JPEG written by tmi_accel's encoder (no Pillow)."""
    from tmi import accel

    w, h = size
    data = bytearray(bytes(rgba) * (w * h))
    for y in range(h // 2):  # a second colour, so the image is not flat
        for x in range(w // 3):
            i = (y * w + x) * 4
            data[i:i + 4] = bytes((255 - rgba[0], rgba[1] // 2, 255 - rgba[2], 255))
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(accel.native().writeback_jpeg(bytes(data), w, h, quality=quality)[0])


def _clip_example() -> Path:
    ex = ROOT / "example"
    return next(p for p in (ex / "csp515" / "v2.clip", ex / "blend_modes.clip", ex / "tama.clip") if p.exists())


def _clip_edit_step(rel: str, which: int, colour: tuple[int, int, int, int], box: tuple[int, int, int, int]):
    def step(repo: Path) -> None:
        leaves = _clip_leaves(repo / rel)
        _edit_clip_layer(repo / rel, leaves[which], colour, box)
    step.__name__ = f"edit {rel} layer #{which}"
    return step


def _copy_step(src: Path, rel: str):
    def step(repo: Path) -> None:
        (repo / rel).parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(src, repo / rel)
    step.__name__ = f"copy {src.name} to {rel}"
    return step


def _jpeg_step(rel: str, rgba: tuple[int, int, int, int]):
    def step(repo: Path) -> None:
        _jpeg(repo / rel, rgba)
    step.__name__ = f"jpeg {rel}"
    return step


def _edit_mdp_layer(path: Path, index: int, colour: tuple[int, int, int, int], box: tuple[int, int, int, int]) -> None:
    """Paint ``box`` of MDP2 layer ``index`` with tmi's own write-back."""
    from tmi import raster
    from tmi.formats import mdp2 as M

    data = M.open_mdp2(path)
    layer = data.layers[index]
    image = raster.dense(M.render_layer_rgba(data, layer, apply_opacity=False))
    buf = bytearray(image.tobytes())
    x0, y0, x1, y1 = box
    for y in range(max(0, y0), min(image.height, y1)):
        for x in range(max(0, x0), min(image.width, x1)):
            i = (y * image.width + x) * 4
            buf[i:i + 4] = bytes(colour)
    png = raster.png(raster.Rgba(image.width, image.height, bytes(buf)))
    path.write_bytes(M.replace_layer_pixels(path, f"mdp2:{layer.layer_id}", png))


def _mdp_edit_step(rel: str, index: int, colour: tuple[int, int, int, int], box: tuple[int, int, int, int]):
    def step(repo: Path) -> None:
        _edit_mdp_layer(repo / rel, index, colour, box)
    step.__name__ = f"edit {rel} layer #{index}"
    return step


def scenario_merge_base(run: Runner, repo: Path) -> None:
    """main at "base"; branches ff (a PNG edit on top of base), layers
    (other CLIP and MDP2 layers than main will edit) and clash (the same
    CLIP layer, overlapping; the PNG and the JPEG; non-overlapping edits of
    a layer the layers branch edits too)."""
    ex = ROOT / "example"
    repo.mkdir(parents=True)
    run.tmi(repo, "init")
    _png(repo / "a.png", (200, 40, 40, 255), (64, 48))
    _jpeg(repo / "photo.jpg", (30, 120, 200, 255))
    shutil.copyfile(_clip_example(), repo / "art.clip")
    shutil.copyfile(ex / "new.mdp", repo / "art.mdp")
    run.tmi(repo, "add", "a.png", "photo.jpg", "art.clip", "art.mdp")
    run.tmi(repo, "commit", "-m", "base", stamp=True)
    leaves = _clip_leaves(repo / "art.clip")
    run.tmi(repo, "switch", "-c", "ff")
    _png(repo / "a.png", (10, 200, 10, 255), (64, 48))
    run.tmi(repo, "commit", "-a", "-m", "ff work", stamp=True)
    run.tmi(repo, "switch", "-c", "layers", "main")
    _edit_clip_layer(repo / "art.clip", leaves[-1], (30, 30, 200, 255), (100, 100, 160, 160))
    _edit_mdp_layer(repo / "art.mdp", 1, (0, 0, 255, 255), (30, 30, 60, 60))
    run.tmi(repo, "commit", "-a", "-m", "layers work", stamp=True)
    run.tmi(repo, "switch", "-c", "clash", "main")
    _edit_clip_layer(repo / "art.clip", leaves[0], (30, 200, 30, 255), (20, 20, 50, 50))
    _edit_clip_layer(repo / "art.clip", leaves[-1], (200, 200, 30, 255), (300, 200, 340, 260))
    _edit_mdp_layer(repo / "art.mdp", 1, (0, 255, 0, 255), (70, 70, 100, 100))
    _png(repo / "a.png", (9, 9, 9, 255), (64, 48))
    _jpeg(repo / "photo.jpg", (10, 90, 10, 255))
    run.tmi(repo, "commit", "-a", "-m", "clash work", stamp=True)
    run.tmi(repo, "switch", "main")


def scenario_diff_base(run: Runner, repo: Path) -> None:
    """A PNG, a JPEG, a CLIP and an MDP2 file, on main and (edited) on
    feature."""
    ex = ROOT / "example"
    repo.mkdir(parents=True)
    run.tmi(repo, "init")
    _png(repo / "a.png", (200, 40, 40, 255), (64, 48))
    _jpeg(repo / "photo.jpg", (30, 120, 200, 255))
    shutil.copyfile(_clip_example(), repo / "art.clip")
    (repo / "pic").mkdir()
    shutil.copyfile(ex / "new.mdp", repo / "pic" / "art.mdp")
    run.tmi(repo, "add", "a.png", "photo.jpg", "art.clip", "pic/art.mdp")
    run.tmi(repo, "commit", "-m", "base", stamp=True)
    run.tmi(repo, "switch", "-c", "feature")
    _png(repo / "a.png", (10, 200, 10, 255), (64, 48))
    _jpeg(repo / "photo.jpg", (200, 120, 30, 255))
    leaves = _clip_leaves(repo / "art.clip")
    _edit_clip_layer(repo / "art.clip", leaves[0], (200, 30, 30, 255), (10, 10, 60, 40))
    shutil.copyfile(ex / "new2.mdp", repo / "pic" / "art.mdp")
    run.tmi(repo, "commit", "-a", "-m", "feature edits", stamp=True)
    run.tmi(repo, "switch", "main")


# Each scenario: (name, base builder, steps). A step is a list of tmi
# arguments, ("stamp", [args]) for one that records a time, or a function
# changing the working tree.
WRITE_SCENARIOS = [
    ("refs", scenario_write_base, [
        ["branch", "new1"], ["branch", "new2", "v1"], ["branch"], ["branch", "-d", "new1"], ["branch", "-D", "new2"],
        ["branch", "-d", "feature"], ["branch", "-d"], ["branch", "-d", "x", "y"], ["branch", "機能/テスト", "HEAD~1"],
        ["branch", "dup"], ["branch", "dup"], ["branch", "-d", "main"], ["branch", "-d", "MAIN"], ["branch", "bad:name"],
        ["branch", "x", "nope"], ["branch", "-d", "nothere"], ["branch", "feature/sub"], ["branch", "-v"],
        ["tag", "t1"], ["tag", "t1"], ("stamp", ["tag", "-a", "t2", "-m", "message\nline 2"]),
        ("stamp", ["tag", "-m", "implied", "t3"]), ["tag", "-m", "", "t4"], ["tag", "-a", "t5"], ["tag", "t6", "HEAD~1"],
        ["tag", "-d", "t1"], ["tag", "-d", "nope"], ["tag", "-d"], ["tag", "-d", "t6", "x"], ["tag", "-n", "0", "t7"],
        ["tag", "-n"],
        ["config", "user.email", "e@example.com"], ["config", "--unset", "user.email"], ["config", "--unset", "nope.key"],
        ["config", "--global", "user.name", "Global Two"], ["config", "--global", "--unset", "core.x"],
        ["config", "core.custom", "日本語の値"], ["config", "--list"], ["config", "--global", "--unset", "core.y"],
        ["remote", "add", "origin", "../nowhere"], ["remote", "add", "origin", "x"], ["remote", "-v"],
        ["remote", "set-url", "origin", "https://example.com/api/u/r"], ["remote", "add", "two", "sub/dir"],
        ["remote", "rename", "origin", "up"], ["remote", "rename", "nope", "x"], ["remote", "rename", "up", "two"],
        ["remote", "remove", "up"], ["remote", "rm", "nope"], ["remote", "set-url", "nope", "x"], ["remote"],
        ["recover", "--as", "rescued", "HEAD@{2}"], ["recover", "--as", "r2", "feature"], ["recover", "--as", "r3", "zzzz"],
        ["recover", "--as", "r4"], ["reflog"], ["branch", "-a", "-v"],
    ]),
    ("worktree", scenario_write_base, [
        ["switch", "feature"], ["status"], ["switch", "main"], ["switch", "-c", "topic"], ["switch", "-c", "topic"],
        ["switch", "nope"], ["switch", "main", "extra"], _png_step("a.png", (5, 6, 7, 255)), ["status", "-s"],
        ["switch", "feature"], ["switch", "--discard", "feature"], ["recover", "--files"], ["status"],
        ["checkout", "main"], ["checkout", "-b", "co-new", "v1"], ["checkout", "-b", "co-new"], ["checkout", "v1"],
        ["status", "-sb"], ["checkout", "main"], ["checkout", "HEAD~1", "a.png"], ["status", "--porcelain"],
        ["checkout", "a.png"], ["restore", "--source", "v1", "a.png"], ["restore", "a.png"],
        ["restore", "--source", "v1", "--output", "out/x.png", "a.png"], ["restore", "--source", "HEAD~1", "b.png", "a.png"],
        ["restore", "--source", "nope", "a.png"], ["restore", "missing.png"], ["restore", "--staged", "a.png"],
        ["restore", "--staged", "--source", "v1", "a.png"], ["restore", "--output", "o.png", "a.png", "b.png"],
        _png_step("b.png", (7, 7, 7, 255)), ["add", "b.png"], ["status", "-s"], ["restore", "--staged", "b.png"],
        ["restore", "--staged", "nothing.png"], ["status"], ["checkout", "--", "b.png"], ["checkout", "b.png"],
        ["reset", "--soft", "HEAD~1"], ["status", "-s"], ["reset"], ["status", "-s"], ["reset", "--hard", "v1"],
        ["status"], ["reset", "--hard"], ["reset", "--soft", "--hard"], ["reset", "HEAD~1", "a.png"], ["status", "-s"],
        ["reset", "a.png"], ["reset", "nope"], ["reset", "--hard", "nope.png"], ["reset", "HEAD", "zz.png"],
        ["rm", "b.png"], ["status", "-s"], ["rm", "--cached", "a.png"], ["status", "-s"], ["rm", "nope.png"],
        _png_step("画像/空.png", (9, 9, 9, 9)), ["rm", "画像/空.png"], ["rm", "-f", "画像/空.png"],
        _png_step("new.png", (1, 1, 1, 255)), ["add", "new.png"], ["rm", "new.png"], ["rm", "--cached", "new.png"],
        ["status"], ["reset", "--hard"], ["recover", "--files"], ["recover", "--file", "0"],
        ["recover", "--file", "0", "--output", "rec/out.png"], ["recover", "--file", "99"], ["status", "-s"],
        ["switch", "--discard", "main"], ["checkout", "--discard", "v1"], ["switch", "main"], ["reflog"],
        _write("untracked.txt", "hello"), _remove("sub"), ["status"], ["checkout", "feature"], ["checkout", "main"],
        ["mv", "a.png", "moved.png"], ["status", "-s"], ["mv", "moved.png", "sub2/"], ["mv", "b.png", "sub2"],
        _write("sub3/.keep", "x"), ["mv", "b.png", "sub3"], ["status", "-s"], ["mv", "nope.png", "x.png"],
        ["mv", "art/big.clip", "art/renamed.clip"], ["mv", "art/renamed.clip", "../outside.clip"],
        _png_step("other.png", (1, 2, 3, 4)), ["mv", "moved.png", "other.png"], ["mv", "-f", "moved.png", "other.png"],
        ["mv", "other.png", "OTHER.png"], ["mv", "画像/空.png", "画像/青空.png"], ["status"], ("stamp", ["commit", "-m", "moves"]),
        ["log", "--name-status", "-n", "1"],
    ]),
    ("stash", scenario_write_base, [
        ["stash"], ["stash", "list"], ["stash", "pop"], ["stash", "drop"], _png_step("a.png", (11, 12, 13, 255)),
        ("stamp", ["stash", "push", "-m", "first"]), ["stash", "list"], ["status", "-s"], ["stash", "apply"],
        ["stash", "apply"], ["restore", "a.png"], ["stash", "pop"], ["status", "-s"], ["stash", "list"],
        ["checkout", "--discard", "main"], _png_step("b.png", (1, 2, 3, 255)), ("stamp", ["stash", "push", "-m", "one"]),
        _png_step("a.png", (4, 5, 6, 255)), ("stamp", ["stash", "save", "two"]), ["stash", "list"],
        ["stash", "drop", "stash@{1}"], ["stash", "list"], ["stash", "apply", "9"], ["stash", "pop", "x"],
        ["stash", "pop", "0"], ["stash", "list"], _png_step("b.png", (8, 8, 8, 255)), ("stamp", ["stash", "push"]),
        ["stash", "clear"], ["stash", "list"], ["stash", "bogus"], ["status"],
    ]),
    ("gc", scenario_write_base, [
        ["gc"], ["branch", "-D", "feature"], ["tag", "-d", "v1"], ["tag", "-d", "v1-ann"], ["gc"],
        _write(".tmi/diff/20260101-000000/x.png", b"\x89PNG fake" * 100), _write(".tmi/tmp/stage/y.bin", b"y" * 5000),
        _write(".tmi/cache/render/z", b"z" * 123), _png_step("a.png", (3, 3, 3, 255)), ["reset", "--hard"],
        ["gc", "--prune", "bad"], ["gc", "--prune=1d"], ["recover", "--files"], ["gc", "--prune=now"], ["recover"],
        ["recover", "--files"], ["gc", "--prune", "0d"], ["log", "--oneline", "--all"],
    ]),
    ("hooks", scenario_write_base, [
        _hooks, ["hook"], ["hook", "list"], ["switch", "feature"], ["branch", "blocked"], ["switch", "blocked"],
        ["status", "-s"], ["switch", "-c", "made", "v1"], ["checkout", "-b", "blocked2"], ["checkout", "main"],
        ["hook", "run", "post-switch"], ["hook", "run", "pre-commit"], ["hook", "run", "pre-push"],
        ["hook", "run", "bogus"], ["reflog"], _png_step("a.png", (9, 8, 7, 255)),
        ("stamp", ["commit", "-a", "-m", "hooked"]), _png_step("b.png", (9, 8, 7, 255)),
        ("stamp", ["commit", "-a", "-m", "blocked"]), ("stamp", ["commit", "--amend", "-m", "amended"]),
        _write(".tmi/hooks/pre-commit.py", _HOOK_PY), ("stamp", ["commit", "-m", "blocked"]),
        ("stamp", ["commit", "-m", "py pre-commit"]), ["status", "-s"], ["log", "--oneline"],
    ]),
    ("merge", scenario_write_conflict, [
        ["status"], ["status", "-s"], ["switch", "other"], ["checkout", "other"], ["restore", "--ours", "c.png"],
        ["status"], ["restore", "--theirs", "c.png"], ["status"], ["restore", "--ours", "c.png:layer"],
        ["restore", "--theirs", "nope.png"], ["add", "c.png"], ["status"], ["status", "-s"],
        ["restore", "--staged", "c.png"], ["status"], ["restore", "--ours", "--theirs", "c.png"], ["recover", "--files"],
        ["merge", "--abort"], ["status"], ["restore", "--ours", "c.png"], ["switch", "other"],
    ]),
    ("logs", scenario_write_base, [
        _png_step("a.png", (77, 66, 55, 255)),
        ("stamp", ["commit", "-a", "-m", "Straße commit\n\nbody"], {"TMI_AUTHOR_NAME": "Straße Ünï", "TMI_AUTHOR_EMAIL": "s@x"}),
        ["log", "--author", "STRASSE"], ["log", "--author", "ÜNÏ", "--oneline"], ["log", "--author", "strasse", "--oneline"],
        ["log", "--author", "ﬀ", "--oneline"], ["log", "--author", "Tester", "--oneline"],
        ["log", "--grep", "^Stra"], ["log", "--grep", "work$", "--oneline"], ["log", "--grep", "main|feature", "--oneline"],
        ["log", "--grep", "[ab]ody", "--oneline"], ["log", "--grep", "fea.ure", "--oneline"], ["log", "--grep", "x*", "-n", "1"],
        ["log", "--grep", "fir?st", "--oneline"], ["log", "--grep", "\\d+"], ["log", "--grep", "(?i)main"], ["log", "--grep", "["],
        ["log", "--grep", "a{2}"], ["log", "--grep", "(work|body)", "--oneline"], ["log", "--grep", "", "--oneline"],
        ["log", "--since", "2026-02-01", "--oneline"], ["log", "--since", "2026-01-01T00:00:00Z", "--oneline"],
        ["log", "--until", "2026-01-02", "--oneline"], ["log", "--after", "2026-01-01", "--before", "2026-12-31T00:00:00+09:00",
                                                     "--oneline"],
        ["log", "--since", "yesterday"], ["log", "--since", "2026-13-01"], ["log", "--since", " 2026-01-02 ", "--oneline"],
        ["log", "--until", "2026-01-03 09:00", "--oneline"], ["log", "--date", "local"],
        ["log", "--date", "local", "--format=%ad %s"], ["show", "--date", "local"], ["log", "--date", "short", "-n", "1"],
        ["ls-layers", "art/big.clip"], ["ls-layers", "a.png", "HEAD~1"], ["ls-layers", "art/big.clip", "v1"],
        ["ls-layers", "nope.png"], ["ls-layers", "a.png", "nope"], _png_step("a.png", (1, 1, 1, 255)), ["ls-layers", "a.png"],
    ]),
    ("diff", scenario_diff_base, [
        ["diff"], ["diff", "--stat", "main", "feature"], ["diff", "--name-only", "main", "feature"],
        ["diff", "--name-status", "main", "feature"], ["diff", "--name-only", "--name-status", "feature"],
        ["diff", "main", "feature", "--out", "outs/d1"], ["diff", "--overlay", "main", "feature", "--out", "outs/d2"],
        ["diff", "--per-layer", "main", "feature", "--out", "outs/d3"],
        ["diff", "--html", "--no-open", "main", "feature", "--out", "outs/d4"],
        ["diff", "--side-by-side", "feature", "main", "pic/art.mdp", "--out", "outs/d5"],
        ["diff", "--html", "--overlay", "main", "feature"], ["diff", "main", "feature", "nope.png"],
        ["diff", "nope1", "nope2"], ["diff", "--staged", "main", "feature"],
        _png_step("a.png", (1, 2, 200, 255)), _clip_edit_step("art.clip", -1, (30, 30, 200, 255), (100, 100, 160, 160)),
        _jpeg_step("photo.jpg", (90, 90, 90, 255)),
        ["diff", "--out", "outs/w1"], ["diff", "--html", "--no-open", "--out", "outs/w2"], ["diff", "--stat"],
        ["diff", "--name-status"], ["diff", "a.png", "--out", "outs/w3"], ["diff", "feature", "--per-layer", "--out", "outs/w4"],
        ["diff", "nope.png"], ["add", "a.png"], ["diff", "--staged", "--out", "outs/s1"], ["diff", "--cached", "--stat"],
        ["diff", "a.png"], _write(".tmi/diff/20200101-000000/x.png", b"x"), _write(".tmi/diff/20200102-000000/x.png", b"x"),
        _write(".tmi/diff/20200103-000000/x.png", b"x"), _write(".tmi/diff/keep-me/x.png", b"x"), ["config", "diff.retain", "2"],
        ["diff", "--html", "--no-open"], ["diff", "--overlay", "--out", "outs/o"], _remove("pic/art.mdp"),
        _png_step("new.png", (5, 5, 5, 255)), ["add", "new.png"], ["diff", "--name-status", "HEAD"],
        ["diff", "HEAD", "--out", "outs/x"], ["diff", "--stat", "HEAD"], ["diff", "--html", "--no-open", "HEAD", "--out", "outs/xh"],
        _copy_step(ROOT / "example" / "new2.mdp", "pic/art.mdp"), ["diff", "feature", "--stat"],
        ["diff", "feature", "--html", "--no-open", "--out", "outs/f"],
    ]),
    ("merges", scenario_merge_base, [
        ["merge"], ["merge", "nope"], ("stamp", ["merge", "ff"]), ["merge", "ff"], ["log", "--oneline", "--graph"],
        _clip_edit_step("art.clip", 0, (200, 30, 30, 255), (10, 10, 60, 40)),
        _mdp_edit_step("art.mdp", 0, (255, 0, 0, 255), (0, 0, 20, 20)), _jpeg_step("photo.jpg", (90, 10, 10, 255)),
        ("stamp", ["commit", "-a", "-m", "main work"]), ["merge", "layers", "--abort"],
        ("stamp", ["merge", "layers", "-m", "merge layers\n\nbody"]), ["log", "--oneline", "--graph"], ["status"],
        ["ls-layers", "art.clip"], ["diff", "--stat", "HEAD~1"],
        ("stamp", ["merge", "clash"]), ["status"], ["status", "-s"], ("stamp", ["merge", "clash"]),
        ("stamp", ["commit", "-m", "too early"]), ("stamp", ["commit", "-a", "-m", "too early"]),
        ("stamp", ["commit", "--amend"]), ["diff", "--stat"], ["restore", "--theirs", "art.clip:レイヤー 1"], ["status"],
        ["restore", "--ours", "art.clip:nope"], ["restore", "--theirs", "art.clip:用紙"], ["restore", "--ours", "art.clip:レイヤー 1"],
        ["restore", "--theirs", "art.clip:レイヤー 1"], ["restore", "--theirs", "photo.jpg:photo"], ["restore", "--ours", "a.png"],
        ["add", "art.clip"], ["add", "a.png"], ["status"], ["restore", "--staged", "a.png"], ["status", "-s"],
        ["restore", "--theirs", "a.png:a"], ["add", "a.png", "photo.jpg"], ["status"],
        ("stamp", ["commit", "-m", "resolved clash"]), ["log", "--oneline", "--graph"], ["status"], ["reflog"],
    ]),
    ("merge-abort", scenario_merge_base, [
        _png_step("a.png", (1, 2, 3, 255)), ("stamp", ["commit", "-a", "-m", "main png"]), ("stamp", ["merge", "clash"]),
        ["status"], ["restore", "--theirs", "a.png"], _png_step("a.png", (4, 5, 6, 255)), ["merge", "--abort"], ["status"],
        ["recover", "--files"], ["merge", "--abort"], ("stamp", ["merge", "clash", "-m", "again"]), ["add", "a.png"],
        _remove("art.mdp"), ["add", "art.mdp"], ["status"], ["merge", "--abort"], ["status"], ["log", "--oneline"],
        ("stamp", ["merge", "layers"]), ["log", "--oneline", "--graph"],
        _clip_edit_step("art.clip", 0, (5, 5, 250, 255), (40, 40, 90, 90)), _mdp_edit_step("art.mdp", 2, (9, 9, 9, 255), (0, 0, 9, 9)),
        ("stamp", ["stash", "push", "-m", "clip wip"]), ["status"], ["stash", "list"], ["stash", "pop"], ["status"],
        ["add", "art.mdp"], ("stamp", ["stash", "save", "both", "words"]), ["stash", "pop"], ["status", "-s"],
    ]),
    ("layer-restore", scenario_merge_base, [
        _copy_step(ROOT / "example" / "layer_types.clip", "types.clip"), ["add", "types.clip"],
        ("stamp", ["commit", "-m", "types"]), _clip_edit_step("art.clip", 0, (7, 7, 7, 255), (0, 0, 30, 30)),
        ["restore", "--source", "HEAD", "types.clip:フォルダー 1"], ["restore", "--source", "HEAD", "--output", "o/k.png", "types.clip:コマ 1"],
        ["restore", "--source", "HEAD", "--in-place", "types.clip:フォルダー 1"], ["restore", "--source", "clash", "art.clip:レイヤー 1"],
        ["restore", "--source", "clash", "--in-place", "art.clip:レイヤー 1"], ["status", "-s"],
        ["restore", "--source", "clash", "--in-place", "a.png:a", "photo.jpg:photo", "art.mdp:レイヤー2"], ["status"],
        ["restore", "--source", "WORKTREE", "--output", "o/w1.png", "art.clip:レイヤー 1"],
        ["restore", "--source", "WORKTREE", "--output", "o/w2.png", "types.clip:フォルダー 1"],
        ["restore", "--source", "WORKTREE", "--output", "o/w3.png", "art.mdp:レイヤー2"],
        ["restore", "--source", "WORKTREE", "--output", "o/whole.jpg", "photo.jpg"], ["restore", "--source", "WORKTREE", "a.png:a"],
        ["restore", "--source", "WORKTREE", "--output", "o/x.png", "art.clip:nope"], ["restore", "--in-place", "art.clip:用紙"],
        ["restore", "art.clip:レイヤー 1"], ["restore", "--source", "HEAD~1", "art.mdp:レイヤー2", "--output", "o/m.png"],
        # a whole file to an output of another extension (issue #30)
        ["restore", "--source", "HEAD", "--output", "o/photo.png", "photo.jpg"], ["restore", "--output", "o/art.PNG", "art.mdp"],
        ["restore", "--output", "o/types.png", "types.clip"], ["restore", "--output", "o/photo.gif", "photo.jpg"],
        ["restore", "--output", "o/photo.jpeg", "photo.jpg"], ["restore", "--output", "o/art.bak", "art.clip"],
        ["restore", "--source", "WORKTREE", "--output", "o/wt.png", "art.clip"],
        ["restore", "--source", "WORKTREE", "--output", "o/wt2.png", "photo.jpg"],
        ["restore", "--source", "WORKTREE", "--output", "o/wt.clip.webp", "art.clip"],
        _remove("art.clip"), ["restore", "--source", "HEAD", "--in-place", "art.clip:レイヤー 1"], ["status", "-s"],
    ]),
    # lock files (issue #35): a lock that exists stops the command with the same words in both
    # implementations and nothing is written; the first line of each group has the lock in place
    ("locks", scenario_write_base, [
        _png_step("a.png", (5, 6, 7, 255)), _write(".tmi/index.lock", ""), ["add", "a.png"], ["commit", "-a", "-m", "x"],
        ["restore", "--staged", "a.png"], ["restore", "a.png"], ["reset"], ["reset", "--hard"], ["rm", "--cached", "b.png"],
        ["mv", "b.png", "c.png"], ["switch", "feature"], ["checkout", "feature"], ["merge", "feature"], ["stash", "push"],
        ["stash", "pop"], ["status", "-s"], ["log", "--oneline"], _remove(".tmi/index.lock"), ["add", "a.png"],
        _write(".tmi/refs/heads/main.lock", ""), ("stamp", ["commit", "-m", "blocked by the ref lock"]),
        ("stamp", ["commit", "--amend", "-m", "blocked"]), ["status", "-s"], ["branch", "topic.lock"], ["tag", "v2.lock"],
        _write(".tmi/refs/heads/topic.lock", ""), ["branch", "topic"], ["switch", "-c", "topic"], ["branch", "-v"],
        _write(".tmi/refs/tags/v3.lock", ""), ["tag", "v3"], ("stamp", ["tag", "-a", "v3", "-m", "annotated"]), ["tag"],
        _remove(".tmi/refs/tags/v3.lock"), _remove(".tmi/refs/heads/topic.lock"), ["branch", "topic"],
        ["tag", "-d", "v1"], _write(".tmi/refs/heads/topic.lock", ""), ["branch", "-D", "topic"], ["branch", "-v"],
        _remove(".tmi/refs/heads/topic.lock"), ["branch", "-D", "topic"], _remove(".tmi/refs/heads/main.lock"),
        ("stamp", ["commit", "-m", "after the locks"]), ["log", "--oneline"], ["status"],
    ]),
    ("meta", scenario_empty, [
        ["--version"], ["-h"], ["--help"], ["log", "-h"], ["remote", "add", "-h"], ["stash", "push", "--help"],
        ["recover", "-h"], ["reflog", "-h"], ["ls-layers", "-h"], ["status"], ["switch", "main"], ["reset"],
        ["gc"], ["stash"], ["hook", "list"],
    ]),
]


_re_stamp = __import__("re").compile(rb"\d{8}-\d{6}")


def _mask(text: bytes) -> bytes:
    import re as _re

    text = _re.sub(rb"\d{4}-\d{2}-\d{2}[T ]\d{2}:\d{2}:\d{2}(\.\d+)?(Z|\+00:00)?", b"<TIME>", text)
    return text


_PROGRESS_LINE = __import__("re").compile(rb"^tmiki: \[ *\d+\.\ds\] (.*)$")
_DONE_LINE = __import__("re").compile(rb"^tmiki: done in \d+\.\ds \((.*)\)$")


def _phase_of(msg: bytes) -> bytes:
    """cli.py `_phase_key`."""
    words = []
    for word in msg.split():
        if len(words) == 2 or not word.isalpha() or not word.islower():
            break
        words.append(word)
    return b" ".join(words) or msg


def _normalize_progress(text: bytes) -> bytes:
    """TMI_PROGRESS=1's lines without what depends on the clock or on how
    threads interleave: no times; "decoding layers k/N" only for k = 0 (the
    decode pool's ticks interleave with the composite's phases); a phase's
    line once where it repeats (the once-a-second lines within a phase);
    the closing breakdown's phase names only."""
    out: list[bytes] = []
    last = None
    for line in text.split(b"\n"):
        bare = line.rstrip(b"\r")
        m = _PROGRESS_LINE.match(bare)
        if m:
            msg = m.group(1)
            if msg.startswith(b"decoding layers ") and not msg.startswith(b"decoding layers 0/"):
                continue
            phase = _phase_of(msg)
            if phase == last:
                continue
            last = phase
            out.append(b"tmiki: [T] " + msg)
            continue
        m = _DONE_LINE.match(bare)
        if m:
            names = [part.rsplit(b" ", 1)[0] for part in m.group(1).split(b", ") if part]
            out.append(b"tmiki: done (" + b", ".join(names) + b")")
            last = None
            continue
        out.append(line)
    return b"\n".join(out)


def write_state(repo: Path, user_config: Path) -> dict:
    """Everything a write may change, with wall-clock values masked."""
    import re as _re

    tmi = repo / ".tmi"
    state: dict = {}
    refs = {}
    stash_refs = []
    for path in sorted((tmi / "refs").rglob("*")):
        if path.is_file():
            rel = path.relative_to(tmi).as_posix()
            value = path.read_text(encoding="utf-8")
            if rel.startswith("refs/stash/"):
                stash_refs.append(value)
            else:
                refs[rel] = value
    state["refs"] = refs
    state["stash_refs"] = sorted(stash_refs)
    for name in ("HEAD", "index", "config", "MERGE_HEAD", "MERGE_MSG", "hooks.log"):
        path = tmi / name
        state[name] = path.read_bytes() if path.exists() else None
    state["objects"] = sorted(p.parent.name + p.name for p in (tmi / "objects").glob("*/*") if p.is_file())
    state["object_dirs"] = sorted(p.name for p in (tmi / "objects").iterdir()) if (tmi / "objects").is_dir() else []
    logs = {}
    for path in sorted((tmi / "logs").rglob("*")) if (tmi / "logs").is_dir() else []:
        if path.is_file():
            data = path.read_bytes()
            logs[path.relative_to(tmi).as_posix()] = _re.sub(rb" \d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z\t", b" <TS>\t", data)
    state["logs"] = logs
    backups = tmi / "backups.json"
    state["backups"] = (_re.sub(rb'"(id|timestamp)": "[^"]*"', rb'"\1": "<X>"', backups.read_bytes())
                        if backups.exists() else None)
    stash = tmi / "stash.json"
    state["stash"] = (_re.sub(rb'"(id|ref)": "[^"]*"', rb'"\1": "<X>"', stash.read_bytes()) if stash.exists() else None)
    cache = tmi / "stat_cache"
    state["stat_cache"] = (_re.sub(rb'"(mtime_ns|written_ns)": \d+', rb'"\1": 0', cache.read_bytes())
                           if cache.exists() else None)
    state["conflicts"] = {p.relative_to(tmi).as_posix(): p.read_bytes()
                          for p in sorted((tmi / "conflicts").rglob("*")) if p.is_file()} if (tmi / "conflicts").is_dir() else {}
    # an automatic diff directory is named by the wall clock (%Y%m%d-%H%M%S)
    state["scratch"] = sorted(_re.sub(r"\d{8}-\d{6}", "<STAMP>", p.relative_to(tmi).as_posix())
                              for name in ("diff", "cache", "tmp") for p in (tmi / name).rglob("*")) if tmi.is_dir() else []
    work = {}
    for path in sorted(repo.rglob("*")):
        rel = path.relative_to(repo)
        if rel.parts and rel.parts[0] == ".tmi":
            continue
        data = path.read_bytes() if path.is_file() else "<dir>"
        if path.suffix == ".html" and path.is_file():
            # diff --html: the report's time stamp (datetime.now())
            data = _re.sub(rb"tmiki diff @ \d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}", b"tmiki diff @ <TS>", data)
        work[rel.as_posix()] = data
    state["worktree"] = work
    state["user_config"] = user_config.read_bytes() if user_config.exists() else None
    return state


def run_write_scenario(name: str, builder, steps: list, binary: Path, env: dict[str, str], work: Path) -> dict:
    base = work / f"wbase-{name}"
    if base.exists():
        shutil.rmtree(base)
    builder(Runner(env), base)
    user_home = Path(env["TMI_CONFIG_HOME"])
    user_base = (user_home / "config").read_bytes()
    run_dir = work / f"wrun-{name}"
    modes = {"python": "P", "native": "R", "alt-native-first": "RP", "alt-python-first": "PR"}
    results: dict[str, list] = {}
    forward_log = work / f"forward-{name}.log"
    native_steps: dict[str, int] = {}
    forwarded_steps: list[str] = []
    for mode, pattern in modes.items():
        if run_dir.exists():
            shutil.rmtree(run_dir)
        shutil.copytree(base, run_dir)
        (user_home / "config").write_bytes(user_base)
        clock = 100
        record = []
        tmi_index = 0
        for step in steps:
            if callable(step):
                step(run_dir)
                record.append((step.__name__, None, write_state(run_dir, user_home / "config")))
                continue
            stamp = isinstance(step, tuple)
            args = list(step[1]) if stamp else list(step)
            side = pattern[tmi_index % len(pattern)]
            tmi_index += 1
            step_env = dict(env, TMI_PYTHON=sys.executable)
            if stamp and len(step) > 2:
                step_env.update(step[2])
            if stamp:
                clock += 1
                step_env["TMI_COMMIT_TIMESTAMP"] = f"2026-02-{clock % 28 + 1:02d}T10:00:00+00:00"
            if side == "P":
                step_env["TMI_NATIVE"] = "0"
            else:
                step_env["TMI_FORWARD_LOG"] = str(forward_log)
                forward_log.unlink(missing_ok=True)
            res = subprocess.run([str(binary), *args], cwd=run_dir, env=step_env, capture_output=True, timeout=120)
            if env.get("TMI_PROGRESS") == "1":
                res.stderr = _normalize_progress(res.stderr)
            label = " ".join(args)
            if side == "R" and mode == "native":
                log = forward_log.read_text(encoding="utf-8") if forward_log.exists() else ""
                if log.startswith("native"):
                    key = args[0] if args else "(none)"
                    native_steps[key] = native_steps.get(key, 0) + 1
                else:
                    forwarded_steps.append(f"{label}: {log.strip().split(chr(9), 1)[-1]}")
            wall = args[:1] in (["reflog"], ["recover"])  # wall-clock times in the listing
            stdout, stderr = (_mask(res.stdout), _mask(res.stderr)) if wall else (res.stdout, res.stderr)
            if args[:1] == ["diff"]:  # the automatic output directory's name
                stdout = _re_stamp.sub(b"<STAMP>", stdout)
            record.append((label, (res.returncode, stdout, stderr), write_state(run_dir, user_home / "config")))
        results[mode] = record
    shutil.rmtree(run_dir, ignore_errors=True)
    (user_home / "config").write_bytes(user_base)
    failures = []
    reference = results["python"]
    for mode, record in results.items():
        if mode == "python":
            continue
        for i, ((label, out, state), (_, ref_out, ref_state)) in enumerate(zip(record, reference)):
            if out != ref_out:
                failures.append({"mode": mode, "step": i, "command": label, "what": "output",
                                 "python": [ref_out[0], ref_out[1].decode(errors="replace")[:1500], ref_out[2].decode(errors="replace")[:800]],
                                 "rust": [out[0], out[1].decode(errors="replace")[:1500], out[2].decode(errors="replace")[:800]]})
            if state != ref_state:
                diff = sorted(k for k in state if state[k] != ref_state.get(k))
                detail = {}
                for k in diff:
                    a, b = ref_state.get(k), state.get(k)
                    if isinstance(a, dict) and isinstance(b, dict):
                        detail[k] = {kk: [str(a.get(kk))[:300], str(b.get(kk))[:300]]
                                     for kk in sorted(set(a) | set(b)) if a.get(kk) != b.get(kk)}
                    else:
                        detail[k] = [str(a)[:600], str(b)[:600]]
                failures.append({"mode": mode, "step": i, "command": label, "what": "state", "differs": detail})
                break  # later steps differ as a consequence
    return {"steps": len(steps), "native": native_steps, "forwarded": forwarded_steps, "failures": failures}


# ---------------------------------------------------------------------------
# Transport (RUST_MIGRATION.md 4b): init, clone, fetch, push and pull with a
# local remote and with an HTTPS one (tools/fake_tmihub.py, which records
# every request). Several repositories side by side in one directory; each
# step runs in one of them. The same four ways as the write scenarios; the
# state after every step is every repository's (working, bare and the
# hub's) and the requests the hub saw.
# ---------------------------------------------------------------------------

def T(cwd: str, *args: str, stamp: bool = False, env: dict | None = None) -> dict:
    return {"cwd": cwd, "args": list(args), "stamp": stamp, "env": env or {}}


def _tsrc(run: Runner, root: Path, url: str) -> None:
    ex = ROOT / "example"
    src = root / "src"
    src.mkdir(parents=True)
    run.tmi(src, "init")
    _png(src / "a.png", (200, 40, 40, 255))
    _png(src / "画像" / "空.png", (10, 120, 220, 128), (17, 9))
    (src / "art").mkdir()
    shutil.copyfile(ex / "new.mdp", src / "art" / "new.mdp")
    shutil.copyfile(ex / "blend_modes.clip", src / "art" / "blend.clip")
    shutil.copyfile(ex / "layer_types.clip", src / "art" / "big.clip")  # 2 MB: a chunked blob
    run.tmi(src, "add", "a.png", "画像/空.png", "art/new.mdp", "art/blend.clip", "art/big.clip")
    run.tmi(src, "commit", "-m", "first", stamp=True)
    run.tmi(src, "switch", "-c", "feature")
    _png(src / "f.png", (1, 200, 1, 255))
    run.tmi(src, "add", "f.png")
    run.tmi(src, "commit", "-m", "feature work", stamp=True)
    run.tmi(src, "switch", "main")
    run.tmi(src, "remote", "add", "origin", url)


def tbase_local(run: Runner, root: Path) -> None:
    root.mkdir(parents=True)
    run.tmi(root, "init", "--bare", "bare.tmi")
    _tsrc(run, root, "../bare.tmi")


def tbase_hub(run: Runner, root: Path) -> None:
    root.mkdir(parents=True)
    (root / "hub" / "u").mkdir(parents=True)
    run.tmi(root / "hub" / "u", "init", "--bare", "r")
    _tsrc(run, root, run.hub_url)


_PRE_PUSH = r'''import os
from pathlib import Path
keys = ["TMI_HOOK", "TMI_REMOTE", "TMI_BRANCH", "TMI_COMMIT", "TMI_REMOTE_COMMIT"]
with open(Path(os.environ["TMI_REPO"]) / ".tmi" / "hooks.log", "a", encoding="utf-8") as f:
    f.write(" ".join(f"{k}={os.environ.get(k, '-')}" for k in keys) + "\n")
print("pre-push ran")
'''


def _tedit(rel: str, rgba: tuple[int, int, int, int]):
    def step(root: Path) -> None:
        _png(root / rel, rgba)
    step.__name__ = f"png {rel}"
    return step


def _thook(repo: str):
    def step(root: Path) -> None:
        hooks = root / repo / ".tmi" / "hooks"
        hooks.mkdir(parents=True, exist_ok=True)
        (hooks / "pre-push.py").write_text(_PRE_PUSH, encoding="utf-8")
    step.__name__ = f"pre-push hook in {repo}"
    return step


def _transport_common(url: str, clone_dir: str) -> list:
    """Steps both remotes share; `url` is how a fresh repo names the remote."""
    return [
        T("", "init", "fresh"), T("", "init", "--bare", "fresh.tmi"), T("fresh", "init"), T("fresh", "init", "sub/deeper"),
        T("src", "push"), T("src", "push", "origin", "feature"), T("src", "push", "nope"), T("src", "push", "origin", "ghost"),
        T("", "clone", url), T(clone_dir, "status"), T("", "clone", url, "c2"), T("", "clone", url, "c2"),
        T("c2", "log", "--oneline", "--all", "--decorate"), T("c2", "branch", "-a"), T("c2", "remote", "-v"),
        _tedit("src/a.png", (5, 6, 7, 255)), T("src", "commit", "-a", "-m", "second", stamp=True), T("src", "push"),
        T("c2", "fetch"), T("c2", "pull", env={"TMI_PROGRESS": "1"}), T("c2", "pull"), T("c2", "fetch", "--all"),
        T("c2", "fetch", "nope"),
        T("c2", "pull", "origin", "feature", stamp=True, env={"TMI_PROGRESS": "1"}),  # a true merge
        T("c2", "push"), T("src", "fetch"), T("src", "pull"), T("src", "status"),
        T("", "init", "empty"), T("empty", "remote", "add", "origin", url if "://" in url else "../" + url),
        T("empty", "pull"), T("empty", "status"), T("empty", "log", "--oneline"), T("empty", "pull", "origin", "nobranch"),
        _tedit("c2/a.png", (9, 9, 9, 255)), T("c2", "commit", "-a", "-m", "from c2", stamp=True), T("c2", "push"),
        _tedit("src/b.png", (1, 1, 1, 255)), T("src", "add", "b.png"), T("src", "commit", "-m", "from src", stamp=True),
        T("src", "push"),  # rejected: not a fast-forward
        T("src", "push", "--force-with-lease"),  # tracking ref is stale: rejected
        _tedit("empty/a.png", (2, 2, 2, 255)), T("empty", "pull"),  # local changes block the merge
        T("empty", "restore", "a.png"), T("empty", "pull"),
        _thook("src"), T("src", "push", "origin", "feature"), T("src", "fetch", env={"TMI_PROGRESS": "1"}),
        T("c2", "fetch", env={"TMI_PROGRESS": "1"}),
        T("", "clone", url, "c5", env={"TMI_PROGRESS": "1", "TMIHUB_BATCH_OBJECTS": "4"}),
        T("c5", "log", "--oneline", "-n", "3"),
        # a pull whose merge conflicts (stage 4a's merge), then given up
        _tedit("c5/a.png", (11, 22, 33, 255)), T("c5", "commit", "-a", "-m", "c5 change", stamp=True), T("c5", "push"),
        _tedit("empty/a.png", (44, 55, 66, 255)), T("empty", "commit", "-a", "-m", "empty change", stamp=True),
        T("empty", "pull", stamp=True), T("empty", "status"), T("empty", "merge", "--abort"), T("empty", "log", "--oneline", "-n", "2"),
    ]


TRANSPORT_SCENARIOS = [
    ("transport-local", tbase_local, None, lambda url: _transport_common("bare.tmi", "bare") + [
        T("", "clone", "nowhere.tmi"), T("", "clone", "src", "c3"), T("c3", "log", "--oneline"),
        _tedit("c3/a.png", (4, 4, 4, 255)), T("c3", "commit", "-a", "-m", "c3 work", stamp=True),
        T("c3", "push"),  # src has main checked out
        T("c3", "push", "origin", "main:x"), T("src", "remote", "add", "peer", "../c3"), T("src", "push", "peer", "feature"),
        T("src", "push", "--force"), T("src", "log", "--oneline", "-n", "2", "--all"),
        T("", "clone", "src/../bare.tmi", "c6"), T("", "clone", "file:///nowhere", "c7"),
    ]),
    ("transport-https", tbase_hub, None, lambda url: _transport_common(url, "r") + [
        T("", "clone", url.rsplit("/", 1)[0] + "/missing", "c4"), T("", "clone", "ftp://example.com/r"),
        T("src", "push", "--force"),  # tmihub refuses force pushes (409)
        T("src", "push", env={"TMIHUB_BATCH_OBJECTS": "2", "TMIHUB_BATCH_BYTES": "1000"}),
    ]),
    ("transport-unsafe-local", tbase_local, None, lambda url: _unsafe_transport_steps("bare.tmi", "bare.tmi") + [
        _forge_step("src", "evilC", ["../escape.txt"]), T("src", "push", "origin", "evilC"),
        T("", "clone", "src", "c3"),
    ]),
    ("transport-unsafe-https", tbase_hub, None, lambda url: _unsafe_transport_steps(url, "hub/u/r")),
    ("transport-auth", tbase_hub, "s3cret", lambda url: [
        T("", "clone", url, "c1"), T("", "clone", url, "c2", env={"TMIHUB_TOKEN": " s3cret "}),
        T("src", "push"), T("src", "push", env={"TMIHUB_TOKEN": "wrong"}),
        T("src", "config", "--global", "tmihub.token", "s3cret"), T("src", "push"), T("c2", "pull"),
        T("src", "config", "--global", "--unset", "tmihub.token"),
    ]),
]


def _clip_edit(rel: str, main_id: int, colour: tuple[int, int, int, int], box: tuple[int, int, int, int]):
    def step(repo: Path) -> None:
        _edit_clip_layer(repo / rel, main_id, colour, box)
    step.__name__ = f"edit {rel} layer {main_id}"
    return step


def _copy_example(name: str, rel: str):
    def step(repo: Path) -> None:
        (repo / rel).parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(ROOT / "example" / name, repo / rel)
    step.__name__ = f"copy {name} to {rel}"
    return step


# Stage 4b's gaps, run as the write scenarios are.
GAP_SCENARIOS = [
    ("dashdash", scenario_write_base, [
        _png_step("a.png", (1, 2, 3, 255)), ["checkout", "--", "a.png"], ["checkout", "HEAD~1", "--", "b.png"], ["status", "-s"],
        ["restore", "--source", "v1", "--", "a.png"], ["reset", "HEAD", "--", "a.png"], ["add", "--", "a.png"],
        ["status", "-s"], ["log", "--oneline", "--", "a.png"], ["log", "--"], ["rm", "--cached", "--", "b.png"],
        ["status", "-s"], ["reset", "--", "b.png"], ["mv", "--", "b.png", "c.png"], ["checkout", "--", "--", "a.png"],
        ["commit", "-m", "x", "--"], ["checkout", "-b", "--", "x"], ["show", "--", "HEAD"], ["branch", "--", "newb"],
        ["tag", "--", "t1"], _png_step("-x.png", (5, 5, 5, 255)), ["add", "--", "-x.png"], ["status", "-s"],
        ["restore", "--staged", "--", "-x.png"], ["checkout", "a.png", "--"], ["push", "--", "nowhere"],
        ["add", "a.png", "--source", "--", "b.png"],
    ]),
    ("argerrors", scenario_write_base, [
        [], ["bogus"], ["--bogus"], ["mv", "a"], ["clone"], ["switch"], ["restore"], ["remote", "add", "x"],
        ["status", "--bogus"], ["status", "--bogus", "x"], ["commit", "-m"], ["log", "--date", "foo"], ["log", "-n", "x"],
        ["push", "-f", "--force-with-lease"], ["switch", "a", "b", "c"], ["log", "--oneline", "--bogus=1"], ["status", "-x"],
        ["mv", "a", "b", "c", "d"], ["commit", "-m", "--amend"], ["status", "--porcelain=v2"], ["branch", "--sort"],
        ["log", "--decorate=bad"], ["reset", "--soft", "--hard"], ["log", "--a"], ["show", "a", "b"], ["ls-layers"],
        ["init", "a", "b"], ["fetch", "a", "b"], ["hook", "run"], ["gc", "--prune"], ["config", "a", "b", "c"],
        ["status", "--short=x"], ["status", "-sx"], ["status", "-sb"], ["log", "-n2"], ["log", "-nx"], ["add", "-Af"],
        ["commit", "-am"], ["log", "--date=ü"], ["switch", "--disc", "main"], ["checkout", "-b"], ["diff", "--bogus"],
        ["merge", "a", "b"], ["stash", "push", "-x"], ["remote", "rename", "a"], ["tag", "-n", "x", "y", "z"],
        ["log", "--max-count", " 3 ", "--oneline"], ["log", "-n", "1_0", "--oneline"], ["recover", "--as"],
    ]),
    ("layerignore", scenario_write_base, [
        _write(".tmiignore", "art/*.clip:ラスタレイヤー\n"), _clip_edit("art/big.clip", 3, (250, 10, 10, 255), (0, 0, 40, 40)),
        _clip_edit("art/big.clip", 7, (10, 250, 10, 255), (10, 10, 50, 50)), ["status"], ["add", "art/big.clip"],
        ["status"], ["status", "-s"], ("stamp", ["commit", "-m", "folder layer only"]), ["ls-layers", "art/big.clip"],
        ["status"], _copy_example("blend_modes.clip", "art/new.clip"), ["add", "art/new.clip"],
        _write(".tmiignore", "art/*.clip:ラスタレイヤー\nart/new.clip:ラスタ1*\n!art/new.clip:ラスタ1 のコピー 9\n"
                             "art/*.clip:it's*\n"),
        ("stamp", ["commit", "-m", "new clip"]), _clip_edit("art/new.clip", 21, (1, 2, 3, 255), (0, 0, 30, 30)),
        _clip_edit("art/new.clip", 42, (3, 2, 1, 255), (0, 0, 30, 30)), _clip_edit("art/new.clip", 20, (9, 9, 9, 255), (0, 0, 30, 30)),
        ["status", "-s"], ("stamp", ["commit", "-a", "-m", "partly ignored"]), ["status"], ["log", "--stat", "-n", "1"],
        ["mv", "art/new.clip", "art/moved.clip"], ["status", "-s"], ("stamp", ["commit", "-m", "moved"]),
        _write(".tmiignore", "*.clip:*\n"), _clip_edit("art/big.clip", 7, (1, 1, 1, 255), (0, 0, 9, 9)),
        ["add", "-A"], ["status"],
    ]),
    ("opaque", scenario_write_base, [
        _write("notes.txt", "hello\n"), ["add", "notes.txt"], ["status"], ["status", "-s"],
        ("stamp", ["commit", "-m", "notes"]), _write("notes.txt", "hello again\n"), ["status"], ["status", "--porcelain"],
        ("stamp", ["commit", "-a", "-m", "notes changed"]), _write("data/blob.bin", bytes(range(256)) * 9),
        _write("data/empty.dat", b""), _write("Makefile", "all:\n\techo hi\n"), ["add", "-A"], ["status"],
        ("stamp", ["commit", "-m", "more files"]), ["mv", "notes.txt", "docs/notes.md"], ["status", "-s"],
        ("stamp", ["commit", "-m", "moved"]), ["rm", "Makefile"], _write("data/blob.bin", b"x" * 3000),
        ["status"], ["add", "data"], ["restore", "--staged", "data/blob.bin"], ["status", "-s"],
        _write("fake.gif", b"GIF89a\x01\x00\x01\x00not really"), ["add", "fake.gif"],  # a signature alone is not an image
        _write("hidden.dat", b"\x89PNG\r\n\x1a\nnot really"), ["add", "hidden.dat"], ["status"],
        ("stamp", ["commit", "-a", "-m", "all"]), ["log", "--stat", "-n", "2"], ["show", "--name-status"],
        ["restore", "--source", "HEAD~3", "docs/notes.md"], ["status"],
        # PLUGINS.md P0e: image iff tmi's decoders read it, whatever the extension says
        _write("pics/real.gif", _tiny_gif()), _write("pics/real.dat", _tiny_gif()), _write("pics/gif.png", _tiny_gif()),
        _write("pics/nope.png", b"\x89PNG\r\n\x1a\nnot really"), _write("pics/cut.jpg", _truncated_jpeg()),
        # Pillow opens these; tmi has no decoder for them, or declines the variant
        _write("pics/pixel.tga", _tga()), _write("pics/lab.tif", _lab_tiff()), _write("pics/a.tga.png", _tga()),
        ["status", "-s"], ["add", "pics"], ["status"], ("stamp", ["commit", "-m", "pictures"]), ["show", "--stat"],
        ["log", "--stat", "-n", "1"], ["ls-layers", "pics/real.gif"], ["ls-layers", "pics/gif.png"],
        ["ls-layers", "pics/pixel.tga"], ["ls-layers", "pics/cut.jpg"], ["ls-layers", "pics/lab.tif"],
        _write("pics/real.gif", _tiny_gif() + b"\0"), _write("pics/pixel.tga", _tga() + b"\1"),
        _write("pics/cut.jpg", _truncated_jpeg()[:-2]), ["status"], ["diff", "--stat"], ["diff", "pics/real.gif"],
        ["restore", "--source", "HEAD", "--output", "outs/real.png", "pics/real.gif"],
        ["restore", "--source", "HEAD", "pics/pixel.tga", "pics/cut.jpg"], ["status", "-s"],
        ("stamp", ["commit", "-a", "-m", "pictures changed"]), ["diff", "HEAD~1", "HEAD", "--stat"],
    ]),
]

# Trees a hostile remote writes (tests/forge.py): every path the rule of
# tmi/recpath.py refuses is refused when received (clone, fetch, pull, push) and
# when written to a working tree (switch, checkout, reset --hard, merge), with
# the same words in both languages and nothing written outside `.tmi`'s refs.
UNSAFE_PATHS = [
    "../escape.txt", "sub/../../escape2.txt", ".tmi/hooks/post-commit.bat", ".TMI/hooks/x.bat", ".tmi./hooks/x.bat",
    "TMI~1/hooks/x.bat", ".git/config", "c:/escape.txt", "c:escape.txt", "//server/share/x", "sub\\x", "/abs.txt",
    "CON", "sub/nul.txt", "file.txt:stream", "trailing.", "a//b", "a\x1b[2Jb",
]


def _forge_step(rel_repo: str, branch: str, paths: list[str], base: str = "main"):
    def step(root: Path) -> None:
        from tests.forge import forge_branch

        forge_branch(root / rel_repo, [(p, b"payload") for p in paths], branch=branch, base=base)
    step.__name__ = f"forge {branch} in {rel_repo or '.'}"
    return step


def _unsafe_write_steps() -> list:
    steps: list = []
    for i, path in enumerate(UNSAFE_PATHS):
        steps += [_forge_step("", f"evil{i}", ["good.txt", path]), ["switch", f"evil{i}"], ["reset", "--hard", f"evil{i}"],
                  ["merge", f"evil{i}"]]
        if i < 4:
            steps += [["checkout", f"evil{i}"], ["switch", "--discard", f"evil{i}"]]
    steps += [["status", "-s"], ["log", "--oneline", "-n", "2"]]
    # names that look unusual but are fine are written
    steps += [_forge_step("", "fine", ["日本語/ファイル.mdp", "a b/c d.png", ".tmiattributes", ".gitignore", "console.png", "-x.png"]),
              ["switch", "fine"], ["status", "-s"], ["switch", "main"]]
    # what tmi records itself follows the same rule
    steps += [_write("GIT~1", b"data"), ["add", "GIT~1"], ["add", "-A"], ["status", "-s"]]
    return steps


UNSAFE_SCENARIOS = [("unsafe-paths", scenario_write_base, _unsafe_write_steps())]


def _unsafe_transport_steps(url: str, remote: str) -> list:
    """`remote`: where the remote's repository is on disk, under the run directory."""
    return [
        T("src", "push"), T("", "clone", url, "c1"),
        _forge_step(remote, "evilA", ["good.txt", "../escape.txt"]),
        T("c1", "fetch"), T("c1", "fetch", "--all"), T("c1", "pull"), T("", "clone", url, "c2"), T("c1", "branch", "-a"),
        _forge_step(remote, "evilB", [".tmi/hooks/post-commit.bat"]), T("c1", "fetch"), T("c1", "status"),
        T("c1", "log", "--oneline", "--all"),
    ]

# Scenarios run with extra environment variables for every step.
ENV_SCENARIOS = [
    # stdout / stderr Python encodes as cp932 (backslashreplace for the rest)
    ("cp932", scenario_write_base, [
        ["status"], _write("ü/emoji😀.txt", "x"), _write("メモ.txt", "メモ"), ["status", "-s"], ["status"],
        ["add", "メモ.txt"], ("stamp", ["commit", "-m", "日本語の commit ü 😀"]), ["log", "--oneline"], ["log", "-n", "1"],
        ["branch", "機能"], ["branch", "-v"], ["switch", "機能"], ["tag", "タグ"], ["tag", "-n"], ["show", "--stat"],
        ["ls-layers", "art/big.clip"], ["remote", "add", "遠隔", "remotes/リモート"], ["remote", "-v"], ["init", "--bare", "remotes/リモート"],
        ["push", "遠隔", "機能"], ["fetch", "遠隔"], ["mv", "メモ.txt", "ü/メモ2.txt"], ["status", "-s"], ["add", "-A"],
        ["rm", "--cached", "ü/emoji😀.txt"], ["status"], ["config", "user.name", "名前 ü"], ["config", "--list"],
        ["reflog"], ["stash", "list"], ["switch", "nope"], ["recover", "--files"], ["clone", "remotes/リモート", "remotes/複製"],
    ], {"PYTHONIOENCODING": "cp932", "PYTHONUTF8": "0"}),
    # TMI_PROGRESS=1's phase lines (issue #26), compared without their
    # timing (_normalize_progress)
    ("progress", scenario_merge_base, [
        ["status"], _clip_edit_step("art.clip", 0, (200, 30, 30, 255), (10, 10, 60, 40)),
        _mdp_edit_step("art.mdp", 0, (255, 0, 0, 255), (0, 0, 20, 20)), _png_step("a.png", (1, 2, 200, 255)),
        ["status"], ["status", "-s"], ["diff", "--stat"], ["diff", "--out", "outs/d1"], ["diff", "--per-layer", "--out", "outs/d2"],
        ["add", "art.clip"], ("stamp", ["commit", "-a", "-m", "main work"]), ["diff", "HEAD~1", "HEAD", "--out", "outs/d3"],
        ["diff", "--name-status", "HEAD~1"], ["switch", "ff"], ["switch", "main"], ("stamp", ["merge", "layers", "-m", "ml"]),
        ("stamp", ["merge", "clash"]), ["restore", "--theirs", "art.clip:レイヤー 1"], ["restore", "--ours", "a.png"],
        ["merge", "--abort"], _clip_edit_step("art.clip", 0, (5, 5, 250, 255), (40, 40, 90, 90)),
        ("stamp", ["stash", "push", "-m", "wip"]), ["stash", "pop"], ["reset", "--hard"],
        ["restore", "--source", "HEAD~1", "a.png", "art.mdp"], ["restore", "--source", "HEAD~1", "--output", "outs/l.png", "art.clip:レイヤー 1"],
        ["checkout", "HEAD~1"], ["checkout", "main"], ["checkout", "-b", "side"], ["checkout", "HEAD~1", "a.png"],
        ["switch", "nope"], ["gc"], ["mv", "a.png", "b.png"], ["status"],
    ], {"TMI_PROGRESS": "1"}),
]


def _bare_state(repo: Path) -> dict:
    state = {"HEAD": (repo / "HEAD").read_bytes() if (repo / "HEAD").exists() else None}
    state["refs"] = {p.relative_to(repo).as_posix(): p.read_bytes() for p in sorted((repo / "refs").rglob("*")) if p.is_file()}
    state["objects"] = sorted(p.parent.name + p.name for p in (repo / "objects").glob("*/*") if p.is_file())
    for name in ("config", "index"):
        state[name] = (repo / name).read_bytes() if (repo / name).exists() else None
    return state


def transport_state(root: Path, user_config: Path) -> dict:
    state: dict = {}
    for dirpath, dirnames, filenames in os.walk(root):
        d = Path(dirpath)
        rel = d.relative_to(root).as_posix()
        if ".tmi" in dirnames:
            state[rel] = write_state(d, user_config)
            dirnames[:] = []  # its files are in write_state's worktree
        elif "objects" in dirnames and "refs" in dirnames:
            state[rel] = _bare_state(d)
            dirnames[:] = []
        else:
            for f in filenames:
                state[f"{rel}/{f}"] = (d / f).read_bytes()
    state["user_config"] = user_config.read_bytes() if user_config.exists() else None
    return state


def _mask_progress(text: bytes) -> bytes:
    import re as _re

    text = _re.sub(rb"tmiki: \[ *\d+\.\ds\]", b"tmiki: [T]", text)
    return _re.sub(rb"\d+\.\ds", b"Ns", text) if b"tmiki: done in" in text else text


def run_transport_scenario(name: str, builder, token: str | None, steps_for, binary: Path, env: dict[str, str],
                           work: Path) -> dict:
    from tools.fake_tmihub import Hub

    base = work / f"tbase-{name}"
    run_dir = work / f"trun-{name}"
    for d in (base, run_dir):
        if d.exists():
            shutil.rmtree(d)
    hub = Hub(run_dir / "hub", token=token).start()
    try:
        url = hub.url("u", "r")
        runner = Runner(dict(env, **({"TMIHUB_TOKEN": token} if token else {})))
        runner.hub_url = url  # type: ignore[attr-defined]
        # Built where the runs happen (remote paths are stored absolute), then
        # set aside as the copy every run starts from.
        builder(runner, run_dir)
        shutil.move(str(run_dir), str(base))
        steps = steps_for(url)
        user_home = Path(env["TMI_CONFIG_HOME"])
        user_base = (user_home / "config").read_bytes()
        modes = {"python": "P", "native": "R", "alt-native-first": "RP", "alt-python-first": "PR"}
        results: dict[str, list] = {}
        forward_log = work / f"forward-{name}.log"
        native_steps: dict[str, int] = {}
        forwarded_steps: list[str] = []
        for mode, pattern in modes.items():
            if run_dir.exists():
                shutil.rmtree(run_dir)
            shutil.copytree(base, run_dir)
            (user_home / "config").write_bytes(user_base)
            hub.requests.clear()
            clock, record, tmi_index = 100, [], 0
            for step in steps:
                if callable(step):
                    step(run_dir)
                    record.append((step.__name__, None, None, transport_state(run_dir, user_home / "config")))
                    continue
                args = step["args"]
                side = pattern[tmi_index % len(pattern)]
                tmi_index += 1
                step_env = dict(env, TMI_PYTHON=sys.executable, **step["env"])
                if step["stamp"]:
                    clock += 1
                    step_env["TMI_COMMIT_TIMESTAMP"] = f"2026-03-{clock % 28 + 1:02d}T10:00:00+00:00"
                if side == "P":
                    step_env["TMI_NATIVE"] = "0"
                else:
                    step_env["TMI_FORWARD_LOG"] = str(forward_log)
                    forward_log.unlink(missing_ok=True)
                cwd = run_dir / step["cwd"]
                res = subprocess.run([str(binary), *args], cwd=cwd, env=step_env, capture_output=True, timeout=300)
                label = f"[{step['cwd'] or '.'}] {' '.join(args)}"
                if side == "R" and mode == "native":
                    log = forward_log.read_text(encoding="utf-8") if forward_log.exists() else ""
                    if log.startswith("native"):
                        native_steps[args[0]] = native_steps.get(args[0], 0) + 1
                    else:
                        forwarded_steps.append(f"{label}: {log.strip().split(chr(9), 1)[-1]}")
                requests = list(hub.requests)
                hub.requests.clear()
                wall = args[0] in ("reflog", "recover")
                stdout = _mask(res.stdout) if wall else res.stdout
                stderr = _mask_progress(_mask(res.stderr) if wall else res.stderr)
                record.append((label, (res.returncode, stdout, stderr), requests, transport_state(run_dir, user_home / "config")))
            results[mode] = record
        shutil.rmtree(run_dir, ignore_errors=True)
        (user_home / "config").write_bytes(user_base)
    finally:
        hub.stop()
    failures = []
    reference = results["python"]
    for mode, record in results.items():
        if mode == "python":
            continue
        for i, ((label, out, reqs, state), (_, ref_out, ref_reqs, ref_state)) in enumerate(zip(record, reference)):
            if out != ref_out:
                failures.append({"mode": mode, "step": i, "command": label, "what": "output",
                                 "python": [ref_out[0], ref_out[1].decode(errors="replace")[:1500], ref_out[2].decode(errors="replace")[:800]],
                                 "rust": [out[0], out[1].decode(errors="replace")[:1500], out[2].decode(errors="replace")[:800]]})
            if reqs != ref_reqs:
                failures.append({"mode": mode, "step": i, "command": label, "what": "requests",
                                 "python": ref_reqs, "rust": reqs})
            if state != ref_state:
                diff = sorted(k for k in set(state) | set(ref_state) if state.get(k) != ref_state.get(k))
                detail = {}
                for k in diff:
                    a, b = ref_state.get(k), state.get(k)
                    if isinstance(a, dict) and isinstance(b, dict):
                        detail[k] = {kk: [str(a.get(kk))[:300], str(b.get(kk))[:300]]
                                     for kk in sorted(set(a) | set(b)) if a.get(kk) != b.get(kk)}
                    else:
                        detail[k] = [str(a)[:300], str(b)[:300]]
                failures.append({"mode": mode, "step": i, "command": label, "what": "state", "differs": detail})
                break
    return {"steps": len(steps), "native": native_steps, "forwarded": forwarded_steps, "failures": failures}


def repo_state(repo: Path) -> dict:
    tmi = repo / ".tmi"
    refs = {}
    for path in sorted((tmi / "refs").rglob("*")):
        if path.is_file():
            rel = path.relative_to(tmi).as_posix()
            # stash refs are named by the wall clock; their targets are compared
            key = "refs/stash/*" + str(len(refs)) if rel.startswith("refs/stash/") else rel
            refs[key] = path.read_text(encoding="utf-8").strip()
    objects = sorted(p.parent.name + p.name for p in (tmi / "objects").glob("*/*") if p.is_file())
    return {
        "HEAD": (tmi / "HEAD").read_text(encoding="utf-8"),
        "refs": refs,
        "index": (tmi / "index").read_bytes().decode("utf-8"),
        "objects": objects,
    }


def check_floats(conform: Path, count: int) -> dict:
    rng = random.Random(0x746D69)
    values: list[float] = [0.0, -0.0, 1.0, 0.1, 1e-05, 1e-4, 1e16, 1e15, 5e-324, 1.7976931348623157e308,
                           float("inf"), float("-inf"), float("nan"), 0.5, 2.5, 0.125, 0.375, 2.675, 0.05, 0.15, 0.25, 0.35]
    values += [a / 255 for a in range(256)] + [a / 256 for a in range(257)]
    values += [round(rng.uniform(-1000, 1000), rng.randint(0, 9)) for _ in range(count // 4)]
    values += [rng.randint(-10**6, 10**6) / 10 ** rng.randint(0, 12) for _ in range(count // 4)]
    values += [10.0 ** rng.randint(-30, 30) * rng.random() for _ in range(count // 4)]
    while len(values) < count:
        bits = rng.getrandbits(64)
        x = struct.unpack("<d", struct.pack("<Q", bits))[0]
        if x == x:
            values.append(x)
    hexes = [format(struct.unpack("<Q", struct.pack("<d", x))[0], "016x") for x in values]
    out = subprocess.run([str(conform), "floats"], input="\n".join(hexes).encode(), capture_output=True, check=True)
    got = out.stdout.decode().splitlines()
    bad = []
    for x, line in zip(values, got):
        fmt1 = format(x, ".1f") if x == x and abs(x) != float("inf") else "-"
        want = f"{x!r} {json.dumps(x)} {round(x, 6)!r} {fmt1}"
        if line != want:
            bad.append({"value": repr(x), "python": want, "rust": line})
    return {"checked": len(values), "mismatches": bad[:20], "mismatch_count": len(bad)}


def check_large(conform: Path) -> dict:
    from tmi.objects import ObjectStore

    files = sorted(p for p in (ROOT / "example").rglob("*") if p.is_file())
    out = subprocess.run([str(conform), "large-oids", *map(str, files)], capture_output=True, check=True)
    rust = dict(reversed(line.split(" ", 1)) for line in out.stdout.decode("utf-8").splitlines())
    bad, chunked = [], 0
    for path in files:
        with tempfile.TemporaryDirectory() as tmp:
            store = ObjectStore(Path(tmp) / "objects")
            oid = store.write_large_blob(path.read_bytes())
            if store.read(oid)[0] == "chunked":
                chunked += 1
        if rust.get(str(path)) != oid:
            bad.append(str(path.relative_to(ROOT)))
    return {"files": len(files), "chunked": chunked, "mismatches": bad}


# ---------------------------------------------------------------------------
# Format plugins (PLUGINS.md P6): `.tmiattributes`, `.tmiplugins/`, the ORA
# test plugin (tools/build_test_plugins.py builds it, as 0.1.0 and as 0.2.0).
# Python does not run plugins, so this scenario is the executable's: every
# step runs natively twice (the outputs and the repositories must be the
# same: determinism) and once more with the steps marked "py" run by
# `python -m tmi` (the same outputs and repositories: Python keeps a
# plugin's records as they are); each step's exit code and the texts it
# must print are checked on the native run. Steps marked "python" run in
# Python in every mode and must be refused there.

def _ora_bytes(layers: list[tuple[str, tuple[int, int, int, int], tuple[int, int], int, int]],
               size: tuple[int, int] = (32, 24), deflate: bool = True, image_attrs: str = "") -> bytes:
    """An OpenRaster file (stored mimetype first, deflated stack.xml and
    layer PNGs from tmi's own encoder); `layers` bottom to top: (name,
    colour, (w, h), x, y)."""
    import io
    import zipfile

    from tmi import raster

    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as z:
        def put(name: str, data: bytes, deflate: bool) -> None:
            info = zipfile.ZipInfo(name, date_time=(1980, 1, 1, 0, 0, 0))
            info.compress_type = zipfile.ZIP_DEFLATED if deflate else zipfile.ZIP_STORED
            z.writestr(info, data)

        put("mimetype", b"image/openraster", False)
        rows = "".join(f'  <layer name="{n}" src="data/{n}.png" x="{x}" y="{y}"/>\n' for n, _, _, x, y in reversed(layers))
        put("stack.xml", f'<?xml version="1.0" encoding="UTF-8"?>\n<image w="{size[0]}" h="{size[1]}"{image_attrs}>\n <stack>\n{rows} </stack>\n</image>\n'.encode(), True)
        for n, rgba, (w, h), _, _ in layers:
            put(f"data/{n}.png", raster.png(raster.Rgba(w, h, bytes(rgba) * (w * h))), deflate)
    return buf.getvalue()


def _ora_step(rel: str, bg: tuple[int, int, int, int], fg: tuple[int, int, int, int], deflate: bool = True, fault: str = ""):
    """`fault`: the ORA test plugin's `tmi-test-write-fault` (its writer misbehaves on purpose)."""
    def step(root: Path) -> None:
        path = root / "r" / rel
        path.parent.mkdir(parents=True, exist_ok=True)
        attrs = f' tmi-test-write-fault="{fault}"' if fault else ""
        path.write_bytes(_ora_bytes([("bg", bg, (32, 24), 0, 0), ("fg", fg, (8, 6), 4, 3)], deflate=deflate, image_attrs=attrs))
    step.__name__ = f"write {rel} bg={bg[:3]} fg={fg[:3]}{'' if deflate else ' stored'}{' fault=' + fault if fault else ''}"
    return step


_remembered: dict[str, bytes] = {}


def _remember(rel: str):
    def step(root: Path) -> None:
        _remembered[rel] = (root / rel).read_bytes()
    step.__name__ = f"remember {rel}"
    return step


def _unchanged(rel: str):
    """A check: `rel` holds what `_remember` saw (a refused write left it alone)."""
    def step(root: Path) -> str | None:
        return None if (root / rel).read_bytes() == _remembered.get(rel) else f"{rel} changed"
    step.__name__ = f"check {rel} unchanged"
    return step


def _file_step(name: str, fn):
    fn.__name__ = name
    return fn


def _move(src: str, dst: str):
    def step(root: Path) -> None:
        (root / dst).parent.mkdir(parents=True, exist_ok=True)
        (root / src).replace(root / dst)
    step.__name__ = f"move {src} -> {dst}"
    return step


def _append(rel: str, text: str):
    def step(root: Path) -> None:
        with (root / rel).open("a", encoding="utf-8", newline="\n") as f:
            f.write(text)
    step.__name__ = f"append to {rel}: {text.strip()}"
    return step


def _copy_component(which: str, rel: str):
    def step(root: Path) -> None:
        (root / rel).parent.mkdir(parents=True, exist_ok=True)
        shutil.copyfile(root / f"{which}.wasm", root / rel)
    step.__name__ = f"copy {which}.wasm to {rel}"
    return step


def S(*args: str, cwd: str = "r", code: int = 0, out: tuple[str, ...] = (), err: tuple[str, ...] = (),
      py: bool = False, python: bool = False, stamp: bool = False, env: dict | None = None,
      no_out: tuple[str, ...] = ()) -> dict:
    return {"args": list(args), "cwd": cwd, "code": code, "out": out, "err": err, "py": py, "python": python,
            "stamp": stamp, "env": env or {}, "no_out": no_out}


def plugins_steps() -> list:
    v1, v2, v3 = "v1.wasm", "v2.wasm", "v3.wasm"
    not_available = "maps it to plugin 'ora', which is not available"
    refused_in_python = "format plugins run in the tmi executable only"
    return [
        # add / status / diff
        S("plugin", "add", f"../{v1}", out=("added plugin ora 0.1.0", ".tmiattributes: *.ora format=plugin:ora",
                                           "staged .tmiattributes", "staged .tmiplugins/ora.wasm"),
          err=("experimental",)),
        _ora_step("a.ora", (200, 30, 30, 255), (20, 200, 20, 255)),
        S("add", "a.ora", out=("staged a.ora",)),
        S("status", out=("staged     a.ora",)),
        S("commit", "-m", "an ora file", stamp=True, py=True),
        S("ls-layers", "a.ora", py=True, out=("fg", "bg")),
        S("log", py=True),
        S("show", "HEAD", py=True),
        S("plugin", "list", out=("builtin", "ora", "0.1.0", "*.ora", "tmi:format@0.1.0", "trusted (read)", "cached")),
        S("plugin", "-h", py=True),
        S("plugin", "list", python=True, code=1, err=("'tmiki plugin' runs in the tmi executable only",)),
        _ora_step("a.ora", (200, 30, 30, 255), (20, 20, 220, 255)),
        S("status", out=("modified   a.ora  changed layer fg",)),
        S("diff", "--stat", out=(" a.ora | 1 layer changed",)),
        S("diff", "--name-status", py=True),
        S("diff", out=("diff a.ora", "changed layer fg")),
        S("add", "a.ora", python=True, code=1, err=(refused_in_python,)),
        S("commit", "-a", "-m", "fg blue", python=True, code=1, err=(refused_in_python,), stamp=True),
        S("commit", "-a", "-m", "fg blue", stamp=True, out=("staged a.ora",)),
        S("diff", "HEAD~1", "HEAD", "--stat", out=(" a.ora | 1 layer changed",)),
        # merges: different layers on both sides (no writer: a conflict), then
        # an upgrade of the plugin on one side and an edit on the other
        S("branch", "side", py=True),
        _ora_step("a.ora", (200, 30, 30, 255), (240, 240, 20, 255)),
        S("commit", "-a", "-m", "main: fg yellow", stamp=True),
        S("switch", "side", py=True),
        _ora_step("a.ora", (30, 30, 30, 255), (20, 20, 220, 255)),
        S("commit", "-a", "-m", "side: bg dark", stamp=True),
        S("switch", "main", py=True),
        S("merge", "side", "-m", "merge side", stamp=True, code=1,
          out=("conflict a.ora", "plugin 'ora' 0.1.0 cannot write layers back, so the two sides could not be combined",
               "ours changed fg; theirs changed bg", "'tmiki restore --ours a.ora' or 'tmiki restore --theirs a.ora'",
               ".tmi/conflicts/a.ora/", "Each conflicting file still holds our version"),
          no_out=("<file>:<layer>", "already merged"), err=()),
        S("status", out=("conflict a.ora",)),
        # issue #47: their layer cannot be written into the file either
        S("restore", "--theirs", "a.ora:fg", code=1,
          err=("a.ora: plugin 'ora' 0.1.0 cannot write layers back; take the whole file with 'tmiki restore --theirs a.ora'",)),
        S("merge", "--abort"),
        S("branch", "up", py=True),
        S("switch", "up", py=True),
        # status and diff of a modified file before and after the plugin is
        # upgraded (P9: the render cache and the structure cache are keyed
        # by the component, so 0.2.0 reads the file itself), each twice (the
        # second time from the structure cache); `diff --stat` only, as a
        # full diff writes .tmi/diff/<second> and two in one second collide
        _ora_step("a.ora", (200, 30, 30, 255), (20, 200, 200, 255)),
        S("status", out=("modified   a.ora  changed layer fg",)),
        S("status", out=("modified   a.ora  changed layer fg",)),
        S("diff", "--stat", out=(" a.ora | 1 layer changed",)),
        S("diff", "--stat", out=(" a.ora | 1 layer changed",)),
        _copy_component("v2", "r/.tmiplugins/ora.wasm"),
        S("status", out=("modified   .tmiplugins/ora.wasm", "modified   a.ora  changed layer fg"),
          err=("this repository uses format plugin ora 0.2.0", "It runs in a sandbox")),
        S("status", out=("modified   .tmiplugins/ora.wasm", "modified   a.ora  changed layer fg")),
        S("diff", "--stat", out=(" a.ora | 1 layer changed (importer changed ora 0.1.0 -> ora 0.2.0)",)),
        S("diff", "--stat", out=(" a.ora | 1 layer changed (importer changed ora 0.1.0 -> ora 0.2.0)",)),
        S("restore", "a.ora"),
        S("add", ".tmiplugins/ora.wasm", out=("staged .tmiplugins/ora.wasm",)),
        S("status", out=("staged     .tmiplugins/ora.wasm",)),
        S("plugin", "reimport", "ora", out=("staged a.ora", "reimported 1 file(s) with plugin ora (1 changed)")),
        S("diff", "--staged", out=("diff a.ora", "importer changed ora 0.1.0 -> ora 0.2.0")),
        S("commit", "-m", "plugin 0.2.0", stamp=True, py=True),
        # the same layers in other bytes (stored members)
        _ora_step("a.ora", (200, 30, 30, 255), (240, 240, 20, 255), deflate=False),
        S("status", out=("modified   a.ora",)),
        S("commit", "-a", "-m", "up: stored members", stamp=True),
        S("switch", "main", py=True),
        _ora_step("a.ora", (200, 30, 30, 255), (250, 120, 0, 255)),
        S("commit", "-a", "-m", "main: fg orange", stamp=True),
        S("merge", "up", "-m", "merge up", stamp=True, out=("Merged",)),
        S("ls-layers", "a.ora", py=True),
        S("diff", "HEAD~1", "HEAD", out=("importer changed ora 0.1.0 -> ora 0.2.0",)),
        # .tmiattributes changes: nothing is modified until it is re-imported
        _append("r/.tmiattributes", "*.ora format=binary\n"),
        S("status", out=("modified   .tmiattributes",)),
        S("add", "--renormalize", "a.ora", out=("staged a.ora",)),
        S("diff", "--staged", "--stat", out=(" a.ora | binary",)),
        S("restore", "--staged", "a.ora"),
        S("restore", ".tmiattributes"),
        S("add", "--renormalize", out=()),
        S("status", out=("nothing to commit, working tree clean",)),
        # the plugin missing
        _ora_step("a.ora", (10, 10, 10, 255), (250, 120, 0, 255)),
        _move("r/.tmiplugins/ora.wasm", "aside.wasm"),
        S("status", out=("modified   a.ora  (layer details need plugin 'ora')",)),
        S("add", "a.ora", code=1, err=(not_available, ".tmiplugins/ora.wasm is missing")),
        S("commit", "-a", "-m", "x", code=1, err=(not_available,), stamp=True),
        S("diff", out=("diff a.ora", "layer details need plugin 'ora'", "binary file changed")),
        S("diff", "HEAD~2", "HEAD", "--stat", out=(" a.ora | 1 layer changed (importer changed ora 0.1.0 -> ora 0.2.0)",)),
        S("log", "-n", "2", py=True),
        S("ls-layers", "a.ora", code=1, err=(not_available,)),
        S("mv", "a.ora", "b.ora", code=1, err=(not_available,)),
        S("stash", "push", "-m", "wip", code=1, err=(not_available,), stamp=True),
        S("restore", "--source", "HEAD", "a.ora"),
        S("status", out=("staged     .tmiplugins/ora.wasm  delete",)),
        _move("aside.wasm", "r/.tmiplugins/ora.wasm"),
        # trust
        S("config", "plugin.trust", "deny", py=True),
        _ora_step("a.ora", (10, 10, 10, 255), (250, 120, 0, 255)),
        S("add", "a.ora", code=1, err=("not trusted: plugin.trust is deny",)),
        S("config", "--unset", "plugin.trust", py=True),
        S("plugin", "trust", "ora", out=("plugin ora 0.2.0", "may now read and write files of this repository")),
        S("plugin", "list", out=("trusted (read, write)",)),
        S("restore", "a.ora"),
        # clone and fetch: the clone compiles the plugin at its end (an empty
        # cache) and tells its notice; fetch runs no plugin
        S("clone", "r", "c", cwd=".", env={"TMI_PLUGIN_CACHE": "{root}/cache-clone"},
          err=("compiling plugin ora ...", "this repository uses format plugin ora 0.2.0",
               "tmiki config --global plugin.trust prompt")),
        S("plugin", "prepare", cwd="c", out=("plugin ora 0.2.0 ready",), env={"TMI_PLUGIN_CACHE": "{root}/cache-clone"}),
        S("ls-layers", "a.ora", cwd="c", py=True),
        _ora_step("a.ora", (10, 10, 10, 255), (250, 250, 250, 255)),
        S("commit", "-a", "-m", "main: fg white", stamp=True),
        S("fetch", cwd="c", out=("fetched",), env={"TMI_PLUGIN_CACHE": "{root}/cache-fetch"}),
        S("status", cwd="c", out=("nothing to commit",)),
        # P7: writing layers back through the plugin's writer (0.3.0). A new
        # component needs the user's consent to write, which a run without
        # a terminal cannot give: refused, nothing changed
        S("plugin", "add", f"../{v3}", out=("added plugin ora 0.3.0", "staged .tmiplugins/ora.wasm")),
        S("commit", "-m", "plugin 0.3.0", stamp=True, py=True),
        _remember("r/a.ora"),
        S("restore", "--source", "HEAD~1", "--in-place", "a.ora:fg", code=1,
          err=("a.ora: writing through plugin 'ora' 0.3.0 needs your consent once for this repository; run 'tmiki plugin trust ora'",)),
        _unchanged("r/a.ora"),
        # merges: different layers on both sides, merged by the writer
        S("branch", "w-side", py=True),
        _ora_step("a.ora", (90, 90, 250, 255), (250, 250, 250, 255)),
        S("commit", "-a", "-m", "main: bg blue", stamp=True),
        S("switch", "w-side", py=True),
        _ora_step("a.ora", (10, 10, 10, 255), (250, 250, 0, 255)),
        S("commit", "-a", "-m", "w-side: fg yellow", stamp=True),
        S("switch", "main", py=True),
        S("merge", "w-side", "-m", "merge w-side", stamp=True, code=1,
          err=("a.ora: writing through plugin 'ora' 0.3.0 needs your consent once",)),
        S("status", out=("nothing to commit, working tree clean",), no_out=("middle of a merge",)),
        S("plugin", "trust", "ora", out=("plugin ora 0.3.0", "may now read and write files of this repository")),
        S("merge", "w-side", "-m", "merge w-side", stamp=True, out=("Merged w-side",)),
        S("ls-layers", "a.ora", py=True),
        S("diff", "HEAD~1", "HEAD", "--stat", out=(" a.ora | 1 layer changed",)),
        S("status", out=("nothing to commit",)),
        # restore --in-place: a layer of HEAD~1 into the working file
        S("restore", "--source", "HEAD~1", "--in-place", "a.ora:fg", out=("restored",)),
        S("status", out=("modified   a.ora  changed layer fg",)),
        S("diff", "--stat", out=(" a.ora | 1 layer changed",)),
        S("ls-layers", "a.ora", out=("fg", "bg")),
        # the round trip: the same pixels again give the same bytes
        _remember("r/a.ora"),
        S("restore", "--source", "HEAD~1", "--in-place", "a.ora:fg", out=("restored",)),
        _unchanged("r/a.ora"),
        # HEAD's own pixels back: the very bytes the merge wrote (the writer
        # is deterministic)
        S("restore", "--source", "HEAD", "--in-place", "a.ora:fg", out=("restored",)),
        S("status", out=("nothing to commit, working tree clean",)),
        # one layer staged: written into HEAD's file, the working file untouched
        _ora_step("a.ora", (60, 160, 60, 255), (200, 0, 200, 255)),
        S("add", "a.ora:fg", out=("staged a.ora",)),
        S("diff", "--staged", "--stat", out=(" a.ora | 1 layer changed",)),
        S("commit", "-m", "fg only", stamp=True),
        S("status", out=("modified   a.ora  changed layer bg",)),
        S("add", "a.ora:bg", out=("staged a.ora",)),
        S("commit", "-m", "bg too", stamp=True),
        # the same layers, but HEAD holds the writer's bytes
        S("status", out=("modified   a.ora",), no_out=("changed layer",)),
        S("restore", "a.ora"),
        S("status", out=("nothing to commit",)),
        # the same layer on both sides: a layer conflict; their layer taken
        S("branch", "w-side2", py=True),
        _ora_step("a.ora", (60, 160, 60, 255), (0, 250, 250, 255)),
        S("commit", "-a", "-m", "main: fg cyan", stamp=True),
        S("switch", "w-side2", py=True),
        _ora_step("a.ora", (60, 160, 60, 255), (250, 120, 0, 255)),
        S("commit", "-a", "-m", "w-side2: fg orange", stamp=True),
        S("switch", "main", py=True),
        S("merge", "w-side2", "-m", "merge w-side2", stamp=True, code=1,
          out=("conflict a.ora", "fg: overlapping pixel edits", "tmiki restore --theirs <file>:<layer>")),
        S("restore", "--theirs", "a.ora:fg", out=("took theirs for a.ora:fg",)),
        S("add", "a.ora", out=("staged a.ora",)),
        S("commit", "-m", "merge w-side2", stamp=True),
        S("ls-layers", "a.ora", py=True),
        # writers that break the rules: the check after writing stops them
        _ora_step("a.ora", (60, 160, 60, 255), (250, 120, 0, 255), fault="other-layer"),
        S("commit", "-a", "-m", "a writer that also writes another layer", stamp=True),
        _remember("r/a.ora"),
        S("restore", "--source", "HEAD~1", "--in-place", "a.ora:fg", code=1,
          err=("a.ora: plugin 'ora' 0.3.0 could not write layers back", "layer bg (ora:0) changed although it was not edited")),
        _unchanged("r/a.ora"),
        _ora_step("a.ora", (60, 160, 60, 255), (250, 120, 0, 255), fault="bad-span"),
        S("commit", "-a", "-m", "a writer with a piece past the end", stamp=True),
        _remember("r/a.ora"),
        S("restore", "--source", "HEAD~1", "--in-place", "a.ora:fg", code=1,
          err=("the writer's plan is invalid", "is outside the")),
        _unchanged("r/a.ora"),
        S("status", out=("nothing to commit",)),
        # issues #49, #51: a folder with a file the plugin cannot read (cut
        # short, as by an interrupted save) and one it reads: the second is
        # staged, the first listed, exit 1
        _file_step("write bad/good.ora and bad/cut.ora (its first 300 bytes)", _bad_folder),
        S("add", "bad", code=1, out=("staged bad/good.ora",),
          err=("tmiki: error: 1 file could not be added:",
               "  bad/cut.ora: plugin 'ora' 0.3.0 could not read it: the file is damaged (the archive is cut short",
               "tmiki: hint: with on-error=binary in .tmiattributes, a file the plugin cannot read is kept as an opaque blob instead")),
        S("status", out=("staged     bad/good.ora", "untracked  bad/cut.ora")),
        # issue #50: `rm` of the component warns (Python says the same);
        # without it the error names both ways out; `plugin remove` stops
        # using the plugin
        S("rm", ".tmiplugins/ora.wasm", py=True, out=("removed .tmiplugins/ora.wasm",),
          err=("warning: .tmiattributes still maps files to plugin 'ora' (2 tracked files); without .tmiplugins/ora.wasm "
               "they cannot be added or committed. To stop using the plugin, run 'tmiki plugin remove ora'",)),
        _ora_step("a.ora", (10, 10, 10, 255), (90, 90, 90, 255)),
        S("add", "a.ora", code=1, err=("'tmiki plugin add <file.wasm>' puts it back, 'tmiki plugin remove ora' stops using it",)),
        S("ls-layers", "a.ora", code=1, err=("'tmiki ls-layers a.ora HEAD' lists its layers as last committed",)),
        S("ls-layers", "a.ora", "HEAD", py=True, out=("fg", "bg")),
        S("plugin", "remove", "ora", out=(".tmiattributes: removed '*.ora format=plugin:ora'", "staged .tmiattributes",
                                          "staged a.ora", "staged bad/good.ora",
                                          "plugin ora is no longer used here; 2 tracked files it read are recorded again without it")),
        S("diff", "--staged", "--stat", out=(" a.ora | binary",)),
        S("add", "bad", out=("staged bad/cut.ora",)),
        S("commit", "-m", "no ora plugin", stamp=True, py=True),
        S("ls-layers", "a.ora", "HEAD~1", py=True, out=("fg", "bg")),
        S("plugin", "remove", "ora", code=1, err=("plugin ora is not used in this repository",)),
        # in the clone, where the component is there: it is removed too
        S("plugin", "remove", "ora", cwd="c", out=("removed .tmiplugins/ora.wasm", ".tmiattributes: removed '*.ora format=plugin:ora'",
                                                   "staged .tmiattributes", "staged a.ora",
                                                   "plugin ora is no longer used here; 1 tracked file it read is recorded again without it")),
        S("status", cwd="c", out=("staged     .tmiplugins/ora.wasm  delete", "staged     a.ora")),
        # P8: the plugin author's tools. install keeps a component in the
        # user's store (TMI_PLUGIN_HOME), `plugin add <id>` takes it from there
        *[S("plugin", sub, "-h", py=True) for sub in ("new", "build", "test", "doctor", "install", "add")],
        S("plugin", "install", f"../{v3}", python=True, code=1, err=("'tmiki plugin' runs in the tmi executable only",),
          env={"TMI_PLUGIN_HOME": "{root}/plugin-home"}),
        S("plugin", "install", f"../{v3}", out=("installed plugin ora 0.3.0", "plugin-home", "'tmiki plugin add ora' puts it there"),
          env={"TMI_PLUGIN_HOME": "{root}/plugin-home"}),
        S("plugin", "add", "kra", code=1, err=("no plugin kra is installed",), env={"TMI_PLUGIN_HOME": "{root}/plugin-home"}),
        S("plugin", "add", "ora@0.1.0", code=1, err=("plugin ora is installed as 0.3.0 only",),
          env={"TMI_PLUGIN_HOME": "{root}/plugin-home"}),
        S("plugin", "add", "ora", out=("added plugin ora 0.3.0", ".tmiattributes: *.ora format=plugin:ora",
                                       "staged .tmiattributes", "staged .tmiplugins/ora.wasm"),
          env={"TMI_PLUGIN_HOME": "{root}/plugin-home"}),
        S("plugin", "list", out=("ora", "0.3.0", "*.ora")),
        # `new` writes a plugin directory outside any repository; plugins are
        # written in Rust (PLUGINS.md Q14): another --lang is refused
        S("plugin", "new", "demo", "--lang", "rust", cwd=".", out=("created demo/ (rust plugin demo, tmi:format@0.1.0, world format-reader)",)),
        S("plugin", "new", "demo", cwd=".", code=1, err=("already exists and is not empty",)),
        S("plugin", "new", "demo-py", "--lang", "python", cwd=".", code=1,
          err=("tmi format plugins are written in Rust: there is no python template",)),
        S("plugin", "new", "demo-js", "--lang", "js", cwd=".", code=1, err=("tmi format plugins are written in Rust: there is no js template",)),
        S("plugin", "test", f"{v3}", cwd=".", code=1, err=("a component on its own needs --corpus <dir>",)),
    ]


def _bad_folder(root: Path) -> None:
    good = _ora_bytes([("bg", (1, 2, 3, 255), (32, 24), 0, 0), ("fg", (4, 5, 6, 255), (8, 6), 4, 3)])
    (root / "r" / "bad").mkdir(parents=True, exist_ok=True)
    (root / "r" / "bad" / "good.ora").write_bytes(good)
    (root / "r" / "bad" / "cut.ora").write_bytes(good[:300])


def _dev_record(rel: str, dev: bool):
    """A check: the index records `rel` with (or without) a development
    plugin (`importer.component_sha256 == "dev"`)."""
    def step(root: Path) -> str | None:
        entry = json.loads((root / "r" / ".tmi" / "index").read_text(encoding="utf-8"))["entries"].get(rel, {})
        got = (entry.get("importer") or {}).get("component_sha256") == "dev"
        return None if got == dev else f"{rel}: component_sha256 {'is not' if dev else 'is'} \"dev\""
    step.__name__ = f"check {rel} {'is' if dev else 'is not'} a dev record"
    return step


def plugin_dev_steps() -> list:
    """P8: the ORA plugin as a development plugin (`ora-dev`, the same code on
    tmi-plugin-sdk) next to its component (v3): `tmiki plugin dev`, its
    records, the commits that refuse them, and write-back through it
    writing the component's bytes."""
    exe = ".exe" if os.name == "nt" else ""
    dev = f"../ora-dev{exe}"
    dev_env = {"TMI_PLUGIN_DEV": f"{{root}}/ora-dev{exe}"}
    refused = ("a.ora: recorded with a development plugin (tmiki plugin dev); a commit cannot hold such records. "
               "Add it again without TMI_PLUGIN_DEV first")
    return [
        S("plugin", "add", "../v3.wasm", out=("added plugin ora 0.3.0",), err=("experimental",)),
        S("commit", "-m", "the plugin", stamp=True, py=True),
        _ora_step("a.ora", (200, 30, 30, 255), (20, 200, 20, 255)),
        S("plugin", "dev", dev, out=("development plugin ora 0.3.0 (OpenRaster: .ora; reads and writes layers), tmi:format@0.1.0",
                                     ".tmiattributes: *.ora files are read with it", 'its records carry component_sha256 "dev"')),
        S("plugin", "dev", "-h", py=True, out=("usage: tmiki plugin dev",)),
        S("plugin", "dev", dev, "--", "add", "a.ora", python=True, code=1, err=("'tmiki plugin' runs in the tmi executable only",)),
        S("plugin", "dev", dev, "--", "add", "a.ora", out=("staged a.ora",)),
        _dev_record("a.ora", True),
        S("status", env=dev_env, out=("a.ora",)),
        S("commit", "-m", "dev records", stamp=True, code=1, err=(refused,)),
        S("commit", "-m", "dev records", stamp=True, python=True, code=1, err=(refused,)),
        S("commit", "-a", "-m", "dev records", stamp=True, env=dev_env, code=1, err=(refused,)),
        S("stash", stamp=True, env=dev_env, code=1, err=(refused,)),
        # the component again: its record, which commits
        S("add", "a.ora", out=("staged a.ora",)),
        _dev_record("a.ora", False),
        S("commit", "-m", "an ora file", stamp=True),
        S("plugin", "trust", "ora", out=("may now read and write files of this repository",)),
        _ora_step("a.ora", (200, 30, 30, 255), (20, 20, 220, 255)),
        S("commit", "-a", "-m", "fg blue", stamp=True, out=("staged a.ora",)),
        # write-back: through the component, then through the dev plugin: the same bytes
        S("restore", "--source", "HEAD~1", "--in-place", "a.ora:fg", out=("restored",)),
        _remember("r/a.ora"),
        S("restore", "a.ora"),
        S("plugin", "dev", dev, "--", "restore", "--source", "HEAD~1", "--in-place", "a.ora:fg", out=("restored",)),
        _unchanged("r/a.ora"),
        S("restore", "a.ora"),
        # one layer staged through the dev plugin: its record, refused by commit
        _ora_step("a.ora", (60, 160, 60, 255), (200, 0, 200, 255)),
        S("plugin", "dev", dev, "--", "add", "a.ora:fg", out=("staged a.ora",)),
        _dev_record("a.ora", True),
        S("commit", "-m", "fg through dev", stamp=True, code=1, err=(refused,)),
        S("add", "a.ora:fg", out=("staged a.ora",)),
        S("commit", "-m", "fg", stamp=True),
        # a program that does not start: the file is not read another way
        S("plugin", "dev", f"../no-such-plugin{exe}", "--", "add", "a.ora", code=1,
          err=("the development plugins of TMI_PLUGIN_DEV cannot be used", "cannot run the development plugin")),
        S("status", out=("modified   a.ora  changed layer bg",)),
        S("restore", "a.ora"),
    ]


def _plugins_state(root: Path, user_home: Path) -> dict:
    state = {}
    for name in ("r", "c"):
        if (root / name / ".tmi").is_dir():
            state[name] = write_state(root / name, user_home / "config")
    trust = user_home / "trust.json"
    state["trust"] = trust.read_bytes() if trust.exists() else None
    return state


def run_plugins_scenario(binary: Path, env: dict[str, str], work: Path, dev: bool = False) -> dict:
    """The `plugins` scenario, or with `dev` the `plugin-dev` one (P8)."""
    sys.path.insert(0, str(ROOT / "tools"))
    import build_test_plugins

    # 0.1.0 and 0.2.0 read only (no writer: issue #47's conflicts); 0.3.0 writes (P7)
    components = {"v1": build_test_plugins.ora("0.1.0", read_only=True), "v2": build_test_plugins.ora("0.2.0", read_only=True),
                  "v3": build_test_plugins.ora()}
    programs = {}
    if dev:
        ora_dev = build_test_plugins.ora_dev()
        programs[ora_dev.name] = ora_dev
    # (P8 ran it a fourth time with the same plugin written in Python; P9
    # took Python plugins out, PLUGINS.md Q14)
    modes = ["native", "native-again", "alternating"]
    steps = plugin_dev_steps() if dev else plugins_steps()
    user_home = Path(env["TMI_CONFIG_HOME"])
    user_base = (user_home / "config").read_bytes()
    root = work / ("plugin-dev-run" if dev else "plugins-run")
    results: dict[str, list] = {}
    failures: list[dict] = []
    native: dict[str, int] = {}
    for mode in modes:
        if root.exists():
            shutil.rmtree(root)
        root.mkdir(parents=True)
        for name, path in components.items():
            shutil.copyfile(path, root / f"{name}.wasm")
        for name, path in programs.items():
            shutil.copyfile(path, root / name)
        (user_home / "config").write_bytes(user_base)
        (user_home / "trust.json").unlink(missing_ok=True)
        # every structure is kept (not only those that took long: a step's
        # .tmi/cache must not depend on the machine's speed)
        base_env = dict(env, TMI_PYTHON=sys.executable, TMI_PLUGIN_CACHE=str(root / "cache"), TMI_STRUCTURE_CACHE_MIN_MS="0")
        # the repository the steps start from: one PNG, committed
        r = root / "r"
        r.mkdir()
        Runner(base_env).tmi(r, "init")
        _png(r / "a.png", (90, 90, 200, 255))
        Runner(base_env).tmi(r, "add", "a.png")
        Runner(base_env).tmi(r, "commit", "-m", "base", stamp=True)
        clock = 100
        record = []
        for i, step in enumerate(steps):
            if callable(step):
                problem = step(root)
                if problem:
                    failures.append({"mode": mode, "step": i, "command": step.__name__, "what": "check", "problem": problem})
                record.append((step.__name__, None, _plugins_state(root, user_home)))
                continue
            step_env = dict(base_env)
            step_env.update({k: v.replace("{root}", str(root)) for k, v in step["env"].items()})
            if step["stamp"]:
                clock += 1
                step_env["TMI_COMMIT_TIMESTAMP"] = f"2026-03-{clock % 28 + 1:02d}T10:00:00+00:00"
            in_python = step["python"] or (mode == "alternating" and step["py"])
            if in_python:
                step_env["TMI_NATIVE"] = "0"
            else:
                forward_log = root / "forward.log"
                step_env["TMI_FORWARD_LOG"] = str(forward_log)
                forward_log.unlink(missing_ok=True)
            cwd = root if step["cwd"] == "." else root / step["cwd"]
            res = subprocess.run([str(binary), *step["args"]], cwd=cwd, env=step_env, capture_output=True, timeout=300)
            label = " ".join(step["args"])
            stdout = _re_stamp.sub(b"<STAMP>", res.stdout) if step["args"][:1] == ["diff"] else res.stdout
            if mode == "native" and not in_python:
                log = (root / "forward.log").read_text(encoding="utf-8") if (root / "forward.log").exists() else ""
                key = step["args"][0]
                if log.startswith("native"):
                    native[key] = native.get(key, 0) + 1
                else:
                    failures.append({"mode": mode, "step": i, "command": label, "what": "forwarded", "log": log.strip()})
            if mode == "native" or step["python"]:
                text_out, text_err = res.stdout.decode("utf-8", "replace"), res.stderr.decode("utf-8", "replace")
                missing = [s for s in step["out"] if s not in text_out] + [s for s in step["err"] if s not in text_err]
                missing += [f"(not) {s}" for s in step["no_out"] if s in text_out]
                if res.returncode != step["code"] or missing:
                    failures.append({"mode": mode, "step": i, "command": label, "what": "expected",
                                     "code": [step["code"], res.returncode], "missing": missing,
                                     "stdout": text_out[:1500], "stderr": text_err[:1500]})
            after = _plugins_state(root, user_home)
            if step["python"] and record:
                # refused before it wrote anything: no object, index or ref
                before = record[-1][2]
                changed = [k for k in ("index", "objects", "refs", "HEAD") if before["r"].get(k) != after["r"].get(k)]
                if changed:
                    failures.append({"mode": mode, "step": i, "command": label, "what": "python wrote", "differs": changed})
            record.append((label, (res.returncode, stdout, res.stderr), after))
        results[mode] = record
    transcript = os.environ.get("TMI_CONFORMANCE_PLUGINS_LOG")
    if transcript:  # every native step's output, to read
        lines = []
        for label, out, _ in results["native"]:
            lines.append(f"$ {label}" + ("" if out is None else f"  [{out[0]}]"))
            if out is not None:
                lines += [out[1].decode("utf-8", "replace").rstrip(), out[2].decode("utf-8", "replace").rstrip()]
        Path(transcript).write_text("\n".join(l for l in lines if l) + "\n", encoding="utf-8")
    (user_home / "config").write_bytes(user_base)
    (user_home / "trust.json").unlink(missing_ok=True)
    reference = results["native"]
    for mode in ("native-again", "alternating"):
        for i, ((label, out, state), (_, ref_out, ref_state)) in enumerate(zip(results[mode], reference)):
            if out != ref_out:
                failures.append({"mode": mode, "step": i, "command": label, "what": "output",
                                 "native": [ref_out[0], ref_out[1].decode(errors="replace")[:1500], ref_out[2].decode(errors="replace")[:800]],
                                 "other": [out[0], out[1].decode(errors="replace")[:1500], out[2].decode(errors="replace")[:800]]})
            if state != ref_state:
                diff = {}
                for repo_name in sorted(set(state) | set(ref_state)):
                    a, b = ref_state.get(repo_name), state.get(repo_name)
                    if a != b:
                        diff[repo_name] = sorted(k for k in (a or {}) if (a or {}).get(k) != (b or {}).get(k)) if isinstance(a, dict) else "differs"
                failures.append({"mode": mode, "step": i, "command": label, "what": "state", "differs": diff})
                break
    shutil.rmtree(root, ignore_errors=True)
    return {"steps": len(steps), "native": native, "failures": failures, "runs": len(modes)}


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--bin", type=Path, default=ROOT / "rust" / "target" / "release" / f"tmi{EXE}")
    parser.add_argument("--conform", type=Path, default=ROOT / "rust" / "target" / "release" / f"tmi-conform{EXE}")
    parser.add_argument("--work", type=Path, help="scratch directory (default: a temporary one)")
    parser.add_argument("--keep", action="store_true", help="keep the scratch directory")
    parser.add_argument("--floats", type=int, default=200_000, help="random doubles to compare")
    parser.add_argument("--large", action="store_true", help="also compare write_large_blob over example/")
    parser.add_argument("--json", type=Path, help="write the full report here")
    parser.add_argument("--writes-only", action="store_true", help="only the write-command scenarios")
    parser.add_argument("--no-writes", action="store_true", help="skip the write-command scenarios")
    parser.add_argument("--scenario", action="append", help="write scenario(s) to run (default: all)")
    args = parser.parse_args()
    for exe in (args.bin, args.conform):
        if not exe.exists():
            raise SystemExit(f"missing {exe}: cargo build --release -p tmi-cli -p tmi-conform (in rust/)")
    work = args.work or Path(tempfile.mkdtemp(prefix="tmi-conformance-"))
    work.mkdir(parents=True, exist_ok=True)
    user_home = work / "userconfig"
    user_home.mkdir(exist_ok=True)
    (user_home / "config").write_text(
        json.dumps({"version": 1, "user": {"name": "Global Name", "email": "g@example.com"}, "core": {"x": 1.5}},
                   indent=2) + "\n", encoding="utf-8")
    env = dict(os.environ)
    # parity mode: the executable hands over what it does not serve
    env.setdefault("TMI_PYTHON", sys.executable)
    env.update({
        "TMI_AUTHOR_NAME": "Tester", "TMI_AUTHOR_EMAIL": "tester@example.com", "TMI_CONFIG_HOME": str(user_home),
        "PYTHONPATH": str(ROOT) + os.pathsep + env.get("PYTHONPATH", ""),
    })
    env.pop("TMI_COMMIT_TIMESTAMP", None)
    report: dict = {"work": str(work)}
    failed = False
    started = time.perf_counter()

    if not args.writes_only:
        repos: dict[str, Path] = {}
        states = []
        for run_name in ("a", "b"):
            repo = work / f"main-{run_name}"
            if repo.exists():
                shutil.rmtree(repo)
            scenario_main(Runner(env), repo)
            states.append(repo_state(repo))
        repos["main"] = work / "main-a"
        same = states[0] == states[1]
        report["deterministic"] = {"same": same, "objects": len(states[0]["objects"]), "refs": len(states[0]["refs"])}
        failed |= not same
        detached = work / "detached"
        if detached.exists():
            shutil.rmtree(detached)
        shutil.copytree(work / "main-b", detached)
        Runner(env).tmi(detached, "stash", "clear")
        Runner(env).tmi(detached, "restore", "--staged", "art/staged.mdp")
        Runner(env).tmi(detached, "checkout", "v1", "--discard")
        repos["detached"] = detached
        for name, scenario in (("conflict", scenario_conflict), ("empty", scenario_empty)):
            repo = work / name
            if repo.exists():
                shutil.rmtree(repo)
            scenario(Runner(env), repo)
            repos[name] = repo
        print(f"scenarios built in {time.perf_counter() - started:.1f}s; deterministic: {same}")

        report["commands"] = {}
        for name, repo in repos.items():
            result = compare_commands(repo, args.bin, env)
            report["commands"][name] = result
            failed |= bool(result["failures"])
            print(f"{name}: {len(result['native'])} native, {len(result['forwarded'])} forwarded, "
                  f"{len(result['failures'])} differ")
            for f in result["failures"]:
                print(f"  DIFF tmi {f['command']}\n    python: {f['python']}\n    rust:   {f['rust']}")

        writes = compare_writes(work, args.bin, env)
        report["writes"] = writes
        failed |= bool(writes["output_diffs"]) or not all(writes["same_state"].values())
        print(f"writes: {writes['steps']} steps, {len(writes['native_steps'])} native in the Rust run "
              f"({', '.join(sorted(set(writes['native_steps'])))}); {writes['objects']} objects; "
              f"same state {writes['same_state']}; {len(writes['output_diffs'])} outputs differ")
        for d in writes["output_diffs"]:
            print(f"  DIFF tmi {d['step']}\n    python: {d['python']}\n    rust:   {d['rust']}")
        repos["writes"] = work / "writes-rust"

        report["rehash"] = {}
        for name, repo in [*repos.items(), ("main-b", work / "main-b")]:
            out = subprocess.run([str(args.conform), "rehash", str(repo / ".tmi")], capture_output=True, check=True)
            result = json.loads(out.stdout)
            report["rehash"][name] = result
            failed |= bool(result["mismatches"])
            print(f"rehash {name}: {result['objects']} objects {result['by_type']}, "
                  f"{result['files_byte_equal']} files byte-equal, {result['json_reserialised']} JSON re-serialised, "
                  f"{result['large_blobs_rechunked']} large blobs re-chunked, files {result['json_files']}, "
                  f"mismatches {result['mismatches']}")

        report["floats"] = check_floats(args.conform, args.floats)
        failed |= bool(report["floats"]["mismatch_count"])
        print(f"floats: {report['floats']['checked']} checked, {report['floats']['mismatch_count']} differ "
              f"{report['floats']['mismatches'][:3]}")
        if args.large:
            report["large"] = check_large(args.conform)
            failed |= bool(report["large"]["mismatches"])
            print(f"large blobs: {report['large']['files']} example files ({report['large']['chunked']} chunked), "
                  f"mismatches {report['large']['mismatches']}")
    if not args.no_writes:
        report["writes"] = {}
        wanted = set(args.scenario or [])
        for name, builder, steps, extra in [*((*s, {}) for s in [*WRITE_SCENARIOS, *GAP_SCENARIOS, *UNSAFE_SCENARIOS]), *ENV_SCENARIOS]:
            if wanted and name not in wanted:
                continue
            t0 = time.perf_counter()
            result = run_write_scenario(name, builder, steps, args.bin, dict(env, **extra), work)
            report["writes"][name] = result
            failed |= bool(result["failures"])
            native = sum(result["native"].values())
            print(f"writes {name}: {result['steps']} steps x 4 runs, native {native} {result['native']}, "
                  f"forwarded {len(result['forwarded'])}, {len(result['failures'])} differ "
                  f"({time.perf_counter() - t0:.1f}s)")
            for item in result["forwarded"]:
                print(f"    forwarded: {item}")
            for f in result["failures"][:8]:
                print(f"  DIFF [{f['mode']}] step {f['step']} tmi {f['command']} ({f['what']})")
                for key in ("python", "rust", "differs"):
                    if key in f:
                        print(f"    {key}: {f[key]}")
        if not wanted or "plugins" in wanted:
            t0 = time.perf_counter()
            result = run_plugins_scenario(args.bin, env, work)
            report["writes"]["plugins"] = result
            failed |= bool(result["failures"])
            print(f"writes plugins: {result['steps']} steps x {result['runs']} runs, native {sum(result['native'].values())} "
                  f"{result['native']}, {len(result['failures'])} differ ({time.perf_counter() - t0:.1f}s)")
            for f in result["failures"][:12]:
                print(f"  DIFF [{f['mode']}] step {f['step']} tmi {f['command']} ({f['what']})")
                for key in ("code", "missing", "stdout", "stderr", "native", "other", "differs", "log"):
                    if key in f:
                        print(f"    {key}: {f[key]}")
        if not wanted or "plugin-dev" in wanted:
            t0 = time.perf_counter()
            result = run_plugins_scenario(args.bin, env, work, dev=True)
            report["writes"]["plugin-dev"] = result
            failed |= bool(result["failures"])
            print(f"writes plugin-dev: {result['steps']} steps x 3 runs, native {sum(result['native'].values())} "
                  f"{result['native']}, {len(result['failures'])} differ ({time.perf_counter() - t0:.1f}s)")
            for f in result["failures"][:12]:
                print(f"  DIFF [{f['mode']}] step {f['step']} tmi {f['command']} ({f['what']})")
                for key in ("code", "missing", "stdout", "stderr", "native", "other", "differs", "log"):
                    if key in f:
                        print(f"    {key}: {f[key]}")
        for name, builder, token, steps_for in TRANSPORT_SCENARIOS:
            if wanted and name not in wanted:
                continue
            t0 = time.perf_counter()
            result = run_transport_scenario(name, builder, token, steps_for, args.bin, env, work)
            report["writes"][name] = result
            failed |= bool(result["failures"])
            native = sum(result["native"].values())
            print(f"{name}: {result['steps']} steps x 4 runs, native {native} {result['native']}, "
                  f"forwarded {len(result['forwarded'])}, {len(result['failures'])} differ "
                  f"({time.perf_counter() - t0:.1f}s)")
            for item in result["forwarded"]:
                print(f"    forwarded: {item}")
            for f in result["failures"][:8]:
                print(f"  DIFF [{f['mode']}] step {f['step']} tmi {f['command']} ({f['what']})")
                for key in ("python", "rust", "differs"):
                    if key in f:
                        print(f"    {key}: {f[key]}")
    if args.json:
        args.json.write_text(json.dumps(report, ensure_ascii=False, indent=2), encoding="utf-8")
    if not args.keep and not args.work:
        shutil.rmtree(work, ignore_errors=True)
    print("conformance:", "FAIL" if failed else "ok", f"({time.perf_counter() - started:.1f}s)")
    return 1 if failed else 0


if __name__ == "__main__":
    raise SystemExit(main())
