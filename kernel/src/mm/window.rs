//! Mapping fresh frames into the heap and stack windows, all or nothing:
//! what the heap ([`super::heap`]) and the kernel stacks ([`super::stack`])
//! share on top of the `unsafe` [`paging::map`]/[`paging::unmap`].

use core::fmt;
use core::ops::Range;

use lumen_mm::paging::Attrs;
use lumen_mm::{FRAME_SIZE, VirtAddr};

use super::{frame, paging};

/// Why [`map_fresh`] failed (everything it mapped is undone).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MapError {
    /// The frame allocator is empty.
    OutOfFrames,
    /// The page tables refused the mapping.
    Map(paging::VmError),
}

impl fmt::Display for MapError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            MapError::OutOfFrames => f.write_str("out of physical frames"),
            MapError::Map(e) => write!(f, "mapping failed: {e}"),
        }
    }
}

/// Map `pages` (page-aligned, inside `owner`) to fresh frames, read-write
/// and no-execute. All or nothing: on failure every page mapped so far is
/// unmapped and its frame freed (page tables created on the way stay; tables
/// are never freed).
///
/// # Safety
/// The caller owns `owner` (a part of the heap or stack window nothing else
/// maps) and nothing in `pages` is mapped or in use.
pub unsafe fn map_fresh(owner: &Range<u64>, pages: Range<u64>) -> Result<(), MapError> {
    check_bounds(owner, &pages);
    for virt in pages.clone().step_by(FRAME_SIZE as usize) {
        // SAFETY: the caller owns `virt` and nothing uses it.
        if let Err(e) = unsafe { map_one(virt) } {
            // SAFETY: we mapped these pages just now and handed out nothing
            // in them.
            unsafe { unmap_and_free(owner, pages.start..virt, Some(e)) };
            return Err(e);
        }
    }
    Ok(())
}

/// Unmap `pages` (inside `owner`) and free their frames. A failure is a
/// kernel bug (the pages are the caller's and mapped), so it panics, naming
/// `cause` if this undoes a failed [`map_fresh`].
///
/// # Safety
/// The caller owns `pages`, they are mapped, and nothing refers to memory
/// through them any more.
pub unsafe fn unmap_and_free(owner: &Range<u64>, pages: Range<u64>, cause: Option<MapError>) {
    check_bounds(owner, &pages);
    for virt in pages.step_by(FRAME_SIZE as usize) {
        let v = VirtAddr::new(virt).expect("window addresses are canonical");
        // SAFETY: the caller owns the page and nothing refers to it.
        let m = unsafe { paging::unmap(v) }.unwrap_or_else(|e| release_failed(cause, format_args!("unmap {v}: {e}")));
        frame::free(m.phys).unwrap_or_else(|e| release_failed(cause, format_args!("free {}: {e:?}", m.phys)));
    }
}

/// A page the caller owns couldn't be unmapped or freed: a kernel bug.
fn release_failed(cause: Option<MapError>, what: fmt::Arguments<'_>) -> ! {
    match cause {
        Some(c) => panic!("mm: rollback after `{c}` failed: {what}"),
        None => panic!("mm: releasing owned pages failed: {what}"),
    }
}

/// Map a fresh frame at `virt`.
///
/// # Safety
/// The caller owns the page at `virt` and nothing uses it.
unsafe fn map_one(virt: u64) -> Result<(), MapError> {
    let v = VirtAddr::new(virt).expect("window addresses are canonical");
    let frame = frame::alloc().ok_or(MapError::OutOfFrames)?;
    // SAFETY: the frame was just allocated for the caller, who owns the page.
    if let Err(e) = unsafe { paging::map(v, frame, Attrs::DATA) } {
        frame::free(frame).unwrap_or_else(|fe| panic!("mm: freeing {frame} after `{e}` failed: {fe:?}"));
        return Err(MapError::Map(e));
    }
    Ok(())
}

/// `pages` is page-aligned and inside `owner`: a caller bug otherwise.
fn check_bounds(owner: &Range<u64>, pages: &Range<u64>) {
    assert!(
        pages.start.is_multiple_of(FRAME_SIZE)
            && pages.end.is_multiple_of(FRAME_SIZE)
            && owner.start <= pages.start
            && pages.start <= pages.end
            && pages.end <= owner.end,
        "pages {:#x}..{:#x} not page-aligned inside {:#x}..{:#x}",
        pages.start,
        pages.end,
        owner.start,
        owner.end
    );
}
