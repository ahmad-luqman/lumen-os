"""QEMU and gdb command-line construction, the harness's environment overrides,
and what each run records about its environment."""

import math
import os
import re
import shlex
import socket
import struct
import subprocess
import sys
import tempfile
import threading
from contextlib import contextmanager
from pathlib import Path
from typing import Self

import pytest
from lumen_harness import REPO_ROOT, qemu
from lumen_harness.__main__ import debug_options, find_gdb, gdb_binary, gdb_command, run_options
from lumen_harness.netfix import SERVICE_PORTS, NetFixture
from lumen_harness.qemu import (
    DEFAULT_MEMORY,
    Devices,
    ExpectTimeout,
    Machine,
    ovmf_vars_template,
    ovmf_version,
    qemu_args,
    qemu_version,
    timeout_scale,
)
from lumen_harness.qmp import QmpClient, QmpError
from PIL import Image
from qemu_fakes import fake_qemu, opts


def test_headless_run_keeps_display_none_without_monitor():
    args = qemu_args(opts(serial="mon:stdio", monitor=None))
    assert args[args.index("-display") + 1] == "none"
    assert "-monitor" not in args


def test_tests_get_debug_exit_and_no_reboot():
    args = qemu_args(opts())
    assert "isa-debug-exit,iobase=0xf4,iosize=0x04" in args
    assert "-no-reboot" in args
    assert args[args.index("-monitor") + 1] == "none"


def test_reboot_drops_no_reboot(tmp_path):
    # Only the reboot scenarios let a reset boot the machine again.
    assert "-no-reboot" not in qemu_args(opts(reboot=True))
    m = Machine(Path("x.iso"), tmp_path, reboot=True)
    assert "-no-reboot" not in qemu_args(m.options(Path("v.fd")))
    assert "-no-reboot" in qemu_args(Machine(Path("x.iso"), tmp_path).options(Path("v.fd")))


def test_machine_waits_for_events_with_scaled_timeouts(tmp_path, monkeypatch):
    monkeypatch.setenv("LUMEN_TIMEOUT_SCALE", "3")
    m = Machine(Path("x.iso"), tmp_path)
    waits = []

    class FakeQmp:
        def wait_event(self, name, timeout):
            waits.append((name, timeout))
            return {"event": name}

    m._qmp = FakeQmp()  # type: ignore[assignment]
    assert m.wait_event("SHUTDOWN", 10) == {"event": "SHUTDOWN"}
    assert waits == [("SHUTDOWN", 30)]


def test_pointer_events_are_paced_and_scaled_to_the_screen(tmp_path, monkeypatch):
    # Pixels become QEMU's axis values for the screen's size (taken once),
    # and no two pointer events go closer together than POINTER_GAP.
    m = Machine(Path("x.iso"), tmp_path, devices=Devices(tablet=True))
    m._screen_size = (1280, 800)
    clock = [100.0]
    sent = []
    monkeypatch.setattr(qemu.time, "monotonic", lambda: clock[0])
    monkeypatch.setattr(qemu.time, "sleep", lambda s: clock.__setitem__(0, clock[0] + s))

    class FakeQmp:
        def send_input(self, args):
            sent.append((clock[0], args["events"]))

    m._qmp = FakeQmp()  # type: ignore[assignment]
    m.move_pointer(1279, 799)
    m.click("left")
    clock[0] += 1
    m.click("right", release=False)
    m.release("right")
    times = [t for t, _ in sent]
    assert times == pytest.approx([100.0, 100.03, 100.06, 101.06, 101.09])
    assert sent[0][1][0]["data"] == {"axis": "x", "value": 0x7FFF}
    assert sent[0][1][1]["data"] == {"axis": "y", "value": 0x7FFF}
    assert [e[0]["data"] for _, e in sent[1:]] == [
        {"down": True, "button": "left"},
        {"down": False, "button": "left"},
        {"down": True, "button": "right"},
        {"down": False, "button": "right"},
    ]


def test_pointer_events_need_a_tablet(tmp_path):
    # Without one QEMU would give them to the PS/2 mouse, silently.
    m = Machine(Path("x.iso"), tmp_path)
    m._screen_size = (1280, 800)
    for send in (lambda: m.move_pointer(1, 1), m.click, m.release):
        with pytest.raises(ValueError, match="no tablet to point with"):
            send()


def test_make_run_and_debug_attach_a_tablet():
    # So the mouse moves the pointer in their windows.
    for machine in (
        run_options(Path("x.iso"), Path("v.fd"), headless=True),
        debug_options(Path("x.iso"), Path("v.fd"), batch=True),
    ):
        assert "virtio-tablet-pci,id=lumen-tablet" in qemu_args(machine)
    assert "-S" in qemu_args(debug_options(Path("x.iso"), Path("v.fd"), batch=True))


def test_the_screen_size_is_taken_once(tmp_path, monkeypatch):
    # From one screendump, kept for every later pointer event.
    m = Machine(Path("x.iso"), tmp_path, devices=Devices(tablet=True))
    shots = []

    def screendump(name):
        shots.append(name)
        path = tmp_path / name
        Image.new("RGB", (640, 480)).save(path)
        return path

    monkeypatch.setattr(m, "screendump", screendump)
    assert m.screen_size() == (640, 480)
    assert m.screen_size() == (640, 480)
    assert shots == ["screen-size.png"]


def test_machine_event_failures_show_the_serial_output(tmp_path):
    m = Machine(Path("x.iso"), tmp_path)
    m._buf += b"last words from the guest\n"

    class FakeQmp:
        def wait_event(self, name, timeout):
            raise QmpError(f"no {name} event")

    m._qmp = FakeQmp()  # type: ignore[assignment]
    with pytest.raises(
        QmpError, match="(?s)no SHUTDOWN event.*last serial output.*last words from the guest"
    ):
        m.wait_event("SHUTDOWN", 1)


def test_args_include_extra():
    args = qemu_args(opts(extra=["-smp", "4", "-device", "virtio-rng-pci"]))
    assert args[-4:] == ["-smp", "4", "-device", "virtio-rng-pci"]


