"""The kernel's `screenshot` over serial: its frame read back into a PNG
(the frame is written by crates/lumen-png/src/frame.rs; change them
together; ARCHITECTURE section 11, "Screenshot"):

    @@png-begin <w>x<h> bytes=<n> crc32=<8 hex digits> lines=<k>
    @@png <base64>            (k lines; all but the last 120 characters)
    @@png-end

then `screenshot: <w>x<h>, <n> bytes of PNG (...)` and the prompt. Log
lines may land between the frame's lines (never inside one): `shell.run`
takes them out, and `parse_frame` keeps only lines starting with `@@png`
anyway. Every way a frame can come back wrong (a line lost, a byte
changed, a second frame) fails with what was seen.
"""

from __future__ import annotations

import base64
import binascii
import io
import re
import zlib
from dataclasses import dataclass
from typing import TYPE_CHECKING

from PIL import Image

from lumen_harness.shell import run

if TYPE_CHECKING:
    from lumen_harness.qemu import Machine

PREFIX = "@@png"
BEGIN = re.compile(r"@@png-begin (\d+)x(\d+) bytes=(\d+) crc32=([0-9a-f]{8}) lines=(\d+)")
BODY = "@@png "
END = "@@png-end"
LINE_CHARS = 120
SUMMARY = re.compile(r"^screenshot: (\d+)x(\d+), (\d+) bytes of PNG \(.*\)$", re.MULTILINE)


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


def screenshot_via_serial(m: Machine, timeout: float = 60) -> Image.Image:
    """Run `screenshot` at the serial shell's prompt (back at the prompt
    afterwards) and return the screen it sent, checked (`parse_frame`,
    `Frame.image`, and the summary line naming the same size)."""
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
    return frame.image()
