"""The kernel's `screenshot` over serial: its frame read back into a PNG
(the frame is written by crates/lumen-png/src/frame.rs; change them
together; ARCHITECTURE section 11, "Screenshot"):

    @@png-begin <w>x<h> bytes=<n> crc32=<8 hex digits> lines=<k>
    @@png <base64>            (k lines; all but the last 120 characters)
    @@png-end

then `screenshot: <w>x<h>, <n> bytes of PNG (...)` (with `, a command held
the screen (a canvas): may be torn` after it when a command that draws
without the screen's lock held it meanwhile) and the prompt. Log
lines may land between the frame's lines (never inside one): `shell.run`
takes them out, and `parse_frame` keeps only lines starting with `@@png`
anyway. Every way a frame can come back wrong (a line lost, a byte
changed, a second frame) fails with what was seen.

`assert_matches_screendump` compares a screenshot with QEMU's screendump,
pixel for pixel (`test_gui`).
"""

from __future__ import annotations

import base64
import binascii
import io
import re
import time
import zlib
from collections.abc import Callable
from dataclasses import dataclass
from pathlib import Path
from typing import TYPE_CHECKING

from PIL import Image

from lumen_harness.image import compare
from lumen_harness.screen import Screen, cursor_box, read_screen
from lumen_harness.shell import run

if TYPE_CHECKING:
    from lumen_harness.qemu import Machine

PREFIX = "@@png"
BEGIN = re.compile(r"@@png-begin (\d+)x(\d+) bytes=(\d+) crc32=([0-9a-f]{8}) lines=(\d+)")
BODY = "@@png "
END = "@@png-end"
LINE_CHARS = 120
TORN = ", a command held the screen (a canvas): may be torn"
SUMMARY = re.compile(
    rf"^screenshot: (\d+)x(\d+), (\d+) bytes of PNG \([^)]*\)({re.escape(TORN)})?$",
    re.MULTILINE,
)


class FrameError(AssertionError):
    """A screenshot frame that doesn't check out."""


@dataclass(frozen=True)
class Frame:
    width: int
    height: int
    png: bytes

    def image(self) -> Image.Image:
        """The PNG decoded (by PIL, an encoder-independent check), which must
        be an RGB image of the frame's size."""
        try:
            img = Image.open(io.BytesIO(self.png))
            img.load()
        except Exception as e:  # PIL raises several kinds for a bad file
            raise FrameError(f"the frame's PNG doesn't decode: {e}") from e
        if img.size != (self.width, self.height) or img.mode != "RGB":
            raise FrameError(
                f"the PNG is {img.size[0]}x{img.size[1]} {img.mode}, "
                f"the frame said {self.width}x{self.height} RGB"
            )
        return img


def parse_frame(text: str) -> Frame:
    """The one frame in `text` (other lines skipped), checked: its begin
    line first and end line last, the stated number of body lines (each but
    the last exactly LINE_CHARS long), then the PNG's length and CRC-32."""
    lines = [line for line in text.split("\n") if line.startswith(PREFIX)]
    if not lines:
        raise FrameError(f"no screenshot frame in the output:\n{text[-2000:]}")
    begin = BEGIN.fullmatch(lines[0])
    if begin is None:
        raise FrameError(f"the frame doesn't start with its begin line: {lines[0][:200]!r}")
    if lines[-1] != END:
        raise FrameError(f"the frame doesn't end with {END!r}: {lines[-1][:200]!r}")
    width, height, size, n = (int(begin[i]) for i in (1, 2, 3, 5))
    crc = int(begin[4], 16)
    body = lines[1:-1]
    for i, line in enumerate(body):
        if not line.startswith(BODY):
            raise FrameError(f"body line {i + 1} of {len(body)} isn't one: {line[:200]!r}")
    if len(body) != n:
        raise FrameError(f"{len(body)} body lines read, the begin line said {n}")
    chars = [line[len(BODY) :] for line in body]
    for i, line in enumerate(chars[:-1]):
        if len(line) != LINE_CHARS:
            raise FrameError(f"body line {i + 1} has {len(line)} characters, not {LINE_CHARS}")
    try:
        png = base64.b64decode("".join(chars), validate=True)
    except binascii.Error as e:
        raise FrameError(f"the body isn't base64: {e}") from e
    if len(png) != size:
        raise FrameError(f"{len(png)} bytes of PNG decoded, the begin line said {size}")
    if zlib.crc32(png) != crc:
        raise FrameError(f"CRC-32 {zlib.crc32(png):08x}, the begin line said {crc:08x}")
    return Frame(width, height, png)


