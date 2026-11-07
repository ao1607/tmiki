"""PLUGINS.md 6.4: the format plugins' costs, items 1-10, as Markdown and JSON.

    python tools/bench_plugins.py [--items 1-10] [--repeat F] [--out DIR]
                                  [--tmi PATH] [--conform PATH] [--label TEXT]
                                  [--writes-python PY] [--writes-against OLD.exe ...]
                                  [--size-of FILE ...]
    python tools/bench_plugins.py --compare BEFORE.json AFTER.json

Items 1-6 run in `tmi-conform plugin-bench` (the host in one process, next
to the ORA test plugin built natively, `OraNative`); 7-10 run the
executable:

 1. compiling a component (Cranelift) and loading it from the .cwasm cache
 2. a fresh instance: pooling / CoW, compiled in memory or mapped from the
    .cwasm, and instances per second with pools (K) of 1-12
 3. one read-structure by size (10 / 100 / 10,000 layers; 100,000 tiles)
 4. the same ORA built natively and as wasm through the
    whole import
 5. fuel / epoch on and off
 6. deferred layers (1-64) by K
 7. peak memory of `tmiki add` of an ORA with K = 1 and 8 (`TMI_PLUGIN_WORKERS`)
 8. the size of the executable
 9. the built-in formats' add / commit -a / status (`tools/bench_writes.py
    --rust-only`; needs a Python with tmi_accel for its templates,
    `--writes-python`), alternating with older executables (`--writes-against`)
10. how much a new version of a component adds to a repository (CDC)

The plugin is the Rust ORA test plugin (plugins are written in Rust only,
PLUGINS.md Q14; P8 and P9-A also measured the same plugin in Python).

Measuring another build (before / after a change): point `--tmi` and
`--conform` (or TMI_BENCH_TMI / TMI_BENCH_CONFORM) at its binaries; the
components are built from this checkout's sources unless `--ora` or `--bad`
name others. Then `--compare` two results.json.

Every number is the minimum and the median of n runs (`--repeat` scales n).
The machine's load at the start and the end is recorded with the results,
so numbers taken while other builds ran can be told apart; rerun when the
machine is quiet and keep the minimum.
"""

from __future__ import annotations

import argparse
import ctypes
import datetime as _dt
import json
import os
import platform
import shutil
import statistics
import subprocess
import sys
import tempfile
import time
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
sys.path.insert(0, str(ROOT / "tools"))
import build_test_plugins as btp  # noqa: E402

EXE = ".exe" if os.name == "nt" else ""
TMI_BEFORE_P5 = 11_959_296  # PLUGINS.md 6.4 item 8: tmiki.exe before wasmtime
TMI_P5_BEFORE = 12_716_544  # "P5 の結果": the build P5 started from
TMI_P5_AFTER = 27_576_320  # "P5 の結果": with wasmtime


# -- running things ------------------------------------------------------------

def run(cmd, cwd=None, env=None, check=True):
    r = subprocess.run([str(c) for c in cmd], cwd=cwd, env=env, capture_output=True, text=True,
                       encoding="utf-8", errors="replace")
    if check and r.returncode != 0:
        raise SystemExit(f"bench_plugins: {' '.join(map(str, cmd))} failed ({r.returncode}):\n{r.stdout}{r.stderr}")
    return r


if os.name == "nt":
    class _PMC(ctypes.Structure):
        _fields_ = [("cb", ctypes.c_ulong), ("PageFaultCount", ctypes.c_ulong),
                    ("PeakWorkingSetSize", ctypes.c_size_t), ("WorkingSetSize", ctypes.c_size_t),
                    ("QuotaPeakPagedPoolUsage", ctypes.c_size_t), ("QuotaPagedPoolUsage", ctypes.c_size_t),
                    ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t), ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
                    ("PagefileUsage", ctypes.c_size_t), ("PeakPagefileUsage", ctypes.c_size_t)]