def test_machine_appends_extra_args_last(tmp_path):
    m = Machine(Path("x.iso"), tmp_path, extra_args=["-smp", "2,maxcpus=4"])
    args = qemu_args(m.options(Path("v.fd")))
    assert args[-2:] == ["-smp", "2,maxcpus=4"]
    assert args[args.index("-qmp") + 1] == f"unix:{m.qmp_path},server=on,wait=off"
    plain = qemu_args(Machine(Path("x.iso"), tmp_path).options(Path("v.fd")))
    assert "-smp" not in plain


def dash_m(args: list[str]) -> list[str]:
    """Every `-m` value in a command line."""
    return [args[i + 1] for i, a in enumerate(args) if a == "-m"]


def test_machine_memory_is_its_one_dash_m(tmp_path):
    # (#278) A scenario sets the guest's RAM through `memory`: QEMU's one
    # `-m`, never a second one appended; every other machine keeps the default.
    m = Machine(Path("x.iso"), tmp_path, memory="384M")
    assert dash_m(qemu_args(m.options(Path("v.fd")))) == ["384M"]
    plain = qemu_args(Machine(Path("x.iso"), tmp_path).options(Path("v.fd")))
    assert dash_m(plain) == [DEFAULT_MEMORY] == ["256M"]
    # A `-m` in the extra args, in any spelling QEMU takes, is refused where
    # the command line is built (`extra_args` is a public list).
    for spelling in (["-m", "512M"], ["--m", "512M"], ["-m=512M"], ["--m=512M"]):
        with pytest.raises(ValueError, match="not `-m` in extra args"):
            qemu_args(opts(extra=spelling))
        late = Machine(Path("x.iso"), tmp_path)
        late.extra_args += spelling
        with pytest.raises(ValueError, match="not `-m` in extra args"):
            qemu_args(late.options(Path("v.fd")))
    # Options that only start with an m are not it, nor `-M` (`-machine`).
    others = ["-machine", "q35,hpet=off", "-mem-prealloc", "-M", "q35"]
    assert dash_m(qemu_args(opts(extra=others))) == [DEFAULT_MEMORY]


def test_trace_events_go_to_the_log_file(tmp_path):
    # Logged with `-d`'s trace to qemu.log (`-D`), under KVM too: never to
    # stderr, which `Machine.output` mixes into serial.
    m = Machine(Path("x.iso"), tmp_path, trace_events=["virtio_input_queue_full"])
    args = qemu_args(m.options(Path("v.fd")))
    assert args[args.index("-trace") + 1] == "virtio_input_queue_full"
    assert args[args.index("-D") + 1] == str(tmp_path / "qemu.log")
    plain = qemu_args(Machine(Path("x.iso"), tmp_path).options(Path("v.fd")))
    assert "-trace" not in plain
    with pytest.raises(ValueError, match="need a log file"):
        qemu_args(opts(log_file=None, trace_events=("x",)))
    # Without `-d` tracing, the events still get `-D`.
    alone = qemu_args(opts(log_file=tmp_path / "q.log", trace="", trace_events=("x",)))
    assert "-d" not in alone and alone[alone.index("-D") + 1] == str(tmp_path / "q.log")
    assert qemu_args(opts(log_file=tmp_path / "q.log", trace="")).count("-D") == 0
    (tmp_path / "qemu.log").write_text("first\n")
    seen = len(m.trace_log())
    with (tmp_path / "qemu.log").open("a") as f:
        f.write("virtio_input_queue_full\n")
    assert m.trace_log() == "first\nvirtio_input_queue_full\n"
    assert m.trace_log(seen) == "virtio_input_queue_full\n"


def test_machine_without_serial_has_no_serial_port(tmp_path):
    # `-serial none` (not just no output): q35 then creates no COM1 at all.
    args = qemu_args(Machine(Path("x.iso"), tmp_path, serial=False).options(Path("v.fd")))
    assert args[args.index("-serial") + 1] == "none" and args.count("-serial") == 1
    plain = qemu_args(Machine(Path("x.iso"), tmp_path).options(Path("v.fd")))
    assert plain[plain.index("-serial") + 1] == "stdio"


def test_com1_can_move_to_another_irq(tmp_path):
    # (#120) No default serial; an isa-serial at COM1's port on stdio, on
    # IRQ 3. Only with serial on stdio.
    args = qemu_args(Machine(Path("x.iso"), tmp_path, com1_irq=3).options(Path("v.fd")))
    assert args[args.index("-serial") + 1] == "none" and args.count("-serial") == 1
    assert args[args.index("-chardev") + 1] == "stdio,id=lumen-com1"
    assert args[args.index("-device", args.index("-chardev")) + 1] == (
        "isa-serial,chardev=lumen-com1,iobase=0x3f8,irq=3"
    )
    with pytest.raises(ValueError, match="com1_irq needs serial on stdio"):
        qemu_args(Machine(Path("x.iso"), tmp_path, serial=False, com1_irq=3).options(Path("v.fd")))


def test_live_machines_never_share_a_qmp_socket(tmp_path):
    # Parallel workers are kept apart by the pid in the name; machines in one
    # process (a test that boots twice) by a per-process counter.
    machines = [Machine(Path("x.iso"), tmp_path) for _ in range(3)]
    paths = {m.qmp_path for m in machines}
    assert len(paths) == 3
    assert all(f"-{os.getpid()}-" in p.name for p in paths)


def test_qemu_version_is_the_first_non_blank_line(tmp_path):
    exe = fake_qemu(tmp_path, 'printf "\\nQEMU emulator version 9.2.0\\nCopyright\\n"')
    assert qemu_version(str(exe)) == "QEMU emulator version 9.2.0"
    # Bytes that aren't UTF-8 are replaced, not raised.
    odd = fake_qemu(tmp_path, 'printf "QEMU \\377\\n"', "odd")
    assert qemu_version(str(odd)) == "QEMU \ufffd"


