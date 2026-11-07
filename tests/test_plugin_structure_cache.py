"""PLUGINS.md P9 (6.1, 6.2) through the tmi executable: what a format plugin
said about a file is kept in `.tmi/cache/plugin/<component>/<digest>` and
a bulk `add` reads the next file's structure while the current one
decodes, and neither changes a record. Two repositories take the same
steps, one with the structure cache off (`TMI_NO_STRUCTURE_CACHE=1`, which
also turns the prefetch off): their objects, index and outputs must be the
same. Needs the executable (TMI_BIN, else rust/target/release) and the ORA
test plugin (tools/build_test_plugins.py: cargo and the wasm32-wasip2
target); skipped without them.
"""
from __future__ import annotations

import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
sys.path.insert(0, str(ROOT / "tools"))
EXE = ".exe" if os.name == "nt" else ""


def _tmi_exe() -> Path:
    exe = Path(os.environ.get("TMI_BIN") or ROOT / "rust" / "target" / "release" / f"tmi{EXE}")
    if not exe.is_file():
        raise unittest.SkipTest(f"{exe} is not built (cargo build --release -p tmi-cli)")
    return exe


def _component() -> Path:
    import build_test_plugins

    try:
        return build_test_plugins.ora()
    except SystemExit as e:  # no cargo or no wasm32-wasip2 target
        raise unittest.SkipTest(str(e)) from None


class StructureCacheTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.exe = _tmi_exe()
        cls.component = _component()

    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="tmi-p9-structure-"))
        (self.tmp / "config").mkdir()
        self.repos = {"cached": self.tmp / "cached", "off": self.tmp / "off"}

    def tearDown(self) -> None:
        shutil.rmtree(self.tmp, ignore_errors=True)

    def run_both(self, *args: str, same: bool = True) -> str:
        outs = []
        for name, repo in self.repos.items():
            env = dict(os.environ, TMI_AUTHOR_NAME="Tester", TMI_AUTHOR_EMAIL="t@example.com",
                       TMI_COMMIT_TIMESTAMP="2026-01-01T00:00:00+00:00", TMI_CONFIG_HOME=str(self.tmp / "config"),
                       TMI_PLUGIN_CACHE=str(self.tmp / "cwasm"), TMI_PYTHON=sys.executable,
                       TMI_STRUCTURE_CACHE_MIN_MS="0")  # every file, however quick its plugin
            for k in ("TMI_PROGRESS", "TMI_PLUGIN_DEV", "TMI_NO_STRUCTURE_CACHE"):
                env.pop(k, None)
            if name == "off":
                env["TMI_NO_STRUCTURE_CACHE"] = "1"
            r = subprocess.run([str(self.exe), *args], cwd=repo, env=env, capture_output=True)
            self.assertEqual(r.returncode, 0, f"{name}: {args}: {r.stderr.decode(errors='replace')}")
            here = str(repo).encode()
            outs.append((r.stdout.replace(here, b"<repo>"), r.stderr.replace(here, b"<repo>")))
        if same:
            self.assertEqual(outs[0], outs[1], args)
        return outs[0][0].decode("utf-8", errors="replace")

    def write_both(self, rel: str, data: bytes) -> None:
        for repo in self.repos.values():
            (repo / rel).write_bytes(data)

    def state(self, repo: Path) -> tuple:
        tmi = repo / ".tmi"
        objects = sorted(p.relative_to(tmi).as_posix() for p in (tmi / "objects").rglob("*") if p.is_file())
        index = json.loads((tmi / "index").read_text(encoding="utf-8")) if (tmi / "index").exists() else None
        return objects, index

    def cached_files(self, repo: Path) -> list[str]:
        d = repo / ".tmi" / "cache" / "plugin"
        return sorted(p.relative_to(d).as_posix() for p in d.rglob("*") if p.is_file()) if d.is_dir() else []

    def test_the_records_are_the_same_with_and_without_the_cache(self) -> None:
        from conformance import _ora_bytes

        def ora(seed: int, deferred: bool = False) -> bytes:
            layers = [(f"l{i}", ((seed * 37 + i * 11) % 256, (seed * 7 + i) % 256, (i * 29) % 256, 255), (8 + i, 6 + i), i, i)
                      for i in range(12 if deferred else 2)]
            return _ora_bytes(layers, image_attrs=' tmi-test-defer="1"' if deferred else "")

        for repo in self.repos.values():
            repo.mkdir()
        self.run_both("init")
        self.run_both("config", "--global", "plugin.trust", "allow")
        self.run_both("plugin", "add", str(self.component))
        for i in range(6):
            self.write_both(f"f{i}.ora", ora(i))
        self.write_both("d.ora", ora(99, deferred=True))
        # a bulk add (the prefetch reads the next file's structure)
        self.run_both("add", "-A")
        self.assertEqual(self.state(self.repos["cached"]), self.state(self.repos["off"]))
        self.run_both("commit", "-m", "files")
        self.assertEqual(self.state(self.repos["cached"]), self.state(self.repos["off"]))
        cached = self.cached_files(self.repos["cached"])
        self.assertEqual(len(cached), 7, cached)
        self.assertEqual(len({c.split("/")[0] for c in cached}), 1, "one component")
        self.assertEqual(self.cached_files(self.repos["off"]), [])
        # status and diff of modified files, twice: the second time from the cache
        self.write_both("f1.ora", ora(101))
        self.write_both("d.ora", ora(102, deferred=True))
        first = self.run_both("status")
        self.assertIn("modified   d.ora", first)
        self.assertEqual(self.run_both("status"), first)
        diff = self.run_both("diff")
        self.assertIn("diff d.ora", diff)
        self.assertEqual(self.run_both("diff"), diff)
        self.assertEqual(len(self.cached_files(self.repos["cached"])), 9)
        # damaged cache files are a miss: the plugin reads the files again
        for rel in self.cached_files(self.repos["cached"]):
            (self.repos["cached"] / ".tmi" / "cache" / "plugin" / rel).write_bytes(b"\xa1\x00")
        self.run_both("add", "-A")
        self.assertEqual(self.state(self.repos["cached"]), self.state(self.repos["off"]))
        self.run_both("commit", "-m", "edits")
        self.assertEqual(self.state(self.repos["cached"]), self.state(self.repos["off"]))
        # gc empties it with the rest of .tmi/cache
        self.run_both("gc", same=False)  # (it says how much it removed)
        self.assertEqual(self.cached_files(self.repos["cached"]), [])


if __name__ == "__main__":
    unittest.main()
