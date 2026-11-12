"""QEMU command-line construction."""

from pathlib import Path

from lumen_harness.qemu import Machine, QemuOptions, qemu_args


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


def test_machine_appends_extra_args_last(tmp_path):
    m = Machine(Path("x.iso"), tmp_path, extra_args=["-smp", "2,maxcpus=4"])
    args = qemu_args(m.options(Path("v.fd")))
    assert args[-2:] == ["-smp", "2,maxcpus=4"]
    assert args[args.index("-qmp") + 1] == f"unix:{m.qmp_path},server=on,wait=off"
    plain = qemu_args(Machine(Path("x.iso"), tmp_path).options(Path("v.fd")))
    assert "-smp" not in plain
