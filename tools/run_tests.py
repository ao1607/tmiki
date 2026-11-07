"""Run tmi's test suites in parallel worker processes.

    python tools/run_tests.py                 # all suites
    python tools/run_tests.py --all           # both variants, golden hashes, render corpus (tmi and reference)
    python tools/run_tests.py --variants verify -j 6
    python tools/run_tests.py -k incremental  # only tests whose id contains the text

Each test runs whole in one worker (``unittest`` semantics are unchanged: the
same ``setUp``/``tearDown``, its own temp dir), and workers take the next test
from a shared queue, longest first, by the durations recorded in
``.test-durations.json`` (gitignored) on earlier runs. Variants run one after
another, each with its environment set before the workers start:

    accel     the default environment (tmi needs tmi_accel; RUST_MIGRATION.md 1e)
    verify    TMI_VERIFY_PARTIAL=1 (incremental composites checked against a full render)
''s CLI runs against
              a loopback tmihub) through the Rust `tmi` executable
              (TMI_BIN=rust/target/release/tmiki; RUST_MIGRATION.md 3a, 4b), which serves what
              it reproduces and hands the rest to `python -m tmi`. Build it first:
              `cargo build --release -p tmi-cli` in rust/. Not part of `--all`.

``--all`` then runs ``tools/bench.py --verify`` and ``tools/render_corpus.py
--verify --quick`` with tmi's renderer and with the reference ``tmiref``.

Exit status is non-zero when any test fails or errors, or the golden or render-corpus
check fails.
"""
from __future__ import annotations

import argparse
import io
import json
import multiprocessing as mp
import os
import subprocess
import sys
import time
import traceback
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
DEFAULT_MODULES = [
    "tests.test_cli_regressions",
    "tests.test_bugfix_regressions",
    "tests.test_storage_dedup",
    "tests.test_https_transport",
    "tests.test_locking",
    "tests.test_layer_blob",
    "tests.test_rust_kernels",
    "tests.test_rust_image_kernels",
    "tests.test_raster",
    "tests.test_raster_formats",
    "tests.test_render_tools",
    "tests.test_render_parity",
    "tests.test_mdp2_parity",
    "tests.test_clip_io_parity",
    "tests.test_plugins_p6",
    "tests.test_plugin_dev",
]
RUST_TMI = ROOT / "rust" / "target" / "release" / ("tmiki.exe" if os.name == "nt" else "tmiki")
VARIANTS = {
    "accel": {},
    "verify": {"TMI_VERIFY_PARTIAL": "1"},
    "rustcli": {"TMI_BIN": str(RUST_TMI)},
}
# --all runs these; rustcli needs a cargo build first.
ALL_VARIANTS = ("accel", "verify")
# The tests a variant runs, by id prefix (others: all of them).
VARIANT_TESTS = {"rustcli": ("tests.test_cli_regressions.TmiCliRegressionTests.",
                             "tests.test_https_transport.", "tests.test_locking.")}
DURATIONS = ROOT / ".test-durations.json"
# Tests create their temp dirs under .test-tmp and remove it when empty; a
# marker keeps it in place while other workers may be about to use it.
TMP_ROOT = ROOT / ".test-tmp"
MARKER = TMP_ROOT / ".parallel-run"


def _flatten(suite: unittest.TestSuite) -> list[unittest.TestCase]:
    out: list[unittest.TestCase] = []
    for item in suite:
        if isinstance(item, unittest.TestSuite):
            out.extend(_flatten(item))
        else:
            out.append(item)
    return out


def discover(modules: list[str], pattern: str | None) -> list[str]:
    sys.path.insert(0, str(ROOT))
    suite = unittest.defaultTestLoader.loadTestsFromNames(modules)
    ids = []
    for test in _flatten(suite):
        test_id = test.id()
        if test_id.startswith("unittest.loader._FailedTest"):
            raise SystemExit(f"could not load tests: {test_id}")
        if pattern is None or pattern.lower() in test_id.lower():
            ids.append(test_id)
    return ids


def _worker_init() -> None:
    os.chdir(ROOT)
    sys.path.insert(0, str(ROOT))


def _run_one(test_id: str) -> tuple[str, str, float, str]:
    """Run one test in this worker; returns (id, outcome, seconds, details)."""
    stream = io.StringIO()
    start = time.perf_counter()
    try:
        suite = unittest.defaultTestLoader.loadTestsFromName(test_id)
        result = unittest.TextTestRunner(stream=stream, verbosity=0).run(suite)
    except Exception:
        return test_id, "error", time.perf_counter() - start, traceback.format_exc()
    seconds = time.perf_counter() - start
    if result.errors:
        return test_id, "error", seconds, "".join(text for _, text in result.errors)
    if result.failures:
        return test_id, "fail", seconds, "".join(text for _, text in result.failures)
    if result.unexpectedSuccesses:
        return test_id, "fail", seconds, "unexpected success"
    if result.skipped:
        return test_id, "skip", seconds, result.skipped[0][1]
    return test_id, "ok", seconds, ""


