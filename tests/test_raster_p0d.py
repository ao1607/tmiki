"""PLUGINS.md P0d: the PNG / JPEG cases tmi used to leave to Pillow, read
by tmi-import / tmi-image / tmi-diffmerge in Rust, against Pillow 12.3.0.

- decode (``tmi-conform raster-dump``): APNG frame 0 (the fcTL rectangle,
  fdAT data, the default image), chunks after the image data (tRNS, pHYs,
  iCCP; up to an animation's next fcTL), short and damaged chunks, zlib
  streams that end early or carry a bad checksum, cut files; JPEG EXIF dpi
  over every value type and damaged directories, MPF (MPO or not), APP13
  Photoshop resources, short JFIF / Adobe / ICC markers. Pixels, size,
  dpi, ICC presence and the MPO verdict must equal Pillow's, or both refuse.
  ``--zip`` decodes every PNG through the ZipDecode port instead of the png
  crate.
- write-back (``tmi-conform writeback-dump``) against
  ``image_ops.encode_raster_like`` (which asks Pillow for these originals):
  the same bytes, or both fail.

JPEGs libjpeg warns about are still declined (RUST_MIGRATION.md, P0d).
Needs ``cargo build --release -p tmi-conform``; counts with
TMI_IMAGE_REPORT=1.
"""
from __future__ import annotations

import hashlib
import io
import json
import os
import struct
import subprocess
import sys
import tempfile
import unittest
import warnings
import zlib
from collections import Counter
from pathlib import Path

import numpy as np
from PIL import Image

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT))

from tmi import image_ops, raster  # noqa: E402

EXE = ROOT / "rust" / "target" / "release" / ("tmi-conform.exe" if os.name == "nt" else "tmi-conform")


def _report(line: str) -> None:
    if os.environ.get("TMI_IMAGE_REPORT"):
        print(f"\n  {line}")


def _external(name: str) -> Path | None:
    base = Path(os.environ.get("TMI_TEST_IMAGES") or ROOT / ".test-images")
    path = base / name
    return path if path.is_dir() else None


# ── generated cases ──


def _chunk(kind: bytes, body: bytes, crc_ok: bool = True) -> bytes:
    crc = zlib.crc32(kind + body) ^ (0 if crc_ok else 1)
    return struct.pack(">I", len(body)) + kind + body + struct.pack(">I", crc)


def _split(png: bytes) -> list[tuple[bytes, bytes]]:
    pos, out = 8, []
    while pos + 8 <= len(png):
        n, kind = struct.unpack(">I4s", png[pos:pos + 8])
        out.append((kind, png[pos + 8:pos + 8 + n]))
        pos += 12 + n
    return out


def _join(chunks) -> bytes:
    return b"\x89PNG\r\n\x1a\n" + b"".join(_chunk(k, b) for k, b in chunks)


def _save(img, fmt="PNG", **kw) -> bytes:
    buf = io.BytesIO()
    img.save(buf, fmt, **kw)
    return buf.getvalue()


def _with_marker(data: bytes, code: int, body: bytes) -> bytes:
    return data[:2] + bytes([0xFF, code]) + struct.pack(">H", len(body) + 2) + body + data[2:]


def _without_jfif(data: bytes) -> bytes:
    n = int.from_bytes(data[4:6], "big")
    return data[:2] + data[4 + n:]


_TYPES = {1: 1, 2: 1, 3: 2, 4: 4, 5: 8, 6: 1, 7: 1, 8: 2, 9: 4, 10: 8, 11: 4, 12: 8, 13: 4, 16: 8, 17: 8, 18: 8, 99: 4}


def _tiff(order: str, entries, *, next_ifd=True, count=None) -> bytes:
    e = "<" if order == "II" else ">"
    head = (b"II*\0" if order == "II" else b"MM\0*") + struct.pack(e + "L", 8)
    out = bytearray(head + struct.pack(e + "H", len(entries) if count is None else count))
    extra = bytearray()
    data_at = len(head) + 2 + 12 * len(entries) + (4 if next_ifd else 0)
    for tag, typ, cnt, value in entries:
        if cnt * _TYPES.get(typ, 4) > 4 and len(value) > 4:
            out += struct.pack(e + "HHLL", tag, typ, cnt, data_at + len(extra))
            extra += value
        else:
            out += struct.pack(e + "HHL", tag, typ, cnt) + value[:4].ljust(4, b"\0")
    if next_ifd:
        out += b"\0\0\0\0"
    return bytes(out + extra)


