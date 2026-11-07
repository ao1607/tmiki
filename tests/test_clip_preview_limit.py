"""A CLIP's CanvasPreview bigger than its canvas is a decompression bomb
(a 4 MB PNG held 4.3 GB of pixels): refused before it is decoded."""
from __future__ import annotations

import struct
import unittest

from tmi.formats.clip import _preview_exceeds_canvas

PNG = b"\x89PNG\r\n\x1a\n"


def png_header(w: int, h: int) -> bytes:
    return PNG + struct.pack(">I", 13) + b"IHDR" + struct.pack(">II", w, h) + b"\x08\x00\x00\x00\x00" + b"\0\0\0\0"


class PreviewBombTests(unittest.TestCase):
    def test_ihdr_larger_than_the_canvas(self):
        self.assertTrue(_preview_exceeds_canvas(png_header(30000, 30000), 100, 100, 100, 100))

    def test_declared_larger_than_the_canvas(self):
        self.assertTrue(_preview_exceeds_canvas(b"junk", 1 << 20, 1 << 20, 100, 100))

    def test_smaller_or_equal_preview_is_kept(self):
        self.assertFalse(_preview_exceeds_canvas(png_header(50, 50), 50, 50, 100, 100))
        self.assertFalse(_preview_exceeds_canvas(png_header(100, 100), 0, 0, 100, 100))
        self.assertFalse(_preview_exceeds_canvas(b"\xff\xd8\xff", None, None, 100, 100))


if __name__ == "__main__":
    unittest.main()