def run_measured(cmd, cwd, env):
    """(wall seconds, peak working set / max RSS bytes, peak commit bytes or None)."""
    start = time.perf_counter()
    p = subprocess.Popen([str(c) for c in cmd], cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.PIPE)
    if os.name == "nt":
        out, err = p.communicate()
        wall = time.perf_counter() - start
        pmc = _PMC()
        pmc.cb = ctypes.sizeof(_PMC)
        handle = int(p._handle)  # still open: Popen closes it on deletion
        if not ctypes.windll.psapi.GetProcessMemoryInfo(ctypes.c_void_p(handle), ctypes.byref(pmc), pmc.cb):
            raise OSError("GetProcessMemoryInfo failed")
        rss, commit = pmc.PeakWorkingSetSize, pmc.PeakPagefileUsage
        code = p.returncode
    else:
        out = p.stdout.read()
        err = p.stderr.read()
        _, status, ru = os.wait4(p.pid, 0)
        wall = time.perf_counter() - start
        p.returncode = code = os.waitstatus_to_exitcode(status)
        rss = ru.ru_maxrss * (1 if sys.platform == "darwin" else 1024)
        commit = None
    if code != 0:
        raise SystemExit(f"bench_plugins: {' '.join(map(str, cmd))} failed ({code}):\n{out.decode(errors='replace')}{err.decode(errors='replace')}")
    return wall, rss, commit


def machine_load() -> dict:
    """What else the machine is doing: CPU load and the build processes."""
    info: dict = {"time": _dt.datetime.now().isoformat(timespec="seconds")}
    busy = ("cargo", "rustc", "python", "link", "cl", "tmi", "tmi-conform")
    if os.name == "nt":
        r = run(["powershell", "-NoProfile", "-Command",
                 "(Get-CimInstance Win32_Processor | Measure-Object LoadPercentage -Average).Average;"
                 "Get-Process | Group-Object ProcessName | ForEach-Object { $_.Name + ' ' + $_.Count }"], check=False)
        lines = r.stdout.splitlines()
        try:
            info["cpu_load_percent"] = float(lines[0])
        except (IndexError, ValueError):
            pass
        procs = {}
        for l in lines[1:]:
            name, _, count = l.rpartition(" ")
            if name.lower() in busy:
                procs[name.lower()] = int(count)
        info["processes"] = procs
    else:
        info["loadavg"] = os.getloadavg()
        r = run(["ps", "-eo", "comm="], check=False)
        procs = {}
        for name in r.stdout.split():
            if name in busy:
                procs[name] = procs.get(name, 0) + 1
        info["processes"] = procs
    return info


def machine() -> dict:
    cpu = platform.processor()
    if os.name == "nt":
        r = run(["powershell", "-NoProfile", "-Command", "(Get-CimInstance Win32_Processor).Name"], check=False)
        cpu = r.stdout.strip() or cpu
    elif Path("/proc/cpuinfo").exists():
        for l in Path("/proc/cpuinfo").read_text().splitlines():
            if l.startswith("model name"):
                cpu = l.split(":", 1)[1].strip()
                break
    return {"os": platform.platform(), "cpu": cpu, "threads": os.cpu_count(), "python": sys.version.split()[0]}


# -- the records -----------------------------------------------------------------

class Results:
    def __init__(self):
        self.records: list[dict] = []

    def add(self, item: int, lang: str, case: str, **fields):
        self.records.append({"item": item, "lang": lang, "case": case, **fields})

    def timed(self, item: int, lang: str, case: str, times: list[float], **extra):
        self.add(item, lang, case, n=len(times), min_s=min(times), median_s=statistics.median(times), **extra)


def fmt_s(s: float | None) -> str:
    if s is None:
        return "-"
    if s < 1e-3:
        return f"{s * 1e6:.1f} µs"
    if s < 1:
        return f"{s * 1e3:.2f} ms"
    return f"{s:.3f} s"


