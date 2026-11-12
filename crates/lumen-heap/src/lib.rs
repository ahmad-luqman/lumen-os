//! The kernel heap's allocator, kept free of kernel dependencies so it can
//! be tested on the host.
//!
//! # Design
//!
//! A [`Heap`] manages one contiguous span `[base, base + len)` that can only
//! grow at its end ([`Heap::extend`]); the kernel grows it by mapping frames.
//! Free memory is a singly linked list of blocks in address order. Each free
//! block keeps its size and the offset of the next free block in its first
//! 16 bytes. Allocated blocks carry no header: `GlobalAlloc::dealloc` gets the
//! same `Layout` that `alloc` got, and [`block_size`] maps both to the same
//! block size.
//!
//! - Every block's offset and size is a multiple of [`GRANULE`] (16 bytes,
//!   the size of a free-list node), so any leftover piece can hold a node.
//! - `alloc` is first fit: the first free block that holds the size at the
//!   requested alignment. The padding in front of the aligned address stays a
//!   free block, and so does the tail.
//! - `dealloc` inserts the block in address order and merges it with free
//!   neighbours, so no two free blocks are ever adjacent. It refuses a block
//!   outside the heap, misaligned, or overlapping memory that is free right
//!   now, changing nothing. That catches a double free while the block is
//!   still free and a `Layout` larger than the original; it can't catch a
//!   double free after the memory was handed out again, or a smaller
//!   `Layout` (which leaks the difference; [`Heap::check`] reports the leak
//!   once nothing is allocated).
//! - Every walk checks each free block it reads (inside the heap, links
//!   going up), so a corrupt list is a panic, not a hang or a stray access.
//!
//! Both operations walk the free list: O(free blocks). Size classes for small
//! objects can sit in front of this later, behind the same interface.
//!
//! The list stores offsets from `base`, and every pointer handed out is
//! derived from `base`, so the host tests run with real pointer provenance.

#![no_std]

use core::alloc::Layout;
use core::fmt;
use core::ptr::NonNull;

/// Every block's offset and size is a multiple of this, and every allocation
/// is at least this aligned.
pub const GRANULE: usize = 16;

/// Ends the free list.
const NIL: usize = usize::MAX;

/// The start of a free block.
#[repr(C)]
#[derive(Clone, Copy)]
struct Node {
    /// Bytes in this free block (a multiple of [`GRANULE`]).
    size: usize,
    /// Offset of the next free block, which starts above this one, or [`NIL`].
    next: usize,
}

const _: () = assert!(size_of::<Node>() <= GRANULE && align_of::<Node>() <= GRANULE);

/// The size of the block that holds an allocation of `layout`: its size
/// rounded up to a multiple of [`GRANULE`] (at least one). `alloc` and
/// `dealloc` both use this, so a block is freed with the size it was given.
/// (`None` is defensive: a `Layout`'s size is at most `isize::MAX`.)
pub fn block_size(layout: Layout) -> Option<usize> {
    layout.size().max(1).checked_next_multiple_of(GRANULE)
}

/// The alignment a block for `layout` gets: at least [`GRANULE`]. Shared by
/// `alloc` and [`growth_for`], whose guarantee depends on agreeing with it.
fn block_align(layout: Layout) -> usize {
    layout.align().max(GRANULE)
}

/// How many bytes [`Heap::extend`] must add so that allocating `layout`
/// succeeds whatever the free list looks like: the block size plus the
/// worst-case padding to reach the alignment (the added span starts at a
/// [`GRANULE`]-aligned address).
pub fn growth_for(layout: Layout) -> Option<usize> {
    block_size(layout)?.checked_add(block_align(layout) - GRANULE)
}

/// Counters kept as the heap changes (O(1) to read).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Stats {
    /// Bytes the heap manages (its length).
    pub size: usize,
    /// Bytes in allocated blocks (sizes rounded up by [`block_size`]).
    pub used: usize,
    /// Allocations not yet freed.
    pub live: usize,
}

impl Stats {
    /// Bytes in free blocks (0 if the counters are inconsistent, which
    /// [`Heap::check`] reports).
    pub fn free(&self) -> usize {
        self.size.saturating_sub(self.used)
    }
}

/// What a walk of the free list found ([`Heap::check`]).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FreeList {
    pub blocks: usize,
    pub bytes: usize,
    /// The largest free block: the biggest allocation (at [`GRANULE`]
    /// alignment) that succeeds without growing the heap.
    pub largest: usize,
}

