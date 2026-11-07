"""tmi's CLIP CanvasPreview decode and resize, and its CLIP layer
write-back (tmi_accel, RUST_MIGRATION.md stage 2c), against the Python
reference ``tmiref.clip_io``: the same preview pixels, the same written
bytes."""
from __future__ import annotations

import io
import sqlite3
from tmiref import clip_io  # the Python write-back helpers that build test files
import unittest
from pathlib import Path

import numpy as np
from PIL import Image

PROJECT_ROOT = Path(__file__).resolve().parents[1]
EXAMPLE = PROJECT_ROOT / "example"


def example_clips() -> list[Path]:
    paths = sorted(EXAMPLE.glob("*.clip")) + sorted((EXAMPLE / "csp515").glob("*.clip"))
    return paths + sorted((EXAMPLE / "additional_examples").glob("*.clip"))


def with_preview(raw: bytes, data: bytes | None, width, height) -> bytes:
    """``raw`` with its CanvasPreview row replaced (ImageData ``data``)."""
    from tmi.formats import clip as C

    clip = C.open_clip_bytes(raw)
    con = clip_io._clone_sqlite_connection(clip)
    con.execute("UPDATE CanvasPreview SET ImageWidth = ?, ImageHeight = ?, ImageData = ?", (width, height, data))
    con.commit()
    container = C.parse_container(raw)
    payloads = {c.file_off: raw[c.payload_off:c.payload_off + c.payload_size] for c in container.chunks}
    return clip_io._emit_container_with_sqli_payload(container, payloads, clip_io._serialize_sqlite(con))


def encode(image: Image.Image, fmt: str, **kw) -> bytes:
    buf = io.BytesIO()
    image.save(buf, fmt, **kw)
    return buf.getvalue()


