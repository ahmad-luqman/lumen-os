"""QEMU command-line construction."""

from pathlib import Path

from lumen_harness.qemu import QemuOptions, qemu_args


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
