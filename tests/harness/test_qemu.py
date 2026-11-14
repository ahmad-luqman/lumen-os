"""QEMU and gdb command-line construction, the harness's environment overrides,
and what each run records about its environment."""

import math
import os
import re
import subprocess
import sys
import threading
from pathlib import Path
from typing import Self

import pytest
from lumen_harness import REPO_ROOT, qemu
from lumen_harness.__main__ import find_gdb, gdb_binary, gdb_command
from lumen_harness.qemu import (
    ExpectTimeout,
    Machine,
    QemuOptions,
    ovmf_vars_template,
    ovmf_version,
    qemu_args,
    qemu_version,
    timeout_scale,
)


def opts(**kw):
    return QemuOptions(iso=Path("x.iso"), vars_file=Path("v.fd"), accel="tcg", **kw)


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
    # Only the reboot scenario lets a reset boot the machine again.
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


def test_machine_without_serial_has_no_serial_port(tmp_path):
    # `-serial none` (not just no output): q35 then creates no COM1 at all.
    args = qemu_args(Machine(Path("x.iso"), tmp_path, serial=False).options(Path("v.fd")))
    assert args[args.index("-serial") + 1] == "none" and args.count("-serial") == 1
    plain = qemu_args(Machine(Path("x.iso"), tmp_path).options(Path("v.fd")))
    assert plain[plain.index("-serial") + 1] == "stdio"


def test_live_machines_never_share_a_qmp_socket(tmp_path):
    # Parallel workers are kept apart by the pid in the name; machines in one
    # process (a test that boots twice) by a per-process counter.
    machines = [Machine(Path("x.iso"), tmp_path) for _ in range(3)]
    paths = {m.qmp_path for m in machines}
    assert len(paths) == 3
    assert all(f"-{os.getpid()}-" in p.name for p in paths)


def fake_qemu(tmp_path: Path, script: str, name: str = "fake-qemu") -> Path:
    exe = tmp_path / name
    exe.write_text("#!/bin/sh\n" + script + "\n")
    exe.chmod(0o755)
    return exe


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