/// Why [`Heap::dealloc`] refused a block. The heap is unchanged.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FreeError {
    /// The block is not inside the heap.
    OutsideHeap,
    /// The pointer is not aligned as its `Layout` requires.
    Misaligned,
    /// The block overlaps memory that is free (a double free, or a `Layout`
    /// larger than the original).
    NotAllocated,
}

impl fmt::Display for FreeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            FreeError::OutsideHeap => "block is not entirely inside the heap",
            FreeError::Misaligned => "pointer is not a block start aligned for its layout",
            FreeError::NotAllocated => "block overlaps free memory (double free or wrong layout)",
        })
    }
}

/// What [`Heap::check`] found wrong (a broken invariant; `offset` is the
/// free block where it showed), or [`CheckError::TooManyBlocks`] from
/// [`Heap::check_bounded`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CheckError {
    /// A link points outside the heap or not at a [`GRANULE`] boundary.
    BadLink { offset: usize },
    /// A block's size is zero, not a multiple of [`GRANULE`], or runs past
    /// the end of the heap.
    BadSize { offset: usize, size: usize },
    /// The block starts inside or below the previous one.
    Unordered { offset: usize },
    /// The block touches the previous one (they should have been merged).
    Adjacent { offset: usize },
    /// The free bytes don't match the heap's size minus the bytes in use.
    Accounting { listed: usize, expected: usize },
    /// Bytes are counted as used but nothing is allocated: a block was freed
    /// with a smaller `Layout` than it was allocated with.
    Leaked { used: usize },
    /// Not an invariant: [`Heap::check_bounded`] stopped at its block limit.
    TooManyBlocks,
}

impl fmt::Display for CheckError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            CheckError::BadLink { offset } => write!(f, "bad link to offset {offset:#x}"),
            CheckError::BadSize { offset, size } => write!(f, "bad size {size:#x} at offset {offset:#x}"),
            CheckError::Unordered { offset } => write!(f, "block at {offset:#x} is out of order or overlaps"),
            CheckError::Adjacent { offset } => write!(f, "block at {offset:#x} touches the previous one"),
            CheckError::Accounting { listed, expected } => {
                write!(f, "free list holds {listed:#x} bytes, expected {expected:#x}")
            }
            CheckError::Leaked { used } => write!(f, "{used:#x} bytes used with nothing allocated"),
            CheckError::TooManyBlocks => f.write_str("walk stopped: too many blocks"),
        }
    }
}

/// A heap over one growable span of memory. See the crate docs.
pub struct Heap {
    /// Start of the span; `None` until [`Heap::init`].
    base: Option<NonNull<u8>>,
    len: usize,
    /// Offset of the lowest free block, or [`NIL`].
    head: usize,
    used: usize,
    live: usize,
}

// SAFETY: the heap owns the memory it manages (its `init` and `extend`
// contracts), so moving it to another thread moves that ownership; it is
// only read or written through `&self`/`&mut self`.
unsafe impl Send for Heap {}

impl fmt::Debug for Heap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Heap").field("base", &self.base).field("stats", &self.stats()).finish()
    }
}

impl Default for Heap {
    fn default() -> Self {
        Self::empty()
    }
}

impl Heap {
    /// A heap with no memory: every allocation fails until [`Heap::init`].
    pub const fn empty() -> Self {
        Heap { base: None, len: 0, head: NIL, used: 0, live: 0 }
    }

    /// Start managing `[base, base + len)`, all of it free. `len` may be 0
    /// (then only [`Heap::extend`] gives it memory).
    ///
    /// # Safety
    /// `[base, base + len)` is valid for reads and writes, and nothing else
    /// uses it for as long as the heap exists.
    ///
    /// # Panics
    /// If the heap is already initialized, `base` is not [`GRANULE`]-aligned,
    /// `len` is not a multiple of [`GRANULE`], or the span wraps around.
    pub unsafe fn init(&mut self, base: NonNull<u8>, len: usize) {
        assert!(self.base.is_none(), "heap already initialized");
        assert!(base.addr().get().is_multiple_of(GRANULE), "heap base {base:p} is not {GRANULE}-byte aligned");
        assert!(len.is_multiple_of(GRANULE), "heap length {len:#x} is not a multiple of {GRANULE}");
        assert!(base.addr().get().checked_add(len).is_some(), "heap span wraps around");
        self.base = Some(base);
        self.len = len;
        if len > 0 {
            self.set(0, Node { size: len, next: NIL });
            self.head = 0;
        }
    }

