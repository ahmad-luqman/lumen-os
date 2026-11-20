//! Per-process user address spaces (`AddressSpace`), ELF64 segment loading,
//! anonymous VM operations (`mmap`/`munmap`/`mprotect`/`brk`), demand paging,
//! and safe teardown.
//!
//! See `docs/ARCHITECTURE.md` §5 ("Per-process address spaces and user VM")
//! and §8 ("Static ELF64 loader and initial stack").

use alloc::vec;
use core::fmt;

use lumen_elf::loader::{ElfLoadError, LoadedElf, PageAction, Perm};
use lumen_elf::stack::{AuxvInfo, InitialStack, StackBuildError, build_initial_stack};
use lumen_mm::paging::{Attrs, MapError, WalkEnd};
use lumen_mm::{FRAME_SIZE, PhysFrame, VirtAddr};
use lumen_vm::{Brk, MMAP_TOP, MapPlace, PAGE_SIZE, Prot, USER_MAX, USER_MIN, USER_STACK_TOP, VmError, VmaMap};

use super::{frame, paging};
use crate::arch;
use crate::linux::{errno, general};
use crate::sync::SleepMutex;

/// Initial user stack VMA size (8 MiB, matching M6 `RLIMIT_STACK`).
pub const USER_STACK_SIZE: u64 = 8 * 1024 * 1024;
/// Inclusive bottom of the initial demand-paged user stack VMA.
pub const USER_STACK_BOTTOM: u64 = USER_STACK_TOP - USER_STACK_SIZE;

const _: () = {
    assert!(FRAME_SIZE == PAGE_SIZE);
    assert!(USER_STACK_BOTTOM > USER_MIN + PAGE_SIZE);
    assert!(USER_STACK_TOP < USER_MAX);
};

/// Result of loading a static ELF64 program into an [`AddressSpace`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LoadedProgram {
    /// Entry-point user virtual address (`load_bias + e_entry`).
    pub entry: u64,
    /// Initial 16-byte-aligned user stack pointer pointing at `argc`.
    pub rsp: u64,
    /// Initial page-aligned program break above the highest `PT_LOAD` segment.
    pub initial_brk: u64,
}

/// Outcome of [`AddressSpace::handle_page_fault`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FaultResult {
    /// Fault was resolved by demand-paging (or spurious TLB refresh); retry.
    Resolved,
    /// Access violated VMA permissions, hit `PROT_NONE`, or hit an unmapped address.
    Invalid,
    /// Valid demand-paging fault, but physical memory is exhausted.
    OutOfMemory,
}

/// Error returned by [`AddressSpace`] construction, ELF loading, or VM calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserVmError {
    OutOfMemory,
    Elf(ElfLoadError),
    Stack(StackBuildError),
    Vm(VmError),
    InvalidArg,
}

impl UserVmError {
    /// Corresponding Linux `errno` value for syscall returns.
    pub const fn to_errno(self) -> u32 {
        match self {
            UserVmError::OutOfMemory | UserVmError::Vm(VmError::NoMem) => errno::ENOMEM,
            UserVmError::Vm(VmError::Exists) => errno::EEXIST,
            UserVmError::Vm(VmError::Misaligned | VmError::InvalidRange) | UserVmError::InvalidArg => errno::EINVAL,
            UserVmError::Elf(_) => errno::ENOEXEC,
            UserVmError::Stack(_) => errno::E2BIG,
        }
    }
}

impl From<VmError> for UserVmError {
    fn from(e: VmError) -> Self {
        Self::Vm(e)
    }
}

impl fmt::Display for UserVmError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            UserVmError::OutOfMemory => f.write_str("out of physical frames"),
            UserVmError::Elf(e) => write!(f, "ELF load error: {e}"),
            UserVmError::Stack(e) => write!(f, "initial stack error: {e}"),
            UserVmError::Vm(e) => write!(f, "VM error: {e}"),
            UserVmError::InvalidArg => f.write_str("invalid argument"),
        }
    }
}

/// Convert `lumen_vm::Prot` to `lumen_mm::paging::Attrs` for a user page.
#[inline]
pub const fn prot_to_attrs(prot: Prot) -> Attrs {
    Attrs::for_user(prot.read(), prot.write(), prot.exec())
}

/// Convert `lumen_elf::Perm` to `lumen_vm::Prot`.
#[inline]
const fn perm_to_prot(perm: Perm) -> Prot {
    let mut bits = 0u8;
    if perm.read {
        bits |= Prot::READ.bits();
    }
    if perm.write {
        bits |= Prot::WRITE.bits();
    }
    if perm.exec {
        bits |= Prot::EXEC.bits();
    }
    match Prot::from_bits(bits) {
        Some(p) => p,
        None => Prot::NONE,
    }
}

/// Parse Linux `PROT_*` syscall flags into [`Prot`], rejecting unknown bits.
#[inline]
pub fn prot_from_linux(raw: u64) -> Option<Prot> {
    let allowed = u64::from(general::PROT_READ | general::PROT_WRITE | general::PROT_EXEC);
    if raw & !allowed != 0 {
        return None;
    }
    Prot::from_bits(raw as u8)
}

