"""Property tests of tmi's CLIP renderer (tmi_accel) against the reference
renderer ``tmiref`` (RUST_MIGRATION.md 1a, done criterion 4): example files
with their layers' blend mode, opacity, clipping and visibility (eye and mask
bits) rewritten at random render to the same bytes, and print the same
warnings, through both. Stages 1b / 1c add random フチ and トーン (on layers
and folders), gradient fills and image material placements; 1e damaged
offscreens (tiles that do not inflate, unknown tile encodings, repeated and
oversized tiles), which both warn about and leave out alike."""
from __future__ import annotations

import contextlib
import io
import os
import random
import struct
from tmiref import clip_io  # the Python write-back helpers that build test files
import unittest
import zlib
from pathlib import Path

PROJECT_ROOT = Path(__file__).resolve().parents[1]
EXAMPLES = PROJECT_ROOT / "example"
MODES = [0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 30, 36, 99]


def _patch_layers(raw: bytes, updates: dict[int, dict], add_columns: dict[str, str] | None = None,
                  statements: list = ()) -> bytes:
    """``raw`` with Layer columns rewritten (metadata only); ``add_columns``
    (name -> SQL type) are added to the Layer table first where missing, and
    any other ``(sql, params)`` statements run."""
    from tmi.formats import clip as C

    clip = C.open_clip_bytes(raw)
    con = clip_io._clone_sqlite_connection(clip)
    present = {row[1] for row in con.execute("PRAGMA table_info(Layer)")}
    for name, kind in (add_columns or {}).items():
        if name not in present:
            con.execute(f"ALTER TABLE Layer ADD COLUMN {name} {kind}")
    for main_id, cols in updates.items():
        sets = ", ".join(f"{k} = ?" for k in cols)
        con.execute(f"UPDATE Layer SET {sets} WHERE MainId = ?", (*cols.values(), main_id))
    for sql, params in statements:
        con.execute(sql, params)
    con.commit()
    clip.db.close()
    container = C.parse_container(raw)
    payloads = {c.file_off: raw[c.payload_off:c.payload_off + c.payload_size] for c in container.chunks}
    try:
        return clip_io._emit_container_with_sqli_payload(container, payloads, clip_io._serialize_sqlite(con))
    finally:
        con.close()


def _effect_blob(edge=None, tone=None) -> bytes:
    """A LayerEffectInfo with a フチ ``(width, (r, g, b))`` and / or a トーン
    ``(lpi, angle, shape id)``, enabled (the sections clip_style /
    clip_tone read)."""
    import struct

    def key(name: str) -> bytes:
        return struct.pack(">I", len(name)) + name.encode("utf-16-be")

    out = b""
    if edge is not None:
        width, rgb = edge
        out += key("EffectEdge") + struct.pack(">Id3I", 1, width, *(c * 0x01010101 for c in rgb))
    if tone is not None:
        lpi, angle, shape = tone
        out += key("EffectTone") + struct.pack(">Id3Id4I", 1, 72.0, shape, 0, 20, lpi, 1, 0, angle, 30) + bytes(20)
    return out


def _renderer(name: str):
    """The module that renders: tmi's own ("rust") or the reference ("python")."""
    if name == "python":
        from tmiref import clip as R

        return R
    from tmi.formats import clip as C

    return C


def _forget_warnings() -> None:
    from tmi.formats import clip as C
    from tmiref import clip as R

    for seen in (C._warned_composites, C._warned_filters, C._warned_offscreens, C._warned_tile_sizes, R._warned_layers):
        seen.clear()


def _render(raw: bytes, name: str, region=None, decodable=False, failures: list | None = None) -> tuple[bytes, list[str]]:
    """(RGBA bytes, the warnings printed) of one render, warnings de-duplicated
    afresh. ``failures``: a list to append the render's decode failures to."""
    from tmi.formats import clip as C

    _forget_warnings()
    clip = C.open_clip_bytes(raw)
    clip.decode_failures = set()
    if decodable:
        clip.decodable_layers = frozenset(n.main_id for n in C.walk_layer_nodes(clip.root.children))
    err = io.StringIO()
    try:
        with contextlib.redirect_stderr(err):
            image = _renderer(name).render_layered_composite_rgba(clip, region=region)
    finally:
        clip.db.close()
    if failures is not None:
        failures.append(sorted(clip.decode_failures))
    return image.tobytes(), [line for line in err.getvalue().splitlines() if line.startswith("tmi:")]