def fmt_bytes(b: float) -> str:
    return f"{int(b):,} B" if b < 1 << 20 else f"{b / (1 << 20):.1f} MiB ({int(b):,} B)"


def fmt_value(r: dict) -> str:
    if "min_s" in r:
        return f"{fmt_s(r['min_s'])} ({fmt_s(r['median_s'])})"
    unit = r.get("unit", "")
    if unit == "bytes":
        return fmt_bytes(r["value"])
    if unit == "per_s":
        return f"{r['value']:,.0f}/s"
    return f"{r['value']:,.2f} {unit}".strip()


TITLES = {
    0: "部品と機械",
    1: "初回のコンパイルと .cwasm の読み込み",
    2: "インスタンスの作成",
    3: "read-structure 1 回（大きさごと）",
    4: "パイプライン全体：ネイティブ（OraNative）と wasm",
    5: "fuel・epoch の有無（read-structure 10,000 レイヤー）",
    6: "deferred のレイヤー数と K",
    7: "`tmiki add`（ORA）のピークメモリ",
    8: "実行ファイルの大きさ",
    9: "組み込みの形式（`tools/bench_writes.py --rust-only`）",
    10: "component の版を重ねたときの増え方（CDC）",
}

LANGS = ["native", "rust", "-"]


def markdown(meta: dict, records: list[dict]) -> str:
    out = [f"# bench_plugins {meta.get('label') or ''}".rstrip(), ""]
    m = meta["machine"]
    out.append(f"- 機械：{m['cpu']}、{m['threads']} スレッド、{m['os']}")
    out.append(f"- tmi：`{meta['tmi']}`、tmi-conform：`{meta['conform']}`、repeat {meta['repeat']}")
    for k in ("load_start", "load_end"):
        if k in meta:
            out.append(f"- 負荷（{k}）：{json.dumps(meta[k], ensure_ascii=False)}")
    out.append("- 数字は最小値、括弧内は中央値。")
    out.append("")
    for item in range(0, 11):
        rs = [r for r in records if r["item"] == item]
        if not rs:
            continue
        out.append(f"## {item}. {TITLES[item]}")
        out.append("")
        if item == 2:
            out += _pivot_parallel(rs)
            rs = [r for r in rs if not r["case"].startswith("parallel")]
        if item == 6:
            out += _pivot_deferred(rs)
            out.append("")
            continue
        cases = list(dict.fromkeys(r["case"] for r in rs))
        langs = [l for l in LANGS if any(r["lang"] == l for r in rs)]
        langs += [l for l in dict.fromkeys(r["lang"] for r in rs) if l not in LANGS]
        out.append("| | " + " | ".join(langs) + " |")
        out.append("|---|" + "---:|" * len(langs))
        for c in cases:
            cells = []
            for l in langs:
                hit = [r for r in rs if r["case"] == c and r["lang"] == l]
                cells.append(fmt_value(hit[0]) if hit else "")
            out.append(f"| {c} | " + " | ".join(cells) + " |")
        out.append("")
    return "\n".join(out) + "\n"


def _pivot_parallel(rs: list[dict]) -> list[str]:
    par = [r for r in rs if r["case"].startswith("parallel") and "(best)" in r["case"]]
    if not par:
        return []
    ks = sorted({int(r["case"].split("k=")[1].split(":")[0]) for r in par})
    rows = sorted({(r["lang"], r["case"].split(" k=")[0]) for r in par}, key=lambda x: (LANGS.index(x[0]), x[1]))
    out = ["インスタンス/秒（最良、K 本のスレッドで並列に作る）：", "",
           "| | " + " | ".join(f"K={k}" for k in ks) + " |", "|---|" + "---:|" * len(ks)]
    for lang, prefix in rows:
        cells = []
        for k in ks:
            hit = [r for r in par if r["lang"] == lang and r["case"].startswith(f"{prefix} k={k}:")]
            cells.append(f"{hit[0]['value']:,.0f}" if hit else "")
        out.append(f"| {lang} {prefix.replace('parallel ', '')} | " + " | ".join(cells) + " |")
    out.append("")
    return out


