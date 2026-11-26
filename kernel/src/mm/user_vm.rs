//! Per-process user address spaces (`AddressSpace`), ELF64 segment loading,
//! anonymous and file-backed VM operations (`mmap`/`munmap`/`mprotect`/`brk`),
//! demand paging (zero-fill, the page cache's pages, copy-on-write), and safe
//! teardown.
//!
//! See `docs/ARCHITECTURE.md` §5 ("Per-process address spaces and user VM")
//! and §8 ("Static ELF64 loader and initial stack").

use alloc::collections::BTreeMap;
use alloc::sync::{Arc, Weak};
#[cfg(feature = "ktest")]
use alloc::vec;
use alloc::vec::Vec;
use core::fmt;
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use lumen_elf::loader::{ElfLoadError, LoadedElf, MappedRange, PageAction, Perm};
use lumen_elf::stack::{AuxvInfo, InitialStack, StackBuildError, build_initial_stack};
use lumen_mm::paging::{Attrs, MapError, WalkEnd};
use lumen_mm::{FRAME_SIZE, PhysFrame, VirtAddr};
use lumen_vfs::{AccessMode, DevId, Ino, OpenFile, OpenGuard, VfsError, VfsInode, VfsLock};
use lumen_vm::maps::ImageRange;
use lumen_vm::{
    Backing, Brk, FileMapping, MMAP_TOP, MapPlace, PAGE_SIZE, Prot, USER_MAX, USER_MIN, USER_STACK_TOP, VmError, Vma,
    VmaMap,
};

use super::tlb::{self, Shoot};
use super::{frame, paging};
use crate::arch;
use crate::linux::{errno, general};
use crate::sync::{SleepMutex, SpinLock};
use crate::vfs::page_cache::{self, CachePage, WritableMap};
use crate::vfs::{MappedPage, mmap_page};

// The initial user stack's size and bottom live in `lumen_vm`, next to
// `USER_STACK_TOP`. USER_STACK_BOTTOM is only the default: maps labels
// each address space's [stack_bottom, USER_STACK_TOP) as `[stack]`.
use lumen_vm::USER_STACK_BOTTOM;

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

/// One ELF image to map: its header plan, the source to stream, and
/// its optional file identity, which names the image's `/proc/<pid>/maps` lines.
/// PT_INTERP contents must separately pass `validate_interp` before loading
/// an interpreter.
#[derive(Clone, Copy)]
pub struct ElfImage<'a> {
    pub plan: &'a LoadedElf,
    pub source: &'a ImageSource<'a>,
    pub file: Option<&'a ImageFile>,
}

/// An executable file held open, or bytes supplied by a boot module/test image.
pub enum ImageSource<'a> {
    File(OpenGuard),
    Bytes(alloc::borrow::Cow<'a, [u8]>),
}

impl ImageSource<'_> {
    /// The current source length; files may shrink after this snapshot.
    pub fn len(&self) -> Result<u64, VfsError> {
        match self {
            Self::File(file) => Ok(file.inode().stat()?.size),
            Self::Bytes(bytes) => Ok(bytes.len() as u64),
        }
    }

    /// Read the entire requested range, returning EIO on a short read.
    pub fn read_exact(&self, offset: u64, bytes: &mut [u8]) -> Result<(), VfsError> {
        match self {
            Self::File(file) => {
                if file.inode().read_at(offset, bytes)? != bytes.len() {
                    return Err(VfsError::Io);
                }
            }
            Self::Bytes(source) => copy_image_bytes(source, offset, bytes)?,
        }
        Ok(())
    }
}

fn copy_image_bytes(source: &[u8], offset: u64, bytes: &mut [u8]) -> Result<(), VfsError> {
    let end = offset.checked_add(bytes.len() as u64).ok_or(VfsError::Io)?;
    let from = source.get(offset as usize..end as usize).ok_or(VfsError::Io)?;
    bytes.copy_from_slice(from);
    Ok(())
}

/// Which parts of a program's layout collide ([`UserVmError::Layout`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LayoutClash {
    /// The main image reaches the user stack or its guard page.
    MainOnStack,
    /// The interpreter is not wholly below the user stack's guard page.
    InterpNotBelowStack,
    /// The main image and its interpreter overlap.
    ImagesOverlap,
}

impl fmt::Display for LayoutClash {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            LayoutClash::MainOnStack => "the binary reaches the user stack or its guard page",
            LayoutClash::InterpNotBelowStack => "the interpreter is not below the user stack's guard page",
            LayoutClash::ImagesOverlap => "the binary and its interpreter overlap",
        })
    }
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
    /// A file-backed page that can't be had: wholly past the file's end, its
    /// disk read failed (`ext4` logs that, naming the device, inode and
    /// page), or the page cache refused it for any other reason than a
    /// missing frame (a quota or the `ext4` hard limit, `NoSpc`; an unlinked
    /// inode, `NoEnt`; a `WARN` once per streak for a ring-3 fault, from
    /// `file_page`). A ring-3
    /// fault ends the process with `UserExit::BusError` (`SIGBUS` from M8);
    /// a kernel user copy fails with `-EFAULT`.
    BusError,
}

/// Error returned by [`AddressSpace`] construction, ELF loading, or VM calls.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UserVmError {
    OutOfMemory,
    Elf(ElfLoadError),
    Stack(StackBuildError),
    /// The program's images and stack collide (`EEXIST`, as Linux's
    /// `MAP_FIXED_NOREPLACE` ELF mappings).
    Layout(LayoutClash),
    Vm(VmError),
    InvalidArg,
    /// `mmap` of a file with no pages to map: anything but a regular `ext4`
    /// or `tmpfs` file.
    NoDev,
    ImageRead(VfsError),
}

impl From<ElfLoadError> for UserVmError {
    fn from(error: ElfLoadError) -> Self {
        if error == ElfLoadError::NoMemory { Self::OutOfMemory } else { Self::Elf(error) }
    }
}

impl UserVmError {
    /// Corresponding Linux `errno` value for syscall returns.
    pub const fn to_errno(self) -> u32 {
        match self {
            UserVmError::OutOfMemory | UserVmError::Vm(VmError::NoMem) => errno::ENOMEM,
            UserVmError::Vm(VmError::Exists) | UserVmError::Layout(_) => errno::EEXIST,
            UserVmError::Vm(VmError::Access) => errno::EACCES,
            UserVmError::Vm(VmError::Misaligned | VmError::InvalidRange) | UserVmError::InvalidArg => errno::EINVAL,
            UserVmError::Elf(_) => errno::ENOEXEC,
            UserVmError::Stack(_) => errno::E2BIG,
            UserVmError::NoDev => errno::ENODEV,
            UserVmError::ImageRead(e) => e.to_errno(),
        }
    }

    /// Linux `mprotect(2)` `errno` mapping: out-of-range `[addr, addr + len)`
    /// (`VmError::InvalidRange`) returns `ENOMEM`, while misaligned `addr`
    /// remains `EINVAL`.
    pub const fn to_mprotect_errno(self) -> u32 {
        match self {
            UserVmError::Vm(VmError::InvalidRange) => errno::ENOMEM,
            other => other.to_errno(),
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
            UserVmError::Layout(c) => write!(f, "layout error: {c}"),
            UserVmError::Stack(e) => write!(f, "initial stack error: {e}"),
            UserVmError::Vm(e) => write!(f, "VM error: {e}"),
            UserVmError::InvalidArg => f.write_str("invalid argument"),
            UserVmError::NoDev => f.write_str("the file can't be mapped"),
            UserVmError::ImageRead(e) => write!(f, "executable read: {e}"),
        }
    }
}

/// Convert `lumen_vm::Prot` to `lumen_mm::paging::Attrs` for a user page.
#[inline]
pub const fn prot_to_attrs(prot: Prot) -> Attrs {
    Attrs::for_user(prot.read(), prot.write(), prot.exec())
}

/// Next populated leaf, including PROT_NONE, skipping missing subtrees.
/// Callers hold the space's Lock 1 throughout the walk and any mutation.
fn next_user_leaf(root: PhysFrame, addr: &mut u64, end: u64) -> Option<(VirtAddr, lumen_mm::paging::Mapping)> {
    while *addr < end {
        let va = VirtAddr::new(*addr).expect("user address");
        match paging::walk_user(root, va).end() {
            WalkEnd::Mapped(mapping) | WalkEnd::ProtNone(mapping) => {
                *addr += PAGE_SIZE;
                return Some((va, mapping));
            }
            WalkEnd::NotPresent(level) => {
                let span = level.span();
                *addr = (*addr & !(span - 1)).saturating_add(span).min(end);
            }
            WalkEnd::Unreadable(level, frame) => panic!("unreadable user table {frame} at {level:?} for {va}"),
        }
    }
    None
}

fn map_error_vm(err: MapError) -> UserVmError {
    match err {
        MapError::OutOfMemory => UserVmError::OutOfMemory,
        _ => UserVmError::InvalidArg,
    }
}

/// Map a counted reference, releasing it on error and transferring it to
/// the leaf only on success. `PTE_SHARED` is reserved for uncounted cache leaves.
///
/// # Safety
/// The caller holds the root's address-space lock and meets map_user_page's
/// root, address and private-write exclusion requirements.
unsafe fn map_user_frame(
    root: PhysFrame,
    va: VirtAddr,
    reference: frame::UserFrame,
    attrs: Attrs,
) -> Result<(), MapError> {
    assert!(!attrs.shared(), "a counted leaf cannot be a page-cache leaf");
    // SAFETY: caller holds Lock 1 and supplies valid root/address/rights;
    // the token keeps the frame allocated until the leaf takes it.
    unsafe { paging::map_user_page(root, va, reference.frame(), attrs) }?;
    let _ = reference.into_raw();
    Ok(())
}

/// Map a fresh zero-filled counted page, releasing it if mapping fails.
///
/// # Safety
/// The caller meets map_user_frame's locking and mapping requirements.
unsafe fn map_zeroed_user_page(root: PhysFrame, va: VirtAddr, attrs: Attrs) -> Result<(), MapError> {
    let reference = frame::alloc_user().ok_or(MapError::OutOfMemory)?;
    // SAFETY: caller supplies the required lock, root, address and rights.
    unsafe { map_user_frame(root, va, reference, attrs) }
}

/// Translate allocation failure; any other mapping error violates fork's
/// missing-leaf/user-attributes invariant and panics with the original error.
fn fork_map_error(err: MapError) -> UserVmError {
    match err {
        MapError::OutOfMemory => UserVmError::OutOfMemory,
        err => panic!("fork maps a missing lower-half leaf with valid user attributes: {err:?}"),
    }
}

/// Snapshot sizes while Lock 1 keeps every mapped-file reference live.
fn file_sizes(inner: &AddressSpaceInner) -> Result<BTreeMap<u64, u64>, UserVmError> {
    inner
        .mapped_files
        .iter()
        .map(|(&id, file)| {
            file.inode().stat().map(|stat| (id, stat.size)).map_err(|err| {
                log::error!(target: "mm", "fork stat failed for mapped file {id}: {err:?}");
                UserVmError::OutOfMemory
            })
        })
        .collect()
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
    stack_bottom: u64,
    vmas: VmaMap,
    brk: Brk,
    torn_down: bool,
    /// An open reference and mappers-list registration on each file some VMA
    /// maps, keyed by its `FileMapping::file_id` ([`file_id_of`]): kept while
    /// any VMA (or a piece of one split by `mprotect` or `munmap`) references
    /// it, so closing the descriptor after `mmap` keeps the mapping valid.
    /// Entries removed are dropped only after `inner` is released, which
    /// unregisters the space from the inode's [`Mappers`] list.
    mapped_files: BTreeMap<u64, Mapped>,
    /// The ELF images' file-backed parts, loaded by copying into anonymous
    /// VMAs, as `/proc/<pid>/maps` names them (recorded by
    /// `commit_loaded_elf` with the pages they name): trimmed wherever their
    /// VMAs are unmapped or replaced, so a later mapping in their place
    /// isn't named after them.
    images: Vec<ImageRange<Arc<ImageFile>>>,
    /// The cache page under every `PTE_SHARED` leaf (present or
    /// `PTE_PROTNONE`), keyed by its page-aligned user address: the pin that
    /// keeps the page cached, and its frame allocated, while the PTE maps
    /// it. Released after the PTE is cleared and the TLB flushed.
    pinned_pages: BTreeMap<u64, Arc<CachePage>>,
    /// The [`WritableMap`] of every `PRESENT | WRITABLE | PTE_SHARED` leaf,
    /// keyed like `pinned_pages`: a page's `writable_maps` counts exactly
    /// these. Each is dropped (uncounting it) only after its PTE is cleared
    /// or made read-only and the TLB flushed (ARCHITECTURE section 5's
    /// unmap order).
    writable_maps: BTreeMap<u64, WritableMap>,
    /// The file and error of the last file-fault failure warned about, so a
    /// streak of the same failure warns once (`file_page`).
    fault_warned: Option<(u64, VfsError)>,
}

/// How a mapping shares writes (`mmap(2)`'s `MAP_SHARED` or
/// `MAP_PRIVATE`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Sharing {
    /// `MAP_PRIVATE`: copy-on-write; may always be made writable.
    Private,
    /// `MAP_SHARED`: anonymous pages share writes across fork. For file
    /// mappings, stores reach the file if the [`MappedFile`] it maps
    /// may write it (`VM_MAYWRITE`, not `PROT_WRITE`: its description is
    /// open `O_RDWR`). Without that the mapping is shared only as Linux's
    /// without `VM_SHARED` is: `PROT_WRITE` is refused, at `mmap` and at
    /// `mprotect`, and `msync` writes nothing back for it.
    Shared,
}

/// The `FileMapping::file_id` of a file mapped from `inode`: its `lock_id`,
/// unique among all inodes of every filesystem and fixed for its lifetime
/// (unlike `ino`, which a detached unlink changes). [`invalidate_file_mappings`]
/// names the file to [`AddressSpace::invalidate_file_from`] by it.
pub fn file_id_of(inode: &dyn VfsInode) -> u64 {
    inode.lock_id()
}

/// Multiset of address spaces currently mapping one file (ARCHITECTURE
/// section 5, B7; section 9 level 10c leaf spinlock, never acquired under
/// the inode's `pages` lock).
///
/// Entries are owned by [`Mapped`] values inside each space's `mapped_files`.
/// Keeping a multiset (register always pushes; drop removes one matching
/// entry via `Weak::ptr_eq`) ensures a `Mapped` dropped after Lock 1 is
/// released can never erase a fresh registration from a concurrent `mmap_file`
/// of the same file. Dead `Weak`s are pruned at each registration.
pub struct Mappers(SpinLock<Vec<Weak<AddressSpace>>>);

impl Mappers {
    pub const fn new() -> Self {
        Self(SpinLock::new(Vec::new()))
    }

    fn register(&self, space: &Arc<AddressSpace>) {
        let mut list = self.0.lock();
        list.retain(|w| w.strong_count() > 0);
        list.push(Arc::downgrade(space));
    }

    fn unregister_one(&self, space: &Weak<AddressSpace>) {
        let mut list = self.0.lock();
        let found = list.iter().position(|w| Weak::ptr_eq(w, space));
        debug_assert!(
            found.is_some() || space.strong_count() == 0,
            "unregister_one called for a live space absent from Mappers"
        );
        if let Some(idx) = found {
            list.swap_remove(idx);
        }
        if list.is_empty() {
            // Release the backing buffer once no space maps the file.
            *list = Vec::new();
        }
    }

    fn snapshot(&self) -> Vec<Arc<AddressSpace>> {
        let weaks = self.0.lock().clone();
        weaks.into_iter().filter_map(|w| w.upgrade()).collect()
    }

    #[cfg(feature = "ktest")]
    pub(crate) fn len(&self) -> usize {
        self.0.lock().len()
    }
}

/// Unmap every page of `inode` at or past `align_up_page(new_size)` in every
/// address space currently mapping it (ARCHITECTURE section 5, B7).
///
/// Must be called once the inode's truncate has returned, with no VFS or
/// address-space lock held and interrupts enabled. Snapshots `inode`'s
/// [`Mappers`] list (upgrading each `Weak` outside the list's spinlock) and
/// calls [`AddressSpace::invalidate_file_from`] on each space in turn, each
/// under its own Lock 1 with its own cross-CPU TLB shootdown. Returns the
/// total number of unmapped leaves across all spaces.
pub fn invalidate_file_mappings(inode: &dyn VfsInode, new_size: u64) -> usize {
    invalidate_file_mappings_with(inode, new_size, || {})
}

fn invalidate_file_mappings_with(inode: &dyn VfsInode, new_size: u64, after_snapshot: impl FnOnce()) -> usize {
    let spaces = page_cache::page_cache_mappers(inode).map_or_else(Vec::new, Mappers::snapshot);
    after_snapshot();
    let file_id = file_id_of(inode);
    spaces.into_iter().map(|space| space.invalidate_file_from(file_id, new_size)).sum()
}

/// A file description as [`AddressSpace::mmap_file`] maps it: what the
/// address space keeps of the file (a [`KeptFile`]: its open reference and
/// path) and the description's shared-write right (`VM_MAYWRITE`), which
/// `mmap_file` copies into the new VMA's `FileMapping::may_write` and
/// keeps nowhere else.
pub struct MappedFile {
    kept: KeptFile,
    /// The description is open `O_RDWR` and its mount was writable when
    /// mapped: a `MAP_SHARED` mapping of it may be made writable, and its
    /// stores reach the file. (`openat` refuses `O_RDWR` on a read-only
    /// mount with `EROFS`, but an `ext4` volume can go read-only after the
    /// open, `errors=remount-ro`, #349, so the caller says;
    /// `OpenFile::new_unchecked_flags`, which skips the open's check, is
    /// the ktests' alone.)
    may_write: bool,
}

impl MappedFile {
    /// The file open description `file` has open, by its path: a clone of
    /// the description's own open reference, so never refused and never on
    /// a freed inode. Its shared-write right is the description's, `O_RDWR`
    /// or not, while `mount_writable` (the file's volume isn't read-only
    /// now: `page_cache::mapped_read_only`). Never an `O_PATH` description:
    /// `mmap` refuses one (`EBADF`) before getting here.
    pub fn of<L: VfsLock<u64>>(file: &OpenFile<L>, mount_writable: bool) -> Self {
        debug_assert!(!file.is_path(), "a mapping of an O_PATH description");
        let kept = KeptFile { guard: file.open_guard().clone(), path: Arc::clone(&file.path) };
        Self { kept, may_write: mount_writable && file.access_mode() == AccessMode::ReadWrite }
    }
}

/// A file an address space maps, as `mapped_files` keeps one per file
/// (the first mapping's [`MappedFile`], without its right: each VMA's is
/// its own `FileMapping::may_write`): the open reference that keeps the
/// file (and the mapping) valid after its descriptor is closed and its
/// name unlinked, and the path it was opened by, which `/proc/<pid>/maps`
/// shows. Where Linux prints the file's `d_path` (canonical, following
/// renames), this is the descriptor's path as `/proc/<pid>/fd` shows it
/// (a relative open's joined to its directory's, not normalised; a later
/// rename isn't seen; an unlink is, as ` (deleted)`), and the first
/// mapping's for every later mapping of the same file while any maps it.
#[derive(Clone)]
struct KeptFile {
    guard: OpenGuard,
    path: Arc<[u8]>,
}

impl KeptFile {
    /// The mapped file's inode.
    fn inode(&self) -> &Arc<dyn VfsInode> {
        self.guard.inode()
    }
}

/// An address space's live registration on a mapped file's [`Mappers`] list,
/// paired with its [`KeptFile`] open reference. Intentionally not `Clone`:
/// copying into a forked child must go through [`Self::fork_for`], which
/// registers the child; dropping removes one matching entry from the inode's
/// mappers list (taking only the level 10c leaf spinlock, never a sleeping
/// lock).
struct Mapped {
    kept: KeptFile,
    space: Weak<AddressSpace>,
}

impl Mapped {
    fn register(kept: KeptFile, space: &Arc<AddressSpace>) -> Self {
        page_cache::page_cache_mappers(&**kept.inode()).expect("every mapped file has a mappers list").register(space);
        Self { kept, space: Arc::downgrade(space) }
    }

    fn fork_for(&self, child: &Arc<AddressSpace>) -> Self {
        Self::register(self.kept.clone(), child)
    }

    fn inode(&self) -> &Arc<dyn VfsInode> {
        self.kept.inode()
    }

    fn path(&self) -> &[u8] {
        &self.kept.path
    }

    fn guard(&self) -> &OpenGuard {
        &self.kept.guard
    }
}

impl Drop for Mapped {
    fn drop(&mut self) {
        page_cache::page_cache_mappers(&**self.kept.inode())
            .expect("every mapped file has a mappers list")
            .unregister_one(&self.space);
    }
}

/// Named test seams for [`AddressSpace::fork_copy_inner`]; no-ops in
/// production calls.
struct ForkSeams<R, S, P> {
    before_register: R,
    before_sizes: S,
    after_precount: P,
}

/// A file as `/proc/<pid>/maps` names an ELF image's lines: its device,
/// inode number and path when it was loaded. A snapshot, holding no
/// reference: unlike a file mapping's, an image's lines don't show
/// ` (deleted)` once the file is unlinked, and keep the inode number it
/// had (Linux maps the segments from the file, so it shows both). The raw
/// canonical path is escaped only when maps text is formatted; later renames
/// can leave this snapshot stale.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImageFile {
    pub dev: DevId,
    pub ino: Ino,
    pub path: Arc<[u8]>,
}

impl ImageFile {
    /// `inode`, read from `path`, as its maps lines name it.
    pub fn of(inode: &dyn VfsInode, path: Arc<[u8]>) -> Self {
        Self { dev: inode.dev_id(), ino: inode.ino(), path }
    }
}

/// Where `mmap(2)`'s `flags` and `hint` place a new mapping:
/// `MAP_FIXED_NOREPLACE` before `MAP_FIXED` (Linux sets both for the
/// former), else `hint` as a hint (0: none). The anonymous and file paths
/// place alike through this.
pub fn map_place(hint: u64, flags: u64) -> MapPlace {
    if (flags & u64::from(general::MAP_FIXED_NOREPLACE)) != 0 {
        MapPlace::FixedNoReplace(hint)
    } else if (flags & u64::from(general::MAP_FIXED)) != 0 {
        MapPlace::Fixed(hint)
    } else {
        MapPlace::Hint((hint != 0).then_some(hint))
    }
}

/// What `msync(MS_SYNC)` must write back over a range, found under the
/// address space's lock and used after it is released
/// ([`AddressSpace::msync_files`]).
pub struct MsyncFiles {
    /// An open reference on each file a `MAP_SHARED` VMA in the range maps,
    /// once each.
    pub shared: Vec<OpenGuard>,
    /// Whether part of the range is mapped by no VMA (`ENOMEM`, after the
    /// mapped parts are written back, as Linux).
    pub hole: bool,
}

/// A VMA's file missing from `mapped_files` was logged by `format_maps`.
static MAPS_MISS_WARNED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Record the pages of each `PT_LOAD` segment of `plan` that hold its file
/// bytes as `file`'s, for `/proc/<pid>/maps` (Linux maps the segments from
/// the file, so its lines show the file; Lumen copies them into anonymous
/// memory, so names them by range: [`ImageRange::from_segment`]). A
/// segment `from_segment` refuses, one whose file offset and address lie at
/// different offsets in their pages (which Linux couldn't map) or whose end
/// overflows, is left unnamed, logged at `debug`.
fn name_image_locked(images: &mut Vec<ImageRange<Arc<ImageFile>>>, plan: &LoadedElf, file: &ImageFile) {
    let file = Arc::new(file.clone());
    for seg in plan.segments() {
        let (vaddr, filesz, offset) = (seg.vaddr, seg.filesz as u64, seg.file_offset as u64);
        match ImageRange::from_segment(vaddr, filesz, offset, Arc::clone(&file)) {
            Some(range) => images.push(range),
            None if filesz > 0 => log::debug!(
                target: "mm",
                "maps: {}'s segment at {vaddr:#x} (file offset {offset:#x}, {filesz:#x} bytes) left unnamed",
                lumen_vfs::Escaped(&file.path)
            ),
            None => {}
        }
    }
}