def run_variant(name: str, ids: list[str], jobs: int, durations: dict[str, float]) -> tuple[bool, dict[str, float]]:
    extra = VARIANTS[name]
    saved = {key: os.environ.get(key) for key in (*extra, "TMI_AUTHOR_NAME", "TMI_AUTHOR_EMAIL")}
    os.environ.setdefault("TMI_AUTHOR_NAME", "Tester")
    os.environ.setdefault("TMI_AUTHOR_EMAIL", "tester@example.com")
    os.environ.update(extra)
    prefix = VARIANT_TESTS.get(name)
    if prefix:
        ids = [t for t in ids if t.startswith(prefix)]
    order = sorted(ids, key=lambda t: durations.get(t, float("inf")), reverse=True)
    start = time.perf_counter()
    counts = {"ok": 0, "skip": 0, "fail": 0, "error": 0}
    problems: list[tuple[str, str, str]] = []
    timings: dict[str, float] = {}
    try:
        # spawn: workers import tmi afresh, so env set above (e.g.
        # TMI_VERIFY_PARTIAL) takes effect exactly as in a plain
        # `python -m unittest` run.
        ctx = mp.get_context("spawn")
        with ctx.Pool(jobs, initializer=_worker_init) as pool:
            for done, (test_id, outcome, seconds, details) in enumerate(pool.imap_unordered(_run_one, order), 1):
                counts[outcome] += 1
                timings[test_id] = seconds
                if outcome in ("fail", "error"):
                    problems.append((test_id, outcome, details))
                    print(f"  {outcome.upper()}: {test_id}", flush=True)
                if sys.stdout.isatty():
                    print(f"\r  [{name}] {done}/{len(order)}", end="", flush=True)
    finally:
        for key, value in saved.items():
            if value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = value
    if sys.stdout.isatty():
        print()
    wall = time.perf_counter() - start
    for test_id, outcome, details in problems:
        print(f"\n{'=' * 70}\n{outcome.upper()}: {test_id}\n{'-' * 70}\n{details}")
    status = "OK" if not problems else "FAILED"
    print(
        f"[{name}] {status}: {len(order)} tests in {wall:.1f}s with {jobs} workers "
        f"(ok {counts['ok']}, skipped {counts['skip']}, failed {counts['fail']}, errors {counts['error']})",
        flush=True,
    )
    return not problems, timings


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("modules", nargs="*", default=DEFAULT_MODULES, help="test modules (default: all suites)")
    parser.add_argument("-j", "--jobs", type=int, default=max(2, min(8, (os.cpu_count() or 4) // 2)))
    parser.add_argument("--variants", default="accel", help="comma-separated: accel, verify, rustcli")
    parser.add_argument("--all", action="store_true",
                        help="all variants, then tools/bench.py --verify and tools/render_corpus.py --verify --quick (both renderers)")
    parser.add_argument("-k", dest="pattern", help="only tests whose id contains this text")
    args = parser.parse_args(argv)

    variants = list(ALL_VARIANTS) if args.all else [v.strip() for v in args.variants.split(",") if v.strip()]
    unknown = [v for v in variants if v not in VARIANTS]
    if unknown:
        parser.error(f"unknown variant(s): {', '.join(unknown)}")
    if "rustcli" in variants and not RUST_TMI.exists():
        parser.error(f"rustcli needs {RUST_TMI}: cargo build --release -p tmi-cli (in rust/)")

    ids = discover(args.modules, args.pattern)
    if not ids:
        print("no tests selected")
        return 1
    try:
        durations = json.loads(DURATIONS.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        durations = {}

    TMP_ROOT.mkdir(exist_ok=True)
    MARKER.write_text("tools/run_tests.py is running\n", encoding="utf-8")
    ok = True
    start = time.perf_counter()
    try:
        for name in variants:
            passed, timings = run_variant(name, ids, args.jobs, durations)
            ok &= passed
            if name == "accel" or not durations:
                durations.update(timings)
                DURATIONS.write_text(json.dumps(durations, indent=0, sort_keys=True), encoding="utf-8")
        if args.all:
            golden = subprocess.run([sys.executable, str(ROOT / "tools" / "bench.py"), "--verify"], cwd=ROOT,
                                    capture_output=True, text=True)
            last = (golden.stdout.strip().splitlines() or [golden.stderr.strip()])[-1]
            print(f"[golden] {last}")
            ok &= golden.returncode == 0
            # tmi's renderer (tmi_accel), then the reference (tmiref), each
            # against the pinned corpus.
            for renderer in ("rust", "python"):
                corpus = subprocess.run([sys.executable, str(ROOT / "tools" / "render_corpus.py"), "--verify", "--quick",
                                         "--render", renderer, "-j", str(args.jobs)], cwd=ROOT, capture_output=True, text=True)
                lines = corpus.stdout.strip().splitlines()
                for line in lines:
                    if line.lstrip().startswith("MISMATCH"):
                        print(f"[corpus] {line.strip()}")
                print(f"[corpus] {(lines or [corpus.stderr.strip()])[-1]}")
                ok &= corpus.returncode == 0
    finally:
        MARKER.unlink(missing_ok=True)
        try:
            TMP_ROOT.rmdir()
        except OSError:
            pass
    print(f"{'ALL OK' if ok else 'FAILED'} in {time.perf_counter() - start:.1f}s")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
