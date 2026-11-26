"""Randomness (#97): `random` from RDRAND and virtio-rng.

Each source alone: QEMU's default CPU (`qemu64`) has no RDRAND, so a boot
with virtio-rng attached tests that source alone; `cpu="max"` has RDRAND,
so a boot with it and no virtio-rng tests the other; and both together.
With neither, `random` refuses and the boot log says why. Two boots give
different bytes, with each source alone. `lspci` shows the virtio-rng
device bound, and a second one declined. A device (backed by an EGD server
silenced once boot confirms it bound) that stops answering is dropped,
`lspci` shows it failed, and `random` goes on from the pool. The pool
itself is host-tested (`cargo test -p lumen-rand`) and the driver checked
in-kernel (`lumen_kernel::rng::*`).
"""

import itertools
import os
import re
import shlex
import socket
import threading
from pathlib import Path
from typing import Self

import pytest
from lumen_harness.calibration import assert_quiet_boot
from lumen_harness.qemu import Devices, Machine
from lumen_harness.screen import PROMPT
from lumen_harness.shell import run

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
    # A driver line with spaces comes quoted: read as one word.
    return [shlex.split(line)[0] for line in lines[1:]]


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
    assert_quiet_boot(m.output)
    assert_random_works(m)


_egd_ids = itertools.count()


class EgdServer:
    """Minimal EGD (Entropy Gathering Daemon) server for QEMU's `rng-egd`.

    Answers QEMU's blocking-read requests (`[0x02, count]`) immediately with
    `os.urandom(count)` until `silence()` is called, then keeps the socket
    open and leaves every later request unanswered.
    """

    def __init__(self) -> None:
        # Short socket directory (macOS limits unix socket paths to ~104 bytes),
        # matching `Machine.qmp_path`.
        sock_dir = Path(os.environ.get("LUMEN_SOCK_DIR") or "/tmp")
        self.path = sock_dir / f"lumen-egd-{os.getpid()}-{next(_egd_ids)}.sock"
        self._sock: socket.socket | None = None
        self._thread: threading.Thread | None = None
        self._stop = threading.Event()
        self._silent = threading.Event()
        self._saw_ignored = threading.Event()
        self._lock = threading.Lock()
        self._answered = 0
        self._ignored = 0
        self.error: str | None = None

    @property
    def qemu_args(self) -> list[str]:
        escaped = str(self.path).replace(",", ",,")
        return [
            "-chardev", f"socket,id=lumen-rng-egd,path={escaped}",
            "-object", "rng-egd,id=lumen-rng-backend,chardev=lumen-rng-egd",
            "-device", "virtio-rng-pci,id=lumen-rng,rng=lumen-rng-backend",
        ]  # fmt: skip

    @property
    def answered(self) -> int:
        with self._lock:
            return self._answered

    @property
    def ignored(self) -> int:
        with self._lock:
            return self._ignored

    def silence(self) -> None:
        self._silent.set()

    def wait_ignored(self, timeout: float = 5) -> bool:
        return self._saw_ignored.wait(timeout)

    def __enter__(self) -> Self:
        self.path.unlink(missing_ok=True)
        sock = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            sock.bind(str(self.path))
            sock.listen(1)
            sock.settimeout(0.1)
        except BaseException:
            sock.close()
            self.path.unlink(missing_ok=True)
            raise
        self._sock = sock
        self._thread = threading.Thread(target=self._serve, daemon=True)
        self._thread.start()
        return self

    def __exit__(self, *exc: object) -> None:
        self._stop.set()
        if self._thread is not None:
            self._thread.join(timeout=5)
        if self._sock is not None:
            self._sock.close()
        self.path.unlink(missing_ok=True)

    def _recv_exact(self, conn: socket.socket, n: int) -> bytes | None:
        buf = bytearray()
        while len(buf) < n and not self._stop.is_set():
            try:
                chunk = conn.recv(n - len(buf))
            except TimeoutError:
                continue
            if not chunk:
                return None
            buf += chunk
        return bytes(buf) if len(buf) == n else None

    def _serve(self) -> None:
        assert self._sock is not None
        conn: socket.socket | None = None
        try:
            while not self._stop.is_set():
                try:
                    conn, _ = self._sock.accept()
                    break
                except TimeoutError:
                    continue
            if conn is None:
                return
            conn.settimeout(0.1)
            with conn:
                while not self._stop.is_set():
                    header = self._recv_exact(conn, 2)
                    if header is None:
                        return
                    cmd, count = header[0], header[1]
                    if cmd != 0x02:
                        self.error = f"unexpected EGD command {cmd:#04x}"
                        return
                    if self._silent.is_set():
                        with self._lock:
                            self._ignored += 1
                        self._saw_ignored.set()
                        continue
                    if count:
                        conn.sendall(os.urandom(count))
                    with self._lock:
                        self._answered += 1
        except OSError as e:
            if not self._stop.is_set():
                self.error = str(e)