def _pivot_deferred(rs: list[dict]) -> list[str]:
    ds = sorted({int(r["case"].split("deferred=")[1].split()[0]) for r in rs})
    ks = sorted({int(r["case"].split("k=")[1]) for r in rs if "k=" in r["case"]})
    out = []
    for lang in [l for l in LANGS if any(r["lang"] == l for r in rs)]:
        if lang == "native":
            continue
        out += [f"{lang}（native は deferred を 1 つの処理で読む参考値）：", "",
                "| deferred | native | " + " | ".join(f"K={k}" for k in ks) + " | K=1→最良 |",
                "|---:|---:|" + "---:|" * (len(ks) + 1)]
        for d in ds:
            nat = [r for r in rs if r["lang"] == "native" and r["case"] == f"deferred={d}"]
            cells = [fmt_s(nat[0]["min_s"]) if nat else ""]
            mins = []
            for k in ks:
                hit = [r for r in rs if r["lang"] == lang and r["case"] == f"deferred={d} k={k}"]
                cells.append(fmt_s(hit[0]["min_s"]) if hit else "")
                if hit:
                    mins.append(hit[0]["min_s"])
            speed = f"×{mins[0] / min(mins):.2f}" if mins else ""
            out.append(f"| {d} | " + " | ".join(cells) + f" | {speed} |")
        out.append("")
    return out


def compare(a_path: Path, b_path: Path) -> str:
    a = json.loads(a_path.read_text(encoding="utf-8"))
    b = json.loads(b_path.read_text(encoding="utf-8"))
    key = lambda r: (r["item"], r["lang"], r["case"])  # noqa: E731
    bm = {key(r): r for r in b["records"]}
    out = [f"| item | lang | case | {a['meta'].get('label') or 'before'} | {b['meta'].get('label') or 'after'} | after/before |",
           "|---:|---|---|---:|---:|---:|"]
    for r in a["records"]:
        s = bm.get(key(r))
        if not s:
            continue
        va = r.get("min_s", r.get("value"))
        vb = s.get("min_s", s.get("value"))
        ratio = f"{vb / va:.3f}" if va else ""
        out.append(f"| {r['item']} | {r['lang']} | {r['case']} | {fmt_value(r)} | {fmt_value(s)} | {ratio} |")
    return "\n".join(out) + "\n"


# -- items 7-10 ------------------------------------------------------------------

def tmi_env(work: Path) -> dict:
    env = dict(os.environ, TMI_AUTHOR_NAME="T", TMI_AUTHOR_EMAIL="t@example.com",
               TMI_PLUGIN_CACHE=str(work / "plugin-cache"),
               TMI_CONFIG_HOME=str(work / "config"))
    for k in ("TMI_PROGRESS", "TMI_PLUGIN_DEV", "TMI_PLUGIN_WORKERS", "TMI_NATIVE"):
        env.pop(k, None)
    return env


def item7(res: Results, a, work: Path, components: dict[str, Path]) -> None:
    """Peak memory (and wall time) of `tmiki add <file>.ora` with K = 1 and 8."""
    files = {
        "64 layers 1024x1024 on 2048x2048": ["64", "1024", "2048"],
        "64 layers 1024x1024 on 2048x2048, deferred": ["64", "1024", "2048", "--deferred"],
    }
    env0 = tmi_env(work)
    n = max(3, round(3 * a.repeat))
    for what, spec in files.items():
        ora = work / f"item7-{len(list(work.glob('item7-*.ora')))}.ora"
        run([a.conform, "ora-make", ora, *spec])
        res.add(7, "-", f"file: {what}", value=ora.stat().st_size, unit="bytes")
        for lang, wasm in components.items():
            template = work / f"item7-{lang}-template"
            if template.exists():
                shutil.rmtree(template)
            template.mkdir()
            run([a.tmi, "init"], cwd=template, env=env0)
            run([a.tmi, "plugin", "add", wasm], cwd=template, env=env0)
            run([a.tmi, "commit", "-m", "plugin"], cwd=template, env=env0)
            shutil.copyfile(ora, template / "f.ora")
            run([a.tmi, "plugin", "prepare"], cwd=template, env=env0, check=False)  # warm the .cwasm cache
            for k in (1, 8):
                env = dict(env0, TMI_PLUGIN_WORKERS=str(k))
                walls, rss, commit = [], [], []
                for _ in range(n):
                    rd = work / "item7-run"
                    if rd.exists():
                        shutil.rmtree(rd)
                    shutil.copytree(template, rd)
                    w, r, c = run_measured([a.tmi, "add", "f.ora"], rd, env)
                    walls.append(w)
                    rss.append(r)
                    if c is not None:
                        commit.append(c)
                res.timed(7, lang, f"{what}: K={k} wall", walls)
                res.add(7, lang, f"{what}: K={k} peak working set", value=max(rss), unit="bytes",
                        all=rss)
                if commit:
                    res.add(7, lang, f"{what}: K={k} peak commit", value=max(commit), unit="bytes", all=commit)
            # the same file as an opaque blob (no plugin): the floor
        base = work / "item7-binary"
        if base.exists():
            shutil.rmtree(base)
        base.mkdir()
        run([a.tmi, "init"], cwd=base, env=env0)
        shutil.copyfile(ora, base / "f.ora")
        walls, rss = [], []
        for _ in range(n):
            rd = work / "item7-run"
            if rd.exists():
                shutil.rmtree(rd)
            shutil.copytree(base, rd)
            w, r, _ = run_measured([a.tmi, "add", "f.ora"], rd, env0)
            walls.append(w)
            rss.append(r)
        res.timed(7, "-", f"{what}: no plugin (opaque blob) wall", walls)
        res.add(7, "-", f"{what}: no plugin (opaque blob) peak working set", value=max(rss), unit="bytes")


def item8(res: Results, a) -> None:
    size = Path(a.tmi).stat().st_size
    res.add(8, "-", "tmi executable (this build)", value=size, unit="bytes")
    res.add(8, "-", "before wasmtime (PLUGINS.md 6.4: 11,959,296)", value=TMI_BEFORE_P5, unit="bytes")
    res.add(8, "-", "P5: before → after (12,716,544 → 27,576,320)", value=TMI_P5_AFTER, unit="bytes")
    res.add(8, "-", "this build − before wasmtime", value=size - TMI_BEFORE_P5, unit="bytes")
    res.add(8, "-", "this build / before wasmtime", value=size / TMI_BEFORE_P5, unit="×")
    res.add(8, "-", "this build − P5 after", value=size - TMI_P5_AFTER, unit="bytes")
    for extra in a.size_of or []:
        res.add(8, "-", f"{Path(extra).name} ({extra})", value=Path(extra).stat().st_size, unit="bytes")


