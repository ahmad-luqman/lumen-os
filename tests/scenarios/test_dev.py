"""Randomness (#97): `random` from RDRAND and virtio-rng.

Each source alone: QEMU's default CPU (`qemu64`) has no RDRAND, so a boot
with virtio-rng attached tests that source alone; `cpu="max"` has RDRAND,
so a boot with it and no virtio-rng tests the other. With neither, `random`
refuses and the boot log says why. Two boots give different bytes. `lspci`
shows the virtio-rng device bound. The pool itself is host-tested
(`cargo test -p lumen-rand`) and the driver checked in-kernel
(`lumen_kernel::rng::*`).
"""

import re

from lumen_harness.qemu import Devices, Machine
from lumen_harness.screen import PROMPT

SHELLS = r"INFO  shell: shells on serial and the screen; type 'help'\n"
HEX = re.compile(r"[0-9a-f]+")


def shell(boot, **kwargs) -> Machine:
    m = boot("qemutest", **kwargs)
    m.expect(SHELLS, 60)
    m.expect(PROMPT, 60)
    return m


def run(m: Machine, line: str) -> str:
    """Run `line` in the serial shell: what it printed between its echo and
    the next prompt."""
    m.send_line(line)
    echo = m.expect(re.escape(line) + r"\n", 10)
    end = m.expect(PROMPT, 10)
    return m.output[echo.end() : end.start()]


def random_bytes(m: Machine, n: int | None = None) -> str:
    out = run(m, "random" if n is None else f"random {n}").strip()
    want = 2 * (16 if n is None else n)
    assert HEX.fullmatch(out) and len(out) == want, out
    return out


def assert_random_works(m: Machine) -> None:
    a, b = random_bytes(m), random_bytes(m)
    assert a != b
    # Long output is not one repeated block.
    long = random_bytes(m, 4096)
    assert len({long[i : i + 32] for i in range(0, len(long), 32)}) == 256


def test_random_with_only_virtio_rng(boot):
    m = shell(boot, devices=Devices(rng=True))
    assert "INFO  rng: sources: virtio-rng\n" in m.output
    assert_random_works(m)
    assert "WARN  rng" not in m.output


def test_random_with_only_rdrand(boot):
    m = shell(boot, cpu="max")
    assert "INFO  rng: sources: RDRAND\n" in m.output
    assert_random_works(m)
    assert "WARN  rng" not in m.output


def test_random_with_both_sources(boot):
    m = shell(boot, devices=Devices(rng=True), cpu="max")
    assert "INFO  rng: sources: RDRAND, virtio-rng\n" in m.output
    assert_random_works(m)


def test_random_refuses_without_a_source(boot):
    m = shell(boot)
    why = "no randomness source (this CPU has no RDRAND, and no virtio-rng device is bound)"
    # Info, not a warning: QEMU's default machine (and boot's scenarios
    # that check for warnings) has neither source.
    assert f"INFO  rng: {why}: `random` refuses\n" in m.output
    assert run(m, "random") == f"random: {why}\n"
    assert run(m, "rhai -e 'random()'").startswith(f"rhai: Runtime error: random: {why}")


def test_random_usage(boot):
    m = shell(boot, cpu="max")
    for bad in ("random 0", "random 4097", "random x", "random 1 2"):
        assert run(m, bad) == "usage: random [bytes] (1 to 4096)\n", bad
    assert len(random_bytes(m, 1)) == 2


def test_random_differs_between_boots(boot):
    first = random_bytes(shell(boot, devices=Devices(rng=True)), 32)
    second = random_bytes(shell(boot, devices=Devices(rng=True)), 32)
    assert first != second


def test_lspci_shows_rng_bound(boot):
    m = shell(boot, devices=Devices(rng=True))
    rows = run(m, "lspci | where driver = 'virtio-rng: bound' | select name driver")
    assert re.fullmatch(r"\s*name\s+driver\n\s*virtio rng\s+virtio-rng: bound\n", rows), rows
    # Boot offered it to its row, and nothing else was attached.
    assert re.search(r"INFO  drivers: probing virtio-rng on \S+\n", m.output)
    assert re.search(
        r"INFO  drivers: 1 bound, 0 failed, 0 declined, 0 not probed \(\d+ driver rows?\)\n",
        m.output,
    )
    assert "WARN  drivers" not in m.output