def _val(order: str, typ: int, v) -> bytes:
    e = "<" if order == "II" else ">"
    fmt = {3: "H", 4: "L", 8: "h", 9: "l", 13: "L", 16: "Q", 6: "b", 11: "f", 12: "d", 5: "LL", 10: "ll"}[typ]
    return struct.pack(e + fmt, *(v if isinstance(v, tuple) else (v,)))


def png_cases(rng: np.random.Generator) -> list[tuple[str, bytes]]:
    out: list[tuple[str, bytes]] = []
    bases: dict[str, bytes] = {}
    for mode in ("RGBA", "RGB", "P", "L", "LA", "1", "I;16"):
        frames = []
        for _ in range(3):
            im = Image.fromarray(rng.integers(0, 256, (17, 23, 4), dtype=np.uint8), "RGBA")
            im = im.convert("RGB").quantize(16) if mode == "P" else im.convert("L").convert("I;16") if mode == "I;16" else im.convert(mode)
            frames.append(im)
        kw = {"transparency": 3} if mode == "P" else {}
        m = mode.replace(";", "")
        for default in (False, True):
            data = _save(frames[0], save_all=True, append_images=frames[1:], default_image=default, duration=50, **kw)
            bases[f"apng_{m}_{int(default)}"] = data
        bases[f"png_{m}"] = _save(frames[0], **kw)
    for name, data in bases.items():
        out.append((name, data))
        ch = _split(data)
        kinds = [k for k, _ in ch]
        i_idat = kinds.index(b"IDAT")
        if name.startswith("apng"):
            w, h = struct.unpack(">II", ch[0][1][:8])
            for j, (k, b) in enumerate(ch):
                if k == b"fcTL" and j < i_idat:
                    for fw, fh, fx, fy in ((w - 3, h - 2, 1, 1), (5, 4, 2, 3), (0, 3, 0, 0), (w, h, 1, 0)):
                        nb = b[:4] + struct.pack(">IIII", fw, fh, fx, fy) + b[20:]
                        out.append((f"{name}_fctl0_{fw}x{fh}+{fx}+{fy}", _join(ch[:j] + [(k, nb)] + ch[j + 1:])))
            actl = [c for c in ch if c[0] == b"acTL"]
            rest = [c for c in ch if c[0] != b"acTL"]
            out.append((f"{name}_noactl", _join(rest)))
            i = [n for n, c in enumerate(rest) if c[0] == b"IDAT"][0]
            out.append((f"{name}_actl_after", _join(rest[:i + 1] + actl + rest[i + 1:])))
            out.append((f"{name}_actl_dup", _join(ch[:2] + actl + ch[2:])))
            for tag, body in (("zero", struct.pack(">II", 0, 0)), ("one", struct.pack(">II", 1, 0)), ("short", actl[0][1][:6])):
                out.append((f"{name}_actl_{tag}", _join([(k, body if k == b"acTL" else b) for k, b in ch])))
            seq = []
            for k, b in ch:
                n = struct.unpack(">I", b[:4])[0] if k in (b"fcTL", b"fdAT") else 0
                seq.append((k, struct.pack(">I", n + (n >= 1)) + b[4:]) if k in (b"fcTL", b"fdAT") else (k, b))
            out.append((f"{name}_seqgap", _join(seq)))
            out.append((f"{name}_noidat", _join([c for c in ch if c[0] != b"IDAT"])))
            extras = [(b"pHYs", struct.pack(">IIB", 5000, 5000, 1)), (b"iCCP", b"x\0\0" + zlib.compress(b"\0" * 140))]
            if "_P_" in name:
                extras.append((b"tRNS", bytes([0, 128, 255, 7])))
            out.append((f"{name}_extras_after_idat", _join(ch[:i_idat + 1] + extras + ch[i_idat + 1:])))
            nxt = [n for n, k in enumerate(kinds) if k == b"fcTL" and n > i_idat]
            if nxt:
                out.append((f"{name}_extras_after_fctl", _join(ch[:nxt[0] + 1] + extras + ch[nxt[0] + 1:])))
            for cut in (1, 6, 12, 13, 30):
                out.append((f"{name}_cut{cut}", data[:-cut]))
            continue
        ihdr = ch[0][1]
        raw_idat = b"".join(b for k, b in ch if k == b"IDAT")
        out.append((f"{name}_ihdr_comp1", _join([(b"IHDR", ihdr[:10] + b"\1" + ihdr[11:])] + ch[1:])))
        out.append((f"{name}_ihdr_interlace2", _join([(b"IHDR", ihdr[:12] + b"\2")] + ch[1:])))
        out.append((f"{name}_ihdr_long", _join([(b"IHDR", ihdr + b"\0")] + ch[1:])))
        out.append((f"{name}_ihdr_twice", _join([ch[0]] + ch)))
        for tag, body in (("nonul", b"\0" + zlib.compress(b"\1" * 200)), ("nonul_bad", b"\1abc"), ("nul_last", b"abc\0"),
                          ("method1", b"a\0\1" + zlib.compress(b"x")), ("damaged", b"a\0\0garbage"), ("empty", b"")):
            out.append((f"{name}_iccp_{tag}", _join(ch[:1] + [(b"iCCP", body)] + ch[1:])))
        parts = [raw_idat[i:i + 37] for i in range(0, len(raw_idat), 37)]
        out.append((f"{name}_idat_split", _join(ch[:i_idat] + [(b"IDAT", p) for p in parts] + ch[i_idat + 1:])))
        if len(parts) > 2:
            out.append((f"{name}_idat_ddat", _join(ch[:i_idat] + [(b"IDAT", parts[0]), (b"DDAT", b"".join(parts[1:]))] + ch[i_idat + 1:])))
            out.append((f"{name}_idat_junk_between",
                        _join(ch[:i_idat] + [(b"IDAT", parts[0]), (b"juNk", b"zz"), (b"IDAT", b"".join(parts[1:]))] + ch[i_idat + 1:])))
        raw = zlib.decompress(raw_idat)
        height = struct.unpack(">I", ihdr[4:8])[0]
        stride = len(raw) // height
        for tag, z in (("early", zlib.compress(raw[: stride * (height // 2)])), ("early_mid_row", zlib.compress(raw[: stride * 2 + 3])),
                       ("extra", zlib.compress(raw + b"\0" * 50)), ("badadler", raw_idat[:-4] + b"\0\0\0\0"),
                       ("noadler", raw_idat[:-4]), ("trailing", raw_idat + b"garbage")):
            out.append((f"{name}_zlib_{tag}", _join(ch[:i_idat] + [(b"IDAT", z)] + ch[i_idat + 1:])))
        out.append((f"{name}_badcrc_before", b"\x89PNG\r\n\x1a\n" + _chunk(*ch[0]) + _chunk(b"tEXt", b"a\0b", False)
                    + b"".join(_chunk(*c) for c in ch[1:])))
        out.append((f"{name}_badcrc_after", _join(ch[:-1]) + _chunk(b"tEXt", b"a\0b", False) + _chunk(b"IEND", b"")))
        trns = {"png_P": bytes([0, 9, 200]), "png_RGB": struct.pack(">HHH", 7, 7, 7), "png_L": struct.pack(">H", 50),
                "png_1": struct.pack(">H", 1), "png_I16": struct.pack(">H", 300)}.get(name)
        if trns:
            out.append((f"{name}_trns_after", _join(ch[:i_idat + 1] + [(b"tRNS", trns)] + ch[i_idat + 1:])))
        for kind, body in ((b"pHYs", b"\0" * 4), (b"gAMA", b"\0\0"), (b"sRGB", b""), (b"cHRM", b"\0" * 5), (b"fcTL", b"\0" * 10),
                           (b"fdAT", b"\0\0"), (b"acTL", b"\0" * 4), (b"IHDR", b"\0" * 5), (b"zTXt", b"k\0\1x"), (b"iTXt", b"k\0\1\0\0\0x")):
            out.append((f"{name}_after_{kind.decode()}_short", _join(ch[:i_idat + 1] + [(kind, body)] + ch[i_idat + 1:])))
        for cut in range(1, 17):
            out.append((f"{name}_tailcut{cut}", data[:-cut]))
        out.append((f"{name}_iend_first", _join(ch[:1] + [(b"IEND", b"")] + ch[1:])))
        # text chunks under the keys the write-back of a PNG into a .jpg reads
        out.append((f"{name}_text_keys", _join(ch[:1] + [(b"tEXt", b"comment\0hello \xe9"), (b"tEXt", b"progressive\0x"),
                                                        (b"iTXt", b"XML:com.adobe.xmp\0\0\0\0\0<x/>")] + ch[1:])))
    return out


def jpeg_cases(rng: np.random.Generator) -> list[tuple[str, bytes]]:
    out: list[tuple[str, bytes]] = []
    rgb = Image.fromarray(rng.integers(0, 256, (13, 29, 3), dtype=np.uint8), "RGB")
    base = _save(rgb, "JPEG", quality=80)
    plain = _without_jfif(base)
    units = [(3, 1, 2), (3, 1, 3), (3, 1, 1), (4, 1, 3), (5, 1, (3, 1)), (5, 1, (6, 2)), (5, 1, (3, 0)), (10, 1, (-6, -2)),
             (12, 1, 3.0), (2, 3, b"cm\0"), (2, 5, b"inch\0"), (2, 2, b"x\0"), (1, 1, b"\3"), (7, 1, b"\3"), (3, 2, None)]
    xress = [(5, 1, (300, 1)), (5, 1, (7, 3)), (5, 1, (5, 0)), (5, 1, (0, 5)), (10, 1, (-7, 2)), (3, 1, 144), (4, 1, 99),
             (9, 1, -5), (11, 1, 72.5), (12, 1, float("nan")), (12, 1, float("inf")), (13, 1, 96), (16, 1, 10 ** 12),
             (6, 1, -3), (8, 1, -9), (2, 4, b"300\0"), (2, 2, b"5\0"), (2, 1, b"\0"), (2, 3, b"x5\0"), (1, 2, b"\x96\x02"),
             (1, 2, b"\x96\x00"), (1, 1, b"\x96"), (7, 3, b"abc"), (17, 1, b"\0" * 8), (5, 2, None)]
    for order in ("II", "MM"):
        for ui, (ut, uc, uv) in enumerate(units):
            for xi, (xt, xc, xv) in enumerate(xress):
                if (ui + xi) % 3 and ui not in (0, 1) and xi not in (0, 1):
                    continue
                ub = _val(order, ut, 2) + _val(order, ut, 3) if uv is None else uv if isinstance(uv, bytes) else _val(order, ut, uv)
                xb = (_val(order, 5, (300, 1)) + _val(order, 5, (5, 1))) if xv is None else xv if isinstance(xv, bytes) else _val(order, xt, xv)
                exif = b"Exif\0\0" + _tiff(order, [(0x0128, ut, uc, ub), (0x011A, xt, xc, xb)])
                out.append((f"exif_{order}_u{ui}_x{xi}", _with_marker(plain, 0xE1, exif)))
    good = _tiff("II", [(0x0128, 3, 1, _val("II", 3, 2)), (0x011A, 5, 1, _val("II", 5, (600, 2))), (0x0131, 2, 20, b"software name long\0\0")])
    for cut in range(0, len(good) + 1, 3):
        out.append((f"exif_cut{cut}", _with_marker(plain, 0xE1, b"Exif\0\0" + good[:cut])))
    for head in (b"MM*\0\0\0\0\x08", b"II\0*\x08\0\0\0", b"II+\0\x08\0\0\0", b"MM\0+\0\0\0\x08", b"XX*\0\x08\0\0\0", b"II*\0\xff\0\0\0"):
        out.append((f"exif_head_{head.hex()}", _with_marker(plain, 0xE1, b"Exif\0\0" + head + good[8:])))
    out.append(("exif_count_big", _with_marker(plain, 0xE1, b"Exif\0\0" + _tiff("II", [(0x0128, 3, 1, _val("II", 3, 2))], count=40))))
    out.append(("exif_no_next", _with_marker(plain, 0xE1, b"Exif\0\0" + _tiff("II", [(0x0128, 3, 1, _val("II", 3, 3)),
                                                                                    (0x011A, 3, 1, _val("II", 3, 40))], next_ifd=False))))
    out.append(("exif_dup_tags", _with_marker(plain, 0xE1, b"Exif\0\0" + _tiff("II", [(0x011A, 3, 1, _val("II", 3, 40)), (0x0128, 3, 1, _val("II", 3, 3)),
                                                                                    (0x011A, 99, 1, b"zzzz"), (0x011A, 3, 0, b"")]))))
    out.append(("exif_two_segments", _with_marker(_with_marker(plain, 0xE1, b"Exif\0\0" + good[30:]), 0xE1, b"Exif\0\0" + good[:30])))
    out.append(("exif_exif_exif", _with_marker(plain, 0xE1, b"Exif\0\0Exif\0\0" + good)))
    out.append(("exif_empty", _with_marker(plain, 0xE1, b"Exif\0\0")))
    out.append(("exif_jfif_unit0", _with_marker(_with_marker(plain, 0xE1, b"Exif\0\0" + good), 0xE0, b"JFIF\0\1\1\0\0\x48\0\x48\0\0")))
    for n in range(4, 16):
        out.append((f"jfif_len{n}", _with_marker(plain, 0xE0, b"JFIF\0\1\2\1\1\x2c\1\x2c\0\0\0\0"[:n])))
        out.append((f"jfif_len{n}_cm", _with_marker(plain, 0xE0, b"JFIF\0\1\2\2\0\x76\0\x76\0\0\0\0"[:n])))
        out.append((f"adobe_len{n}", _with_marker(plain, 0xEE, b"Adobe\0\x64\0\0\0\0\x01\0\0\0\0"[:n])))
    res = b"8BIM\x03\xed\0\0\0\0\0\x10" + struct.pack(">IHHIHH", 200 << 16, 1, 0, 200 << 16, 1, 0) + b"8BIM\x04\x04\x05named\0\0\0\0\x04abcd"
    ps = b"Photoshop 3.0\0" + res
    for cut in range(14, len(ps) + 1):
        out.append((f"ps_cut{cut}", _with_marker(base, 0xED, ps[:cut])))
    icc = bytes(16) + b"RGB " + bytes(108)
    for tag, parts in (("ok", [b"ICC_PROFILE\0\1\1" + icc]), ("count2of1", [b"ICC_PROFILE\0\1\2" + icc]), ("short", [b"ICC_PROFILE\0\1"]),
                       ("two", [b"ICC_PROFILE\0\2\2" + icc[60:], b"ICC_PROFILE\0\1\2" + icc[:60]])):
        d = base
        for p in parts:
            d = _with_marker(d, 0xE2, p)
        out.append((f"icc_{tag}", d))
    second = Image.open(io.BytesIO(_save(rgb.transpose(Image.Transpose.FLIP_LEFT_RIGHT), "JPEG", quality=60)))
    out.append(("mpo_pillow", _save(rgb, "MPO", save_all=True, append_images=[second])))

    def mpf(order, quant, entries, qtype=4, etype=7, ecount=None):
        e = "<" if order == "II" else ">"
        ent = b"".join(struct.pack(e + "LLLHH", a, s, o, 0, 0) for a, s, o in entries)
        q = quant if qtype in (2, 7) else _val(order, qtype, quant)
        tags = [(0xB000, 7, 4, b"0100"), (0xB001, qtype, 1, q),
                (0xB002, etype, ecount if ecount is not None else len(ent), ent if etype == 7 else _val(order, etype, 5))]
        return b"MPF\0" + _tiff(order, tags)

    for order in ("II", "MM"):
        for tag, kw in (("ok2", dict(quant=2, entries=[(0x20030000, 100, 0), (0x00020002, 100, 500)])),
                        ("ok1", dict(quant=1, entries=[(0x20030000, 100, 0)])), ("zero", dict(quant=0, entries=[])),
                        ("short_entries", dict(quant=3, entries=[(0x20030000, 100, 0)])),
                        ("bad_format", dict(quant=2, entries=[(0x21030000, 100, 0), (0, 100, 500)])),
                        ("quant_short", dict(quant=2, entries=[(0x20030000, 100, 0), (0, 1, 2)], qtype=3)),
                        ("quant_rational", dict(quant=(2, 1), entries=[(0x20030000, 100, 0), (0, 1, 2)], qtype=5)),
                        ("quant_ascii", dict(quant=b"2\0", entries=[(0x20030000, 100, 0), (0, 1, 2)], qtype=2)),
                        ("entries_long", dict(quant=2, entries=[(0x20030000, 100, 0)], etype=4, ecount=1)),
                        ("quant_negative", dict(quant=-1, entries=[], qtype=9))):
            out.append((f"mpf_{order}_{tag}", _with_marker(base, 0xE2, mpf(order, **kw))))
    out.append(("mpf_no_b002", _with_marker(base, 0xE2, b"MPF\0" + _tiff("II", [(0xB001, 4, 1, _val("II", 4, 2))]))))
    out.append(("mpf_not_tiff", _with_marker(base, 0xE2, b"MPF\0XXXXXXXX")))
    out.append(("mpf_hdrgm", _with_marker(_with_marker(base, 0xE2, mpf("II", 2, [(0x20030000, 100, 0), (0, 100, 500)])),
                                          0xE1, b"http://ns.adobe.com/xap/1.0/\0<x hdrgm:Version=\"1.0\"/>")))
    for tag, prefix in (("ff_padding", b"\xff\xff\xff"), ("rst_before_sof", b"\xff\xd0"), ("soi_twice", b"\xff\xd8"),
                        ("bad_marker", b"\xff\x01"), ("com_negative_len", b"\xff\xfe\x00\x01")):
        out.append((tag, base[:2] + prefix + base[2:]))
    return out


def _cases() -> list[tuple[str, bytes]]:
    rng = np.random.default_rng(7)
    return [(f"{n}.png", d) for n, d in png_cases(rng)] + [(f"{n}.jpg", d) for n, d in jpeg_cases(rng)]


def _pillow_decode(data: bytes):
    """What import_raster_image read with Pillow, or None where it fails."""
    try:
        with warnings.catch_warnings(record=True) as caught:
            warnings.simplefilter("always")
            opened = Image.open(io.BytesIO(data))
            fmt = (opened.format or "").lower()
            image = opened.convert("RGBA")
    except Exception:  # noqa: BLE001
        return None
    malformed = any("malformed MPO" in str(w.message) for w in caught)
    dpi = image.info.get("dpi")
    dpi = (float(dpi[0]) if isinstance(dpi, tuple) else float(dpi)) if dpi else None
    return {"mpo": fmt == "mpo", "size": list(image.size), "dpi": dpi, "icc": "icc_profile" in image.info, "malformed_mpo": malformed,
            "rgba": hashlib.sha256(image.tobytes()).hexdigest()}


def _run(args: list[str], lines: list[str]) -> list[dict]:
    res = subprocess.run([str(EXE), *args], input="\n".join(lines).encode(), capture_output=True, check=True)
    return [json.loads(line) for line in res.stdout.decode().splitlines()]


@unittest.skipUnless(EXE.exists(), "tmi-conform not built (cargo build --release -p tmi-conform)")
class P0dDecodeTests(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.tmp = tempfile.TemporaryDirectory()
        base = Path(cls.tmp.name)
        cls.files: list[Path] = []
        for name, data in _cases():
            p = base / name
            p.write_bytes(data)
            cls.files.append(p)
        for name in ("pillow", "pngsuite"):
            ext = _external(name)
            if ext is not None:
                cls.files += sorted(p for p in ext.rglob("*") if p.is_file() and p.read_bytes()[:3] in (b"\x89PN", b"\xff\xd8\xff"))

    @classmethod
    def tearDownClass(cls) -> None:
        cls.tmp.cleanup()

    def check(self, args: list[str]) -> None:
        rows = {r["file"]: r for r in _run(["raster-dump", *args], [str(p) for p in self.files])}
        counts: Counter = Counter()
        cats: Counter = Counter()
        for p in self.files:
            got, want = rows[str(p)], _pillow_decode(p.read_bytes())
            cat = "pillow-images" if "pillow" in p.parts else "pngsuite" if "pngsuite" in p.parts else p.name.split("_")[0].split(".")[0]
            cats[(cat, "tmi decodes = Pillow" if want is not None and "rgba" in got else "both refuse" if want is None else "declined")] += 1
            if want is None:
                self.assertIn("declined", got, f"{p.name}: Pillow refuses, tmi decodes")
                counts["both refuse"] += 1
                continue
            if "declined" in got:
                # only JPEGs libjpeg warns about, and malformed MPOs (a
                # Pillow UserWarning), are left to Pillow
                self.assertTrue("Corrupt(" in got["declined"] or "malformed MPO" in got["declined"], f"{p.name}: tmi declines: {got['declined']}")
                counts["declined (libjpeg warns)" if "Corrupt(" in got["declined"] else "declined (malformed MPO)"] += 1
                self.assertEqual("malformed MPO" in got["declined"], want["malformed_mpo"], p.name)
                continue
            self.assertFalse(want["malformed_mpo"], f"{p.name}: Pillow warns of a malformed MPO, tmi decodes")
            want = {k: v for k, v in want.items() if k != "malformed_mpo"}
            self.assertEqual(want, {"mpo": got.get("mpo", False), **{k: got[k] for k in ("size", "dpi", "icc", "rgba")}}, p.name)
            counts["same"] += 1
        _report(f"P0d decode {' '.join(args) or '(png crate)'}: {dict(counts)}")
        _report("  by case: " + ", ".join(f"{c[0]}/{c[1]}={n}" for c, n in sorted(cats.items())))

    def test_decode_matches_pillow(self) -> None:
        self.check([])

    def test_zip_decode_matches_pillow(self) -> None:
        self.check(["--zip"])


@unittest.skipUnless(EXE.exists(), "tmi-conform not built (cargo build --release -p tmi-conform)")
class P0dWriteBackTests(unittest.TestCase):
    def test_writeback_matches_python(self) -> None:
        """tmi-diffmerge's encode_raster_like against image_ops'
        (Pillow reading the originals tmi.raster leaves to it): each
        original written back as PNG and as JPEG, with its own pixels and
        with grey ones."""
        files = [(n, d) for n, d in _cases()]
        ext = _external("pillow")
        if ext is not None:
            files += [(p.name, p.read_bytes()) for p in sorted(ext.rglob("*")) if p.is_file()
                      and p.read_bytes()[:3] in (b"\x89PN", b"\xff\xd8\xff") and p.stat().st_size < 400_000]
        rng = np.random.default_rng(3)
        with tempfile.TemporaryDirectory() as tmp:
            base = Path(tmp)
            reqs, cases = [], []
            # (a file with a broken PNG signature is another format: P0e)
            files = [(n, d) for n, d in files if d.startswith(raster.PNG_SIGNATURE) or d[:3] == b"\xff\xd8\xff"]
            for i, (name, data) in enumerate(files):
                orig = base / f"{i}.orig"
                orig.write_bytes(data)
                try:
                    with warnings.catch_warnings():
                        warnings.simplefilter("ignore")
                        im = Image.open(io.BytesIO(data)).convert("RGBA")
                    w, h, px = im.width, im.height, im.tobytes()
                    if w * h > 300 * 300:
                        raise ValueError
                except Exception:  # noqa: BLE001
                    w, h, px = 7, 5, rng.integers(0, 256, 140, dtype=np.uint8).tobytes()
                grey = bytes(b for p in range(w * h) for b in (px[4 * p],) * 3 + (255,))
                for vname, pixels in (("own", px), ("grey", grey)):
                    rp = base / f"{i}_{vname}.rgba"
                    rp.write_bytes(pixels)
                    for jpeg in (False, True):
                        out = base / f"{i}_{vname}_{int(jpeg)}.out"
                        reqs.append(f"{orig}\t{rp}\t{w}\t{h}\t{int(jpeg)}\t{out}")
                        cases.append((name, data, w, h, pixels, jpeg, out))
            rows = _run(["writeback-dump"], reqs)
            counts: Counter = Counter()
            for (name, data, w, h, pixels, jpeg, out), row in zip(cases, rows):
                label = f"{name} {'jpeg' if jpeg else 'png'}"
                try:
                    with warnings.catch_warnings():
                        warnings.simplefilter("ignore")
                        want = image_ops.encode_raster_like(raster.Rgba(w, h, pixels), data, "jpeg" if jpeg else "png")
                except Exception:  # noqa: BLE001
                    want = None
                # (a malformed MPO original is an opaque blob to tmiki: both write
                # as if there were no original, with no Pillow warning)
                if "error" in row:
                    # Rust hands to Python only what Python itself fails on
                    self.assertIsNone(want, f"{label}: tmi-diffmerge fails: {row['error']}")
                    counts["both fail"] += 1
                    continue
                self.assertIsNotNone(want, f"{label}: Python fails, tmi-diffmerge writes")
                self.assertEqual(want, out.read_bytes(), label)
                counts["same bytes"] += 1
            _report(f"P0d write-back: {dict(counts)}")


def damaged_jpegs() -> list[tuple[str, bytes]]:
    """Truncations, corrupted / inserted / deleted bytes, stray markers and
    damaged headers of JPEGs of every layout."""
    import random

    rng, r = np.random.default_rng(11), random.Random(5)

    def img(mode, w=64, h=48):
        a = rng.integers(0, 256, (h, w, 3), dtype=np.uint8)
        a = ((a // 4) + np.linspace(0, 255, w)[None, :, None] // 2).astype(np.uint8)
        return Image.fromarray(a, "RGB").convert(mode)

    bases = {"rgb420": _save(img("RGB"), "JPEG", quality=85), "rgb444": _save(img("RGB"), "JPEG", quality=85, subsampling=0),
             "rgb422": _save(img("RGB"), "JPEG", quality=60, subsampling=1), "gray": _save(img("L"), "JPEG", quality=85),
             "prog": _save(img("RGB"), "JPEG", quality=85, progressive=True), "cmyk": _save(img("CMYK"), "JPEG", quality=85),
             "big": _save(img("RGB", 200, 150), "JPEG", quality=90), "opt": _save(img("RGB"), "JPEG", quality=85, optimize=True)}

    def scan_start(d):
        p = 2
        while p < len(d):
            if d[p] != 0xFF:
                p += 1
                continue
            m = d[p + 1]
            if m == 0xDA:
                return p + 2 + struct.unpack(">H", d[p + 2:p + 4])[0]
            if m in (0xD8, 0x01) or 0xD0 <= m <= 0xD7 or m == 0xFF:
                p += 2
                continue
            p += 2 + struct.unpack(">H", d[p + 2:p + 4])[0]
        return len(d) // 2

    out = []
    for name, d in bases.items():
        s, e = scan_start(d), len(d) - 2
        out.append((f"{name}_orig", d))
        for k in range(25):
            b = bytearray(d)
            b[r.randrange(s, e)] = r.randrange(256)
            out.append((f"{name}_byte{k}", bytes(b)))
            b = bytearray(d)
            b[r.randrange(s, e)] ^= 1 << r.randrange(8)
            out.append((f"{name}_flip{k}", bytes(b)))
        for k in range(15):
            p, n, b = r.randrange(s, e - 8), r.randrange(2, 8), bytearray(d)
            b[p:p + n] = bytes(r.randrange(256) for _ in range(n))
            out.append((f"{name}_burst{k}", bytes(b)))
        for k in range(10):
            p = r.randrange(s, e)
            out.append((f"{name}_del{k}", d[:p] + d[p + r.randrange(1, 4):]))
            out.append((f"{name}_ins{k}", d[:p] + bytes(r.randrange(256) for _ in range(r.randrange(1, 4))) + d[p:]))
            out.append((f"{name}_marker{k}", d[:p] + bytes([0xFF, r.choice([0xD0, 0xD9, 0xDA, 0xC4, 0x01, 0xFF, 0x00, 0xE0, 0xFE])]) + d[p:]))
            p = r.randrange(s, e - 20)
            b = bytearray(d)
            b[p:p + 8] = b"\xff" * 8
            out.append((f"{name}_ffs{k}", bytes(b)))
        for cut in range(1, 40):
            out.append((f"{name}_trunc{cut}", d[:len(d) * cut // 40]))
        out.append((f"{name}_noeoi", d[:-2]))
        out.append((f"{name}_junk", d + b"junk after the end"))
        for k in range(20):
            b = bytearray(d)
            b[r.randrange(2, s)] = r.randrange(256)
            out.append((f"{name}_hdr{k}", bytes(b)))
    return out


@unittest.skipUnless(EXE.exists(), "tmi-conform not built (cargo build --release -p tmi-conform)")
class P0dDamagedJpegTests(unittest.TestCase):
    """What libjpeg-turbo warns about (or silently recovers from) in damaged
    JPEGs. tmi's build (mozjpeg-sys 2.2.3, no SIMD) is compared with Pillow's
    libjpeg-turbo 3.1.4.1: the default decode must equal Pillow's or decline
    (never other pixels); ``--lenient`` (accept the warned decodes) is a
    study, whose count of differing files is why they stay declined."""

    def test_damaged(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            paths = []
            for name, data in damaged_jpegs():
                p = Path(tmp) / f"{name}.jpg"
                p.write_bytes(data)
                paths.append(str(p))
            strict = {r["file"]: r for r in _run(["raster-dump"], paths)}
            lenient = {r["file"]: r for r in _run(["raster-dump", "--lenient"], paths)}
            counts: Counter = Counter()
            for p in paths:
                want = _pillow_decode(Path(p).read_bytes())
                s, lv = strict[p], lenient[p]
                if want is None:
                    self.assertIn("declined", s, f"{p}: Pillow refuses, tmi decodes")
                    counts["Pillow refuses (tmi declines)"] += 1
                elif "rgba" in s:
                    self.assertEqual(want["rgba"], s["rgba"], p)
                    counts["decoded, same as Pillow"] += 1
                else:
                    counts["declined; lenient same" if lv.get("rgba") == want["rgba"] else "declined; lenient differs or fails"] += 1
            _report(f"P0d damaged JPEGs ({len(paths)}): {dict(counts)}")
            self.assertGreater(counts["decoded, same as Pillow"], 100)


if __name__ == "__main__":
    unittest.main()
