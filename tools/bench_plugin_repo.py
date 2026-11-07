"""Time repository commands on plugin-format files (PLUGINS.md P9, 6.1 / 6.2).

    python tools/bench_plugin_repo.py --bins OLD/tmiki.exe NEW/tmiki.exe [-n 7]

With the ORA test plugin (`rust/plugins/ora`, which
tools/build_test_plugins.py builds; P9-A also ran the Python one, which
PLUGINS.md Q14 took out) the
template repositories are built once with the first executable, and every
timed run works on a fresh copy (the copy is not timed); the executables
are run one after the other, run for run, so a busy machine slows both
alike:

- `add -A` of 48 small ORA files (two layers each) in a new repository;
- `add` of `many-100-deferred.ora` (100 layers of 64x64, every layer
  deferred: one `read-layer` per layer) and of `many-1000.ora`;
- `status` with nothing changed (the stat cache answers), with all of them
  committed;
- `status` and `diff` with one layer of `many-1000.ora` and of
  `many-100-deferred.ora` changed in the working tree: the first time in
  a fresh copy (`first`: the files are hashed again), and again in the
  same copy (`again`: each executable has its own copy, where it ran the
  command once before the timed runs);
- `add -A` of those two files right after a `status`.

Prints the minimum (median) wall time of n runs per executable as a
Markdown table. `tmi-conform` (rust/target/release) writes the large ORA
files (`tmi-conform ora-samples`).
"""
from __future__ import annotations

import argparse
import io
import os
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
sys.path.insert(0, str(ROOT / "tools"))
EXE = ".exe" if os.name == "nt" else ""


def _rewrite_member(path: Path, member: str, data: bytes) -> None:
    """`path` with `member`'s bytes replaced (the other members as they were)."""
    src = zipfile.ZipFile(io.BytesIO(path.read_bytes()))
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as out:
        for info in src.infolist():
            out.writestr(info, data if info.filename == member else src.read(info.filename))
    path.write_bytes(buf.getvalue())


def _png(w: int, h: int, rgba: tuple[int, int, int, int]) -> bytes:
    from tmi import raster

    return raster.png(raster.Rgba(w, h, bytes(rgba) * (w * h)))


def _run(exe: str, repo: Path, env: dict[str, str], *args: str) -> subprocess.CompletedProcess:
    r = subprocess.run([exe, *args], cwd=repo, env=env, capture_output=True)
    if r.returncode:
        raise SystemExit(f"{exe} {' '.join(args)} in {repo}: exit {r.returncode}\n{r.stderr.decode(errors='replace')}")
    return r


