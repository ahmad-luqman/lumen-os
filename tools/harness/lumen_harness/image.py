"""Golden-image comparison for QEMU screendumps.

A golden image is a PNG of a known-good screen region, stored in
`tests/golden/`. `compare` crops the actual screenshot to the golden's
size at a given origin and counts pixels whose channels differ by more than
`tolerance`. On mismatch the caller saves the actual crop and a diff image
next to the test artifacts.

Set `LUMEN_UPDATE_GOLDEN=1` (or `make golden`) to re-record goldens; always
review the new PNGs before committing them.
"""

from __future__ import annotations

import os
from dataclasses import dataclass
from pathlib import Path

from PIL import Image, ImageChops


@dataclass
class ImageDiff:
    width: int
    height: int
    differing: int
    max_delta: int

    @property
    def fraction(self) -> float:
        return self.differing / max(1, self.width * self.height)


def update_requested() -> bool:
    return os.environ.get("LUMEN_UPDATE_GOLDEN") == "1"


def crop(actual: Path, origin: tuple[int, int], size: tuple[int, int]) -> Image.Image:
    img = Image.open(actual).convert("RGB")
    x, y = origin
    w, h = size
    if img.width < x + w or img.height < y + h:
        raise AssertionError(f"screenshot {img.size} smaller than region {origin}+{size}")
    return img.crop((x, y, x + w, y + h))


def compare(
    actual: Image.Image, golden: Image.Image, tolerance: int = 8
) -> tuple[ImageDiff, Image.Image]:
    """Return the diff stats and a diff image (white = differing pixel)."""
    a = actual.convert("RGB")
    g = golden.convert("RGB")
    if a.size != g.size:
        raise AssertionError(f"size mismatch: actual {a.size}, golden {g.size}")
    delta = ImageChops.difference(a, g)
    # Per-pixel max channel delta.
    r, gc, b = delta.split()
    per_pixel = ImageChops.lighter(ImageChops.lighter(r, gc), b)
    mask = per_pixel.point(lambda v: 255 if v > tolerance else 0)
    hist = mask.histogram()
    differing = hist[255]
    max_delta = max((i for i, n in enumerate(per_pixel.histogram()) if n), default=0)
    return ImageDiff(a.width, a.height, differing, max_delta), mask


def assert_matches_golden(
    screenshot: Path,
    golden_path: Path,
    origin: tuple[int, int],
    size: tuple[int, int],
    artifacts: Path,
    *,
    tolerance: int = 0,
    max_fraction: float = 0.0,
) -> ImageDiff | None:
    """Compare a region of `screenshot` to `golden_path`, or re-record it.

    Exact by default: our rendering is deterministic (pinned OVMF, fixed mode,
    pre-rasterized font), and identical on macOS/TCG and Linux/KVM. Loosen
    the tolerance only with a reason written next to the call.
    """
    region = crop(screenshot, origin, size)
    if update_requested():
        golden_path.parent.mkdir(parents=True, exist_ok=True)
        region.save(golden_path)
        return None
    if not golden_path.exists():
        region.save(artifacts / f"{golden_path.stem}.actual.png")
        raise AssertionError(f"missing golden {golden_path}; run `make golden` and review it")
    diff, mask = compare(region, Image.open(golden_path), tolerance)
    if diff.fraction > max_fraction:
        region.save(artifacts / f"{golden_path.stem}.actual.png")
        mask.save(artifacts / f"{golden_path.stem}.diff.png")
        raise AssertionError(
            f"{golden_path.name}: {diff.differing} of {diff.width * diff.height} pixels differ "
            f"(allowed {max_fraction:.2%}, max channel delta {diff.max_delta}); "
            f"see {artifacts}/{golden_path.stem}.actual.png and .diff.png"
        )
    return diff