def test_qemu_version_says_why_it_is_unknown(tmp_path):
    missing = tmp_path / "no-such-qemu"
    assert qemu_version(str(missing)) == f"not found ({missing})"
    failing = fake_qemu(tmp_path, "echo oops; exit 3", "failing")
    assert qemu_version(str(failing)) == f"unknown ({failing} --version exited 3)"
    silent = fake_qemu(tmp_path, "exit 0", "silent")
    assert qemu_version(str(silent)) == f"unknown ({silent} --version printed nothing)"
    not_executable = tmp_path / "not-executable"
    not_executable.write_text("#!/bin/sh\n")
    assert qemu_version(str(not_executable)).startswith(f"unknown ({not_executable}: ")


def test_qemu_version_retries_after_a_failure(tmp_path, monkeypatch):
    # A probe that failed is not remembered; one that worked is.
    monkeypatch.setattr(qemu, "_qemu_versions", {})
    ready = tmp_path / "ready"
    exe = fake_qemu(tmp_path, f'[ -e "{ready}" ] || exit 1; echo "QEMU emulator version 9.2.0"')
    assert qemu_version(str(exe)) == f"unknown ({exe} --version exited 1)"
    ready.touch()
    assert qemu_version(str(exe)) == "QEMU emulator version 9.2.0"
    ready.unlink()
    assert qemu_version(str(exe)) == "QEMU emulator version 9.2.0"


def test_qemu_version_survives_a_timeout(tmp_path, monkeypatch):
    # On a busy host `--version` can time out: say so, and try again next time.
    monkeypatch.setattr(qemu, "_qemu_versions", {})
    exe = fake_qemu(tmp_path, 'echo "QEMU emulator version 9.2.0"')

    def timeout(cmd, **kw):
        raise subprocess.TimeoutExpired(cmd, kw["timeout"])

    with monkeypatch.context() as m:
        m.setattr(qemu.subprocess, "run", timeout)
        assert qemu_version(str(exe)).startswith(f"unknown ({exe}: Command ")
    assert qemu_version(str(exe)) == "QEMU emulator version 9.2.0"


def test_empty_overrides_mean_unset(monkeypatch):
    monkeypatch.setenv("LUMEN_QEMU", "")
    monkeypatch.setenv("LUMEN_OVMF_VARS", "")
    assert qemu_args(opts())[0] == "qemu-system-x86_64"
    assert ovmf_vars_template() == qemu.BUILD_DIR / "deps/ovmf/OVMF_VARS.fd"
    # A set value is still used.
    monkeypatch.setenv("LUMEN_QEMU", "/opt/qemu/bin/qemu-system-x86_64")
    monkeypatch.setenv("LUMEN_OVMF_VARS", "/elsewhere/VARS.fd")
    assert qemu_args(opts())[0] == "/opt/qemu/bin/qemu-system-x86_64"
    assert ovmf_vars_template() == Path("/elsewhere/VARS.fd")


def test_empty_sock_dir_means_unset(tmp_path, monkeypatch):
    monkeypatch.setenv("LUMEN_SOCK_DIR", "")
    assert Machine(Path("x.iso"), tmp_path).qmp_path.parent == Path("/tmp")
    monkeypatch.setenv("LUMEN_SOCK_DIR", str(tmp_path))  # a set value is still used
    assert Machine(Path("x.iso"), tmp_path).qmp_path.parent == tmp_path


def test_empty_timeout_scale_means_unset(monkeypatch):
    monkeypatch.setenv("LUMEN_TIMEOUT_SCALE", "")
    assert timeout_scale() == 1.0
    monkeypatch.setenv("LUMEN_TIMEOUT_SCALE", "2.5")  # a set value is still used
    assert timeout_scale() == 2.5


@pytest.mark.parametrize("raw", ["0", "-1", "nan", "inf", "1e999", "abc", " "])
def test_timeout_scale_must_be_finite_and_positive(monkeypatch, raw):
    # 0 or less would time out at once; nan and inf (1e999 parses to inf) would
    # never time out; a typo or a blank would otherwise surface as a bare float()
    # error inside the first expect().
    monkeypatch.setenv("LUMEN_TIMEOUT_SCALE", raw)
    with pytest.raises(ValueError, match=re.escape(f"LUMEN_TIMEOUT_SCALE={raw!r}: expected")):
        timeout_scale()


@pytest.mark.parametrize(("raw", "scale"), [("1", 1.0), ("0.5", 0.5), ("2.5", 2.5)])
def test_timeout_scale_accepts_finite_positive_values(monkeypatch, raw, scale):
    monkeypatch.setenv("LUMEN_TIMEOUT_SCALE", raw)
    assert timeout_scale() == scale


def test_a_bad_timeout_scale_fails_before_qemu_starts(tmp_path, monkeypatch):
    monkeypatch.setenv("LUMEN_TIMEOUT_SCALE", "abc")
    with pytest.raises(ValueError, match="LUMEN_TIMEOUT_SCALE='abc'"):
        Machine(Path("x.iso"), tmp_path)  # creating it raises, before start()


class FakeQemu:
    """Stands in for the QEMU process: `poll()` gives `code` (None: running), and
    `wait()` records its timeout and times out."""

    def __init__(self, code: int | None = None):
        self.code = code
        self.waits: list[float] = []

    def poll(self) -> int | None:
        return self.code

    def wait(self, timeout: float) -> int:
        self.waits.append(timeout)
        raise subprocess.TimeoutExpired("qemu", timeout)


class FakeClock:
    """Stands in for `time` and the machine's condition variable in expect(). The
    clock moves only when expect() waits, and each wait wakes after at most 0.1s,
    as when serial output arrives, so expect() must re-read the clock rather than
    add up what it asked for. A wait a real Condition would refuse, or one that
    can't move the clock (0, negative, nan), raises, and so does waiting past an
    hour: a wait that never ends fails the test instead of hanging it."""

    LIMIT = 3600.0
    STEP = 0.1

    def __init__(self):
        self.now = 0.0

    def monotonic(self) -> float:
        return self.now

    def wait(self, timeout: float) -> bool:
        if not 0 < timeout <= threading.TIMEOUT_MAX:
            raise AssertionError(f"wait({timeout!r}) can't move the clock or would overflow")
        step = min(timeout, self.STEP)
        self.now += step
        if self.now > self.LIMIT:
            raise AssertionError(f"still waiting after {self.LIMIT:g}s (fake time)")
        return step < timeout  # woken early, as by a notify

    def __enter__(self) -> Self:
        return self

    def __exit__(self, *exc: object) -> None:
        pass


