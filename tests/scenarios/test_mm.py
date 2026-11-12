"""Memory management: the frame allocator, the kernel's page tables and the
heap.

The allocators' and the mapper's correctness are checked in-kernel
(`lumen_kernel::mm::{frame,paging,heap}::*`, run by test_ktest) and on the
host (`cargo test -p lumen-mm -p lumen-heap`); this checks what boot and
kmon report, that W^X is enforced (writing to the kernel's code and
executing its data both fault with a page-table walk in the crash report),
and that heap failures end in a crash report showing the heap.
"""

import re

from lumen_harness.crash import backtrace, page_walk

BOOT_TIMEOUT = 60
MIB = 1024 * 1024
HEAP_BASE = 0xFFFF_C000_0000_0000
# The heap's one-line summary (kmon `heap`, crash reports).
HEAP_LINE = re.compile(
    r"^heap: (?P<mapped>\d+) bytes mapped at (?P<base>0x[0-9a-f]+), (?P<used>\d+) used in "
    r"(?P<live>\d+) allocations, (?P<free>\d+) free in (?P<blocks>\d+) blocks \(largest (?P<largest>\d+)\)"
    r"(?:; last growth failed: (?P<error>.+))?$",
    re.MULTILINE,
)


def heap_line(text: str) -> dict[str, str]:
    m = HEAP_LINE.search(text)
    assert m, f"no heap line in:\n{text}"
    return m.groupdict()


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


def test_write_to_text_crashes_cleanly(crash_report):
    report = crash_report("wtext")
    # Error code 0x3: present page, write access.
    assert "exception: #PF page fault (vector 14, error code 0x3)" in report, report
    assert "page fault: write of protection violation (kernel mode)" in report, report
    rip = r"^rip: 0x[0-9a-f]{16} <lumen_kernel::kmon::crash_write_text"
    assert re.search(rip, report, re.MULTILINE), report
    walk = page_walk(report)
    cr2 = re.search(r"faulting address \(cr2\): (0x[0-9a-f]{16})", report)
    assert cr2 and walk.virt == int(cr2.group(1), 16), report
    # The kernel is in the top 2 GiB: L4 entry 511, L3 entry 510; the text is
    # mapped with 4 KiB pages (2 MiB once it is big and aligned enough).
    assert [(lvl, idx) for lvl, idx, _ in walk.steps[:2]] == [(4, 511), (3, 510)], report
    end = r"(4 KiB|2 MiB) page at phys 0x[0-9a-f]+: read-only, executable"
    assert re.fullmatch(end, walk.end), report


def test_nx_data_crashes_cleanly(crash_report):
    report = crash_report("nx")
    # Error code 0x11: present page, instruction fetch.
    assert "exception: #PF page fault (vector 14, error code 0x11)" in report, report
    assert "page fault: instruction fetch of protection violation (kernel mode)" in report, report
    # The fetch faults at the target itself: rip == cr2 == the walked address.
    cr2 = re.search(r"faulting address \(cr2\): (0x[0-9a-f]{16})", report)
    rip = re.search(r"^rip: (0x[0-9a-f]{16})", report, re.MULTILINE)
    assert cr2 and rip and cr2.group(1) == rip.group(1), report
    walk = page_walk(report)
    assert walk.virt == int(cr2.group(1), 16), report
    end = r"(4 KiB|2 MiB) page at phys 0x[0-9a-f]+: writable, no-execute"
    assert re.fullmatch(end, walk.end), report
    # rip is in .data, which has no function symbols; the frame-pointer walk
    # still reaches the monitor (the faulting `call` set up no frame, so its
    # caller's caller is the first named frame).
    frames = backtrace(report)
    assert any("kmon::run" in f for f in frames) and frames[-1].startswith("_start"), report


def test_kmon_heap_reports_usage(boot):
    m = boot("qemutest")
    m.expect(r"kmon> ", BOOT_TIMEOUT)
    m.send_line("heap")
    line = m.expect(r"heap: .*\n")
    heap = heap_line(line.group(0).rstrip("\n"))
    assert int(heap["base"], 16) == HEAP_BASE, heap
    mapped, used, free = int(heap["mapped"]), int(heap["used"]), int(heap["free"])
    assert used + free == mapped and mapped % 4096 == 0, heap
    assert int(heap["largest"]) <= free and heap["error"] is None, heap


def test_out_of_memory_report_shows_the_heap(crash_report):
    # kmon asks for 2 TiB (more than the heap window) and treats the null as
    # an infallible allocation would: alloc's error handler panics.
    report = crash_report("oom")
    assert f"panic: memory allocation of {1 << 41} bytes failed" in report, report
    heap = heap_line(report)
    assert heap["error"] == "heap window full", heap
    # Serial only, after the trace events.
    assert report.index("heap: ") > report.index("recent trace events"), report


def test_double_free_is_refused(crash_report):
    report = crash_report("dfree")
    bad_free = (
        r"^panic: heap: bad free of 0xffffc[0-9a-f]{11} \(64 bytes, align 16\): "
        r"block overlaps free memory \(double free or wrong layout\)$"
    )
    assert re.search(bad_free, report, re.MULTILINE), report
    assert any("kmon::crash_double_free" in f for f in backtrace(report)), report
    # The first free went through and the second changed nothing: only the
    # 32-byte block kmon keeps is in use, and the free list is intact (one
    # block: everything above it).
    heap = heap_line(report)
    assert (heap["used"], heap["live"], heap["blocks"]) == ("32", "1", "1"), heap
    assert int(heap["largest"]) == int(heap["free"]) == int(heap["mapped"]) - 32, heap
