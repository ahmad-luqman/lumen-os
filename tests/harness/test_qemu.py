"""QEMU and gdb command-line construction, the harness's environment overrides,
and what each run records about its environment."""

import os
import re
import subprocess
import time
from pathlib import Path

import pytest
from lumen_harness import qemu
from lumen_harness.__main__ import gdb_binary, gdb_command
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
    # 0 or less would time out at once; nan, inf (and 1e999, which parses to
    # inf) would never time out; a typo would fail deep inside expect().
    monkeypatch.setenv("LUMEN_TIMEOUT_SCALE", raw)
    with pytest.raises(ValueError, match=re.escape(f"LUMEN_TIMEOUT_SCALE={raw!r}: expected")):
        timeout_scale()


def test_a_bad_timeout_scale_fails_before_qemu_starts(tmp_path, monkeypatch):
    monkeypatch.setenv("LUMEN_TIMEOUT_SCALE", "abc")
    with pytest.raises(ValueError, match="LUMEN_TIMEOUT_SCALE='abc'"):
        Machine(Path("x.iso"), tmp_path / "run")
    assert not (tmp_path / "run").exists()
    monkeypatch.setenv("LUMEN_TIMEOUT_SCALE", "2.5")  # a good one is read once, here
    m = Machine(Path("x.iso"), tmp_path / "run")
    monkeypatch.setenv("LUMEN_TIMEOUT_SCALE", "abc")
    assert m.timeout_scale == 2.5


def sleeping_machine(tmp_path: Path, monkeypatch) -> Machine:
    """A started Machine whose "QEMU" prints nothing and doesn't exit. `exec`, so
    that close() kills the sleep itself, not just the shell around it."""
    exe = fake_qemu(
        tmp_path,
        'case "$1" in --version) echo "QEMU emulator version 0.0 (fake)" ;; '
        "*) exec sleep 30 ;; esac",
    )
    vars_template = tmp_path / "VARS.fd"
    vars_template.write_bytes(b"vars")
    monkeypatch.setenv("LUMEN_QEMU", str(exe))
    monkeypatch.setenv("LUMEN_OVMF_VARS", str(vars_template))
    return Machine(Path("x.iso"), tmp_path / "run").start()


SCALED_WAITS = [
    ("", 0.2, "0.2s"),
    ("2.5", 0.5, "0.5s (0.2s scaled by LUMEN_TIMEOUT_SCALE=2.5)"),
]


@pytest.mark.parametrize(("scale", "wait", "said"), SCALED_WAITS)
def test_expect_timeout_shows_the_scaled_wait(tmp_path, monkeypatch, scale, wait, said):
    monkeypatch.setenv("LUMEN_TIMEOUT_SCALE", scale)
    with sleeping_machine(tmp_path, monkeypatch) as m:
        start = time.monotonic()
        with pytest.raises(ExpectTimeout) as e:
            m.expect("never", 0.2)
        assert time.monotonic() - start >= wait  # it waited that long, too
    assert str(e.value).startswith(f"timeout after {said} waiting for 'never'\n")


@pytest.mark.parametrize(("scale", "wait", "said"), SCALED_WAITS)
def test_wait_exit_timeout_shows_the_scaled_wait(tmp_path, monkeypatch, scale, wait, said):
    monkeypatch.setenv("LUMEN_TIMEOUT_SCALE", scale)
    with sleeping_machine(tmp_path, monkeypatch) as m:
        start = time.monotonic()
        with pytest.raises(ExpectTimeout) as e:
            m.wait_exit(0.2)
        assert time.monotonic() - start >= wait
    assert str(e.value) == f"QEMU still running after {said}"


def test_empty_gdb_means_unset(monkeypatch):
    monkeypatch.setenv("LUMEN_GDB", "")
    assert gdb_binary() == "gdb"
    monkeypatch.setenv("LUMEN_GDB", "x86_64-elf-gdb")  # a set value is still used
    assert gdb_binary() == "x86_64-elf-gdb"
    # `make debug` goes through it (and test_devtools' skip check calls it).
    assert gdb_command(Path("kernel"), batch=False)[0] == "x86_64-elf-gdb"


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
