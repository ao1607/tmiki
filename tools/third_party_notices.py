"""Write THIRD-PARTY-NOTICES.txt: every crate compiled into the tmiki
executable, with its license expression and the license / notice files it
ships (MIT, BSD and Apache-2.0 ask for those to travel with binaries).

    python tools/third_party_notices.py [--target x86_64-pc-windows-msvc] [--out FILE]

The crate list is `cargo tree -p tmi-cli -e normal` for the target; the
files come from each crate's directory in the cargo registry (or the
repository, for vendored code). tmiki's own crates are left out.
"""
from __future__ import annotations

import argparse
import json
import subprocess
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
RUST = ROOT / "rust"
NAMES = ("license", "licence", "copying", "notice", "unlicense", "authors")


def crates(target: str) -> list[tuple[str, str]]:
    out = subprocess.run(
        ["cargo", "tree", "-p", "tmi-cli", "-e", "normal", "--target", target, "--prefix", "none", "-f", "{p}"],
        cwd=RUST, capture_output=True, text=True, check=True,
    ).stdout
    seen = set()
    for line in out.splitlines():
        parts = line.split()
        if len(parts) >= 2 and parts[1].startswith("v"):
            seen.add((parts[0], parts[1][1:]))
    return sorted(seen)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--target", default="x86_64-pc-windows-msvc")
    ap.add_argument("--out", type=Path, default=ROOT / "THIRD-PARTY-NOTICES.txt")
    a = ap.parse_args()
    meta = json.loads(subprocess.run(["cargo", "metadata", "--format-version", "1", "--locked"], cwd=RUST,
                                     capture_output=True, text=True, check=True).stdout)
    members = set(meta["workspace_members"])
    by_key = {(p["name"], p["version"]): p for p in meta["packages"]}
    parts = [
        "Third-party software in tmiki\n"
        "=============================\n\n"
        "The tmiki executable contains the following crates. Each is listed with\n"
        "its license expression and the license and notice files it ships.\n",
    ]
    missing = []
    count = 0
    for name, version in crates(a.target):
        p = by_key.get((name, version))
        vendored = "/vendor/" in p["manifest_path"].replace("\\", "/") if p else False
        if p is None or (p["id"] in members and not vendored):
            continue
        count += 1
        d = Path(p["manifest_path"]).parent
        files = sorted(f for f in d.iterdir() if f.is_file() and f.name.lower().startswith(NAMES))
        if p.get("license_file"):
            lf = d / p["license_file"]
            if lf.is_file() and lf not in files:
                files.append(lf)
        if name.endswith("-sys"):
            # the C library a -sys crate bundles carries its own terms
            for sub in sorted(x for x in d.rglob("*") if x.is_file() and len(x.relative_to(d).parts) <= 3):
                if sub.parent != d and sub.name.lower().startswith(NAMES + ("patents",)):
                    files.append(sub)
        head = f"\n{'-' * 78}\n{name} {version}\nLicense: {p.get('license') or '(see file)'}\n"
        if p.get("repository"):
            head += f"Source: {p['repository']}\n"
        parts.append(head)
        if not files:
            missing.append(f"{name} {version}")
            parts.append("(no license file in the package; the license expression above applies)\n")
        for f in files:
            parts.append(f"\n--- {f.relative_to(d).as_posix()} ---\n")
            parts.append(f.read_text(encoding="utf-8", errors="replace").rstrip() + "\n")
    a.out.write_text("".join(parts), encoding="utf-8", newline="\n")
    print(f"wrote {a.out} ({count} crates, {a.out.stat().st_size // 1024} KB)")
    if missing:
        print("without a license file:", ", ".join(missing))


if __name__ == "__main__":
    main()
