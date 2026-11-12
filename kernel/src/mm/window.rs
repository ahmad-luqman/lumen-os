//! Mapping fresh frames into a window, all or nothing: what the heap
//! ([`super::heap`]) and the kernel stacks ([`super::stack`]) share on top of
//! [`paging::map`]/[`paging::unmap`]. The caller's [`Window`] token is what
//! says the pages are its own.

use core::fmt;
use core::ops::Range;

use lumen_mm::paging::Attrs;
use lumen_mm::window::Window;
use lumen_mm::{FRAME_SIZE, VirtAddr};

use super::{frame, paging};

/// Why [`map_fresh`] failed (everything it mapped is undone).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WindowError {
    /// The frame allocator is empty.
    OutOfFrames,
    /// The page tables refused the mapping.
    Map(paging::VmError),
}

impl WindowError {
    /// Is this memory running out (no frame for a page or for a page table),
    /// rather than a kernel bug (the page tables refusing a mapping the
    /// caller owns)?
    pub fn is_exhaustion(self) -> bool {
        match self {
            WindowError::OutOfFrames => true,
            WindowError::Map(e) => e == paging::VmError::Map(lumen_mm::paging::MapError::OutOfMemory),
        }
    }
}

impl fmt::Display for WindowError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            WindowError::OutOfFrames => f.write_str("out of physical frames"),
            WindowError::Map(e) => write!(f, "mapping failed: {e}"),
        }
    }
}

/// Map `pages` (page-aligned, inside `window`) to fresh frames, read-write
/// and no-execute. All or nothing: on failure every page mapped so far is
/// unmapped and its frame freed (page tables created on the way stay; tables
/// are never freed). A page already mapped is a [`WindowError::Map`].
pub fn map_fresh(window: &mut Window, pages: Range<u64>) -> Result<(), WindowError> {
    check_bounds(window, &pages);
    for virt in pages.clone().step_by(FRAME_SIZE as usize) {
        if let Err(e) = map_one(window, virt) {
            // SAFETY: we mapped these pages just now and handed out nothing
            // in them.
            unsafe { release(window, pages.start..virt, Some(e)) };
            return Err(e);
        }
    }
    Ok(())
}

/// Unmap `pages` (inside `window`) and free their frames. A failure is a
/// kernel bug (the pages are the window's and mapped), so it panics.
///
/// # Safety
/// `pages` are mapped and nothing refers to memory through them any more.
pub unsafe fn unmap_and_free(window: &mut Window, pages: Range<u64>) {
    // SAFETY: upheld by the caller.
    unsafe { release(window, pages, None) }
}

/// [`unmap_and_free`], naming `cause` in the panic if this undoes a failed
/// [`map_fresh`].
///
/// # Safety
/// As for [`unmap_and_free`].
unsafe fn release(window: &mut Window, pages: Range<u64>, cause: Option<WindowError>) {
    check_bounds(window, &pages);
    for virt in pages.step_by(FRAME_SIZE as usize) {
        let v = VirtAddr::new(virt).expect("window addresses are canonical");
        // SAFETY: the page is the window's and nothing refers to it. The
        // frame comes back owned; dropping it frees it.
        let freed = unsafe { paging::unmap(window, v) };
        if let Err(e) = freed {
            match cause {
                Some(c) => panic!("mm: rollback after `{c}` failed: unmap {v}: {e}"),
                None => panic!("mm: releasing owned pages failed: unmap {v}: {e}"),
            }
        }
    }
}

/// Map a fresh frame at `virt` (a page of `window`). On failure the frame
/// comes back and is freed when dropped.
fn map_one(window: &mut Window, virt: u64) -> Result<(), WindowError> {
    let v = VirtAddr::new(virt).expect("window addresses are canonical");
    let frame = frame::alloc_owned().ok_or(WindowError::OutOfFrames)?;
    paging::map(window, v, frame, Attrs::DATA).map_err(|(e, _freed_on_drop)| WindowError::Map(e))
}

/// `pages` is page-aligned and inside `window`: a caller bug otherwise.
fn check_bounds(window: &Window, pages: &Range<u64>) {
    let owner = window.range();
    assert!(
        pages.start.is_multiple_of(FRAME_SIZE) && pages.end.is_multiple_of(FRAME_SIZE) && window.contains(pages),
        "pages {:#x}..{:#x} not page-aligned inside {:#x}..{:#x}",
        pages.start,
        pages.end,
        owner.start,
        owner.end
    );
}

crate::ktest!(
    fn window_errors_are_classified() {
        // Running out (no frame for a page or a table) is an error callers
        // handle; any other refusal by the page tables is a kernel bug.
        use crate::kassert;
        use lumen_mm::paging::MapError as PtError;
        use paging::VmError;
        kassert!(WindowError::OutOfFrames.is_exhaustion());
        kassert!(WindowError::Map(VmError::Map(PtError::OutOfMemory)).is_exhaustion());
        kassert!(!WindowError::Map(VmError::Map(PtError::AlreadyMapped)).is_exhaustion());
        kassert!(!WindowError::Map(VmError::NotWriteBack).is_exhaustion());
        kassert!(!WindowError::Map(VmError::Executable).is_exhaustion());
        Ok(())
    }
);