class LiveReader:
    """A serial reader thread still draining QEMU's output."""

    def is_alive(self) -> bool:
        return True


def fake_time(m: Machine, monkeypatch, proc: FakeQemu) -> FakeClock:
    """Runs `m` on `proc` and a FakeClock. monkeypatch.setattr raises if Machine
    no longer has these attributes, so a rename fails the test instead of leaving
    expect() on a real clock or Condition."""
    clock = FakeClock()
    monkeypatch.setattr(qemu, "time", clock)
    monkeypatch.setattr(m, "_cond", clock)
    monkeypatch.setattr(m, "proc", proc)
    return clock


def hung_machine(tmp_path: Path, monkeypatch, scale: str = "") -> tuple[Machine, FakeClock]:
    """A Machine created under LUMEN_TIMEOUT_SCALE=`scale` whose QEMU prints
    nothing and never exits, on a FakeClock. The variable changes afterwards,
    which the machine must ignore: it reads the scale once."""
    monkeypatch.setenv("LUMEN_TIMEOUT_SCALE", scale)
    m = Machine(Path("x.iso"), tmp_path)
    monkeypatch.setenv("LUMEN_TIMEOUT_SCALE", "100")
    return m, fake_time(m, monkeypatch, FakeQemu())


scaled_waits = pytest.mark.parametrize(
    ("scale", "timeout", "wait", "said"),
    [
        ("", 30, 30, "30s"),
        ("2.5", 30.0, 75, "75s (30s scaled by LUMEN_TIMEOUT_SCALE=2.5)"),
        ("0.5", 0.2, 0.1, "0.1s (0.2s scaled by LUMEN_TIMEOUT_SCALE=0.5)"),
        ("1.0000001", 10, 10.000001, "10s (10s scaled by LUMEN_TIMEOUT_SCALE=1.0000001)"),
    ],
    ids=["unscaled", "scaled-up", "scaled-down", "near-one"],
)


@scaled_waits
def test_expect_timeout_shows_the_scaled_wait(tmp_path, monkeypatch, scale, timeout, wait, said):
    m, clock = hung_machine(tmp_path, monkeypatch, scale)
    with pytest.raises(ExpectTimeout) as e:
        m.expect("never", timeout)
    assert clock.now == pytest.approx(wait)  # it waited exactly that long
    assert str(e.value).startswith(f"timeout after {said} waiting for 'never'\n")


@scaled_waits
def test_wait_exit_timeout_shows_the_scaled_wait(tmp_path, monkeypatch, scale, timeout, wait, said):
    m, _ = hung_machine(tmp_path, monkeypatch, scale)
    with pytest.raises(ExpectTimeout) as e:
        m.wait_exit(timeout)
    assert m.proc.waits == [pytest.approx(wait)]
    assert str(e.value) == f"QEMU still running after {said}"


@pytest.mark.parametrize("call", ["expect", "wait_exit"])
def test_a_scale_that_overflows_a_wait_is_rejected(tmp_path, monkeypatch, call):
    # Finite, so it passes timeout_scale(), but 30 x 1e307 is inf: that wait would
    # never time out.
    m, clock = hung_machine(tmp_path, monkeypatch, "1e307")
    with pytest.raises(ValueError, match=re.escape("LUMEN_TIMEOUT_SCALE=1e+307 makes a 30s")):
        if call == "expect":
            m.expect("never", 30)
        else:
            m.wait_exit(30)
    assert clock.now == 0  # rejected before waiting at all
    assert m.proc.waits == []


def test_a_huge_finite_scale_keeps_waiting(tmp_path, monkeypatch):
    # 1e9 is accepted: a 30s wait becomes 3e10s, past what one Condition.wait()
    # takes (threading.TIMEOUT_MAX, about 9.2e9s, raises OverflowError). expect()
    # waits in slices, so it just keeps waiting, here until the fake clock's hour.
    m, clock = hung_machine(tmp_path, monkeypatch, "1e9")
    with pytest.raises(AssertionError, match="still waiting after 3600s"):
        m.expect("never", 30)
    assert clock.now > FakeClock.LIMIT


@pytest.mark.parametrize("call", ["expect", "wait_exit"])
@pytest.mark.parametrize("timeout", [math.inf, math.nan], ids=["inf", "nan"])
def test_a_timeout_that_isnt_finite_is_the_callers_fault(tmp_path, monkeypatch, call, timeout):
    # Not blamed on LUMEN_TIMEOUT_SCALE, which is unset here.
    m, clock = hung_machine(tmp_path, monkeypatch)
    with pytest.raises(ValueError, match=re.escape(f"timeout={timeout!r}: expected a finite")):
        if call == "expect":
            m.expect("never", timeout)
        else:
            m.wait_exit(timeout)
    assert clock.now == 0
    assert m.proc.waits == []


@pytest.mark.parametrize(
    ("reader", "waited"), [(None, 0), (LiveReader(), 30)], ids=["read", "reading"]
)
def test_expect_says_when_qemu_exited(tmp_path, monkeypatch, reader, waited):
    # Not a timeout: once QEMU is gone and its output read, expect() stops at once.
    # While the reader is still draining the output, the pattern may yet appear,
    # so expect() keeps waiting (here to its deadline, unscaled).
    monkeypatch.delenv("LUMEN_TIMEOUT_SCALE", raising=False)
    m = Machine(Path("x.iso"), tmp_path)
    clock = fake_time(m, monkeypatch, FakeQemu(code=0))
    monkeypatch.setattr(m, "_reader", reader)
    with pytest.raises(ExpectTimeout) as e:
        m.expect("never", 30)
    assert clock.now == pytest.approx(waited)
    assert str(e.value).startswith("QEMU exited waiting for 'never'\n")


