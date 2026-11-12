"""Memory management at boot: the frame allocator is built and reports itself.

The allocator's correctness is proven in-kernel
(`lumen_kernel::mm::frame::never_returns_reserved`, run by test_ktest) and on
the host (`cargo test -p lumen-mm`); this checks what boot reports.
"""

import re
from pathlib import Path

from lumen_harness.qemu import QemuOptions

BOOT_TIMEOUT = 60
MIB = 1024 * 1024


def test_frame_allocator_summary_logged(boot):
    m = boot("qemutest")
    m.expect(r"INFO  mem: \d+ regions, \d+ MiB usable", BOOT_TIMEOUT)
    m.expect(r"INFO  boot: boot complete")
    found = re.search(r"INFO  mem: frames: (\d+) usable, (\d+) reserved\n", m.output)
    assert found, "no frame allocator summary before boot complete"
    usable, reserved = (int(n) for n in found.groups())
    ram = int(QemuOptions(Path(), Path()).memory.removesuffix("M")) * MIB  # the default size
    # Most of the guest's RAM is allocatable; the rest (kernel, Limine's
    # structures, firmware, the bitmaps) is reserved.
    assert ram // 2 < usable * 4096 < ram, f"{usable} usable frames for {ram // MIB} MiB of RAM"
    assert reserved > 0