/// Remove every entry of `map` keyed in `[start, end)` (dropping it), with
/// no allocation.
fn drop_range<V>(map: &mut BTreeMap<u64, V>, start: u64, end: u64) {
    while let Some(&va) = map.range(start..end).next().map(|(va, _)| va) {
        map.remove(&va);
    }
}

/// Per-process 4-level x86-64 user address space.
///
/// Owns a dedicated lower-half page-table hierarchy (`L4[0..256]`) sharing
/// kernel `L4[256..512]` (including the 6 pre-allocated dynamic kernel L3
/// tables), a [`VmaMap`], and a byte-granular [`Brk`] cursor, all synchronized
/// under a sleeping, interrupt-enabled [`SleepMutex`].
pub struct AddressSpace {
    /// Processes using this space, independent of observer `Arc`s. A new
    /// space starts at one; whoever drops the last user (`drop_user`, after
    /// detaching its CPU) tears it down. Threads of one process do not
    /// raise it.
    users: AtomicU32,
    /// Bitmask of CPUs (bit `cpu`) that currently have this space's root
    /// loaded in `CR3`. Set before `write_cr3`, cleared after switching away
    /// ([`Self::switch_loaded`]).
    active: AtomicU64,
    root: PhysFrame,
    inner: SleepMutex<AddressSpaceInner>,
}

impl AddressSpace {
    /// Allocate a fresh empty user address space copying kernel `L4[256..512]`.
    pub fn new() -> Result<Self, UserVmError> {
        let root = paging::new_user_root().ok_or(UserVmError::OutOfMemory)?;
        let brk = Brk::with_limit(USER_MIN, MMAP_TOP).expect("USER_MIN..MMAP_TOP is valid");
        Ok(Self {
            users: AtomicU32::new(1),
            active: AtomicU64::new(0),
            root,
            inner: SleepMutex::new(AddressSpaceInner {
                stack_bottom: USER_STACK_BOTTOM,
                vmas: VmaMap::new(),
                brk,
                torn_down: false,
                mapped_files: BTreeMap::new(),
                images: Vec::new(),
                pinned_pages: BTreeMap::new(),
                writable_maps: BTreeMap::new(),
                fault_warned: None,
            }),
        })
    }

    /// Bitmask of CPUs that currently have this address space loaded in `CR3`.
    #[inline]
    pub(crate) fn active_mask(&self) -> u64 {
        self.active.load(Ordering::SeqCst)
    }

    /// Switch this CPU's `CR3` to `next`'s root (or `kernel_root()` if `None`),
    /// updating [`Self::active`] and `PerCpu::loaded_space` in shootdown-safe
    /// order: set this CPU's bit in `next` before `write_cr3`, and clear it in
    /// the previously recorded space after `write_cr3`.
    ///
    /// Skips the `CR3` reload when the recorded space is unchanged, so two
    /// threads sharing one address space keep the bit set.
    ///
    /// # Safety
    /// Interrupts must be disabled on this CPU. `next` (if `Some`) must stay
    /// alive until a later [`Self::switch_loaded`] on this CPU replaces or
    /// clears it (as `percpu::set_loaded_space` requires), and the space
    /// currently recorded in `PerCpu::loaded_space` (if non-null) must remain
    /// alive for the duration of this call.
    pub(crate) unsafe fn switch_loaded(next: Option<&AddressSpace>) {
        debug_assert!(!arch::irq::enabled(), "AddressSpace::switch_loaded called with interrupts enabled");
        let cpu = arch::cpu_index();
        let bit = 1u64 << cpu;
        let prev_ptr = arch::percpu::loaded_space();
        let next_ptr = next.map_or(core::ptr::null(), |s| s as *const AddressSpace);
        let target_root = next.map_or_else(paging::kernel_root, AddressSpace::root);
        if prev_ptr == next_ptr {
            debug_assert_eq!(
                arch::paging::active_root(),
                target_root,
                "CR3 out of step with PerCpu::loaded_space on cpu {cpu}"
            );
            return;
        }
        if let Some(next_space) = next {
            // Set before `write_cr3` so a concurrent `tlb::shoot_down` either
            // observes this CPU in `active` or orders its PTE writes before
            // our serializing `write_cr3`.
            next_space.active.fetch_or(bit, Ordering::SeqCst);
        }
        // SAFETY: `target_root` is either `kernel_root()` or a user L4 root
        // sharing kernel `L4[256..512]` verbatim, and this function maintains
        // `AddressSpace::active` and `PerCpu::loaded_space` around the write.
        unsafe { arch::paging::write_cr3(target_root.as_u64()) };
        // SAFETY: interrupts are disabled, `write_cr3` just loaded `next_ptr`'s
        // root, and the caller guarantees `next` stays alive until the next
        // `switch_loaded` replaces or clears it on this CPU.
        unsafe { arch::percpu::set_loaded_space(next_ptr) };
        if !prev_ptr.is_null() {
            // SAFETY: caller guarantees the previously recorded space stays
            // alive until after this switch completes.
            let prev_space = unsafe { &*prev_ptr };
            // Clear after `write_cr3` so every non-global TLB entry of
            // `prev_space` has been flushed from this CPU first.
            prev_space.active.fetch_and(!bit, Ordering::SeqCst);
        }
    }

