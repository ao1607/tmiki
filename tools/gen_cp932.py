"""Write rust/crates/tmi-cli/src/cp932.bin: Python's cp932 codec as a table.

Every code point Python's ``str.encode("cp932")`` encodes, with its bytes:
little-endian records of a u32 code point and a u16 code (one byte when
below 0x100), sorted by code point. The Rust front door writes text to a
stdout / stderr Python would encode as cp932 (Windows with the ANSI code
page 932, not a console) with it, and backslashreplace for the rest, as
tmi.cli.main configures the streams (RUST_MIGRATION.md 4b).

    python tools/gen_cp932.py          # rewrite the file
    python tools/gen_cp932.py --check  # exit 1 if it is out of date
"""
from __future__ import annotations

import argparse
import struct
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
OUT = ROOT / "rust" / "crates" / "tmi-cli" / "src" / "cp932.bin"


def render() -> bytes:
    out = bytearray()
    for cp in range(0x110000):
        if 0xD800 <= cp <= 0xDFFF:
            continue
        try:
            data = chr(cp).encode("cp932")
        except UnicodeEncodeError:
            continue
        code = data[0] if len(data) == 1 else (data[0] << 8) | data[1]
        assert len(data) <= 2 and (len(data) == 1) == (code < 0x100), (cp, data)
        out += struct.pack("<IH", cp, code)
    return bytes(out)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    data = render()
    if args.check:
        if not OUT.exists() or OUT.read_bytes() != data:
            print(f"{OUT} is out of date: python tools/gen_cp932.py")
            return 1
        print(f"cp932 table up to date ({len(data) // 6} code points)")
        return 0
    OUT.write_bytes(data)
    print(f"wrote {OUT} ({len(data) // 6} code points)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
