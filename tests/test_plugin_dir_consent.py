"""PLUGINS.md 11 (a plugin directory is somebody else's code): `tmiki plugin
test --native` and `tmiki plugin build` run what a plugin directory brings
(the manifest's `[dev] command`, cargo's build scripts), so they ask once
for that directory and that content; without a terminal they refuse and
say how to allow it. A manifest may not name files outside its directory.
Skipped without rust/target/release/tmiki (TMI_BIN) or the ORA test component
(cargo, wasm32-wasip2).
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

from tests.test_plugin_dev import ora_file, tmi_exe  # noqa: E402


def ora_component() -> Path:
    import build_test_plugins

    if shutil.which("cargo") is None:
        raise unittest.SkipTest("cargo is not installed (the ORA component is built from rust/plugins)")
    return build_test_plugins.ora()


class PluginDirectoryConsentTests(unittest.TestCase):
    def setUp(self) -> None:
        self.exe = tmi_exe()
        self.tmp = Path(tempfile.mkdtemp(prefix="tmi-dirtrust-"))
        (self.tmp / "userconfig").mkdir()
        self.plugin = self.tmp / "theirs"
        (self.plugin / "tests" / "corpus").mkdir(parents=True)
        (self.plugin / "tests" / "corpus" / "a.ora").write_bytes(ora_file((200, 30, 30, 255), (20, 200, 20, 255)))
        self.marker = self.plugin / "marker"
        # "somebody else's" development plugin: whatever it is, running it leaves the marker
        run = json.dumps([sys.executable, "-c", "open('marker', 'w').write('ran')"])
        self.write_manifest(f'[dev]\ncommand = {json.dumps(run)}\n')

    def tearDown(self) -> None:
        shutil.rmtree(self.tmp, ignore_errors=True)

    def write_manifest(self, extra: str) -> None:
        (self.plugin / "tmi-plugin.toml").write_text(f'[plugin]\nid = "ora"\n{extra}', encoding="utf-8")

    def tmi(self, *args: str, **env: str) -> subprocess.CompletedProcess:
        e = dict(os.environ, TMI_CONFIG_HOME=str(self.tmp / "userconfig"), TMI_PLUGIN_CACHE=str(self.tmp / "cache"),
                 TMI_PLUGIN_CACHE_KEY=str(self.tmp / "cache.key"))
        e.pop("TMI_PLUGIN_DIR_TRUST", None)
        e.update(env)
        return subprocess.run([str(self.exe), *args], cwd=self.plugin, env=e, capture_output=True, text=True,
                              encoding="utf-8", timeout=600, stdin=subprocess.DEVNULL)

    def with_component(self) -> None:
        out = self.plugin / "target" / "tmi-plugin"
        out.mkdir(parents=True)
        shutil.copy(ora_component(), out / "ora.wasm")

    def test_native_test_does_not_run_the_manifests_command_without_consent(self) -> None:
        self.with_component()
        res = self.tmi("plugin", "test", "--native", ".")
        self.assertNotEqual(res.returncode, 0, res.stdout)
        self.assertIn("TMI_PLUGIN_DIR_TRUST=allow", res.stderr)
        self.assertIn("not running its [dev] command", res.stderr)
        self.assertFalse(self.marker.exists(), "the manifest's command ran without a yes")

    def test_the_environment_says_yes_for_scripts(self) -> None:
        self.with_component()
        self.tmi("plugin", "test", "--native", ".", TMI_PLUGIN_DIR_TRUST="allow")
        self.assertTrue(self.marker.exists())

    def test_build_asks_before_cargo_runs_the_directorys_code(self) -> None:
        (self.plugin / "Cargo.toml").write_text('[package]\nname = "x"\nversion = "0.1.0"\n', encoding="utf-8")
        res = self.tmi("plugin", "build", ".")
        self.assertNotEqual(res.returncode, 0)
        self.assertIn("not running its build", res.stderr)
        self.assertNotIn("building plugin", res.stderr)

    def test_a_command_the_user_names_is_theirs_to_run(self) -> None:
        self.with_component()
        run = json.dumps([sys.executable, "-c", "open('marker', 'w').write('ran')"])
        self.tmi("plugin", "test", ".", "--native-command", run)
        self.assertTrue(self.marker.exists())

    def test_the_manifest_cannot_name_files_outside_its_directory(self) -> None:
        for key, value in (("build", "output"), ("test", "corpus")):
            for bad in ("../outside.wasm", "C:/outside.wasm", "/outside"):
                self.write_manifest(f'[{key}]\n{value} = "{bad}"\n')
                res = self.tmi("plugin", "test", ".")
                self.assertNotEqual(res.returncode, 0)
                self.assertIn("inside the plugin directory", res.stderr, (key, bad))


if __name__ == "__main__":
    unittest.main()