    /// Add a process user, refusing zero so a retired space never revives.
    /// Panics on count overflow. Observer `Arc`s do not count as users.
    #[must_use = "a refused add means the space is retired; the caller must not use it"]
    pub fn add_user(&self) -> bool {
        self.users
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| {
                (n != 0).then(|| n.checked_add(1).expect("address-space users overflow"))
            })
            .is_ok()
    }

    /// Release one process user, tearing down the space at zero.
    /// The process must first detach its CPU from this space. Panics if
    /// no user remains; observers may keep the bare root alive afterwards.
    pub fn drop_user(&self) {
        let previous = self
            .users
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
            .expect("dropping an address-space user at zero");
        if previous == 1 {
            self.teardown();
        }
    }

    /// Process users, excluding observer `Arc`s (exit and the ktests).
    pub fn users(&self) -> u32 {
        self.users.load(Ordering::Acquire)
    }

    /// Parse and load a static ELF64 image into a brand-new [`AddressSpace`]
    /// (`PT_INTERP` refused, as in M6).
    ///
    /// On any validation or allocation error, leaves kernel frames unchanged.
    pub fn from_elf(
        image: &[u8],
        argv: &[&[u8]],
        envp: &[&[u8]],
        execfn: &[u8],
        random: [u8; 16],
    ) -> Result<(Self, LoadedProgram), UserVmError> {
        let plan = LoadedElf::parse(image).map_err(UserVmError::from)?;
        Self::from_images(
            ElfImage { plan: &plan, source: &ImageSource::Bytes(alloc::borrow::Cow::Borrowed(image)), file: None },
            None,
            argv,
            envp,
            execfn,
            random,
        )
    }

    /// Load a main image and, when it names one, its interpreter into a
    /// brand-new [`AddressSpace`] (ARCHITECTURE section 8, "Dynamic linking").
    ///
    /// `main` is planned by `LoadedElf::parse`, `parse_headers` or `parse_headers_with_pie_base`
    /// (`ET_DYN` at `PIE_BASE`, `ET_EXEC` where it says); `interp` by
    /// `LoadedElf::parse_interp_headers` at `INTERP_BASE`, given exactly when `main`
    /// has `PT_INTERP` (`InvalidArg` otherwise), after `validate_interp`
    /// accepts the separately read path. With an interpreter,
    /// `AT_BASE` is its load bias, `AT_PHDR`/`AT_ENTRY` stay the main image's,
    /// and the process starts at the interpreter's entry; `brk` is always the
    /// main image's. The images and the user stack with its guard page are
    /// checked to be disjoint ([`LayoutClash`]) before any frame is mapped.
    ///
    /// On any validation or allocation error, leaves kernel frames unchanged.
    pub fn from_images(
        main: ElfImage<'_>,
        interp: Option<ElfImage<'_>>,
        argv: &[&[u8]],
        envp: &[&[u8]],
        execfn: &[u8],
        random: [u8; 16],
    ) -> Result<(Self, LoadedProgram), UserVmError> {
        Self::from_images_with_stack(main, interp, argv, envp, execfn, random, lumen_vm::StackSize::DEFAULT)
    }

    /// Load with StackSize's page-rounded 128 KiB..=256 MiB reservation.
    pub fn from_images_with_stack(
        main: ElfImage<'_>,
        interp: Option<ElfImage<'_>>,
        argv: &[&[u8]],
        envp: &[&[u8]],
        execfn: &[u8],
        random: [u8; 16],
        stack_size: lumen_vm::StackSize,
    ) -> Result<(Self, LoadedProgram), UserVmError> {
        // Validate the initial stack before allocating any page-table root.
        let stack = StackPages::build(main, interp, argv, envp, execfn, random, stack_size)?;
        let space = Self::new()?;
        let prog = space.commit_loaded_elf(main, interp, &stack)?;
        Ok((space, prog))
    }

    /// Physical frame of this address space's L4 root (lock-free, immutable).
    #[inline]
    pub fn root(&self) -> PhysFrame {
        self.root
    }

    /// Copy VM metadata and counted user leaves, including `PROT_NONE`,
    /// into a new space. Leaves of private VMAs become read-only in both spaces;
    /// shared anonymous pages are populated first so untouched pages share.
    /// Page-cache leaves are never copied, nor private file copies wholly past
    /// EOF; both refault in the child. Mapping sharing is unrelated to `PTE_SHARED`,
    /// which marks an uncounted page-cache leaf.
    ///
    /// Allocation/count exhaustion and failed file-size reads return
    /// `OutOfMemory`; a torn-down parent also returns `OutOfMemory` (and
    /// asserts in debug builds). Invalid page-table states are invariant
    /// violations. Missing shared anonymous pages are counted before
    /// population; a conservative table allowance and 16-frame reserve
    /// refuse requests that would exhaust the current free-frame snapshot.
    /// Other allocations can still consume those frames after the check.
    /// On failure the child is released outside both locks, parent private
    /// leaves may remain read-only, and newly populated shared anonymous
    /// pages stay as demand-zero-equivalent pages at count one.
    pub fn fork_copy(self: &Arc<Self>) -> Result<Arc<Self>, UserVmError> {
        self.fork_copy_inner(ForkSeams { before_register: || {}, before_sizes: || {}, after_precount: || {} })
    }

    /// Run a test hook after child mapper registration, `file_sizes`, and the
    /// shared-anonymous precount, without bypassing the production check,
    /// population, or rollback paths (used for allocator contention and for
    /// B7's post-registration truncate race).
    #[cfg(feature = "ktest")]
    pub(super) fn fork_copy_after_precount(
        self: &Arc<Self>,
        after_precount: impl FnOnce(),
    ) -> Result<Arc<Self>, UserVmError> {
        self.fork_copy_inner(ForkSeams { before_register: || {}, before_sizes: || {}, after_precount })
    }

    fn fork_copy_inner<R: FnOnce(), S: FnOnce(), P: FnOnce()>(
        self: &Arc<Self>,
        seams: ForkSeams<R, S, P>,
    ) -> Result<Arc<Self>, UserVmError> {
        // Declared first: rollback drops the child (and its `Mapped` entries)
        // only after both Lock 1s are released.
        let child = Arc::new(Self::new()?);
        let parent = self.inner.lock();
        if parent.torn_down {
            debug_assert!(false, "fork_copy requires a live parent");
            return Err(UserVmError::OutOfMemory);
        }
        let mut copied = child.inner.lock();
        copied.vmas = parent.vmas.clone();
        copied.brk = parent.brk;
        copied.stack_bottom = parent.stack_bottom;
        copied.images = parent.images.clone();
        (seams.before_register)();
        // Register the child before reading sizes, while both Lock 1s exclude
        // faults and invalidation (ARCHITECTURE section 5, B7): a truncate
        // that snapshotted mappers before this registration has already set
        // the new size that `file_sizes` reads next; one snapshotting after
        // holds `child` and waits on `child.inner` until this copy finishes.
        copied.mapped_files = parent.mapped_files.iter().map(|(&id, m)| (id, m.fork_for(&child))).collect();
        (seams.before_sizes)();
        let mut populated = 0;
        let result = file_sizes(&copied).and_then(|sizes| {
            self.fork_check_shared_anon(&parent)?;
            (seams.after_precount)();
            self.fork_populate_shared_anon(&parent, &mut populated)?;
            self.fork_share_leaves(&parent, &child, &mut copied, &sizes)
        });
        tlb::shoot_down(self, Shoot::Full);
        drop(copied);
        drop(parent);
        if let Err(error) = &result
            && populated != 0
        {
            log::warn!(target: "mm", "failed fork ({error:?}) retained {populated} demand-zero shared anonymous pages");
        }
        result?;
        Ok(child)
    }

    /// Refuse eager population that would consume the free-frame snapshot,
    /// leaving 16 frames (64 KiB) for other work. Count each missing leaf and
    /// conservatively allow every intermediate table its walk could need.
    /// Lock 1 stabilizes the leaves; this check does not reserve physical RAM.
    fn fork_check_shared_anon(&self, parent: &AddressSpaceInner) -> Result<(), UserVmError> {
        const RESERVE: usize = 16;
        let budget = frame::free_frames().saturating_sub(RESERVE);
        let mut needed = 0;
        for vma in parent.vmas.iter().filter(|v| matches!(v.backing, Backing::Anonymous { shared: true })) {
            for addr in (vma.start..vma.end).step_by(PAGE_SIZE as usize) {
                let va = VirtAddr::new(addr).expect("valid VMA");
                match paging::walk_user(self.root, va).end() {
                    WalkEnd::Mapped(_) | WalkEnd::ProtNone(_) => {}
                    WalkEnd::NotPresent(level) => {
                        needed += usize::from(level.number());
                        if needed > budget {
                            return Err(UserVmError::OutOfMemory);
                        }
                    }
                    WalkEnd::Unreadable(level, frame) => panic!("unreadable user table {frame} at {level:?} for {va}"),
                }
            }
        }
        Ok(())
    }

    /// Populate missing shared anonymous leaves under parent Lock 1,
    /// recording successful pages even if a later allocation fails.
    fn fork_populate_shared_anon(&self, parent: &AddressSpaceInner, populated: &mut usize) -> Result<(), UserVmError> {
        for vma in parent.vmas.iter() {
            if matches!(vma.backing, Backing::Anonymous { shared: true }) {
                for addr in (vma.start..vma.end).step_by(PAGE_SIZE as usize) {
                    let va = VirtAddr::new(addr).expect("valid VMA");
                    match paging::walk_user(self.root, va).end() {
                        WalkEnd::Mapped(_) | WalkEnd::ProtNone(_) => {}
                        WalkEnd::NotPresent(_) => {
                            // SAFETY: parent Lock 1 is held; this missing
                            // leaf takes a freshly allocated counted reference.
                            unsafe { map_zeroed_user_page(self.root, va, prot_to_attrs(vma.prot)) }
                                .map_err(fork_map_error)?;
                            *populated += 1;
                        }
                        WalkEnd::Unreadable(level, frame) => {
                            panic!("unreadable user table {frame} at {level:?} for {va}")
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Copy counted leaves while both Lock 1 guards remain held. `_copied`
    /// proves the child's lock is held; fork_copy flushes the parent before
    /// publishing the child or releasing either guard on failure.
    fn fork_share_leaves(
        &self,
        parent: &AddressSpaceInner,
        child: &Self,
        _copied: &mut AddressSpaceInner,
        sizes: &BTreeMap<u64, u64>,
    ) -> Result<(), UserVmError> {
        let mut addr = USER_MIN;
        while let Some((va, leaf)) = next_user_leaf(self.root, &mut addr, USER_MAX) {
            let vma = parent.vmas.find(va.as_u64()).expect("every user leaf has a VMA");
            if leaf.shared {
                continue;
            }
            if let Backing::File(fm) = vma.backing
                && fm.offset + (va.as_u64() - vma.start)
                    >= *sizes.get(&fm.file_id).expect("every file VMA has a mapped_files entry and a size")
            {
                continue; // never inherit a private copy wholly past EOF
            }
            let mut attrs =
                if leaf.present { Attrs::for_user(true, leaf.writable, leaf.executable) } else { Attrs::USER_NONE };
            if !vma.backing.is_shared() {
                attrs = attrs.without_writable();
                if leaf.writable {
                    // SAFETY: parent Lock 1 excludes mutations. The full
                    // flush in fork_copy retires removed rights on success
                    // and failure, before child publication or release.
                    unsafe { paging::protect_user_page(self.root, va, attrs) }
                        .expect("walk just found the parent leaf");
                }
            }
            // SAFETY: the parent's leaf keeps a reference under Lock 1;
            // its private PTE is write-protected. The full flush follows
            // before the child is published, retiring cached write access.
            let reference = unsafe { frame::UserFrame::share_leaf(leaf.phys) }.ok_or_else(|| {
                log::error!(target: "mm", "fork reference count overflow for frame {}", leaf.phys);
                UserVmError::OutOfMemory
            })?;
            // SAFETY: child Lock 1 is held and its missing leaf takes the
            // additional counted reference.
            unsafe { map_user_frame(child.root, va, reference, attrs) }.map_err(fork_map_error)?;
        }
        Ok(())
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
        let plan = LoadedElf::parse(image).map_err(UserVmError::from)?;
        let main =
            ElfImage { plan: &plan, source: &ImageSource::Bytes(alloc::borrow::Cow::Borrowed(image)), file: None };
        let stack = StackPages::build(main, None, argv, envp, execfn, random, lumen_vm::StackSize::DEFAULT)?;
        self.commit_loaded_elf(main, None, &stack)
    }

    /// Map `main`, its interpreter if any, and the initial stack into this
    /// empty address space, starting at the interpreter's entry if there is
    /// one (else the main image's) with the main image's `brk`.
    ///
    /// The images' page ranges and the user stack with its guard page are
    /// checked to be disjoint before any frame is mapped; on any later error
    /// every image's pages and the stack's are unmapped and released.
    fn commit_loaded_elf(
        &self,
        main: ElfImage<'_>,
        interp: Option<ElfImage<'_>>,
        stack: &StackPages,
    ) -> Result<LoadedProgram, UserVmError> {
        let mut inner = self.inner.lock();
        if inner.torn_down || !inner.vmas.is_empty() {
            return Err(UserVmError::InvalidArg);
        }
        debug_assert!(inner.images.is_empty(), "image names in an address space with no VMAs");

        let main_ranges = main.plan.mapped_ranges();
        let interp_ranges = interp.map(|i| i.plan.mapped_ranges()).unwrap_or_default();
        check_layout(&main_ranges, &interp_ranges, stack.bottom)?;
        let mapped_ranges: alloc::vec::Vec<MappedRange> = main_ranges.into_iter().chain(interp_ranges).collect();

        let mut new_vmas = VmaMap::new();
        for range in &mapped_ranges {
            new_vmas.map_fixed_noreplace(range.start, range.end - range.start, perm_to_prot(range.perm))?;
        }
        new_vmas.map_fixed_noreplace(stack.bottom, USER_STACK_TOP - stack.bottom, Prot::READ | Prot::WRITE)?;

        let initial_brk = main.plan.initial_brk;
        // `brk` grows up to the mmap area, or, for an image above it, up to
        // the stack's guard page (never over it: `Brk::set` stops only at
        // VMAs, and the guard page is none), or to the top of user space.
        let brk_limit = if initial_brk <= MMAP_TOP {
            MMAP_TOP
        } else if initial_brk < stack.bottom {
            stack.bottom - PAGE_SIZE
        } else {
            USER_MAX
        };
        let new_brk = Brk::with_limit(initial_brk, brk_limit)?;

        let stack_buf = stack.buf.as_slice();
        let load_result = (|| -> Result<(), UserVmError> {
            for image in core::iter::once(main).chain(interp) {
                self.map_elf_image_locked(image)?;
            }

            // Eagerly map ONLY the top pages required by `build_initial_stack`.
            let stack_eager_start = USER_STACK_TOP - stack_buf.len() as u64;
            let mut va_addr = stack_eager_start;
            while va_addr < USER_STACK_TOP {
                let va = VirtAddr::new(va_addr).ok_or(UserVmError::InvalidArg)?;
                // SAFETY: Lock 1 is held and this is a fresh stack leaf.
                unsafe { map_zeroed_user_page(self.root, va, Attrs::USER_RW) }.map_err(map_error_vm)?;
                va_addr += PAGE_SIZE;
            }
            self.write_mapped_bytes_locked(stack_eager_start, stack_buf)?;
            Ok(())
        })();

        if let Err(err) = load_result {
            // Roll back all partially mapped pages so no frames leak.
            for range in &mapped_ranges {
                self.unmap_range_locked(&mut inner, range.start, range.end);
            }
            let stack_eager_start = USER_STACK_TOP - stack_buf.len() as u64;
            self.unmap_range_locked(&mut inner, stack_eager_start, USER_STACK_TOP);
            return Err(err);
        }

        inner.vmas = new_vmas;
        inner.brk = new_brk;
        inner.stack_bottom = stack.bottom;
        for image in core::iter::once(main).chain(interp) {
            if let Some(file) = image.file {
                name_image_locked(&mut inner.images, image.plan, file);
            }
        }
        let entry = interp.map_or(main.plan.entry, |i| i.plan.entry);
        Ok(LoadedProgram { entry, rsp: stack.initial.rsp, initial_brk })
    }

    /// Map one validated ELF image's pages (`plan.plan_pages()`), copying its
    /// file bytes from `image.source`. The caller holds `inner` and has checked
    /// the image's ranges are free; on error it unmaps what was mapped.
    fn map_elf_image_locked(&self, image: ElfImage<'_>) -> Result<(), UserVmError> {
        for action in image.plan.plan_pages() {
            match action {
                PageAction::MapZero { vaddr, page_count, perm } => {
                    let attrs = prot_to_attrs(perm_to_prot(perm));
                    for i in 0..page_count {
                        let va = VirtAddr::new(vaddr + i * PAGE_SIZE).ok_or(UserVmError::InvalidArg)?;
                        // SAFETY: Lock 1 is held and the validated ELF page is missing.
                        unsafe { map_zeroed_user_page(self.root, va, attrs) }.map_err(map_error_vm)?;
                    }
                }
                PageAction::UpdatePerm { vaddr, perm } => {
                    let va = VirtAddr::new(vaddr).ok_or(UserVmError::InvalidArg)?;
                    let attrs = prot_to_attrs(perm_to_prot(perm));
                    // SAFETY: `self.root` is owned by `self` and `inner` lock is held.
                    unsafe { paging::protect_user_page(self.root, va, attrs) }.map_err(|_| UserVmError::InvalidArg)?;
                    tlb::shoot_down(self, Shoot::Pages(va.as_u64()..va.as_u64() + PAGE_SIZE));
                }
                PageAction::CopyBytes { vaddr, file_offset, len } => {
                    let mut buffer = [0u8; PAGE_SIZE as usize];
                    for off in (0..len).step_by(buffer.len()) {
                        let count = (len - off).min(buffer.len());
                        image
                            .source
                            .read_exact((file_offset + off) as u64, &mut buffer[..count])
                            .map_err(UserVmError::ImageRead)?;
                        self.write_mapped_bytes_locked(vaddr + off as u64, &buffer[..count])?;
                    }
                }
            }
        }
        Ok(())
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
            // SAFETY: Lock 1 is held and the new VMA's leaf is missing.
            if let Err(err) = unsafe { map_zeroed_user_page(self.root, va, attrs) } {
                self.unmap_range_locked(&mut inner, start, addr);
                let _ = inner.vmas.unmap(start, aligned_len);
                return Err(map_error_vm(err));
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
    /// and `PROT_NONE`. For `MAP_FIXED`, unmaps and releases any replaced leaves and
    /// flushes the local TLB. The flags checked, [`Self::map_anon_at`] maps.
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
        self.map_anon_at(
            map_place(hint, flags),
            len,
            prot,
            if share_bits == shared_flag { Sharing::Shared } else { Sharing::Private },
        )
    }

    /// Register a lazy anonymous VMA of `len` bytes with `prot` where
    /// `place` says, sharing across fork exactly when `sharing` is Shared:
    /// `mmap_anon`, and `mmap` of `/dev/zero`.
    pub fn map_anon_at(&self, place: MapPlace, len: u64, prot: Prot, sharing: Sharing) -> Result<u64, UserVmError> {
        // Declared before `inner`, so dropped after it is released.
        let mut gone_files = Vec::new();
        let mut inner = self.inner.lock();
        if inner.torn_down {
            return Err(UserVmError::OutOfMemory);
        }
        let mapped = inner.vmas.map(place, len, prot, Backing::Anonymous { shared: sharing == Sharing::Shared })?;
        self.unmap_vmas_locked(&mut inner, &mapped.unmapped, &mut gone_files);
        Ok(mapped.start)
    }

    /// Register a lazy file-backed VMA over `[offset, offset + len)` of the
    /// file `file` holds open (`mmap(2)` of a file descriptor, whose open
    /// reference it is), shared or private as `sharing` says. A `Shared`
    /// mapping may be made writable only if `file`'s description is open
    /// `O_RDWR` ([`MappedFile::of`]): `PROT_WRITE` on one that isn't is
    /// refused (`Access`), now and at a later `mprotect`. A file with no
    /// pages to map (anything but a regular `ext4` or `tmpfs` file) is
    /// `NoDev`. `offset` must be page-aligned (`Misaligned`).
    ///
    /// It doesn't check that the descriptor is open for reading: Linux
    /// refuses `mmap` of one that isn't (`EACCES`), and the syscall checks
    /// that before calling this.
    ///
    /// Nothing is mapped until a fault ([`Self::handle_page_fault`]). Takes
    /// `self: &Arc<Self>` so it can register this space on the inode's
    /// [`Mappers`] list (via [`Mapped`]) when inserting the file's
    /// `mapped_files` entry under Lock 1, before any fault can map a page of
    /// it. The address space keeps `file`'s open reference and registration
    /// while any VMA maps the file, so closing the descriptor keeps the
    /// mapping valid; when the file is mapped already, the first mapping's
    /// reference (and so its path, for `/proc/<pid>/maps`) is the one kept
    /// ([`KeptFile`]), and `file`'s is dropped once `inner` is released.
    /// `MAP_FIXED` releases what it replaces as `munmap` does.
    pub fn mmap_file(
        self: &Arc<Self>,
        place: MapPlace,
        len: u64,
        prot: Prot,
        file: MappedFile,
        offset: u64,
        sharing: Sharing,
    ) -> Result<u64, UserVmError> {
        let MappedFile { kept, may_write } = file;
        let (shared, may_write) = match sharing {
            Sharing::Private => (false, true),
            Sharing::Shared => (true, may_write),
        };
        let inode = kept.inode();
        if !page_cache::mappable(&**inode) {
            return Err(UserVmError::NoDev);
        }
        let file_id = file_id_of(&**inode);
        let mapping = FileMapping { file_id, offset, shared, may_write };
        // Declared before `inner`, so dropped after it is released.
        let mut spare = Some(kept);
        let mut gone_files = Vec::new();
        let mut inner = self.inner.lock();
        if inner.torn_down {
            return Err(UserVmError::OutOfMemory);
        }
        let mapped = inner.vmas.map_file(place, len, prot, mapping)?;
        if let alloc::collections::btree_map::Entry::Vacant(slot) = inner.mapped_files.entry(file_id) {
            slot.insert(Mapped::register(spare.take().expect("taken once"), self));
        }
        self.unmap_vmas_locked(&mut inner, &mapped.unmapped, &mut gone_files);
        Ok(mapped.start)
    }

    /// Unmap `[addr, align_up_page(addr + len))`, releasing any populated Present
    /// or `PTE_PROTNONE` counted references and cache pins, flushing the local
    /// TLB when this root is loaded here.
    pub fn munmap(&self, addr: u64, len: u64) -> Result<(), UserVmError> {
        // Declared before `inner`, so dropped after it is released.
        let mut gone_files = Vec::new();
        let mut inner = self.inner.lock();
        if inner.torn_down {
            return Ok(());
        }
        let removed = inner.vmas.unmap(addr, len)?;
        self.unmap_vmas_locked(&mut inner, &removed, &mut gone_files);
        Ok(())
    }

    /// Unmap every page of file `file_id` ([`file_id_of`]) at or past
    /// `align_up_page(new_size)`, in every VMA mapping it: the cache's pages
    /// released and private-copy references dropped, in the unmap order. The
    /// VMAs stay, so a later access past the new end faults into
    /// [`FaultResult::BusError`]. Reached through [`invalidate_file_mappings`],
    /// once per registered address space, after `truncate`/`ftruncate`
    /// (`syscall::meta`) or `openat(O_TRUNC)` (`syscall::fd`) has returned
    /// from the inode's truncate with no VFS lock held (ARCHITECTURE section
    /// 5): a page the cache has dropped stays alive, pinned by the mapping,
    /// until this unmaps it. Returns how many leaves (present or `PROT_NONE`)
    /// were unmapped; pages never faulted in count for nothing.
    ///
    /// Retires translations across every CPU that currently has this space
    /// loaded (`tlb::shoot_down`) before dropping the removed pages' cache
    /// guards, pins, or private-copy references.
    pub fn invalidate_file_from(&self, file_id: u64, new_size: u64) -> usize {
        let Some(cut) = lumen_vm::align_up_page(new_size) else {
            return 0; // no page starts past such a size
        };
        let mut inner = self.inner.lock();
        if inner.torn_down {
            return 0;
        }
        let ranges: Vec<(u64, u64)> = inner
            .vmas
            .iter()
            .filter_map(|vma| match vma.backing {
                Backing::File(fm) if fm.file_id == file_id => {
                    let file_end = fm.offset.saturating_add(vma.len());
                    (file_end > cut).then(|| (vma.start + cut.saturating_sub(fm.offset), vma.end))
                }
                _ => None,
            })
            .collect();
        ranges.into_iter().map(|(start, end)| self.unmap_range_locked(&mut inner, start, end)).sum()
    }

    /// Atomically update protections on `[addr, align_up_page(addr + len))`,
    /// splitting/coalescing VMAs and updating any already-mapped (Present or
    /// `PTE_PROTNONE`) PTEs via `lumen_mm::paging::protect` while preserving
    /// physical frames across `Present <-> PTE_PROTNONE`.
    ///
    /// `PTE_SHARED` leaves always become read-only (the next store faults:
    /// copy-on-write, or `MAP_SHARED` dirty promotion), so each writable one
    /// loses its [`WritableMap`], dropped after the walk retires its entry on
    /// every CPU holding this space (`tlb::shoot_down`). Their pins stay,
    /// `PROT_NONE` included.
    /// Counted private leaves with multiple references also stay read-only
    /// when write access is granted; shared anonymous leaves follow `prot`.
    pub fn mprotect(&self, addr: u64, len: u64, prot: Prot) -> Result<(), UserVmError> {
        let mut inner = self.inner.lock();
        if inner.torn_down {
            return Err(UserVmError::Vm(VmError::NoMem));
        }
        // A shared mapping of a file whose volume went read-only since it was
        // made (`errors=remount-ro`, #349) can't be made writable: `EACCES`,
        // as without `VM_MAYWRITE`.
        if prot.write() {
            let end = addr.saturating_add(len);
            let read_only = inner.vmas.overlapping(addr, end).any(|vma| match vma.backing {
                Backing::File(fm) if fm.shared && fm.may_write => inner
                    .mapped_files
                    .get(&fm.file_id)
                    .is_some_and(|kept| page_cache::mapped_read_only(&**kept.inode())),
                _ => false,
            });
            if read_only {
                return Err(UserVmError::Vm(VmError::Access));
            }
        }
        inner.vmas.protect(addr, len, prot)?;
        if len == 0 {
            return Ok(());
        }
        let aligned_len = lumen_vm::align_up_page(len).expect("validated by VmaMap::protect");
        let end = addr + aligned_len;
        let attrs = prot_to_attrs(prot);
        let active = arch::paging::active_root() == self.root;
        let writable = &inner.writable_maps;
        let mut pending = PendingShoot::new();
        // SAFETY: Lock 1 excludes mutations over this validated lower-half
        // range. Narrowing changes (losing WRITABLE, gaining NX, or becoming
        // PROT_NONE) are shot down across all active CPUs before any
        // `WritableMap` guard is released; pure upgrades need only a local
        // refresh if loaded here.
        unsafe {
            paging::protect_user_range_with(
                self.root,
                addr,
                end,
                |va, mapping| {
                    let private = !inner.vmas.find(va.as_u64()).expect("protected VMA").backing.is_shared();
                    if !mapping.shared && private && frame::user_count(mapping.phys) > 1 {
                        attrs.without_writable()
                    } else {
                        attrs
                    }
                },
                |leaf, narrowed| {
                    release_order::step(&leaf, release_order::Step::Cleared);
                    if narrowed {
                        pending.add(&leaf);
                    } else if active {
                        arch::paging::invalidate(leaf.va);
                        release_order::step(&leaf, release_order::Step::Flushed);
                    }
                    note_shared_leaf(writable, &leaf);
                },
            );
        }
        pending.flush(self);
        // Every cache leaf in the range is now read-only and retired across
        // all active CPUs; release its writable-map guard while retaining its pin.
        drop_range(&mut inner.writable_maps, addr, end);
        Ok(())
    }

    /// Adjust the byte-granular program break (`brk(2)`).
    ///
    /// Returns the exact requested `addr` on success, or the unchanged current
    /// break on query (`addr == 0`) or failure. On shrink across page
    /// boundaries, unmaps/releases tail pages and flushes the local TLB.
    pub fn brk(&self, addr: u64) -> u64 {
        // Declared before `inner`, so dropped after it is released.
        let mut gone_files = Vec::new();
        let mut inner = self.inner.lock();
        if inner.torn_down || addr == 0 {
            return inner.brk.current();
        }
        let AddressSpaceInner { vmas, brk, .. } = &mut *inner;
        let change = brk.set(vmas, addr);
        self.unmap_vmas_locked(&mut inner, &change.unmapped, &mut gone_files);
        change.current
    }

    /// Whether a VMA covers an address, even when no PTE is present.
    /// Signal faults use this to distinguish MAPERR from ACCERR.
    pub fn contains_vma(&self, addr: u64) -> bool {
        self.inner.lock().vmas.find(addr).is_some()
    }

    /// Resolve a demand-paging `#PF` at `fault_addr` with CPU `error_code`.
    ///
    /// Must be called with interrupts enabled and `irq::depth() == 0` (step 3
    /// of the `#PF` interrupt protocol). Rejects unmapped addresses, `PROT_NONE`,
    /// write fault (`bit 1`) on non-writable VMA, and exec fault (`bit 4`) on
    /// non-executable VMA. What happens next depends on the leaf found under
    /// the lock, not on the error code's present bit (another fault may have
    /// mapped the page since):
    ///
    /// - **None:** an anonymous page is zero-filled with count one. A file page comes from
    ///   the inode's page cache ([`mmap_page`]): `MAP_SHARED` maps the
    ///   cache's own frame (`PTE_SHARED`), writable only for a write fault
    ///   (counted by a [`WritableMap`]); `MAP_PRIVATE` maps it read-only for
    ///   a read or exec fault, and copies it into a private writable frame
    ///   for a write fault. A page the cache can't hand out (wholly past the
    ///   file's end, unreadable, or refused: see [`FaultResult::BusError`])
    ///   is a bus error; a missing frame is `OutOfMemory`.
    /// - **Present and allowing the access:** a stale TLB entry, flushed.
    /// - **A store to a read-only `PTE_SHARED` leaf:** copy-on-write for
    ///   `MAP_PRIVATE` (a private copy replaces it), dirty promotion for
    ///   `MAP_SHARED` (made writable, counted by a [`WritableMap`]).
    /// - **A store to a read-only counted private leaf:** reuse at count
    ///   one (local `invlpg` only), otherwise copy the whole frame into a new
    ///   counted reference. The old reference is released only after
    ///   `tlb::shoot_down` retires the translation across all active CPUs.
    /// - Anything else is `Invalid`.
    ///
    /// File faults hold Lock 1 (`inner`) while [`mmap_page`] takes the
    /// inode's Locks 4 and 5 (and reads the disk), and while a
    /// copy-on-write copy takes Lock 5 (ARCHITECTURE section 9).
    pub fn handle_page_fault(&self, fault_addr: u64, error_code: u64) -> FaultResult {
        // Defensive RSVD guard (bit 3): `idt.rs` panics on RSVD before reaching
        // here, and direct callers must never map a frame for an RSVD fault.
        if (error_code & (1 << 3)) != 0 {
            return FaultResult::Invalid;
        }
        if !(USER_MIN..USER_MAX).contains(&fault_addr) {
            return FaultResult::Invalid;
        }
        // Only present (0), write (1), user (2), reserved (3) and fetch (4)
        // are modelled: a protection-key, shadow-stack or SGX fault (bits
        // 5, 6, 15) can't be resolved here, and retrying it would loop.
        if (error_code & !0x1f) != 0 {
            return FaultResult::Invalid;
        }
        let is_write = (error_code & (1 << 1)) != 0;
        let from_user = (error_code & (1 << 2)) != 0;
        let is_exec = (error_code & (1 << 4)) != 0;

        let mut inner = self.inner.lock();
        if inner.torn_down {
            return FaultResult::Invalid;
        }
        let Some(&vma) = inner.vmas.find(fault_addr) else {
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
            WalkEnd::Mapped(m) => {
                if m.user && (!is_write || m.writable) && (!is_exec || m.executable) {
                    // A stale TLB entry: the PTE already allows the access.
                    if arch::paging::active_root() == self.root {
                        arch::paging::invalidate(page_va);
                    }
                    return FaultResult::Resolved;
                }
                if m.user && is_write && !m.writable && !m.shared && !vma.backing.is_shared() {
                    return self.split_private_leaf(&mut inner, page_va, m.phys, prot_to_attrs(vma.prot));
                }
                return match vma.backing {
                    Backing::File(fm) if is_write && m.shared => {
                        self.store_to_shared_leaf(&mut inner, &vma, fm, page_va, from_user)
                    }
                    _ => FaultResult::Invalid,
                };
            }
            WalkEnd::ProtNone(_) => return FaultResult::Invalid,
            WalkEnd::NotPresent(_) => {}
            WalkEnd::Unreadable(level, frame) => {
                panic!("unreadable user page table {frame} at {level:?} for {page_va}");
            }
        }

        if let Backing::File(fm) = vma.backing {
            return match file_page(&mut inner, &vma, fm, page_va, from_user) {
                Ok(mapped) => self.map_file_page(&mut inner, &vma, fm, page_va, is_write, &mapped),
                Err(fail) => fail,
            };
        }

        // SAFETY: Lock 1 is held and this valid anonymous leaf is missing.
        if let Err(err) = unsafe { map_zeroed_user_page(self.root, page_va, prot_to_attrs(vma.prot)) } {
            return map_error_fault(err);
        }
        FaultResult::Resolved
    }

    /// Resolve a write to a counted private leaf while Lock 1 is held.
    /// The old reference is released only after its translation is retired.
    fn split_private_leaf(
        &self,
        _inner: &mut AddressSpaceInner,
        va: VirtAddr,
        old: PhysFrame,
        attrs: Attrs,
    ) -> FaultResult {
        if frame::user_count(old) == 1 {
            // SAFETY: under Lock 1 this is the sole reference, so no other
            // space can raise its count or observe private writes.
            unsafe { paging::protect_user_page(self.root, va, attrs) }.expect("existing private leaf");
            // Upgrading RO -> RW needs no cross-CPU shootdown: another CPU's
            // stale RO entry simply faults and finds the leaf writable.
            if arch::paging::active_root() == self.root {
                arch::paging::invalidate(va);
            }
            return FaultResult::Resolved;
        }
        let Some(copy) = frame::alloc_owned() else {
            return FaultResult::OutOfMemory;
        };
        // SAFETY: the old leaf pins the frame under Lock 1; while its count
        // exceeds one every holder maps it read-only, so the bytes are stable.
        // The new frame is exclusively ours, disjoint and HHDM-mapped.
        unsafe {
            core::ptr::copy_nonoverlapping(
                super::phys_to_virt(old.start()).as_ptr::<u8>(),
                super::phys_to_virt(copy.frame().start()).as_mut_ptr::<u8>(),
                PAGE_SIZE as usize,
            );
        }
        let copy = frame::UserFrame::from_owned(copy);
        let mut removed = None;
        // SAFETY: Lock 1 excludes mutations. Retain the old reference until
        // its translation is retired below; unmap retains the table frames.
        unsafe {
            paging::unmap_user_range(self.root, va.as_u64(), va.as_u64() + PAGE_SIZE, |leaf| {
                assert!(leaf.phys == old && !leaf.was_shared, "split must remove the counted old leaf");
                removed = Some(leaf);
            });
        }
        let removed = removed.expect("split must remove one leaf");
        // Break-before-make: retire the cleared read-only leaf across every
        // active CPU BEFORE mapping the writable copy, so no sibling CPU can
        // keep reading stale data through a cached old entry after stores land
        // in the copy (Linux's `wp_page_copy`).
        tlb::shoot_down(self, Shoot::Pages(va.as_u64()..va.as_u64() + PAGE_SIZE));
        // SAFETY: Lock 1 is held and the leaf is now missing. Its tables
        // remain, so replacing it cannot allocate or run out of frames.
        unsafe { map_user_frame(self.root, va, copy, attrs) }.expect("retained private leaf tables");
        // SAFETY: this is the counted leaf just removed, and `tlb::shoot_down`
        // retired it from every CPU that had this space loaded before we drop
        // its reference.
        drop(unsafe { frame::UserFrame::from_leaf(removed.phys) });
        FaultResult::Resolved
    }

    /// Map file page `mapped` at the not-present `page_va` of file VMA
    /// `vma` for a fault (`is_write` for a store): a private copy for a
    /// `MAP_PRIVATE` store, else the cache's frame itself (`PTE_SHARED`,
    /// pinned in `pinned_pages`), writable only for a `MAP_SHARED` store,
    /// whose [`WritableMap`] is taken before the PTE becomes writable.
    fn map_file_page(
        &self,
        inner: &mut AddressSpaceInner,
        vma: &Vma,
        fm: FileMapping,
        page_va: VirtAddr,
        is_write: bool,
        mapped: &MappedPage,
    ) -> FaultResult {
        if is_write && !fm.shared {
            return self.map_private_copy(page_va, mapped, prot_to_attrs(vma.prot));
        }
        let writable = is_write && fm.may_write;
        if writable && mapped.volume_read_only() {
            // The volume went read-only after the mapping was made (#349):
            // a store would never reach the disk.
            return FaultResult::BusError;
        }
        let mut attrs = prot_to_attrs(vma.prot).with_shared(true);
        if !writable {
            attrs = attrs.without_writable();
        }
        let guard = writable.then(|| mapped.map_writable());
        let page = mapped.page();
        debug_assert!(!writable || page.writable_maps() > 0, "a shared PTE made writable uncounted");
        // SAFETY: `self.root` is owned by `self`, `inner` lock is held, and
        // `page_va` is in `USER_MIN..USER_MAX` and not present. The frame
        // stays the page cache's: `PTE_SHARED` keeps every unmap and
        // teardown path from freeing it, and the pin stored in
        // `pinned_pages` below (before `mapped` lets go of its own) keeps it
        // allocated for as long as the PTE maps it.
        if let Err(err) = unsafe { paging::map_user_page(self.root, page_va, page.phys_frame(), attrs) } {
            drop(guard); // the PTE never became writable
            return map_error_fault(err);
        }
        let pinned_before = inner.pinned_pages.insert(page_va.as_u64(), Arc::clone(page));
        debug_assert!(pinned_before.is_none(), "pinned_pages out of step at {page_va}");
        if let Some(guard) = guard {
            let counted_before = inner.writable_maps.insert(page_va.as_u64(), guard);
            debug_assert!(counted_before.is_none(), "writable_maps out of step at {page_va}");
        }
        FaultResult::Resolved
    }

    /// Map a private copy of `mapped` (its bytes below the file's size,
    /// zeros after) at the not-present `page_va` with `attrs`.
    fn map_private_copy(&self, page_va: VirtAddr, mapped: &MappedPage, attrs: Attrs) -> FaultResult {
        let Some(mut copy) = frame::alloc_owned() else {
            return FaultResult::OutOfMemory;
        };
        mapped.copy_to_frame(&mut copy);
        self.map_copy(page_va, copy, attrs)
    }

    /// Map the private frame `copy` at the not-present `page_va`.
    fn map_copy(&self, page_va: VirtAddr, copy: frame::OwnedFrame, attrs: Attrs) -> FaultResult {
        let copy = frame::UserFrame::from_owned(copy);
        debug_assert_eq!(frame::user_count(copy.frame()), 1, "private copy has one counted reference");
        // SAFETY: `self.root` is owned by `self`, the caller holds `inner`,
        // `page_va` is in `USER_MIN..USER_MAX` and not present, and `copy`
        // holds the only counted reference, transferred to the leaf only
        // if mapping succeeds (otherwise the token releases it).
        match unsafe { map_user_frame(self.root, page_va, copy, attrs) } {
            Ok(()) => FaultResult::Resolved,
            Err(err) => map_error_fault(err), // the unmapped reference was released
        }
    }

    /// A store to the present, read-only `PTE_SHARED` leaf at `page_va` of
    /// file VMA `vma` (which allows stores): `MAP_PRIVATE` copy-on-write,
    /// or `MAP_SHARED` dirty promotion. The page is looked up again, for
    /// the file's size now (a copy takes the bytes below it) and to find a
    /// page the cache has let go: a shrink not yet invalidating mappings
    /// leaves the PTE on a page the file no longer has, so that mapping is
    /// dropped and the page the file has now (or `BusError`) used instead.
    fn store_to_shared_leaf(
        &self,
        inner: &mut AddressSpaceInner,
        vma: &Vma,
        fm: FileMapping,
        page_va: VirtAddr,
        from_user: bool,
    ) -> FaultResult {
        let va = page_va.as_u64();
        let mapped = match file_page(inner, vma, fm, page_va, from_user) {
            Ok(mapped) => mapped,
            Err(FaultResult::BusError) => {
                // Past the new end, or unreadable: don't keep mapping a page
                // the file may no longer have.
                self.unmap_page_locked(inner, page_va);
                return FaultResult::BusError;
            }
            Err(fail) => return fail, // out of memory: the leaf stays as it was
        };
        let current = inner.pinned_pages.get(&va).is_some_and(|pin| Arc::ptr_eq(pin, mapped.page()));
        if !fm.shared {
            // Copy-on-write: the copy is taken first (the bytes below the
            // file's size now), then the shared leaf is cleared, flushed and
            // its pin dropped, and the copy mapped private and writable.
            // The copy's source is the file's current page, which is not the
            // one the leaf maps when the cache has let that go (`!current`:
            // a truncate that didn't invalidate mappings), so the copy holds
            // what the file holds now, as a fresh fault would.
            let Some(mut copy) = frame::alloc_owned() else {
                return FaultResult::OutOfMemory;
            };
            mapped.copy_to_frame(&mut copy);
            self.unmap_page_locked(inner, page_va);
            return self.map_copy(page_va, copy, prot_to_attrs(vma.prot));
        }
        if !current {
            // A page the cache has let go: map the one the file has now.
            self.unmap_page_locked(inner, page_va);
            return self.map_file_page(inner, vma, fm, page_va, true, &mapped);
        }
        // Dirty promotion: counted before the PTE becomes writable.
        if !fm.may_write {
            log::error!(target: "mm", "a store promoted a shared page at {page_va} of a mapping without VM_MAYWRITE");
            debug_assert!(false, "VmaMap refuses PROT_WRITE without may_write");
            return FaultResult::Invalid;
        }
        if mapped.volume_read_only() {
            // The volume went read-only after the mapping was made (#349):
            // the leaf stays read-only, the store a bus error.
            return FaultResult::BusError;
        }
        let guard = mapped.map_writable();
        debug_assert!(mapped.page().writable_maps() > 0, "a shared PTE made writable uncounted");
        let attrs = prot_to_attrs(vma.prot).with_shared(true);
        // SAFETY: `self.root` is owned by `self`, `inner` lock is held, and
        // the leaf at `page_va` maps the page pinned in `pinned_pages`.
        if let Err(err) = unsafe { paging::protect_user_page(self.root, page_va, attrs) } {
            log::error!(target: "mm", "promoting the shared page at {page_va}: {err}");
            debug_assert!(false, "the walk just found the leaf at {page_va}");
            return FaultResult::Invalid; // `guard` dropped: never writable
        }
        if arch::paging::active_root() == self.root {
            arch::paging::invalidate(page_va);
        }
        let counted_before = inner.writable_maps.insert(va, guard);
        debug_assert!(counted_before.is_none(), "writable_maps out of step at {page_va}");
        FaultResult::Resolved
    }

    /// Release all user leaf references (present and `PTE_PROTNONE`) and
    /// VMAs, and free user L1/L2/L3 tables while interrupts are enabled.
    /// `drop_user` calls this at zero (a Linux process's takedown gives up
    /// its user there, #372); a space no process adopted is torn down
    /// directly. Direct teardown retires users to zero and asserts there was
    /// at most one user. Repeated calls are harmless.
    ///
    /// Must be called only after switching `CR3` back to `kernel_root()`, with
    /// interrupts enabled and no `SpinLock` held: it may block while waiting
    /// for other CPUs to clear [`Self::active`] and while acquiring Lock 1
    /// (`inner`). Subsequent [`Drop`] frees only the bare L4 root frame without
    /// locking.
    ///
    /// Cache frames remain owned by the cache (`PTE_SHARED` leaves): their
    /// [`WritableMap`]s and pins are dropped after the sweep (the switch
    /// away from this `CR3` was the unmap order's flush), and the
    /// `mapped_files` references after `inner` is released.
    pub(crate) fn teardown(&self) {
        assert_ne!(
            arch::paging::active_root(),
            self.root,
            "AddressSpace::teardown called while CR3 still holds {}",
            self.root
        );
        let here_bit = 1u64 << arch::cpu_index();
        assert_eq!(
            self.active.load(Ordering::Acquire) & here_bit,
            0,
            "AddressSpace::teardown called while this CPU is still active on {}",
            self.root
        );
        let previous = self.users.swap(0, Ordering::AcqRel);
        assert!(previous <= 1, "teardown requires at most one process user");
        // Wait until every other CPU that still has this root loaded (a thread
        // running on the space that is not a counted process user, such as a
        // sibling or ktest thread not yet detached) has switched away before
        // freeing any leaf frames, pins, or page tables.
        let mut mask = self.active.load(Ordering::Acquire);
        if mask != 0 {
            crate::crash::halt_if_stopping_elsewhere();
            debug_assert!(
                arch::irq::enabled() && arch::irq::depth() == 0,
                "AddressSpace::teardown waiting for active CPUs with interrupts off or a SpinLock held"
            );
            let mut timer = tlb::WaitTimer::new();
            while mask != 0 {
                if let Some((true, _)) = timer.poll() {
                    let root = self.root;
                    log::warn!(target: "mm", "cpus {mask:#x} still have {root} loaded: teardown waits for them");
                }
                core::hint::spin_loop();
                crate::sched::yield_now();
                mask = self.active.load(Ordering::Acquire);
            }
        }
        let gone_files;
        let mut inner = self.inner.lock();
        if inner.torn_down {
            return;
        }
        debug_assert_eq!(self.active.load(Ordering::Acquire), 0, "teardown entered while CPUs still active");
        inner.vmas = VmaMap::new();
        self.unmap_range_locked(&mut inner, USER_MIN, USER_MAX);
        // SAFETY: the root is inactive and every leaf has been removed and
        // released. Only this space's exclusively owned tables remain.
        unsafe { paging::clear_user_lower_half(self.root, |_| unreachable!("all user leaves removed")) };
        inner.writable_maps.clear();
        inner.pinned_pages.clear();
        inner.images.clear();
        gone_files = core::mem::take(&mut inner.mapped_files);
        inner.torn_down = true;
        drop(inner);
        drop(gone_files);
    }

    /// [`format_maps`](Self::format_maps) as text, for the ktests that
    /// match its lines (their paths are UTF-8).
    #[cfg(feature = "ktest")]
    pub fn format_maps_text(&self) -> alloc::string::String {
        alloc::string::String::from_utf8_lossy(&self.format_maps()).into_owned()
    }

    /// Format `/proc/<pid>/maps` strictly under Lock 1 (`AddressSpace::inner`),
    /// with no `PROCS`, process or `VfsInode` locks held. The lines themselves
    /// are `lumen_vm::maps::write_maps`'s; the files they name are resolved
    /// here: a file mapping's from `mapped_files` (its inode's device and
    /// number now, which read no lock, deleted once detached by an unlink,
    /// and the path it was opened by), an ELF image's from the ranges
    /// `commit_loaded_elf` recorded. Bytes: a path is its bytes, UTF-8 or
    /// not (#268).
    pub fn format_maps(&self) -> Vec<u8> {
        use lumen_vm::maps::{FileInfo, MapsLayout};
        let mut out = Vec::new();
        let inner = self.inner.lock();
        if inner.torn_down {
            return out;
        }
        let layout = MapsLayout {
            brk_initial: inner.brk.initial(),
            brk_mapped_end: inner.brk.mapped_end(),
            stack_bottom: inner.stack_bottom,
            stack_top: USER_STACK_TOP,
        };
        fn image_file(f: &Arc<ImageFile>) -> FileInfo<'_> {
            FileInfo { major: f.dev.major(), minor: f.dev.minor(), ino: f.ino, path: &f.path, deleted: false }
        }
        let files = |id: u64| {
            let Some(m) = inner.mapped_files.get(&id) else {
                // Every VMA's file is in `mapped_files` (`file_page` relies
                // on it too): a miss is a bug, shown as `00:00 0`.
                if !MAPS_MISS_WARNED.swap(true, core::sync::atomic::Ordering::Relaxed) {
                    log::error!(target: "mm", "maps: file {id} of a VMA isn't in mapped_files (a bug)");
                }
                debug_assert!(false, "a mapped file missing from mapped_files: {id}");
                return None;
            };
            let inode = m.inode();
            let dev = inode.dev_id();
            Some(FileInfo {
                major: dev.major(),
                minor: dev.minor(),
                ino: inode.ino(),
                path: m.path(),
                deleted: inode.is_detached(),
            })
        };
        lumen_vm::maps::write_maps(&mut out, inner.vmas.iter(), &layout, &inner.images, image_file, files);
        out
    }

    /// What `msync(2)` over page-aligned `[start, end)` (`start < end`)
    /// must write back: an open reference on each file a `MAP_SHARED` VMA
    /// with `may_write` overlapping it maps, once each (one made through a
    /// read-only descriptor stores nothing to write), and whether part of
    /// the range is
    /// mapped by no VMA. Private file mappings and anonymous memory have
    /// nothing to write.
    ///
    /// The references are cloned under Lock 1 and the caller writes back
    /// after it is released: writeback takes the inode's Locks 4 and 5 and
    /// the filesystem's and block cache's below them, which Lock 1 may be
    /// held across (ARCHITECTURE section 9's order), but holding it through
    /// disk writes would stall every fault and `mmap` of the process
    /// meanwhile, as Linux's `msync` avoids by dropping `mmap_lock` around
    /// `vfs_fsync_range`. A file unmapped meanwhile stays valid through its
    /// reference.
    pub fn msync_files(&self, start: u64, end: u64) -> MsyncFiles {
        let mut found = MsyncFiles { shared: Vec::new(), hole: false };
        let inner = self.inner.lock();
        if inner.torn_down {
            found.hole = true;
            return found;
        }
        let mut seen: Vec<u64> = Vec::new();
        let mut covered = start;
        for vma in inner.vmas.overlapping(start, end) {
            if vma.start > covered {
                found.hole = true;
            }
            covered = vma.end;
            // Shared through a descriptor open for writing: Linux's
            // `VM_SHARED`, which `do_mmap` clears without `FMODE_WRITE`, and
            // the only mappings its `msync` writes back.
            if let Backing::File(fm) = vma.backing
                && fm.shared
                && fm.may_write
                && !seen.contains(&fm.file_id)
            {
                // As `file_page`'s: every VMA's file is in `mapped_files`.
                let mapped = inner
                    .mapped_files
                    .get(&fm.file_id)
                    .expect("a mapped file stays in mapped_files while a VMA maps it");
                seen.push(fm.file_id);
                found.shared.push(mapped.guard().clone());
            }
        }
        if covered < end {
            found.hole = true;
        }
        found
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
            debug_assert_eq!(frame::user_count(phys), 1, "loader writes only fresh private pages");
            // SAFETY: callers hold Lock 1 and write only newly mapped count-1
            // private pages, never a shared frame. The HHDM maps this usable
            // RAM writable, and off + chunk <= PAGE_SIZE.
            unsafe { core::ptr::copy_nonoverlapping(bytes.as_ptr(), dst.add(off), chunk) };
            va += chunk as u64;
            bytes = &bytes[chunk..];
        }
        Ok(())
    }

    /// Unmap the PTEs of `removed` (VMAs `VmaMap` just took out) and move
    /// the open references and mappers registrations of files no VMA maps any
    /// more into `gone`.
    fn unmap_vmas_locked(&self, inner: &mut AddressSpaceInner, removed: &[Vma], gone: &mut Vec<Mapped>) {
        for vma in removed {
            self.unmap_range_locked(inner, vma.start, vma.end);
            ImageRange::trim(&mut inner.images, vma.start, vma.end);
        }
        release_unmapped_files(inner, gone);
    }

    /// [`Self::unmap_range_locked`] for the one page at `page_va`.
    fn unmap_page_locked(&self, inner: &mut AddressSpaceInner, page_va: VirtAddr) {
        self.unmap_range_locked(inner, page_va.as_u64(), page_va.as_u64() + PAGE_SIZE);
    }

    /// Unmap populated leaves in page-aligned `[start, end)`: clear each
    /// leaf, shoot down stale translations across every active CPU
    /// (`tlb::shoot_down`), then release counted references, cache writable
    /// guards, and cache pins in that order. Batches counted frames on the
    /// stack so OOM rollbacks never allocate. Returns the number of removed
    /// leaves. Every removal except the counted CoW split and Drop uses this
    /// path, including cache-leaf replacement and teardown before freeing empty
    /// page tables.
    fn unmap_range_locked(&self, inner: &mut AddressSpaceInner, start: u64, end: u64) -> usize {
        const FRAME_BATCH: usize = 64;
        let writable = &inner.writable_maps;
        let mut leaves = 0usize;
        let mut batch = [PhysFrame::from_u64(0).expect("zero is aligned"); FRAME_BATCH];
        let mut batch_len = 0usize;
        let mut pending = PendingShoot::new();
        // SAFETY: `self.root` is owned by `self`, `inner` lock is held, and
        // `[start, end)` is page-aligned and strictly in the lower half.
        // Removed counted frames are held until `tlb::shoot_down` retires their
        // translations on every active CPU; cache writable guards and pins are
        // dropped only after the final shootdown.
        unsafe {
            paging::unmap_user_range(self.root, start, end, |leaf| {
                leaves += 1;
                release_order::step(&leaf, release_order::Step::Cleared);
                note_shared_leaf(writable, &leaf);
                if leaf.was_shared {
                    pending.add(&leaf);
                    return;
                }
                if batch_len == FRAME_BATCH {
                    pending.flush(self);
                    for &phys in &batch[..batch_len] {
                        // SAFETY: removed and retired across all active CPUs above.
                        drop(frame::UserFrame::from_leaf(phys));
                    }
                    batch_len = 0;
                }
                batch[batch_len] = leaf.phys;
                batch_len += 1;
                pending.add(&leaf);
            });
        }
        pending.flush(self);
        for &phys in &batch[..batch_len] {
            // SAFETY: every leaf in `batch` was removed and shot down above.
            drop(unsafe { frame::UserFrame::from_leaf(phys) });
        }
        drop_range(&mut inner.writable_maps, start, end);
        drop_range(&mut inner.pinned_pages, start, end);
        leaves
    }

    /// How many `PTE_SHARED` pages are pinned and how many of them are
    /// counted writable, for the ktests.
    #[cfg(feature = "ktest")]
    pub(crate) fn file_page_counts(&self) -> (usize, usize) {
        let inner = self.inner.lock();
        (inner.pinned_pages.len(), inner.writable_maps.len())
    }

    /// The number of files the address space holds open for its mappings,
    /// for the ktests.
    #[cfg(feature = "ktest")]
    pub(crate) fn mapped_file_count(&self) -> usize {
        self.inner.lock().mapped_files.len()
    }
}

/// Accumulates cleared or narrowed leaves across a page-table walk so they are
/// retired with a single [`tlb::shoot_down`] call (and, under `ktest`, recorded
/// as [`release_order::Step::Flushed`] after the shootdown completes).
struct PendingShoot {
    start: u64,
    end: u64,
    #[cfg(feature = "ktest")]
    shared: [u64; 64],
    #[cfg(feature = "ktest")]
    shared_len: usize,
}

impl PendingShoot {
    fn new() -> Self {
        Self {
            start: u64::MAX,
            end: 0,
            #[cfg(feature = "ktest")]
            shared: [0; 64],
            #[cfg(feature = "ktest")]
            shared_len: 0,
        }
    }

    fn add(&mut self, leaf: &paging::UserLeaf) {
        let va = leaf.va.as_u64();
        self.start = self.start.min(va);
        self.end = self.end.max(va + PAGE_SIZE);
        #[cfg(feature = "ktest")]
        if leaf.was_shared && self.shared_len < self.shared.len() {
            self.shared[self.shared_len] = leaf.phys.as_u64();
            self.shared_len += 1;
        }
    }

    fn flush(&mut self, space: &AddressSpace) {
        if self.start < self.end {
            tlb::shoot_down(space, Shoot::Pages(self.start..self.end));
            #[cfg(feature = "ktest")]
            for &phys in &self.shared[..self.shared_len] {
                release_order::record(phys, release_order::Step::Flushed);
            }
            // Mark flushed before replacing so `Drop` on the old value passes.
            self.start = u64::MAX;
            *self = Self::new();
        }
    }
}

impl Drop for PendingShoot {
    fn drop(&mut self) {
        debug_assert!(self.start >= self.end, "PendingShoot dropped unflushed");
    }
}

/// Release a removed counted leaf's reference; cache leaves are held by pins.
///
/// # Safety
/// The leaf was removed exactly once and its translation retired from all
/// CPUs. The caller owns its reference; cache pins are released separately.
unsafe fn release_removed_leaf(leaf: &paging::UserLeaf) {
    if !leaf.was_shared {
        // SAFETY: caller owns this removed, retired counted leaf's reference.
        drop(unsafe { frame::UserFrame::from_leaf(leaf.phys) });
    }
}

/// A leaf a walk just cleared, downgraded or swept: a `PTE_SHARED` one
/// whose hardware `DIRTY` bit is set marks its page dirty through its
/// [`WritableMap`], and its guard must exist exactly when it was writable
/// (the `writable_maps` invariant).
fn note_shared_leaf(writable: &BTreeMap<u64, WritableMap>, leaf: &paging::UserLeaf) {
    if !leaf.was_shared {
        return;
    }
    let guard = writable.get(&leaf.va.as_u64());
    debug_assert_eq!(guard.is_some(), leaf.was_writable, "writable_maps out of step at {}", leaf.va);
    if leaf.was_dirty
        && let Some(guard) = guard
    {
        guard.note_pte_dirty();
    }
}

/// The page of file VMA `vma` (mapping `fm`) at `page_va`, from the page
/// cache, or the fault's outcome when there is none: `BusError` for a page
/// wholly past the file's end, a failed disk read (`ext4` logged it), or a
/// page the file's quota or the cache's hard limit refuses, or an unlinked
/// inode; `OutOfMemory` when no frame is free.
///
/// The last kind is logged here: a `WARN` for a ring-3 fault (`from_user`),
/// once per streak of the same file and error, and only a `debug` line for
/// a kernel user copy, which hands its caller `-EFAULT` (a loop of `read`s
/// into a mapped hole on a full `tmpfs` would flood the log otherwise).
///
/// The `expect` relies on `mapped_files` holding every file a VMA maps
/// (`mmap_file` adds it; `release_unmapped_files` removes it only once no
/// VMA maps it).
fn file_page(
    inner: &mut AddressSpaceInner,
    vma: &Vma,
    fm: FileMapping,
    page_va: VirtAddr,
    from_user: bool,
) -> Result<MappedPage, FaultResult> {
    let file = inner.mapped_files.get(&fm.file_id).expect("a mapped file stays in mapped_files while a VMA maps it");
    let offset = fm.offset + (page_va.as_u64() - vma.start); // `VmaMap` checked it can't overflow
    let index = offset / PAGE_SIZE;
    let result = mmap_page(file.inode(), index);
    let err = match result {
        Err(e) if !matches!(e, VfsError::NoMem | VfsError::Range | VfsError::Io) => e,
        other => {
            // Any other outcome ends this file's streak of warned failures.
            inner.fault_warned.take_if(|(id, _)| *id == fm.file_id);
            return other.map_err(|e| match e {
                VfsError::NoMem => FaultResult::OutOfMemory,
                _ => FaultResult::BusError, // `Range`, or `Io` (`ext4` logged it)
            });
        }
    };
    let inode = file.inode();
    let (dev, ino) = (inode.dev_id(), inode.ino());
    let warn = from_user && inner.fault_warned != Some((fm.file_id, err));
    if warn {
        inner.fault_warned = Some((fm.file_id, err));
    }
    let level = if warn { log::Level::Warn } else { log::Level::Debug };
    log::log!(target: "mm", level, "file page {index} of inode {dev:?}:{ino} can't be mapped ({err:?}): bus error");
    Err(FaultResult::BusError)
}

/// A ktest-only record of the steps of an unmap or downgrade, per page (by
/// its frame): its leaf cleared or made read-only, its TLB entry flushed,
/// and its [`WritableMap`] released, so a ktest can check they happen in
/// that order (ARCHITECTURE section 5's unmap order). Without the ktests
/// these are no-ops.
pub(crate) mod release_order {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) enum Step {
        Cleared,
        Flushed,
        Released,
    }

    #[cfg(feature = "ktest")]
    mod log {
        use core::sync::atomic::AtomicBool;

        use super::Step;
        use crate::sync::SpinLock;

        /// Whether a test is recording: checked before taking `LOG`, so a
        /// `WritableMap` dropped while nothing records takes no lock.
        pub(super) static ON: AtomicBool = AtomicBool::new(false);

        const CAP: usize = 64;

        /// The steps so far (a fixed array: recording never allocates),
        /// how many, and whether recording is on.
        pub(super) struct Log {
            pub(super) steps: [(u64, Step); CAP],
            pub(super) len: usize,
            pub(super) on: bool,
        }

        pub(super) static LOG: SpinLock<Log> =
            SpinLock::new(Log { steps: [(0, Step::Cleared); CAP], len: 0, on: false });
    }

    /// Record `step` of the `PTE_SHARED` page under `leaf` (others aren't
    /// counted).
    #[inline]
    pub(crate) fn step(leaf: &super::paging::UserLeaf, step: Step) {
        if leaf.was_shared {
            record(leaf.phys.as_u64(), step);
        }
    }

    /// Record `step` of the page in frame `frame`.
    #[inline]
    pub(crate) fn record(frame: u64, step: Step) {
        #[cfg(feature = "ktest")]
        if log::ON.load(core::sync::atomic::Ordering::Acquire) {
            let mut log = log::LOG.lock();
            if log.on && log.len < log.steps.len() {
                let at = log.len;
                log.steps[at] = (frame, step);
                log.len += 1;
            }
        }
        #[cfg(not(feature = "ktest"))]
        let _ = (frame, step);
    }

    /// Start recording, from nothing.
    #[cfg(feature = "ktest")]
    pub(crate) fn start() {
        let mut log = log::LOG.lock();
        log.len = 0;
        log.on = true;
        log::ON.store(true, core::sync::atomic::Ordering::Release);
    }

    /// Stop recording and return the steps.
    #[cfg(feature = "ktest")]
    pub(crate) fn stop() -> alloc::vec::Vec<(u64, Step)> {
        log::ON.store(false, core::sync::atomic::Ordering::Release);
        let mut log = log::LOG.lock();
        log.on = false;
        let (steps, len) = (log.steps, log.len);
        drop(log); // copied out: no allocation with the lock held
        steps[..len].to_vec()
    }
}

/// A failed `map_user_page` as a fault's outcome.
fn map_error_fault(err: MapError) -> FaultResult {
    match err {
        MapError::OutOfMemory => FaultResult::OutOfMemory,
        _ => FaultResult::Invalid,
    }
}

/// Move the [`Mapped`] entries of files no VMA maps any more out of
/// `mapped_files` into `gone`, which the caller drops once `inner` is
/// released (unregistering each from its inode's [`Mappers`] list and
/// dropping its open reference). Liveness is read from the VMAs as they are
/// now, so a `MAP_FIXED` replace by the same file keeps its entry.
fn release_unmapped_files(inner: &mut AddressSpaceInner, gone: &mut Vec<Mapped>) {
    if inner.mapped_files.is_empty() {
        return;
    }
    let unused: Vec<u64> = inner
        .mapped_files
        .keys()
        .copied()
        .filter(|&id| !inner.vmas.iter().any(|v| matches!(v.backing, Backing::File(fm) if fm.file_id == id)))
        .collect();
    for id in unused {
        if let Some(mapped) = inner.mapped_files.remove(&id) {
            gone.push(mapped);
        }
    }
}

impl Drop for AddressSpace {
    fn drop(&mut self) {
        assert_ne!(arch::paging::active_root(), self.root, "AddressSpace dropped while CR3 still holds {}", self.root);
        let active = self.active.load(Ordering::Acquire);
        assert_eq!(active, 0, "AddressSpace dropped while active on CPUs {active:#x}");
        // No sleeping lock needed: `&mut self` guarantees exclusive access, so
        // `Drop` is safe even inside `sched::finish_switch` with interrupts off.
        let inner = self.inner.get_mut();
        if inner.torn_down {
            // SAFETY: lower half was already cleared by `teardown()`; only the
            // bare L4 root frame remains to be freed.
            unsafe { frame::free(self.root) }.expect("freeing user L4 root frame");
        } else {
            let writable = &inner.writable_maps;
            // SAFETY: no CPU holds this root, and &mut self excludes any
            // concurrent page-table mutation. Release leaves first so the
            // paging teardown sees only exclusively owned empty tables.
            unsafe {
                paging::unmap_user_range(self.root, USER_MIN, USER_MAX, |leaf| {
                    note_shared_leaf(writable, &leaf);
                    // SAFETY: the leaf is removed, no CPU holds this root,
                    // and PCIDE=0 retired translations on switching away.
                    release_removed_leaf(&leaf);
                });
            }
            // SAFETY: all leaves were removed above; only exclusively owned
            // empty tables remain, and no CPU holds this root.
            unsafe { paging::destroy_user_root(self.root, |_| unreachable!("all user leaves removed")) };
            // No CPU holds this `CR3`: that was the flush. Dropping each
            // `Mapped` unregisters from its inode's leaf `Mappers` spinlock
            // and drops its open reference (taking no sleeping lock).
            inner.writable_maps.clear();
            inner.pinned_pages.clear();
            inner.mapped_files.clear();
        }
    }
}

/// The initial stack's top pages (`argv`, `envp`, `execfn`, `AT_RANDOM`'s
/// bytes and `auxv`), built before any page table or frame is allocated.
struct StackPages {
    bottom: u64,
    buf: alloc::vec::Vec<u8>,
    initial: InitialStack,
}

impl StackPages {
    /// The stack for `main` and its interpreter: `AT_BASE` is the
    /// interpreter's load bias when there is one; `InvalidArg` unless an
    /// interpreter is given exactly when `main` names one.
    fn build(
        main: ElfImage<'_>,
        interp: Option<ElfImage<'_>>,
        argv: &[&[u8]],
        envp: &[&[u8]],
        execfn: &[u8],
        random: [u8; 16],
        stack_size: lumen_vm::StackSize,
    ) -> Result<Self, UserVmError> {
        let auxv = match (main.plan.has_interp(), interp) {
            (false, None) => AuxvInfo::from_loaded(main.plan),
            (true, Some(interp)) => AuxvInfo::from_loaded_with_interp(main.plan, interp.plan.load_bias),
            _ => return Err(UserVmError::InvalidArg),
        };
        // Probe required initial stack bytes (and validate no interior NULs) without allocating.
        let required = match build_initial_stack(&mut [], USER_STACK_TOP, argv, envp, execfn, random, &auxv) {
            Err(StackBuildError::BufferTooSmall { required, .. }) => required,
            Err(e) => return Err(UserVmError::Stack(e)),
            Ok(info) => info.bytes_written,
        };
        if (required as u64) > stack_size.bytes() {
            return Err(UserVmError::Stack(StackBuildError::BufferTooSmall {
                required,
                available: stack_size.bytes() as usize,
            }));
        }
        let buf_len = lumen_vm::align_up_page((required as u64).max(1)).ok_or(UserVmError::InvalidArg)? as usize;
        let mut buf = Vec::new();
        buf.try_reserve_exact(buf_len).map_err(|_| UserVmError::OutOfMemory)?;
        buf.resize(buf_len, 0);
        let initial = build_initial_stack(&mut buf, USER_STACK_TOP, argv, envp, execfn, random, &auxv)
            .map_err(UserVmError::Stack)?;
        Ok(Self { buf, initial, bottom: stack_size.bottom() })
    }
}

/// Refuse a layout where the main image reaches the user stack or its guard
/// page, the interpreter is not wholly below the guard page (ARCHITECTURE
/// section 8), or the two overlap (each is already disjoint within itself),
/// before anything is mapped.
fn check_layout(main: &[MappedRange], interp: &[MappedRange], stack_bottom: u64) -> Result<(), UserVmError> {
    let stack_guard_bottom = stack_bottom - PAGE_SIZE;
    if main.iter().any(|a| a.end > stack_guard_bottom && a.start < USER_STACK_TOP) {
        return Err(UserVmError::Layout(LayoutClash::MainOnStack));
    }
    if interp.iter().any(|b| b.end > stack_guard_bottom) {
        return Err(UserVmError::Layout(LayoutClash::InterpNotBelowStack));
    }
    if main.iter().any(|a| interp.iter().any(|b| a.start < b.end && b.start < a.end)) {
        return Err(UserVmError::Layout(LayoutClash::ImagesOverlap));
    }
    Ok(())
}

#[cfg(feature = "ktest")]
pub(crate) fn build_relro_test_elf() -> alloc::vec::Vec<u8> {
    use lumen_elf::loader::{ET_EXEC, PF_R, PF_W, PF_X, PT_LOAD};

    // 3 PT_LOAD segments:
    //  0: R-X code at 0x401000..0x401020 (file offset 0x1000, 32 bytes)
    //  1: R-- rodata at 0x402000..0x402800 (file offset 0x2000, 0x800 bytes)
    //  2: RW- data+bss at 0x402800..0x403200 (file offset 0x2800, filesz 0x100, memsz 0xa00)
    // Segments 1 and 2 share boundary page 0x402000 (RELRO union: RW-, neither clobbered).
    crate::user::build_elf_with_phdrs(
        ET_EXEC,
        0x40_1000,
        &[
            (PT_LOAD, PF_R | PF_X, 0x1000, 0x40_1000, 0x20, 0x20),
            (PT_LOAD, PF_R, 0x2000, 0x40_2000, 0x800, 0x800),
            (PT_LOAD, PF_R | PF_W, 0x2800, 0x40_2800, 0x100, 0xa00),
        ],
        0x3000,
        &[(0x1000, &[0x90, 0x90, 0x90, 0xc3]), (0x2000, b"RO_PART!"), (0x2800, b"RW_PART!")],
    )
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

            // mprotect error mapping: misaligned -> EINVAL, out-of-range/hole -> ENOMEM, len==0 -> Ok.
            kassert_eq!(
                space.mprotect(base + 1, PAGE_SIZE, Prot::READ).map_err(UserVmError::to_mprotect_errno),
                Err(errno::EINVAL)
            );
            kassert_eq!(
                space.mprotect(0, PAGE_SIZE, Prot::READ).map_err(UserVmError::to_mprotect_errno),
                Err(errno::ENOMEM)
            );
            kassert_eq!(space.mprotect(base, 0, Prot::READ), Ok(()));
            kassert_eq!(UserVmError::Vm(VmError::Access).to_errno(), errno::EACCES);
            kassert_eq!(UserVmError::Vm(VmError::Access).to_mprotect_errno(), errno::EACCES);
            // Defensive RSVD bit check never maps a frame.
            kassert_eq!(space.handle_page_fault(base + 2 * PAGE_SIZE, 1 << 3), FaultResult::Invalid);

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

// File-backed mappings (#255): every test below runs on real PTEs, faulting
// through `copy_to_user`/`copy_from_user` (real `#PF`s from a kernel user
// copy) or calling `handle_page_fault` directly, over files on a fresh
// `tmpfs` and a fresh `ext4` volume (`vfs::page_cache::ktest_files`).

#[cfg(feature = "ktest")]
use crate::vfs::page_cache::ktest_files;

/// A populated present or PROT_NONE leaf, for counted-frame ktests.
#[cfg(feature = "ktest")]
pub(crate) fn ktest_any_leaf(space: &AddressSpace, addr: u64) -> Result<lumen_mm::paging::Mapping, &'static str> {
    let walk = paging::walk_user(space.root(), VirtAddr::new(addr).ok_or("invalid address")?);
    walk.mapping().or_else(|| walk.prot_none_mapping()).ok_or("missing leaf")
}

/// The present leaf at `va`, if any.
#[cfg(feature = "ktest")]
pub(crate) fn file_vm_leaf(space: &AddressSpace, va: u64) -> Option<lumen_mm::paging::Mapping> {
    paging::walk_user(space.root(), VirtAddr::new(va)?).mapping()
}

/// Number of entries currently on `inode`'s [`Mappers`] list, for ktests.
#[cfg(feature = "ktest")]
pub(crate) fn file_vm_mappers(inode: &dyn VfsInode) -> usize {
    page_cache::page_cache_mappers(inode).map_or(0, Mappers::len)
}

/// Run `check` in a fresh `tmpfs`'s root, then in a fresh `ext4` volume's
/// (devices `0:minor` and `0:minor+1`), naming the filesystem on failure.
#[cfg(feature = "ktest")]
fn file_vm_on_both(
    minor: u32,
    mut check: impl FnMut(&Arc<dyn VfsInode>) -> Result<(), &'static str>,
) -> Result<(), &'static str> {
    use crate::vfs::page_cache::ktest_files::{Ext4Volume, TmpfsVolume};
    let tmpfs = TmpfsVolume::new(minor, 64 * PAGE_SIZE as usize);
    check(&tmpfs.root).inspect_err(|_| crate::ktest::diag(format_args!("on tmpfs")))?;
    let ext4 = Ext4Volume::new(minor + 1)?;
    check(&ext4.root).inspect_err(|_| crate::ktest::diag(format_args!("on ext4")))
}

/// Whether the recorded `steps` hold page `frame`'s clear, flush and
/// release, in that order.
#[cfg(feature = "ktest")]
fn file_vm_ordered(steps: &[(u64, release_order::Step)], frame: u64) -> bool {
    use release_order::Step;
    let at = |step| steps.iter().position(|&s| s == (frame, step));
    let (c, f, r) = (at(Step::Cleared), at(Step::Flushed), at(Step::Released));
    c.is_some() && c < f && f < r
}

/// Whether every release in `steps` comes after every flush: the guards
/// are dropped once the walk is done, not leaf by leaf.
#[cfg(feature = "ktest")]
fn file_vm_released_after_the_walk(steps: &[(u64, release_order::Step)]) -> bool {
    use release_order::Step;
    let last_flush = steps.iter().rposition(|&(_, s)| s == Step::Flushed);
    let first_release = steps.iter().position(|&(_, s)| s == Step::Released);
    first_release.is_some() && last_flush < first_release
}

/// How many lines of the kernel log's ring hold `text`.
#[cfg(feature = "ktest")]
fn file_vm_logged(text: &str) -> usize {
    let mut buf = vec![0u8; 64 * 1024];
    let (n, _) = crate::klog::copy_recent(&mut buf);
    core::str::from_utf8(&buf[..n]).map_or(0, |s| s.lines().filter(|l| l.contains(text)).count())
}

/// `f` as `mmap` of a descriptor open `O_RDWR` maps it: through an open
/// file description of its own, standing in for a descriptor's, dropped on
/// return (the `MappedFile` keeps a clone of its open reference). Opening
/// it takes `OpenGuard::try_new`: it may sleep (no spinlock held), and
/// refuses (`"open the file"`) an inode an unlink has freed.
#[cfg(feature = "ktest")]
pub(crate) fn file_vm_open(f: &Arc<dyn VfsInode>) -> Result<MappedFile, &'static str> {
    file_vm_open_as(f, lumen_vfs::OpenFlags::O_RDWR)
}

/// [`file_vm_open`] through a description open `O_RDONLY`.
#[cfg(feature = "ktest")]
fn file_vm_open_ro(f: &Arc<dyn VfsInode>) -> Result<MappedFile, &'static str> {
    file_vm_open_as(f, lumen_vfs::OpenFlags::O_RDONLY)
}