struct AddressSpaceInner {
    vmas: VmaMap,
    brk: Brk,
    torn_down: bool,
}

/// Per-process 4-level x86-64 user address space.
///
/// Owns a dedicated lower-half page-table hierarchy (`L4[0..256]`) sharing
/// kernel `L4[256..512]` (including the 6 pre-allocated dynamic kernel L3
/// tables), a [`VmaMap`], and a byte-granular [`Brk`] cursor, all synchronized
/// under a sleeping, interrupt-enabled [`SleepMutex`].
pub struct AddressSpace {
    root: PhysFrame,
    inner: SleepMutex<AddressSpaceInner>,
}

impl AddressSpace {
    /// Allocate a fresh empty user address space copying kernel `L4[256..512]`.
    pub fn new() -> Result<Self, UserVmError> {
        let root = paging::new_user_root().ok_or(UserVmError::OutOfMemory)?;
        let brk = Brk::with_limit(USER_MIN, MMAP_TOP).expect("USER_MIN..MMAP_TOP is valid");
        Ok(Self { root, inner: SleepMutex::new(AddressSpaceInner { vmas: VmaMap::new(), brk, torn_down: false }) })
    }

    /// Parse and load a static ELF64 image into a brand-new [`AddressSpace`].
    ///
    /// On any validation or allocation error, leaves kernel frames unchanged.
    pub fn from_elf(
        image: &[u8],
        argv: &[&[u8]],
        envp: &[&[u8]],
        execfn: &[u8],
        random: [u8; 16],
    ) -> Result<(Self, LoadedProgram), UserVmError> {
        // Validate ELF and initial stack layout before allocating any page-table root.
        let (loaded, stack_buf, initial_stack) = prepare_elf_and_stack(image, argv, envp, execfn, random)?;
        let space = Self::new()?;
        let prog = space.commit_loaded_elf(&loaded, image, &stack_buf, initial_stack)?;
        Ok((space, prog))
    }

    /// Physical frame of this address space's L4 root (lock-free, immutable).
    #[inline]
    pub fn root(&self) -> PhysFrame {
        self.root
    }

    /// Parse and load a static ELF64 image into this empty address space.
    ///
    /// Validates the ELF and stack before mutating page tables; on any error,
    /// rolls back so no frames are leaked and the address space stays empty.
    #[allow(dead_code)] // public AddressSpace API
    pub fn load_elf(
        &self,
        image: &[u8],
        argv: &[&[u8]],
        envp: &[&[u8]],
        execfn: &[u8],
        random: [u8; 16],
    ) -> Result<LoadedProgram, UserVmError> {
        let (loaded, stack_buf, initial_stack) = prepare_elf_and_stack(image, argv, envp, execfn, random)?;
        self.commit_loaded_elf(&loaded, image, &stack_buf, initial_stack)
    }

