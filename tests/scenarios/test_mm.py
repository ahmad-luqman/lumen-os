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
# The heap's one-line summary (kmon `heap`, crash reports), written by
# `Snapshot::write` in kernel/src/mm/heap.rs. This matches only the plain
# form with an intact free list: a crash with the heap's lock held labels it
# "heap (lock held; may be mid-update)", and a failed walk replaces the
# "in N blocks" part.
HEAP_LINE = re.compile(
    r"^heap: (?P<mapped>\d+) bytes mapped at (?P<base>0x[0-9a-f]+), (?P<used>\d+) used in "
    r"(?P<live>\d+) allocations, (?P<free>\d+) free in (?P<blocks>\d+) blocks \(largest (?P<largest>\d+)\)"
    r"(?:; last growth failed for (?P<fail_size>\d+) bytes \(align (?P<fail_align>\d+)\): (?P<error>.+))?$",
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


HEAP_NUMBERS = ("mapped", "base", "used", "live", "free", "blocks", "largest")


def heap_numbers(text: str) -> dict[str, int]:
    """The heap summary's numbers in `text` (not the optional failure fields).
    A summary only matches with an intact free list whose bytes add up."""
    heap = heap_line(text)
    return {k: int(heap[k], 0) for k in HEAP_NUMBERS}


def kmon_heap(m) -> dict[str, int]:
    """Run kmon's `heap` at its prompt; the summary's numbers, back at the prompt."""
    m.send_line("heap")
    heap = heap_numbers(m.expect(r"heap: .*\n").group(0))
    m.expect(r"kmon> ")
    return heap


def crash_after_kmon_heap(crash_report, kind: str) -> tuple[dict[str, int], str]:
    """kmon's heap summary at the prompt, then the report of `crash <kind>`."""
    before: dict[str, int] = {}
    report = crash_report(kind, at_prompt=lambda m: before.update(kmon_heap(m)))
    return before, report


def test_kmon_heap_reports_usage(boot):
    m = boot("qemutest")
    m.expect(r"kmon> ", BOOT_TIMEOUT)
    heap = kmon_heap(m)
    # What boot keeps (`acpi::Info`, for one) is in use, a handful of
    # allocations; the rest of what the heap mapped is free.
    # (`discovery_keeps_nothing_it_does_not_return` checks that ACPI
    # discovery frees what it doesn't keep.)
    assert heap["base"] == HEAP_BASE, heap
    assert 0 < heap["live"] <= 16, f"{heap}: nothing kept at boot (did acpi::init fail?), or a leak"
    assert 0 < heap["largest"] <= heap["free"] < heap["mapped"], heap
    # Looking doesn't allocate: the summary is the same the second time.
    assert kmon_heap(m) == heap


def test_out_of_memory_report_shows_the_heap(crash_report):
    # kmon asks for 2 TiB (more than the heap window) and treats the null as
    # an infallible allocation would: alloc's error handler panics.
    report = crash_report("oom")
    assert f"panic: memory allocation of {1 << 41} bytes failed" in report, report
    heap = heap_line(report)
    assert (heap["fail_size"], heap["fail_align"], heap["error"]) == (
        str(1 << 41),
        "1",
        "heap window full",
    ), heap
    # After the trace events (the serial-only part of the report).
    assert report.index("heap: ") > report.index("recent trace events"), report


def test_double_free_is_refused(crash_report):
    before, report = crash_after_kmon_heap(crash_report, "dfree")
    bad_free = (
        r"^panic: heap: bad free of 0xffffc[0-9a-f]{11} \(64 bytes, align 16\): "
        r"block overlaps free memory \(double free or wrong layout\)$"
    )
    assert re.search(bad_free, report, re.MULTILINE), report
    assert any("kmon::crash_double_free" in f for f in backtrace(report)), report
    # The block was freed after its lower neighbour, so the second free is
    # caught by its overlap with the merged free block below. It changed
    # nothing: compared with the prompt, exactly the 128-byte block kmon
    # keeps is in use, and the free list is intact (the summary only matches
    # when the walk finds its bytes add up). Where the block landed, and so
    # the free blocks' count and largest size, depends on boot's layout.
    heap = heap_numbers(report)
    change = {k: heap[k] - before[k] for k in ("mapped", "base", "used", "live", "free")}
    assert change == {"mapped": 0, "base": 0, "used": 128, "live": 1, "free": -128}, (before, heap)


def test_corrupt_free_list_is_a_crash_not_a_hang(crash_report):
    # kmon links a freed block's free-list node back to itself and then
    # allocates past it. The walk must panic (with the heap's lock held),
    # and the report must still show the heap, naming the corruption.
    before, report = crash_after_kmon_heap(crash_report, "heapcorrupt")
    corrupt = re.search(
        r"^panic: heap free list corrupt at offset (0x[0-9a-f]+) "
        r"\(size (0x[0-9a-f]+), next (0x[0-9a-f]+)\)$",
        report,
        re.MULTILINE,
    )
    assert corrupt and corrupt[1] == corrupt[3], report  # the node links to itself
    offset, size = int(corrupt[1], 16), int(corrupt[2], 16)
    assert offset % 16 == 0 and offset < before["mapped"], (before, report)
    # The freed 64-byte block, merged with whatever free space it landed
    # next to (first fit depends on boot's layout).
    assert size >= 0x40 and size % 16 == 0, report
    # One more 64-byte block in use than at the prompt.
    locked = (
        r"^heap \(lock held; may be mid-update\): \d+ bytes mapped at 0xffffc00000000000, "
        rf"{before['used'] + 64} used in {before['live'] + 1} allocations"
    )
    assert re.search(locked, report, re.MULTILINE), (before, report)
    assert f"(free list broken: block at {corrupt[1]} is out of order or overlaps)" in report, (
        report
    )


# The crash reporter's line for an overflow (kernel/src/crash.rs,
# write_guard_hit): cr2 in the guard page below a stack, with rsp at it.
OVERFLOW = re.compile(
    r"^kernel stack overflow \(guard page hit\): cr2 (0x[0-9a-f]{16}) is in the guard page below "
    r"the kernel main stack \[(0x[0-9a-f]+), (0x[0-9a-f]+)\)$",
    re.MULTILINE,
)


def assert_main_stack_overflow(report: str, recursing: str) -> None:
    """A #DF whose overflow line puts cr2 in the page right below the 128 KiB
    main stack, faulting in `recursing`, and a complete report."""
    assert "exception: #DF double fault (vector 8, error code 0x0)" in report, report
    overflow = OVERFLOW.search(report)
    assert overflow, report
    cr2, bottom, top = (int(g, 16) for g in overflow.groups())
    assert bottom - 4096 <= cr2 < bottom and top - bottom == 128 * 1024, report
    # What makes it an overflow: the interrupted rsp is at the guard page.
    rsp = re.search(r"^rsp: (0x[0-9a-f]{16})", report, re.MULTILINE)
    assert rsp and bottom - 4096 <= int(rsp.group(1), 16) < bottom + 4096, report
    assert bottom >> 40 == 0xFFFFD0, report  # the stack window
    rip = re.search(r"^rip: 0x[0-9a-f]{16} <(.+)>$", report, re.MULTILINE)
    assert rip and recursing in rip.group(1), report
    frames = backtrace(report)
    assert frames and recursing in frames[0], report
    # The reporter ran to the end on its 64 KiB IST stack: no nested crash,
    # and the last section (the heap line) is there.
    assert "!!!" not in report and heap_line(report), report


def test_stack_overflow_hits_guard(crash_report):
    # Unbounded recursion on the main kernel stack runs into its guard page.
    # The #PF can't be delivered on the full stack, so it arrives as a #DF on
    # the double-fault IST stack, with cr2 still in the guard page: the report
    # says so, and QEMU exits with the crash code (no reset, no triple fault).
    report = crash_report("stack")
    assert_main_stack_overflow(report, "kmon::recurse+")
    frames = backtrace(report)
    assert len(frames) == 32 and all("kmon::recurse+" in f for f in frames), report
    assert "(stopped after 32 frames)" in report, report


def test_big_frames_hit_the_guard_page_too(crash_report):
    # Sparse 16 KiB frames (only the lowest word written): stack probes touch
    # every page in turn, so the overflow still faults in the guard page
    # instead of jumping over it into the stack mapped below (the #MC IST
    # stack).
    report = crash_report("bigframe")
    assert_main_stack_overflow(report, "kmon::recurse_big")
    # Only about 8 such frames fit in 128 KiB, so the walk goes past them,
    # back across the stack switch, to the entry point.
    frames = backtrace(report)
    assert any("kmon::crash_big_frame_overflow" in f for f in frames), report
    assert frames[-1].startswith("_start"), report


def test_touching_a_guard_page_is_named(crash_report):
    # A write into the main stack's guard page from a healthy stack is a
    # plain #PF (it can be delivered), and the report names the guard page
    # as a stray access, not an overflow (rsp is nowhere near it).
    report = crash_report("guard")
    assert "exception: #PF page fault (vector 14, error code 0x2)" in report, report
    assert "kernel stack overflow" not in report, report
    cr2 = re.search(r"faulting address \(cr2\): (0x[0-9a-f]{16})", report)
    line = re.search(
        r"^guard page hit: cr2 (?P<cr2>0x[0-9a-f]{16}) is in the guard page below the kernel main stack "
        r"\[(?P<bottom>0x[0-9a-f]+), (?P<top>0x[0-9a-f]+)\), above the #MC IST stack; "
        r"rsp (?P<rsp>0x[0-9a-f]{16}) is not near it: a stray access, not an overflow$",
        report,
        re.MULTILINE,
    )
    assert cr2 and line and line["cr2"] == cr2.group(1), report
    bottom, top = int(line["bottom"], 16), int(line["top"], 16)
    assert bottom - 4096 + 8 == int(cr2.group(1), 16), report
    # kmon runs well inside the main stack, nowhere near its guard page.
    assert bottom + 4096 <= int(line["rsp"], 16) < top, report
    assert page_walk(report).end == "not present at L1", report
