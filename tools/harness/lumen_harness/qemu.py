"""QEMU configuration and a scriptable, headless machine.

This module is the single source of truth for how Lumen boots under QEMU:
`make run`, `make debug` and every scenario test build their command line
with `qemu_args`.

`Devices` picks the virtio devices a `Machine` attaches (none by default):
tablet, keyboard, rng, sound (into a wav file), blk (a scratch disk), net
(user networking with TCP ports forwarded from the host) and 9p (a shared
host folder). `Machine(cpu=...)` sets the CPU model (`max` for RDRAND).

`Machine` boots an ISO headless with serial on a pipe (or none at all), then lets a test wait
for output (`expect`, with a timeout, never fixed sleeps), type input
(`send_line` on serial, `type_text` and `press` on the keyboard through QMP),
take screenshots over QMP (`screendump`), and collect the exit
status that the kernel sets through `isa-debug-exit`.

Every `Machine` run writes its artifacts to a directory: `serial.log`
(everything the guest printed), `qemu.log` (QEMU `-d` trace: interrupts, CPU
resets, guest errors), `cmdline.txt`, `environment.txt` (QEMU version,
accelerator, OVMF version) and any screenshots. (`make run` and `make debug`
keep only `qemu.log`, and debug also `serial.log`, in build/run/ and
build/debug/.)
"""

from __future__ import annotations

import itertools
import math
import os
import re
import shutil
import socket
import subprocess
import threading
import time
from collections.abc import Sequence
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Self

from . import BUILD_DIR
from .qmp import QmpClient, QmpError, qcodes_for

# isa-debug-exit: QEMU exits with (code << 1) | 1. Must match kernel qemu.rs.
# Any other status fails a test; the only exception is 0 in the power-off
# scenario, together with a SHUTDOWN event whose reason is guest-shutdown
# (`Machine.wait_event`).
EXIT_SUCCESS = (0x10 << 1) | 1  # 33
EXIT_FAILURE = (0x11 << 1) | 1  # 35
EXIT_CRASH = (0x12 << 1) | 1  # 37


def qemu_binary() -> str:
    """The QEMU to run (override: LUMEN_QEMU)."""
    return os.environ.get("LUMEN_QEMU") or "qemu-system-x86_64"


def ovmf_code() -> Path:
    """Pinned OVMF firmware from tools/fetch-deps.sh (override: LUMEN_OVMF_CODE)."""
    return Path(os.environ.get("LUMEN_OVMF_CODE") or BUILD_DIR / "deps/ovmf/OVMF_CODE.fd")


def ovmf_vars_template() -> Path:
    return Path(os.environ.get("LUMEN_OVMF_VARS") or BUILD_DIR / "deps/ovmf/OVMF_VARS.fd")


def default_accel() -> str:
    """KVM when usable (Linux CI), otherwise TCG. Override with LUMEN_ACCEL."""
    if accel := os.environ.get("LUMEN_ACCEL"):
        return accel
    if os.access("/dev/kvm", os.R_OK | os.W_OK):
        return "kvm"
    return "tcg"


# Successful `--version` probes, per binary (failures are retried: a probe can
# time out while the host is busy booting many guests).
_qemu_versions: dict[str, str] = {}


def qemu_version(binary: str) -> str:
    """First non-blank line of `binary --version`, or why it could not be read
    (never raises: this feeds a report header and environment.txt)."""
    if known := _qemu_versions.get(binary):
        return known
    try:
        run = subprocess.run(
            [binary, "--version"],
            capture_output=True,
            text=True,
            errors="replace",
            timeout=10,
            check=False,
        )
    except FileNotFoundError:
        return f"not found ({binary})"
    except (OSError, subprocess.SubprocessError) as e:
        return f"unknown ({binary}: {e})"
    if run.returncode != 0:
        return f"unknown ({binary} --version exited {run.returncode})"
    first = next((line.strip() for line in run.stdout.splitlines() if line.strip()), "")
    if not first:
        return f"unknown ({binary} --version printed nothing)"
    _qemu_versions[binary] = first
    return first