class _ParityCase(unittest.TestCase):
    def setUp(self) -> None:
        # Whole renders, never an incremental one: TMI_VERIFY_PARTIAL (the
        # "verify" variant of tools/run_tests.py) changes nothing here.
        if os.environ.get("TMI_VERIFY_PARTIAL", "").strip().lower() in {"1", "true", "yes", "on"}:
            self.skipTest("unaffected by TMI_VERIFY_PARTIAL (run in the default variant)")


class RenderParityTests(_ParityCase):
    def _check(self, name: str, rounds: int, seed: int, vectors: bool = False) -> None:
        """``vectors``: also move layers (LayerOffsetX/Y: vector strokes and
        comic frames follow) and rewrite the strokes' brushes (透明色,
        flattened tips)."""
        from tmi.formats import clip as C

        path = EXAMPLES / name
        if not path.exists():
            self.skipTest(f"{name} not present")
        base = path.read_bytes()
        clip = C.open_clip_bytes(base)
        nodes = C.walk_layer_nodes(clip.root.children)
        w, h = clip.canvas.width, clip.canvas.height
        brushes = [row[0] for row in clip.db.execute("SELECT MainId FROM BrushStyle")] if vectors else []
        clip.db.close()
        rng = random.Random(seed)
        for k in range(rounds):
            updates: dict[int, dict] = {}
            statements = []
            if vectors:
                for node in rng.sample(nodes, min(len(nodes), rng.randint(1, 4))):
                    updates[node.main_id] = {"LayerOffsetX": rng.randint(-300, 300), "LayerOffsetY": rng.randint(-300, 300)}
                for brush in rng.sample(brushes, min(len(brushes), rng.randint(0, 3))):
                    statements.append(("UPDATE BrushStyle SET CompositeMode = ?, ThicknessBase = ? WHERE MainId = ?",
                                       (rng.choice([0, 27]), rng.choice([None, 0.3, 0.75, 1.0]), brush)))
            for node in rng.sample(nodes, min(len(nodes), rng.randint(3, 12))):
                cols: dict = {}
                if rng.random() < 0.6:
                    cols["LayerComposite"] = rng.choice(MODES)
                if rng.random() < 0.5:
                    cols["LayerOpacity"] = rng.choice([0, 1, 64, 128, 200, 255, 256, rng.randint(0, 256)])
                if rng.random() < 0.3:
                    cols["LayerClip"] = rng.randint(0, 1)
                if rng.random() < 0.3:
                    cols["LayerVisibility"] = rng.randint(0, 3)
                if cols:
                    updates.setdefault(node.main_id, {}).update(cols)
            raw = _patch_layers(base, updates, statements=statements)
            label = f"{name} round {k}: {updates} {statements}"
            want, want_warn = _render(raw, "python")
            got, got_warn = _render(raw, "rust")
            self.assertEqual(want, got, label)
            self.assertEqual(want_warn, got_warn, label)
            x0, y0 = rng.randrange(0, w - 1), rng.randrange(0, h - 1)
            region = (x0, y0, rng.randint(x0 + 1, w), rng.randint(y0 + 1, h))
            for decodable in (False, True):
                want_part, _ = _render(raw, "python", region, decodable)
                got_part, _ = _render(raw, "rust", region, decodable)
                self.assertEqual(want_part, got_part, f"{label} region {region} decodable={decodable}")

    def test_random_layer_properties_tama(self) -> None:
        self._check("tama.clip", 2, 20261012)

    def test_random_layer_properties_structure(self) -> None:
        self._check("csp515/structure.clip", 12, 7)

    def test_random_layer_properties_adjustments(self) -> None:
        self._check("csp515/adjust.clip", 8, 11)
        self._check("additional_examples/probe_saido+50_colored.clip", 4, 13)

    def test_random_vector_layers_and_frames(self) -> None:
        """Stage 1d: vector layers and comic frames."""
        self._check("vector_complex.clip", 6, 1607, vectors=True)
        self._check("vector_simple.clip", 4, 1608, vectors=True)
        self._check("additional_examples/probe_vector_mouse_senhaba.clip", 3, 1609, vectors=True)

    # ── stage 1b / 1c: effects, gradients and materials rewritten at random ──

    def _compare(self, raw: bytes, label: str, rng: random.Random, regions: int = 1) -> None:
        from tmi.formats import clip as C

        clip = C.open_clip_bytes(raw)
        w, h = clip.canvas.width, clip.canvas.height
        clip.db.close()
        want, want_warn = _render(raw, "python")
        got, got_warn = _render(raw, "rust")
        self.assertEqual(want, got, label)
        self.assertEqual(want_warn, got_warn, label)
        for _ in range(regions):
            x0, y0 = rng.randrange(0, w - 1), rng.randrange(0, h - 1)
            region = (x0, y0, rng.randint(x0 + 1, min(w, x0 + 400)), rng.randint(y0 + 1, min(h, y0 + 400)))
            for decodable in (False, True):
                want_part, _ = _render(raw, "python", region, decodable)
                got_part, _ = _render(raw, "rust", region, decodable)
                self.assertEqual(want_part, got_part, f"{label} region {region} decodable={decodable}")

    def _base(self, name: str) -> bytes:
        path = EXAMPLES / name
        if not path.exists():
            self.skipTest(f"{name} not present")
        return path.read_bytes()

    def test_random_edges_and_tones(self) -> None:
        """フチ and トーン attached to random layers and folders (folders
        dilate the window and screen their composite), over random blend
        modes and opacities."""
        from tmi.formats import clip as C

        for name, rounds, seed in (("csp515/structure.clip", 6, 21), ("csp515/tone_lpi10.clip", 4, 22), ("tama.clip", 1, 23)):
            base = self._base(name)
            clip = C.open_clip_bytes(base)
            nodes = [n for n in C.walk_layer_nodes(clip.root.children)]
            clip.db.close()
            rng = random.Random(seed)
            for k in range(rounds):
                updates: dict[int, dict] = {}
                for node in rng.sample(nodes, min(len(nodes), rng.randint(1, 4))):
                    edge = tone = None
                    if rng.random() < 0.7:
                        edge = (rng.choice([0.5, 1.0, 2.0, 2.5, 3.0009, rng.uniform(0.2, 7.0)]),
                                tuple(rng.randrange(256) for _ in range(3)))
                    if rng.random() < 0.5 or edge is None:
                        tone = (rng.choice([10.0, 30.0, 60.0, rng.uniform(5.0, 80.0)]), rng.choice([0, 45, 30, rng.randrange(360)]),
                                rng.choice([1, 2, 4]))
                    cols = {"LayerEffectAttached": 1, "LayerEffectInfo": _effect_blob(edge, tone)}
                    if rng.random() < 0.4:
                        cols["LayerComposite"] = rng.choice(MODES[:-1])
                    if rng.random() < 0.3:
                        cols["LayerOpacity"] = rng.choice([64, 200, 256])
                    updates[node.main_id] = cols
                raw = _patch_layers(base, updates, add_columns={"LayerEffectAttached": "INTEGER", "LayerEffectInfo": "BLOB"})
                self._compare(raw, f"{name} round {k}: {updates}", rng, regions=1 if name == "tama.clip" else 2)

    def test_random_gradients(self) -> None:
        """Gradient fills with random stops, line, 混色モード and level."""
        from tmiref import clip as C

        for name, rounds, seed in (("csp515/fill_grad.clip", 10, 31), ("csp515/grad_perceptual.clip", 10, 32)):
            base = self._base(name)
            clip = C.open_clip_bytes(base)
            fills = {}
            for node in C.walk_layer_nodes(clip.root.children):
                blob = C._safe_layer_blob(clip, node.main_id, "GradationFillInfo")
                if blob and C._parse_gradation_fill(blob)["solid"] is None:
                    fills[node.main_id] = bytearray(blob)
            w, h = clip.canvas.width, clip.canvas.height
            clip.db.close()
            self.assertTrue(fills)
            rng = random.Random(seed)
            for k in range(rounds):
                updates = {}
                for main_id, blob in fills.items():
                    blob = bytearray(blob)
                    p = C._gradation_section(bytes(blob), "GradationData")
                    header, stop_size, count = struct.unpack(">III", blob[p + 4:p + 16])
                    if header >= 24:
                        struct.pack_into(">II", blob, p + 20, rng.choice([0, 1, 2, 3, 7]), rng.randrange(6))
                    for i in range(count):
                        s = p + 4 + header + i * stop_size
                        rgba = [rng.randrange(256) * 0x01010101 for _ in range(4)]
                        if rng.random() < 0.6:
                            rgba[3] = 0xFFFFFFFF
                        struct.pack_into(">4I", blob, s, *rgba)
                        struct.pack_into(">I", blob, s + 20, rng.choice([0, 0x8000, rng.randrange(0x8001), 0x9000]))
                    q = C._gradation_section(bytes(blob), "GradationSetting")
                    pts = [rng.uniform(-200, w + 200), rng.uniform(-200, h + 200), rng.uniform(-200, w + 200), rng.uniform(-200, h + 200)]
                    struct.pack_into(">4d", blob, q + 12 + 24, *pts)
                    updates[main_id] = {"GradationFillInfo": bytes(blob)}
                self._compare(_patch_layers(base, updates), f"{name} round {k}", rng, regions=2)

    def test_random_materials(self) -> None:
        """Image materials re-placed at random: scale, rotation, flips,
        sub-pixel corners, tiling and layer offsets."""
        import math

        from tmi.formats import clip as C
        from tmi.formats import clip_style

        for name, rounds, seed in (("additional_examples/probe_material_scale.clip", 6, 41),
                                   ("additional_examples/probe_material_madara_color.clip", 6, 42)):
            base = self._base(name)
            clip = C.open_clip_bytes(base)
            mats = {}
            for node in C.walk_layer_nodes(clip.root.children):
                style = clip_style.layer_style(clip, node.main_id)
                if style.get("ResizableImageInfo") and style.get("ResizableOriginalMipmap"):
                    mats[node.main_id] = bytes(style["ResizableImageInfo"])
            w, h = clip.canvas.width, clip.canvas.height
            clip.db.close()
            self.assertTrue(mats)
            rng = random.Random(seed)
            for k in range(rounds):
                updates = {}
                for main_id, blob in mats.items():
                    blob = bytearray(blob)
                    ow, oh = struct.unpack(">II", blob[32:40])
                    s = rng.choice([0.1, 0.3, 0.5, 1.0, 1.7, rng.uniform(0.05, 3.0)])
                    sy = s * rng.choice([1.0, 1.0, -1.0, 0.6])
                    th = math.radians(rng.choice([0, 0, 90, 30, 180, rng.uniform(0, 360)]))
                    ex = (s * math.cos(th), s * math.sin(th))
                    ey = (-sy * math.sin(th), sy * math.cos(th))
                    tl = (rng.uniform(-200, w), rng.uniform(-200, h))
                    if rng.random() < 0.3:
                        tl = (round(tl[0]) + 0.5, round(tl[1]))
                    tr = (tl[0] + ex[0] * ow, tl[1] + ex[1] * ow)
                    bl = (tl[0] + ey[0] * oh, tl[1] + ey[1] * oh)
                    br = (tr[0] + bl[0] - tl[0], tr[1] + bl[1] - tl[1])
                    struct.pack_into(">8d", blob, len(blob) - 64, *tl, *tr, *bl, *br)
                    struct.pack_into(">I", blob, 16, 1 if rng.random() < 0.3 else 0)
                    updates[main_id] = {"ResizableImageInfo": bytes(blob), "LayerOffsetX": rng.randint(-30, 30),
                                        "LayerOffsetY": rng.randint(-30, 30)}
                    if rng.random() < 0.3:
                        updates[main_id]["LayerComposite"] = rng.choice(MODES[:-1])
                self._compare(_patch_layers(base, updates), f"{name} round {k}: {updates}", rng, regions=2)


