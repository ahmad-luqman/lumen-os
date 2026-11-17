"""lumen_harness.screenshot: the kernel's `screenshot` frame read back
(#150), every way a frame can come back wrong, and lumen-png's golden file
and frame (written by its Rust tests) decoded and parsed here: the two ends
of the wire format tied together."""

from __future__ import annotations

import base64
import io
import re
import struct
import zlib
from pathlib import Path

import pytest
from lumen_harness.screen import Screen, cursor_box
from lumen_harness.screenshot import (
    LINE_CHARS,
    TORN,
    Frame,
    FrameError,
    assert_matches_screendump,
    parse_frame,
    screenshot_via_serial,
)
from PIL import Image
from qemu_fakes import FakeMachine

LOG = "[    3.141592] INFO  drivers: a log line that landed in between"
GOLDEN = Path(__file__).resolve().parents[2] / "crates/lumen-png/tests/golden"


def png_bytes(img: Image.Image) -> bytes:
    out = io.BytesIO()
    img.save(out, format="PNG")
    return out.getvalue()


def small() -> Image.Image:
    img = Image.new("RGB", (5, 3))
    img.putdata([(x * 40, y * 80, (x + y) * 20) for y in range(3) for x in range(5)])
    return img


def noise() -> Image.Image:
    """Noise, so the PNG spans several body lines."""
    return Image.frombytes("RGB", (40, 30), bytes((i * 7919) % 251 for i in range(40 * 30 * 3)))


def frame_lines(png: bytes, size: tuple[int, int]) -> list[str]:
    """The frame as crates/lumen-png/src/frame.rs writes it."""
    b64 = base64.b64encode(png).decode()
    body = [b64[i : i + LINE_CHARS] for i in range(0, len(b64), LINE_CHARS)]
    begin = (
        f"@@png-begin {size[0]}x{size[1]} bytes={len(png)} crc32={zlib.crc32(png):08x} "
        f"lines={len(body)}"
    )
    return [begin, *(f"@@png {line}" for line in body), "@@png-end"]


def text(lines: list[str]) -> str:
    return "".join(f"{line}\n" for line in lines)


def test_a_frame_parses_into_its_png():
    img = noise()
    png = png_bytes(img)
    lines = frame_lines(png, (40, 30))
    assert len(lines) > 4, "several body lines"
    frame = parse_frame(text(lines))
    assert frame == Frame(40, 30, png)
    assert frame.image().tobytes() == img.tobytes()


def test_lines_between_the_frames_lines_are_skipped():
    png = png_bytes(noise())
    mixed = ["lumen> screenshot", LOG]
    for line in frame_lines(png, (40, 30)):
        mixed += [line, LOG]
    mixed.append("screenshot: 40x30, 1 bytes of PNG (...)")
    assert parse_frame(text(mixed)).png == png


@pytest.mark.parametrize(
    ("change", "message"),
    [
        (lambda ls: [], "no screenshot frame"),
        (lambda ls: ls[1:], "doesn't start with its begin line"),
        (lambda ls: ls[:-1], "doesn't end with '@@png-end'"),
        # A body line lost: the count says so (before the CRC would).
        (lambda ls: ls[:2] + ls[3:], r"\d+ body lines read, the begin line said \d+"),
        # A body line cut short (but not the last).
        (lambda ls: [ls[0], ls[1][:-4], *ls[2:]], "body line 1 has 116 characters, not 120"),
        (lambda ls: [ls[0], "@@pngX" + ls[1][6:], *ls[2:]], "body line 1 of .* isn't one"),
        (lambda ls: [ls[0], ls[1][:-1] + "!", *ls[2:]], "the body isn't base64"),
        # One base64 character changed: same length, wrong CRC.
        (
            lambda ls: [ls[0], ls[1][:-1] + ("A" if ls[1][-1] != "A" else "B"), *ls[2:]],
            "CRC-32 [0-9a-f]{8}, the begin line said [0-9a-f]{8}",
        ),
        (
            lambda ls: [re.sub(r"bytes=\d+", "bytes=7", ls[0]), *ls[1:]],
            r"\d+ bytes of PNG decoded, the begin line said 7",
        ),
        (
            lambda ls: [ls[0].replace("crc32=", "crc32=x"), *ls[1:]],
            "doesn't start with its begin line",
        ),
        # Two frames: the second's begin line is no body line.
        (lambda ls: ls[:-1] + ls, "isn't one"),
    ],
)
def test_a_bad_frame_fails_saying_why(change, message):
    png = png_bytes(noise())
    with pytest.raises(FrameError, match=message):
        parse_frame(text(change(frame_lines(png, (40, 30)))))