    fn commit_loaded_elf(
        &self,
        loaded: &LoadedElf,
        image: &[u8],
        stack_buf: &[u8],
        initial_stack: InitialStack,
    ) -> Result<LoadedProgram, UserVmError> {
        let mut inner = self.inner.lock();
        if inner.torn_down || !inner.vmas.is_empty() {
            return Err(UserVmError::InvalidArg);
        }

        let mapped_ranges = loaded.mapped_ranges();
        let stack_guard_bottom = USER_STACK_BOTTOM - PAGE_SIZE;
        for range in &mapped_ranges {
            if range.end > stack_guard_bottom && range.start < USER_STACK_TOP {
                return Err(UserVmError::Vm(VmError::Exists));
            }
        }

        let mut new_vmas = VmaMap::new();
        for range in &mapped_ranges {
            new_vmas.map_fixed_noreplace(range.start, range.end - range.start, perm_to_prot(range.perm))?;
        }
        new_vmas.map_fixed_noreplace(USER_STACK_BOTTOM, USER_STACK_SIZE, Prot::READ | Prot::WRITE)?;

        let brk_limit = if loaded.initial_brk <= MMAP_TOP { MMAP_TOP } else { USER_MAX };
        let new_brk = Brk::with_limit(loaded.initial_brk, brk_limit)?;

        let actions = loaded.plan_pages();
        let active = arch::paging::active_root() == self.root;
        let load_result = (|| -> Result<(), UserVmError> {
            for action in actions {
                match action {
                    PageAction::MapZero { vaddr, page_count, perm } => {
                        let attrs = prot_to_attrs(perm_to_prot(perm));
                        for i in 0..page_count {
                            let va = VirtAddr::new(vaddr + i * PAGE_SIZE).ok_or(UserVmError::InvalidArg)?;
                            let phys = frame::alloc_zeroed().ok_or(UserVmError::OutOfMemory)?;
                            // SAFETY: `self.root` is owned by `self`, `inner` lock is held,
                            // `va` is a fresh lower-half page, and `phys` transfers ownership.
                            if let Err(err) = unsafe { paging::map_user_page(self.root, va, phys, attrs) } {
                                // SAFETY: `phys` was never mapped.
                                unsafe { frame::free(phys) }.expect("freeing unmapped ELF frame");
                                return Err(match err {
                                    MapError::OutOfMemory => UserVmError::OutOfMemory,
                                    _ => UserVmError::InvalidArg,
                                });
                            }
                        }
                    }
                    PageAction::UpdatePerm { vaddr, perm } => {
                        let va = VirtAddr::new(vaddr).ok_or(UserVmError::InvalidArg)?;
                        let attrs = prot_to_attrs(perm_to_prot(perm));
                        // SAFETY: `self.root` is owned by `self` and `inner` lock is held.
                        unsafe { paging::protect_user_page(self.root, va, attrs) }
                            .map_err(|_| UserVmError::InvalidArg)?;
                        if active {
                            arch::paging::invalidate(va);
                        }
                    }
                    PageAction::CopyBytes { vaddr, file_offset, len } => {
                        let bytes = image.get(file_offset..file_offset + len).ok_or(UserVmError::InvalidArg)?;
                        self.write_mapped_bytes_locked(vaddr, bytes)?;
                    }
                }
            }

            // Eagerly map ONLY the top pages required by `build_initial_stack`.
            let stack_eager_start = USER_STACK_TOP - stack_buf.len() as u64;
            let mut va_addr = stack_eager_start;
            while va_addr < USER_STACK_TOP {
                let va = VirtAddr::new(va_addr).ok_or(UserVmError::InvalidArg)?;
                let phys = frame::alloc_zeroed().ok_or(UserVmError::OutOfMemory)?;
                // SAFETY: `self.root` is owned by `self`, `inner` lock is held,
                // `va` is a fresh lower-half stack page, and `phys` transfers ownership.
                if let Err(err) = unsafe { paging::map_user_page(self.root, va, phys, Attrs::USER_RW) } {
                    // SAFETY: `phys` was never mapped.
                    unsafe { frame::free(phys) }.expect("freeing unmapped stack frame");
                    return Err(match err {
                        MapError::OutOfMemory => UserVmError::OutOfMemory,
                        _ => UserVmError::InvalidArg,
                    });
                }
                va_addr += PAGE_SIZE;
            }
            self.write_mapped_bytes_locked(stack_eager_start, stack_buf)?;
            Ok(())
        })();

        if let Err(err) = load_result {
            // Roll back all partially mapped pages so no frames leak.
            for range in &mapped_ranges {
                self.unmap_and_free_range_locked(range.start, range.end);
            }
            let stack_eager_start = USER_STACK_TOP - stack_buf.len() as u64;
            self.unmap_and_free_range_locked(stack_eager_start, USER_STACK_TOP);
            return Err(err);
        }

        inner.vmas = new_vmas;
        inner.brk = new_brk;
        Ok(LoadedProgram { entry: loaded.entry, rsp: initial_stack.rsp, initial_brk: loaded.initial_brk })
    }

    /// Eagerly map a fixed page-aligned user region `[start, start + aligned_len)`
    /// with `prot` and copy `bytes` into it at offset 0 (for kernel test runners).
    pub fn map_eager_with_bytes(&self, start: u64, len: u64, prot: Prot, bytes: &[u8]) -> Result<(), UserVmError> {
        let aligned_len = lumen_vm::align_up_page(len.max(PAGE_SIZE)).ok_or(UserVmError::InvalidArg)?;
        if (bytes.len() as u64) > aligned_len {
            return Err(UserVmError::InvalidArg);
        }
        let mut inner = self.inner.lock();
        if inner.torn_down {
            return Err(UserVmError::InvalidArg);
        }
        inner.vmas.map_fixed_noreplace(start, aligned_len, prot)?;
        let attrs = prot_to_attrs(prot);
        let mut addr = start;
        let end = start + aligned_len;
        while addr < end {
            let va = VirtAddr::new(addr).ok_or(UserVmError::InvalidArg)?;
            let phys = match frame::alloc_zeroed() {
                Some(p) => p,
                None => {
                    self.unmap_and_free_range_locked(start, addr);
                    let _ = inner.vmas.unmap(start, aligned_len);
                    return Err(UserVmError::OutOfMemory);
                }
            };
            // SAFETY: `self.root` is owned by `self`, `inner` lock is held,
            // and `phys` is a freshly allocated frame mapped into the lower half.
            if let Err(err) = unsafe { paging::map_user_page(self.root, va, phys, attrs) } {
                // SAFETY: `phys` was not mapped.
                unsafe { frame::free(phys) }.expect("freeing unmapped user frame");
                self.unmap_and_free_range_locked(start, addr);
                let _ = inner.vmas.unmap(start, aligned_len);
                return Err(match err {
                    MapError::OutOfMemory => UserVmError::OutOfMemory,
                    _ => UserVmError::InvalidArg,
                });
            }
            addr += PAGE_SIZE;
        }
        if !bytes.is_empty() {
            self.write_mapped_bytes_locked(start, bytes)?;
        }
        Ok(())
    }