#[cfg(feature = "ktest")]
fn file_vm_open_as(f: &Arc<dyn VfsInode>, flags: u32) -> Result<MappedFile, &'static str> {
    file_vm_open_path(f, b"/ktest/file", flags)
}

/// `f` opened with `flags` by `path` (its bytes, as `/proc/<pid>/maps`
/// shows them), as a mapping keeps it.
#[cfg(feature = "ktest")]
fn file_vm_open_path(f: &Arc<dyn VfsInode>, path: &[u8], flags: u32) -> Result<MappedFile, &'static str> {
    let flags = lumen_vfs::OpenFlags::from_bits(flags);
    let file = crate::linux::process::KernelFile::new_unchecked_flags(Arc::clone(f), Arc::from(path), flags)
        .map_err(|_| "open the file")?;
    Ok(MappedFile::of(&file, true))
}

#[cfg(feature = "ktest")]
const RW: Prot = Prot::READ.union(Prot::WRITE);

crate::ktest!(
    fn file_vm_maps_names_a_mapped_file_by_its_path_bytes() {
        // #268: a file mapped through a descriptor opened by a path that
        // isn't UTF-8 and holds two newlines is named in `format_maps` by
        // the path's bytes, each newline as `\012`: the listing stays one
        // line, which a name can't split.
        use crate::vfs::page_cache::ktest_files::TmpfsVolume;
        use crate::{kassert, kassert_eq};
        let vol = TmpfsVolume::new(272, 16 * PAGE_SIZE as usize);
        let f = ktest_files::file(&vol.root, "maps.bin", &ktest_files::pattern(PAGE_SIZE as usize, 1))?;
        let file = file_vm_open_path(&f, b"/k/\xff\nf\n", lumen_vfs::OpenFlags::O_RDONLY)?;
        let space = Arc::new(AddressSpace::new().map_err(|_| "AddressSpace::new")?);
        let mapped = space.mmap_file(MapPlace::Hint(None), PAGE_SIZE, Prot::READ, file, 0, Sharing::Private);
        let maps = space.format_maps();
        space.teardown();
        mapped.map_err(|_| "mmap_file")?;
        let named = maps.ends_with(b" /k/\xff\\012f\\012\n");
        if !named {
            crate::ktest::diag(format_args!("maps: {:?}", lumen_vfs::Escaped(&maps)));
        }
        kassert!(named);
        kassert_eq!(maps.iter().filter(|&&b| b == b'\n').count(), 1);
        Ok(())
    }
);