def test_empty_gdb_means_unset(monkeypatch):
    monkeypatch.setenv("LUMEN_GDB", "")
    assert gdb_binary() == "gdb"
    monkeypatch.setenv("LUMEN_GDB", "x86_64-elf-gdb")  # a set value is still used
    assert gdb_binary() == "x86_64-elf-gdb"
    # `make debug` goes through it (as does find_gdb, which test_devtools' check uses).
    assert gdb_command(Path("kernel"), batch=False)[0] == "x86_64-elf-gdb"


def only_on_path(tmp_path: Path, monkeypatch, *names: str) -> Path:
    """Sets PATH to a new directory, `tmp_path`/bin, holding executables `names`
    (and nothing else, so the host's own gdb is out of sight); returns it."""
    bindir = tmp_path / "bin"
    bindir.mkdir()
    for name in names:
        fake_qemu(bindir, "exit 0", name)
    monkeypatch.setenv("PATH", str(bindir))
    return bindir


@pytest.mark.parametrize("value", [None, ""], ids=["unset", "empty"])
def test_find_gdb_without_gdb_or_an_override_is_none(tmp_path, monkeypatch, value):
    # gdb is optional: make debug's scenario skips as "gdb not installed".
    if value is None:
        monkeypatch.delenv("LUMEN_GDB", raising=False)
    else:
        monkeypatch.setenv("LUMEN_GDB", value)  # empty is unset
    bindir = only_on_path(tmp_path, monkeypatch)
    assert find_gdb() is None
    fake_qemu(bindir, "exit 0", "gdb")
    assert find_gdb() == str(bindir / "gdb")  # and with gdb, that one


@pytest.mark.parametrize(
    ("on_path", "value", "found"),
    [
        (["gdb", "x86_64-elf-gdb"], "x86_64-elf-gdb", "{tmp}/bin/x86_64-elf-gdb"),
        (["x86_64-elf-gdb"], "x86_64-elf-gdb", "{tmp}/bin/x86_64-elf-gdb"),
        ([], "{tmp}/my-gdb", "{tmp}/my-gdb"),
    ],
    ids=["name", "name-alone", "path"],
)
def test_find_gdb_uses_an_override_that_is_found(tmp_path, monkeypatch, on_path, value, found):
    # A name wins over a plain `gdb` on PATH and doesn't need one (e.g. a Mac with
    # only x86_64-elf-gdb). A path off PATH works too, also without a plain `gdb`.
    only_on_path(tmp_path, monkeypatch, *on_path)
    fake_qemu(tmp_path, "exit 0", "my-gdb")  # off PATH
    monkeypatch.setenv("LUMEN_GDB", value.format(tmp=tmp_path))
    assert find_gdb() == found.format(tmp=tmp_path)


def not_found(value: str) -> str:
    """A regex for exactly find_gdb's message about LUMEN_GDB=`value`, nothing
    before or after it."""
    message = f"LUMEN_GDB={value!r} not found or not executable (give a name on PATH or a path)"
    return f"^{re.escape(message)}$"


@pytest.mark.parametrize(
    "value",
    [
        pytest.param("{tmp}/nowhere/gdb", id="missing-path"),
        pytest.param("x86_64-elf-gdb", id="missing-name"),
        pytest.param(" ", id="blank"),
        pytest.param(" gdb ", id="padded"),
        pytest.param("{tmp}", id="directory"),
        pytest.param("{tmp}/plain", id="not-executable"),
    ],
)
def test_find_gdb_raises_on_an_override_that_isnt_found(tmp_path, monkeypatch, value):
    # A plain `gdb` is on PATH: a bad override must not fall back to it silently.
    only_on_path(tmp_path, monkeypatch, "gdb")
    (tmp_path / "plain").write_text("")  # a file, but not executable
    value = value.format(tmp=tmp_path)
    monkeypatch.setenv("LUMEN_GDB", value)
    with pytest.raises(ValueError, match=not_found(value)):
        find_gdb()


def test_find_gdb_raises_on_an_explicit_gdb_that_isnt_found(tmp_path, monkeypatch):
    # LUMEN_GDB=gdb is still set: no gdb at all fails, it doesn't skip.
    only_on_path(tmp_path, monkeypatch)
    monkeypatch.setenv("LUMEN_GDB", "gdb")
    with pytest.raises(ValueError, match=not_found("gdb")):
        find_gdb()


def run_debug_scenario(tmp_path: Path, lumen_gdb: str | None) -> tuple[str, str]:
    """make debug's scenario in a pytest of its own; returns its whole output and
    its last line (pytest's summary, e.g. `1 failed in 0.01s`).

    No gdb is on PATH, and "QEMU" exits at once, so nothing boots even if the
    check is bypassed. No PYTEST_* or LUMEN_* variables are inherited: LUMEN_GDB is
    `lumen_gdb`, or unset when that is None. No cache, so its deliberate failure
    never reaches the repo's lastfailed (which CI's --last-failed re-run reads);
    -rs prints the skip reason whatever pyproject's addopts say; PY_COLORS=0 keeps
    colour codes out of the summary."""
    empty = tmp_path / "empty"
    empty.mkdir()
    env = {k: v for k, v in os.environ.items() if not k.startswith(("PYTEST_", "LUMEN_"))}
    env["PATH"] = str(empty)
    env["PY_COLORS"] = "0"
    env["LUMEN_QEMU"] = str(fake_qemu(tmp_path, "exit 0"))
    if lumen_gdb is not None:
        env["LUMEN_GDB"] = lumen_gdb
    node = "tests/scenarios/test_devtools.py::test_make_debug_stops_at_kmain"
    run = subprocess.run(
        [sys.executable, "-m", "pytest", "-q", "-rs", "-p", "no:cacheprovider", "-p", "no:xdist",
         node],
        cwd=REPO_ROOT, env=env, capture_output=True, text=True, timeout=120, check=False,
    )  # fmt: skip
    return run.stdout + run.stderr, run.stdout.strip().splitlines()[-1]


def test_the_debug_scenario_fails_on_a_lumen_gdb_that_isnt_found(tmp_path):
    # Failing, not skipping: the override was set on purpose, a skip is easy to
    # miss locally (pytest still exits 0), and the fix is the variable, not gdb.
    out, summary = run_debug_scenario(tmp_path, "/nonexistent/gdb")
    assert re.fullmatch(r"1 failed in [\d.]+s", summary), out  # failed; not skipped, no error
    assert "ValueError: LUMEN_GDB='/nonexistent/gdb' not found" in out, out


