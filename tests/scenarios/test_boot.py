"""First boot: Limine loads the kernel, which reaches the logger and the monitor."""

import re

from lumen_harness.qemu import EXIT_SUCCESS

BOOT_TIMEOUT = 60


def test_boot_reaches_logger(boot):
    m = boot("qemutest")
    m.expect(r"INFO  boot: Lumen \d+\.\d+\.\d+ \(x86_64\) booting", BOOT_TIMEOUT)
    m.expect(r"INFO  boot: cmdline: 'lumen.qemu_exit'")
    m.expect(r"INFO  boot: symbols: \d+ entries")
    m.expect(r"INFO  mem: \d+ regions, \d+ MiB usable")
    m.expect(r"INFO  fb: framebuffer 1280x800 ready")
    m.expect(r"INFO  boot: boot complete")
    out = m.output
    assert "WARN" not in out, "boot logged a warning"
    assert "ERROR" not in out, "boot logged an error"


def test_log_lines_are_structured(boot):
    """Every kernel log line is `LEVEL tag: message` (hard rule 8)."""
    m = boot("qemutest")
    m.expect(r"kmon> ", BOOT_TIMEOUT)
    start = m.output.index("INFO  boot: Lumen")
    kernel_lines = m.output[start:].splitlines()[:-1]  # drop the prompt
    line_rx = re.compile(r"^(ERROR|WARN |INFO |DEBUG|TRACE) [a-z0-9_:]+: \S")
    bad = [line for line in kernel_lines if not line_rx.match(line)]
    assert not bad, f"unstructured kernel output: {bad}"


def test_serial_input_round_trip(boot):
    """The harness can type into the kernel and read the reply."""
    m = boot("qemutest")
    m.expect(r"kmon> ", BOOT_TIMEOUT)
    m.send_line("echo hello-from-the-harness")
    m.expect(r"\nhello-from-the-harness\n", 10)
    m.expect(r"kmon> ", 10)
    m.send_line("frobnicate")
    m.expect(r"unknown command: frobnicate", 10)
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS


def test_breakpoint_is_survivable(boot):
    """#BP returns to the interrupted code; the stub restores all registers."""
    m = boot("qemutest")
    m.expect(r"kmon> ", BOOT_TIMEOUT)
    m.send_line("int3")
    m.expect(r"breakpoints taken: 1\n", 10)
    m.send_line("int3")
    m.expect(r"breakpoints taken: 2\n", 10)
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS
