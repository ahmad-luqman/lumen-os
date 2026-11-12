"""The kernel trace ring, seen through the kmon `trace` command.

The ring itself is tested on the host (`cargo test -p lumen-trace`) and
in-kernel (`lumen_kernel::trace::*`); this checks what a person sees.
"""

import re

BOOT_TIMEOUT = 60
PHASES = ["arch-init", "logger", "symbols", "memory", "framebuffer", "complete"]


def test_trace_command_shows_boot_phases_and_logs(boot):
    m = boot("qemutest")
    m.expect(r"kmon> ", BOOT_TIMEOUT)
    m.send_line("trace 4096")
    summary = m.expect(r"trace: (\d+) events recorded, (\d+) dropped\n", 20)
    out = m.output[: summary.start()]
    phases = re.findall(r"boot-phase +(\S+)", out)
    assert phases == PHASES, phases
    assert re.search(r"log +INFO boot\n", out), "log lines are traced with level and tag"
    tsc = [int(t) for t in re.findall(r"tsc=(\d+)", out)]
    assert tsc == sorted(tsc), "timestamps go backwards"
    assert int(summary.group(1)) > len(PHASES)
    assert summary.group(2) == "0"


def test_exception_appears_in_trace(boot):
    m = boot("qemutest")
    m.expect(r"kmon> ", BOOT_TIMEOUT)
    m.send_line("int3")
    m.expect(r"breakpoints taken: 1")
    m.send_line("trace 3")
    m.expect(r"exception +vector=3 rip=0x[0-9a-f]+ error=0x0\n", 20)
