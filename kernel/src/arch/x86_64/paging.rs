//! Paging hardware: CR3, the TLB, NX and write protection, and the PAT.
//!
//! The page-table format, walks and the mapper are pure logic in
//! `lumen_mm::paging` (host-tested); `crate::mm::paging` builds the kernel's
//! tables with it. This module is the part that touches the CPU.

use core::arch::asm;

use lumen_mm::{PhysFrame, VirtAddr, paging::Cache};

use super::cpu::{cpuid, rdmsr, read_cr0, read_cr3, read_cr4, write_cr0, write_cr3 as write_cr3_raw, write_cr4, wrmsr};

/// CR0.WP: supervisor writes honour read-only pages.
pub const CR0_WP: u64 = 1 << 16;
/// CR4.PGE: global pages survive CR3 writes.
const CR4_PGE: u64 = 1 << 7;
/// CR4.LA57: 5-level paging.
const CR4_LA57: u64 = 1 << 12;
/// CR4.PCIDE: process-context identifiers (not used; must stay 0).
const CR4_PCIDE: u64 = 1 << 17;
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
fn nx_supported() -> bool {
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
/// data and every pointer it holds) at the same virtual addresses, to the
/// same physical frames, with rights that allow how the kernel uses them.
pub unsafe fn load_root(root: PhysFrame) {
    // SAFETY: the caller guarantees the new hierarchy maps the running
    // kernel. A CR3 write flushes non-global entries; toggling PGE flushes
    // the global ones too (Limine may have set G on its mappings).
    unsafe {
        write_cr3_raw(root.as_u64());
        let cr4 = read_cr4();
        if cr4 & CR4_PGE != 0 {
            write_cr4(cr4 & !CR4_PGE);
            write_cr4(cr4);
        }
    }
}

/// Drop every non-global translation this CPU has cached, by writing CR3
/// its own value back (no PCIDs in use). That covers the kernel's heap and
/// stack windows: the kernel's tables never set the global bit
/// (`lumen_mm::paging::Attrs` has none).
pub fn flush_local() {
    // SAFETY: CR3 keeps its value, so the same tables stay in use; only
    // cached translations go, and the walker refills them from the tables.
    unsafe { write_cr3_raw(read_cr3()) }
}

/// Write `root` directly to CR3 (`CR4.PCIDE = 0`), without toggling `CR4.PGE`.
///
/// Used on per-process address-space switches: flushes non-global TLB entries
/// while avoiding [`load_root`]'s global-page flush.
///
/// # Safety
/// `root` must be a 4 KiB-aligned physical L4 root table (below 2^52) whose
/// upper half (`L4[256..512]`) maps the running kernel identically to the
/// kernel hierarchy. Any load of a user address space root (or switch away from
/// one) must go through `AddressSpace::switch_loaded`, which keeps
/// `AddressSpace::active` and `PerCpu::loaded_space` in step with `CR3`.
pub unsafe fn write_cr3(root: u64) {
    debug_assert!(PhysFrame::from_u64(root).is_some(), "CR3 root {root:#x} is not a 4 KiB physical frame");
    debug_assert!(read_cr4() & CR4_PCIDE == 0, "CR4.PCIDE must remain 0");
    // SAFETY: delegated to the caller. With CR4.PCIDE = 0, a plain mov to CR3
    // loads the new L4 root and flushes non-global TLB entries.
    unsafe { write_cr3_raw(root) }
}

/// Drop any TLB entry for the page containing `virt`.
pub fn invalidate(virt: VirtAddr) {
    // SAFETY: invlpg only discards a cached translation.
    unsafe { asm!("invlpg [{}]", in(reg) virt.as_u64(), options(nostack, preserves_flags)) }
    #[cfg(feature = "ktest")]
    invalidate_watch::record(virt);
}

/// Ktest-only observer of local [`invalidate`] calls for a single `(root, va)`.
#[cfg(feature = "ktest")]
pub mod invalidate_watch {
    use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};
    use lumen_mm::{PhysFrame, VirtAddr};

    static WATCH_ROOT: AtomicU64 = AtomicU64::new(0);
    static WATCH_VA: AtomicU64 = AtomicU64::new(0);
    static HITS: AtomicU32 = AtomicU32::new(0);

    /// RAII guard recording [`super::invalidate`] calls on `va` while `root` is active.
    #[must_use]
    pub struct Watch {
        _private: (),
    }

    impl Watch {
        pub fn new(root: PhysFrame, va: u64) -> Self {
            assert!(va != 0, "invalidate_watch requires a nonzero virtual address");
            assert!(
                WATCH_VA.compare_exchange(0, u64::MAX, Ordering::AcqRel, Ordering::Acquire).is_ok(),
                "overlapping invalidate_watch::Watch"
            );
            HITS.store(0, Ordering::Relaxed);
            WATCH_ROOT.store(root.as_u64(), Ordering::Relaxed);
            WATCH_VA.store(va, Ordering::Release);
            Self { _private: () }
        }

        pub fn hits(&self) -> u32 {
            HITS.load(Ordering::Acquire)
        }
    }

    impl Drop for Watch {
        fn drop(&mut self) {
            WATCH_VA.store(0, Ordering::Release);
            WATCH_ROOT.store(0, Ordering::Release);
        }
    }

    pub(super) fn record(virt: VirtAddr) {
        let va = WATCH_VA.load(Ordering::Acquire);
        if va == 0 || va != virt.as_u64() {
            return;
        }
        if super::active_root().as_u64() == WATCH_ROOT.load(Ordering::Relaxed) {
            HITS.fetch_add(1, Ordering::AcqRel);
        }
    }
}