    /// Register a lazy anonymous VMA (`mmap(2)` with `MAP_ANONYMOUS`).
    ///
    /// Supports `MAP_PRIVATE` or `MAP_SHARED`, `MAP_FIXED`, `MAP_FIXED_NOREPLACE`,
    /// and `PROT_NONE`. For `MAP_FIXED`, unmaps and frees any replaced PTEs and
    /// flushes the local TLB.
    pub fn mmap_anon(&self, hint: u64, len: u64, prot: Prot, flags: u64) -> Result<u64, UserVmError> {
        let anon = u64::from(general::MAP_ANONYMOUS);
        let priv_flag = u64::from(general::MAP_PRIVATE);
        let shared_flag = u64::from(general::MAP_SHARED);
        if (flags & anon) == 0 {
            return Err(UserVmError::InvalidArg);
        }
        let share_bits = flags & (priv_flag | shared_flag);
        if share_bits != priv_flag && share_bits != shared_flag {
            return Err(UserVmError::InvalidArg);
        }
        let place = if (flags & u64::from(general::MAP_FIXED_NOREPLACE)) != 0 {
            MapPlace::FixedNoReplace(hint)
        } else if (flags & u64::from(general::MAP_FIXED)) != 0 {
            MapPlace::Fixed(hint)
        } else {
            MapPlace::Hint((hint != 0).then_some(hint))
        };

        let mut inner = self.inner.lock();
        if inner.torn_down {
            return Err(UserVmError::OutOfMemory);
        }
        let mapped = inner.vmas.map_anon(place, len, prot)?;
        for removed in mapped.unmapped {
            self.unmap_and_free_range_locked(removed.start, removed.end);
        }
        Ok(mapped.start)
    }

    /// Unmap `[addr, align_up_page(addr + len))`, freeing any populated Present
    /// or `PTE_PROTNONE` physical frames and flushing the local TLB.
    pub fn munmap(&self, addr: u64, len: u64) -> Result<(), UserVmError> {
        let mut inner = self.inner.lock();
        if inner.torn_down {
            return Ok(());
        }
        let removed = inner.vmas.unmap(addr, len)?;
        for vma in removed {
            self.unmap_and_free_range_locked(vma.start, vma.end);
        }
        Ok(())
    }

    /// Atomically update protections on `[addr, align_up_page(addr + len))`,
    /// splitting/coalescing VMAs and updating any already-mapped (Present or
    /// `PTE_PROTNONE`) PTEs via `lumen_mm::paging::protect` while preserving
    /// physical frames across `Present <-> PTE_PROTNONE`.
    pub fn mprotect(&self, addr: u64, len: u64, prot: Prot) -> Result<(), UserVmError> {
        let mut inner = self.inner.lock();
        if inner.torn_down {
            return Err(UserVmError::Vm(VmError::NoMem));
        }
        inner.vmas.protect(addr, len, prot)?;
        if len == 0 {
            return Ok(());
        }
        let aligned_len = lumen_vm::align_up_page(len).expect("validated by VmaMap::protect");
        let end = addr + aligned_len;
        let attrs = prot_to_attrs(prot);
        let active = arch::paging::active_root() == self.root;
        // SAFETY: `self.root` is owned by `self`, `inner` lock is held, and
        // `[addr, end)` is page-aligned and strictly in `USER_MIN..USER_MAX`.
        unsafe {
            paging::protect_user_range(self.root, addr, end, attrs, |va, _was_present| {
                if active {
                    arch::paging::invalidate(va);
                }
            });
        }
        Ok(())
    }

    /// Adjust the byte-granular program break (`brk(2)`).
    ///
    /// Returns the exact requested `addr` on success, or the unchanged current
    /// break on query (`addr == 0`) or failure. On shrink across page
    /// boundaries, unmaps/frees tail pages and flushes the local TLB.
    pub fn brk(&self, addr: u64) -> u64 {
        let mut inner = self.inner.lock();
        if inner.torn_down || addr == 0 {
            return inner.brk.current();
        }
        let AddressSpaceInner { vmas, brk, .. } = &mut *inner;
        let change = brk.set(vmas, addr);
        for vma in change.unmapped {
            self.unmap_and_free_range_locked(vma.start, vma.end);
        }
        change.current
    }