def screenshot_via_serial(
    m: Machine, timeout: float = 60, *, allow_torn: bool = False
) -> Image.Image:
    """Run `screenshot` at the serial shell's prompt (back at the prompt
    afterwards) and return the screen it sent, checked (`parse_frame`,
    `Frame.image`, and the summary line naming the same size). A capture
    the kernel flags as possibly torn (a command held the screen) fails
    unless `allow_torn`."""
    out = run(m, "screenshot", timeout)
    frame = parse_frame(out)
    summary = SUMMARY.search(out)
    if summary is None:
        raise FrameError(f"no summary line after the frame:\n{out[-500:]}")
    if (int(summary[1]), int(summary[2]), int(summary[3])) != (
        frame.width,
        frame.height,
        len(frame.png),
    ):
        raise FrameError(f"the summary {summary[0]!r} disagrees with the frame")
    if summary[4] and not allow_torn:
        raise FrameError(f"a command held the screen during the capture: {summary[0]!r}")
    return frame.image()


def changed_rows(before: Screen, after: Screen) -> str:
    """What differs between two readings of the screen: the rows (numbered
    from 0, before and after) and the cursor."""
    lines = []
    for i in range(max(len(before.rows), len(after.rows))):
        old = before.rows[i] if i < len(before.rows) else None
        new = after.rows[i] if i < len(after.rows) else None
        if old != new:
            lines.append(f"  row {i}: {old!r} -> {new!r}")
    if before.cursor != after.cursor:
        lines.append(f"  cursor: {before.cursor} -> {after.cursor}")
    return "\n".join(lines)


def assert_matches_screendump(
    m: Machine,
    artifacts: Path,
    name: str,
    *,
    read: Callable[[Machine], Screen] = read_screen,
    tries: int = 4,
    pause: float = 0.3,
) -> None:
    """`screenshot` over serial equals QEMU's screendump, every pixel. The
    terminal's cursor blinks every 500 ms, so the screendump is retaken (up
    to `tries` times, `pause` seconds apart) until it caught the cursor in
    the screenshot's phase. The screen (`read`: `term text`) must be the
    same before and after, so nothing else changed meanwhile. A failure
    says how many pixels differ outside the cursor's cell, and saves the
    difference as `<name>-diff.png`."""
    before = read(m)
    shot = m.screenshot_via_serial()
    shot.save(artifacts / f"{name}-screenshot.png")
    for attempt in range(1, tries + 1):
        dump = Image.open(m.screendump(f"{name}-screendump-{attempt}.png")).convert("RGB")
        diff, mask = compare(shot, dump, tolerance=0)
        if diff.differing == 0:
            break
        if attempt < tries:
            time.sleep(pause)
    after = read(m)
    if (after.rows, after.cursor) != (before.rows, before.cursor):
        raise AssertionError(
            f"the screen changed during the capture:\n{changed_rows(before, after)}"
        )
    if diff.differing:
        box = cursor_box(before)
        a, b = shot.copy(), dump.copy()
        for img in (a, b):
            img.paste((0, 0, 0), box)
        outside, _ = compare(a, b, tolerance=0)
        mask.save(artifacts / f"{name}-diff.png")
        raise AssertionError(
            f"{diff.differing} pixels differ from each of {tries} screendumps, {outside.differing} "
            f"of them outside the cursor's cell {box} (see {name}-diff.png)"
        )