def item9(res: Results, a, work: Path) -> None:
    """bench_writes.py --rust-only with this tmi and each --writes-against
    binary in turn, `rounds` times (alternating, so a busy spell hits all of
    them); per command the minimum of all runs and the median of the medians."""
    py = a.writes_python or sys.executable
    bins = [("rust", a.tmi)] + [(f"bin:{Path(b).stem}", b) for b in (a.writes_against or [])]
    rounds = max(1, round(3 * a.repeat)) if len(bins) > 1 else 1
    n = max(3, round((3 if len(bins) > 1 else 5) * a.repeat))
    got: dict[tuple[str, str], list[tuple[float, float]]] = {}
    env = {k: v for k, v in os.environ.items() if k != "TMI_PLUGIN_WORKERS"}
    for _ in range(rounds):
        for label, b in bins:
            cmd = [py, ROOT / "tools" / "bench_writes.py", "--rust-only", "--bin", b, "-n", str(n), "--work", work / "bench-writes"]
            if a.writes_files:
                cmd += ["--files", *a.writes_files]
            r = run(cmd, env=env, check=False)
            if r.returncode != 0:
                res.add(9, label, "bench_writes.py failed", value=r.returncode, unit="exit", detail=(r.stderr or r.stdout)[-2000:])
                return
            for line in r.stdout.splitlines():
                cells = [c.strip() for c in line.strip().strip("|").split("|")]
                if len(cells) != 3 or not cells[2].endswith(")"):
                    continue
                try:
                    mn, med = cells[2].split("s (")
                    got.setdefault((label, cells[0]), []).append((float(mn), float(med.rstrip(")"))))
                except ValueError:
                    continue
    for (label, case), v in got.items():
        res.add(9, label, case, n=n * len(v), min_s=min(x[0] for x in v), median_s=statistics.median(x[1] for x in v))
    for label, b in bins:
        res.add(9, label, f"binary: {b}", value=Path(b).stat().st_size, unit="bytes")


def _tree_bytes(d: Path) -> tuple[int, int]:
    files = [p for p in d.rglob("*") if p.is_file()]
    return sum(p.stat().st_size for p in files), len(files)


def item10(res: Results, a, work: Path) -> None:
    """Commit successive versions of a component; what each adds to .tmi/objects."""
    series: dict[str, list[tuple[str, Path]]] = {}
    series["rust"] = [("0.2.0 read-only", btp.ora("0.2.0", True)), ("0.3.0", btp.ora()),
                      ("0.3.0 rebuilt", btp.ora()), ("0.1.0 read-only", btp.ora("0.1.0", True))]
    env = tmi_env(work)
    for lang, versions in series.items():
        repo = work / f"item10-{lang}"
        if repo.exists():
            shutil.rmtree(repo)
        repo.mkdir()
        run([a.tmi, "init"], cwd=repo, env=env)
        objects = repo / ".tmi" / "objects"
        before, files_before = _tree_bytes(objects)
        prev = None
        for i, (label, wasm) in enumerate(versions):
            data = wasm.read_bytes()
            same = prev is not None and data == prev
            prev = data
            if i == 0:
                run([a.tmi, "plugin", "add", wasm], cwd=repo, env=env)
            else:
                shutil.copyfile(wasm, repo / ".tmiplugins" / "ora.wasm")
                run([a.tmi, "add", ".tmiplugins/ora.wasm"], cwd=repo, env=env)
            if not same:
                run([a.tmi, "commit", "-m", label], cwd=repo, env=env)
            after, files_after = _tree_bytes(objects)
            added = after - before
            res.add(10, lang, f"v{i + 1} {label}: component", value=len(data), unit="bytes",
                    identical_to_previous=same)
            res.add(10, lang, f"v{i + 1} {label}: added to .tmi/objects", value=added, unit="bytes",
                    files=files_after - files_before)
            res.add(10, lang, f"v{i + 1} {label}: added / component", value=added / len(data), unit="×")
            before, files_before = after, files_after


# -- main ------------------------------------------------------------------------

def parse_items(s: str) -> list[int]:
    out = []
    for part in s.split(","):
        if "-" in part:
            lo, hi = part.split("-")
            out += range(int(lo), int(hi) + 1)
        elif part:
            out.append(int(part))
    return sorted(set(out))