crate::ktest!(
    fn file_vm_shared_mapping_is_coherent_both_ways() {
        // `MAP_SHARED` maps the cache's own frame: a store through the PTE
        // is what `read_at` reads, and `write_at` shows through the mapping.
        // A read maps it read-only and uncounted; a store (to a mapped or a
        // not-yet-mapped page) makes it writable and counts it.
        use crate::user::{CopyError, copy_from_user, copy_to_user};
        use crate::{kassert, kassert_eq};
        const P: u64 = PAGE_SIZE;
        file_vm_on_both(150, |dir| {
            let data = ktest_files::pattern(2 * P as usize + 100, 3);
            let f = ktest_files::file(dir, "shared.bin", &data)?;
            let opens = f.open_count();
            let space = Arc::new(AddressSpace::new().map_err(|_| "AddressSpace::new")?);
            let base = space
                .mmap_file(MapPlace::Hint(None), 3 * P, RW, file_vm_open(&f)?, 0, Sharing::Shared)
                .map_err(|_| "mmap_file")?;
            kassert_eq!(space.file_page_counts(), (0, 0)); // lazy
            kassert_eq!(space.mapped_file_count(), 1);
            kassert_eq!(f.open_count(), opens + 1);
            let pages = [mmap_page(&f, 0), mmap_page(&f, 1), mmap_page(&f, 2)];
            let [Ok(p0), Ok(p1), Ok(p2)] = pages else {
                return Err("mmap_page");
            };
            {
                let _active = crate::sched::UserSpaceGuard::enter(space.clone());
                let mut buf = [0u8; 16];
                kassert_eq!(copy_from_user(&mut buf, base + 8), Ok(()));
                kassert!(buf[..] == data[8..24]);
                let m = file_vm_leaf(&space, base).ok_or("page 0 not mapped")?;
                kassert!(m.shared && !m.writable);
                kassert_eq!(m.phys, p0.page().phys_frame());
                kassert_eq!(space.file_page_counts(), (1, 0));
                kassert_eq!(p0.page().writable_maps(), 0);

                // `write_at` across pages 0 and 1, read through the mapping.
                kassert_eq!(f.write_at(P - 4, b"ABCDEFGH").map_err(|_| "write_at")?, 8);
                kassert_eq!(copy_from_user(&mut buf[..8], base + P - 4), Ok(()));
                kassert_eq!(&buf[..8], b"ABCDEFGH");
                kassert_eq!(space.file_page_counts(), (2, 0));

                // Stores: page 0 promoted, page 2 filled writable at once.
                kassert_eq!(copy_to_user(base + 100, b"through-the-pte"), Ok(()));
                let m = file_vm_leaf(&space, base).ok_or("page 0 gone")?;
                kassert!(m.shared && m.writable);
                kassert_eq!(m.phys, p0.page().phys_frame());
                kassert_eq!(copy_to_user(base + 2 * P + 10, b"last-page"), Ok(()));
                let m = file_vm_leaf(&space, base + 2 * P).ok_or("page 2 not mapped")?;
                kassert!(m.shared && m.writable);
                kassert_eq!(m.phys, p2.page().phys_frame());
                kassert_eq!(space.file_page_counts(), (3, 2));
                kassert_eq!((p0.page().writable_maps(), p1.page().writable_maps()), (1, 0));
                kassert_eq!(p2.page().writable_maps(), 1);
                kassert!(ktest_files::read(&f, 100, 15)? == b"through-the-pte");
                kassert!(ktest_files::read(&f, 2 * P + 10, 9)? == b"last-page");
                kassert_eq!(f.stat().map_err(|_| "stat")?.size, data.len() as u64); // never extended
                // The last page's bytes past EOF read as zeros.
                kassert_eq!(copy_from_user(&mut buf, base + 2 * P + 200), Ok(()));
                kassert_eq!(buf, [0u8; 16]);
                space.munmap(base, 3 * P).map_err(|_| "munmap")?;
                // A stale TLB entry would still let this read through.
                kassert_eq!(copy_from_user(&mut buf, base), Err(CopyError::Fault));
            }
            kassert!(frame::is_allocated(p0.page().phys_frame())); // the cache's, never freed
            kassert_eq!(space.file_page_counts(), (0, 0));
            kassert_eq!((p0.page().writable_maps(), p2.page().writable_maps()), (0, 0));
            kassert_eq!(space.mapped_file_count(), 0);
            kassert_eq!(f.open_count(), opens);
            Ok(())
        })
    }
);

crate::ktest!(
    fn file_vm_private_mapping_copies_on_write() {
        // `MAP_PRIVATE`: a read maps the cache's frame read-only; a store
        // copies it into a private frame (after a read, or at once), which
        // neither the file nor another mapping sees, and which doesn't see
        // the file's later writes. Teardown frees the copies, never the
        // cache's frames.
        use crate::user::{copy_from_user, copy_to_user};
        use crate::{kassert, kassert_eq};
        const P: u64 = PAGE_SIZE;
        file_vm_on_both(152, |dir| {
            let data = ktest_files::pattern(2 * P as usize, 5);
            let f = ktest_files::file(dir, "private.bin", &data)?;
            let opens = f.open_count();
            let cache0 = mmap_page(&f, 0).map_err(|_| "mmap_page 0")?;
            let space = Arc::new(AddressSpace::new().map_err(|_| "AddressSpace::new")?);
            let a = space
                .mmap_file(MapPlace::Hint(None), 2 * P, RW, file_vm_open(&f)?, 0, Sharing::Private)
                .map_err(|_| "map a")?;
            let b = space
                .mmap_file(MapPlace::Hint(None), 2 * P, RW, file_vm_open(&f)?, 0, Sharing::Private)
                .map_err(|_| "map b")?;
            let (copy_a, copy_b);
            {
                let _active = crate::sched::UserSpaceGuard::enter(space.clone());
                let mut buf = [0u8; 9];
                kassert_eq!(copy_from_user(&mut buf, a + 5), Ok(()));
                let m = file_vm_leaf(&space, a).ok_or("a not mapped")?;
                kassert!(m.shared && !m.writable);
                kassert_eq!(m.phys, cache0.page().phys_frame());
                kassert_eq!(space.file_page_counts(), (1, 0));

                // A store after the read: copy-on-write.
                kassert_eq!(copy_to_user(a + 5, b"private-a"), Ok(()));
                let m = file_vm_leaf(&space, a).ok_or("a gone")?;
                kassert!(!m.shared && m.writable);
                kassert!(m.phys != cache0.page().phys_frame());
                copy_a = m.phys;
                kassert!(frame::is_allocated(copy_a));
                kassert_eq!(space.file_page_counts(), (0, 0)); // the pin went with the leaf
                kassert_eq!(cache0.page().writable_maps(), 0);
                kassert!(ktest_files::read(&f, 0, data.len())? == data);
                kassert_eq!(copy_from_user(&mut buf, b + 5), Ok(()));
                kassert!(buf[..] == data[5..14]);
                kassert_eq!(copy_from_user(&mut buf, a), Ok(()));
                kassert!(buf[..5] == data[..5]); // the copy kept the rest of the page

                // A store to a page never mapped: a private copy at once.
                kassert_eq!(copy_to_user(b + P + 7, b"private-b"), Ok(()));
                let m = file_vm_leaf(&space, b + P).ok_or("b page 1 not mapped")?;
                kassert!(!m.shared && m.writable);
                copy_b = m.phys;
                kassert!(ktest_files::read(&f, 0, data.len())? == data);
                kassert_eq!(copy_from_user(&mut buf, a + P + 7), Ok(()));
                kassert!(buf[..] == data[P as usize + 7..P as usize + 16]);

                // The file's own later writes: seen through b's untouched
                // read-only page 0, not through a's copy.
                kassert_eq!(f.write_at(5, b"file-side").map_err(|_| "write_at")?, 9);
                kassert_eq!(copy_from_user(&mut buf, b + 5), Ok(()));
                kassert_eq!(&buf, b"file-side");
                kassert_eq!(copy_from_user(&mut buf, a + 5), Ok(()));
                kassert_eq!(&buf, b"private-a");
            }
            space.teardown();
            kassert!(!frame::is_allocated(copy_a));
            kassert!(!frame::is_allocated(copy_b));
            kassert!(frame::is_allocated(cache0.page().phys_frame()));
            kassert_eq!(space.file_page_counts(), (0, 0));
            kassert_eq!(space.mapped_file_count(), 0);
            kassert_eq!(f.open_count(), opens);
            Ok(())
        })
    }
);

crate::ktest!(
    fn file_vm_refuses_writes_without_may_write_and_unmappable_files() {
        // `PROT_WRITE` on a mapping without `VM_MAYWRITE` (an `O_RDONLY`
        // descriptor's `MAP_SHARED`) is `EACCES`, at `mmap` and at
        // `mprotect`; a directory has no pages (`ENODEV`); a misaligned
        // offset is `EINVAL`. None keeps a reference to the file.
        use crate::kassert_eq;
        const P: u64 = PAGE_SIZE;
        file_vm_on_both(154, |dir| {
            let f = ktest_files::file(dir, "ro.bin", &ktest_files::pattern(P as usize, 7))?;
            let opens = f.open_count();
            let space = Arc::new(AddressSpace::new().map_err(|_| "AddressSpace::new")?);
            let refused = space.mmap_file(MapPlace::Hint(None), P, RW, file_vm_open_ro(&f)?, 0, Sharing::Shared);
            kassert_eq!(refused, Err(UserVmError::Vm(VmError::Access)));
            kassert_eq!(refused.map_err(UserVmError::to_errno), Err(errno::EACCES));
            kassert_eq!((space.mapped_file_count(), f.open_count()), (0, opens));
            let ro = space
                .mmap_file(MapPlace::Hint(None), P, Prot::READ, file_vm_open_ro(&f)?, 0, Sharing::Shared)
                .map_err(|_| "ro")?;
            kassert_eq!(space.mprotect(ro, P, RW), Err(UserVmError::Vm(VmError::Access)));
            kassert_eq!(space.handle_page_fault(ro, 1 << 1), FaultResult::Invalid); // a store
            kassert_eq!(space.handle_page_fault(ro, 0), FaultResult::Resolved); // a read
            kassert_eq!(space.file_page_counts(), (1, 0));
            let not_file =
                space.mmap_file(MapPlace::Hint(None), P, Prot::READ, file_vm_open_ro(dir)?, 0, Sharing::Shared);
            kassert_eq!(not_file.map_err(UserVmError::to_errno), Err(errno::ENODEV));
            let misaligned =
                space.mmap_file(MapPlace::Hint(None), P, Prot::READ, file_vm_open(&f)?, 100, Sharing::Private);
            kassert_eq!(misaligned, Err(UserVmError::Vm(VmError::Misaligned)));
            kassert_eq!(space.mapped_file_count(), 1);
            drop(space);
            kassert_eq!(f.open_count(), opens);
            Ok(())
        })
    }
);

crate::ktest!(
    fn file_vm_shared_write_right_is_each_descriptions_own() {
        // A `MAP_SHARED` mapping's right to store to the file comes from the
        // description it maps, not from the first mapping of the file (the
        // one whose open reference the address space keeps), in either
        // order: only the `O_RDWR` mapping may be made writable, maps a
        // store's page writable, and is one `msync` writes back. A private
        // mapping, which may always be made writable, is never written back.
        use crate::{kassert, kassert_eq};
        const P: u64 = PAGE_SIZE;
        file_vm_on_both(180, |dir| {
            let f = ktest_files::file(dir, "modes.bin", &ktest_files::pattern(P as usize, 9))?;
            let space = Arc::new(AddressSpace::new().map_err(|_| "AddressSpace::new")?);
            let shared = |file| space.mmap_file(MapPlace::Hint(None), P, Prot::READ, file, 0, Sharing::Shared);
            let synced = |va: u64| space.msync_files(va, va + P).shared.len();
            for rw_first in [true, false] {
                let (rw, ro) = if rw_first {
                    let rw = shared(file_vm_open(&f)?).map_err(|_| "map rw")?;
                    let ro = shared(file_vm_open_ro(&f)?).map_err(|_| "map ro")?;
                    (rw, ro)
                } else {
                    let ro = shared(file_vm_open_ro(&f)?).map_err(|_| "map ro")?;
                    let rw = shared(file_vm_open(&f)?).map_err(|_| "map rw")?;
                    (rw, ro)
                };
                kassert_eq!(space.mapped_file_count(), 1); // the first one's kept
                kassert_eq!((synced(rw), synced(ro)), (1, 0));
                kassert_eq!(space.mprotect(ro, P, RW), Err(UserVmError::Vm(VmError::Access)));
                kassert_eq!(space.mprotect(rw, P, RW), Ok(()));
                kassert_eq!(space.handle_page_fault(rw, 1 << 1), FaultResult::Resolved); // a store
                let leaf = file_vm_leaf(&space, rw).ok_or("rw not mapped")?;
                kassert!(leaf.shared && leaf.writable);
                kassert_eq!((synced(rw), synced(ro)), (1, 0));
                let private = space
                    .mmap_file(MapPlace::Hint(None), P, RW, file_vm_open(&f)?, 0, Sharing::Private)
                    .map_err(|_| "map private")?;
                kassert_eq!(space.handle_page_fault(private, 1 << 1), FaultResult::Resolved); // a copy
                kassert_eq!(synced(private), 0);
                space.munmap(private, P).map_err(|_| "munmap private")?;
                space.munmap(rw, P).map_err(|_| "munmap rw")?;
                space.munmap(ro, P).map_err(|_| "munmap ro")?;
                kassert_eq!(space.mapped_file_count(), 0);
            }
            Ok(())
        })
    }
);