    /// Resolve a demand-paging `#PF` at `fault_addr` with CPU `error_code`.
    ///
    /// Must be called with interrupts enabled and `irq::depth() == 0` (step 3
    /// of the `#PF` interrupt protocol). Rejects unmapped addresses, `PROT_NONE`,
    /// write fault (`bit 1`) on non-writable VMA, exec fault (`bit 4`) on
    /// non-executable VMA, and present-page protection violations (`bit 0 == 1`).
    pub fn handle_page_fault(&self, fault_addr: u64, error_code: u64) -> FaultResult {
        if !(USER_MIN..USER_MAX).contains(&fault_addr) {
            return FaultResult::Invalid;
        }
        // Present-page protection violation (bit 0 == 1): no CoW in M6.
        if (error_code & (1 << 0)) != 0 {
            return FaultResult::Invalid;
        }
        let is_write = (error_code & (1 << 1)) != 0;
        let is_exec = (error_code & (1 << 4)) != 0;

        let inner = self.inner.lock();
        if inner.torn_down {
            return FaultResult::Invalid;
        }
        let Some(vma) = inner.vmas.find(fault_addr) else {
            return FaultResult::Invalid;
        };
        if vma.prot.is_none() {
            return FaultResult::Invalid;
        }
        if is_write && !vma.prot.write() {
            return FaultResult::Invalid;
        }
        if is_exec && !vma.prot.exec() {
            return FaultResult::Invalid;
        }

        let page_va = VirtAddr::new(lumen_vm::align_down_page(fault_addr)).expect("in USER_MIN..USER_MAX");
        match paging::walk_user(self.root, page_va).end() {
            WalkEnd::Mapped(_) => {
                if arch::paging::active_root() == self.root {
                    arch::paging::invalidate(page_va);
                }
                return FaultResult::Resolved;
            }
            WalkEnd::ProtNone(_) => return FaultResult::Invalid,
            WalkEnd::NotPresent(_) => {}
            WalkEnd::Unreadable(level, frame) => {
                panic!("unreadable user page table {frame} at {level:?} for {page_va}");
            }
        }

        let Some(phys) = frame::alloc_zeroed() else {
            return FaultResult::OutOfMemory;
        };
        let attrs = prot_to_attrs(vma.prot);
        // SAFETY: `self.root` is owned by `self`, `inner` lock is held,
        // `page_va` is in `USER_MIN..USER_MAX` and not present, and `phys` is
        // a freshly allocated zeroed frame whose ownership transfers to `self`.
        if let Err(err) = unsafe { paging::map_user_page(self.root, page_va, phys, attrs) } {
            // SAFETY: `phys` was never mapped.
            unsafe { frame::free(phys) }.expect("freeing unmapped demand-page frame");
            return match err {
                MapError::OutOfMemory => FaultResult::OutOfMemory,
                _ => FaultResult::Invalid,
            };
        }
        FaultResult::Resolved
    }

    /// Tear down all VMAs, user leaf frames (present and `PTE_PROTNONE`), and
    /// user L1/L2/L3 tables while interrupts are enabled.
    ///
    /// Must be called only after switching `CR3` back to `kernel_root()`.
    /// Subsequent [`Drop`] frees only the bare L4 root frame without locking.
    pub fn teardown(&self) {
        assert_ne!(
            arch::paging::active_root(),
            self.root,
            "AddressSpace::teardown called while CR3 still holds {}",
            self.root
        );
        let mut inner = self.inner.lock();
        if inner.torn_down {
            return;
        }
        inner.vmas = VmaMap::new();
        // SAFETY: `self.root` is not active in CR3 (asserted above), `inner`
        // lock is held, and all lower-half leaves/tables belong exclusively to `self`.
        unsafe { paging::clear_user_lower_half(self.root) };
        inner.torn_down = true;
    }

    /// Copy `bytes` into already-mapped pages of `self.root` via the HHDM.
    fn write_mapped_bytes_locked(&self, mut va: u64, mut bytes: &[u8]) -> Result<(), UserVmError> {
        while !bytes.is_empty() {
            let page_start = lumen_vm::align_down_page(va);
            let off = (va - page_start) as usize;
            let chunk = (PAGE_SIZE as usize - off).min(bytes.len());
            let page_va = VirtAddr::new(page_start).ok_or(UserVmError::InvalidArg)?;
            let phys = match paging::walk_user(self.root, page_va).end() {
                WalkEnd::Mapped(m) | WalkEnd::ProtNone(m) => m.phys,
                _ => return Err(UserVmError::InvalidArg),
            };
            let dst = super::phys_to_virt(phys.start()).as_mut_ptr::<u8>();
            // SAFETY: `phys` is an allocated usable RAM frame owned by `self`,
            // mapped RW through the HHDM, and `off + chunk <= PAGE_SIZE`.
            unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst.add(off), chunk) };
            va += chunk as u64;
            bytes = &bytes[chunk..];
        }
        Ok(())
    }

    /// Unmap and free all populated leaf frames in page-aligned `[start, end)`.
    fn unmap_and_free_range_locked(&self, start: u64, end: u64) {
        let active = arch::paging::active_root() == self.root;
        // SAFETY: `self.root` is owned by `self`, `inner` lock is held, and
        // `[start, end)` is page-aligned and strictly in the lower half.
        unsafe {
            paging::unmap_user_range(self.root, start, end, |va, phys, _was_present| {
                if active {
                    arch::paging::invalidate(va);
                }
                // SAFETY: leaf frame belonged exclusively to this address space
                // and has been unmapped and invalidated from the local TLB.
                frame::free(phys).expect("freeing unmapped user leaf frame");
            });
        }
    }
}

