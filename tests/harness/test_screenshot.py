"""lumen_harness.screenshot: the kernel's `screenshot` frame read back
(#150), every way a frame can come back wrong, and lumen-png's golden file
and frame (written by its Rust tests) decoded and parsed here: the two ends
of the wire format tied together."""

from __future__ import annotations

import base64
import io
import re
import zlib
from pathlib import Path

import pytest
from lumen_harness.screenshot import (
    LINE_CHARS,
    Frame,
    FrameError,
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


def test_lumen_pngs_golden_decodes_in_pil():
    # Written by lumen-png's encoder (its test
    # the_golden_png_and_its_frame_are_unchanged pins it byte for byte):
    # PIL, an independent decoder, reads the pixels it was given.
    img = Image.open(GOLDEN / "gradient.png")
    img.load()
    assert (img.size, img.mode) == ((32, 16), "RGB")
    want = [(x * 8 % 256, y * 16 % 256, (x ^ y) * 9 % 256) for y in range(16) for x in range(32)]
    assert list(img.getdata()) == want


def test_lumen_pngs_frame_parses_into_its_golden():
    # The frame lumen-png's frame::write made of that file, parsed by the
    # harness's reader.
    frame = parse_frame((GOLDEN / "gradient.frame.txt").read_text())
    assert frame == Frame(32, 16, (GOLDEN / "gradient.png").read_bytes())
    assert frame.image().size == (32, 16)


def serial(png: bytes, size: tuple[int, int], summary: str) -> str:
    return "lumen> screenshot\n" + text(frame_lines(png, size)) + LOG + "\n" + summary + "\nlumen> "


def test_screenshot_via_serial_runs_the_command_and_checks_it():
    img = small()
    png = png_bytes(img)
    m = FakeMachine(serial(png, (5, 3), f"screenshot: 5x3, {len(png)} bytes of PNG (read 1 ms)"))
    got = screenshot_via_serial(m)
    assert m.sent == ["screenshot"]
    assert got.tobytes() == img.tobytes()


def test_screenshot_via_serial_fails_when_the_summary_disagrees():
    png = png_bytes(small())
    m = FakeMachine(serial(png, (5, 3), f"screenshot: 5x3, {len(png) + 1} bytes of PNG (x)"))
    with pytest.raises(FrameError, match="disagrees with the frame"):
        screenshot_via_serial(m)
    m = FakeMachine(serial(png, (5, 3), "screenshot: no screen to capture"))
    with pytest.raises(FrameError, match="no summary line"):
        screenshot_via_serial(m)


def test_a_capture_flagged_torn_fails_unless_allowed():
    img = small()
    png = png_bytes(img)
    torn = (
        f"screenshot: 5x3, {len(png)} bytes of PNG (read 1 ms), a canvas was drawing: may be torn"
    )
    with pytest.raises(FrameError, match="a canvas was drawing during the capture"):
        screenshot_via_serial(FakeMachine(serial(png, (5, 3), torn)))
    got = screenshot_via_serial(FakeMachine(serial(png, (5, 3), torn)), allow_torn=True)
    assert got.tobytes() == img.tobytes()
