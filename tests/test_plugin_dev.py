"""PLUGINS.md P8, `tmiki plugin dev` seen from a repository: the ORA test
plugin as a development plugin (`ora-dev`, the native build of
`rust/plugins/ora` on tmi-plugin-sdk, `tools/build_test_plugins.py
--ora-dev`) run by the tmi executable, the records it makes, and the commit
that refuses them in the executable and in `python -m tmi`.

Plugins are written in Rust (PLUGINS.md Q14): the Python SDK and the
componentize-py toy app these tests used in P8 are gone. Skipped without
rust/target/release/tmiki (TMI_BIN) or without cargo for ora-dev; the
conformance scenario `plugin-dev` covers the same and more.
"""
from __future__ import annotations

import io
import json
import os
import shutil
import struct
import subprocess
import sys
import tempfile
import unittest
import zipfile
import zlib
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))
sys.path.insert(0, str(ROOT / "tools"))

EXE = ".exe" if os.name == "nt" else ""


def tmi_exe() -> Path:
    exe = Path(os.environ.get("TMI_BIN") or ROOT / "rust" / "target" / "release" / f"tmi{EXE}")
    if not exe.is_file():
        raise unittest.SkipTest(f"{exe} is not built (cargo build --release -p tmi-cli)")
    return exe


_ORA_DEV: list[Path] = []


def ora_dev() -> Path:
    """The ORA plugin as a development plugin, built once per process."""
    if not _ORA_DEV:
        import build_test_plugins

        if shutil.which("cargo") is None:
            raise unittest.SkipTest("cargo is not installed (ora-dev is built from rust/plugins)")
        _ORA_DEV.append(build_test_plugins.ora_dev())
    return _ORA_DEV[0]


def png(w: int, h: int, rgba: tuple[int, int, int, int]) -> bytes:
    def chunk(kind: bytes, data: bytes) -> bytes:
        return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))

    rows = b"".join(b"\x00" + bytes(rgba) * w for _ in range(h))
    return (b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 6, 0, 0, 0))
            + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b""))


def ora_file(bg: tuple[int, int, int, int], fg: tuple[int, int, int, int]) -> bytes:
    """An OpenRaster file of 16x12: a background and a multiply layer."""
    stack = ('<?xml version="1.0" encoding="UTF-8"?>\n<image w="16" h="12"><stack>'
             '<layer name="fg" src="data/fg.png" x="4" y="3" opacity="0.5" composite-op="svg:multiply"/>'
             '<layer name="bg" src="data/bg.png"/></stack></image>\n')
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, "w") as z:
        z.writestr(zipfile.ZipInfo("mimetype"), "image/openraster", compress_type=zipfile.ZIP_STORED)
        z.writestr("stack.xml", stack, compress_type=zipfile.ZIP_DEFLATED)
        z.writestr("data/bg.png", png(16, 12, bg), compress_type=zipfile.ZIP_STORED)
        z.writestr("data/fg.png", png(6, 5, fg), compress_type=zipfile.ZIP_STORED)
    return buf.getvalue()


class DevPluginCliTests(unittest.TestCase):
    """`ora-dev` run as a development plugin by the tmi executable."""

    def setUp(self) -> None:
        self.exe = tmi_exe()
        self.dev = [str(ora_dev())]
        self.tmp = Path(tempfile.mkdtemp(prefix="tmi-dev-"))
        (self.tmp / "userconfig").mkdir()
        self.repo = self.tmp / "repo"
        self.repo.mkdir()
        self.assertEqual(self.run_tmi("init").returncode, 0)
        (self.repo / ".tmiattributes").write_text("*.ora format=plugin:ora\n", encoding="utf-8")
        (self.repo / "a.ora").write_bytes(ora_file((200, 30, 30, 255), (20, 200, 20, 255)))

    def tearDown(self) -> None:
        shutil.rmtree(self.tmp, ignore_errors=True)

    def env(self, **extra: str) -> dict:
        env = dict(os.environ, TMI_AUTHOR_NAME="Tester", TMI_AUTHOR_EMAIL="t@example.com", TMI_PYTHON=sys.executable,
                   PYTHONPATH=str(ROOT), TMI_CONFIG_HOME=str(self.tmp / "userconfig"), TMI_PLUGIN_CACHE=str(self.tmp / "cache"))
        env.pop("TMI_PLUGIN_DEV", None)
        env.update(extra)
        return env

    def run_tmi(self, *args: str, python: bool = False, **env: str) -> subprocess.CompletedProcess:
        cmd = [sys.executable, "-m", "tmi", *args] if python else [str(self.exe), *args]
        return subprocess.run(cmd, cwd=self.repo, env=self.env(**env), capture_output=True, text=True, encoding="utf-8", timeout=300)

    def index(self) -> dict:
        return json.loads((self.repo / ".tmi" / "index").read_text(encoding="utf-8"))["entries"]

    def test_ora_dev_runs_as_a_development_plugin_and_commit_refuses_its_records(self) -> None:
        res = self.run_tmi("plugin", "dev", *self.dev)
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertIn("development plugin ora 0.3.0 (OpenRaster: .ora; reads and writes layers), tmi:format@0.1.0", res.stdout)
        self.assertIn(".tmiattributes: *.ora files are read with it", res.stdout)

        res = self.run_tmi("plugin", "dev", *self.dev, "--", "add", "a.ora")
        self.assertEqual(res.returncode, 0, res.stderr)
        entry = self.index()["a.ora"]
        self.assertEqual(entry["importer"], {"abi": "tmi:format@0.1.0", "component_sha256": "dev", "id": "ora", "version": "0.3.0"})
        self.assertEqual(entry["source_format"], "plugin:ora")
        from tmi.repo import Repo

        layers = Repo.open(self.repo).store.read_json(entry["tree_ref"], "tree")["layers"]
        self.assertEqual([(l["name"], l["blend_mode"], l["opacity"]) for l in layers], [("bg", "normal", 1.0), ("fg", "multiply", 0.5)])
        self.assertEqual(entry["canvas_meta"]["width"], 16)

        refused = ("a.ora: recorded with a development plugin (tmiki plugin dev); a commit cannot hold such records. "
                   "Add it again without TMI_PLUGIN_DEV first")
        res = self.run_tmi("commit", "-m", "dev records")
        self.assertEqual((res.returncode, res.stderr.strip()), (1, f"tmiki: error: {refused}"))
        res = self.run_tmi("commit", "-m", "dev records", python=True)
        self.assertEqual((res.returncode, res.stderr.strip()), (1, f"tmiki: error: {refused}"))
        self.assertFalse((self.repo / ".tmi" / "refs" / "heads" / "main").exists())

        # status through the development plugin: the working file is what was staged
        res = self.run_tmi("plugin", "dev", *self.dev, "--", "status")
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertIn("a.ora", res.stdout)


if __name__ == "__main__":
    unittest.main()