def ovmf_version() -> str:
    """The OVMF release from fetch-deps' `.version` stamp, with the firmware's
    path; `custom` under LUMEN_OVMF_CODE, `unknown` without a (non-empty) stamp. A custom
    variable store (LUMEN_OVMF_VARS) is noted too."""
    if custom := os.environ.get("LUMEN_OVMF_CODE"):
        firmware = f"custom, LUMEN_OVMF_CODE={custom}"
    else:
        code = ovmf_code()
        stamp = code.parent / ".version"
        try:
            release = stamp.read_text().strip()
        except OSError:
            release = ""
        firmware = f"{release} ({code})" if release else f"unknown, no {stamp} ({code})"
    if vars_template := os.environ.get("LUMEN_OVMF_VARS"):
        firmware += f", custom vars LUMEN_OVMF_VARS={vars_template}"
    return firmware


def environment(accel: str) -> list[str]:
    """The versions behind a run's command line (cmdline.txt names the QEMU
    binary and OVMF file, not their versions), plus the accelerator for quick
    reading. Shown in the pytest header (by the xdist controller; not under -q)
    and saved as environment.txt by every `Machine` run."""
    return [
        f"qemu: {qemu_version(qemu_binary())}",
        f"accel: {accel}",
        f"ovmf: {ovmf_version()}",
    ]


def timeout_scale() -> float:
    """The factor for `Machine.expect` and `wait_exit` timeouts (LUMEN_TIMEOUT_SCALE;
    unset or empty: 1), e.g. for slow CI runners. Each `Machine` reads it once,
    when it is created.

    It must be a finite number above 0, or this raises ValueError naming the
    variable: 0 or less would time out at once, and nan or inf would never time
    out, so a hung kernel would hang the run. For a long wait while debugging,
    set e.g. 100."""
    raw = os.environ.get("LUMEN_TIMEOUT_SCALE")
    if not raw:
        return 1.0
    try:
        scale = float(raw)
    except ValueError:
        scale = math.nan  # not a number at all: rejected below with the rest
    if not (math.isfinite(scale) and scale > 0):
        raise ValueError(f"LUMEN_TIMEOUT_SCALE={raw!r}: expected a finite number above 0, e.g. 2.5")
    return scale


@dataclass
class QemuOptions:
    iso: Path
    vars_file: Path
    headless: bool = True
    accel: str = field(default_factory=default_accel)
    memory: str = "256M"
    serial: str = "stdio"
    # "none" for tests; None when the serial mux (mon:stdio) provides the monitor.
    monitor: str | None = "none"
    qmp_socket: Path | None = None
    log_file: Path | None = None
    trace: str = "cpu_reset,guest_errors"
    gdb: bool = False
    # False: `-no-reboot`, so a reset (a triple fault among them) ends QEMU
    # with status 0, which fails a test; the power-off scenario passes on a
    # 0 that comes with a guest-shutdown event (see EXIT_SUCCESS). True: a
    # reset boots the machine again (the reboot scenarios).
    reboot: bool = False
    # `-cpu` (None: QEMU's default model, qemu64, which has no RDRAND; `max`
    # has every feature the accelerator can give, under TCG and KVM alike).
    cpu: str | None = None
    # Virtio devices, all PCI (see `Devices`; `Machine` fills these in).
    virtio_tablet: bool = False
    virtio_keyboard: bool = False
    virtio_rng: bool = False
    # virtio-sound, played into this wav file through QEMU's wav audiodev.
    sound_wav: Path | None = None
    # virtio-blk backed by this raw image (read-write).
    blk_image: Path | None = None
    # virtio-net on user networking, each (host port, guest port) a TCP
    # forward from the host's 127.0.0.1. None: no virtio-net; (): no forwards.
    net_forwards: tuple[tuple[int, int], ...] | None = None
    # virtio-9p sharing this host folder, mount tag `SHARE_TAG`.
    share_dir: Path | None = None
    extra: list[str] = field(default_factory=list)


# The 9p mount tag the guest looks for (`QemuOptions.share_dir`).
SHARE_TAG = "lumen-share"


def _opt(value: object) -> str:
    """A value inside a QEMU `key=value,...` option: commas doubled, or QEMU
    would read the rest of a path with one as more options."""
    return str(value).replace(",", ",,")