def build_templates(work: Path, exe: str, conform: str, component: Path, env: dict[str, str]) -> dict[str, tuple[Path, list[str]]]:
    from conformance import _ora_bytes

    samples = work / "samples"
    if not samples.exists():
        samples.mkdir(parents=True)
        subprocess.run([conform, "ora-samples", str(samples)], check=True, capture_output=True)
    base = work / "t"
    if base.exists():
        shutil.rmtree(base)

    def new_repo(name: str) -> Path:
        repo = base / name
        repo.mkdir(parents=True)
        _run(exe, repo, env, "init")
        _run(exe, repo, env, "plugin", "add", str(component))
        _run(exe, repo, env, "commit", "-m", "plugin")
        return repo

    # 48 small files
    many = new_repo("many")
    for i in range(48):
        c = (i * 5 % 256, i * 11 % 256, i * 17 % 256, 255)
        (many / f"f{i:02}.ora").write_bytes(_ora_bytes([("bg", c, (32, 24), 0, 0), ("fg", c[::-1], (8, 6), 4, 3)]))
    deferred = new_repo("deferred")
    shutil.copyfile(samples / "many-100-deferred.ora", deferred / "d.ora")
    big = new_repo("big")
    shutil.copyfile(samples / "many-1000.ora", big / "k.ora")
    # everything committed, then one layer changed in the two large files
    committed = base / "committed"
    shutil.copytree(many, committed)
    shutil.copyfile(samples / "many-100-deferred.ora", committed / "d.ora")
    shutil.copyfile(samples / "many-1000.ora", committed / "k.ora")
    _run(exe, committed, env, "add", "-A")
    _run(exe, committed, env, "commit", "-m", "files")
    _run(exe, committed, env, "status")  # the stat cache filled
    modified = base / "modified"
    shutil.copytree(committed, modified)
    _rewrite_member(modified / "k.ora", "data/0.png", _png(32, 32, (250, 10, 10, 255)))
    _rewrite_member(modified / "d.ora", "data/0.png", _png(64, 64, (10, 250, 10, 255)))
    time.sleep(1.1)  # mtimes the stat cache cannot vouch for
    # (template, untimed command before, timed command, in place: the same
    # copy for every run of an executable, after one untimed run)
    return {
        "`add -A` (48 small files)": (many, None, ["add", "-A"], False),
        "`add` many-100-deferred": (deferred, None, ["add", "d.ora"], False),
        "`add` many-1000": (big, None, ["add", "k.ora"], False),
        "`status` (nothing changed)": (committed, None, ["status"], False),
        "`status` (2 modified, first)": (modified, None, ["status"], False),
        "`status` (2 modified, again)": (modified, None, ["status"], True),
        "`diff` (2 modified, first)": (modified, None, ["diff"], False),
        "`diff` (2 modified, again)": (modified, None, ["diff"], True),
        "`add -A` after `status` (2 modified)": (modified, ["status"], ["add", "-A"], False),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--bins", nargs="+", type=Path, default=[ROOT / "rust" / "target" / "release" / f"tmi{EXE}"])
    parser.add_argument("--conform", type=Path, default=ROOT / "rust" / "target" / "release" / f"tmi-conform{EXE}")
    parser.add_argument("--work", type=Path)
    parser.add_argument("-n", type=int, default=7)
    parser.add_argument("--only", help="run only the cases whose label contains this")
    args = parser.parse_args()
    import build_test_plugins

    work = args.work or Path(tempfile.mkdtemp(prefix="tmi-bench-plugin-repo-"))
    work.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, TMI_AUTHOR_NAME="T", TMI_AUTHOR_EMAIL="t@example.com", TMI_PYTHON=sys.executable,
               TMI_CONFIG_HOME=str(work / "config"), TMI_PLUGIN_CACHE=str(work / "cwasm"),
               TMI_COMMIT_TIMESTAMP="2026-01-01T00:00:00+00:00",
               PYTHONPATH=str(ROOT) + os.pathsep + os.environ.get("PYTHONPATH", ""))
    for k in ("TMI_PROGRESS", "TMI_PLUGIN_DEV", "TMI_NO_DECODE_REUSE"):
        env.pop(k, None)
    (work / "config").mkdir(exist_ok=True)
    subprocess.run([str(args.bins[0]), "config", "--global", "plugin.trust", "allow"], env=env, check=True, capture_output=True)
    names = [str(b) for b in args.bins]
    print("| | " + " | ".join(f"`{Path(b).parent.name}/{Path(b).name}`" for b in names) + " |")
    print("|---|" + "---:|" * len(names))
    for plugin in ["rust"]:
        component = build_test_plugins.ora()
        templates = build_templates(work / plugin, names[0], str(args.conform), component, env)
        for label, (template, before, cmd, in_place) in templates.items():
            if args.only and args.only not in label:
                continue
            times: list[list[float]] = [[] for _ in names]
            if in_place:
                for i, exe in enumerate(names):
                    run_dir = work / f"run{i}"
                    if run_dir.exists():
                        shutil.rmtree(run_dir)
                    shutil.copytree(template, run_dir)
                    _run(exe, run_dir, env, *cmd)
            for _ in range(args.n):
                for i, exe in enumerate(names):
                    run_dir = work / (f"run{i}" if in_place else "run")
                    if not in_place:
                        if run_dir.exists():
                            shutil.rmtree(run_dir)
                        shutil.copytree(template, run_dir)
                        if before:
                            _run(exe, run_dir, env, *before)
                    start = time.perf_counter()
                    _run(exe, run_dir, env, *cmd)
                    times[i].append(time.perf_counter() - start)
            cells = [f"{min(t) * 1000:.0f} ms ({statistics.median(t) * 1000:.0f})" for t in times]
            print(f"| {plugin}: {label} | " + " | ".join(cells) + " |", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
