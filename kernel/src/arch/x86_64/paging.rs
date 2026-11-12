//! Read-only walks of the active 4-level page tables (the ones in CR3).
//!
//! M1's frame allocator uses this to find out which physical frames are in
//! use before it exists: the frames of the page tables themselves, and the
//! physical frames behind live virtual addresses (kernel image, boot stack,
//! framebuffer). Tables are read through the higher-half direct map, so the
//! caller passes its offset. Building our own tables is a separate issue (#4).

use super::cpu::read_cr3;

const ENTRIES: usize = 512;
const PRESENT: u64 = 1 << 0;
/// Page size bit: in an L3 entry maps 1 GiB, in an L2 entry 2 MiB.
const HUGE: u64 = 1 << 7;
/// Physical address bits of an entry pointing to a table or 4 KiB page.
/// Masks off the flags in bits 0-11 and NX/protection keys in bits 52-63.
const ADDR_4K: u64 = 0x000f_ffff_ffff_f000;
/// Physical address bits of a 2 MiB / 1 GiB entry (bit 12 there is PAT).
const ADDR_2M: u64 = 0x000f_ffff_ffe0_0000;
const ADDR_1G: u64 = 0x000f_ffff_c000_0000;

/// Read one table (512 entries) at physical address `table`.
///
/// # Safety
/// `table` must be a page-table frame of the active hierarchy and `hhdm` the
/// direct-map offset under which it is mapped.
unsafe fn table(hhdm: u64, table: u64) -> &'static [u64; ENTRIES] {
    // SAFETY: the caller guarantees the frame is a live, mapped page table;
    // we only read it, and page tables are not modified concurrently (one CPU,
    // nothing changes the tables Limine built).
    unsafe { &*((hhdm + table) as *const [u64; ENTRIES]) }
}

fn root() -> u64 {
    read_cr3() & ADDR_4K
}

/// The physical address `virt` maps to, if it is mapped.
pub fn translate(hhdm: u64, virt: u64) -> Option<u64> {
    let index = |level: u32| ((virt >> (12 + 9 * level)) & 0x1ff) as usize;
    // SAFETY: each table address comes from CR3 or from a present, non-huge
    // entry of the previous level, so it is a live page table.
    unsafe {
        let l4e = table(hhdm, root())[index(3)];
        if l4e & PRESENT == 0 {
            return None;
        }
        let l3e = table(hhdm, l4e & ADDR_4K)[index(2)];
        if l3e & PRESENT == 0 {
            return None;
        }
        if l3e & HUGE != 0 {
            return Some((l3e & ADDR_1G) + (virt & ((1 << 30) - 1)));
        }
        let l2e = table(hhdm, l3e & ADDR_4K)[index(1)];
        if l2e & PRESENT == 0 {
            return None;
        }
        if l2e & HUGE != 0 {
            return Some((l2e & ADDR_2M) + (virt & ((1 << 21) - 1)));
        }
        let l1e = table(hhdm, l2e & ADDR_4K)[index(0)];
        if l1e & PRESENT == 0 {
            return None;
        }
        Some((l1e & ADDR_4K) + (virt & 0xfff))
    }
}

/// Call `f` with the physical address of every page-table frame in the
/// active hierarchy: the L4 table and every L3, L2 and L1 table it reaches.
/// Returns how many frames were visited.
pub fn for_each_table_frame(hhdm: u64, mut f: impl FnMut(u64)) -> usize {
    let mut visited = 0;
    let mut visit = |frame: u64| {
        visited += 1;
        f(frame);
    };
    let l4 = root();
    visit(l4);
    // SAFETY: as in `translate`: every table address comes from CR3 or a
    // present, non-huge entry of the level above.
    unsafe {
        for &l4e in table(hhdm, l4) {
            if l4e & PRESENT == 0 {
                continue;
            }
            let l3 = l4e & ADDR_4K;
            visit(l3);
            for &l3e in table(hhdm, l3) {
                if l3e & PRESENT == 0 || l3e & HUGE != 0 {
                    continue;
                }
                let l2 = l3e & ADDR_4K;
                visit(l2);
                for &l2e in table(hhdm, l2) {
                    if l2e & PRESENT == 0 || l2e & HUGE != 0 {
                        continue;
                    }
                    visit(l2e & ADDR_4K);
                }
            }
        }
    }
    visited
}
