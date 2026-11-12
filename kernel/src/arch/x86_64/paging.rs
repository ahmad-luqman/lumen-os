//! Paging hardware: CR3, the TLB, NX and write protection, and the PAT.
//!
//! The page-table format, walks and the mapper are pure logic in
//! `lumen_mm::paging` (host-tested); `crate::mm::paging` builds the kernel's
//! tables with it. This module is the part that touches the CPU.

use core::arch::asm;

use lumen_mm::{PhysFrame, VirtAddr, paging::Cache};

use super::cpu::{cpuid, rdmsr, read_cr0, read_cr3, read_cr4, write_cr0, write_cr3, write_cr4, wrmsr};

/// CR0.WP: supervisor writes honour read-only pages.
pub const CR0_WP: u64 = 1 << 16;
/// CR4.PGE: global pages survive CR3 writes.
const CR4_PGE: u64 = 1 << 7;
/// CR4.LA57: 5-level paging.
const CR4_LA57: u64 = 1 << 12;
const IA32_EFER: u32 = 0xc000_0080;
/// EFER.NXE: the no-execute bit in page-table entries is honoured.
pub const EFER_NXE: u64 = 1 << 11;
const IA32_PAT: u32 = 0x277;
/// CR3 bits holding the root table's address (no PCIDs in use).
const CR3_ADDR: u64 = 0x000f_ffff_ffff_f000;

/// Is the CPU using 4-level paging, the only kind the kernel supports?
pub fn four_level() -> bool {
    read_cr4() & CR4_LA57 == 0
}

/// Does the CPU support the no-execute bit (CPUID 0x8000_0001, EDX bit 20)?
pub fn nx_supported() -> bool {
    cpuid(0x8000_0000).0 >= 0x8000_0001 && cpuid(0x8000_0001).3 & (1 << 20) != 0
}

/// The root (L4) table in use.
pub fn active_root() -> PhysFrame {
    PhysFrame::from_u64(read_cr3() & CR3_ADDR).expect("CR3's address bits are a frame")
}

/// Switch to the hierarchy rooted at `root` and flush the whole TLB,
/// including global entries.
///
/// # Safety
/// `root` must map everything the kernel uses from here on (code, stack,
/// data and every pointer it holds) at the same virtual addresses.
pub unsafe fn load_root(root: PhysFrame) {
    // SAFETY: the caller guarantees the new hierarchy maps the running
    // kernel. A CR3 write flushes non-global entries; toggling PGE flushes
    // the global ones too (Limine may have set G on its mappings).
    unsafe {
        write_cr3(root.as_u64());
        let cr4 = read_cr4();
        if cr4 & CR4_PGE != 0 {
            write_cr4(cr4 & !CR4_PGE);
            write_cr4(cr4);
        }
    }
}

/// Drop any TLB entry for the page containing `virt`.
pub fn invalidate(virt: VirtAddr) {
    // SAFETY: invlpg only discards a cached translation.
    unsafe { asm!("invlpg [{}]", in(reg) virt.as_u64(), options(nostack, preserves_flags)) }
}

/// Make the CPU honour the NX bit and read-only pages in supervisor mode.
/// Limine turns both on already; the kernel's W^X depends on them, so it
/// doesn't rely on that.
pub fn enforce_nx_and_wp() {
    assert!(nx_supported(), "the CPU has no NX bit; W^X can't be enforced");
    // SAFETY: EFER exists on every x86-64 CPU; setting NXE (supported, checked
    // above) and WP only make the CPU stricter.
    unsafe {
        wrmsr(IA32_EFER, rdmsr(IA32_EFER) | EFER_NXE);
        write_cr0(read_cr0() | CR0_WP);
    }
}

/// EFER, for the in-kernel tests.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))]
pub fn efer() -> u64 {
    // SAFETY: EFER exists on every x86-64 CPU.
    unsafe { rdmsr(IA32_EFER) }
}

/// The memory type encoding the PAT uses for `cache` (Intel SDM 13.12.2).
const fn memory_type(cache: Cache) -> u64 {
    match cache {
        Cache::Uncached => 0,
        Cache::WriteCombining => 1,
        Cache::WriteBack => 6,
    }
}

/// Check that the PAT slots `lumen_mm::paging::Cache` selects hold the memory
/// types it names. Limine sets this layout up (slot 0 WB, 3 UC, 5 WC); the
/// kernel checks rather than rewrites the PAT while its own mappings use it.
pub fn check_pat() {
    // SAFETY: the PAT MSR exists on every x86-64 CPU.
    let pat = unsafe { rdmsr(IA32_PAT) };
    for cache in [Cache::WriteBack, Cache::Uncached, Cache::WriteCombining] {
        let slot = cache.pat_slot();
        let got = (pat >> (8 * slot)) & 0x7;
        assert_eq!(got, memory_type(cache), "PAT slot {slot} is not {cache:?} (PAT {pat:#018x})");
    }
}
