"""tmi's MDP2 reader, layer decode, compositor and write-back (tmi_accel,
RUST_MIGRATION.md stage 2c) against the Python reference ``tmiref.mdp2``.

Everything is compared byte for byte: the parsed layer table, each layer's
pixels (``render_layer_crop``), whole composites over white and over
transparent, folder composites (``native.render_folder``'s subsets) and the
bytes write-back produces. Where the reference raises, tmi must raise too.
The inputs are the example files and seeded random files built here: every
layer type and blend mode, clipping, masks, hidden folders, odd colours,
offsets off the canvas, repeated, off-layer and partial tiles, damaged tile
streams, and XML that exercises expat's attribute normalisation and
CPython's ``int()``.
"""
from __future__ import annotations

import dataclasses
import random
import struct
import unittest
import zlib
from pathlib import Path

import numpy as np

PROJECT_ROOT = Path(__file__).resolve().parents[1]
EXAMPLES = sorted((PROJECT_ROOT / "example").glob("*.mdp"))

MODES = [
    "normal", "through", "mul", "add", "add2", "sub", "inverse", "div", "overlay", "screen", "light", "dark",
    "diff", "exclusion", "dodge", "burn", "ldodge", "lburn", "softlight", "hardlight", "hue", "saturation",
    "color", "luminosity", "hardmix", "bogus",
]
LAYER_FIELDS = (
    "name", "width", "height", "mode", "alpha", "layer_id", "parent_id", "bin_name", "type", "visible",
    "offset_x", "offset_y", "clipping", "protect_alpha", "folder_open", "bin_type", "color", "masking",
    "masking_type", "draft", "lumi", "transform_offset_x", "transform_offset_y", "halftone_type",
    "halftone_line", "text_lines", "attributes",
)


def pac_record(name: str, payload: bytes, *, declared: int | None = None) -> bytes:
    size = len(payload) if declared is None else declared
    header = bytearray(132)
    header[0:4] = b"PAC "
    struct.pack_into("<IIII", header, 4, 132 + size, 0, size, size)
    nb = name.encode("ascii")
    header[68:68 + len(nb)] = nb
    return bytes(header) + payload