impl Drop for AddressSpace {
    fn drop(&mut self) {
        assert_ne!(arch::paging::active_root(), self.root, "AddressSpace dropped while CR3 still holds {}", self.root);
        // No sleeping lock needed: `&mut self` guarantees exclusive access, so
        // `Drop` is safe even inside `sched::finish_switch` with interrupts off.
        let inner = self.inner.get_mut();
        if inner.torn_down {
            // SAFETY: lower half was already cleared by `teardown()`; only the
            // bare L4 root frame remains to be freed.
            unsafe { frame::free(self.root) }.expect("freeing user L4 root frame");
        } else {
            // SAFETY: `self.root` is not active in CR3 and exclusively owns its
            // lower-half subtree.
            unsafe { paging::destroy_user_root(self.root) };
        }
    }
}

/// Validate ELF64 image and build the exact top stack pages buffer before
/// allocating any page tables or physical frames.
fn prepare_elf_and_stack(
    image: &[u8],
    argv: &[&[u8]],
    envp: &[&[u8]],
    execfn: &[u8],
    random: [u8; 16],
) -> Result<(LoadedElf, alloc::vec::Vec<u8>, InitialStack), UserVmError> {
    let loaded = LoadedElf::parse(image).map_err(UserVmError::Elf)?;
    let auxv = AuxvInfo::from_loaded(&loaded);

    // Probe required initial stack bytes (and validate no interior NULs) without allocating.
    let required = match build_initial_stack(&mut [], USER_STACK_TOP, argv, envp, execfn, random, &auxv) {
        Err(StackBuildError::BufferTooSmall { required, .. }) => required,
        Err(e) => return Err(UserVmError::Stack(e)),
        Ok(info) => info.bytes_written,
    };
    if (required as u64) > USER_STACK_SIZE {
        return Err(UserVmError::Stack(StackBuildError::BufferTooSmall {
            required,
            available: USER_STACK_SIZE as usize,
        }));
    }
    let buf_len = lumen_vm::align_up_page((required as u64).max(1)).ok_or(UserVmError::InvalidArg)? as usize;
    let mut stack_buf = vec![0u8; buf_len];
    let initial_stack = build_initial_stack(&mut stack_buf, USER_STACK_TOP, argv, envp, execfn, random, &auxv)
        .map_err(UserVmError::Stack)?;
    Ok((loaded, stack_buf, initial_stack))
}

