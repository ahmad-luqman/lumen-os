"""Memory management at boot: the frame allocator is built and reports itself.

The allocator's correctness is checked in-kernel
(`lumen_kernel::mm::frame::*`, run by test_ktest) and on the host
(`cargo test -p lumen-mm`); this checks what boot reports.
"""

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