@pytest.mark.parametrize(
    ("cpu", "left"),
    [
        (None, "no source left: answering from the pool seeded earlier"),
        ("max", "RDRAND carries on"),
    ],
    ids=["alone", "with-rdrand"],
)
def test_a_virtio_rng_that_stops_answering_is_dropped(boot, cpu, left):
    # An EGD backend answers immediately throughout boot (firmware and the
    # driver's probe); only after boot confirms virtio-rng is bound do we
    # silence it, so the very next `random` misses its 1000 ms deadline
    # deterministically, with no timing precondition on boot or host load.
    with EgdServer() as egd:
        m = shell(boot, extra=egd.qemu_args, cpu=cpu)
        sources = "RDRAND, virtio-rng" if cpu else "virtio-rng"
        assert f"INFO  rng: sources: {sources}\n" in m.output
        assert "WARN  rng" not in m.output
        assert egd.answered >= 1 and egd.error is None
        egd.silence()
        # The request that misses its deadline warns while `random` runs.
        out = run(m, "random", timeout=15, allow=r"WARN  rng: virtio-rng on \S+: ")
        assert HEX.fullmatch(out.strip()) and len(out.strip()) == 32, out
        why = r"the device didn't answer within the request's 1000 ms \(status 0x[0-9a-f]{2}\)"
        lost = re.search(
            rf"WARN  rng: virtio-rng on \S+: ({why}); dropped \(1 DMA run leaked\); {re.escape(left)}\n",
            m.output,
        )
        assert lost, m.output[-2000:]
        assert egd.wait_ignored() and egd.ignored == 1
        assert lspci_driver(m, "virtio rng") == [
            f"virtio-rng: failed: {lost[1]} (1 DMA run leaked)"
        ]
        # The pool goes on answering, without asking the device again.
        assert_random_works(m)
        assert egd.ignored == 1 and egd.error is None
        assert run(m, "random --stats").startswith(
            f"rng: sources: {'RDRAND' if cpu else 'none'}; pool: "
        )
        assert m.output.count("WARN  rng") == 1
        assert m.qemu_complaints() == []


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


def test_a_virtio_rng_without_msix_fails_its_probe(boot):
    # QEMU's `vectors=0`: no MSI-X capability (the device still has INTx,
    # which Lumen doesn't route: no `_PRT`), so it has no interrupt Lumen
    # uses, and there is no polling fallback (#96): the probe fails saying
    # why, before any request.
    m = shell(boot, devices=Devices(rng_no_msix=True))
    why = "MSI-X: no MSI-X capability"
    assert lspci_driver(m, "virtio rng") == [f"virtio-rng: failed: {why}"]
    assert re.search(r"WARN  virtio: \S+: " + re.escape(why) + "\n", m.output)
    assert re.search(
        r"WARN  drivers: \S+: virtio-rng probe failed: " + re.escape(why) + "\n", m.output
    )
    assert re.search(
        r"INFO  drivers: 0 bound, 1 failed, 0 declined, 0 not probed \(\d+ driver rows?\)\n",
        m.output,
    )
    assert "random: no randomness source has answered" in run(m, "random")


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
        # The zeros are warned about (once), and the drop, while `random` runs.
        out = run(m, "random", allow=r"WARN  rng: virtio-rng ")
        assert HEX.fullmatch(out.strip()) and len(out.strip()) == 32, out
        if "; dropped" in m.output:
            break
    else:
        pytest.fail("the device answering zeros was never dropped")
    # Warned about once (rate-limited), then dropped at the 8th in a row:
    # the only two warnings (the ones `random` allowed).
    assert m.output.count("WARN  rng") == 2
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