    /// Add `[end, end + len)` (where `end` is [`Heap::end`]) to the heap as
    /// free memory, merged with a free block that ends at `end`.
    ///
    /// # Safety
    /// As for [`Heap::init`], for the added bytes.
    ///
    /// # Panics
    /// If the heap is not initialized, `len` is not a multiple of
    /// [`GRANULE`], or the span would wrap around.
    pub unsafe fn extend(&mut self, len: usize) {
        let base = self.base.expect("extend before init");
        assert!(len.is_multiple_of(GRANULE), "heap growth {len:#x} is not a multiple of {GRANULE}");
        let new_len = self.len.checked_add(len).expect("heap length overflows");
        assert!(base.addr().get().checked_add(new_len).is_some(), "heap span wraps around");
        if len == 0 {
            return;
        }
        let end = self.len;
        self.len = new_len;
        let (mut last, mut cur) = (NIL, self.head);
        while cur != NIL {
            last = cur;
            cur = self.free_node(cur).next;
        }
        if last != NIL && last + self.free_node(last).size == end {
            let n = self.free_node(last);
            self.set(last, Node { size: n.size + len, ..n });
        } else {
            self.set(end, Node { size: len, next: NIL });
            self.link(last, end);
        }
    }

    /// The start of the heap's span, once initialized.
    pub fn base(&self) -> Option<NonNull<u8>> {
        self.base
    }

    /// The address just past the heap's span (where [`Heap::extend`] adds),
    /// once initialized.
    pub fn end(&self) -> Option<usize> {
        self.base.map(|b| b.addr().get() + self.len)
    }

    pub fn stats(&self) -> Stats {
        Stats { size: self.len, used: self.used, live: self.live }
    }

    /// Allocate a block for `layout`, or `None` if no free block fits
    /// (the heap is then unchanged).
    pub fn alloc(&mut self, layout: Layout) -> Option<NonNull<u8>> {
        let base = self.base?;
        let size = block_size(layout)?;
        let align = block_align(layout);
        let base_addr = base.addr().get();
        let (mut prev, mut cur) = (NIL, self.head);
        while cur != NIL {
            let n = self.free_node(cur);
            // The alignment is of the address, not the offset.
            let aligned = (base_addr + cur).checked_next_multiple_of(align)? - base_addr;
            let end = aligned.checked_add(size)?;
            let block_end = cur + n.size;
            if end <= block_end {
                let tail = block_end - end;
                let after = if tail > 0 {
                    self.set(end, Node { size: tail, next: n.next });
                    end
                } else {
                    n.next
                };
                if aligned > cur {
                    // The padding in front stays free (a multiple of GRANULE).
                    self.set(cur, Node { size: aligned - cur, next: after });
                } else {
                    self.link(prev, after);
                }
                self.used += size;
                self.live += 1;
                // SAFETY: `aligned + size <= cur + n.size <= len`
                // (`free_node` checked the block lies inside the span, which
                // doesn't wrap around), so the pointer is inside the span.
                return Some(unsafe { base.add(aligned) });
            }
            (prev, cur) = (cur, n.next);
        }
        None
    }

    /// Return the block at `ptr`, allocated for `layout`, to the free list,
    /// merged with free neighbours. A block that is outside the heap,
    /// misaligned or overlaps free memory is refused and nothing changes.
    ///
    /// # Safety
    /// `ptr` came from [`Heap::alloc`] on this heap with this `layout` and
    /// hasn't been freed, and nothing uses it any more. The checks catch a
    /// double free while the block is still free and a layout that reaches
    /// into free memory, but not a double free after the memory was reused,
    /// a layout that reaches into another allocation, or a smaller layout.
    pub unsafe fn dealloc(&mut self, ptr: NonNull<u8>, layout: Layout) -> Result<(), FreeError> {
        let base = self.base.ok_or(FreeError::OutsideHeap)?;
        let size = block_size(layout).ok_or(FreeError::NotAllocated)?;
        let off = ptr.addr().get().checked_sub(base.addr().get()).ok_or(FreeError::OutsideHeap)?;
        if off.checked_add(size).is_none_or(|end| end > self.len) {
            return Err(FreeError::OutsideHeap);
        }
        if !off.is_multiple_of(GRANULE) || !ptr.addr().get().is_multiple_of(layout.align()) {
            return Err(FreeError::Misaligned);
        }
        let (mut prev, mut cur) = (NIL, self.head);
        while cur != NIL && cur < off {
            (prev, cur) = (cur, self.free_node(cur).next);
        }
        // Overlapping the free block below (it may have absorbed this one
        // when its neighbour was freed) or the one above.
        let before = (prev != NIL).then(|| self.free_node(prev));
        if before.is_some_and(|p| prev + p.size > off) || (cur != NIL && off + size > cur) {
            return Err(FreeError::NotAllocated);
        }
        // Unreachable while the invariants hold (a block that overlaps no
        // free memory lies in used bytes); a guard against bad counters.
        let (Some(used), Some(live)) = (self.used.checked_sub(size), self.live.checked_sub(1)) else {
            return Err(FreeError::NotAllocated);
        };
        let (size_merged, next) = if cur != NIL && off + size == cur {
            let n = self.free_node(cur);
            (size + n.size, n.next)
        } else {
            (size, cur)
        };
        match before {
            Some(p) if prev + p.size == off => self.set(prev, Node { size: p.size + size_merged, next }),
            _ => {
                self.set(off, Node { size: size_merged, next });
                self.link(prev, off);
            }
        }
        self.used = used;
        self.live = live;
        Ok(())
    }

