"""Make the desktop's wallpaper (#154): an original abstract image, a
diagonal gradient with a few large soft discs of light over it, 1280x800,
written to assets/wallpaper/lumen.png (the kernel embeds that file and
decodes it once, when the desktop starts).

Deterministic: the same Pillow gives the same file. The PNG is committed,
so the kernel's build never runs this; run it again only to change the
wallpaper, then look at the image, and re-record the desktop goldens
(`make golden`) and look at those too.

    uv run python tools/wallpaper.py
"""

from __future__ import annotations

import sys
from pathlib import Path

from PIL import Image, ImageDraw, ImageFilter

WIDTH, HEIGHT = 1280, 800
OUT = Path(__file__).resolve().parent.parent / "assets" / "wallpaper" / "lumen.png"

# The gradient's ends: deep blue (top left) to teal (bottom right).
TOP_LEFT = (0x14, 0x2A, 0x5C)
BOTTOM_RIGHT = (0x1F, 0x7A, 0x8C)

# The discs: (centre x, centre y, radius, colour, opacity out of 255).
DISCS = [
    (240, 660, 420, (0x6F, 0xB7, 0xE8), 110),
    (1060, 160, 380, (0x9C, 0x8C, 0xE0), 100),
    (860, 640, 240, (0x7F, 0xD8, 0xC4), 90),
    (430, 160, 170, (0xC8, 0xE6, 0xFF), 70),
]
BLUR = 70


def gradient() -> Image.Image:
    """TOP_LEFT to BOTTOM_RIGHT along the diagonal."""
    # Each pixel's share of the way from the top-left corner to the
    # bottom-right one, computed on a small grid and stretched (smooth).
    w, h = WIDTH // 8, HEIGHT // 8
    mask = Image.new("L", (w, h))
    mask.putdata([round(255 * (x / (w - 1) + y / (h - 1)) / 2) for y in range(h) for x in range(w)])
    mask = mask.resize((WIDTH, HEIGHT), Image.Resampling.BICUBIC)
    a = Image.new("RGB", (WIDTH, HEIGHT), TOP_LEFT)
    b = Image.new("RGB", (WIDTH, HEIGHT), BOTTOM_RIGHT)
    return Image.composite(b, a, mask)


def main() -> int:
    img = gradient()
    for x, y, r, colour, opacity in DISCS:
        layer = Image.new("L", (WIDTH, HEIGHT), 0)
        ImageDraw.Draw(layer).ellipse((x - r, y - r, x + r, y + r), fill=opacity)
        layer = layer.filter(ImageFilter.GaussianBlur(BLUR))
        img = Image.composite(Image.new("RGB", (WIDTH, HEIGHT), colour), img, layer)
    OUT.parent.mkdir(parents=True, exist_ok=True)
    img.save(OUT, optimize=True)
    print(f"{OUT}: {WIDTH}x{HEIGHT}, {OUT.stat().st_size} bytes")
    return 0


if __name__ == "__main__":
    sys.exit(main())