def device_args(o: QemuOptions) -> list[str]:
    """The virtio devices' part of the command line. Each device has a
    `lumen-*` id, so QMP's `query-pci` names it (`qdev_id`)."""
    args: list[str] = []
    if o.virtio_tablet:
        args += ["-device", "virtio-tablet-pci,id=lumen-tablet"]
    if o.virtio_keyboard:
        args += ["-device", "virtio-keyboard-pci,id=lumen-keyboard"]
    if o.virtio_rng:
        args += ["-device", "virtio-rng-pci,id=lumen-rng"]
    if o.sound_wav:
        args += [
            "-audiodev", f"wav,id=lumen-audio,path={_opt(o.sound_wav)}",
            "-device", "virtio-sound-pci,id=lumen-sound,audiodev=lumen-audio",
        ]  # fmt: skip
    if o.blk_image:
        args += [
            "-drive", f"if=none,id=lumen-disk,format=raw,file={_opt(o.blk_image)}",
            "-device", "virtio-blk-pci,id=lumen-blk,drive=lumen-disk",
        ]  # fmt: skip
    if o.net_forwards is not None:
        forwards = "".join(f",hostfwd=tcp:127.0.0.1:{h}-:{g}" for h, g in o.net_forwards)
        # `romfile=` (empty): no iPXE option ROM, so the firmware doesn't
        # spend boot time on network boot.
        args += [
            "-netdev", f"user,id=lumen-net{forwards}",
            "-device", "virtio-net-pci,id=lumen-nic,netdev=lumen-net,romfile=",
        ]  # fmt: skip
    if o.share_dir:
        # security_model=none: files are the QEMU user's, and the guest's
        # chown/chmod failures are ignored (no root needed on the host).
        args += [
            "-fsdev", f"local,id=lumen-fs,path={_opt(o.share_dir)},security_model=none",
            "-device", f"virtio-9p-pci,id=lumen-9p,fsdev=lumen-fs,mount_tag={SHARE_TAG}",
        ]  # fmt: skip
    return args


@dataclass(frozen=True)
class Devices:
    """The virtio devices a `Machine` attaches (none by default). Their
    files live in the machine's artifacts directory: `Machine.wav_path`,
    `Machine.blk_image`, `Machine.share_dir`; forwarded ports are in
    `Machine.forwards` (guest port -> host port)."""

    tablet: bool = False
    keyboard: bool = False
    rng: bool = False
    # virtio-sound, recorded to `Machine.wav_path`.
    sound: bool = False
    # virtio-blk on a zero-filled (sparse) scratch image of this many MiB.
    blk_mib: int | None = None
    # virtio-net on user networking (implied by `forward`).
    net: bool = False
    # Guest TCP ports reachable from the host, each on a free host port.
    forward: tuple[int, ...] = ()
    # virtio-9p sharing a fresh, empty folder, `Machine.share_dir`.
    share: bool = False

    @classmethod
    def every(cls) -> Devices:
        """Every device the harness knows (the smoke scenario boots them all)."""
        return cls(
            tablet=True, keyboard=True, rng=True, sound=True, blk_mib=16, forward=(80,), share=True
        )


# No devices: the default (a frozen value, so one is shared).
NO_DEVICES = Devices()


def free_port() -> int:
    """A TCP port on 127.0.0.1 that nothing listens on right now (the OS
    picks it; QEMU binds it moments later)."""
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as s:
        s.bind(("127.0.0.1", 0))
        return s.getsockname()[1]


def qemu_args(o: QemuOptions) -> list[str]:
    args = [
        qemu_binary(),
        "-machine", "q35",
        "-accel", o.accel,
        "-m", o.memory,
        "-drive", f"if=pflash,format=raw,readonly=on,file={ovmf_code()}",
        "-drive", f"if=pflash,format=raw,file={o.vars_file}",
        "-cdrom", str(o.iso),
        "-boot", "d",
        "-serial", o.serial,
        "-device", "isa-debug-exit,iobase=0xf4,iosize=0x04",
        # A triple fault must end the run, not silently reboot (but for the
        # reboot scenarios, `reboot=True`).
        *([] if o.reboot else ["-no-reboot"]),
    ]  # fmt: skip
    if o.monitor:
        args += ["-monitor", o.monitor]
    if o.headless:
        args += ["-display", "none"]
    if o.qmp_socket:
        args += ["-qmp", f"unix:{o.qmp_socket},server=on,wait=off"]
    if o.log_file and o.trace:
        args += ["-d", o.trace, "-D", str(o.log_file)]
    if o.gdb:
        args += ["-s", "-S"]
    # The RTC keeps UTC (the shell's `date` says so; the tests compare it
    # with the host's clock).
    args += ["-rtc", "base=utc"]
    if o.cpu:
        args += ["-cpu", o.cpu]
    return args + device_args(o) + o.extra


