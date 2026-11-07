"""A hostile MDP2 file's layer is held to the pixel limit by `python -m tmi`
too: its tiles can paint a rectangle of 2^31 x 1 (8 GiB) in 574 bytes."""

from __future__ import annotations

import struct
import unittest
import zlib

FAR = 16_777_214  # FAR * 128 + 128 <= 2^31 - 1
W = 2**31 - 1


def _pac(name: str, payload: bytes) -> bytes:
    header = bytearray(132)
    header[0:4] = b"PAC "
    struct.pack_into("<IIII", header, 4, 132 + len(payload), 0, len(payload), len(payload))
    header[68:68 + len(name)] = name.encode()
    return bytes(header) + payload


def _tiles(cols: list[int]) -> bytes:
    raw = bytearray(128 * 128 * 4)
    raw[0:4] = b"\x00\x00\xff\xff"
    z = zlib.compress(bytes(raw), 9)
    buf = bytearray(struct.pack("<IIIIII", len(cols), 128, cols[0], 0, 0, len(z))) + z
    for col in cols[1:]:
        buf += b"\x00" * ((-len(buf)) % 4)
        buf += struct.pack("<IIII", col, 0, 0, len(z)) + z
    buf += b"\x00" * ((-len(buf)) % 4)
    return bytes(buf)


def hostile_mdp(layer_w: int, layer_h: int, cols: list[int]) -> bytes:
    xml = (
        '<?xml version="1.0" encoding="UTF-8"?>\n<Mdipack width="16" height="16" dpi="72"><Layers>'
        f'<Layer name="L1" id="1" parentId="-1" width="{layer_w}" height="{layer_h}" type="32bpp" bin="b1" alpha="255" visible="true"/>'
        "</Layers></Mdipack>"
    ).encode()
    pack = _pac("b1", _tiles(cols))
    return b"mdipack\x00" + struct.pack("<III", 0, len(xml), len(pack)) + xml + pack


class Mdp2LayerLimitTests(unittest.TestCase):
    def test_a_layer_over_the_pixel_limit_is_refused_before_it_is_decoded(self) -> None:
        from tmi import pixel_limit
        from tmi.formats import mdp2

        raw = hostile_mdp(W, 1, [0, FAR])
        self.assertLess(len(raw), 1000)
        doc = mdp2.parse_mdp2(raw)
        with self.assertRaises(pixel_limit.CanvasTooLargeError) as ctx:
            mdp2.render_layer_crop(doc, doc.layers[0])
        self.assertIn("canvas too large", str(ctx.exception))

    def test_a_far_tile_layer_within_the_limit_still_decodes(self) -> None:
        from tmi.formats import mdp2

        raw = hostile_mdp(1 << 20, 1, [0, (1 << 20) // 128 - 1])
        doc = mdp2.parse_mdp2(raw)
        crop = mdp2.render_layer_crop(doc, doc.layers[0])
        self.assertEqual(crop.rect[1:2], (0,))


if __name__ == "__main__":
    unittest.main()
