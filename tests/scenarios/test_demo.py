"""The shell's demos on the screen: `mandelbrot`, drawn by every CPU at
once, and `life`, Conway's Game of Life. Each takes the pixels below the
header (the terminal's canvas) and gives them back on a key, typed on the
console that ran it (serial here)."""

import re
import time

from lumen_harness.image import assert_matches_golden
from lumen_harness.qemu import Machine
from PIL import Image

# The canvas: everything below the header of the 1280x800 screen.
TOP = 144
CANVAS = (1280, 800 - TOP)
# The terminal's background (`fb::BG`).
BG = (0x0F, 0x14, 0x20)
MANDELBROT = re.compile(
    r"mandelbrot: 1280x656, 1 CPU (\d+) ms, (\d+) CPUs (\d+) ms \((\d+)\.(\d)x\)\n"
)


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
    # Drawn by the boot CPU alone, then by all four (the same pixels: the
    # golden is the second drawing); the times on serial.
    m = boot_shell(boot, cpus=4)
    m.send_line("mandelbrot")
    found = m.expect(MANDELBROT, 300)
    per_cpu = m.expect(r"mandelbrot: rows drawn by each CPU:((?: cpu\d+=\d+)+)\n", 10)
    m.expect(r"mandelbrot: press a key\n", 10)
    rows = dict(map(int, r.removeprefix("cpu").split("=")) for r in per_cpu[1].split())
    # Each of the four CPUs drew bands, every row once.
    assert sorted(rows) == [0, 1, 2, 3] and sum(rows.values()) == CANVAS[1]
    one, cpus, all_ms = int(found[1]), int(found[2]), int(found[3])
    print(f"mandelbrot: 1 CPU {one} ms, {cpus} CPUs {all_ms} ms")
    assert cpus == 4
    # Nothing warned that a CPU drew no bands or didn't take its job.
    assert "WARN  demo:" not in m.output
    shot = m.screendump("mandelbrot.png")
    assert_matches_golden(
        shot, golden_dir / "demo-mandelbrot.png", origin=(0, TOP), size=CANVAS, artifacts=artifacts
    )
    m.send("q")
    m.expect(r"lumen> ", 10)
    assert_terminal_back(m, "after-mandelbrot.png")


def test_life_glider_golden(boot, artifacts, golden_dir):
    # A glider, 40 generations on: 10 cells down and right of where it
    # started, with the status line under the grid.
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


def test_life_runs_until_a_key(boot):
    # Without a count it runs (20 generations a second) until any key.
    m = boot_shell(boot)
    m.send_line("life gun")
    m.expect(r"life: gun on 160x80 cells; any key quits\n", 10)
    time.sleep(1)
    m.send(" ")
    found = m.expect(r"life: generation (\d+), (\d+) alive\n", 10)
    m.expect(r"lumen> ", 10)
    assert int(found[1]) >= 1
    assert "life: press a key" not in m.output


def test_life_usage_lists_the_patterns(boot):
    m = boot_shell(boot)
    m.send_line("life nope")
    m.expect(r"usage: life \[pattern\] \[generations\]\n", 10)
    for name in ("glider", "gun", "lwss", "rpentomino", "acorn"):
        m.expect(rf"  {name} +\S", 10)
    m.expect(r"lumen> ", 10)
    m.send_line("life glider many")
    m.expect(r"usage: life \[pattern\] \[generations\]\n", 10)
    m.expect(r"lumen> ", 10)