crate::ktest!(
    fn file_vm_fault_past_eof_is_a_bus_error() {
        // A page wholly past the file's end has nothing to map: a bus error
        // for a read or a store, shared or private, and `-EFAULT` for a
        // kernel user copy (the fixup path). The page holding the end maps.
        use crate::user::{CopyError, copy_from_user};
        use crate::{kassert, kassert_eq};
        const P: u64 = PAGE_SIZE;
        file_vm_on_both(156, |dir| {
            let f = ktest_files::file(dir, "short.bin", &ktest_files::pattern(100, 9))?;
            let space = Arc::new(AddressSpace::new().map_err(|_| "AddressSpace::new")?);
            let s = space
                .mmap_file(MapPlace::Hint(None), 3 * P, RW, file_vm_open(&f)?, 0, Sharing::Shared)
                .map_err(|_| "shared")?;
            let p = space
                .mmap_file(MapPlace::Hint(None), 2 * P, RW, file_vm_open(&f)?, 0, Sharing::Private)
                .map_err(|_| "private")?;
            kassert_eq!(space.handle_page_fault(s + P, 0), FaultResult::BusError);
            kassert_eq!(space.handle_page_fault(s + 2 * P + 8, 1 << 1), FaultResult::BusError);
            kassert_eq!(space.handle_page_fault(p + P, 1 << 1), FaultResult::BusError);
            kassert_eq!(space.handle_page_fault(p + P, 0), FaultResult::BusError);
            kassert_eq!(space.file_page_counts(), (0, 0));
            kassert_eq!(space.handle_page_fault(s, 0), FaultResult::Resolved);
            kassert!(file_vm_leaf(&space, s).is_some_and(|m| m.shared));
            let _active = crate::sched::UserSpaceGuard::enter(space.clone());
            let mut buf = [0u8; 8];
            kassert_eq!(copy_from_user(&mut buf, s + P), Err(CopyError::Fault));
            kassert_eq!(copy_from_user(&mut buf, p + P + 64), Err(CopyError::Fault));
            kassert_eq!(copy_from_user(&mut buf, s + 100), Ok(()));
            kassert_eq!(buf, [0u8; 8]); // past EOF in the last page
            Ok(())
        })
    }
);

crate::ktest!(
    fn file_vm_disk_read_error_is_a_bus_error() {
        // A page whose disk read fails is a bus error (the `ext4` fill logs
        // the device, inode and page once); once the disk reads again, the
        // same fault maps it.
        use crate::vfs::page_cache::ktest_files::{Ext4Volume, reclaim_ext4};
        use crate::{kassert, kassert_eq};
        const P: u64 = PAGE_SIZE;
        let vol = Ext4Volume::new(158)?;
        let data = ktest_files::pattern(3 * P as usize, 11);
        let f = ktest_files::file(&vol.root, "unreadable.bin", &data)?;
        f.sync(false).map_err(|_| "sync")?;
        kassert_eq!(reclaim_ext4(&f)?, 0); // nothing cached: a fault reads the disk
        let space = Arc::new(AddressSpace::new().map_err(|_| "AddressSpace::new")?);
        let base = space
            .mmap_file(MapPlace::Hint(None), 3 * P, Prot::READ, file_vm_open(&f)?, 0, Sharing::Shared)
            .map_err(|_| "map")?;
        vol.fail_reads(true);
        let failed = space.handle_page_fault(base + 2 * P, 0);
        vol.fail_reads(false);
        kassert_eq!(failed, FaultResult::BusError);
        kassert!(file_vm_leaf(&space, base + 2 * P).is_none());
        kassert_eq!(space.handle_page_fault(base + 2 * P, 0), FaultResult::Resolved);
        kassert!(file_vm_leaf(&space, base + 2 * P).is_some_and(|m| m.shared));
        Ok(())
    }
);

crate::ktest!(
    fn file_vm_a_volume_gone_read_only_takes_no_store_through_an_older_mapping() {
        // #349: a shared writable mapping made while the ext4 volume was
        // writable, the volume then errored (a failed writeback): a store
        // fault on a page never touched, and one promoting a page mapped
        // read-only by a read, are bus errors (the leaf stays as it was),
        // and `mprotect` can't add write access to the mapping (`EACCES`).
        // A read still maps the page.
        use crate::vfs::page_cache::ktest_files::Ext4Volume;
        use crate::{kassert, kassert_eq};
        const P: u64 = PAGE_SIZE;
        let vol = Ext4Volume::new(359)?;
        let data = ktest_files::pattern(3 * P as usize, 17);
        let f = ktest_files::file(&vol.root, "older.bin", &data)?;
        f.sync(false).map_err(|_| "sync")?;
        let space = Arc::new(AddressSpace::new().map_err(|_| "AddressSpace::new")?);
        let base = space
            .mmap_file(MapPlace::Hint(None), 3 * P, RW, file_vm_open(&f)?, 0, Sharing::Shared)
            .map_err(|_| "map")?;
        kassert_eq!(space.handle_page_fault(base + P, 0), FaultResult::Resolved); // read-only leaf
        // The volume errors: a dirty page's writeback fails.
        kassert_eq!(f.write_at(0, b"dirty").map_err(|_| "write")?, 5);
        vol.fail_writes(true);
        kassert!(f.sync(false).is_err());
        vol.fail_writes(false);
        kassert!(crate::vfs::page_cache::mapped_read_only(&*f));
        kassert_eq!(space.handle_page_fault(base + 2 * P, 1 << 1), FaultResult::BusError); // untouched
        kassert!(file_vm_leaf(&space, base + 2 * P).is_none());
        kassert_eq!(space.handle_page_fault(base + P, 1 << 1), FaultResult::BusError); // promotion
        kassert!(file_vm_leaf(&space, base + P).is_some_and(|m| m.shared));
        kassert!(matches!(space.mprotect(base, 3 * P, RW), Err(UserVmError::Vm(VmError::Access))));
        kassert!(space.mprotect(base, 3 * P, Prot::READ).is_ok());
        kassert_eq!(space.handle_page_fault(base + 2 * P, 0), FaultResult::Resolved);
        Ok(())
    }
);

crate::ktest!(
    fn file_vm_shared_stores_persist_through_unmap_reclaim_and_remount() {
        // #266 end to end through real PTEs: a store faults the page in
        // writable, a writeback while it is mapped clears its dirty bit, a
        // second store (no fault: the PTE is writable already) only the CPU
        // sees. After `munmap`, reclaim must write both back before evicting,
        // and a fresh read-only mount reads them from the disk.
        use crate::user::copy_to_user;
        use crate::vfs::page_cache::ktest_files::{Ext4Volume, reclaim_ext4};
        use crate::{kassert, kassert_eq};
        const P: u64 = PAGE_SIZE;
        let vol = Ext4Volume::new(160)?;
        let mut expected = ktest_files::pattern(2 * P as usize, 13);
        let f = ktest_files::file(&vol.root, "persist.bin", &expected)?;
        f.sync(false).map_err(|_| "sync")?;
        let space = Arc::new(AddressSpace::new().map_err(|_| "AddressSpace::new")?);
        let base = space
            .mmap_file(MapPlace::Hint(None), 2 * P, RW, file_vm_open(&f)?, 0, Sharing::Shared)
            .map_err(|_| "map")?;
        {
            let _active = crate::sched::UserSpaceGuard::enter(space.clone());
            kassert_eq!(copy_to_user(base + 10, b"first-store"), Ok(()));
            f.sync(false).map_err(|_| "sync while mapped")?;
            kassert_eq!(copy_to_user(base + 3000, b"second-store"), Ok(()));
            kassert_eq!(copy_to_user(base + P + 1, b"page-two"), Ok(()));
            space.munmap(base, 2 * P).map_err(|_| "munmap")?;
        }
        expected[10..21].copy_from_slice(b"first-store");
        expected[3000..3012].copy_from_slice(b"second-store");
        expected[P as usize + 1..P as usize + 9].copy_from_slice(b"page-two");
        kassert_eq!(reclaim_ext4(&f)?, 0); // written back and evicted
        let ro = vol.remount_ro(161)?.lookup(b"persist.bin").map_err(|_| "ro lookup")?;
        kassert!(ktest_files::read(&ro, 0, expected.len())? == expected);
        kassert!(ktest_files::read(&f, 0, expected.len())? == expected);
        Ok(())
    }
);

crate::ktest!(
    fn file_vm_invalidate_file_from_unmaps_exactly_the_pages_past_the_new_end() {
        // `invalidate_file_from` (a truncate-shrink's step): every page of
        // that file at or past the new end's page boundary goes, in every
        // mapping of it (counted guards dropped, private copies freed);
        // pages below it, and other files' pages, stay mapped.
        use crate::user::{copy_from_user, copy_to_user};
        use crate::{kassert, kassert_eq};
        const P: u64 = PAGE_SIZE;
        file_vm_on_both(162, |dir| {
            let f = ktest_files::file(dir, "shrink.bin", &ktest_files::pattern(4 * P as usize, 15))?;
            let g = ktest_files::file(dir, "other.bin", &ktest_files::pattern(P as usize, 17))?;
            let space = Arc::new(AddressSpace::new().map_err(|_| "AddressSpace::new")?);
            let a = space
                .mmap_file(MapPlace::Hint(None), 4 * P, RW, file_vm_open(&f)?, 0, Sharing::Shared)
                .map_err(|_| "map a")?;
            let b = space
                .mmap_file(MapPlace::Hint(None), 2 * P, RW, file_vm_open(&f)?, P, Sharing::Private)
                .map_err(|_| "map b")?;
            let c = space
                .mmap_file(MapPlace::Hint(None), P, RW, file_vm_open(&g)?, 0, Sharing::Shared)
                .map_err(|_| "map c")?;
            let pages: Vec<MappedPage> = (0..4).filter_map(|i| mmap_page(&f, i).ok()).collect();
            kassert_eq!(pages.len(), 4);
            let copy;
            {
                // `invalidate_file_from` runs on the space loaded here.
                let _active = crate::sched::UserSpaceGuard::enter(space.clone());
                for i in 0..4 {
                    kassert_eq!(copy_to_user(a + i * P, b"x"), Ok(()));
                }
                let mut buf = [0u8; 1];
                kassert_eq!(copy_from_user(&mut buf, b), Ok(())); // file page 1, read-only
                kassert_eq!(copy_to_user(b + P, b"y"), Ok(())); // file page 2, a private copy
                copy = file_vm_leaf(&space, b + P).ok_or("b page 1 not mapped")?.phys;
                kassert_eq!(copy_to_user(c, b"z"), Ok(()));
                kassert_eq!(space.file_page_counts(), (6, 5));
                let id = file_id_of(&*f);
                release_order::start();
                let invalidated = space.invalidate_file_from(id, P + 1);
                let steps = release_order::stop();
                kassert_eq!(invalidated, 3); // cut at 2P: file pages 2 and 3
                for page in &pages[2..] {
                    kassert!(file_vm_ordered(&steps, page.page().phys_frame().as_u64()));
                }
                kassert!(file_vm_released_after_the_walk(&steps));
                for (va, mapped) in [(a, true), (a + P, true), (a + 2 * P, false), (a + 3 * P, false)] {
                    kassert_eq!(file_vm_leaf(&space, va).is_some(), mapped);
                }
                kassert!(file_vm_leaf(&space, b).is_some()); // file page 1
                kassert!(file_vm_leaf(&space, b + P).is_none()); // file page 2
                kassert!(!frame::is_allocated(copy));
                kassert!(file_vm_leaf(&space, c).is_some_and(|m| m.writable)); // the other file
                kassert_eq!(space.file_page_counts(), (4, 3));
                let counts: Vec<u32> = pages.iter().map(|p| p.page().writable_maps()).collect();
                kassert_eq!(counts, [1, 1, 0, 0]);
                kassert_eq!(space.invalidate_file_from(id, 4 * P), 0); // nothing past the end
                kassert_eq!(space.invalidate_file_from(id, u64::MAX), 0);
                kassert_eq!(space.invalidate_file_from(id, 0), 3); // a's pages 0 and 1, b's page 0
                kassert_eq!(space.file_page_counts(), (1, 1));
                // The VMAs stay: with the file unchanged, a fault maps again.
                kassert_eq!(space.handle_page_fault(a + 3 * P, 0), FaultResult::Resolved);
            }
            Ok(())
        })
    }
);

crate::ktest!(
    fn file_vm_writable_maps_balance_through_mprotect_unmap_and_teardown() {
        // Each writable `MAP_SHARED` leaf counts once in its page's
        // `writable_maps`, and every way out uncounts it: `mprotect` (to
        // read-only, or `PROT_NONE`, which keeps the pin), a store after it
        // counting again, a partial `munmap`, `MAP_FIXED` replacing it,
        // `brk` shrinking over it, `teardown`, and `Drop` without a teardown.
        // `mapped_files` keeps the file open and registered on its `Mappers`
        // list until its last VMA goes (a `MAP_FIXED` replace by the same file
        // keeps its registration).
        use crate::user::{CopyError, copy_from_user, copy_to_user};
        use crate::{kassert, kassert_eq};
        const P: u64 = PAGE_SIZE;
        file_vm_on_both(164, |dir| {
            let f = ktest_files::file(dir, "balance.bin", &ktest_files::pattern(2 * P as usize, 19))?;
            let opens = f.open_count();
            let (Ok(p0), Ok(p1)) = (mmap_page(&f, 0), mmap_page(&f, 1)) else {
                return Err("mmap_page");
            };
            let counts = || (p0.page().writable_maps(), p1.page().writable_maps());
            let space = Arc::new(AddressSpace::new().map_err(|_| "AddressSpace::new")?);
            let base = space
                .mmap_file(MapPlace::Hint(None), 2 * P, RW, file_vm_open(&f)?, 0, Sharing::Shared)
                .map_err(|_| "map")?;
            {
                let _active = crate::sched::UserSpaceGuard::enter(space.clone());
                kassert_eq!(copy_to_user(base, b"0"), Ok(()));
                kassert_eq!(copy_to_user(base + P, b"1"), Ok(()));
                kassert_eq!(counts(), (1, 1));
                space.mprotect(base, P, Prot::READ).map_err(|_| "mprotect ro")?;
                kassert_eq!(counts(), (0, 1));
                kassert!(file_vm_leaf(&space, base).is_some_and(|m| m.shared && !m.writable));
                kassert_eq!(copy_to_user(base, b"0"), Err(CopyError::Fault));
                space.mprotect(base, P, RW).map_err(|_| "mprotect rw")?;
                kassert_eq!(counts(), (0, 1)); // read-only until a store
                kassert_eq!(copy_to_user(base, b"0"), Ok(()));
                kassert_eq!(counts(), (1, 1));
                space.mprotect(base, 2 * P, Prot::NONE).map_err(|_| "mprotect none")?;
                kassert_eq!(counts(), (0, 0));
                let kept = paging::walk_user(space.root(), VirtAddr::new(base).ok_or("va")?).prot_none_mapping();
                kassert!(kept.is_some_and(|m| m.shared && m.phys == p0.page().phys_frame()));
                kassert_eq!(space.file_page_counts(), (2, 0));
                space.mprotect(base, 2 * P, RW).map_err(|_| "mprotect rw again")?;
                kassert_eq!(copy_to_user(base, b"0"), Ok(()));
                kassert_eq!(copy_to_user(base + P, b"1"), Ok(()));
                kassert_eq!(counts(), (1, 1));
                space.munmap(base + P, P).map_err(|_| "munmap half")?;
                kassert_eq!(counts(), (1, 0));
                kassert_eq!((space.mapped_file_count(), file_vm_mappers(&*f)), (1, 1)); // the other half still maps it
            }
            space.teardown();
            kassert_eq!(counts(), (0, 0));
            kassert_eq!(space.file_page_counts(), (0, 0));
            kassert_eq!((space.mapped_file_count(), f.open_count(), file_vm_mappers(&*f)), (0, opens, 0));
            drop(space);

            // `MAP_FIXED` over a mapping: released as by `munmap`.
            let space = Arc::new(AddressSpace::new().map_err(|_| "AddressSpace::new")?);
            let base = space
                .mmap_file(MapPlace::Hint(None), P, RW, file_vm_open(&f)?, 0, Sharing::Shared)
                .map_err(|_| "map 2")?;
            kassert_eq!(space.handle_page_fault(base, 1 << 1), FaultResult::Resolved);
            kassert_eq!(counts(), (1, 0));
            let fixed = u64::from(general::MAP_PRIVATE | general::MAP_ANONYMOUS | general::MAP_FIXED);
            kassert_eq!(space.mmap_anon(base, P, RW, fixed), Ok(base));
            kassert_eq!(counts(), (0, 0));
            kassert_eq!((space.mapped_file_count(), f.open_count(), file_vm_mappers(&*f)), (0, opens, 0));
            space
                .mmap_file(MapPlace::Fixed(base), P, RW, file_vm_open(&f)?, 0, Sharing::Shared)
                .map_err(|_| "map 3")?;
            kassert_eq!(file_vm_mappers(&*f), 1);
            // ... and a file page faulted in there is released by the next.
            kassert_eq!(space.handle_page_fault(base, 1 << 1), FaultResult::Resolved);
            kassert_eq!((counts(), space.file_page_counts()), ((1, 0), (1, 1)));
            // A file mapped over itself keeps its reference and registration.
            space
                .mmap_file(MapPlace::Fixed(base), P, RW, file_vm_open(&f)?, P, Sharing::Shared)
                .map_err(|_| "map 4")?;
            kassert_eq!((counts(), space.file_page_counts()), ((0, 0), (0, 0)));
            kassert_eq!((space.mapped_file_count(), f.open_count(), file_vm_mappers(&*f)), (1, opens + 1, 1));
            {
                let _active = crate::sched::UserSpaceGuard::enter(space.clone());
                let mut head = [0u8; 8];
                kassert_eq!(copy_from_user(&mut head, base), Ok(()));
                kassert!(head[..] == ktest_files::read(&f, P, 8)?[..]); // file page 1 now
            }

            // A full `munmap` and a `brk` shrink over a file mapped inside the
            // heap both release its reference and mappers registration.
            let heap = Arc::new(AddressSpace::new().map_err(|_| "AddressSpace::new")?);
            let unmapped = heap
                .mmap_file(MapPlace::Hint(None), P, Prot::READ, file_vm_open(&f)?, 0, Sharing::Shared)
                .map_err(|_| "map munmap")?;
            kassert_eq!(file_vm_mappers(&*f), 2);
            heap.munmap(unmapped, P).map_err(|_| "full munmap")?;
            kassert_eq!((heap.mapped_file_count(), file_vm_mappers(&*f)), (0, 1));
            let b0 = heap.brk(0);
            kassert_eq!(heap.brk(b0 + 2 * P), b0 + 2 * P);
            heap.mmap_file(MapPlace::Fixed(b0 + P), P, RW, file_vm_open(&f)?, 0, Sharing::Shared)
                .map_err(|_| "map 5")?;
            kassert_eq!(file_vm_mappers(&*f), 2);
            kassert_eq!(heap.handle_page_fault(b0 + P, 1 << 1), FaultResult::Resolved);
            kassert_eq!(counts(), (1, 0));
            kassert_eq!(heap.brk(b0), b0);
            kassert_eq!((counts(), heap.file_page_counts()), ((0, 0), (0, 0)));
            kassert_eq!((heap.mapped_file_count(), f.open_count(), file_vm_mappers(&*f)), (0, opens + 1, 1)); // `space` still maps it
            drop(heap);

            // `Drop` without a teardown (an error path) releases too: the
            // count, the pin, a private copy (freed), the reference, and the
            // mappers registration.
            kassert_eq!(space.handle_page_fault(base, 1 << 1), FaultResult::Resolved);
            kassert_eq!(counts(), (0, 1));
            kassert!(Arc::strong_count(p1.page()) > 2); // the pin and the guard too
            let private = space
                .mmap_file(MapPlace::Hint(None), P, RW, file_vm_open(&f)?, 0, Sharing::Private)
                .map_err(|_| "map 6")?;
            kassert_eq!(space.handle_page_fault(private, 1 << 1), FaultResult::Resolved);
            let copy = file_vm_leaf(&space, private).ok_or("private copy not mapped")?.phys;
            kassert!(frame::is_allocated(copy));
            drop(space);
            kassert_eq!(counts(), (0, 0));
            kassert_eq!(Arc::strong_count(p1.page()), 2); // the cache's and `p1`'s
            kassert!(!frame::is_allocated(copy));
            kassert!(frame::is_allocated(p1.page().phys_frame()));
            kassert_eq!((f.open_count(), file_vm_mappers(&*f)), (opens, 0));
            Ok(())
        })
    }
);

crate::ktest!(
    fn file_vm_ring3_bus_error_ends_the_process() {
        // A ring-3 load from a mapped page past the file's end ends the
        // process with `UserExit::BusError` (its `rip` and address); the
        // same load from the page holding the end runs on to `exit(0)`.
        // The exit's teardown gives the file's reference back.
        use crate::kassert_eq;
        use crate::linux::process::ConsoleIo;
        use crate::user::{UserExit, build_elf_with_phdrs, run_elf_process};
        use crate::vfs::page_cache::ktest_files::TmpfsVolume;
        use lumen_elf::loader::{ET_EXEC, PF_R, PF_X, PT_LOAD};
        const P: u64 = PAGE_SIZE;
        const MAP: u64 = 0x1000_0000;
        let vol = TmpfsVolume::new(166, 4 * P as usize);
        let f = ktest_files::file(&vol.root, "sigbus.bin", &ktest_files::pattern(100, 21))?;
        let run = |addr: u64| -> Result<UserExit, &'static str> {
            // mov rax, addr; mov al, [rax]; exit_group(0); ud2
            let mut code = vec![0x48, 0xb8];
            code.extend_from_slice(&addr.to_le_bytes());
            code.extend_from_slice(&[0x8a, 0x00, 0xb8, 0xe7, 0, 0, 0, 0x31, 0xff, 0x0f, 0x05, 0x0f, 0x0b]);
            let len = code.len() as u64;
            let elf = build_elf_with_phdrs(
                ET_EXEC,
                0x40_1000,
                &[(PT_LOAD, PF_R | PF_X, 0x1000, 0x40_1000, len, len)],
                0x2000,
                &[(0x1000, &code)],
            );
            let (space, prog) =
                AddressSpace::from_elf(&elf, &[b"/sigbus"], &[], b"/sigbus", [0; 16]).map_err(|_| "from_elf")?;
            let space = Arc::new(space);
            space
                .mmap_file(MapPlace::FixedNoReplace(MAP), 2 * P, Prot::READ, file_vm_open_ro(&f)?, 0, Sharing::Shared)
                .map_err(|_| "mmap_file")?;
            run_elf_process("ktest-sigbus", space, prog.entry, prog.rsp, ConsoleIo::Capture, None)
        };
        kassert_eq!(run(MAP + 8)?, UserExit::Exited(0));
        kassert_eq!(run(MAP + P + 8)?, UserExit::BusError { rip: 0x40_100a, cr2: MAP + P + 8 });
        kassert_eq!(f.open_count(), 0);
        Ok(())
    }
);

