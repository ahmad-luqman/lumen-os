"""Memory management: the frame allocator and the kernel's page tables.

The allocator's and the mapper's correctness are checked in-kernel
(`lumen_kernel::mm::{frame,paging}::*`, run by test_ktest) and on the host
(`cargo test -p lumen-mm`); this checks what boot reports, and that W^X is
enforced: writing to the kernel's code and executing its data both fault
with a page-table walk in the crash report.
"""

import re

BOOT_TIMEOUT = 60
MIB = 1024 * 1024


def test_frame_allocator_summary_logged(boot):
    m = boot("qemutest")
    regions = m.expect(r"INFO  mem: \d+ regions, (\d+) MiB usable\n", BOOT_TIMEOUT)
    frames = m.expect(r"INFO  mem: frames: (\d+) usable, (\d+) reserved\n")
    m.expect(r"INFO  boot: boot complete")
    usable_mib = int(regions.group(1))
    usable, reserved = int(frames.group(1)), int(frames.group(2))
    # The allocator hands out the map's usable memory minus its own bitmaps
    # (64 KiB per GiB) and a few reserved frames: well under 2 MiB here.
    # (The map's figure is rounded down to whole MiB.)
    assert (usable_mib - 2) * MIB <= usable * 4096 < (usable_mib + 1) * MIB, (usable, usable_mib)
    assert reserved > 0


def test_kernel_page_tables_active(boot):
    m = boot("qemutest")
    tables = m.expect(
        r"INFO  mem: paging: kernel page tables active \((\d+) tables, root 0x[0-9a-f]+\)\n",
        BOOT_TIMEOUT,
    )
    m.expect(r"INFO  boot: boot complete")
    assert int(tables.group(1)) > 1


def walk(report: str) -> tuple[list[tuple[int, int]], str]:
    """The page walk in a crash report: [(level, index)] and its last line."""
    start = report.index("  page walk (cr3 ")
    lines = report[start:].splitlines()[1:]
    steps = []
    for line in lines:
        step = re.fullmatch(r"    L(\d)\[(\d{3})\] 0x[0-9a-f]{16}", line)
        if not step:
            return steps, line.strip()
        steps.append((int(step.group(1)), int(step.group(2))))
    raise AssertionError(f"page walk has no end line:\n{report}")


def test_write_to_text_crashes_cleanly(crash_report):
    report = crash_report("wtext")
    # Error code 0x3: present page, write access.
    assert "exception: #PF page fault (vector 14, error code 0x3)" in report, report
    assert "page fault: write of protection violation (kernel mode)" in report, report
    assert re.search(
        r"^rip: 0x[0-9a-f]{16} <lumen_kernel::kmon::crash_write_text", report, re.MULTILINE
    ), report
    steps, end = walk(report)
    # The kernel is in the top 2 GiB: L4 entry 511, L3 entry 510.
    assert [lvl for lvl, _ in steps] == [4, 3, 2, 1] and steps[:2] == [(4, 511), (3, 510)], report
    assert re.fullmatch(r"-> 4 KiB page at phys 0x[0-9a-f]+: read-only, executable", end), report


def test_nx_data_crashes_cleanly(crash_report):
    report = crash_report("nx")
    # Error code 0x11: present page, instruction fetch.
    assert "exception: #PF page fault (vector 14, error code 0x11)" in report, report
    assert "page fault: instruction fetch of protection violation (kernel mode)" in report, report
    # The fetch faults at the target itself: rip == cr2.
    cr2 = re.search(r"faulting address \(cr2\): (0x[0-9a-f]{16})", report)
    rip = re.search(r"^rip: (0x[0-9a-f]{16})", report, re.MULTILINE)
    assert cr2 and rip and cr2.group(1) == rip.group(1), report
    steps, end = walk(report)
    assert [lvl for lvl, _ in steps] == [4, 3, 2, 1], report
    assert re.fullmatch(r"-> 4 KiB page at phys 0x[0-9a-f]+: writable, no-execute", end), report
    # rip is in .data, which has no function symbols; the frame-pointer walk
    # still reaches the monitor (the faulting `call` set up no frame, so its
    # caller's caller is the first named frame).
    frames = re.findall(r"^  #\d+\s+0x[0-9a-f]{16} <(.+)>$", report, re.MULTILINE)
    assert any("kmon::run" in f for f in frames) and frames[-1].startswith("_start"), report
