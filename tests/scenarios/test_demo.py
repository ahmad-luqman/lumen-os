"""The shell's demos on the screen: `mandelbrot`, drawn by every CPU at
once, and `life`, Conway's Game of Life. Each takes the pixels below the
header (the terminal's canvas), shows its prompt in a status line at the
canvas's bottom, and gives the canvas back on a key typed on the console
that ran it (serial here)."""

import re
import time

from lumen_harness.image import assert_matches_golden
from lumen_harness.qemu import Machine
from PIL import Image

# The canvas: everything below the header of the 1280x800 screen.
TOP = 144
CANVAS = (1280, 800 - TOP)
# The image above the canvas's status line (one 16-pixel row of text).
IMAGE_H = CANVAS[1] - 16
# The terminal's background (`fb::BG`).
BG = (0x0F, 0x14, 0x20)
MANDELBROT = re.compile(
    rf"mandelbrot: 1280x{IMAGE_H}, 1 CPU (\d+) ms, (\d+) CPUs (\d+) ms \((\d+)\.(\d)x\)\n"
)
ROWS = re.compile(r"mandelbrot: rows drawn by each CPU:((?: cpu\d+=\d+)+)\n")


def boot_shell(boot, cpus: int = 1) -> Machine:
    m = boot("qemutest", extra=["-smp", str(cpus)])
    if cpus > 1:
        m.expect(rf"INFO  smp: {cpus} CPUs online\n", 60)
    m.expect(r"INFO  shell: shells on serial and the screen", 60)
    m.expect(r"lumen> ", 60)
    return m


def assert_terminal_back(m: Machine, name: str) -> None:
    """The canvas was given back: the terminal is drawn again, so the middle
    of the screen (no line reaches it yet) is the terminal's background."""
    shot = Image.open(m.screendump(name)).convert("RGB")
    assert shot.getpixel((CANVAS[0] // 2, TOP + CANVAS[1] // 2)) == BG


def test_mandelbrot_golden(boot, artifacts, golden_dir):
    # Drawn by the calling CPU (the boot CPU) alone, then by all four (the
    # same pixels: the golden is the second drawing, with "press a key" in
    # the status line); the times and each CPU's rows on serial.
    m = boot_shell(boot, cpus=4)
    m.send_line("mandelbrot")
    found = m.expect(MANDELBROT, 300)
    per_cpu = m.expect(ROWS, 10)
    m.expect(r"mandelbrot: press a key\n", 10)
    rows = {int(cpu): int(n) for cpu, n in re.findall(r"cpu(\d+)=(\d+)", per_cpu[1])}
    # Each of the four CPUs drew bands, every row once.
    assert sorted(rows) == [0, 1, 2, 3] and sum(rows.values()) == IMAGE_H
    one, cpus, all_ms = int(found[1]), int(found[2]), int(found[3])
    print(f"mandelbrot: 1 CPU {one} ms, {cpus} CPUs {all_ms} ms")
    assert cpus == 4
    # No CPU went without a thread or was left out.
    assert "got no thread" not in m.output and "runs no scheduler" not in m.output
    shot = m.screendump("mandelbrot.png")
    assert_matches_golden(
        shot, golden_dir / "demo-mandelbrot.png", origin=(0, TOP), size=CANVAS, artifacts=artifacts
    )
    m.send("q")
    m.expect(r"lumen> ", 10)
    assert_terminal_back(m, "after-mandelbrot.png")


def test_life_glider_golden(boot, artifacts, golden_dir):
    # A glider, 40 generations on: 10 cells down and right of where it
    # started, with the status line (and its prompt) under the grid.
    m = boot_shell(boot)
    m.send_line("life glider 40")
    m.expect(r"life: glider on 160x80 cells; any key quits\n", 10)
    m.expect(r"life: generation 40, 5 alive\n", 120)
    m.expect(r"life: press a key\n", 10)
    shot = m.screendump("life-glider.png")
    assert_matches_golden(
        shot, golden_dir / "demo-life-glider.png", origin=(0, TOP), size=CANVAS, artifacts=artifacts
    )
    m.send("x")
    m.expect(r"lumen> ", 10)
    assert_terminal_back(m, "after-life.png")


def step_plane(cells: set[tuple[int, int]]) -> set[tuple[int, int]]:
    """One generation on an endless plane (the reference)."""
    counts: dict[tuple[int, int], int] = {}
    for x, y in cells:
        for dy in (-1, 0, 1):
            for dx in (-1, 0, 1):
                if dx or dy:
                    counts[(x + dx, y + dy)] = counts.get((x + dx, y + dy), 0) + 1
    return {c for c, n in counts.items() if n == 3 or (n == 2 and c in cells)}


def rle(text: str) -> set[tuple[int, int]]:
    """A pattern's cells from its RLE (as LifeWiki gives it)."""
    cells, x, y = set(), 0, 0
    for count, tag in re.findall(r"(\d*)([bo$])", text):
        n = int(count or 1)
        if tag == "o":
            cells |= {(x + i, y) for i in range(n)}
        if tag == "$":
            x, y = 0, y + n
        else:
            x += n
    return cells


# Each pattern, from an independent source; 30 generations on the grid
# reach no edge, so the populations are the endless plane's.
PATTERNS = {
    "glider": "bo$2bo$3o!",
    "gun": "24bo$22bobo$12b2o6b2o12b2o$11bo3bo4b2o12b2o$2o8bo5bo3b2o$2o8bo3bob2o4bobo$"
    "10bo5bo7bo$11bo3bo$12b2o!",
    "lwss": "bo2bo$o4b$o3bo$4o!",
    "rpentomino": "b2o$2o$bo!",
    "acorn": "bo5b$3bo3b$2o2b3o!",
}


def test_every_pattern_runs_as_on_a_plane(boot):
    m = boot_shell(boot)
    for name, text in PATTERNS.items():
        cells = rle(text)
        for _ in range(30):
            cells = step_plane(cells)
        m.send_line(f"life {name} 30")
        m.expect(rf"life: {name} on 160x80 cells; any key quits\n", 10)
        m.expect(rf"life: generation 30, {len(cells)} alive\n", 60)
        m.expect(r"life: press a key\n", 10)
        m.send(" ")
        m.expect(r"lumen> ", 10)


def test_life_runs_until_a_key(boot):
    # Without a count it runs, at most 20 generations a second, until any
    # key.
    m = boot_shell(boot)
    m.send_line("life gun")
    m.expect(r"life: gun on 160x80 cells; any key quits\n", 10)
    start = time.monotonic()
    time.sleep(1)
    m.send(" ")
    found = m.expect(r"life: generation (\d+), (\d+) alive\n", 10)
    elapsed = time.monotonic() - start
    m.expect(r"lumen> ", 10)
    generations = int(found[1])
    assert 3 <= generations <= 20 * elapsed + 2, f"{generations} generations in {elapsed:.2f} s"
    assert "life: press a key" not in m.output


def test_life_usage_names_the_bad_argument(boot):
    m = boot_shell(boot)
    usage = r"usage: life \[pattern\] \[generations\]\n"
    m.send_line("life nope")
    m.expect(r"life: unknown pattern 'nope'\n" + usage, 10)
    for name in PATTERNS:
        m.expect(rf"  {name} +\S", 10)
    m.expect(r"lumen> ", 10)
    m.send_line("life glider many")
    m.expect(r"life: 'many' is not a number of generations\n" + usage, 10)
    m.expect(r"lumen> ", 10)
    m.send_line("life glider 4 more")
    m.expect(r"life: too many arguments\n" + usage, 10)
    m.expect(r"lumen> ", 10)