crate::ktest!(
    fn file_vm_shared_stores_reach_the_disk_through_syncs_while_mapped() {
        // The writable-mapping count alone keeps a mapped page dirty: a
        // store, a sync (which clears `dirty`), a second store through the
        // still-writable PTE (no fault, nothing set in the page), a second
        // sync. That sync must write the second store, so a fresh read-only
        // mount reads both while the mapping still stands.
        use crate::user::copy_to_user;
        use crate::vfs::page_cache::ktest_files::Ext4Volume;
        use crate::{kassert, kassert_eq};
        const P: u64 = PAGE_SIZE;
        let vol = Ext4Volume::new(168)?;
        let mut expected = ktest_files::pattern(P as usize, 23);
        let f = ktest_files::file(&vol.root, "synced.bin", &expected)?;
        f.sync(false).map_err(|_| "sync")?;
        let space = Arc::new(AddressSpace::new().map_err(|_| "AddressSpace::new")?);
        let base =
            space.mmap_file(MapPlace::Hint(None), P, RW, file_vm_open(&f)?, 0, Sharing::Shared).map_err(|_| "map")?;
        let _active = crate::sched::UserSpaceGuard::enter(space.clone());
        kassert_eq!(copy_to_user(base + 10, b"first"), Ok(()));
        f.sync(false).map_err(|_| "first sync")?;
        kassert_eq!(copy_to_user(base + 100, b"second"), Ok(()));
        f.sync(false).map_err(|_| "second sync")?;
        expected[10..15].copy_from_slice(b"first");
        expected[100..106].copy_from_slice(b"second");
        let ro = vol.remount_ro(169)?.lookup(b"synced.bin").map_err(|_| "ro lookup")?;
        kassert!(ktest_files::read(&ro, 0, expected.len())? == expected);
        space.munmap(base, P).map_err(|_| "munmap")?;
        Ok(())
    }
);

crate::ktest!(
    fn file_vm_mapping_outlives_close_and_unlink() {
        // The mapping's own open reference: with the descriptor closed and
        // the name unlinked, stores and reads through the mapping still
        // reach the file (an inode detached rather than freed), and
        // `munmap` gives the last reference back. On ext4 the disk inode
        // is free again and the detached pages go once the inode does.
        use crate::user::{copy_from_user, copy_to_user};
        use crate::vfs::page_cache::detached_cached_pages;
        use crate::vfs::page_cache::ktest_files::{Ext4Volume, TmpfsVolume};
        use crate::{kassert, kassert_eq};
        const P: u64 = PAGE_SIZE;
        fn check(dir: &Arc<dyn VfsInode>) -> Result<(), &'static str> {
            let f = ktest_files::file(dir, "gone.bin", &ktest_files::pattern(P as usize, 25))?;
            let space = Arc::new(AddressSpace::new().map_err(|_| "AddressSpace::new")?);
            // The descriptor's reference goes at the end of this statement.
            let base = space
                .mmap_file(MapPlace::Hint(None), P, RW, file_vm_open(&f)?, 0, Sharing::Shared)
                .map_err(|_| "map")?;
            kassert_eq!(f.open_count(), 1); // the mapping's
            dir.unlink(b"gone.bin").map_err(|_| "unlink")?;
            kassert!(dir.lookup(b"gone.bin").is_err());
            kassert_eq!(f.stat().map_err(|_| "stat")?.nlink, 0);
            kassert!(f.is_detached()); // kept by the mapping's reference, not freed
            {
                let _active = crate::sched::UserSpaceGuard::enter(space.clone());
                kassert_eq!(copy_to_user(base + 5, b"after-unlink"), Ok(()));
                let mut buf = [0u8; 12];
                kassert_eq!(copy_from_user(&mut buf, base + 5), Ok(()));
                kassert_eq!(&buf, b"after-unlink");
                kassert!(ktest_files::read(&f, 5, 12)? == b"after-unlink");
                space.munmap(base, P).map_err(|_| "munmap")?;
            }
            kassert_eq!((space.mapped_file_count(), f.open_count()), (0, 0));
            Ok(())
        }
        let tmpfs = TmpfsVolume::new(170, 8 * P as usize);
        check(&tmpfs.root).inspect_err(|_| crate::ktest::diag(format_args!("on tmpfs")))?;
        let ext4 = Ext4Volume::new(171)?;
        let (inodes, detached) = (ext4.free_inodes(), detached_cached_pages());
        check(&ext4.root).inspect_err(|_| crate::ktest::diag(format_args!("on ext4")))?;
        kassert_eq!(ext4.free_inodes(), inodes);
        kassert_eq!(detached_cached_pages(), detached);
        Ok(())
    }
);

crate::ktest!(
    fn file_vm_private_copy_of_the_last_page_stops_at_eof() {
        // A private copy of the page holding the file's end has zeros past
        // it, even where a shared mapping stored past EOF in that page:
        // copied at once (a store to an unmapped page) or on write (a store
        // after a read).
        use crate::user::{copy_from_user, copy_to_user};
        use crate::{kassert, kassert_eq};
        const P: u64 = PAGE_SIZE;
        file_vm_on_both(172, |dir| {
            let data = ktest_files::pattern(P as usize + 100, 27);
            let f = ktest_files::file(dir, "tail.bin", &data)?;
            let space = Arc::new(AddressSpace::new().map_err(|_| "AddressSpace::new")?);
            let map = |sharing| {
                space.mmap_file(MapPlace::Hint(None), 2 * P, RW, file_vm_open(&f)?, 0, sharing).map_err(|_| "map")
            };
            let (s, now, later) = (map(Sharing::Shared)?, map(Sharing::Private)?, map(Sharing::Private)?);
            let _active = crate::sched::UserSpaceGuard::enter(space.clone());
            kassert_eq!(copy_to_user(s + P + 200, b"past-eof"), Ok(()));
            let mut head = [0u8; 1];
            kassert_eq!(copy_from_user(&mut head, later + P), Ok(())); // read-only first
            for va in [now, later] {
                kassert_eq!(copy_to_user(va + P + 10, b"mine"), Ok(()));
                let m = file_vm_leaf(&space, va + P).ok_or("copy not mapped")?;
                kassert!(!m.shared && m.writable);
                let mut page = vec![0u8; P as usize];
                kassert_eq!(copy_from_user(&mut page, va + P), Ok(()));
                kassert!(page[..10] == data[P as usize..P as usize + 10]);
                kassert_eq!(&page[10..14], b"mine");
                kassert!(page[14..100] == data[P as usize + 14..]);
                kassert!(page[100..].iter().all(|&b| b == 0));
            }
            let mut stored = [0u8; 8];
            kassert_eq!(copy_from_user(&mut stored, s + P + 200), Ok(()));
            kassert_eq!(&stored, b"past-eof"); // the shared mapping keeps it
            Ok(())
        })
    }
);

crate::ktest!(
    fn file_vm_store_after_a_truncate_maps_the_files_current_page() {
        // A truncate that doesn't invalidate mappings (`VfsInode::truncate`
        // called directly, or a kernel-side truncate that skips invalidation)
        // lets the cache drop a page a read-only leaf still maps. A store
        // before it regrows is a bus error that drops the leaf; after it
        // regrows, a store maps the file's current page (`MAP_SHARED`) or
        // copies it (`MAP_PRIVATE`), not the dropped one.
        use crate::user::{copy_from_user, copy_to_user};
        use crate::{kassert, kassert_eq};
        const P: u64 = PAGE_SIZE;
        file_vm_on_both(174, |dir| {
            let f = ktest_files::file(dir, "regrown.bin", &ktest_files::pattern(2 * P as usize, 29))?;
            let space = Arc::new(AddressSpace::new().map_err(|_| "AddressSpace::new")?);
            let map = |sharing| {
                space.mmap_file(MapPlace::Hint(None), 2 * P, RW, file_vm_open(&f)?, 0, sharing).map_err(|_| "map")
            };
            let (s, q) = (map(Sharing::Shared)?, map(Sharing::Private)?);
            let _active = crate::sched::UserSpaceGuard::enter(space.clone());
            let mut byte = [0u8; 1];
            kassert_eq!(copy_from_user(&mut byte, s + P), Ok(()));
            kassert_eq!(copy_from_user(&mut byte, q + P), Ok(()));
            kassert_eq!(space.file_page_counts(), (2, 0));
            // Shrunk and not regrown: a store to either leaf is a bus error,
            // and the leaf (with its pin) goes rather than keep the page the
            // file let go mapped.
            f.truncate(P).map_err(|_| "shrink")?;
            for va in [s + P, q + P] {
                kassert_eq!(space.handle_page_fault(va, 1 << 1), FaultResult::BusError);
                kassert!(file_vm_leaf(&space, va).is_none());
            }
            kassert_eq!(space.file_page_counts(), (0, 0));

            // Regrown, read-only leaves again, then shrunk and regrown once more.
            f.truncate(2 * P).map_err(|_| "regrow")?;
            kassert_eq!(copy_from_user(&mut byte, s + P), Ok(()));
            kassert_eq!(copy_from_user(&mut byte, q + P), Ok(()));
            let old = file_vm_leaf(&space, s + P).ok_or("s page 1 not mapped")?.phys;
            kassert_eq!(file_vm_leaf(&space, q + P).map(|m| m.phys), Some(old));
            f.truncate(P).map_err(|_| "shrink again")?;
            f.truncate(2 * P).map_err(|_| "regrow again")?;

            kassert_eq!(copy_to_user(s + P + 8, b"fresh"), Ok(()));
            let current = mmap_page(&f, 1).map_err(|_| "mmap_page 1")?;
            let m = file_vm_leaf(&space, s + P).ok_or("s page 1 gone")?;
            kassert!(m.shared && m.writable);
            kassert_eq!(m.phys, current.page().phys_frame());
            kassert!(m.phys != old);
            kassert!(ktest_files::read(&f, P + 8, 5)? == b"fresh");

            kassert_eq!(copy_to_user(q + P + 16, b"copy"), Ok(()));
            let mut head = [0u8; 32];
            kassert_eq!(copy_from_user(&mut head, q + P), Ok(()));
            let mut want = [0u8; 32]; // the regrown page: zeros and the shared store
            want[8..13].copy_from_slice(b"fresh");
            want[16..20].copy_from_slice(b"copy");
            kassert_eq!(head, want);
            kassert_eq!(space.file_page_counts(), (1, 1));
            Ok(())
        })
    }
);

crate::ktest!(
    fn file_vm_unmap_releases_after_clearing_and_flushing() {
        // ARCHITECTURE section 5's unmap order, recorded per page: the leaf
        // cleared (or made read-only), its TLB entry flushed, and only then
        // its `WritableMap` released, every release after the whole walk;
        // for `mprotect` and for `munmap` (and `invalidate_file_from`, in its
        // own test). A read-only page has no release.
        use crate::mm::user_vm::release_order::{self, Step};
        use crate::user::copy_to_user;
        use crate::vfs::page_cache::ktest_files::TmpfsVolume;
        use crate::{kassert, kassert_eq};
        const P: u64 = PAGE_SIZE;
        let vol = TmpfsVolume::new(176, 8 * P as usize);
        let f = ktest_files::file(&vol.root, "order.bin", &ktest_files::pattern(2 * P as usize, 31))?;
        let (Ok(p0), Ok(p1)) = (mmap_page(&f, 0), mmap_page(&f, 1)) else {
            return Err("mmap_page");
        };
        let (f0, f1) = (p0.page().phys_frame().as_u64(), p1.page().phys_frame().as_u64());
        let space = Arc::new(AddressSpace::new().map_err(|_| "AddressSpace::new")?);
        let base = space
            .mmap_file(MapPlace::Hint(None), 2 * P, RW, file_vm_open(&f)?, 0, Sharing::Shared)
            .map_err(|_| "map")?;
        let _active = crate::sched::UserSpaceGuard::enter(space.clone());
        kassert_eq!(copy_to_user(base, b"0"), Ok(()));
        kassert_eq!(copy_to_user(base + P, b"1"), Ok(()));

        release_order::start();
        let downgraded = space.mprotect(base, P, Prot::READ);
        let steps = release_order::stop();
        kassert!(downgraded.is_ok());
        kassert!(file_vm_ordered(&steps, f0));
        kassert_eq!(steps.len(), 3);

        // Both pages writable again: the walk clears and flushes both
        // before either guard goes.
        space.mprotect(base, P, RW).map_err(|_| "mprotect rw")?;
        kassert_eq!(copy_to_user(base, b"0"), Ok(()));
        release_order::start();
        let unmapped = space.munmap(base, 2 * P);
        let steps = release_order::stop();
        kassert!(unmapped.is_ok());
        kassert!(file_vm_ordered(&steps, f0) && file_vm_ordered(&steps, f1));
        kassert!(file_vm_released_after_the_walk(&steps));
        kassert_eq!((p0.page().writable_maps(), p1.page().writable_maps()), (0, 0));

        // A read-only page is cleared and flushed, with nothing to release.
        let ro = space.mmap_file(MapPlace::Hint(None), P, Prot::READ, file_vm_open(&f)?, 0, Sharing::Shared);
        let ro = ro.map_err(|_| "map ro")?;
        kassert_eq!(space.handle_page_fault(ro, 0), FaultResult::Resolved);
        release_order::start();
        let unmapped = space.munmap(ro, P);
        let steps = release_order::stop();
        kassert!(unmapped.is_ok());
        kassert_eq!(steps, [(f0, Step::Cleared), (f0, Step::Flushed)]);
        Ok(())
    }
);

crate::ktest!(
    fn file_vm_refused_page_is_a_bus_error_warned_once_per_streak() {
        // A hole the tmpfs quota can't give a page (`NoSpc`) is a bus error.
        // A ring-3 fault warns once per streak of the same file and error; a
        // kernel user copy's fault (no user bit) doesn't warn; a successful
        // fault on the file ends the streak, so the next failure warns again.
        use crate::vfs::page_cache::ktest_files::TmpfsVolume;
        use crate::{kassert, kassert_eq};
        const P: u64 = PAGE_SIZE;
        const USER_STORE: u64 = (1 << 2) | (1 << 1);
        const KERNEL_STORE: u64 = 1 << 1;
        const USER_LOAD: u64 = 1 << 2;
        let vol = TmpfsVolume::new(178, 2 * P as usize);
        let f = vol.root.create(b"holes.bin", 0o644).map_err(|_| "create")?;
        f.truncate(4 * P).map_err(|_| "truncate")?; // four holes, room for two pages
        let ino = f.ino();
        let warning = alloc::format!("inode {:?}:{ino} can't be mapped (NoSpc): bus error", f.dev_id());
        let count = || file_vm_logged(&warning);
        let before = count();
        let space = Arc::new(AddressSpace::new().map_err(|_| "AddressSpace::new")?);
        let base = space.mmap_file(MapPlace::Hint(None), 4 * P, RW, file_vm_open(&f)?, 0, Sharing::Shared);
        let base = base.map_err(|_| "map")?;
        kassert_eq!(space.handle_page_fault(base, USER_STORE), FaultResult::Resolved);
        kassert_eq!(space.handle_page_fault(base + P, USER_STORE), FaultResult::Resolved);
        // A kernel copy's fault, with no streak yet: debug only.
        kassert_eq!(space.handle_page_fault(base + 2 * P, KERNEL_STORE), FaultResult::BusError);
        kassert_eq!(count(), before);
        kassert_eq!(space.handle_page_fault(base + 2 * P, USER_STORE), FaultResult::BusError);
        kassert_eq!(space.handle_page_fault(base + 3 * P, USER_STORE), FaultResult::BusError);
        kassert_eq!(count(), before + 1); // one streak, one warning
        kassert!(file_vm_leaf(&space, base + 2 * P).is_none());
        // A second mapping of page 0 faults in fine: the streak is over.
        let again = space.mmap_file(MapPlace::Hint(None), P, Prot::READ, file_vm_open(&f)?, 0, Sharing::Shared);
        let again = again.map_err(|_| "map again")?;
        kassert_eq!(space.handle_page_fault(again, USER_LOAD), FaultResult::Resolved);
        kassert_eq!(space.handle_page_fault(base + 3 * P, USER_STORE), FaultResult::BusError);
        kassert_eq!(count(), before + 2);
        Ok(())
    }
);

crate::ktest!(
    fn mprotect_keeps_a_shared_frame_read_only() {
        use crate::sched::UserSpaceGuard;
        use crate::user::{copy_from_user, copy_to_user};
        use crate::{kassert, kassert_eq};
        const A: u64 = 0x300_0000;
        const S: u64 = A + 4 * PAGE_SIZE;
        let parent = Arc::new(AddressSpace::new().map_err(|_| "parent")?);
        parent.map_eager_with_bytes(A, 2 * PAGE_SIZE, Prot::READ, b"original").map_err(|_| "eager")?;
        parent.mprotect(A + PAGE_SIZE, PAGE_SIZE, Prot::NONE).map_err(|_| "none")?;
        // Shared NONE is populated at fork, but must stay inaccessible.
        parent
            .map_anon_at(MapPlace::FixedNoReplace(S), PAGE_SIZE, Prot::NONE, Sharing::Shared)
            .map_err(|_| "shared")?;
        let old = file_vm_leaf(&parent, A).ok_or("parent leaf")?.phys;
        let old_none = ktest_any_leaf(&parent, A + PAGE_SIZE)?.phys;
        let child = parent.fork_copy().map_err(|_| "fork")?;
        let shared_none = ktest_any_leaf(&parent, S)?.phys;
        for space in [&parent, &child] {
            let walk = paging::walk_user(space.root(), VirtAddr::new(S).unwrap());
            let leaf = walk.prot_none_mapping().ok_or("shared NONE became present")?;
            kassert_eq!(leaf.phys, shared_none);
            kassert!(!leaf.present && !leaf.writable && !leaf.shared);
            kassert_eq!(frame::user_count(leaf.phys), 2);
            kassert_eq!(space.handle_page_fault(S, 0), FaultResult::Invalid);
        }
        kassert_eq!(frame::user_count(old), 2);
        parent.mprotect(A, 2 * PAGE_SIZE, Prot::WRITE).map_err(|_| "write-only")?;
        // WRITE-only stays present when CoW clears WRITABLE, including a
        // previously PROT_NONE leaf. Both branches must split on a store.
        for addr in [A, A + PAGE_SIZE] {
            let mapping = file_vm_leaf(&parent, addr).ok_or("not present after mprotect")?;
            kassert!(!mapping.writable);
            kassert_eq!(frame::user_count(mapping.phys), 2);
        }
        {
            let _active = UserSpaceGuard::enter(parent.clone());
            kassert_eq!(copy_to_user(A, b"modified"), Ok(()));
            kassert_eq!(copy_to_user(A + PAGE_SIZE, b"split"), Ok(()));
            kassert!(file_vm_leaf(&parent, A).ok_or("copy")?.phys != old);
            kassert!(file_vm_leaf(&parent, A + PAGE_SIZE).ok_or("NONE copy")?.phys != old_none);
        }
        {
            let _active = UserSpaceGuard::enter(child.clone());
            let mut bytes = [0; 8];
            kassert_eq!(copy_from_user(&mut bytes, A), Ok(()));
            kassert_eq!(&bytes, b"original");
        }
        // Shared-anonymous count 2 must allow the upgrade in both spaces.
        for space in [&parent, &child] {
            space.mprotect(S, PAGE_SIZE, RW).map_err(|_| "shared upgrade")?;
            let mapping = file_vm_leaf(space, S).ok_or("shared leaf")?;
            kassert!(mapping.writable && !mapping.shared);
            kassert_eq!(frame::user_count(mapping.phys), 2);
        }
        kassert_eq!(file_vm_leaf(&parent, S).unwrap().phys, file_vm_leaf(&child, S).unwrap().phys);
        {
            let _active = UserSpaceGuard::enter(parent.clone());
            kassert_eq!(copy_to_user(S, b"shared"), Ok(()));
        }
        {
            let _active = UserSpaceGuard::enter(child.clone());
            let mut bytes = [0; 6];
            kassert_eq!(copy_from_user(&mut bytes, S), Ok(()));
            kassert_eq!(&bytes, b"shared");
        }
        parent.teardown();
        child.teardown();
        Ok(())
    }
);

crate::ktest!(
    fn mprotect_decides_each_leaf() {
        use crate::sched::UserSpaceGuard;
        use crate::user::{copy_from_user, copy_to_user};
        use crate::{kassert, kassert_eq};
        const A: u64 = 0x380_0000;
        const S: u64 = A + 2 * PAGE_SIZE;
        let parent = Arc::new(AddressSpace::new().map_err(|_| "parent")?);
        parent.map_eager_with_bytes(A, 2 * PAGE_SIZE, Prot::READ, b"original").map_err(|_| "eager")?;
        parent
            .map_anon_at(MapPlace::FixedNoReplace(S), PAGE_SIZE, Prot::READ, Sharing::Shared)
            .map_err(|_| "shared")?;
        let child = parent.fork_copy().map_err(|_| "fork")?;
        child.munmap(A, PAGE_SIZE).map_err(|_| "child munmap")?;
        let sole = file_vm_leaf(&parent, A).ok_or("sole")?.phys;
        let both = file_vm_leaf(&parent, A + PAGE_SIZE).ok_or("both")?.phys;
        kassert_eq!((frame::user_count(sole), frame::user_count(both)), (1, 2));
        // One call over a count-1 private leaf, a count-2 private leaf and a
        // shared anonymous leaf: each gets its own answer.
        parent.mprotect(A, 3 * PAGE_SIZE, RW).map_err(|_| "mprotect")?;
        kassert!(file_vm_leaf(&parent, A).ok_or("A")?.writable);
        kassert!(!file_vm_leaf(&parent, A + PAGE_SIZE).ok_or("A+P")?.writable);
        kassert!(file_vm_leaf(&parent, S).ok_or("S")?.writable);
        {
            let _active = UserSpaceGuard::enter(parent.clone());
            kassert_eq!(copy_to_user(A + PAGE_SIZE, b"parent!!"), Ok(()));
        }
        {
            let _active = UserSpaceGuard::enter(child.clone());
            let mut bytes = [0; 8];
            kassert_eq!(copy_from_user(&mut bytes, A + PAGE_SIZE), Ok(()));
            kassert_eq!(bytes, [0; 8]); // the parent's store never reached the shared frame
        }
        parent.teardown();
        child.teardown();
        Ok(())
    }
);

