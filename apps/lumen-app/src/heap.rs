//! The app's global allocator: the kernel heap's own allocator logic
//! (`lumen-heap`: an address-ordered free list with coalescing, host-tested)
//! over `[__heap_base, end of memory)`. An app's memory is fixed
//! (ARCHITECTURE section 8, "Its memory is fixed"): the allocator never
//! grows it, and returns null (an allocation error) when it is used up.

use core::alloc::{GlobalAlloc, Layout};
use core::cell::UnsafeCell;
use core::ptr::NonNull;
use lumen_heap::{GRANULE, Heap};

unsafe extern "C" {
    /// Where the linker put the end of the stack and data: the heap's
    /// start.
    static __heap_base: u8;
}

pub(crate) struct AppHeap(UnsafeCell<Option<Heap>>);

// SAFETY: a wasm app has one thread.
unsafe impl Sync for AppHeap {}

impl AppHeap {
    pub(crate) const fn new() -> AppHeap {
        AppHeap(UnsafeCell::new(None))
    }

    /// `f` on the heap, set up over the rest of the memory at the first
    /// use.
    ///
    /// # Safety
    ///
    /// Not reentrant: `f` doesn't allocate (one thread, so nothing else
    /// holds the heap meanwhile).
    unsafe fn with<R>(&self, f: impl FnOnce(&mut Heap) -> R) -> R {
        // SAFETY: the caller's contract: no other reference to the heap is
        // live while `f` runs.
        let slot = unsafe { &mut *self.0.get() };
        let heap = slot.get_or_insert_with(|| {
            let start = (&raw const __heap_base as usize).next_multiple_of(GRANULE);
            let end = core::arch::wasm32::memory_size(0) * 65536;
            let len = end.saturating_sub(start) / GRANULE * GRANULE;
            let mut heap = Heap::empty();
            if let Some(base) = NonNull::new(start as *mut u8)
                && len > 0
            {
                // SAFETY: `[start, start + len)` is the memory past the
                // linker's stack and data, which nothing else uses, and it
                // never shrinks (the memory is fixed).
                unsafe { heap.init(base, len) };
            }
            heap
        });
        f(heap)
    }
}

// SAFETY: `lumen_heap::Heap` hands out disjoint blocks aligned as asked,
// inside its span, and takes back only blocks it handed out with their
// layout (`dealloc` refuses anything else, changing nothing).
unsafe impl GlobalAlloc for AppHeap {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: `Heap::alloc` doesn't allocate.
        unsafe { self.with(|heap| heap.alloc(layout).map_or(core::ptr::null_mut(), NonNull::as_ptr)) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        let Some(p) = NonNull::new(ptr) else { return };
        // SAFETY: `Heap::dealloc` doesn't allocate; `ptr` came from `alloc`
        // with `layout` (GlobalAlloc's contract), and a refusal changes
        // nothing.
        unsafe {
            self.with(|heap| {
                let _ = heap.dealloc(p, layout);
            })
        }
    }
}
