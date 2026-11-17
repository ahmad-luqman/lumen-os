"""The desktop's anti-aliased text (`lumen-font`, #148) in the kernel. The
font logic, the cache and the golden mask are tested on the host
(`cargo test -p lumen-font`); the ktest `font::renders_and_caches` checks
the kernel's mask against the same golden. This checks the shell's `font`,
which links fontdue into the kernel that boots (not only the ktest one)."""

import re

from lumen_harness.screen import PROMPT
from lumen_harness.shell import run

SAMPLE = '"Lumen: AVATAR fj 0123" at 24 px'


def test_font_command_renders_the_golden_and_caches(boot):
    # Parsed, rendered once rasterizing each distinct glyph (17), then again
    # rasterizing none (every glyph a hit: 4 the first time, for the
    # repeated characters, plus all 21 again), and the mask is the
    # host's golden, byte for byte.
    m = boot("qemutest")
    m.expect(PROMPT, 60)
    out = run(m, "font")
    lines = out.splitlines()
    assert len(lines) == 3, out
    ms = r"\d+\.\d{3} ms"
    assert re.fullmatch(
        rf"font: Atkinson Hyperlegible Regular, 369 glyphs, parsed in {ms}", lines[0]
    ), out
    assert re.fullmatch(
        rf"font: {re.escape(SAMPLE)}: 251x30, rendered in {ms} \(17 rasterized\), "
        rf"again in {ms} \(0 rasterized\)",
        lines[1],
    ), out
    assert re.fullmatch(
        r"font: cache 25 hits, 17 misses, 0 evictions, \d+ of 262144 bytes; "
        r"mask fnv1a 0x02cb17f54193ee6d \(the golden\)",
        lines[2],
    ), out