crate::ktest!(
    fn fork_copy_preserves_files_images_and_private_copies() {
        use crate::sched::UserSpaceGuard;
        use crate::user::{copy_from_user, copy_to_user};
        use crate::vfs::page_cache::ktest_files::TmpfsVolume;
        use crate::{kassert, kassert_eq};
        let vol = TmpfsVolume::new(375, 8 * PAGE_SIZE as usize);
        let f = ktest_files::file(&vol.root, "fork.bin", &ktest_files::pattern(3 * PAGE_SIZE as usize, 37))?;
        let elf = build_relro_test_elf();
        let plan = LoadedElf::parse(&elf).map_err(|_| "ELF plan")?;
        let image_file = ImageFile::of(&*f, Arc::from(b"/fork-image".as_slice()));
        let image = ElfImage {
            plan: &plan,
            source: &ImageSource::Bytes(alloc::borrow::Cow::Borrowed(&elf)),
            file: Some(&image_file),
        };
        let (space, _) =
            AddressSpace::from_images(image, None, &[b"fork"], &[], b"fork", [0; 16]).map_err(|_| "load")?;
        let parent = Arc::new(space);
        let heap = parent.brk(0);
        kassert_eq!(parent.brk(heap + 101), heap + 101);
        let base = parent
            .mmap_file(MapPlace::Hint(None), 3 * PAGE_SIZE, RW, file_vm_open(&f)?, 0, Sharing::Private)
            .map_err(|_| "file map")?;
        let shared = parent
            .mmap_file(MapPlace::Hint(None), PAGE_SIZE, RW, file_vm_open(&f)?, 0, Sharing::Shared)
            .map_err(|_| "shared file")?;
        // Nonzero file offset: the first page survives, the second is at EOF.
        let offset = parent
            .mmap_file(MapPlace::Hint(None), 2 * PAGE_SIZE, RW, file_vm_open(&f)?, PAGE_SIZE, Sharing::Private)
            .map_err(|_| "offset file map")?;
        let cached = parent
            .mmap_file(MapPlace::Hint(None), PAGE_SIZE, Prot::READ, file_vm_open(&f)?, 0, Sharing::Private)
            .map_err(|_| "cache file map")?;
        {
            let _active = UserSpaceGuard::enter(parent.clone());
            kassert_eq!(copy_to_user(base, b"private"), Ok(()));
            kassert_eq!(copy_to_user(base + 2 * PAGE_SIZE, b"pastEOF"), Ok(()));
            kassert_eq!(copy_to_user(base + PAGE_SIZE, b"beforeEOF"), Ok(()));
            kassert_eq!(copy_to_user(offset, b"offset"), Ok(()));
            kassert_eq!(copy_to_user(offset + PAGE_SIZE, b"offsetEOF"), Ok(()));
            let mut byte = [0];
            kassert_eq!(copy_from_user(&mut byte, cached), Ok(())); // cache leaf
            kassert_eq!(copy_to_user(shared, b"file"), Ok(())); // writable cache leaf
        }
        let private = file_vm_leaf(&parent, base).ok_or("private leaf")?.phys;
        let past_eof = file_vm_leaf(&parent, base + 2 * PAGE_SIZE).ok_or("past EOF leaf")?.phys;
        let before_eof = file_vm_leaf(&parent, base + PAGE_SIZE).ok_or("before EOF leaf")?.phys;
        let offset_live = file_vm_leaf(&parent, offset).ok_or("offset leaf")?.phys;
        let offset_eof = file_vm_leaf(&parent, offset + PAGE_SIZE).ok_or("offset EOF leaf")?.phys;
        // A shrink whose invalidation has not run yet: fork must skip the
        // stale leaf: it is a private copy, not a cache leaf.
        f.truncate(2 * PAGE_SIZE).map_err(|_| "truncate")?;
        let before_maps = parent.format_maps();
        let child = parent.fork_copy().map_err(|_| "fork")?;
        kassert_eq!(child.format_maps(), before_maps); // VMA, image names, paths, break
        kassert_eq!(child.brk(0), heap + 101);
        kassert_eq!(child.mapped_file_count(), 1);
        kassert_eq!(f.open_count(), 2);
        kassert_eq!(child.file_page_counts(), (0, 0)); // cache leaves never inherited
        kassert!(file_vm_leaf(&child, cached).is_none());
        kassert_eq!(file_vm_leaf(&child, base + PAGE_SIZE).ok_or("child before EOF")?.phys, before_eof);
        kassert_eq!(frame::user_count(before_eof), 2);
        kassert_eq!(file_vm_leaf(&child, offset).ok_or("child offset")?.phys, offset_live);
        kassert_eq!(frame::user_count(offset_live), 2);
        kassert!(file_vm_leaf(&child, offset + PAGE_SIZE).is_none());
        kassert_eq!(frame::user_count(offset_eof), 1);
        kassert!(file_vm_leaf(&child, shared).is_none());
        kassert!(file_vm_leaf(&child, base + 2 * PAGE_SIZE).is_none());
        kassert_eq!(frame::user_count(past_eof), 1);
        kassert_eq!(frame::user_count(private), 2);
        let code = file_vm_leaf(&child, 0x40_1000).ok_or("child text")?;
        kassert!(code.executable && !code.writable);
        kassert_eq!(frame::user_count(code.phys), 2);
        child.mprotect(base, PAGE_SIZE, RW).map_err(|_| "mprotect private file copy")?;
        kassert!(!file_vm_leaf(&child, base).ok_or("child private leaf")?.writable);
        {
            let _active = UserSpaceGuard::enter(child.clone());
            let mut bytes = [0; 7];
            kassert_eq!(copy_from_user(&mut bytes, base), Ok(()));
            kassert_eq!(&bytes, b"private");
            kassert_eq!(copy_to_user(base, b"child!!"), Ok(()));
            kassert!(file_vm_leaf(&child, base).ok_or("child split")?.phys != private);
            kassert_eq!(child.handle_page_fault(base + 2 * PAGE_SIZE, 0), FaultResult::BusError);
            kassert_eq!(copy_from_user(&mut bytes[..4], shared), Ok(()));
            kassert_eq!(&bytes[..4], b"file");
            kassert_eq!(file_vm_leaf(&child, shared).unwrap().phys, file_vm_leaf(&parent, shared).unwrap().phys);
            kassert_eq!(frame::user_count(file_vm_leaf(&child, shared).unwrap().phys), 0);
        }
        {
            let _active = UserSpaceGuard::enter(parent.clone());
            let mut bytes = [0; 7];
            kassert_eq!(copy_from_user(&mut bytes, base), Ok(()));
            kassert_eq!(&bytes, b"private");
        }
        parent.teardown();
        kassert_eq!(f.open_count(), 1);
        kassert!(!frame::is_allocated(private) && !frame::is_allocated(past_eof));
        kassert_eq!(frame::user_count(code.phys), 1);
        child.teardown();
        kassert_eq!(f.open_count(), 0);
        kassert!(!frame::is_allocated(code.phys));
        Ok(())
    }
);

/// Signals `stop` and joins a spawned ktest helper thread on drop, so early
/// errors never leave a background thread behind.
#[cfg(feature = "ktest")]
struct StopOnDrop {
    stop: Arc<core::sync::atomic::AtomicBool>,
    handle: Option<crate::sched::JoinHandle>,
}

#[cfg(feature = "ktest")]
impl Drop for StopOnDrop {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(h) = self.handle.take() {
            h.join();
        }
    }
}

crate::ktest!(
    fn truncate_unmaps_every_address_space() {
        // B7: two spaces mapping one file, the second loaded on another CPU,
        // and a third forked from the first. A shrinking truncate invalidates
        // every mapped space (shooting down the remote CPU's TLB) so none
        // reads past the new end; unregistration on munmap/MAP_FIXED/brk/
        // teardown/Drop is checked in
        // `file_vm_writable_maps_balance_through_mprotect_unmap_and_teardown`.
        use crate::arch::MAX_CPUS;
        use crate::sched::{self, UserSpaceGuard};
        use crate::smp;
        use crate::user::{copy_from_user, copy_to_user, read_u32_nofault};
        use crate::{kassert, kassert_eq};
        use core::sync::atomic::AtomicBool;
        const P: u64 = PAGE_SIZE;

        let here = arch::cpu_index();
        let other_cpu = (0..MAX_CPUS)
            .find(|&cpu| cpu != here && smp::online_lapic_id(cpu).is_some())
            .ok_or("truncate_unmaps_every_address_space requires at least 2 online CPUs")?;

        struct ProbeState {
            primed: AtomicBool,
            after: AtomicBool,
            stop: Arc<AtomicBool>,
            bad_before: AtomicBool,
            stale_after: AtomicU64,
            faults_after: AtomicU64,
            p0_ok_after: AtomicBool,
            p1_tail_zero: AtomicBool,
        }

        const P0_VAL: u32 = 0x1122_3344;
        const P1_VAL: u32 = 0x5566_7788;
        const P2_VAL: u32 = 0x99aa_bbcc;

        file_vm_on_both(378, |dir| {
            let mut data = ktest_files::pattern(3 * P as usize, 41);
            data[0..4].copy_from_slice(&P0_VAL.to_ne_bytes());
            data[P as usize..P as usize + 4].copy_from_slice(&P1_VAL.to_ne_bytes());
            data[2 * P as usize..2 * P as usize + 4].copy_from_slice(&P2_VAL.to_ne_bytes());
            let f = ktest_files::file(dir, "multi.bin", &data)?;
            let page2_cache = mmap_page(&f, 2).map_err(|_| "mmap_page 2")?;

            // Space 1: shared + private mappings of `f`, faulted in on all 3 pages.
            let space1 = Arc::new(AddressSpace::new().map_err(|_| "space1")?);
            let s1_shared = space1
                .mmap_file(MapPlace::Hint(None), 3 * P, RW, file_vm_open(&f)?, 0, Sharing::Shared)
                .map_err(|_| "s1 shared")?;
            let s1_priv = space1
                .mmap_file(MapPlace::Hint(None), 3 * P, RW, file_vm_open(&f)?, 0, Sharing::Private)
                .map_err(|_| "s1 priv")?;
            {
                let _active = UserSpaceGuard::enter(space1.clone());
                let mut tmp = [0u8; 4];
                for p in 0..3 {
                    kassert_eq!(copy_from_user(&mut tmp, s1_shared + p * P), Ok(()));
                }
                kassert_eq!(copy_to_user(s1_shared + 2 * P + 16, b"wr"), Ok(()));
                kassert_eq!(copy_to_user(s1_priv, b"keep-p0"), Ok(()));
                kassert_eq!(copy_to_user(s1_priv + 2 * P, b"priv-p2"), Ok(()));
            }
            let priv_p2_phys = file_vm_leaf(&space1, s1_priv + 2 * P).ok_or("s1 priv p2")?.phys;

            // Space 2: shared mapping of `f`, loaded and probed on `other_cpu`.
            let space2 = Arc::new(AddressSpace::new().map_err(|_| "space2")?);
            let s2_shared = space2
                .mmap_file(MapPlace::Hint(None), 3 * P, RW, file_vm_open(&f)?, 0, Sharing::Shared)
                .map_err(|_| "s2 shared")?;
            for p in 0..3 {
                kassert_eq!(space2.handle_page_fault(s2_shared + p * P, 0), FaultResult::Resolved);
            }

            // Space 3: forked from space 1 (inherits the private CoW copy of
            // page 2 and registers itself on `f`'s mappers list during fork).
            let space3 = space1.fork_copy().map_err(|_| "fork space3")?;
            kassert_eq!(frame::user_count(priv_p2_phys), 2);
            kassert_eq!(space3.handle_page_fault(s1_shared + 2 * P, 1 << 1), FaultResult::Resolved);
            kassert_eq!(file_vm_mappers(&*f), 3);

            // Launch the probe thread on `other_cpu` holding `space2` in CR3.
            let stop = Arc::new(AtomicBool::new(false));
            let probe = Arc::new(ProbeState {
                primed: AtomicBool::new(false),
                after: AtomicBool::new(false),
                stop: stop.clone(),
                bad_before: AtomicBool::new(false),
                stale_after: AtomicU64::new(0),
                faults_after: AtomicU64::new(0),
                p0_ok_after: AtomicBool::new(false),
                p1_tail_zero: AtomicBool::new(false),
            });

            let guard = StopOnDrop {
                stop,
                handle: Some({
                    let probe = probe.clone();
                    sched::spawn_user_pinned_on(other_cpu, "b7-trunc-probe", space2.clone(), move || {
                        let ok0 = read_u32_nofault(s2_shared) == Ok(P0_VAL);
                        let ok1 = read_u32_nofault(s2_shared + P) == Ok(P1_VAL);
                        let ok2 = read_u32_nofault(s2_shared + 2 * P) == Ok(P2_VAL);
                        if !(ok0 && ok1 && ok2) {
                            probe.bad_before.store(true, Ordering::Release);
                        }
                        probe.primed.store(true, Ordering::Release);
                        while !probe.stop.load(Ordering::Acquire) {
                            let retired = probe.after.load(Ordering::Acquire);
                            match read_u32_nofault(s2_shared + 2 * P) {
                                Ok(_) if retired => {
                                    probe.stale_after.fetch_add(1, Ordering::Relaxed);
                                }
                                Err(_) if retired => {
                                    probe.faults_after.fetch_add(1, Ordering::Relaxed);
                                }
                                _ => {}
                            }
                            core::hint::spin_loop();
                        }
                        probe.p0_ok_after.store(read_u32_nofault(s2_shared) == Ok(P0_VAL), Ordering::Release);
                        probe.p1_tail_zero.store(read_u32_nofault(s2_shared + P + 200) == Ok(0), Ordering::Release);
                    })
                    .map_err(|_| "spawn remote probe")?
                }),
            };

            let remote_bit = 1u64 << other_cpu;
            kassert!(sched::yield_until(|| probe.primed.load(Ordering::Acquire)));
            kassert!(sched::yield_until(|| (space2.active_mask() & remote_bit) == remote_bit));

            // Shrink `f` to `P + 100` (pages 0 and 1 survive; page 2 is past EOF)
            // and invalidate every space mapping `f`.
            f.truncate(P + 100).map_err(|_| "truncate")?;
            let unmapped = invalidate_file_mappings(&*f, P + 100);
            // space1: shared p2 + priv p2 (2); space2: shared p2 (1); space3: shared p2 + priv p2 (2).
            kassert_eq!(unmapped, 5);

            // Confirm `other_cpu` immediately faults on page 2 with zero stale
            // reads (proving B5's cross-CPU shootdown ran before we returned).
            probe.after.store(true, Ordering::Release);
            kassert!(sched::yield_until(|| {
                probe.faults_after.load(Ordering::Relaxed) >= 32 || probe.stale_after.load(Ordering::Relaxed) > 0
            }));
            drop(guard);
            kassert!(!probe.bad_before.load(Ordering::Acquire));
            kassert_eq!(probe.stale_after.load(Ordering::Relaxed), 0);
            kassert!(probe.faults_after.load(Ordering::Relaxed) >= 32);
            kassert!(probe.p0_ok_after.load(Ordering::Acquire));
            kassert!(probe.p1_tail_zero.load(Ordering::Acquire));

            // Every space's page-2 leaves (shared and private CoW) are gone
            // and refault into `BusError`.
            for (space, va) in [
                (&space1, s1_shared + 2 * P),
                (&space1, s1_priv + 2 * P),
                (&space2, s2_shared + 2 * P),
                (&space3, s1_shared + 2 * P),
                (&space3, s1_priv + 2 * P),
            ] {
                kassert!(file_vm_leaf(space, va).is_none());
                kassert_eq!(space.handle_page_fault(va, 0), FaultResult::BusError);
            }

            // Spaces 1 and 3 released the shared CoW copy of page 2 and the
            // writable page-cache guards of page 2 (space 2 only read).
            kassert!(!frame::is_allocated(priv_p2_phys));
            kassert_eq!(page2_cache.page().writable_maps(), 0);

            space1.teardown();
            space2.teardown();
            space3.teardown();
            kassert_eq!(file_vm_mappers(&*f), 0);
            kassert_eq!(f.open_count(), 0);
            Ok(())
        })
    }
);

crate::ktest!(
    fn fork_racing_a_truncate_copies_nothing_past_eof() {
        // B7: a parent with private (copied) and page-cache leaves of a file
        // past the new end. Both orderings around the `Mappers` lock leave the
        // child with nothing past EOF:
        // 1. Truncate's mapper snapshot runs through `before_register` while
        //    `fork_copy` holds both Lock 1s BEFORE registering the child (the
        //    truncate thread asserts `mappers == 1`, so its snapshot misses
        //    the child); `fork_copy` registers the child (`before_sizes`
        //    asserts `mappers == 2`), then reads the new size and copies
        //    nothing at or past EOF.
        // 2. Truncate's mapper snapshot runs through `after_precount` AFTER
        //    `fork_copy` registered the child (`mappers == 2`) and read the
        //    old size; the snapshot includes the child and invalidates each
        //    space in turn, each once `fork_copy` releases its Lock 1.
        use crate::sched::{self, UserSpaceGuard};
        use crate::user::{CopyError, copy_from_user, copy_to_user};
        use crate::{kassert, kassert_eq};
        use core::sync::atomic::{AtomicBool, AtomicU8};
        const P: u64 = PAGE_SIZE;

        const TRUNC_PENDING: u8 = 0;
        const TRUNC_OK: u8 = 1;
        const TRUNC_BAD_MAPPERS: u8 = 2;
        const TRUNC_ERR: u8 = 3;

        file_vm_on_both(380, |dir| {
            let f = ktest_files::file(dir, "race.bin", &ktest_files::pattern(3 * P as usize, 43))?;
            let parent = Arc::new(AddressSpace::new().map_err(|_| "parent")?);
            let priv_va = parent
                .mmap_file(MapPlace::Hint(None), 3 * P, RW, file_vm_open(&f)?, 0, Sharing::Private)
                .map_err(|_| "map priv")?;
            let shared_va = parent
                .mmap_file(MapPlace::Hint(None), 3 * P, RW, file_vm_open(&f)?, 0, Sharing::Shared)
                .map_err(|_| "map shared")?;
            let priv_ro_va = parent
                .mmap_file(MapPlace::Hint(None), 3 * P, RW, file_vm_open(&f)?, 0, Sharing::Private)
                .map_err(|_| "map priv ro")?;
            let past_vas = [priv_va + 2 * P, shared_va + 2 * P, priv_ro_va + 2 * P];

            // Populate private (copied) and page-cache (both writable and
            // read-only) leaves past the future EOF (page 2), plus page 0 below.
            let populate = |parent: &Arc<AddressSpace>| -> Result<PhysFrame, &'static str> {
                let _active = UserSpaceGuard::enter(parent.clone());
                let mut byte = [0u8; 1];
                kassert_eq!(copy_to_user(priv_va, b"below-eof"), Ok(()));
                kassert_eq!(copy_to_user(priv_va + 2 * P, b"past-priv"), Ok(()));
                kassert_eq!(copy_to_user(shared_va + 2 * P, b"past-shrd"), Ok(()));
                kassert_eq!(copy_from_user(&mut byte, priv_ro_va + 2 * P), Ok(()));
                let priv_leaf = file_vm_leaf(parent, priv_va + 2 * P).ok_or("priv p2 missing")?;
                kassert!(!priv_leaf.shared);
                kassert!(file_vm_leaf(parent, shared_va + 2 * P).ok_or("shared p2 missing")?.shared);
                kassert!(file_vm_leaf(parent, priv_ro_va + 2 * P).ok_or("priv ro p2 missing")?.shared);
                Ok(priv_leaf.phys)
            };

            let check_past_eof = |child: &Arc<AddressSpace>, priv_p2_phys: PhysFrame| -> Result<(), &'static str> {
                for space in [child, &parent] {
                    for va in past_vas {
                        kassert!(file_vm_leaf(space, va).is_none());
                        kassert_eq!(space.handle_page_fault(va, 0), FaultResult::BusError);
                    }
                    let _active = UserSpaceGuard::enter(space.clone());
                    let mut buf = [0u8; 9];
                    for va in past_vas {
                        kassert_eq!(copy_from_user(&mut buf, va), Err(CopyError::Fault));
                    }
                    kassert_eq!(copy_from_user(&mut buf, priv_va), Ok(()));
                    kassert_eq!(&buf, b"below-eof");
                }
                kassert!(!frame::is_allocated(priv_p2_phys));
                Ok(())
            };

            type ForkFn<'a> = &'a mut dyn FnMut(&dyn Fn()) -> Result<Arc<AddressSpace>, &'static str>;

            // Spawn a truncate thread synchronized with `run_fork`'s seam: the
            // thread checks `file_vm_mappers(&*f) == expected_mappers`, shrinks
            // `f` to `P`, snapshots mappers, wakes the seam, and invalidates
            // each space in turn once `fork_copy` releases its Lock 1.
            let race =
                |name: &'static str, expected_mappers: usize, run_fork: ForkFn<'_>| -> Result<(), &'static str> {
                    let priv_p2_phys = populate(&parent)?;
                    let at_seam = Arc::new(AtomicBool::new(false));
                    let snapshot_done = Arc::new(AtomicBool::new(false));
                    let stop = Arc::new(AtomicBool::new(false));
                    let status = Arc::new(AtomicU8::new(TRUNC_PENDING));

                    let trunc = StopOnDrop {
                        stop: stop.clone(),
                        handle: Some({
                            let f = f.clone();
                            let at_seam = at_seam.clone();
                            let snapshot_done = snapshot_done.clone();
                            let stop = stop.clone();
                            let status = status.clone();
                            sched::spawn(name, move || {
                                while !at_seam.load(Ordering::Acquire) && !stop.load(Ordering::Acquire) {
                                    sched::yield_now();
                                }
                                if stop.load(Ordering::Acquire) {
                                    return;
                                }
                                if file_vm_mappers(&*f) != expected_mappers {
                                    status.store(TRUNC_BAD_MAPPERS, Ordering::Release);
                                    snapshot_done.store(true, Ordering::Release);
                                    return;
                                }
                                if f.truncate(P).is_err() {
                                    status.store(TRUNC_ERR, Ordering::Release);
                                    snapshot_done.store(true, Ordering::Release);
                                    return;
                                }
                                invalidate_file_mappings_with(&*f, P, || {
                                    snapshot_done.store(true, Ordering::Release);
                                });
                                status.store(TRUNC_OK, Ordering::Release);
                            })
                            .map_err(|_| "spawn trunc")?
                        }),
                    };

                    let trigger = || {
                        at_seam.store(true, Ordering::Release);
                        while !snapshot_done.load(Ordering::Acquire) {
                            sched::yield_now();
                        }
                    };
                    let child = run_fork(&trigger)?;
                    drop(trunc);
                    kassert_eq!(status.load(Ordering::Acquire), TRUNC_OK);
                    check_past_eof(&child, priv_p2_phys)?;
                    child.teardown();
                    Ok(())
                };

            // Phase 1: snapshot BEFORE registration (`mappers == 1`), then
            // register (`mappers == 2` before `file_sizes`), then read new EOF.
            let mut mappers_before_sizes = 0;
            race("b7-race-trunc1", 1, &mut |trigger| {
                parent
                    .fork_copy_inner(ForkSeams {
                        before_register: trigger,
                        before_sizes: || mappers_before_sizes = file_vm_mappers(&*f),
                        after_precount: || {},
                    })
                    .map_err(|_| "fork_copy phase 1")
            })?;
            kassert_eq!(mappers_before_sizes, 2);

            // Phase 2: snapshot AFTER registration (`mappers == 2`) and after
            // `file_sizes` saw the old size; invalidates each space in turn,
            // each once `fork_copy` releases its Lock 1.
            f.truncate(3 * P).map_err(|_| "regrow")?;
            kassert_eq!(f.write_at(2 * P, b"regrown-page2").map_err(|_| "rewrite")?, 13);
            race("b7-race-trunc2", 2, &mut |trigger| {
                parent.fork_copy_after_precount(trigger).map_err(|_| "fork_copy phase 2")
            })?;

            parent.teardown();
            kassert_eq!(file_vm_mappers(&*f), 0);
            kassert_eq!(f.open_count(), 0);
            Ok(())
        })
    }
);

crate::ktest!(
    fn mappers_multiset_keeps_registration_across_late_drop() {
        // B7: `Mappers` is a multiset so a `Mapped` dropped after Lock 1 is
        // released (from an earlier `munmap`) never erases a fresh registration
        // from a concurrent `mmap_file` of the same file.
        use crate::{kassert, kassert_eq};
        const P: u64 = PAGE_SIZE;

        file_vm_on_both(382, |dir| {
            let f = ktest_files::file(dir, "multiset.bin", &ktest_files::pattern(2 * P as usize, 47))?;
            let space = Arc::new(AddressSpace::new().map_err(|_| "AddressSpace::new")?);

            // Stands in for an earlier `munmap`'s removed `Mapped` entry that
            // has not yet run `Drop` when a concurrent `mmap_file` registers.
            let late = Mapped::register(file_vm_open(&f)?.kept, &space);
            let va = space
                .mmap_file(MapPlace::Hint(None), 2 * P, RW, file_vm_open(&f)?, 0, Sharing::Shared)
                .map_err(|_| "mmap_file")?;
            kassert_eq!(file_vm_mappers(&*f), 2);

            drop(late);
            kassert_eq!(file_vm_mappers(&*f), 1);

            kassert_eq!(space.handle_page_fault(va + P, 1 << 1), FaultResult::Resolved);
            kassert!(file_vm_leaf(&space, va + P).is_some());
            f.truncate(P).map_err(|_| "truncate")?;
            kassert_eq!(invalidate_file_mappings(&*f, P), 1);
            kassert!(file_vm_leaf(&space, va + P).is_none());
            kassert_eq!(space.handle_page_fault(va + P, 0), FaultResult::BusError);

            space.teardown();
            kassert_eq!(file_vm_mappers(&*f), 0);
            Ok(())
        })
    }
);