# ───────────────────────────── damaged files (1e) ─────────────────────────────

_BEGIN, _END = "BlockDataBeginChunk", "BlockDataEndChunk"


def _tile_record(block: int, bpt: int, tw: int, th: int, data: bytes | None) -> bytes:
    """A BlockDataBeginChunk record as CSP (and ``build_tile_record``) writes it."""
    body = struct.pack(">IIIII", block, bpt, tw, th, 1 if data is not None else 0)
    if data is not None:
        comp = zlib.compress(data)
        blob = struct.pack("<I", len(comp)) + comp
        body += struct.pack(">I", len(blob)) + blob
    body += struct.pack(">I", len(_END)) + _END.encode("utf-16-be")
    name = _BEGIN.encode("utf-16-be")
    return struct.pack(">II", 8 + len(name) + len(body), len(_BEGIN)) + name + body


def _record_fields(rec: bytes):
    block, bpt, tw, th, has = struct.unpack(">IIIII", rec[46:66])
    data = None
    if has:
        size = struct.unpack(">I", rec[66:70])[0]
        data = zlib.decompress(rec[70:70 + size][4:])
    return block, bpt, tw, th, data


def _with_records(raw: bytes, outer_id: str, edit) -> bytes:
    """``raw`` with the tile records of data block ``outer_id`` replaced by
    ``edit(records)`` (a list of record bytes in file order)."""
    from tmi.formats import clip as C

    container = C.parse_container(raw)
    chunk = container.index_extas(raw)[outer_id]
    payload = raw[chunk.payload_off:chunk.payload_off + chunk.payload_size]
    inner_size = struct.unpack(">Q", payload[48:56])[0]
    inner, tail = payload[56:56 + inner_size], payload[56 + inner_size:]
    records, p = [], 0
    while p + 8 <= len(inner):
        size, n = struct.unpack(">II", inner[p:p + 8])
        if n * 2 + 8 > size or p + size > len(inner):
            break
        records.append(inner[p:p + size])
        p += size
    try:
        new_inner = b"".join(edit(records)) + inner[p:]
    except _Painted:
        return None
    payloads = {c.file_off: raw[c.payload_off:c.payload_off + c.payload_size] for c in container.chunks}
    payloads[chunk.file_off] = payload[:48] + struct.pack(">Q", len(new_inner)) + new_inner + tail
    return clip_io._emit_container_with_sqli_payload(container, payloads, payloads[container.sqli_chunk().file_off])