#[cfg(feature = "ktest")]
fn build_relro_test_elf() -> alloc::vec::Vec<u8> {
    use lumen_elf::loader::{EM_X86_64, ET_EXEC, EV_CURRENT, PF_R, PF_W, PF_X, PHDR_MIN_SIZE, PT_LOAD};

    const EHDR_SIZE: usize = 64;
    const ELF_MAGIC: [u8; 4] = [0x7f, b'E', b'L', b'F'];
    const ELFCLASS64: u8 = 2;
    const ELFDATA2LSB: u8 = 1;

    // 3 PT_LOAD segments:
    //  0: R-X code at 0x401000..0x401020 (file offset 0x1000, 32 bytes)
    //  1: R-- rodata at 0x402000..0x402800 (file offset 0x2000, 0x800 bytes)
    //  2: RW- data+bss at 0x402800..0x403200 (file offset 0x2800, filesz 0x100, memsz 0xa00)
    // Segments 1 and 2 share boundary page 0x402000 (RELRO union: RW-, neither clobbered).
    let mut img = vec![0u8; 0x3000];
    img[0..4].copy_from_slice(&ELF_MAGIC);
    img[4] = ELFCLASS64;
    img[5] = ELFDATA2LSB;
    img[6] = EV_CURRENT;
    img[0x10..0x12].copy_from_slice(&ET_EXEC.to_le_bytes());
    img[0x12..0x14].copy_from_slice(&EM_X86_64.to_le_bytes());
    img[0x14..0x18].copy_from_slice(&u32::from(EV_CURRENT).to_le_bytes());
    img[0x18..0x20].copy_from_slice(&0x40_1000u64.to_le_bytes());
    img[0x20..0x28].copy_from_slice(&(EHDR_SIZE as u64).to_le_bytes());
    img[0x34..0x36].copy_from_slice(&(EHDR_SIZE as u16).to_le_bytes());
    img[0x36..0x38].copy_from_slice(&PHDR_MIN_SIZE.to_le_bytes());
    img[0x38..0x3a].copy_from_slice(&3u16.to_le_bytes());

    let write_phdr = |buf: &mut [u8], idx: usize, flags: u32, off: u64, va: u64, fsz: u64, msz: u64| {
        let o = EHDR_SIZE + idx * usize::from(PHDR_MIN_SIZE);
        buf[o..o + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
        buf[o + 4..o + 8].copy_from_slice(&flags.to_le_bytes());
        buf[o + 8..o + 16].copy_from_slice(&off.to_le_bytes());
        buf[o + 16..o + 24].copy_from_slice(&va.to_le_bytes());
        buf[o + 24..o + 32].copy_from_slice(&va.to_le_bytes());
        buf[o + 32..o + 40].copy_from_slice(&fsz.to_le_bytes());
        buf[o + 40..o + 48].copy_from_slice(&msz.to_le_bytes());
        buf[o + 48..o + 56].copy_from_slice(&PAGE_SIZE.to_le_bytes());
    };

    write_phdr(&mut img, 0, PF_R | PF_X, 0x1000, 0x40_1000, 0x20, 0x20);
    write_phdr(&mut img, 1, PF_R, 0x2000, 0x40_2000, 0x800, 0x800);
    write_phdr(&mut img, 2, PF_R | PF_W, 0x2800, 0x40_2800, 0x100, 0xa00);

    img[0x1000..0x1004].copy_from_slice(&[0x90, 0x90, 0x90, 0xc3]);
    img[0x2000..0x2008].copy_from_slice(b"RO_PART!");
    img[0x2800..0x2808].copy_from_slice(b"RW_PART!");
    img
}

crate::ktest!(
    fn lazy_zero_fill_and_unmap() {
        use crate::user::{CopyError, copy_from_user, copy_to_user};
        use crate::{kassert, kassert_eq};
        use alloc::sync::Arc;

        // Pre-warm heap slightly so BTreeMap/Vec allocations below reuse
        // existing heap pages and frame accounting stays exact.
        let _warm: alloc::vec::Vec<u8> = alloc::vec![0u8; 16 * 1024];
        drop(_warm);

        let elf_img = build_relro_test_elf();
        let free_before = frame::free_frames();

        // 1. Demand paging, zero-fill, PROT_NONE frame preservation, munmap, brk.
        {
            let space = Arc::new(AddressSpace::new().map_err(|_| "AddressSpace::new failed")?);
            let _active = crate::sched::UserSpaceGuard::enter(space.clone());
            kassert_eq!(arch::paging::active_root(), space.root());

            // Lazy anonymous mmap (3 pages RW) allocates NO leaf frames yet.
            let frames_before_mmap = frame::free_frames();
            let base = space
                .mmap_anon(
                    0,
                    3 * PAGE_SIZE,
                    Prot::READ | Prot::WRITE,
                    u64::from(general::MAP_PRIVATE | general::MAP_ANONYMOUS),
                )
                .map_err(|_| "mmap_anon failed")?;
            kassert_eq!(frame::free_frames(), frames_before_mmap);
            kassert!(matches!(
                paging::walk_user(space.root(), VirtAddr::new(base).unwrap()).end(),
                WalkEnd::NotPresent(_)
            ));

            // First read triggers real #PF demand paging -> zero-filled frame.
            let mut buf = [0xAAu8; 32];
            kassert_eq!(copy_from_user(&mut buf, base), Ok(()));
            kassert_eq!(buf, [0u8; 32]);
            kassert!(paging::walk_user(space.root(), VirtAddr::new(base).unwrap()).mapping().is_some());
            // Page 1 is still unmapped (lazy).
            kassert!(matches!(
                paging::walk_user(space.root(), VirtAddr::new(base + PAGE_SIZE).unwrap()).end(),
                WalkEnd::NotPresent(_)
            ));

            // Write sentinel data to page 0 and page 1 (page 1 demand-pages on write).
            let sentinel0 = b"lumen-protnone-preserve-01234567";
            let sentinel1 = b"second-page-sentinel-89abcdef012";
            kassert_eq!(copy_to_user(base, sentinel0), Ok(()));
            kassert_eq!(copy_to_user(base + PAGE_SIZE, sentinel1), Ok(()));
            let phys0 =
                paging::walk_user(space.root(), VirtAddr::new(base).unwrap()).mapping().ok_or("page 0 missing")?.phys;

            // mprotect(PROT_NONE) on page 0 preserves physical frame while faulting on access.
            space.mprotect(base, PAGE_SIZE, Prot::NONE).map_err(|_| "mprotect PROT_NONE")?;
            let pn = paging::walk_user(space.root(), VirtAddr::new(base).unwrap())
                .prot_none_mapping()
                .ok_or("expected ProtNone PTE")?;
            kassert_eq!(pn.phys, phys0);
            let frames_before_pn_fault = frame::free_frames();
            kassert_eq!(copy_from_user(&mut buf, base), Err(CopyError::Fault));
            kassert_eq!(copy_to_user(base, sentinel0), Err(CopyError::Fault));
            kassert_eq!(frame::free_frames(), frames_before_pn_fault);

            // mprotect back to RW restores the SAME physical frame and preserves data.
            space.mprotect(base, PAGE_SIZE, Prot::READ | Prot::WRITE).map_err(|_| "mprotect RW")?;
            let m0 =
                paging::walk_user(space.root(), VirtAddr::new(base).unwrap()).mapping().ok_or("page 0 not restored")?;
            kassert_eq!(m0.phys, phys0);
            kassert_eq!(copy_from_user(&mut buf, base), Ok(()));
            kassert_eq!(&buf, sentinel0);

            // mprotect to PROT_READ allows read, rejects write (P == 1 protection fault).
            space.mprotect(base + PAGE_SIZE, PAGE_SIZE, Prot::READ).map_err(|_| "mprotect RO")?;
            kassert_eq!(copy_from_user(&mut buf, base + PAGE_SIZE), Ok(()));
            kassert_eq!(&buf, sentinel1);
            kassert_eq!(copy_to_user(base + PAGE_SIZE, sentinel0), Err(CopyError::Fault));

            // Turn page 0 back to PROT_NONE and verify munmap frees BOTH Present and PTE_PROTNONE frames.
            space.mprotect(base, PAGE_SIZE, Prot::NONE).map_err(|_| "mprotect PROT_NONE 2")?;
            let frames_before_unmap = frame::free_frames();
            space.munmap(base, 3 * PAGE_SIZE).map_err(|_| "munmap failed")?;
            // 2 populated leaf frames freed (page 0 PROT_NONE + page 1 Present; page 2 never faulted).
            kassert_eq!(frame::free_frames(), frames_before_unmap + 2);
            kassert_eq!(copy_from_user(&mut buf, base), Err(CopyError::Fault));

            // brk: byte-granular growth, lazy zero-fill, shrink across page frees tail frame.
            let b0 = space.brk(0);
            kassert_eq!(space.brk(b0 + 100), b0 + 100);
            kassert_eq!(copy_from_user(&mut buf, b0), Ok(()));
            kassert_eq!(buf, [0u8; 32]);
            kassert_eq!(copy_to_user(b0, sentinel0), Ok(()));
            kassert_eq!(space.brk(b0 + PAGE_SIZE + 50), b0 + PAGE_SIZE + 50);
            kassert_eq!(copy_to_user(b0 + PAGE_SIZE, sentinel1), Ok(()));
            let frames_before_shrink = frame::free_frames();
            kassert_eq!(space.brk(b0 + 100), b0 + 100);
            kassert_eq!(frame::free_frames(), frames_before_shrink + 1);
            kassert_eq!(copy_from_user(&mut buf, b0), Ok(()));
            kassert_eq!(&buf, sentinel0);
            kassert_eq!(copy_from_user(&mut buf, b0 + PAGE_SIZE), Err(CopyError::Fault));
            // Invalid brk below initial returns unchanged current break.
            kassert_eq!(space.brk(b0 - 16), b0 + 100);

            drop(_active);
            space.teardown();
        }
        kassert_eq!(arch::paging::active_root(), paging::kernel_root());
        kassert_eq!(frame::free_frames(), free_before);

        // 2. Static ELF64 load with RELRO shared boundary page, .bss zero-fill,
        //    lazy 8 MiB stack, and malformed ELF rejection with zero frame leaks.
        {
            let bad_res = AddressSpace::from_elf(b"not an elf", &[], &[], b"bad", [0; 16]);
            kassert!(matches!(bad_res, Err(UserVmError::Elf(ElfLoadError::Truncated))));
            kassert_eq!(frame::free_frames(), free_before);

            let (space, prog) = AddressSpace::from_elf(&elf_img, &[b"/test"], &[b"K=V"], b"/test", [0x5a; 16])
                .map_err(|_| "from_elf failed")?;
            kassert_eq!(prog.entry, 0x40_1000);
            kassert_eq!(prog.rsp % 16, 0);
            kassert_eq!(prog.initial_brk, 0x40_4000);

            // Top stack page is eagerly mapped; lower stack pages are lazy (NotPresent).
            kassert!(
                paging::walk_user(space.root(), VirtAddr::new(USER_STACK_TOP - PAGE_SIZE).unwrap()).mapping().is_some()
            );
            kassert!(matches!(
                paging::walk_user(space.root(), VirtAddr::new(USER_STACK_TOP - 2 * PAGE_SIZE).unwrap()).end(),
                WalkEnd::NotPresent(_)
            ));

            let space = Arc::new(space);
            let _active = crate::sched::UserSpaceGuard::enter(space.clone());

            // Verify RELRO shared page preserved both RO and RW segment bytes and is writable.
            let mut ro_bytes = [0u8; 8];
            let mut rw_bytes = [0u8; 8];
            let mut bss_bytes = [0xFFu8; 16];
            kassert_eq!(copy_from_user(&mut ro_bytes, 0x40_2000), Ok(()));
            kassert_eq!(&ro_bytes, b"RO_PART!");
            kassert_eq!(copy_from_user(&mut rw_bytes, 0x40_2800), Ok(()));
            kassert_eq!(&rw_bytes, b"RW_PART!");
            kassert_eq!(copy_from_user(&mut bss_bytes, 0x40_3000), Ok(()));
            kassert_eq!(bss_bytes, [0u8; 16]);
            // Code page is read-only/executable (not writable).
            kassert_eq!(copy_to_user(0x40_1000, b"nope"), Err(CopyError::Fault));
            // Demand-page a lower stack page.
            kassert_eq!(copy_from_user(&mut bss_bytes, USER_STACK_TOP - 4 * PAGE_SIZE), Ok(()));
            kassert_eq!(bss_bytes, [0u8; 16]);

            drop(_active);
            space.teardown();
        }
        kassert_eq!(arch::paging::active_root(), paging::kernel_root());
        kassert_eq!(frame::free_frames(), free_before);
        Ok(())
    }
);