def test_a_png_that_isnt_the_frames_size_fails():
    frame = parse_frame(text(frame_lines(png_bytes(small()), (6, 3))))
    with pytest.raises(FrameError, match="the PNG is 5x3 RGB, the frame said 6x3 RGB"):
        frame.image()


def test_a_png_that_isnt_rgb_fails():
    # The kernel writes RGB8 only; an RGBA (or any other mode) PNG of the
    # right size is not what it sent.
    rgba = small().convert("RGBA")
    frame = parse_frame(text(frame_lines(png_bytes(rgba), (5, 3))))
    with pytest.raises(FrameError, match="the PNG is 5x3 RGBA, the frame said 5x3 RGB"):
        frame.image()


def test_a_png_that_doesnt_decode_fails():
    junk = b"\x89PNG\r\n\x1a\n" + b"not a png at all"
    with pytest.raises(FrameError, match="the frame's PNG doesn't decode"):
        parse_frame(text(frame_lines(junk, (5, 3)))).image()


def cut_idat(png: bytes) -> bytes:
    """`png` with its (one) IDAT chunk's data cut in half, the chunk's
    length and CRC recomputed: a file whose header reads fine and whose
    pixels don't."""
    at = png.index(b"IDAT") - 4
    (length,) = struct.unpack(">I", png[at : at + 4])
    data = png[at + 8 : at + 8 + length][: length // 2]
    chunk = (
        struct.pack(">I", len(data))
        + b"IDAT"
        + data
        + struct.pack(">I", zlib.crc32(b"IDAT" + data))
    )
    return png[:at] + chunk + png[at + 12 + length :]


def test_a_png_whose_pixels_dont_decode_fails():
    # Opening reads only the header: the pixels must be decoded too.
    png = cut_idat(png_bytes(noise()))
    Image.open(io.BytesIO(png))  # the header alone is fine
    frame = parse_frame(text(frame_lines(png, (40, 30))))
    with pytest.raises(FrameError, match="the frame's PNG doesn't decode"):
        frame.image()


def test_lumen_pngs_golden_decodes_in_pil():
    # Written by lumen-png's encoder (its test
    # the_golden_png_and_its_frame_are_unchanged pins it byte for byte):
    # PIL, an independent decoder, reads the pixels it was given.
    img = Image.open(GOLDEN / "gradient.png")
    img.load()
    assert (img.size, img.mode) == ((32, 16), "RGB")
    want = [(x * 8 % 256, y * 16 % 256, (x ^ y) * 9 % 256) for y in range(16) for x in range(32)]
    assert img.tobytes() == bytes(c for px in want for c in px)


def test_lumen_pngs_frame_parses_into_its_golden():
    # The frame lumen-png's frame::write made of that file, parsed by the
    # harness's reader.
    frame = parse_frame((GOLDEN / "gradient.frame.txt").read_text())
    assert frame == Frame(32, 16, (GOLDEN / "gradient.png").read_bytes())


def serial(png: bytes, size: tuple[int, int], summary: str) -> str:
    return "lumen> screenshot\n" + text(frame_lines(png, size)) + LOG + "\n" + summary + "\nlumen> "


def test_screenshot_via_serial_runs_the_command_and_checks_it():
    img = small()
    png = png_bytes(img)
    m = FakeMachine(serial(png, (5, 3), f"screenshot: 5x3, {len(png)} bytes of PNG (read 1 ms)"))
    got = screenshot_via_serial(m)
    assert m.sent == ["screenshot"]
    assert got.tobytes() == img.tobytes()


@pytest.mark.parametrize(
    "summary",
    [
        "screenshot: 6x3, {n} bytes of PNG (x)",
        "screenshot: 5x4, {n} bytes of PNG (x)",
        "screenshot: 5x3, {n_plus_1} bytes of PNG (x)",
    ],
)
def test_screenshot_via_serial_fails_when_the_summary_disagrees(summary):
    png = png_bytes(small())
    line = summary.format(n=len(png), n_plus_1=len(png) + 1)
    with pytest.raises(FrameError, match="disagrees with the frame"):
        screenshot_via_serial(FakeMachine(serial(png, (5, 3), line)))


def test_screenshot_via_serial_fails_without_a_summary():
    png = png_bytes(small())
    m = FakeMachine(serial(png, (5, 3), "screenshot: no screen to capture"))
    with pytest.raises(FrameError, match="no summary line"):
        screenshot_via_serial(m)


def test_a_capture_flagged_torn_fails_unless_allowed():
    img = small()
    png = png_bytes(img)
    torn = f"screenshot: 5x3, {len(png)} bytes of PNG (read 1 ms){TORN}"
    with pytest.raises(FrameError, match="a command held the screen during the capture"):
        screenshot_via_serial(FakeMachine(serial(png, (5, 3), torn)))
    got = screenshot_via_serial(FakeMachine(serial(png, (5, 3), torn)), allow_torn=True)
    assert got.tobytes() == img.tobytes()


def test_cursor_box_is_the_cursors_cell():
    # Column 7, row 10 of the 7x16 grid from (32, 144).
    assert cursor_box(Screen(["lumen>"], (7, 10))) == (81, 304, 88, 320)
    assert cursor_box(Screen(["lumen>"], (0, 0))) == (32, 144, 39, 160)
    with pytest.raises(AssertionError, match="below the view"):
        cursor_box(Screen(["lumen>"], None))


# ---- assert_matches_screendump against a machine whose screenshot and
# screendumps are given.

SCREEN = Screen(["lumen> colors", "lumen>"], (7, 10))
CELL = cursor_box(SCREEN)


class DumpMachine:
    def __init__(self, tmp: Path, shot: Image.Image, dumps: list[Image.Image]) -> None:
        self.tmp, self.shot, self.dumps, self.taken = tmp, shot, list(dumps), []

    def screenshot_via_serial(self) -> Image.Image:
        return self.shot

    def screendump(self, name: str) -> Path:
        path = self.tmp / name
        self.dumps.pop(0).save(path)
        self.taken.append(name)
        return path


def screen_image(cursor_on: bool = False, dot: tuple[int, int] | None = None) -> Image.Image:
    img = Image.new("RGB", (200, 400), (15, 20, 32))
    if cursor_on:
        img.paste((230, 230, 230), CELL)
    if dot:
        img.putpixel(dot, (255, 0, 0))
    return img


def check(tmp_path: Path, shot: Image.Image, dumps: list[Image.Image], screens=None) -> DumpMachine:
    m = DumpMachine(tmp_path, shot, dumps)
    readings = iter(screens or [SCREEN, SCREEN])
    assert_matches_screendump(m, tmp_path, "t", read=lambda _: next(readings), pause=0)
    return m


def test_a_matching_screendump_passes_at_once(tmp_path):
    m = check(tmp_path, screen_image(), [screen_image()] * 4)
    assert m.taken == ["t-screendump-1.png"]


def test_a_blink_caught_out_of_phase_is_retaken(tmp_path):
    # The screenshot caught the cursor on, the first screendump off.
    m = check(
        tmp_path, screen_image(cursor_on=True), [screen_image(), screen_image(cursor_on=True)]
    )
    assert m.taken == ["t-screendump-1.png", "t-screendump-2.png"]


def test_one_pixel_outside_the_cursor_fails(tmp_path):
    with pytest.raises(
        AssertionError, match="1 pixels differ from each of 4 screendumps, 1 of them outside"
    ):
        check(tmp_path, screen_image(), [screen_image(dot=(5, 5))] * 4)
    assert (tmp_path / "t-diff.png").exists()


def test_a_cursor_cell_that_never_matches_fails(tmp_path):
    # Inside the cell only, but in every screendump: not a blink.
    with pytest.raises(AssertionError, match="0 of them outside the cursor's cell"):
        check(tmp_path, screen_image(dot=(CELL[0] + 1, CELL[1] + 1)), [screen_image()] * 4)


def test_a_screen_that_changed_meanwhile_fails(tmp_path):
    later = Screen(["lumen> colors", "lumen> x"], (8, 10))
    with pytest.raises(
        AssertionError,
        match=r"(?s)changed during the capture.*row 1: 'lumen>' -> 'lumen> x'.*cursor: \(7, 10\) -> \(8, 10\)",
    ):
        check(tmp_path, screen_image(), [screen_image()], screens=[SCREEN, later])
