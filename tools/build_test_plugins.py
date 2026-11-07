"""Build the test plugins of rust/plugins as wasm32-wasip2 components.

    python tools/build_test_plugins.py            # build; print the directory
    python tools/build_test_plugins.py --ora      # print the ORA component's path
                                                  # (version 0.3.0, with its writer)
    python tools/build_test_plugins.py --ora-version 0.2.0 --read-only
                                                  # the same reader as version 0.2.0,
                                                  # without the writer (its own
                                                  # target directory)
    python tools/build_test_plugins.py --ora-dev  # the ORA plugin as a native
                                                  # development plugin (tmiki plugin dev)

Nothing is committed: the components are built from source here (they are
deterministic for a given toolchain), into TMI_PLUGINS_TARGET_DIR when it is
set, else rust/plugins/target (the same place tmi-plugin-host's tests build
them). Needs `rustup target add wasm32-wasip2`.
"""

from __future__ import annotations

import argparse
import os
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
WORKSPACE = ROOT / "rust" / "plugins"


def target_dir(version: str | None, read_only: bool = False) -> Path:
    base = Path(os.environ.get("TMI_PLUGINS_TARGET_DIR") or WORKSPACE / "target")
    if version is None and not read_only:
        return base
    # inside the base directory (rust/plugins/target is ignored by git)
    return base / "variants" / f"ora-{version or 'default'}{'-ro' if read_only else ''}"


def build(version: str | None = None, read_only: bool = False) -> Path:
    """The directory of the built components (``<crate>.wasm``)."""
    target = target_dir(version, read_only)
    env = {k: v for k, v in os.environ.items()
           if k not in ("CARGO_TARGET_DIR", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "TMI_ORA_VERSION", "TMI_ORA_READ_ONLY")}
    if version is not None:
        env["TMI_ORA_VERSION"] = version
    if read_only:
        env["TMI_ORA_READ_ONLY"] = "1"
    args = ["cargo", "build", "--release", "--target", "wasm32-wasip2", "--target-dir", str(target)]
    args += ["-p", "tmi-plugin-ora"] if version is not None or read_only else ["--workspace"]
    result = subprocess.run(args, cwd=WORKSPACE, env=env, capture_output=True, text=True)
    if result.returncode != 0:
        raise SystemExit(f"building rust/plugins for wasm32-wasip2 failed (is the target installed? "
                         f"`rustup target add wasm32-wasip2`):\n{result.stderr}")
    return target / "wasm32-wasip2" / "release"


def ora(version: str | None = None, read_only: bool = False) -> Path:
    return build(version, read_only) / "tmi_plugin_ora.wasm"


def ora_dev() -> Path:
    """The ORA plugin as a development plugin (``tmiki plugin dev``): a native
    debug build of the same code (tmi-plugin-sdk's ``dev_main``), in
    ``<target>/native`` as tmi-plugin-host's tests build it."""
    target = Path(os.environ.get("TMI_PLUGINS_TARGET_DIR") or WORKSPACE / "target") / "native"
    env = {k: v for k, v in os.environ.items()
           if k not in ("CARGO_TARGET_DIR", "RUSTFLAGS", "CARGO_ENCODED_RUSTFLAGS", "TMI_ORA_VERSION", "TMI_ORA_READ_ONLY")}
    args = ["cargo", "build", "-p", "tmi-plugin-ora", "--features", "dev", "--bin", "ora-dev", "--target-dir", str(target)]
    result = subprocess.run(args, cwd=WORKSPACE, env=env, capture_output=True, text=True)
    if result.returncode != 0:
        raise SystemExit(f"building ora-dev failed:\n{result.stderr}")
    return target / "debug" / ("ora-dev.exe" if os.name == "nt" else "ora-dev")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--ora", action="store_true", help="print the ORA component's path")
    parser.add_argument("--ora-version", help="build the ORA plugin as this version (implies --ora)")
    parser.add_argument("--read-only", action="store_true", help="build the ORA plugin without its writer (implies --ora)")
    parser.add_argument("--ora-dev", action="store_true", help="build the ORA plugin as a development plugin (native); print its path")
    args = parser.parse_args()
    if args.ora_dev:
        print(ora_dev())
    elif args.ora or args.ora_version or args.read_only:
        print(ora(args.ora_version, args.read_only))
    else:
        print(build())
    return 0


if __name__ == "__main__":
    sys.exit(main())