class CanvasPreviewParityTests(unittest.TestCase):
    def assert_same_preview(self, clip, label: str) -> None:
        from tmi.formats import clip as C
        from tmiref import clip_io as R

        for canvas_size in (False, True):
            try:
                want = R.render_canvas_preview_rgba(clip, canvas_size=canvas_size)
            except Exception as exc:  # the reference raises: so must tmi
                with self.assertRaises(Exception, msg=f"{label}: reference raised {exc!r}"):
                    C.render_canvas_preview_rgba(clip, canvas_size=canvas_size)
                continue
            got = C.render_canvas_preview_rgba(clip, canvas_size=canvas_size)
            if want is None:
                self.assertIsNone(got, label)
                continue
            self.assertIsNotNone(got, label)
            self.assertEqual(want.size, got.size, f"{label} canvas_size={canvas_size}")
            self.assertEqual(want.mode, got.mode)
            self.assertTrue(want.tobytes() == got.tobytes(), f"{label} canvas_size={canvas_size}: pixels differ")

    def test_example_files(self) -> None:
        from tmi.formats import clip as C

        paths = example_clips()
        if not paths:
            self.skipTest("no example CLIP files")
        for path in paths:
            with self.subTest(path=path.name):
                clip = C.open_clip(path)
                self.assert_same_preview(clip, path.name)
                # The PNG shortcut of render_composite_with_png.
                from tmiref import clip_io as R

                want = R.render_canvas_preview_rgba(clip, canvas_size=True)
                if want is not None:
                    image, _png = C.render_composite_with_png(clip)
                    self.assertTrue(want.tobytes() == image.tobytes(), path.name)
                clip.db.close()

    def test_preview_formats_and_sizes(self) -> None:
        """Every PNG colour type, JPEG, a format only Pillow reads (BMP),
        damaged data, and declared sizes up, down, zero and invalid."""
        from tmi.formats import clip as C

        source = next((p for p in (EXAMPLE / "blend_modes.clip", EXAMPLE / "tama.clip") if p.exists()), None)
        if source is None:
            self.skipTest("example/blend_modes.clip and tama.clip missing")
        raw = source.read_bytes()
        clip = C.open_clip_bytes(raw)
        canvas = (clip.canvas.width, clip.canvas.height)
        clip.db.close()
        rng = np.random.default_rng(9)
        base = rng.integers(0, 256, (37, 53, 4), dtype=np.uint8)
        base[..., 3] = np.where(rng.random((37, 53)) < 0.3, 0, base[..., 3])
        rgba = Image.fromarray(base, "RGBA")
        images = {
            "png-rgba": encode(rgba, "PNG"),
            "png-rgb": encode(rgba.convert("RGB"), "PNG"),
            "png-l": encode(rgba.convert("L"), "PNG"),
            "png-la": encode(rgba.convert("LA"), "PNG"),
            "png-p": encode(rgba.convert("RGB").convert("P"), "PNG"),
            "png-p-trns": encode(rgba.convert("RGB").convert("P"), "PNG", transparency=3),
            "png-16": encode(Image.fromarray((base[..., 0].astype(np.uint16) * 257), "I;16"), "PNG"),
            "jpeg": encode(rgba.convert("RGB"), "JPEG", quality=90),
            "bmp": encode(rgba.convert("RGB"), "BMP"),
            "damaged": encode(rgba, "PNG")[:200],
            "junk": b"not an image",
        }
        sizes = [(53, 37), (0, 0), (106, 74), (20, 15), (53, 0), (0, 74), canvas, (canvas[0] // 3, canvas[1] // 3), (-4, 10)]
        for name, data in images.items():
            for size in sizes:
                with self.subTest(image=name, size=size):
                    clip = C.open_clip_bytes(with_preview(raw, data, *size))
                    self.assert_same_preview(clip, f"{name} {size}")
                    clip.db.close()


def raster_layers(clip) -> list:
    """Nodes whose render offscreen tmi can write (colour rasters)."""
    from tmi.formats import clip as C

    out = []
    for node in C.walk_layer_nodes(clip.root.children):
        info = C.layer_offscreen_info(clip, node.render_mipmap)
        if not info or info[2] not in clip.extas:
            continue
        try:
            attr = C.parse_offscreen_attribute(info[1])
        except Exception:
            continue
        if attr.channel_profile == "color-alpha-bgra":
            out.append((node, attr))
    return out


class ClipWriteBackParityTests(unittest.TestCase):
    """``clip.replace_layer_pixels`` (tmi_accel: stock zlib 1.3.1, SQLite
    3.50.4) writes the bytes ``tmiref.clip_io.replace_layer_pixels``
    (Pillow, numpy, CPython's zlib and sqlite3) writes."""

    def setUp(self) -> None:
        import tempfile

        root = PROJECT_ROOT / ".test-tmp"
        root.mkdir(exist_ok=True)
        self.tmp = Path(tempfile.mkdtemp(prefix="tmi-clip-wb-", dir=root))

    def tearDown(self) -> None:
        import shutil

        shutil.rmtree(self.tmp, ignore_errors=True)
        try:
            (PROJECT_ROOT / ".test-tmp").rmdir()
        except OSError:
            pass

    def check_write(self, path: Path, selector: str, png: bytes, label: str) -> None:
        import contextlib

        from tmi.formats import clip as C
        from tmiref import clip_io as R

        for noop_flag in (False, True):
            results = []
            for fn in (R.replace_layer_pixels, C.replace_layer_pixels):
                C._warned_offscreens.clear()
                err = io.StringIO()
                try:
                    with contextlib.redirect_stderr(err):
                        out = fn(path, selector, png, invalidate_preview_on_noop=noop_flag)
                    results.append(("ok", out, err.getvalue()))
                except Exception as exc:
                    results.append(("raised", f"{type(exc).__name__}: {exc}", err.getvalue()))
            (want_kind, want, want_err), (got_kind, got, got_err) = results
            self.assertEqual(want_kind, got_kind, f"{label}: {want if want_kind == 'raised' else got}")
            if want_kind == "ok":
                self.assertTrue(want == got, f"{label} invalidate={noop_flag}: written bytes differ")
            # The same warnings, about the same offscreens (tmi words them
            # as tmi_accel reports the failure).
            def subjects(text: str) -> list[str]:
                return sorted(line.split(":")[1].strip().split()[-1] for line in text.splitlines() if line.startswith("tmi:"))
            self.assertEqual(subjects(want_err), subjects(got_err), label)

    def replacements(self, image: Image.Image, rng: np.random.Generator):
        w, h = image.size
        yield "same", image
        arr = np.asarray(image).copy()
        y0, x0 = int(rng.integers(0, h)), int(rng.integers(0, w))
        arr[y0:y0 + 60, x0:x0 + 300] = rng.integers(0, 256, arr[y0:y0 + 60, x0:x0 + 300].shape, dtype=np.uint8)
        yield "edited", Image.fromarray(arr, "RGBA")
        if w > 5 and h > 5:
            yield "smaller", image.crop((0, 0, w - 5, h - 3))
        hidden = np.asarray(image).copy()
        hidden[..., :3] = 77
        hidden[..., 3] = np.where(hidden[..., 3] < 128, 0, hidden[..., 3])
        yield "colour-under-zero-alpha", Image.fromarray(hidden, "RGBA")
        yield "empty", Image.new("RGBA", (w, h), (0, 0, 0, 0))

    def _png(self, image: Image.Image) -> bytes:
        return encode(image, "PNG")

    def check_file(self, source: Path, layers: int, rng: np.random.Generator, images=None) -> None:
        from tmi.formats import clip as C

        raw = source.read_bytes()
        path = self.tmp / source.name
        path.write_bytes(raw)
        clip = C.open_clip_bytes(raw)
        nodes = raster_layers(clip)
        for node, _attr in nodes[:layers]:
            px = C.render_layer_rgba(clip, node)
            if px is None:
                continue
            selector = node.uuid or f"clip:{node.main_id}"
            for name, image in self.replacements(px.image.to_pil(), rng):
                if images is not None and name not in images:
                    continue
                with self.subTest(file=source.name, layer=node.main_id, image=name):
                    self.check_write(path, selector, self._png(image), f"{source.name} {node.main_id} {name}")
        clip.db.close()

    def test_tile_record(self) -> None:
        from tmi import accel
        from tmiref import clip_io as R

        rng = np.random.default_rng(3)
        for k in range(40):
            px = rng.integers(0, 256, (256, 256, 4), dtype=np.uint8)
            if k % 4 == 0:
                px[..., 3] = 0
            elif k % 4 == 1:
                px[..., 3] = np.where(rng.random((256, 256)) < 0.9, 0, px[..., 3])
            tile = Image.new("RGBA", (256, 256), (0, 0, 0, 0))
            tile.alpha_composite(Image.fromarray(px, "RGBA"))
            self.assertEqual(R.build_tile_record(k * 37, tile), accel.native()._test.clip_tile_record(k * 37, tile.tobytes()))

    def test_example_files(self) -> None:
        paths = [p for p in example_clips() if p.stat().st_size < 3_000_000]
        if not paths:
            self.skipTest("no small example CLIP files")
        rng = np.random.default_rng(4)
        for path in paths:
            self.check_file(path, 2, rng)

    def test_tama(self) -> None:
        """A CSP file of 66 layers and a CanvasPreview: an edit keeps CSP's
        records of the blocks it does not touch."""
        source = EXAMPLE / "tama.clip"
        if not source.exists():
            self.skipTest("example/tama.clip missing")
        self.check_file(source, 1, np.random.default_rng(5), images={"same", "edited"})

    def test_damaged_offscreen(self) -> None:
        """The layer's current pixels do not decode: both warn and write every
        block anew."""
        from tmi.formats import clip as C

        source = next((p for p in (EXAMPLE / "blend_modes.clip", EXAMPLE / "text_simple.clip") if p.exists()), None)
        if source is None:
            self.skipTest("no small example CLIP")
        from tmiref import clip_io as R

        clip = C.open_clip(source)
        node, _attr = raster_layers(clip)[0]
        selector = node.uuid or f"clip:{node.main_id}"
        image = C.render_layer_rgba(clip, node).image.to_pil()
        clip.db.close()
        rng = np.random.default_rng(6)
        # The layer as tmi writes it (a block record holds its zlib stream:
        # sizes 8, name 38, five words 20, blob size 4, compressed size 4),
        # then the zlib header of its first painted block broken.
        painted = Image.new("RGBA", image.size, (200, 30, 90, 255))
        raw = bytearray(R.replace_layer_pixels(source, selector, self._png(painted)))
        clip = C.open_clip_bytes(bytes(raw))
        info = C.layer_offscreen_info(clip, node.render_mipmap)
        exta = clip.extas[info[2]]
        _block, offset, _size = next(r for r in C._offscreen_tile_index(clip, info[2])[2] if r[2] > 200)
        clip.db.close()
        at = exta.payload_off + 56 + offset + 74
        self.assertEqual(0x78, raw[at])
        raw[at:at + 2] = b"\x00\x00"
        path = self.tmp / "damaged.clip"
        path.write_bytes(bytes(raw))
        import contextlib

        C._warned_offscreens.clear()
        err = io.StringIO()
        with contextlib.redirect_stderr(err):
            C.replace_layer_pixels(path, selector, self._png(image))
        self.assertIn("skipping unreadable CLIP offscreen", err.getvalue())
        for name, replacement in self.replacements(image, rng):
            with self.subTest(image=name):
                self.check_write(path, selector, self._png(replacement), f"damaged {name}")


def exta_tables(raw: bytes, outer_id: str) -> tuple[list[tuple[bytes, int]], dict[str, list[int]]]:
    """The tile records of the Exta ``outer_id`` as (stored blob, has_data) and
    the trailing tables it ends with, by name."""
    import struct

    from tmi.formats import clip as C

    container = C.parse_container(raw)
    exta = next(c for c in container.chunks if c.cid == b"Exta" and raw[c.payload_off + 8:c.payload_off + 48].decode() == outer_id)
    payload = raw[exta.payload_off:exta.payload_off + exta.payload_size]
    (size,) = struct.unpack(">Q", payload[48:56])
    assert 56 + size == len(payload), "the inner size field covers the whole payload, tables included"
    inner = payload[56:]
    records, tables = [], {}
    p = 0
    while p + 8 <= len(inner):
        rec_size, name_len = struct.unpack(">II", inner[p:p + 8])
        if name_len * 2 + 8 > rec_size or p + rec_size > len(inner):
            break
        body = p + 8 + 2 * name_len
        has_data, data_size = struct.unpack(">II", inner[body + 16:body + 24])
        records.append((inner[body + 24:body + 24 + data_size], has_data))
        p += rec_size
    while p < len(inner):
        (name_len,) = struct.unpack(">I", inner[p:p + 4])
        name = inner[p + 4:p + 4 + 2 * name_len].decode("utf-16-be")
        q = p + 4 + 2 * name_len
        record_size, count, entry_size = struct.unpack(">III", inner[q:q + 12])
        assert (record_size, entry_size) == (12, 4)
        tables[name] = list(struct.unpack(f">{count}I", inner[q + 12:q + 12 + 4 * count]))
        p = q + 12 + 4 * count
    assert p == len(inner)
    return records, tables


def set_exta_tables(raw: bytes, outer_id: str, status: int | None) -> bytes:
    """``raw`` with the trailing tables of the Exta ``outer_id`` rebuilt with
    BlockStatus ``status`` (None: dropped)."""
    import struct

    from tmi.formats import clip as C

    container = C.parse_container(raw)
    chunks = []
    for c in container.chunks:
        payload = raw[c.payload_off:c.payload_off + c.payload_size]
        if c.cid == b"Exta" and payload[8:48].decode() == outer_id:
            records, tables = exta_tables(raw, outer_id)
            inner = payload[56:]
            end = 0
            for _ in records:
                end += struct.unpack(">I", inner[end:end + 4])[0]
            tail = b"" if status is None else clip_io._trailing_tables(status, tables["BlockCheckSum"])
            inner = inner[:end] + tail
            payload = payload[:48] + struct.pack(">Q", len(inner)) + inner
        chunks.append((c.cid, payload))
    return clip_io.emit_container(container.first_chunk_off, chunks)


class ClipWriteBackConsistencyTests(unittest.TestCase):
    """research/clip-writeback-consistency.md section 5: the rewritten Exta
    ends with BlockStatus and BlockCheckSum, and the layer's LayerThumbnail
    is marked stale."""

    SOURCE = EXAMPLE / "csp515" / "v3.clip"
    SELECTOR = "clip:5"

    def setUp(self) -> None:
        if not self.SOURCE.exists():
            self.skipTest("example/csp515/v3.clip missing")
        import tempfile

        root = PROJECT_ROOT / ".test-tmp"
        root.mkdir(exist_ok=True)
        self.tmp = Path(tempfile.mkdtemp(prefix="tmi-clip-wbc-", dir=root))

    def tearDown(self) -> None:
        import shutil

        shutil.rmtree(self.tmp, ignore_errors=True)
        try:
            (PROJECT_ROOT / ".test-tmp").rmdir()
        except OSError:
            pass

    def edited_png(self, raw: bytes) -> bytes:
        from tmi.formats import clip as C

        clip = C.open_clip_bytes(raw)
        node = C.find_layer_node(clip, self.SELECTOR)
        image = C.render_layer_rgba(clip, node).image.to_pil()
        clip.db.close()
        arr = np.asarray(image).copy()
        arr[10:70, 20:400] = (200, 30, 40, 255)
        return encode(Image.fromarray(arr, "RGBA"), "PNG")

    def write(self, raw: bytes, name: str) -> tuple[bytes, bytes]:
        """(tmi's write-back, the reference's) of ``raw``, layer clip:5 edited."""
        from tmi.formats import clip as C

        path = self.tmp / name
        path.write_bytes(raw)
        png = self.edited_png(raw)
        return C.replace_layer_pixels(path, self.SELECTOR, png), clip_io.replace_layer_pixels(path, self.SELECTOR, png)

    def layer_ids(self, raw: bytes):
        from tmi.formats import clip as C

        clip = C.open_clip_bytes(raw)
        node = C.find_layer_node(clip, self.SELECTOR)
        offscreen_id, _attr, outer_id = C.layer_offscreen_info(clip, node.render_mipmap)
        thumbnail = clip.db.execute("SELECT LayerRenderThumbnail FROM Layer WHERE MainId = ?", (node.main_id,)).fetchone()[0]
        clip.db.close()
        return offscreen_id, outer_id, thumbnail

    def test_tables_follow_the_tiles(self) -> None:
        import zlib

        raw = self.SOURCE.read_bytes()
        _offscreen, outer_id, _thumb = self.layer_ids(raw)
        _records, source_tables = exta_tables(raw, outer_id)
        got, want = self.write(raw, "v3.clip")
        self.assertTrue(got == want, "tmi_accel and the reference write the same bytes")
        records, tables = exta_tables(got, outer_id)
        n = len(records)
        self.assertGreater(n, 0)
        self.assertEqual(["BlockStatus", "BlockCheckSum"], list(tables))
        self.assertEqual(n, len(tables["BlockStatus"]))
        self.assertEqual(n, len(tables["BlockCheckSum"]))
        self.assertEqual({source_tables["BlockStatus"][0]}, set(tables["BlockStatus"]))
        for (blob, has_data), checksum in zip(records, tables["BlockCheckSum"]):
            self.assertEqual(zlib.adler32(blob) if has_data else 0, checksum)
        # tmi still reads what it wrote.
        from tmi.formats import clip as C

        clip = C.open_clip_bytes(got)
        self.assertIsNotNone(C.render_layer_rgba(clip, C.find_layer_node(clip, self.SELECTOR)))
        clip.db.close()

    def test_status_comes_from_the_source(self) -> None:
        raw = self.SOURCE.read_bytes()
        _offscreen, outer_id, _thumb = self.layer_ids(raw)
        for status in (0, 1, 7, None):
            with self.subTest(status=status):
                got, want = self.write(set_exta_tables(raw, outer_id, status), f"status-{status}.clip")
                self.assertTrue(got == want, "tmi_accel and the reference write the same bytes")
                _records, tables = exta_tables(got, outer_id)
                self.assertEqual({1 if status is None else status}, set(tables["BlockStatus"]))

    def test_thumbnail_is_marked_stale(self) -> None:
        from tmi.formats import clip as C

        raw = self.SOURCE.read_bytes()
        _offscreen, _outer, thumbnail = self.layer_ids(raw)
        got, _want = self.write(raw, "thumb.clip")

        def rows(data: bytes):
            clip = C.open_clip_bytes(data)
            cur = clip.db.execute("SELECT * FROM LayerThumbnail ORDER BY MainId")
            names = [d[0] for d in cur.description]
            out = {r[names.index("MainId")]: dict(zip(names, r)) for r in cur.fetchall()}
            clip.db.close()
            return names, out

        names, before = rows(raw)
        _names, after = rows(got)
        refresh = [n for n in names if "NeedRefresh" in n]
        self.assertGreaterEqual(len(refresh), 14)
        self.assertEqual(set(before), set(after))
        for main_id, row in after.items():
            if main_id == thumbnail:
                self.assertEqual({1}, {row[n] for n in refresh})
                self.assertEqual(
                    {k: v for k, v in before[main_id].items() if k not in refresh},
                    {k: v for k, v in row.items() if k not in refresh},
                )
            else:
                self.assertEqual(before[main_id], row)


if __name__ == "__main__":
    unittest.main()