    /// Walk the free list and check its invariants: every block inside the
    /// heap, aligned, in address order, not touching the previous one, the
    /// free bytes equal to size minus used, and nothing used once nothing is
    /// allocated. Never reads outside the heap's span, and always ends (each
    /// block must start above the last), even if the list is corrupt.
    pub fn check(&self) -> Result<FreeList, CheckError> {
        self.check_bounded(usize::MAX)
    }

    /// [`Heap::check`], giving up after `max_blocks` blocks (for the crash
    /// reporter, which must finish quickly).
    pub fn check_bounded(&self, max_blocks: usize) -> Result<FreeList, CheckError> {
        let mut list = FreeList::default();
        let mut prev_end = None;
        let mut cur = self.head;
        while cur != NIL {
            if list.blocks == max_blocks {
                return Err(CheckError::TooManyBlocks);
            }
            if !cur.is_multiple_of(GRANULE) || cur >= self.len {
                return Err(CheckError::BadLink { offset: cur });
            }
            let n = self.node(cur);
            if n.size == 0 || !n.size.is_multiple_of(GRANULE) || n.size > self.len - cur {
                return Err(CheckError::BadSize { offset: cur, size: n.size });
            }
            match prev_end {
                Some(end) if cur < end => return Err(CheckError::Unordered { offset: cur }),
                Some(end) if cur == end => return Err(CheckError::Adjacent { offset: cur }),
                _ => {}
            }
            prev_end = Some(cur + n.size);
            list.blocks += 1;
            list.bytes += n.size;
            list.largest = list.largest.max(n.size);
            cur = n.next;
        }
        let expected = self.len.checked_sub(self.used);
        if expected != Some(list.bytes) {
            return Err(CheckError::Accounting { listed: list.bytes, expected: expected.unwrap_or(0) });
        }
        if self.live == 0 && self.used != 0 {
            return Err(CheckError::Leaked { used: self.used });
        }
        Ok(list)
    }

    /// The free block at `off`, checked as the walks need it: inside the
    /// span, and linking to a block above its end (or nowhere). A corrupt
    /// list is then a panic, never a stray access or an endless walk.
    fn free_node(&self, off: usize) -> Node {
        let n = self.node(off);
        let ends_inside = n.size >= GRANULE && n.size.is_multiple_of(GRANULE) && n.size <= self.len - off;
        assert!(
            ends_inside && (n.next == NIL || n.next > off + n.size),
            "heap free list corrupt at offset {off:#x} (size {:#x}, next {:#x})",
            n.size,
            n.next
        );
        n
    }

    /// The free-list node at `off`.
    fn node(&self, off: usize) -> Node {
        let ptr = self.node_ptr(off);
        // SAFETY: `node_ptr` checked the node lies inside the span (valid
        // memory the heap owns) and is aligned.
        unsafe { ptr.read() }
    }

    fn set(&mut self, off: usize, node: Node) {
        let ptr = self.node_ptr(off);
        // SAFETY: as in `node`; `&mut self` makes the write exclusive.
        unsafe { ptr.write(node) }
    }

    /// The node at `off`, checked to lie inside the span (and aligned).
    fn node_ptr(&self, off: usize) -> *mut Node {
        let base = self.base.expect("heap not initialized");
        assert!(
            off.is_multiple_of(GRANULE) && off < self.len && self.len - off >= GRANULE,
            "heap free-list offset {off:#x} misaligned or outside the span (length {:#x})",
            self.len
        );
        // SAFETY: `off < len`, so the result stays inside the span.
        unsafe { base.as_ptr().add(off).cast() }
    }

    /// Make the block after `prev` (or the head, if `prev` is [`NIL`]) `next`.
    fn link(&mut self, prev: usize, next: usize) {
        if prev == NIL {
            self.head = next;
        } else {
            let n = self.free_node(prev);
            self.set(prev, Node { next, ..n });
        }
    }
}

#[cfg(test)]
mod tests;
