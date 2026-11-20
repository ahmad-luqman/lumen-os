"""Make the image viewer's small bundled images (#204), written to
apps/viewer/images/ next to the wallpaper the viewer also bundles
(assets/wallpaper/lumen.png):

- `tiles.png`: 96x64, a palette image (PLTE), coloured tiles;
- `grey16.png`: 40x24, 16-bit greyscale (the decoder keeps the top 8 bits);
- `broken.png`: `tiles.png` with one byte of its image data flipped, so its
  IDAT chunk's CRC no longer matches: a file the decoder refuses;
- `costly.png`: 1280x800, 8-bit RGBA, one colour (a few KB): a valid PNG
  lumen-view's `admit` refuses from its header, its inflated rows past
  `MAX_INFLATED_BYTES`;

and, not bundled, the costliest PNGs the viewer takes to decode (lumen-view's
`admit`), for the memory plan's test, both interlaced: `worst.png`, its
biggest size (1280x800) in 8-bit RGB, and `worst16.png`, 16-bit RGBA at the
most pixels that allows (480x800).

Deterministic: the same Pillow and zlib give the same files. They are committed, so
the build never runs this; run it again only to change them, then look at
them.

    uv run python tools/viewer-images.py
"""

from __future__ import annotations

import struct
import sys
import zlib
from pathlib import Path

from PIL import Image

ROOT = Path(__file__).resolve().parent.parent
OUT = ROOT / "apps" / "viewer" / "images"
TESTS = ROOT / "crates" / "lumen-view" / "tests"
# (file, size, bits a sample, samples a pixel): lumen-view's MAX_IMAGE_W x
# MAX_IMAGE_H in 8-bit RGB, and 16-bit RGBA at the most pixels MAX_INFLATED_BYTES
# takes.
WORST = [("worst.png", (1280, 800), 8, 3), ("worst16.png", (480, 800), 16, 4)]

# tiles.png's palette: index -> RGB.
PALETTE = [
    (0x1F, 0x3A, 0x6E),
    (0xE8, 0x6F, 0x51),
    (0x2A, 0x9D, 0x8F),
    (0xE9, 0xC4, 0x6A),
    (0xF4, 0xF1, 0xDE),
    (0x26, 0x46, 0x53),
]
TILE = 16


def tiles() -> Image.Image:
    """96x64: 16-pixel tiles, each a palette entry, in a diagonal pattern."""
    img = Image.new("P", (96, 64))
    flat = [c for rgb in PALETTE for c in rgb]
    img.putpalette(flat + [0] * (768 - len(flat)))
    for y in range(64):
        for x in range(96):
            img.putpixel((x, y), (x // TILE + y // TILE) % len(PALETTE))
    return img


def grey16() -> Image.Image:
    """40x24, 16-bit grey: a horizontal ramp, a darker band in its middle."""
    img = Image.new("I;16", (40, 24))
    for y in range(24):
        for x in range(40):
            v = x * 65535 // 39
            if 8 <= y < 16:
                v //= 2
            img.putpixel((x, y), v)
    return img


def costly() -> Image.Image:
    """1280x800 RGBA, every pixel the same."""
    return Image.new("RGBA", (1280, 800), (0x2A, 0x9D, 0x8F, 0xFF))


def chunk(kind: bytes, data: bytes) -> bytes:
    """One PNG chunk: length, type, data, CRC."""
    return struct.pack(">I", len(data)) + kind + data + struct.pack(">I", zlib.crc32(kind + data))


def worst(size: tuple[int, int], depth: int, samples: int) -> bytes:
    """`size`, RGB (3 samples) or RGBA (4) at `depth` bits a sample,
    Adam7-interlaced: written by hand (Pillow writes neither 16-bit RGBA nor
    interlacing). Every pixel the same, so it compresses to little; the
    decoder still inflates all of it."""
    w, h = size
    values = (0x1234, 0x5678, 0x9ABC, 0xFFFF)[:samples]
    pixel = struct.pack(f">{samples}H", *values) if depth == 16 else bytes(v >> 8 for v in values)
    # Adam7's seven passes: (x0, y0, dx, dy).
    passes = [
        (0, 0, 8, 8),
        (4, 0, 8, 8),
        (0, 4, 4, 8),
        (2, 0, 4, 4),
        (0, 2, 2, 4),
        (1, 0, 2, 2),
        (0, 1, 1, 2),
    ]
    raw = bytearray()
    for x0, y0, dx, dy in passes:
        pw, ph = len(range(x0, w, dx)), len(range(y0, h, dy))
        if pw == 0 or ph == 0:
            continue
        row = b"\0" + pixel * pw
        raw += row * ph
    colour = {3: 2, 4: 6}[samples]
    ihdr = struct.pack(">IIBBBBB", w, h, depth, colour, 0, 0, 1)
    return (
        b"\x89PNG\r\n\x1a\n"
        + chunk(b"IHDR", ihdr)
        + chunk(b"IDAT", zlib.compress(bytes(raw), 9))
        + chunk(b"IEND", b"")
    )


def broken(good: bytes) -> bytes:
    """`good` with the middle byte of its first IDAT's data flipped, its CRC
    left as it was (so it no longer matches)."""
    out, at = bytearray(good), 8
    while out[at + 4 : at + 8] != b"IDAT":
        (length,) = struct.unpack(">I", out[at : at + 4])
        at += 12 + length
    (length,) = struct.unpack(">I", out[at : at + 4])
    out[at + 8 + length // 2] ^= 0xFF
    (crc,) = struct.unpack(">I", out[at + 8 + length : at + 12 + length])
    assert zlib.crc32(out[at + 4 : at + 8 + length]) != crc
    return bytes(out)


def main() -> int:
    OUT.mkdir(parents=True, exist_ok=True)
    tiles().save(OUT / "tiles.png", optimize=False)
    grey16().save(OUT / "grey16.png", optimize=False)
    costly().save(OUT / "costly.png", optimize=False)
    (OUT / "broken.png").write_bytes(broken((OUT / "tiles.png").read_bytes()))
    for name, size, depth, samples in WORST:
        (TESTS / name).write_bytes(worst(size, depth, samples))
        with Image.open(TESTS / name) as check:
            check.load()
            assert check.size == size, check.size
        print(f"{TESTS / name}: {(TESTS / name).stat().st_size} bytes")
    for name in ("tiles.png", "grey16.png", "broken.png", "costly.png"):
        print(f"{OUT / name}: {(OUT / name).stat().st_size} bytes")
    return 0


if __name__ == "__main__":
    sys.exit(main())