def test_the_debug_scenario_skips_without_gdb(tmp_path):
    out, summary = run_debug_scenario(tmp_path, None)
    assert re.fullmatch(r"1 skipped in [\d.]+s", summary), out
    assert "gdb not installed" in out, out


def test_ovmf_version_comes_from_the_fetch_stamp(tmp_path, monkeypatch):
    monkeypatch.setenv("LUMEN_OVMF_CODE", "")  # empty is unset, as for LUMEN_ACCEL
    monkeypatch.delenv("LUMEN_OVMF_VARS", raising=False)
    monkeypatch.setattr(qemu, "BUILD_DIR", tmp_path)
    code = tmp_path / "deps/ovmf/OVMF_CODE.fd"
    stamp = code.parent / ".version"
    assert ovmf_version() == f"unknown, no {stamp} ({code})"
    stamp.parent.mkdir(parents=True)
    stamp.write_text("\n")  # a truncated stamp is no version either
    assert ovmf_version() == f"unknown, no {stamp} ({code})"
    stamp.write_text("edk2-stable202605-r1\n")
    assert ovmf_version() == f"edk2-stable202605-r1 ({code})"
    monkeypatch.setenv("LUMEN_OVMF_VARS", "/elsewhere/VARS.fd")
    assert ovmf_version() == (
        f"edk2-stable202605-r1 ({code}), custom vars LUMEN_OVMF_VARS=/elsewhere/VARS.fd"
    )
    monkeypatch.delenv("LUMEN_OVMF_VARS")
    monkeypatch.setenv("LUMEN_OVMF_CODE", "/elsewhere/CODE.fd")
    assert ovmf_version() == "custom, LUMEN_OVMF_CODE=/elsewhere/CODE.fd"


@pytest.mark.parametrize("accel", ["kvm", "tcg"])
def test_each_run_records_its_environment(tmp_path, monkeypatch, accel):
    exe = fake_qemu(tmp_path, 'echo "QEMU emulator version 0.0 (fake)"')
    vars_template = tmp_path / "VARS.fd"
    vars_template.write_bytes(b"vars")
    monkeypatch.setenv("LUMEN_QEMU", str(exe))
    monkeypatch.setenv("LUMEN_OVMF_VARS", str(vars_template))
    monkeypatch.setenv("LUMEN_OVMF_CODE", "/x/CODE.fd")
    monkeypatch.setenv("LUMEN_ACCEL", accel)
    with Machine(Path("x.iso"), tmp_path / "run") as m:
        m.wait_exit(10)
    recorded = (tmp_path / "run/environment.txt").read_text().splitlines()
    assert recorded == [
        "qemu: QEMU emulator version 0.0 (fake)",
        f"accel: {accel}",
        f"ovmf: custom, LUMEN_OVMF_CODE=/x/CODE.fd, custom vars LUMEN_OVMF_VARS={vars_template}",
    ]
    assert f"-accel {accel} " in (tmp_path / "run/cmdline.txt").read_text()


def test_rtc_keeps_utc():
    # The shell's `date` says UTC; test_date_matches_host compares it with
    # the host's clock in UTC.
    args = qemu_args(opts())
    assert args[args.index("-rtc") + 1] == "base=utc"


@pytest.mark.parametrize("mode", ["net", "forward", "services"])
def test_net_scenarios_are_restricted(tmp_path, mode):
    services = ((80, Path("/tmp/http.sock")), (443, Path("/tmp/https.sock")))
    devices = {
        "net": Devices(net=True),
        "forward": Devices(forward=(22,)),
        "services": Devices(services=services),
    }[mode]
    machine = Machine(Path("x.iso"), tmp_path, devices=devices)
    args = qemu_args(machine.options(Path("v.fd")))
    net = args[args.index("-netdev") + 1]
    assert ",restrict=on,ipv6=off" in net
    if mode == "services":
        for port, path in services:
            assert f",guestfwd=tcp:10.0.2.100:{port}-cmd:" in net
            assert f"--relay {path}" in net
        assert net.count("guestfwd=") == 2
    else:
        assert "guestfwd=" not in net
    assert "user,id=lumen-net,restrict=on,ipv6=off" in qemu_args(opts(net_forwards={}))


def test_service_paths_are_quoted_and_qemu_commas_escaped(tmp_path):
    path = Path("/tmp/a,b with 'quotes'.sock")
    machine = Machine(Path("x.iso"), tmp_path, devices=Devices(services=((80, path),)))
    args = qemu_args(machine.options(Path("v.fd")))
    net = args[args.index("-netdev") + 1]
    assert "a,,b" in net
    command = net.split("-cmd:", 1)[1].replace(",,", ",")
    argv = shlex.split(command)
    assert argv[:3] == [sys.executable, "-W", "ignore"]
    assert argv[-4:] == [
        "--relay",
        str(path),
        "--log",
        str(path.parent / "relay.log"),
    ]


@pytest.mark.parametrize(
    "services",
    [
        ((0, "/a"),),
        ((True, "/a"),),
        ((65536, "/a"),),
        ((80, "/a"), (80, "/b")),
        ((80, "/a\nb"),),
        ((80, "/a\0b"),),
        ((80, "/" + "a" * 103),),
        ((80, "/" + "é" * 52),),
    ],
)
def test_service_ports_are_valid_and_unique(services):
    with pytest.raises(ValueError, match="services"):
        Devices(services=services)


def test_qemu_has_user_networking():
    # Runs on Ubuntu CI as well as Homebrew; no silent skip for a QEMU
    # package built without libslirp.
    qemu.require_user_network()


def test_missing_slirp_has_an_actionable_error(tmp_path, monkeypatch):
    binary = fake_qemu(tmp_path, 'echo "Available netdev backend types: socket"')
    monkeypatch.setenv("LUMEN_QEMU", str(binary))
    with pytest.raises(RuntimeError, match=r"user networking \(libslirp\).*qemu-system-x86"):
        qemu.require_user_network()