def main() -> int:
    p = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    p.add_argument("--items", default="1-10", help="e.g. 1-6,8 (default 1-10)")
    p.add_argument("--repeat", type=float, default=1.0, help="multiply every n by this")
    p.add_argument("--out", type=Path, help="results.json and results.md go here (default: a new temp dir)")
    p.add_argument("--label", default="", help="a name for this run (shown by --compare)")
    p.add_argument("--tmi", default=os.environ.get("TMI_BENCH_TMI") or str(ROOT / "rust" / "target" / "release" / f"tmi{EXE}"))
    p.add_argument("--conform", default=os.environ.get("TMI_BENCH_CONFORM")
                   or str(ROOT / "rust" / "target" / "release" / f"tmi-conform{EXE}"))
    p.add_argument("--ora", type=Path, help="the Rust ORA component (default: built from rust/plugins)")
    p.add_argument("--bad", type=Path, help="the `bad` test component (default: built)")
    p.add_argument("--writes-python", help="the Python (with tmi_accel) that builds bench_writes.py's templates")
    p.add_argument("--writes-files", nargs="*", help="bench_writes.py --files (default: its own)")
    p.add_argument("--writes-against", nargs="*", help="item 9: other tmi executables timed in turn with --tmi "
                   "(e.g. a build from before P1; copy them out of target/ first)")
    p.add_argument("--size-of", nargs="*", help="item 8: more files whose size to list")
    p.add_argument("--keep", action="store_true", help="keep the work directory")
    p.add_argument("--compare", nargs=2, type=Path, metavar=("BEFORE", "AFTER"), help="compare two results.json")
    a = p.parse_args()
    if a.compare:
        sys.stdout.write(compare(*a.compare))
        return 0
    items = parse_items(a.items)
    for b in ("tmi", "conform"):
        if not Path(getattr(a, b)).is_file():
            raise SystemExit(f"bench_plugins: {getattr(a, b)} is missing (cargo build --release -p tmi-cli -p tmi-conform)")
    out = a.out or Path(tempfile.mkdtemp(prefix="tmi-bench-plugins-"))
    out.mkdir(parents=True, exist_ok=True)
    work = Path(tempfile.mkdtemp(prefix="tmi-bench-plugins-work-"))
    meta = {"label": a.label, "tmi": str(a.tmi), "conform": str(a.conform), "repeat": a.repeat, "items": items,
            "machine": machine(), "load_start": machine_load()}
    res = Results()
    try:
        ora = a.ora or btp.ora()
        bad = a.bad or (btp.build() / "tmi_plugin_bad.wasm")
        components = {"rust": Path(ora)}
        meta["components"] = {k: str(v) for k, v in components.items()}
        host_items = [i for i in items if i <= 6]
        if host_items:
            jl = work / "host.jsonl"
            cmd = [a.conform, "plugin-bench", "--ora", ora, "--bad", bad, "--items", ",".join(map(str, host_items)),
                   "--repeat", str(a.repeat), "--json", jl]
            print("bench_plugins: " + " ".join(map(str, cmd)), file=sys.stderr, flush=True)
            r = subprocess.run([str(c) for c in cmd], stdout=sys.stderr)
            if r.returncode != 0:
                raise SystemExit(f"bench_plugins: tmi-conform plugin-bench failed ({r.returncode})")
            for l in jl.read_text(encoding="utf-8").splitlines():
                res.records.append(json.loads(l))
        if 7 in items:
            item7(res, a, work, components)
        if 8 in items:
            item8(res, a)
        if 9 in items:
            item9(res, a, work)
        if 10 in items:
            item10(res, a, work)
    finally:
        meta["load_end"] = machine_load()
        (out / "results.json").write_text(json.dumps({"meta": meta, "records": res.records}, ensure_ascii=False, indent=1),
                                          encoding="utf-8")
        md = markdown(meta, res.records)
        (out / "results.md").write_text(md, encoding="utf-8")
        if not a.keep:
            shutil.rmtree(work, ignore_errors=True)
    sys.stdout.write(md)
    print(f"bench_plugins: {out / 'results.json'}, {out / 'results.md'}", file=sys.stderr)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
