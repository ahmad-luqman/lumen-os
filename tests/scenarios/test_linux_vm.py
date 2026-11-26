"""M8 ring-3 scenarios of cross-CPU TLB shootdowns (ARCHITECTURE section
15, "M8's tests"), each check an `ok` line of `tools/linux-m8/m8_vm`.
"""

import re

from lumen_harness.qemu import EXIT_SUCCESS, Devices
from lumen_harness.rootfs import copy_rootfs
from lumen_harness.shell import run

VM = "/usr/local/bin/m8_vm"


def test_cross_cpu_munmap_and_mprotect_shootdown(boot, tmp_path):
    """C11: cross-CPU munmap, mprotect, threaded fork write-protect and CoW split shootdowns."""
    m = boot(
        "qemutest",
        extra=["-smp", "4"],
        devices=Devices(blk_image=copy_rootfs(tmp_path), rng=True),
    )
    m.expect(r"lumen> ", 60)
    assert "INFO  smp: 4 CPUs online\n" in m.output, m.output
    # First run warms multi-thread stack-window page tables; the second identical
    # run must return every frame.
    for attempt in range(2):
        out = run(m, f"linux {VM} shootdown", 180)
        assert re.findall(r"^ok (.+)$", out, re.MULTILINE) == [
            "munmap-shootdown",
            "mprotect-shootdown",
            "fork-write-protect-shootdown",
            "cow-split-shootdown",
        ], out
        assert "FAIL" not in out, out
        if attempt == 0:
            assert f"linux: {VM} exit=0 " in out, out
        else:
            assert re.search(rf"linux: {VM} exit=0 leaked_frames=0\b", out), out
    m.send_line("exit")
    assert m.wait_exit(10) == EXIT_SUCCESS