def fresh_vars(dest: Path) -> Path:
    """OVMF needs a writable variable store; give each run its own copy."""
    dest.parent.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(ovmf_vars_template(), dest)
    return dest


class ExpectTimeout(AssertionError):
    pass


_machine_ids = itertools.count()


# How long QEMU holds a key pressed by `Machine.press`. QEMU waits that
# long after each key's press and after each key's release (`ui/input.c`
# `qmp_send_key`), so a press of n keys takes 2n hold times to send:
# measured, 30 shift+a presses reached the guest over 2.4 s.
KEY_HOLD_MS = 20
# How far ahead of QEMU `Machine.press` lets the keys queue, in seconds
# (see there).
KEYS_AHEAD = 3.2


class Machine:
    """A headless QEMU instance driven over serial and QMP."""

    def __init__(
        self,
        iso: Path,
        artifacts: Path,
        *,
        trace_interrupts: bool = False,
        extra_args: Sequence[str] = (),
        serial: bool = True,
        reboot: bool = False,
        devices: Devices = NO_DEVICES,
        cpu: str | None = None,
    ):
        # Read once, before anything starts: a value timeout_scale() rejects fails
        # here, not in the first expect() or wait_exit(). (One so large that it
        # overflows a particular wait fails in _scaled, when that wait starts.)
        self.timeout_scale = timeout_scale()
        self.iso = Path(iso)
        self.artifacts = Path(artifacts)
        self.trace_interrupts = trace_interrupts
        # Appended to the command line, e.g. ["-smp", "2"].
        self.extra_args = list(extra_args)
        # False: no serial port at all (`-serial none`: q35 then has no COM1),
        # so nothing from the guest arrives for expect() (QEMU's own messages
        # still do); drive it with the keyboard and read the screen through
        # screendump().
        self.serial = serial
        # True: a reset boots the machine again instead of ending QEMU.
        self.reboot = reboot
        self.devices = devices
        self.cpu = cpu
        # Where the devices' files go (created by start()). The wav file is
        # QEMU's to write: its header gets the final sizes when QEMU exits.
        self.wav_path = self.artifacts / "audio.wav"
        self.blk_image = self.artifacts / "disk.img"
        self.share_dir = self.artifacts / "share"
        # Guest TCP port -> the host port forwarded to it (on 127.0.0.1).
        self.forwards = {guest: free_port() for guest in devices.forward}
        self.proc: subprocess.Popen[bytes] | None = None
        self._buf = bytearray()
        self._cond = threading.Condition()
        self._pos = 0  # expect() cursor into the normalized output
        self._reader: threading.Thread | None = None
        self._qmp: QmpClient | None = None
        # When QEMU will have sent every key pressed so far (see `press`).
        self._keys_sent_by = 0.0
        # The QMP socket's directory (LUMEN_SOCK_DIR; unset or empty: /tmp). A
        # short one: macOS limits unix socket paths to ~104 bytes.
        self._sock_dir = Path(os.environ.get("LUMEN_SOCK_DIR") or "/tmp")
        # pid: parallel pytest-xdist workers; counter: machines in one process.
        self.qmp_path = self._sock_dir / f"lumen-qmp-{os.getpid()}-{next(_machine_ids)}.sock"

    # -- lifecycle -------------------------------------------------------
    def start(self) -> Self:
        self.artifacts.mkdir(parents=True, exist_ok=True)
        self.qmp_path.unlink(missing_ok=True)
        if self.devices.blk_mib is not None:
            with open(self.blk_image, "wb") as disk:
                disk.truncate(self.devices.blk_mib << 20)
        if self.devices.share:
            self.share_dir.mkdir(exist_ok=True)
        opts = self.options(fresh_vars(self.artifacts / "OVMF_VARS.fd"))
        args = qemu_args(opts)
        (self.artifacts / "cmdline.txt").write_text(" ".join(args) + "\n")
        (self.artifacts / "environment.txt").write_text("\n".join(environment(opts.accel)) + "\n")
        # Lives as long as the machine; closed in close().
        self._serial_log = open(self.artifacts / "serial.log", "wb")  # noqa: SIM115
        self.proc = subprocess.Popen(
            args, stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=subprocess.STDOUT
        )
        self._reader = threading.Thread(target=self._read_loop, daemon=True)
        self._reader.start()
        return self

    def options(self, vars_file: Path) -> QemuOptions:
        """How start() runs QEMU (separate so the harness tests can check it)."""
        trace = "cpu_reset,guest_errors" + (",int" if self.trace_interrupts else "")
        d = self.devices
        return QemuOptions(
            iso=self.iso,
            vars_file=vars_file,
            qmp_socket=self.qmp_path,
            log_file=self.artifacts / "qemu.log",
            trace=trace,
            serial="stdio" if self.serial else "none",
            reboot=self.reboot,
            cpu=self.cpu,
            virtio_tablet=d.tablet,
            virtio_keyboard=d.keyboard,
            virtio_rng=d.rng,
            sound_wav=self.wav_path if d.sound else None,
            blk_image=self.blk_image if d.blk_mib is not None else None,
            net_forwards=(
                tuple((host, guest) for guest, host in self.forwards.items())
                if d.net or d.forward
                else None
            ),
            share_dir=self.share_dir if d.share else None,
            extra=self.extra_args,
        )

    def _read_loop(self) -> None:
        assert self.proc and self.proc.stdout
        fd = self.proc.stdout.fileno()
        while True:
            chunk = os.read(fd, 4096)
            if not chunk:
                break
            self._serial_log.write(chunk)
            self._serial_log.flush()
            with self._cond:
                self._buf += chunk.replace(b"\r", b"")
                self._cond.notify_all()
        with self._cond:
            self._cond.notify_all()

    def close(self) -> None:
        if self._qmp:
            self._qmp.close()
            self._qmp = None
        if self.proc and self.proc.poll() is None:
            self.proc.kill()
            self.proc.wait(timeout=10)
        if self._reader:
            self._reader.join(timeout=5)
        if hasattr(self, "_serial_log"):
            self._serial_log.close()
        if hasattr(self, "qmp_path"):
            self.qmp_path.unlink(missing_ok=True)

    def __enter__(self) -> Self:
        return self.start()

    def __exit__(self, *exc: object) -> None:
        self.close()

    # -- serial ----------------------------------------------------------
    @property
    def output(self) -> str:
        with self._cond:
            return self._buf.decode("utf-8", "replace")

    def expect(self, pattern: str | re.Pattern[str], timeout: float = 30) -> re.Match[str]:
        """Wait until `pattern` (regex) appears after the previous match."""
        rx = re.compile(pattern) if isinstance(pattern, str) else pattern
        wait, said = self._scaled(timeout)
        deadline = time.monotonic() + wait
        with self._cond:
            while True:
                text = self._buf.decode("utf-8", "replace")
                m = rx.search(text, self._pos)
                if m:
                    self._pos = m.end()
                    return m
                remaining = deadline - time.monotonic()
                exited = self.proc is not None and self.proc.poll() is not None
                if remaining <= 0 or (exited and not self._reader_alive()):
                    tail = text[-2000:]
                    why = "QEMU exited" if exited else f"timeout after {said}"
                    raise ExpectTimeout(
                        f"{why} waiting for {rx.pattern!r}\n--- last serial output ---\n{tail}"
                    )
                self._cond.wait(min(remaining, 0.5))

    def _scaled(self, timeout: float) -> tuple[float, str]:
        """The wait for `timeout` under this machine's LUMEN_TIMEOUT_SCALE, and how
        a timeout error says it: when the scale isn't 1, with the unscaled timeout
        and the scale too. A timeout that isn't finite, or a scale so large that
        the wait overflows to inf, raises ValueError: that wait would never time
        out."""
        if not math.isfinite(timeout):
            raise ValueError(f"timeout={timeout!r}: expected a finite number of seconds")
        wait = timeout * self.timeout_scale
        if not math.isfinite(wait):
            raise ValueError(
                f"LUMEN_TIMEOUT_SCALE={self.timeout_scale!r} makes a {timeout:g}s wait "
                "overflow to inf"
            )
        said = f"{wait:g}s"
        if self.timeout_scale != 1:
            said += f" ({timeout:g}s scaled by LUMEN_TIMEOUT_SCALE={self.timeout_scale!r})"
        return wait, said

    def _reader_alive(self) -> bool:
        return self._reader is not None and self._reader.is_alive()

    def send(self, data: str) -> None:
        assert self.proc and self.proc.stdin
        self.proc.stdin.write(data.encode())
        self.proc.stdin.flush()

    def send_line(self, line: str) -> None:
        self.send(line + "\r")

    def wait_exit(self, timeout: float = 30) -> int:
        """Wait for QEMU to exit; returns its status."""
        assert self.proc
        wait, said = self._scaled(timeout)
        try:
            code = self.proc.wait(timeout=wait)
        except subprocess.TimeoutExpired as e:
            raise ExpectTimeout(f"QEMU still running after {said}") from e
        if self._reader:
            self._reader.join(timeout=5)
        return code

    # -- QMP -------------------------------------------------------------
    def qmp(self) -> QmpClient:
        if self._qmp is None:
            self._qmp = QmpClient.connect(self.qmp_path, timeout=10)
        return self._qmp

    def press(self, qcodes: Sequence[str]) -> None:
        """Press `qcodes` together on the guest's keyboard (QMP `send-key`,
        held 20 ms). QEMU queues each key behind the last one, so keys
        pressed back to back arrive in order. The shown virtual terminal's
        shell answers them on the screen, not on serial: wait for what they
        did with
        `lumen_harness.screen.expect_screen`.

        QEMU's input queue holds 1024 entries and silently drops the rest
        (`ui/input.c`): a `send-key` of n keys takes 6n (each key's press
        and release, a sync after each, and the hold time after each), and
        2n hold times to send. So this paces itself: it waits whenever the
        keys queued would take more than `KEYS_AHEAD` seconds to send, which
        is at most 150 entries a second ahead whatever n is (about 480, under
        the limit). `test_a_long_line_of_shifted_keys` checks 150 shifted
        keys arrive."""
        ahead = self._keys_sent_by - time.monotonic()
        if ahead > KEYS_AHEAD:
            time.sleep(ahead - KEYS_AHEAD / 2)
        self.qmp().send_key(qcodes, hold_ms=KEY_HOLD_MS)
        cost = 2 * len(qcodes) * KEY_HOLD_MS / 1000
        self._keys_sent_by = max(self._keys_sent_by, time.monotonic()) + cost

    def type_text(self, text: str) -> None:
        """Type `text` on the guest's keyboard as a US layout would, a key at
        a time (see `press`)."""
        for ch in text:
            self.press(qcodes_for(ch))

    def wait_event(self, name: str, timeout: float = 30) -> dict[str, Any]:
        """Wait for QEMU's QMP event `name` (e.g. `SHUTDOWN`), the timeout
        scaled like `expect`'s. Connect (`qmp()`) before what makes it
        happen: QEMU sends events only to clients already connected."""
        wait, _ = self._scaled(timeout)
        try:
            return self.qmp().wait_event(name, wait)
        except QmpError as e:
            tail = self.output[-2000:]
            raise QmpError(f"{e}\n--- last serial output ---\n{tail}") from e

    def screendump(self, name: str) -> Path:
        """Save the current screen as PNG in the artifacts directory."""
        path = (self.artifacts / name).resolve()
        self.qmp().command("screendump", filename=str(path), format="png")
        # QEMU writes the file before replying, but be robust to slow disks.
        for _ in range(50):
            if path.exists() and path.stat().st_size > 0:
                return path
            time.sleep(0.05)
        raise AssertionError(f"screendump did not produce {path}")
