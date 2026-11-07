"""Time diff --html, a CLIP layer merge and a conflict resolution: `python -m tmi`
against the Rust `tmi` (RUST_MIGRATION.md 4a).

    python tools/bench_diffmerge.py [--work DIR] [-n 3]

Template repositories are built once (in --work, reused) with `python -m tmi`;
every timed run works on a fresh copy (the copy is not timed):

- `diff --html --no-open --out o`: tama.clip committed, tama_2.clip in the
  working tree; lolita page0001 committed, page0002 in the working tree;
- `merge other`: tama.clip, the two branches edit different layers (the
  merged file is written back and imported);
- the conflict of both branches editing the same layer: `restore --theirs
  tama.clip:<layer>`, then `add tama.clip` and `commit -m`.

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
LOLITA = ROOT / "example" / "additional_examples" / "extracted" / "lolita-ja_EX" / "lolita-ja_EX"


def _tmi(repo: Path, env: dict[str, str], *args: str, check: bool = True) -> None:
    subprocess.run([sys.executable, "-m", "tmi", *args], cwd=repo, env=env, check=check, capture_output=True)


def _diff_template(work: Path, env: dict[str, str], name: str, old: Path, new: Path) -> Path:
    repo = work / f"diff-{name}"
    if repo.exists():
        return repo
    repo.mkdir(parents=True)
    _tmi(repo, env, "init")
    shutil.copyfile(old, repo / old.name)
    _tmi(repo, env, "add", old.name)
    _tmi(repo, env, "commit", "-m", "v1")
    shutil.copyfile(new, repo / old.name)
    return repo


def _merge_templates(work: Path, env: dict[str, str]) -> tuple[Path, Path, str]:
    from conformance import _clip_leaves, _edit_clip_layer

    clean, clash = work / "merge-clean", work / "merge-clash"
    tama = ROOT / "example" / "tama.clip"
    leaves = _clip_leaves(tama)
    layer = f"clip:{leaves[0]}"
    for repo, other_leaf in ((clean, leaves[-1]), (clash, leaves[0])):
        if repo.exists():
            continue
        repo.mkdir(parents=True)
        _tmi(repo, env, "init")
        shutil.copyfile(tama, repo / "tama.clip")
        _tmi(repo, env, "add", "tama.clip")
        _tmi(repo, env, "commit", "-m", "base")
        _tmi(repo, env, "switch", "-c", "other")
        _edit_clip_layer(repo / "tama.clip", other_leaf, (30, 30, 200, 255), (500, 500, 900, 900))
        _tmi(repo, env, "commit", "-a", "-m", "other")
        _tmi(repo, env, "switch", "main")
        _edit_clip_layer(repo / "tama.clip", leaves[0], (200, 30, 30, 255), (600, 600, 1000, 1000))
        _tmi(repo, env, "commit", "-a", "-m", "main")
        if repo is clash:
            _tmi(repo, env, "merge", "other", check=False)
    # the conflicting layer as `restore --theirs` selects it (its stable id)
    import json

    meta = json.loads((clash / ".tmi" / "conflicts" / "tama.clip" / "conflict.json").read_text(encoding="utf-8"))
    layer = meta["conflicts"][0]["stable_source_id"]
    return clean, clash, layer


def _time(template: Path, cmds: list[list[str]], exe: list[str], env: dict[str, str], work: Path, n: int) -> list[float]:
    times = []
    for _ in range(n):
        run = work / "run"
        if run.exists():
            shutil.rmtree(run)
        shutil.copytree(template, run)
        t0 = time.perf_counter()
        for args in cmds:
            r = subprocess.run([*exe, *args], cwd=run, env=env, capture_output=True)
            if r.returncode not in (0, 1):
                raise SystemExit(f"{args}: {r.stdout!r} {r.stderr!r}")
        times.append(time.perf_counter() - t0)
    shutil.rmtree(work / "run", ignore_errors=True)
    return times


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--work", type=Path, help="where the templates are built (kept; reused)")
    parser.add_argument("-n", type=int, default=3, help="runs per case and side")
    parser.add_argument("--bin", type=Path, default=ROOT / "rust" / "target" / "release" / f"tmi{EXE}")
    args = parser.parse_args()
    env = dict(os.environ, TMI_AUTHOR_NAME="T", TMI_AUTHOR_EMAIL="t@example.com", TMI_PYTHON=sys.executable,
               PYTHONPATH=str(ROOT) + os.pathsep + os.environ.get("PYTHONPATH", ""))
    env.pop("TMI_PROGRESS", None)
    work = args.work or Path(tempfile.mkdtemp(prefix="tmi-bench-diffmerge-"))
    work.mkdir(parents=True, exist_ok=True)
    ex = ROOT / "example"
    cases = [("`diff --html` (tama.clip → tama_2.clip)", _diff_template(work, env, "tama", ex / "tama.clip", ex / "tama_2.clip"),
              [["diff", "--html", "--no-open", "--out", "o"]])]
    if (LOLITA / "page0002.clip").exists():
        cases.append(("`diff --html` (lolita page0001 → page0002)",
                      _diff_template(work, env, "lolita", LOLITA / "page0001.clip", LOLITA / "page0002.clip"),
                      [["diff", "--html", "--no-open", "--out", "o"]]))
    clean, clash, layer = _merge_templates(work, env)
    cases.append(("`merge other` (tama.clip, different layers)", clean, [["merge", "other"]]))
    cases.append(("`restore --theirs tama.clip:<layer>`", clash, [["restore", "--theirs", f"tama.clip:{layer}"]]))
    cases.append(("…then `add tama.clip` and `commit -m`", clash,
                  [["restore", "--theirs", f"tama.clip:{layer}"], ["add", "tama.clip"], ["commit", "-m", "resolved"]]))
    print("| | `python -m tmi` | Rust `tmi` |")
    print("|---|---:|---:|")
    for label, template, cmds in cases:
        py = _time(template, cmds, [sys.executable, "-m", "tmi"], env, work, args.n)
        rs = _time(template, cmds, [str(args.bin)], env, work, args.n)
        fmt = lambda t: f"{min(t):.2f}s ({statistics.median(t):.2f})"  # noqa: E731
        print(f"| {label} | {fmt(py)} | {fmt(rs)} |", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
