"""Randomness (#97): `random` from RDRAND and virtio-rng.

Each source alone: QEMU's default CPU (`qemu64`) has no RDRAND, so a boot
with virtio-rng attached tests that source alone; `cpu="max"` has RDRAND,
so a boot with it and no virtio-rng tests the other; and both together.
With neither, `random` refuses and the boot log says why. Two boots give
different bytes, with each source alone. `lspci` shows the virtio-rng
device bound, and a second one declined. A rate-limited device that stops
answering is dropped, `lspci` shows it failed, and `random` goes on from
the pool. The pool itself is host-tested (`cargo test -p lumen-rand`) and
the driver checked in-kernel (`lumen_kernel::rng::*`).
"""

import os
import re

import pytest
from lumen_harness.qemu import Devices, Machine
from lumen_harness.screen import PROMPT

SHELLS = r"INFO  shell: shells on serial and the screen; type 'help'\n"
HEX = re.compile(r"[0-9a-f]+")
NO_SOURCE = (
    "no randomness source has answered (no RDRAND, and no virtio-rng device bound; "
    "QEMU: -cpu max or -device virtio-rng-pci)"
)


def shell(boot, **kwargs) -> Machine:
    m = boot("qemutest", **kwargs)
    m.expect(SHELLS, 60)
    m.expect(PROMPT, 60)
    return m


def run(m: Machine, line: str, timeout: float = 10) -> str:
    """Run `line` in the serial shell: what it printed between its echo and
    the next prompt."""
    m.send_line(line)
    echo = m.expect(re.escape(line) + r"\n", 10)
    end = m.expect(PROMPT, timeout)
    return m.output[echo.end() : end.start()]


def random_bytes(m: Machine, n: int | None = None) -> str:
    out = run(m, "random" if n is None else f"random {n}").strip()
    want = 2 * (16 if n is None else n)
    assert HEX.fullmatch(out) and len(out) == want, out
    return out


def assert_random_works(m: Machine) -> None:
    a, b = random_bytes(m), random_bytes(m)
    assert a != b
    # Long output is not one repeated 16-byte block.
    long = random_bytes(m, 4096)
    assert len({long[i : i + 32] for i in range(0, len(long), 32)}) == 256


def check_sources(m: Machine, sources: str) -> None:
    assert f"INFO  rng: sources: {sources}\n" in m.output
    assert_random_works(m)
    assert run(m, "random --stats").startswith(f"rng: sources: {sources}; pool: ")
    assert "WARN  rng" not in m.output


def test_random_with_only_virtio_rng(boot):
    check_sources(shell(boot, devices=Devices(rng=True)), "virtio-rng")


def mixed(m: Machine) -> int:
    """The bytes mixed into the pool, from `random --stats`."""
    found = re.search(r"pool: (\d+) bytes mixed in;", run(m, "random --stats"))
    assert found, m.output[-500:]
    return int(found[1])


def test_random_with_only_rdrand(boot):
    m = shell(boot, cpu="max")
    # RDRAND's two paths: its 64-byte seed at boot, then 32 bytes per request.
    assert mixed(m) == 64
    random_bytes(m)
    assert mixed(m) == 96
    random_bytes(m, 100)
    assert mixed(m) == 128
    assert "RDRAND: in use, 0 draws failed;" in run(m, "random --stats")
    check_sources(m, "RDRAND")


def test_random_with_both_sources(boot):
    check_sources(shell(boot, devices=Devices(rng=True), cpu="max"), "RDRAND, virtio-rng")


def test_random_refuses_without_a_source(boot):
    m = shell(boot)
    # Info, not a warning: QEMU's default machine (and boot's scenarios
    # that check for warnings) has neither source.
    assert f"INFO  rng: {NO_SOURCE}: `random` refuses until one does\n" in m.output
    assert run(m, "random") == f"random: {NO_SOURCE}\n"
    assert run(m, "rhai -e 'random()'").startswith(f"rhai: Runtime error: random: {NO_SOURCE}")
    assert run(m, "random --stats").startswith("rng: sources: none; pool: 0 bytes mixed in;")
    assert "WARN  rng" not in m.output


def test_random_usage(boot):
    m = shell(boot, cpu="max")
    for bad in ("random 0", "random 4097", "random x", "random 1 2", "random --stats 1"):
        assert run(m, bad) == "usage: random [bytes | --stats] (bytes: 1 to 4096)\n", bad
    assert len(random_bytes(m, 1)) == 2


@pytest.mark.parametrize(
    "source",
    [{"cpu": "max"}, {"devices": Devices(rng=True)}],
    ids=["rdrand", "virtio-rng"],
)
def test_random_differs_between_boots(boot, source):
    first = random_bytes(shell(boot, **source), 32)
    second = random_bytes(shell(boot, **source), 32)
    assert first != second


def lspci_driver(m: Machine, name: str) -> list[str]:
    """`lspci`'s driver column for the functions called `name`."""
    out = run(m, f"lspci | where name = '{name}' | select driver")
    lines = out.splitlines()
    assert re.fullmatch(r"\s*driver", lines[0]), out
    return [line.strip() for line in lines[1:]]


def test_lspci_shows_rng_bound(boot):
    m = shell(boot, devices=Devices(rng=True))
    assert lspci_driver(m, "virtio rng") == ["virtio-rng: bound"]
    # Boot offered it to its row, and nothing else was attached.
    assert re.search(r"INFO  drivers: probing virtio-rng on \S+\n", m.output)
    assert re.search(
        r"INFO  drivers: 1 bound, 0 failed, 0 declined, 0 not probed \(\d+ driver rows?\)\n",
        m.output,
    )
    assert "WARN  drivers" not in m.output