/// Make the CPU honour the NX bit and read-only pages in supervisor mode.
/// Limine turns both on already; the kernel's W^X depends on them, so it
/// doesn't rely on that.
pub fn enforce_nx_and_wp() {
    assert!(nx_supported(), "the CPU has no NX bit; W^X can't be enforced");
    // SAFETY: NXE is supported (checked above).
    unsafe { turn_on_nx_and_wp() }
}

/// [`enforce_nx_and_wp`] without the check, for an application processor
/// before it may fault (`smp::ap_entry`: nothing there can be reported).
/// It checks again once it can be.
///
/// # Safety
/// This CPU supports NX: the boot CPU checked, and every CPU of the machine
/// is the same model.
pub unsafe fn turn_on_nx_and_wp() {
    // SAFETY: EFER exists on every x86-64 CPU and NXE is supported (the
    // caller). Limine sets both already, so this is normally a no-op; if NXE
    // were off, no live entry could have bit 63 set (it would be reserved
    // and fault), so turning it on takes execute rights from no page in use.
    // WP only makes ring 0 stricter.
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

/// The memory type encoding the PAT uses for `cache` (Intel SDM Vol. 3A,
/// "Memory Types That Can Be Encoded With PAT").
const fn memory_type(cache: Cache) -> u64 {
    match cache {
        Cache::Uncached => 0,
        Cache::WriteCombining => 1,
        Cache::WriteBack => 6,
    }
}

/// The PAT MSR: eight memory types, one per byte.
pub fn pat() -> u64 {
    // SAFETY: the PAT MSR exists on every x86-64 CPU.
    unsafe { rdmsr(IA32_PAT) }
}

/// Check that the PAT slots `lumen_mm::paging::Cache` selects hold the memory
/// types it names. Limine sets this layout up (slot 0 WB, 3 UC, 5 WC); the
/// kernel checks rather than rewrites the PAT while its own mappings use it.
pub fn check_pat() {
    let pat = pat();
    for cache in [Cache::WriteBack, Cache::Uncached, Cache::WriteCombining] {
        let slot = cache.pat_slot();
        let got = (pat >> (8 * slot)) & 0x7;
        assert_eq!(got, memory_type(cache), "PAT slot {slot} is not {cache:?} (PAT {pat:#018x})");
    }
}