def _checksum(data):
    data += bytes(len(data) % 2)
    total = sum(struct.unpack(f"!{len(data) // 2}H", data))
    while total >> 16:
        total = (total & 65535) + (total >> 16)
    return ~total & 65535


_SOURCE = socket.inet_aton("10.0.2.15")
_GUEST_MAC = bytes.fromhex("525400123456")
_HOST_MAC = bytes.fromhex("52550a000202")


def _ip_packet(destination, protocol, payload, ip_source=_SOURCE):
    header = struct.pack(
        "!BBHHHBBH4s4s",
        0x45,
        0,
        20 + len(payload),
        1,
        0,
        64,
        protocol,
        0,
        ip_source,
        socket.inet_aton(destination),
    )
    header = header[:10] + struct.pack("!H", _checksum(header)) + header[12:]
    return _HOST_MAC + _GUEST_MAC + b"\x08\x00" + header + payload


def _tcp(destination, port, seq=100, ack=0, flags=2, payload=b"", source_port=40000):
    header = struct.pack("!HHIIBBHHH", source_port, port, seq, ack, 5 << 4, flags, 65535, 0, 0)
    pseudo = (
        _SOURCE
        + socket.inet_aton(destination)
        + struct.pack("!BBH", 0, 6, len(header) + len(payload))
    )
    header = header[:16] + struct.pack("!H", _checksum(pseudo + header + payload)) + header[18:]
    return _ip_packet(destination, 6, header + payload)


def _l4_start(frame):
    return 14 + (frame[14] & 15) * 4


def _tcp_payload(frame):
    start = _l4_start(frame)
    data_start = start + (frame[start + 12] >> 4) * 4
    ip_end = 14 + struct.unpack("!H", frame[16:18])[0]
    return frame[data_start:ip_end]


class _Probe:
    def __init__(self, wire, control):
        self.wire, self.control = wire, control

    def send(self, frame):
        self.wire.sendall(struct.pack("!I", len(frame)) + frame)

    def read_exact(self, count):
        data = b""
        while len(data) < count:
            chunk = self.wire.recv(count - len(data))
            assert chunk, "QEMU closed the Ethernet wire"
            data += chunk
        return data

    def receive(self):
        (size,) = struct.unpack("!I", self.read_exact(4))
        assert 14 <= size <= 65536
        return self.read_exact(size)

    def service_reply(self):
        for _ in range(32):
            frame = self.receive()
            assert frame[12:14] == b"\x08\x00"
            assert frame[23] == 6
            if frame[26:30] == socket.inet_aton(qemu.SERVICE_ADDRESS):
                return frame
            assert frame[26:30] in (socket.inet_aton("1.1.1.1"), socket.inet_aton("10.0.2.2"))
            assert frame[_l4_start(frame) + 13] & 0x16 == 0x14  # RST|ACK, no SYN
        pytest.fail("no guestfwd reply among 32 frames")

    def connect(self, port):
        """Complete the guest's TCP handshake; return the server's next sequence."""
        self.send(_tcp(qemu.SERVICE_ADDRESS, port))
        synack = self.service_reply()
        assert synack[26:30] == socket.inet_aton(qemu.SERVICE_ADDRESS)
        start = _l4_start(synack)
        sport, dport, sequence, ack = struct.unpack("!HHII", synack[start : start + 12])
        assert (sport, dport, ack, synack[start + 13] & 0x12) == (port, 40000, 101, 0x12)
        self.send(_tcp(qemu.SERVICE_ADDRESS, port, seq=101, ack=sequence + 1, flags=0x10))
        return sequence + 1

    def arp(self):
        arp = struct.pack("!HHBBH", 1, 0x800, 6, 4, 1)
        arp += _GUEST_MAC + _SOURCE + bytes(6) + socket.inet_aton("10.0.2.2")
        self.send(bytes([255]) * 6 + _GUEST_MAC + b"\x08\x06" + arp)
        assert self.receive()[12:14] == b"\x08\x06"


@contextmanager
def _slirp_probe(devices):
    with tempfile.TemporaryDirectory(prefix="lumen-wire-", dir="/tmp") as short:
        wire, peer = socket.socketpair()
        with wire, peer:
            wire.settimeout(60)
            args = qemu.user_network_probe_args(peer.fileno(), Path(short) / "qmp", devices)
            proc = subprocess.Popen(
                args, pass_fds=(peer.fileno(),), stdout=subprocess.PIPE, stderr=subprocess.PIPE
            )
            peer.close()
            control = None
            try:
                control = QmpClient.connect(Path(short) / "qmp", timeout=60)
                yield _Probe(wire, control)
            finally:
                if control:
                    control.close()
                proc.kill()
                _, errors = proc.communicate(timeout=60)
            # An error in the body propagates past this assertion, preserving
            # the actual failure instead of masking it with QEMU's stderr.
            assert errors in (b"", b"qemu-system-x86_64: warning: hub 0 with no nics\n"), (
                errors.decode(errors="replace")
            )