def _painted(records: list[bytes]) -> list[int]:
    return [i for i, r in enumerate(records) if r[8:8 + 38] == _BEGIN.encode("utf-16-be") and _record_fields(r)[4] is not None]


class _Painted(Exception):
    pass


def _has_painted(records):
    if not _painted(records):
        raise _Painted
    return records


def _corrupt(records):
    i = _painted(records)[0]
    rec = bytearray(records[i])
    rec[46 + 24 + 4 + 2] ^= 0x55  # inside the deflate stream
    records[i] = bytes(rec)
    return records


def _unknown_size(records):
    for i in _painted(records)[:3]:
        block, bpt, tw, th, data = _record_fields(records[i])
        records[i] = _tile_record(block, bpt + 3, tw, th, data + b"\1\2\3")
    return records


def _repeated(records):
    for i in _painted(records)[:3]:
        block, bpt, tw, th, data = _record_fields(records[i])
        n = tw * th
        records.append(_tile_record(block, bpt, tw, th, bytes(255 - a for a in data[:n]) + data[n:]))
    return records


def _oversized(records):
    block = _record_fields(records[_painted(records)[0]])[0]
    tw, th = 300, 40
    records.append(_tile_record(block, tw * th, tw, th, bytes((x * 7 + y) & 255 for y in range(th) for x in range(tw))))
    return records