def test_a_second_virtio_rng_is_declined(boot):
    m = shell(boot, devices=Devices(rng=True), extra=["-device", "virtio-rng-pci"])
    assert sorted(lspci_driver(m, "virtio rng")) == [
        "no driver (virtio-rng declined it)",
        "virtio-rng: bound",
    ]
    assert re.search(
        r"INFO  rng: \S+: a virtio-rng device is already bound; one is enough\n", m.output
    )
    assert re.search(
        r"INFO  drivers: 1 bound, 0 failed, 1 declined, 0 not probed \(\d+ driver rows?\)\n",
        m.output,
    )
    assert "WARN" not in m.output
    assert_random_works(m)


@pytest.mark.parametrize(
    ("cpu", "left"),
    [
        (None, "no source left: answering from the pool seeded earlier"),
        ("max", "RDRAND carries on"),
    ],
    ids=["alone", "with-rdrand"],
)
def test_a_virtio_rng_that_stops_answering_is_dropped(boot, cpu, left):
    # 64 bytes per 4 s (the firmware reads from the device too, so a long
    # period slows boot): each `random` asks for 32, so within a few the
    # device has used up its quota and doesn't answer within the request's
    # second.
    m = shell(boot, devices=Devices(rng_rate=(64, 4_000)), cpu=cpu)
    sources = "RDRAND, virtio-rng" if cpu else "virtio-rng"
    assert f"INFO  rng: sources: {sources}\n" in m.output
    for _ in range(10):
        out = run(m, "random", timeout=15)
        # (A warning lands among the output on serial.)
        answers = [line for line in out.splitlines() if HEX.fullmatch(line)]
        assert len(answers) == 1 and len(answers[0]) == 32, out
        if "WARN  rng" in m.output:
            break
    else:
        pytest.fail("the rate-limited device never missed a request's deadline")
    why = r"the device didn't answer within the request's 1000 ms \(status 0x[0-9a-f]{2}\)"
    lost = re.search(
        rf"WARN  rng: virtio-rng on \S+: ({why}); dropped \(1 DMA run leaked\); {re.escape(left)}\n",
        m.output,
    )
    assert lost, m.output[-2000:]
    assert lspci_driver(m, "virtio rng") == [f"virtio-rng: failed: {lost[1]} (1 DMA run leaked)"]
    # The pool goes on answering, without asking the device again.
    assert_random_works(m)
    assert run(m, "random --stats").startswith(
        f"rng: sources: {'RDRAND' if cpu else 'none'}; pool: "
    )
    assert m.output.count("WARN  rng") == 1


def test_a_virtio_rng_answering_zeros_fails_its_probe(boot):
    m = shell(
        boot,
        extra=[
            "-object",
            "rng-random,filename=/dev/zero,id=zeros",
            "-device",
            "virtio-rng-pci,rng=zeros",
        ],
    )
    why = "unusable device: its first answer is 32 bytes of 0x00"
    assert lspci_driver(m, "virtio rng") == [f"virtio-rng: failed: {why}"]
    assert re.search(
        r"WARN  drivers: \S+: virtio-rng probe failed: " + re.escape(why) + "\n", m.output
    )
    assert re.search(
        r"INFO  drivers: 0 bound, 1 failed, 0 declined, 0 not probed \(\d+ driver rows?\)\n",
        m.output,
    )
    refused = "no randomness source has answered (no RDRAND, and the virtio-rng device failed (lspci says why))"
    assert f"INFO  rng: {refused}: `random` refuses until one does\n" in m.output
    assert run(m, "random") == f"random: {refused}\n"


def test_a_virtio_rng_that_turns_to_zeros_is_dropped(boot, tmp_path):
    # 512 random bytes, then zeros: the firmware reads about 160 bytes at
    # boot and the probe 32, so a few `random`s get good answers, then each
    # gets 32 zero bytes: not mixed in, and 8 in a row drop the device.
    backing = tmp_path / "rng.bin"
    backing.write_bytes(os.urandom(512) + bytes(64 * 1024))
    m = shell(
        boot,
        extra=[
            "-object",
            f"rng-random,filename={backing},id=turns",
            "-device",
            "virtio-rng-pci,rng=turns",
        ],
    )
    assert "INFO  rng: sources: virtio-rng\n" in m.output
    for _ in range(40):
        out = run(m, "random")
        answers = [line for line in out.splitlines() if HEX.fullmatch(line)]
        assert len(answers) == 1 and len(answers[0]) == 32, out
        if "; dropped" in m.output:
            break
    else:
        pytest.fail("the device answering zeros was never dropped")
    # Warned about once (rate-limited), then dropped at the 8th in a row.
    assert m.output.count("WARN  rng: virtio-rng answered 32 bytes of 0x00; not mixed in") == 1
    assert "(1 such answers since boot)" in m.output
    why = "the device answered 32 bytes of 0x00 8 times running"
    assert re.search(
        rf"WARN  rng: virtio-rng on \S+: {why}; dropped; no source left: answering from the pool seeded earlier\n",
        m.output,
    )
    assert lspci_driver(m, "virtio rng") == [f"virtio-rng: failed: {why}"]
    assert "8 answers rejected" in run(m, "random --stats")
    assert_random_works(m)