def test_restricted_slirp_drops_outbound_tcp_and_udp(packages):
    """UDP is dropped; TCP outside guestfwd is reset.

    Check installed QEMU: Homebrew locally, Ubuntu QEMU 8.2.2 in CI.
    """
    # Verified libslirp sources, v4.7.0 and master:
    # src/udp.c:udp_input (:146-168 in both) handles DHCP/TFTP before dropping
    # restricted UDP, before allocating/forwarding any DNS proxy socket.
    # src/tcp_input.c (:403-419 in v4.7.0; :383-399 in master) rejects
    # destinations outside guestfwd_list with RST under restrict=on.
    # https://gitlab.freedesktop.org/slirp/libslirp/-/blob/v4.7.0/src/udp.c#L146
    # https://gitlab.freedesktop.org/slirp/libslirp/-/blob/9741541ae91b1b4aa34288045b7e8e5039c8aa42/src/udp.c#L146
    # https://gitlab.freedesktop.org/slirp/libslirp/-/blob/v4.7.0/src/tcp_input.c#L403
    # https://gitlab.freedesktop.org/slirp/libslirp/-/blob/9741541ae91b1b4aa34288045b7e8e5039c8aa42/src/tcp_input.c#L383
    with NetFixture(**packages) as net, _slirp_probe(Devices(services=net.services)) as probe:
        probe.arp()
        # DHCP is the permitted UDP control: a real offer proves that
        # the rejected DNS probes below are not just malformed IP/UDP.
        bootp = struct.pack("!BBBBIHH", 1, 1, 6, 0, 0x12345678, 0, 0x8000)
        bootp += bytes(16) + _GUEST_MAC + bytes(10 + 64 + 128)
        bootp += bytes.fromhex("638253633501013703010306ff")
        discover = struct.pack("!HHHH", 68, 67, 8 + len(bootp), 0) + bootp
        probe.send(_ip_packet("255.255.255.255", 17, discover, bytes(4)))
        offer = probe.receive()
        assert offer[23] == 17
        udp_start = _l4_start(offer)
        assert struct.unpack("!HH", offer[udp_start : udp_start + 4]) == (67, 68)
        reply = offer[udp_start + 8 :]
        assert reply[0] == 2 and reply[4:8] == bytes.fromhex("12345678")
        assert reply[16:20] == _SOURCE  # default DHCP lease, 10.0.2.15
        assert reply[236:240] == bytes.fromhex("63825363")
        assert b"\x35\x01\x02" in reply[240:]  # DHCP OFFER

        probe.send(_tcp("1.1.1.1", 443, source_port=41000))
        probe.send(_tcp("10.0.2.2", 443, source_port=41001))
        # UDP DNS to the proxy, plus UDP to an otherwise allowed
        # guestfwd address. Zero UDP checksum is valid for IPv4.
        query = bytes.fromhex("123401000001000000000000") + b"\x05lumen\x04test\0\0\1\0\1"
        udp = struct.pack("!HHHH", 41002, 53, 8 + len(query), 0) + query
        probe.send(_ip_packet("10.0.2.3", 17, udp))
        probe.send(_ip_packet(qemu.SERVICE_ADDRESS, 17, udp))

        sequence = probe.connect(SERVICE_PORTS["echo"])
        message = b"slirp forwarded this connection"
        probe.send(
            _tcp(
                qemu.SERVICE_ADDRESS,
                SERVICE_PORTS["echo"],
                seq=101,
                ack=sequence,
                flags=0x18,
                payload=message,
            )
        )
        echoed = b""
        while len(echoed) < len(message):
            frame = probe.service_reply()
            assert frame[23] == 6 and frame[26:30] == socket.inet_aton(qemu.SERVICE_ADDRESS)
            echoed += _tcp_payload(frame)
        assert echoed == message
        table = probe.control.command("human-monitor-command", **{"command-line": "info usernet"})
        assert "40000" in table  # live guestfwd positive control
        assert "41000" not in table and "41001" not in table and "41002" not in table
        assert "1.1.1.1" not in table and "UDP" not in table


def test_relay_failure_logs_without_sending_diagnostics_to_guest(packages, tmp_path):
    artifacts = tmp_path / "artifacts"
    with NetFixture(**packages, artifacts=artifacts) as net:
        # A real guestfwd command will fail to connect to this removed name.
        net.socket("echo").unlink()
        with _slirp_probe(Devices(services=net.services)) as probe:
            probe.arp()
            probe.connect(SERVICE_PORTS["echo"])
            for _ in range(32):
                frame = probe.service_reply()
                assert _tcp_payload(frame) == b"", "relay diagnostics leaked into guest TCP"
                if frame[_l4_start(frame) + 13] & 0x05:  # FIN or RST
                    break
            else:
                pytest.fail("relay did not close the failed connection")
        relay_log = net.relay_log.read_text()
        assert "FileNotFoundError" in relay_log
        assert re.search(
            r"\d{4}-\d\d-\d\d \d\d:\d\d:\d\d,\d{3} \d+ ERROR guestfwd relay failed", relay_log
        )
    assert "FileNotFoundError" in (artifacts / "relay.log").read_text()
    assert (artifacts / "services.log").exists()


def test_unrestricted_requires_a_network_device():
    with pytest.raises(ValueError, match="requires a network device"):
        Devices(restrict=False)
    machine = Machine(Path("x.iso"), Path("/tmp"), devices=Devices(net=True, restrict=False))
    args = qemu_args(machine.options(Path("v.fd")))
    assert "restrict=off,ipv6=off" in args[args.index("-netdev") + 1]


def test_probe_uses_shared_netdev_without_host_forwards():
    devices = Devices(forward=(22,), services=((80, Path("/tmp/service.sock")),))
    options = Machine(Path("x.iso"), Path("/tmp"), devices=devices).options(Path("v.fd"))
    options.net_forwards = {22: 43210}
    normal = qemu.device_args(options)
    probe = qemu.user_network_probe_args(5, Path("/tmp/probe.qmp"), devices)
    normal_net = normal[normal.index("-netdev") + 1]
    assert ",hostfwd=tcp:127.0.0.1:43210-:22" in normal_net
    assert probe[probe.index("-netdev") + 1] == normal_net.replace(
        ",hostfwd=tcp:127.0.0.1:43210-:22", ""
    )
    assert "hostfwd" not in probe[probe.index("-netdev") + 1]


@pytest.mark.parametrize("path", ["/" + "a" * 102, "/" + "é" * 51])
def test_service_socket_paths_accept_103_bytes(path):
    assert len(os.fsencode(path)) == 103
    devices = Devices(services=((80, path),))
    assert devices.services[0] == qemu.Service(80, Path(path))


@pytest.mark.parametrize("services", [(80,), ((80,),), ((80, "/a", "extra"),), (None,), ("ab",)])
def test_services_require_port_path_pairs(services):
    with pytest.raises(ValueError, match=r"services: expected \(port, path\) pairs"):
        Devices(services=services)


def test_qemu_services_require_enabled_networking():
    services = ((80, Path("/tmp/service.sock")),)
    with pytest.raises(ValueError, match="net_services requires net_forwards"):
        opts(net_services=services)
    assert "guestfwd=tcp:" in qemu._user_netdev(opts(net_forwards={}, net_services=services))