def tile_stream(tiles: list[tuple[int, int, bytes]], *, damage: str | None = None, rng: random.Random | None = None) -> bytes:
    """``tiles`` as (col, row, raw) in a tile stream; ``damage`` breaks it."""
    if not tiles:
        return struct.pack("<I", 0)
    comps = [zlib.compress(raw) for _, _, raw in tiles]
    if damage == "truncate-zlib":
        comps[-1] = comps[-1][: len(comps[-1]) // 2]
    if damage == "bad-zlib":
        comps[0] = b"\x78\x9c" + bytes(10)
    buf = bytearray(struct.pack("<IIIIII", len(tiles), 128, tiles[0][0], tiles[0][1], 7 if damage == "first-zero" else 0, len(comps[0])))
    buf += comps[0]
    for (col, row, _), comp in zip(tiles[1:], comps[1:]):
        pad = (-len(buf)) % 4
        buf += (b"\x01" if damage == "bad-pad" and pad else b"\x00") * pad
        buf += struct.pack("<IIII", col, row, 5 if damage == "bad-zero" else 0, len(comp))
        buf += comp
    if damage != "no-final-pad":
        buf += b"\x00" * ((-len(buf)) % 4)
    if damage == "trailing":
        buf += b"\x00" * 8
    if damage == "tile-size":
        struct.pack_into("<I", buf, 4, 64)
    if damage == "short":
        buf = buf[: len(buf) // 2]
    return bytes(buf)


def build_mdp(canvas: tuple[int, int], layer_xml: list[str], blobs: list[tuple[str, bytes]], *,
              root: str = "Mdiapp", root_extra: str = "", prolog: str = "", layers_attrs: str = "") -> bytes:
    xml = (
        f'{prolog}<{root} width="{canvas[0]}" height="{canvas[1]}" dpi="72"{root_extra}>'
        f"<Layers{layers_attrs}>" + "".join(layer_xml) + f"</Layers></{root}>"
    ).encode("utf-8")
    pack = b"".join(pac_record(name, payload) for name, payload in blobs)
    return b"mdipack\x00" + struct.pack("<III", 0, len(xml), len(pack)) + xml + pack


def random_tile(kind: str, rng: np.random.Generator, style: str) -> bytes:
    if kind == "8bpp":
        v = rng.integers(0, 256, 128 * 128, dtype=np.uint8)
        if style != "dense":
            v[rng.random(128 * 128) < 0.7] = 0
        return v.tobytes()
    if kind == "1bpp":
        return rng.integers(0, 256, 128 * 128 // 8, dtype=np.uint8).tobytes()
    px = rng.integers(0, 256, (128, 128, 4), dtype=np.uint8)
    if style == "opaque":
        px[:, :, 3] = 255
    elif style == "sparse":
        px[:, :, 3] = 0
        for _ in range(int(rng.integers(1, 30))):
            px[rng.integers(0, 128), rng.integers(0, 128), 3] = rng.integers(1, 256)
    elif style == "clear":
        px[:, :, 3] = 0
    else:
        px[:, :, 3] = np.where(px[:, :, 3] < 80, 0, px[:, :, 3])
    return px.tobytes()


COLOURS = [None, "FF000000", "80FF0000", "#00FF00", "123456", "0x102030", " 40AABBCC ", "zz", "", "-1", "00FFFFFF", "FFFFFFFFFF"]


def random_file(seed: int) -> bytes:
    rng = random.Random(seed)
    nrng = np.random.default_rng(seed)
    canvas = (rng.randint(1, 420) if rng.random() < 0.1 else rng.randint(64, 420), rng.randint(32, 420))
    # Half the files start with an opaque normal layer, so that blend modes
    # meet a coloured backdrop rather than white.
    backdrop = rng.random() < 0.5
    n = rng.randint(1, 9)
    ids = list(range(n))
    if rng.random() < 0.15:
        ids[rng.randrange(n)] = ids[rng.randrange(n)]  # a repeated id
    folders = [i for i in range(n) if rng.random() < 0.25]
    xml, blobs = [], []
    for i in range(n):
        attrs: list[tuple[str, str]] = [("name", f"l{i}"), ("id", str(ids[i]))]
        parent = rng.choice([-1, -1, -1] + folders) if folders else -1
        if rng.random() < 0.05:
            parent = ids[i]  # its own parent
        attrs.append(("parentId", str(parent)))
        if i in folders:
            kind = "folder"
        else:
            kind = rng.choice(["32bpp", "32bpp", "32bpp", "text", "vector", "8bpp", "1bpp", "64bpp"])
        attrs.append(("type", kind))
        w = rng.choice([canvas[0], rng.randint(0, 500), 128, 256])
        h = rng.choice([canvas[1], rng.randint(0, 500), 128, 300])
        if rng.random() < 0.03:
            w = -5
        attrs += [("width", str(w)), ("height", str(h))]
        first = backdrop and i == 0
        if first:
            kind = "32bpp"
            replace = {"type": kind, "parentId": "-1", "id": str(n + 100)}
            attrs = [(k, replace.get(k, v)) for k, v in attrs]
        attrs.append(("mode", "normal" if first else rng.choice(MODES)))
        attrs.append(("alpha", "255" if first else str(rng.choice([255, 255, 255, 0, 1, 128, 254, 300, -20]))))
        if rng.random() < 0.2 and not first:
            attrs.append(("visible", rng.choice(["false", "true", "False"])))
        if rng.random() < 0.3 and not first:
            attrs.append(("clipping", rng.choice(["true", "true", "TRUE"])))
        if rng.random() < 0.06 and not first:
            attrs.append(("masking", rng.choice(["true", "True", "false"])))
        if rng.random() < 0.7 and not first:
            attrs.append(("ofsx", str(rng.randint(-100, 200))))
            attrs.append(("ofsy", str(rng.randint(-100, 200))))
        elif rng.random() < 0.5:
            attrs.append(("tofsx", str(rng.randint(-50, 50))))
        if kind in ("8bpp", "1bpp") or rng.random() < 0.1:
            colour = rng.choice(COLOURS)
            if colour is not None:
                attrs.append(("color", colour))
        bin_name = f"layer{i}img"
        if rng.random() < 0.05:
            bin_name = f"layer{max(0, i - 1)}img"  # shares a blob name
        if rng.random() < 0.95:
            attrs.append(("bin", bin_name))
        if kind != "folder":
            cols = max(1, (max(w, 1) + 127) // 128)
            rows = max(1, (max(h, 1) + 127) // 128)
            tile_kind = "8bpp" if kind == "8bpp" else "1bpp" if kind == "1bpp" else "32bpp"
            cells = [(c, r) for r in range(rows + 1) for c in range(cols + 1)]
            chosen = rng.sample(cells, rng.randint(0, min(len(cells), 6)))
            if chosen and rng.random() < 0.2:
                chosen.append(rng.choice(chosen))  # a repeated cell, composited in order
            if rng.random() < 0.1:
                chosen.append((rng.randint(5, 9), rng.randint(0, 2)))  # off the layer
            style = "opaque" if first else rng.choice(["dense", "sparse", "opaque", "clear", "dense"])
            if first:
                chosen = [(c, r) for r in range(rows) for c in range(cols)]
            tiles = [(c, r, random_tile(tile_kind, nrng, style)) for c, r in chosen]
            damage = None
            if tiles and rng.random() < 0.05 and not first:
                damage = rng.choice(["truncate-zlib", "bad-zlib", "first-zero", "bad-pad", "bad-zero", "no-final-pad",
                                     "trailing", "tile-size", "short"])
            if tiles and rng.random() < 0.05:
                # a 32bpp tile in an 8bpp layer (or the reverse): wrong size
                tiles[0] = (tiles[0][0], tiles[0][1], random_tile("8bpp" if tile_kind == "32bpp" else "32bpp", nrng, "dense"))
            blobs.append((f"layer{i}img", tile_stream(tiles, damage=damage)))
        elif rng.random() < 0.5:
            blobs.append((f"layer{i}img", struct.pack("<I", 0)))
        xml.append("<Layer " + " ".join(f'{k}="{v}"' for k, v in attrs) + "/>")
    rng.shuffle(blobs)
    return build_mdp(canvas, xml, blobs)


def _call(fn, *args, **kwargs):
    try:
        return ("ok", fn(*args, **kwargs))
    except Exception as exc:  # noqa: BLE001 - both sides must agree that it fails
        return ("raised", f"{type(exc).__name__}: {exc}")


class Mdp2ParityTests(unittest.TestCase):
    def setUp(self) -> None:
        from tmi.formats import mdp2 as T
        from tmiref import mdp2 as R

        self.T, self.R = T, R

    # ── comparisons ──

    def assert_same_parse(self, raw: bytes, label: str):
        got = _call(self.T.parse_mdp2, raw)
        want = _call(self.R.parse_mdp2, raw)
        self.assertEqual(want[0], got[0], f"{label}: parse {want[1] if want[0] == 'raised' else got[1]}")
        if want[0] != "ok":
            return None, None
        t, r = got[1], want[1]
        for name in ("reserved", "xml_size", "pack_size", "canvas_width", "canvas_height", "dpi", "xml_text"):
            self.assertEqual(getattr(r, name), getattr(t, name), f"{label}: {name}")
        self.assertEqual([dataclasses.astuple(b) for b in r.blobs], [dataclasses.astuple(b) for b in t.blobs], label)
        self.assertEqual(len(r.layers), len(t.layers), label)
        for lt, lr in zip(t.layers, r.layers):
            for name in LAYER_FIELDS:
                self.assertEqual(getattr(lr, name), getattr(lt, name), f"{label}: layer {lr.layer_id} {name}")
            self.assertEqual([r.layers.index(c) for c in lr.children], [t.layers.index(c) for c in lt.children], label)
            self.assertEqual(self.R.is_renderable_raster(r, lr), self.T.is_renderable_raster(t, lt), f"{label}: renderable")
        return t, r

    def assert_same_layers(self, t, r, label: str) -> None:
        for lt, lr in zip(t.layers, r.layers):
            if not self.R.is_renderable_raster(r, lr):
                continue
            got = _call(self.T.render_layer_crop, t, lt)
            want = _call(self.R.render_layer_crop, r, lr)
            self.assertEqual(want[0], got[0], f"{label}: layer {lr.layer_id}: {want[1] if want[0] == 'raised' else got[1]}")
            if want[0] == "ok":
                a, b = got[1], want[1]
                self.assertEqual((b.size, b.rect), (a.size, a.rect), f"{label}: layer {lr.layer_id}")
                self.assertEqual(b.rgba, a.rgba, f"{label}: layer {lr.layer_id} pixels")
                self.assertEqual(b.digest(), a.digest())

    def assert_same_composite(self, t, r, label: str, background=(255, 255, 255, 255)) -> None:
        got = _call(self.T.render_composite_rgba, t, background)
        want = _call(self.R.render_composite_rgba, r, background)
        self.assertEqual(want[0], got[0], f"{label}: composite {want[1] if want[0] == 'raised' else got[1]}")
        if want[0] == "ok":
            self.assertEqual(want[1].size, got[1].size, label)
            if want[1].tobytes() != got[1].tobytes():
                diff = np.argwhere(np.any(np.asarray(want[1]) != np.asarray(got[1]), axis=-1))
                self.fail(f"{label}: composite differs at {len(diff)} pixels, first {diff[:3].tolist()}")

    def assert_same_folders(self, t, r, label: str) -> None:
        """``native.render_folder``'s subset of each folder."""
        def subset(doc, folder):
            by_id = {layer.layer_id: layer for layer in doc.layers}

            def inside(layer) -> bool:
                seen = set()
                cur = by_id.get(layer.parent_id)
                while cur is not None and cur.layer_id not in seen:
                    if cur.layer_id == folder.layer_id:
                        return True
                    seen.add(cur.layer_id)
                    cur = by_id.get(cur.parent_id)
                return False

            return dataclasses.replace(doc, layers=[layer for layer in doc.layers if inside(layer)])

        for ft, fr in zip(t.layers, r.layers):
            if fr.children:
                self.assert_same_composite(subset(t, ft), subset(r, fr), f"{label}: folder {fr.layer_id}", (0, 0, 0, 0))

    def check_file(self, raw: bytes, label: str) -> None:
        t, r = self.assert_same_parse(raw, label)
        if t is None:
            return
        self.assert_same_layers(t, r, label)
        self.assert_same_composite(t, r, label)
        self.assert_same_composite(t, r, label, (0, 0, 0, 0))
        self.assert_same_folders(t, r, label)

    # ── tests ──

    def test_example_files(self) -> None:
        if not EXAMPLES:
            self.skipTest("no example MDP2 files")
        for path in EXAMPLES:
            with self.subTest(path=path.name):
                self.check_file(path.read_bytes(), path.name)

    def test_random_files(self) -> None:
        for seed in range(400):
            with self.subTest(seed=seed):
                self.check_file(random_file(seed), f"seed {seed}")

    def test_far_tiles_and_sizes(self) -> None:
        """Tile cells whose pixels lie past 2^31 (Pillow's C ints overflow in
        ``render_layer_rgba``, not in the compositor's decode), negative and
        zero layer sizes, and alphas below C long."""
        rng = np.random.default_rng(3)
        tile = random_tile("32bpp", rng, "dense")
        for cell in [(16777214, 0), (16777215, 0), (0, 16777215), (2**32 - 1, 2**32 - 1)]:
            for size in [(200, 200), (0, 128), (128, 0), (-1, 128)]:
                for alpha in ("255", "100", "-2147483648", "-2147483649"):
                    raw = build_mdp(
                        (256, 256),
                        [f'<Layer name="a" id="1" parentId="-1" type="32bpp" width="{size[0]}" height="{size[1]}" '
                         f'alpha="{alpha}" bin="b"/>'],
                        [("b", tile_stream([(0, 0, tile), (*cell, tile)]))],
                    )
                    with self.subTest(cell=cell, size=size, alpha=alpha):
                        self.check_file(raw, f"cell {cell} size {size} alpha {alpha}")

    def test_payload_shapes(self) -> None:
        """Empty sentinels, short and overrunning payloads, a blob record that
        runs past the end of the file, and tile streams on folders."""
        rng = np.random.default_rng(4)
        tile = zlib.compress(random_tile("32bpp", rng, "dense"))
        one = struct.pack("<IIIIII", 1, 128, 0, 0, 0, len(tile)) + tile
        one += b"\x00" * ((-len(one)) % 4)
        payloads = [b"", b"\x00", struct.pack("<I", 0), struct.pack("<I", 3), bytes(20), bytes(24), one, one[:30], one + b"\x00"]
        for kind in ("32bpp", "8bpp", "folder", "unknown", "text"):
            for i, payload in enumerate(payloads):
                for declared in (None, len(payload) + 40):
                    xml = [f'<Layer name="a" id="1" parentId="-1" type="{kind}" width="128" height="128" bin="b"/>']
                    xml_bytes = (
                        '<Mdiapp width="128" height="128"><Layers>' + "".join(xml) + "</Layers></Mdiapp>"
                    ).encode()
                    pack = pac_record("b", payload, declared=declared)
                    raw = b"mdipack\x00" + struct.pack("<III", 0, len(xml_bytes), len(pack)) + xml_bytes + pack
                    with self.subTest(kind=kind, payload=i, declared=declared):
                        self.check_file(raw, f"{kind} payload {i} declared {declared}")

    def test_containers(self) -> None:
        good = random_file(7)
        cases = {
            "short": good[:15],
            "magic": b"mdipack!" + good[8:],
            "sizes": good[:12] + struct.pack("<I", 1) + good[16:],
            "no-pac": good + b"XXXX",
            "pac-short": good + b"PAC " + bytes(8),
            "record-small": good + b"PAC " + struct.pack("<IIII", 10, 0, 0, 0) + bytes(112),
            "name-not-ascii": good + pac_record("b", b"")[:68] + b"\xff" + bytes(63),
            "size-mismatch": good + b"PAC " + struct.pack("<IIII", 140, 0, 4, 4) + bytes(120),
        }
        for label, raw in cases.items():
            if label not in ("short", "magic", "sizes"):
                # keep the header's pack size in step with what was appended
                raw = raw[:16] + struct.pack("<I", len(raw) - 20 - struct.unpack("<I", raw[12:16])[0]) + raw[20:]
            with self.subTest(case=label):
                self.check_file(raw, label)

    def test_xml_normalisation_and_ints(self) -> None:
        """Attribute values the way expat hands them to ElementTree, and
        integers the way CPython's int() reads them."""
        values = [
            "a&#10;b", "a&#x9;b&#13;c", "a\tb\nc\r\nd\re", "  sp  ", "&amp;&lt;&gt;&quot;&apos;", "&#x1F600;",
            "１２", " 12 ", "+3", "1_000", "1__0", "_5", "0x1f", "٣", "　" + "7", "1e3", "", "9" * 30,
            "12 ", " 7", "-0", "007",
        ]
        for value in values:
            for attr in ("ofsx", "width", "alpha", "id", "maskingType", "name", "color", "halftoneLine"):
                xml = [f'<Layer name="n" id="1" parentId="-1" type="32bpp" width="64" height="64" {attr}="{value}"/>'
                       if attr not in ("name", "id", "width") else
                       f'<Layer {attr}="{value}" parentId="-1" type="32bpp" height="64"/>']
                raw = build_mdp((64, 64), xml, [])
                with self.subTest(attr=attr, value=value):
                    self.assert_same_parse(raw, f"{attr}={value!r}")
        documents = [
            ("bom", "﻿", "", ""),
            ("decl", '<?xml version="1.0" encoding="UTF-8"?>', "", ""),
            ("decl-sjis", '<?xml version="1.0" encoding="Shift_JIS"?>', "", ""),
            ("comment", "<!-- c --><?pi x?>", "", ""),
            ("ns-attr", "", ' xmlns:a="urn:a"', ""),
            ("default-ns", "", ' xmlns="urn:d"', ""),
            ("layers-ns", "", ' xmlns:a="urn:a"', ' xmlns="urn:l"'),
            ("bare-doctype", "<!-- c --><!DOCTYPE Mdiapp >", "", ""),
        ]
        layer = ('<Layer name="x&#10;y" id="1" parentId="-1" type="32bpp" width="64" height="64" '
                 'a:q="1" {extra}><Line text="t1"/><Line/><a:Line text="no"/></Layer>')
        for label, prolog, root_extra, layers_attrs in documents:
            body = layer.format(extra="")
            if "xmlns:a" not in root_extra:
                body = body.replace(' a:q="1"', "").replace('<a:Line text="no"/>', "")
            raw = build_mdp((64, 64), [body], [], prolog=prolog, root_extra=root_extra, layers_attrs=layers_attrs)
            with self.subTest(document=label):
                self.assert_same_parse(raw, label)
        # A DTD's internal subset (entities, attribute defaults) or external
        # id: expat applies them, tmi refuses the file (no MDP2 writer emits
        # one; RUST_MIGRATION.md "2c の結果").
        for prolog in ('<!DOCTYPE Mdiapp [<!ENTITY e "val">]>', '<!DOCTYPE Mdiapp [<!ATTLIST Layer extra CDATA "d">]>',
                       '<!DOCTYPE Mdiapp SYSTEM "x.dtd">'):
            raw = build_mdp((64, 64), [layer.format(extra="").replace(' a:q="1"', "").replace('<a:Line text="no"/>', "")],
                            [], prolog=prolog)
            with self.subTest(prolog=prolog):
                with self.assertRaisesRegex(ValueError, "document type definition"):
                    self.T.parse_mdp2(raw)
        broken = [
            '<Layer name="a" name="b"/>',
            '<Layer name="a<b"/>',
            '<Layer name="&undefined;"/>',
            '<Layer name="a"></Layr>',
            '<Layer x:y="1"/>',
            '<Layer name="\x01"/>',
            "<Layer name='single' other=\"double\"/>",
        ]
        for body in broken:
            raw = build_mdp((64, 64), [body], [])
            with self.subTest(body=body):
                self.assert_same_parse(raw, body)
        for root_attrs in ['width="5"', 'height="5"', 'width="x" height="5"', 'width="5" height="5" dpi="z"', 'width=" 5" height="5_0" dpi="+7"']:
            xml = f"<R {root_attrs}><Layers/></R>".encode()
            raw = b"mdipack\x00" + struct.pack("<III", 0, len(xml), 0) + xml
            with self.subTest(root=root_attrs):
                self.assert_same_parse(raw, root_attrs)
        bad_utf8 = b"<R width=\"1\" height=\"1\" n=\"\xff\"/>"
        raw = b"mdipack\x00" + struct.pack("<III", 0, len(bad_utf8), 0) + bad_utf8
        self.assert_same_parse(raw, "bad utf-8")


class Mdp2WriteBackParityTests(unittest.TestCase):
    """``replace_layer_pixels``: tmi (tmi_accel, stock zlib 1.3.1) writes the
    bytes the reference (Pillow + CPython's zlib) writes."""

    def setUp(self) -> None:
        import tempfile

        from tmi.formats import mdp2 as T
        from tmiref import mdp2 as R

        self.T, self.R = T, R
        root = PROJECT_ROOT / ".test-tmp"
        root.mkdir(exist_ok=True)
        self.tmp = Path(tempfile.mkdtemp(prefix="tmi-mdp2-wb-", dir=root))

    def tearDown(self) -> None:
        import shutil

        shutil.rmtree(self.tmp, ignore_errors=True)
        try:
            (PROJECT_ROOT / ".test-tmp").rmdir()
        except OSError:
            pass

    def test_zlib_is_cpythons(self) -> None:
        from tmi import accel

        test = accel.native()._test
        if test.zlib_version() != zlib.ZLIB_RUNTIME_VERSION:
            self.skipTest(f"Python's zlib is {zlib.ZLIB_RUNTIME_VERSION}, tmi's {test.zlib_version()}")
        rng = np.random.default_rng(11)
        inputs = [b"", b"a", bytes(65536), bytes(5 * 65536), b"abc" * 30000]
        for n in (1, 7, 100, 4096, 65536, 81920, 327680, 1 << 20):
            inputs.append(rng.integers(0, 256, n, dtype=np.uint8).tobytes())
            inputs.append(rng.integers(0, 4, n, dtype=np.uint8).tobytes())
            sparse = np.zeros(n, np.uint8)
            sparse[rng.integers(0, n, max(1, n // 50))] = rng.integers(1, 256, max(1, n // 50), dtype=np.uint8)
            inputs.append(sparse.tobytes())
        for path in EXAMPLES[:1]:
            raw = path.read_bytes()
            inputs += [raw[i:i + 65536] for i in range(0, min(len(raw), 40 * 65536), 65536)]
        for data in inputs:
            for level in (-1, 0, 1, 6, 9):
                self.assertEqual(zlib.compress(data, level), test.zlib_compress(data, level), (len(data), level))

    def _png(self, image) -> bytes:
        import io

        buf = io.BytesIO()
        image.save(buf, "PNG")
        return buf.getvalue()

    def check_write(self, raw: bytes, selector: str, png: bytes, label: str) -> None:
        path = self.tmp / "art.mdp"
        if getattr(self, "_written", None) is not raw:
            path.write_bytes(raw)
            self._written = raw
        got = _call(self.T.replace_layer_pixels, path, selector, png)
        want = _call(self.R.replace_layer_pixels, path, selector, png)
        self.assertEqual(want[0], got[0], f"{label}: {want[1] if want[0] == 'raised' else got[1]}")
        if want[0] == "ok":
            self.assertTrue(want[1] == got[1], f"{label}: write-back bytes differ")

    def replacements(self, mdp, layer, rng: np.random.Generator):
        """Images to write into ``layer``: unchanged, edited, smaller, empty,
        colour under zero alpha, and random."""
        from PIL import Image

        current = self.R.render_layer_rgba(mdp, layer, apply_opacity=False)
        yield "same", current
        w, h = current.size
        arr = np.asarray(current).copy()
        if w and h:
            y0, x0 = rng.integers(0, h), rng.integers(0, w)
            arr[y0:y0 + 70, x0:x0 + 90] = rng.integers(0, 256, arr[y0:y0 + 70, x0:x0 + 90].shape, dtype=np.uint8)
        yield "edited", Image.fromarray(arr, "RGBA")
        if w > 3 and h > 3:
            yield "smaller", current.crop((0, 0, w - 3, h - 2))
        yield "empty", Image.new("RGBA", (w, h), (0, 0, 0, 0))
        hidden = rng.integers(0, 256, (h, w, 4), dtype=np.uint8)
        hidden[:, :, 3] = np.where(rng.random((h, w)) < 0.5, 0, hidden[:, :, 3])
        yield "random", Image.fromarray(hidden, "RGBA")
        hidden[:, :, 3] = 0
        yield "colour-under-zero-alpha", Image.fromarray(hidden, "RGBA")

    def check_file(self, raw: bytes, label: str, rng: np.random.Generator) -> None:
        try:
            mdp = self.R.parse_mdp2(raw)
        except Exception:
            return
        for layer in mdp.layers:
            if layer.width > 2000 or layer.height > 2000 or layer.width < 0 or layer.height < 0:
                continue
            try:
                images = list(self.replacements(mdp, layer, rng))
            except Exception:
                images = [("plain", None)]
            for name, image in images:
                if image is None:
                    from PIL import Image

                    image = Image.new("RGBA", (max(layer.width, 1), max(layer.height, 1)), (1, 2, 3, 4))
                if not image.width or not image.height:
                    continue  # a PNG cannot be empty
                with self.subTest(file=label, layer=layer.layer_id, image=name):
                    self.check_write(raw, f"mdp2:{layer.layer_id}", self._png(image), f"{label} layer {layer.layer_id} {name}")

    def test_example_files(self) -> None:
        if not EXAMPLES:
            self.skipTest("no example MDP2 files")
        rng = np.random.default_rng(1)
        for path in EXAMPLES:
            raw = path.read_bytes()
            if len(raw) > 5_000_000:
                # kakenhi-kaguya: a few layers, not all (each write re-emits 24 MB).
                mdp = self.R.parse_mdp2(raw)
                for layer in [layer for layer in mdp.layers if self.R.is_renderable_raster(mdp, layer)][:1]:
                    for name, image in self.replacements(mdp, layer, rng):
                        if name in ("same", "smaller"):
                            with self.subTest(file=path.name, layer=layer.layer_id, image=name):
                                self.check_write(raw, f"mdp2:{layer.layer_id}", self._png(image), f"{path.name} {name}")
                continue
            self.check_file(raw, path.name, rng)

    def test_random_files(self) -> None:
        rng = np.random.default_rng(2)
        for seed in range(0, 400, 13):
            self.check_file(random_file(seed), f"seed {seed}", rng)


if __name__ == "__main__":
    unittest.main()
