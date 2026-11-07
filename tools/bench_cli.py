"""Time commands: `python -m tmi` against the Rust `tmi` (RUST_MIGRATION.md 3a, 3b-1).

    python tools/bench_cli.py [--work DIR] [-n 9] [--writes | --reads]

Builds (once, in --work) a repository holding tama.clip, kakenhi-kaguya.mdp,
new.mdp and blend_modes.clip in four commits with a tag each, warms the stat
cache, then runs each command n times both ways and prints the minimum
(median) wall time as a Markdown table. A write case runs its setup before
and its undo after each timed run (untimed, by the same side), so every run
starts from the same state. Build the executable first:
`cargo build --release -p tmi-cli` in rust/.
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
EXE = ".exe" if os.name == "nt" else ""
FILES = ["tama.clip", "kakenhi-kaguya.mdp", "new.mdp", "blend_modes.clip"]
CASES = [
    ("start-up (`config --global user.name`)", ["config", "--global", "user.name"]),
    ("`status` (clean)", ["status"]),
    ("`status -s`", ["status", "-s"]),
    ("`log`", ["log"]),
    ("`log --oneline --stat`", ["log", "--oneline", "--stat"]),
    ("`branch -v`", ["branch", "-v"]),
    ("`tag -n`", ["tag", "-n"]),
    ("`show` (HEAD)", ["show"]),
    ("`show v0:tama.clip` (60 MB, chunked)", ["show", "v0:tama.clip"]),
    ("`stash list`", ["stash", "list"]),
    ("`log --grep add --since 2020-01-01`", ["log", "--grep", "add", "--since", "2020-01-01"]),
    ("`ls-layers tama.clip v0`", ["ls-layers", "tama.clip", "v0"]),
    ("`--version`", ["--version"]),
    ("`log -h`", ["log", "-h"]),
    ("forwarded: `diff --stat v0 v1`", ["diff", "--stat", "v0", "v1"]),
]
# (label, command, setup, undo): setup and undo are lists of commands
# (["touch", path] rewrites a file with its own bytes: a stat-cache miss).
WRITE_CASES = [
    ("`branch x`", ["branch", "x"], [], [["branch", "-D", "x"]]),
    ("`tag -a t -m m`", ["tag", "-a", "t", "-m", "m"], [], [["tag", "-d", "t"]]),
    ("`config user.email e@x`", ["config", "user.email", "e@x"], [], []),
    ("`remote add r ../r`", ["remote", "add", "r", "../r"], [], [["remote", "remove", "r"]]),
    ("`switch old` (removes 3 files)", ["switch", "old"], [], [["switch", "main"]]),
    ("`switch main` (writes 3 files, 24 MB)", ["switch", "main"], [["switch", "old"]], []),
    ("`checkout v0` (detached)", ["checkout", "v0"], [], [["switch", "main"]]),
    ("`restore --source v1 new.mdp`", ["restore", "--source", "v1", "new.mdp"], [], [["restore", "new.mdp"]]),
    ("`reset --soft v2`", ["reset", "--soft", "v2"], [], [["reset", "--soft", "v3"]]),
    ("`reset --hard` (nothing to rewrite)", ["reset", "--hard"], [], []),
    ("`rm --cached new.mdp`", ["rm", "--cached", "new.mdp"], [], [["reset", "new.mdp"]]),
    ("`mv blend_modes.clip b.clip` (re-imports it)", ["mv", "blend_modes.clip", "b.clip"], [],
     [["rm", "-f", "b.clip"], ["reset", "--hard"]]),
    ("`recover --as r HEAD@{1}`", ["recover", "--as", "r", "HEAD@{1}"], [], [["branch", "-D", "r"]]),
    ("`status` (stat-cache miss: hashes tama.clip)", ["status"], [["touch", "tama.clip"]], []),
    ("`gc`", ["gc"], [], []),
]


def build_repo(repo: Path, env: dict[str, str]) -> None:
    py = [sys.executable, "-m", "tmi"]
    repo.mkdir(parents=True)
    subprocess.run(py + ["init"], cwd=repo, env=env, check=True, capture_output=True)
    for i, name in enumerate(FILES):
        shutil.copyfile(ROOT / "example" / name, repo / name)
        for args in (["add", name], ["commit", "-m", f"add {name}"], ["tag", f"v{i}"]):
            subprocess.run(py + args, cwd=repo, env=env, check=True, capture_output=True)
    subprocess.run(py + ["branch", "feature"], cwd=repo, env=env, check=True, capture_output=True)
    subprocess.run(py + ["branch", "old", "v0"], cwd=repo, env=env, check=True, capture_output=True)
    time.sleep(1)  # the stat cache trusts mtimes older than its write
    subprocess.run(py + ["status"], cwd=repo, env=env, check=True, capture_output=True)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--work", type=Path, help="where the repository is built (kept; reused)")
    parser.add_argument("-n", type=int, default=9, help="runs per command and side")
    parser.add_argument("--bin", type=Path, default=ROOT / "rust" / "target" / "release" / f"tmi{EXE}")
    which = parser.add_mutually_exclusive_group()
    which.add_argument("--writes", action="store_true", help="only the write commands")
    which.add_argument("--reads", action="store_true", help="only the read-only commands")
    args = parser.parse_args()
    work = args.work or Path(tempfile.mkdtemp(prefix="tmi-bench-cli-"))
    (work / "userconfig").mkdir(parents=True, exist_ok=True)
    env = dict(os.environ, TMI_AUTHOR_NAME="T", TMI_AUTHOR_EMAIL="t@example.com", TMI_PYTHON=sys.executable,
               TMI_CONFIG_HOME=str(work / "userconfig"),
               PYTHONPATH=str(ROOT) + os.pathsep + os.environ.get("PYTHONPATH", ""))
    repo = work / "bench"
    if not repo.exists():
        build_repo(repo, env)

    def run(prefix: list[str], cmd: list[str]) -> None:
        if cmd[0] == "touch":
            path = repo / cmd[1]
            path.write_bytes(path.read_bytes())
            return
        result = subprocess.run(prefix + cmd, cwd=repo, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
        if result.returncode not in (0, 1):
            raise SystemExit(f"{cmd}: {result.stderr.decode(errors='replace')}")

    def timeit(prefix: list[str], cmd: list[str], setup=(), undo=()) -> tuple[float, float]:
        times = []
        for _ in range(args.n):
            for step in setup:
                run(prefix, step)
            start = time.perf_counter()
            result = subprocess.run(prefix + cmd, cwd=repo, env=env, stdout=subprocess.DEVNULL, stderr=subprocess.PIPE)
            times.append(time.perf_counter() - start)
            if result.returncode not in (0, 1):
                raise SystemExit(f"{cmd}: {result.stderr.decode(errors='replace')}")
            for step in undo:
                run(prefix, step)
        return min(times), statistics.median(times)

    cases = [] if args.writes else [(label, cmd, [], []) for label, cmd in CASES]
    cases += [] if args.reads else WRITE_CASES
    print("| | `python -m tmi` | Rust `tmi` |")
    print("|---|---:|---:|")
    for label, cmd, setup, undo in cases:
        py = timeit([sys.executable, "-m", "tmi"], cmd, setup, undo)
        rs = timeit([str(args.bin)], cmd, setup, undo)
        print(f"| {label} | {py[0] * 1000:.0f} ms ({py[1] * 1000:.0f}) | {rs[0] * 1000:.1f} ms ({rs[1] * 1000:.1f}) |",
              flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
