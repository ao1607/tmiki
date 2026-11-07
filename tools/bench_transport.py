"""Time clone / fetch / push: `python -m tmi` against the Rust `tmi` (RUST_MIGRATION.md 4b).

    python tools/bench_transport.py [--work DIR] [-n 3]

Builds (once, in --work) a repository holding tama.clip and
kakenhi-kaguya.mdp in two commits, then for a local bare remote and for an
HTTP one (tools/fake_tmihub.py, in this process) times each command n
times both ways, every run from the same state (a fresh remote for push,
a fresh directory for clone, a fresh repository with only the remote for
fetch). Prints the minimum (median) wall time as a Markdown table. Build
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
EXE = ".exe" if os.name == "nt" else ""
FILES = ["tama.clip", "kakenhi-kaguya.mdp"]


def run(cmd: list[str], cwd: Path, env: dict[str, str]) -> float:
    t0 = time.perf_counter()
    result = subprocess.run(cmd, cwd=cwd, env=env, capture_output=True)
    elapsed = time.perf_counter() - t0
    if result.returncode:
        raise SystemExit(f"{cmd} failed ({result.returncode:#x}) in {cwd}:\n{result.stdout.decode(errors='replace')}\n{result.stderr.decode(errors='replace')}")
    return elapsed


def main() -> int:
    from tools.fake_tmihub import Hub

    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--work", type=Path)
    parser.add_argument("-n", type=int, default=3)
    parser.add_argument("--bin", type=Path, default=ROOT / "rust" / "target" / "release" / f"tmi{EXE}")
    args = parser.parse_args()
    work = args.work or Path(tempfile.mkdtemp(prefix="tmi-bench-transport-"))
    work.mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, TMI_AUTHOR_NAME="Bench", TMI_AUTHOR_EMAIL="b@example.com",
               PYTHONPATH=str(ROOT) + os.pathsep + os.environ.get("PYTHONPATH", ""), TMI_PYTHON=sys.executable)
    env.pop("TMI_PROGRESS", None)
    sides = {"python": [sys.executable, "-m", "tmi"], "rust": [str(args.bin)]}
    src = work / "src"
    if not (src / ".tmi").is_dir():
        src.mkdir(parents=True)
        run(sides["python"] + ["init"], src, env)
        for name in FILES:
            shutil.copyfile(ROOT / "example" / name, src / name)
            run(sides["python"] + ["add", name], src, env)
            run(sides["python"] + ["commit", "-m", f"add {name}"], src, env)
    objects = sum(1 for p in (src / ".tmi" / "objects").glob("*/*") if p.is_file())
    size = sum(p.stat().st_size for p in (src / ".tmi" / "objects").glob("*/*") if p.is_file())
    print(f"source: {objects} objects, {size / 1e6:.1f} MB")

    hub_root = work / "hub"
    # stored zips: the time is the client's, not the stand-in server's deflate
    import zipfile

    hub = Hub(hub_root, compression=zipfile.ZIP_STORED).start()
    rows: list[tuple[str, dict[str, list[float]]]] = []
    try:
        for kind in ("local", "https"):
            url = str(work / "remote.tmi") if kind == "local" else hub.url("u", "r")
            subprocess.run(sides["python"] + ["remote", "remove", "bench"], cwd=src, env=env, capture_output=True)
            run(sides["python"] + ["remote", "add", "bench", url], src, env)

            def fresh_remote() -> None:
                target = work / "remote.tmi" if kind == "local" else hub_root / "u" / "r"
                shutil.rmtree(target, ignore_errors=True)
                target.parent.mkdir(parents=True, exist_ok=True)
                run(sides["python"] + ["init", "--bare", str(target)], work, env)

            populated = []

            def populated_remote() -> None:
                # once: clone and fetch leave the remote as it is (and a
                # remote just written is slower to read while the virus
                # scanner sees its files for the first time)
                if not populated:
                    fresh_remote()
                    run(sides["python"] + ["push", "bench"], src, env)
                    populated.append(True)

            def pushable_remote() -> None:
                fresh_remote()
                populated.clear()

            cases = {
                "push": (pushable_remote, lambda: (src, ["push", "bench"])),
                "clone": (populated_remote, lambda: (work, ["clone", url, "copy"])),
                "fetch": (populated_remote, lambda: (work / "copy", ["fetch", "origin"])),
            }
            for label, (prepare, command) in cases.items():
                times: dict[str, list[float]] = {"python": [], "rust": []}
                for _ in range(args.n):
                    for side in ("python", "rust"):
                        prepare()
                        shutil.rmtree(work / "copy", ignore_errors=True)
                        if label == "fetch":
                            (work / "copy").mkdir()
                            run(sides["python"] + ["init"], work / "copy", env)
                            run(sides["python"] + ["remote", "add", "origin", url], work / "copy", env)
                        cwd, cmd = command()
                        times[side].append(run(sides[side] + cmd, cwd, env))
                rows.append((f"`{label}` ({kind})", times))
                print(f"{label} ({kind}): python {min(times['python']):.2f}s, rust {min(times['rust']):.2f}s", flush=True)
    finally:
        hub.stop()
    print()
    print("| | `python -m tmi` | Rust `tmi` |")
    print("|---|---:|---:|")
    for label, times in rows:
        py, rs = times["python"], times["rust"]
        print(f"| {label} | {min(py):.2f}s ({statistics.median(py):.2f}) | {min(rs):.2f}s ({statistics.median(rs):.2f}) |")
    if not args.work:
        shutil.rmtree(work, ignore_errors=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
