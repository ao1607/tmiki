"""PLUGINS.md P6 on the Python side: `.tmiattributes` is read the way the
Rust side reads it (tmi_repo::attributes), `format=binary` files are opaque
blobs, and a file mapped to a format plugin is refused, never recorded
another way (plugins run in the tmi executable only; tools/conformance.py's
`plugins` scenario checks the executable).
"""
from __future__ import annotations

import json
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from tmi import attributes, raster  # noqa: E402


def _tmi(cwd: Path, *args: str) -> subprocess.CompletedProcess:
    env = dict(os.environ, TMI_AUTHOR_NAME="Tester", TMI_AUTHOR_EMAIL="t@example.com", PYTHONPATH=str(ROOT))
    env["TMI_CONFIG_HOME"] = str(cwd.parent / "userconfig")
    return subprocess.run([sys.executable, "-m", "tmi", *args], cwd=cwd, env=env, capture_output=True, text=True, encoding="utf-8")


class AttributesTests(unittest.TestCase):
    def test_the_rules_of_the_rust_side(self) -> None:
        rules = attributes.parse(
            "# comment\n*.ora format=plugin:ora\n*.PSD   format=binary   # opaque\nscans/**/*.tif format=binary\n"
            "*.ora on-error=binary\nkeep/*.ora format=builtin\nodd/* format=what\n"
        )
        self.assertEqual(attributes.lookup(rules, "a/b.ora"), "plugin:ora")
        self.assertEqual(attributes.lookup(rules, "A/B.ORA"), "plugin:ora", "ASCII case does not count, on any OS")
        self.assertEqual(attributes.lookup(rules, "x.psd"), "binary")
        self.assertEqual(attributes.lookup(rules, "scans/2026/x.TIF"), "binary")
        self.assertEqual(attributes.lookup(rules, "x.tif"), "builtin")
        self.assertEqual(attributes.lookup(rules, "keep/x.ora"), "builtin")
        self.assertEqual(attributes.lookup(rules, "odd/x"), "what")
        self.assertEqual(attributes.lookup(rules, ".tmiplugins/ora.wasm"), "builtin")
        self.assertEqual(attributes.lookup(rules, ".tmiattributes"), "builtin")
        self.assertEqual(attributes.lookup([], "x.ora"), "builtin")
        self.assertEqual(attributes.fold("Ä.ORA"), "Ä.ora", "only ASCII is folded")


class PythonRefusesPluginFormats(unittest.TestCase):
    def setUp(self) -> None:
        self.tmp = Path(tempfile.mkdtemp(prefix="tmi-p6-"))
        (self.tmp / "userconfig").mkdir()
        self.repo = self.tmp / "repo"
        self.repo.mkdir()
        self.assertEqual(_tmi(self.repo, "init").returncode, 0)
        (self.repo / ".tmiattributes").write_text("*.ora format=plugin:ora\nforced.png format=binary\n", encoding="utf-8")
        (self.repo / "a.ora").write_bytes(b"PK\x03\x04 not really a zip")
        pixels = raster.png(raster.Rgba(3, 2, bytes([10, 20, 30, 255]) * 6))
        (self.repo / "forced.png").write_bytes(pixels)
        (self.repo / "plain.png").write_bytes(pixels)

    def tearDown(self) -> None:
        import shutil

        shutil.rmtree(self.tmp, ignore_errors=True)

    def _index(self) -> dict:
        path = self.repo / ".tmi" / "index"
        return json.loads(path.read_text(encoding="utf-8")).get("entries", {}) if path.exists() else {}

    def test_add_refuses_a_plugin_format_and_writes_nothing(self) -> None:
        objects = sorted(p.name for p in (self.repo / ".tmi" / "objects").rglob("*") if p.is_file())
        res = _tmi(self.repo, "add", "a.ora")
        self.assertEqual(res.returncode, 1, res.stderr)
        self.assertIn("a.ora: .tmiattributes maps it to plugin 'ora'; format plugins run in the tmi executable only", res.stderr)
        self.assertNotIn("a.ora", self._index())
        self.assertEqual(objects, sorted(p.name for p in (self.repo / ".tmi" / "objects").rglob("*") if p.is_file()))
        res = _tmi(self.repo, "add", "-A")
        self.assertEqual(res.returncode, 1, res.stdout + res.stderr)
        self.assertNotIn("a.ora", self._index(), "a bulk add never records it either")

    def test_format_binary_is_an_opaque_blob(self) -> None:
        self.assertEqual(_tmi(self.repo, "add", "forced.png", "plain.png").returncode, 0)
        index = self._index()
        self.assertEqual(index["forced.png"]["source_format"], "binary")
        self.assertEqual(index["plain.png"]["source_format"], "png")
        self.assertNotIn("importer", index["forced.png"], "built-in records have no importer (Q3)")

    def test_the_plugin_command_is_the_executables(self) -> None:
        for args in (("list",), ("remove", "ora")):
            res = _tmi(self.repo, "plugin", *args)
            self.assertEqual(res.returncode, 1)
            self.assertIn("'tmiki plugin' runs in the tmi executable only", res.stderr)

    def test_a_bulk_add_stages_the_rest_and_lists_the_refused(self) -> None:
        # issue #49: the plugin-format file is listed, the others staged
        res = _tmi(self.repo, "add", ".")
        self.assertEqual(res.returncode, 1, res.stdout + res.stderr)
        self.assertIn("tmiki: error: 1 file could not be added:\n  a.ora: .tmiattributes maps it to plugin 'ora'", res.stderr)
        self.assertIn("staged plain.png", res.stdout)
        self.assertIn("plain.png", self._index())

    def test_removing_a_component_still_mapped_warns(self) -> None:
        # issue #50 (the executable says the same; conformance compares them)
        component = self.repo / ".tmiplugins" / "ora.wasm"
        component.parent.mkdir()
        component.write_bytes(b"\0asm\x01\0\0\0")
        self.assertEqual(_tmi(self.repo, "add", ".tmiplugins/ora.wasm", "plain.png").returncode, 0)
        self.assertEqual(_tmi(self.repo, "commit", "-m", "x").returncode, 0)
        res = _tmi(self.repo, "rm", ".tmiplugins/ora.wasm")
        self.assertEqual(res.returncode, 0, res.stderr)
        self.assertEqual(
            res.stderr,
            "warning: .tmiattributes still maps files to plugin 'ora' (0 tracked files); without .tmiplugins/ora.wasm "
            "they cannot be added or committed. To stop using the plugin, run 'tmiki plugin remove ora'\n",
        )
        res = _tmi(self.repo, "rm", "plain.png")
        self.assertEqual(res.stderr, "", "other files: no warning")


class RemovedComponentWarnings(unittest.TestCase):
    def test_component_ids(self) -> None:
        self.assertEqual(attributes.component_id(".tmiplugins/ora.wasm"), "ora")
        self.assertIsNone(attributes.component_id(".tmiplugins/a/ora.wasm"))
        self.assertIsNone(attributes.component_id(".tmiplugins/Ora.wasm"))
        self.assertIsNone(attributes.component_id("ora.wasm"))


if __name__ == "__main__":
    unittest.main()
