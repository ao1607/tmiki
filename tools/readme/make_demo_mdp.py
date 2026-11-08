"""Draw a small 'stacked blocks' illustration in four layers and write it as
an MDP2 (.mdp) file, for tmiki's README screenshots.

    python make_demo_mdp.py OUT.mdp [--rough] [--green] [--flag]

--rough: 背景 + 線画 only
default: 背景 / 塗り / 影 / 線画, yellow roof
--green: the roof on the 塗り layer is green
--flag:  a flag on the 線画 layer
Everything is drawn here; no outside artwork is used.
"""
from __future__ import annotations

import struct
import sys
import zlib
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter

W, H = 960, 600
S = 3  # supersampling

INK = (74, 59, 50, 255)
PAPER = (246, 241, 232, 255)
FLOOR = (233, 224, 208, 255)


def canvas():
    return Image.new("RGBA", (W * S, H * S), (0, 0, 0, 0))


def sc(*xy):
    return [v * S for v in xy]


def finish(img):
    return img.resize((W, H), Image.LANCZOS)


# shapes in canvas coordinates
BLOCKS = [
    # (kind, coords, fill)
    ("rect", (300, 400, 470, 500), (226, 183, 122, 255)),   # bottom left, wood
    ("rect", (490, 400, 660, 500), (108, 155, 210, 255)),   # bottom right, blue
    ("rect", (360, 300, 600, 400), (217, 115, 91, 255)),    # middle, red
    ("arch", (395, 230, 565, 300), (246, 241, 232, 255)),   # (arch hole drawn as paper)
    ("tri", (360, 300, 600, 300, 480, 170), None),          # roof, colour per variant
]
ROOF_YELLOW = (235, 197, 90, 255)
ROOF_GREEN = (126, 178, 120, 255)


def layer_background():
    img = canvas()
    d = ImageDraw.Draw(img)
    d.rectangle(sc(0, 0, W, H), fill=PAPER)
    d.rectangle(sc(0, 500, W, H), fill=FLOOR)
    return finish(img)


def layer_shadow():
    img = canvas()
    d = ImageDraw.Draw(img)
    d.ellipse(sc(270, 488, 700, 528), fill=(60, 45, 35, 70))
    img = img.filter(ImageFilter.GaussianBlur(6 * S))
    d = ImageDraw.Draw(img)
    d.rectangle(sc(296, 380, 664, 500), fill=(0, 0, 0, 0))  # keep the floor shadow off the blocks
    # inner shading on the right side of each block
    d.rectangle(sc(445, 400, 470, 500), fill=(0, 0, 0, 38))
    d.rectangle(sc(635, 400, 660, 500), fill=(0, 0, 0, 38))
    d.rectangle(sc(575, 300, 600, 400), fill=(0, 0, 0, 38))
    d.polygon(sc(480, 170, 600, 300, 560, 300), fill=(0, 0, 0, 30))
    return finish(img)


def layer_fill(roof):
    img = canvas()
    d = ImageDraw.Draw(img)
    for kind, c, fill in BLOCKS:
        if kind == "rect":
            d.rectangle(sc(*c), fill=fill)
        elif kind == "tri":
            d.polygon(sc(*c), fill=roof)
    # a round window in the red block
    d.ellipse(sc(455, 320, 505, 370), fill=(250, 236, 200, 255))
    return finish(img)


def layer_line(flag):
    img = canvas()
    d = ImageDraw.Draw(img)
    w = 5 * S
    for kind, c, _ in BLOCKS:
        if kind == "rect":
            d.rounded_rectangle(sc(*c), radius=4 * S, outline=INK, width=w)
        elif kind == "tri":
            pts = sc(*c)
            d.line(pts + pts[:2], fill=INK, width=w, joint="curve")
    d.ellipse(sc(455, 320, 505, 370), outline=INK, width=w)
    d.line(sc(480, 320, 480, 370), fill=INK, width=3 * S)
    d.line(sc(455, 345, 505, 345), fill=INK, width=3 * S)
    # floor line
    d.line(sc(120, 500, 840, 500), fill=INK, width=3 * S)
    if flag:
        d.line(sc(480, 170, 480, 105), fill=INK, width=w)
        d.polygon(sc(483, 105, 540, 122, 483, 139), fill=(217, 115, 91, 255), outline=INK)
        d.line(sc(483, 105, 540, 122, 483, 139), fill=INK, width=4 * S, joint="curve")
    return finish(img)


def tiles_payload(img):
    """32bpp payload: only the 128x128 tiles that have any alpha."""
    bgra = img.convert("RGBA").tobytes("raw", "BGRA")
    cols, rows = (W + 127) // 128, (H + 127) // 128
    out = []
    for ty in range(rows):
        for tx in range(cols):
            tile = bytearray(128 * 128 * 4)
            any_alpha = False
            for y in range(128):
                py = ty * 128 + y
                if py >= H:
                    break
                x0 = tx * 128
                x1 = min(x0 + 128, W)
                if x0 >= W:
                    continue
                row = bgra[(py * W + x0) * 4:(py * W + x1) * 4]
                if not any_alpha and any(row[3::4]):
                    any_alpha = True
                tile[y * 512:y * 512 + len(row)] = row
            if any_alpha:
                out.append((tx, ty, zlib.compress(bytes(tile), 6)))
    assert out, "empty layer"
    buf = bytearray(struct.pack("<IIIIII", len(out), 128, out[0][0], out[0][1], 0, len(out[0][2]))) + out[0][2]
    for tx, ty, z in out[1:]:
        buf += b"\x00" * ((-len(buf)) % 4)
        buf += struct.pack("<IIII", tx, ty, 0, len(z)) + z
    buf += b"\x00" * ((-len(buf)) % 4)
    return bytes(buf)


def pac(name, payload):
    header = bytearray(132)
    header[0:4] = b"PAC "
    struct.pack_into("<IIII", header, 4, 132 + len(payload), 0, len(payload), len(payload))
    header[68:68 + len(name)] = name.encode()
    return bytes(header) + payload


def write_mdp(path, layers):
    """layers: bottom-up list of (name, image)."""
    rows, packs = [], []
    for i, (name, img) in enumerate(layers):
        b = f"layer{i}img"
        rows.append(
            f'<Layer name="{name}" id="{i}" parentId="-1" width="{W}" height="{H}" ofsx="0" ofsy="0" '
            f'type="32bpp" mode="normal" alpha="255" visible="true" bin="{b}"/>'
        )
        packs.append(pac(b, tiles_payload(img)))
    xml = (
        '<?xml version="1.0" encoding="UTF-8"?>\n'
        f'<Mdiapp width="{W}" height="{H}" dpi="144"><Layers active="{len(layers) - 1}">'
        + "".join(rows)
        + "</Layers></Mdiapp>"
    ).encode()
    pack = b"".join(packs)
    Path(path).write_bytes(b"mdipack\x00" + struct.pack("<III", 0, len(xml), len(pack)) + xml + pack)


def main():
    out = sys.argv[1]
    rough = "--rough" in sys.argv
    roof = ROOF_GREEN if "--green" in sys.argv else ROOF_YELLOW
    flag = "--flag" in sys.argv
    layers = [("背景", layer_background())]
    if not rough:
        layers.append(("塗り", layer_fill(roof)))
        layers.append(("影", layer_shadow()))
    layers.append(("線画", layer_line(flag=flag)))
    write_mdp(out, layers)
    print("wrote", out, [n for n, _ in layers], "green" if roof == ROOF_GREEN else "yellow", "flag" if flag else "")


if __name__ == "__main__":
    main()