def _odd_one_bit(records):
    block = _record_fields(records[_painted(records)[0]])[0]
    records.append(_tile_record(block, 12, 12, 8, b"\xff" * 12))
    return records


class DamagedFileTests(_ParityCase):
    """Offscreens that do not decode as CSP wrote them: tmi (tmi_accel) and
    the reference give the same pixels and decode failures and warn about
    the same offscreens and tile sizes (the reasons' wording differs). A
    banded whole-canvas render with an unreadable layer starts over in one
    pass in both. The edits are checked to matter where they should."""

    EDITS = (_corrupt, _unknown_size, _repeated, _oversized, _odd_one_bit)

    @staticmethod
    def _gist(warnings: list[str]) -> list[str]:
        # "tmiki: skipping unreadable CLIP offscreen <id>: <why>" -> up to the id
        return sorted({w.split(": ", 2)[1] if "unreadable CLIP offscreen" in w else w for w in warnings})

    def _targets(self, name: str) -> tuple[bytes, list[tuple[str, str]], tuple[int, int]]:
        from tmi.formats import clip as C
        from tmi.formats import clip_style

        path = EXAMPLES / name
        if not path.exists():
            self.skipTest(f"{name} not present")
        raw = path.read_bytes()
        clip = C.open_clip_bytes(raw)
        nodes = [n for n in C.walk_layer_nodes(clip.root.children) if n.visible]
        targets = []
        for kind, mipmap in (
            ("render", lambda n: None if n.folder else n.render_mipmap),
            ("mask", lambda n: n.mask_mipmap if n.mask_enabled and not n.comic_frame else None),
            ("material", lambda n: clip_style.layer_style(clip, n.main_id).get("ResizableOriginalMipmap")),
        ):
            # The topmost layer of that kind whose data block has painted tiles.
            for node in reversed(nodes):
                info = C.layer_offscreen_info(clip, mipmap(node)) if mipmap(node) else None
                if info and info[2] in clip.extas and _with_records(raw, info[2], _has_painted) is not None:
                    targets.append((kind, info[2]))
                    break
        size = (clip.canvas.width, clip.canvas.height)
        clip.db.close()
        return raw, targets, size

    def _compare(self, raw: bytes, label: str, size: tuple[int, int], windows: bool = True) -> bytes:
        w, h = size
        window = (w // 4, h // 3, min(w, w // 4 + 300), min(h, h // 3 + 260))
        out = None
        for region, decodable in ((None, False), (window, False), (window, True))[:3 if windows else 1]:
            got = {}
            for renderer in ("python", "rust"):
                failures: list = []
                pixels, warnings = _render(raw, renderer, region, decodable, failures)
                got[renderer] = (pixels, failures, self._gist(warnings))
            self.assertEqual(got["python"][0], got["rust"][0], f"{label} {region} decodable={decodable}: pixels")
            self.assertEqual(got["python"][1:], got["rust"][1:], f"{label} {region} decodable={decodable}")
            if region is None:
                out = got["rust"][0]
        return out

    def _check(self, name: str, kinds: set[str], edits=EDITS, windows: bool = True) -> None:
        raw, targets, size = self._targets(name)
        targets = [t for t in targets if t[0] in kinds]
        self.assertEqual(kinds, {kind for kind, _ in targets}, name)
        plain = self._compare(raw, name, size, windows)
        for kind, outer_id in targets:
            for edit in edits:
                label = f"{name} {kind} {edit.__name__}"
                damaged = self._compare(_with_records(raw, outer_id, edit), label, size, windows)
                if edit in (_corrupt, _odd_one_bit) and kind == "render":
                    self.assertNotEqual(plain, damaged, f"{label}: the layer should drop out")

    def test_stored_pixels_whole_and_windowed(self) -> None:
        self._check("csp515/structure.clip", {"render"})

    def test_banded_render_starts_over_in_one_pass(self) -> None:
        # (Whole renders only: the reference takes seconds on tama.)
        self._check("tama.clip", {"render"}, (_corrupt,), windows=False)
        self._check("tama.clip", {"mask"}, (_unknown_size,), windows=False)

    def test_material_originals(self) -> None:
        self._check("additional_examples/probe_material_scale.clip", {"material"})


if __name__ == "__main__":
    unittest.main()
