extern crate std;

use super::*;
use std::vec;
use std::vec::Vec;

/// Host memory for a heap, 64 KiB-aligned so alignments up to that are
/// deterministic relative to the start.
struct Arena {
    ptr: NonNull<u8>,
    layout: Layout,
}

impl Arena {
    fn new(len: usize) -> Arena {
        let layout = Layout::from_size_align(len, 1 << 16).unwrap();
        // SAFETY: `layout` has a non-zero size.
        let ptr = NonNull::new(unsafe { std::alloc::alloc(layout) }).expect("host allocation");
        Arena { ptr, layout }
    }

    /// A heap over `[skew, skew + len)` of the arena.
    fn heap(&self, skew: usize, len: usize) -> Heap {
        assert!(skew + len <= self.layout.size());
        let mut heap = Heap::empty();
        // SAFETY: the range is inside the arena, which outlives the heap in
        // every test, and only the heap uses it.
        unsafe { heap.init(self.ptr.add(skew), len) };
        heap
    }

    fn offset(&self, p: NonNull<u8>) -> usize {
        p.addr().get() - self.ptr.addr().get()
    }
}

impl Drop for Arena {
    fn drop(&mut self) {
        // SAFETY: allocated in `new` with this layout.
        unsafe { std::alloc::dealloc(self.ptr.as_ptr(), self.layout) }
    }
}

fn layout(size: usize, align: usize) -> Layout {
    Layout::from_size_align(size, align).unwrap()
}

/// Free a block the test allocated, asserting the heap accepts it.
fn free(heap: &mut Heap, p: NonNull<u8>, l: Layout) {
    // SAFETY: every caller passes a block it got from `heap.alloc(l)`.
    unsafe { heap.dealloc(p, l) }.unwrap();
}

#[test]
fn block_size_rounds_up_to_the_granule() {
    assert_eq!(block_size(layout(0, 1)), Some(16));
    assert_eq!(block_size(layout(1, 1)), Some(16));
    assert_eq!(block_size(layout(16, 8)), Some(16));
    assert_eq!(block_size(layout(17, 1)), Some(32));
    assert_eq!(growth_for(layout(17, 1)), Some(32));
    assert_eq!(growth_for(layout(16, 4096)), Some(16 + 4096 - 16));
}

#[test]
fn reuse_after_free() {
    let arena = Arena::new(4096);
    let mut heap = arena.heap(0, 4096);
    let l = layout(100, 8);
    let a = heap.alloc(l).unwrap();
    let b = heap.alloc(l).unwrap();
    free(&mut heap, a, l);
    // First fit: the hole `a` left is the first block that fits.
    assert_eq!(heap.alloc(l), Some(a));
    free(&mut heap, a, l);
    free(&mut heap, b, l);
    assert_eq!(heap.stats(), Stats { size: 4096, used: 0, live: 0 });
    // The same memory serves an unbounded number of alloc/free cycles.
    for _ in 0..10_000 {
        let p = heap.alloc(layout(4096, 16)).expect("the whole heap is free again");
        free(&mut heap, p, layout(4096, 16));
    }
}

#[test]
fn coalescing_with_each_neighbour() {
    let arena = Arena::new(4096);
    let mut heap = arena.heap(0, 4096);
    let l = layout(1024, 16);
    let blocks: Vec<_> = (0..4).map(|_| heap.alloc(l).unwrap()).collect();
    assert_eq!(heap.alloc(layout(16, 16)), None, "full");
    // Free 1 and 3: two separate holes.
    free(&mut heap, blocks[1], l);
    free(&mut heap, blocks[3], l);
    assert_eq!(heap.check().unwrap(), FreeList { blocks: 2, bytes: 2048, largest: 1024 });
    // Freeing 2 merges with both neighbours: one 3 KiB hole.
    free(&mut heap, blocks[2], l);
    assert_eq!(heap.check().unwrap(), FreeList { blocks: 1, bytes: 3072, largest: 3072 });
    // Freeing 0 merges with the next block only.
    free(&mut heap, blocks[0], l);
    assert_eq!(heap.check().unwrap(), FreeList { blocks: 1, bytes: 4096, largest: 4096 });
    assert_eq!(heap.alloc(layout(4096, 16)), Some(blocks[0]));

    // Merging with the previous block only.
    let mut heap = arena.heap(0, 2048);
    let a = heap.alloc(l).unwrap();
    let b = heap.alloc(l).unwrap();
    free(&mut heap, a, l);
    free(&mut heap, b, l);
    assert_eq!(heap.check().unwrap(), FreeList { blocks: 1, bytes: 2048, largest: 2048 });
}

#[test]
fn alignment_for_every_power_of_two_up_to_a_page() {
    // Skewed starts make the aligned address land past the block start, so
    // the front padding path runs too.
    for skew in [0, 16, 48, 4096 - 16] {
        let arena = Arena::new(1 << 20);
        let mut heap = arena.heap(skew, (1 << 20) - 4096);
        let mut live = Vec::new();
        for shift in 0..=12 {
            let align = 1usize << shift;
            for size in [1, align, 3 * align + 1] {
                let l = layout(size, align);
                let p = heap.alloc(l).unwrap_or_else(|| panic!("align {align} size {size}"));
                assert!(p.addr().get().is_multiple_of(align), "{p:p} not aligned to {align}");
                // SAFETY: the block is ours and `size` bytes long.
                unsafe { p.as_ptr().write_bytes(shift as u8, size) };
                live.push((p, l, shift as u8));
            }
            heap.check().unwrap();
        }
        for (p, l, tag) in live {
            // SAFETY: as above; nothing else wrote to the block.
            let bytes = unsafe { core::slice::from_raw_parts(p.as_ptr(), l.size()) };
            assert!(bytes.iter().all(|&b| b == tag), "block {p:p} was overwritten");
            free(&mut heap, p, l);
        }
        let len = heap.stats().size;
        assert_eq!(heap.check().unwrap(), FreeList { blocks: 1, bytes: len, largest: len });
    }
}

#[test]
fn alignment_beyond_a_page() {
    let arena = Arena::new(1 << 20);
    let mut heap = arena.heap(16, 512 << 10);
    let l = layout(100, 1 << 16);
    let p = heap.alloc(l).unwrap();
    assert!(p.addr().get().is_multiple_of(1 << 16));
    heap.check().unwrap();
    free(&mut heap, p, l);
    assert_eq!(heap.check().unwrap().blocks, 1);
}

#[test]
fn refused_frees_change_nothing() {
    let arena = Arena::new(8192);
    let mut heap = arena.heap(0, 4096);
    let l = layout(64, 16);
    let a = heap.alloc(l).unwrap();
    let b = heap.alloc(l).unwrap();
    let c = heap.alloc(l).unwrap();
    free(&mut heap, b, l);
    let (stats, list) = (heap.stats(), heap.check().unwrap());
    let refused = |heap: &mut Heap, p: NonNull<u8>, l: Layout| {
        // SAFETY: each call is one the heap must refuse without touching
        // memory outside its free list.
        unsafe { heap.dealloc(p, l) }.unwrap_err()
    };
    // Double free.
    assert_eq!(refused(&mut heap, b, l), FreeError::NotAllocated);
    // A layout that reaches into the free block after `a`.
    assert_eq!(refused(&mut heap, a, layout(80, 16)), FreeError::NotAllocated);
    // Outside the span: past the end (inside the arena), and below the base.
    // SAFETY: inside the arena.
    let past = unsafe { arena.ptr.add(4096) };
    assert_eq!(refused(&mut heap, past, l), FreeError::OutsideHeap);
    // SAFETY: inside the arena.
    let near_end = unsafe { arena.ptr.add(4096 - 32) };
    assert_eq!(refused(&mut heap, near_end, l), FreeError::OutsideHeap);
    // A second heap in the arena's upper half, and a pointer just below it.
    let mut upper = arena.heap(4096 + 64, 1024);
    // SAFETY: inside the arena.
    let below_base = unsafe { arena.ptr.add(4096) };
    assert_eq!(refused(&mut upper, below_base, l), FreeError::OutsideHeap);
    // Misaligned: not on a granule (though at the layout's 8-byte
    // alignment, inside `c`, overlapping nothing free), or not at the
    // layout's alignment.
    // SAFETY: inside `c`'s block.
    let inside_c = unsafe { c.add(8) };
    assert_eq!(refused(&mut heap, inside_c, layout(32, 8)), FreeError::Misaligned);
    assert_eq!(refused(&mut heap, c, layout(64, 4096)), FreeError::Misaligned);
    assert_eq!(heap.stats(), stats);
    assert_eq!(heap.check().unwrap(), list);
    free(&mut heap, a, l);
    free(&mut heap, c, l);
    assert_eq!(heap.check().unwrap().blocks, 1);
}

#[test]
fn double_free_after_merging_with_the_block_below_is_refused() {
    // Freeing `b` after `a` merges it into `a`'s free block, so no free
    // block starts at `b`: only the overlap with the block below catches
    // the second free.
    let arena = Arena::new(4096);
    let mut heap = arena.heap(0, 4096);
    let l = layout(64, 16);
    let (a, b, _c) = (heap.alloc(l).unwrap(), heap.alloc(l).unwrap(), heap.alloc(l).unwrap());
    free(&mut heap, a, l);
    free(&mut heap, b, l);
    let (stats, list) = (heap.stats(), heap.check().unwrap());
    // SAFETY: a double free, which the heap must refuse without changes.
    assert_eq!(unsafe { heap.dealloc(b, l) }, Err(FreeError::NotAllocated));
    assert_eq!((heap.stats(), heap.check().unwrap()), (stats, list));
}

#[test]
fn freeing_with_a_smaller_layout_is_reported_as_a_leak() {
    let arena = Arena::new(4096);
    let mut heap = arena.heap(0, 4096);
    let a = heap.alloc(layout(64, 16)).unwrap();
    // A smaller layout overlaps nothing free, so it can't be refused; the
    // 32 bytes it leaves behind show once nothing is allocated.
    free(&mut heap, a, layout(32, 16));
    assert_eq!(heap.check(), Err(CheckError::Leaked { used: 32 }));
}

#[test]
#[should_panic(expected = "heap free list corrupt")]
fn a_corrupt_list_is_a_panic_not_an_endless_walk() {
    let arena = Arena::new(4096);
    let mut heap = arena.heap(0, 4096);
    let a = heap.alloc(layout(64, 16)).unwrap();
    let _b = heap.alloc(layout(64, 16)).unwrap();
    free(&mut heap, a, layout(64, 16));
    // Link the free block at `a` back to itself (a use-after-free write).
    // SAFETY: the second word of the free block at `a` is its `next`.
    unsafe { a.as_ptr().cast::<usize>().add(1).write(0) };
    // Too big for the block at `a`: a walk without the link check would
    // follow the self-link forever.
    heap.alloc(layout(128, 16));
}

#[test]
#[should_panic(expected = "heap free list corrupt")]
fn a_free_block_running_past_the_heap_is_a_panic() {
    let arena = Arena::new(8192);
    let mut heap = arena.heap(0, 4096);
    let l = layout(64, 16);
    let (_a, b) = (heap.alloc(l).unwrap(), heap.alloc(l).unwrap());
    let _rest = heap.alloc(layout(4096 - 128, 16)).unwrap();
    free(&mut heap, b, l);
    // `b` (offset 64) is now the only free block, linking nowhere. Grow it to
    // end 16 bytes past the heap: still smaller than the heap and with no
    // link to contradict it, so only the end-inside check catches it
    // (unchecked, `alloc` would hand out memory overlapping `_rest` and
    // running past the end).
    // SAFETY: the first word of the free block at `b` is its size.
    unsafe { b.as_ptr().cast::<usize>().write(4096 - 64 + 16) };
    heap.alloc(layout(128, 16));
}

#[test]
fn a_free_below_a_live_tail_does_not_absorb_it_when_extending() {
    let arena = Arena::new(4096);
    let mut heap = arena.heap(0, 256);
    let (a, b, c) = (layout(64, 16), layout(176, 16), layout(16, 16));
    let (pa, pb, pc) = (heap.alloc(a).unwrap(), heap.alloc(b).unwrap(), heap.alloc(c).unwrap());
    free(&mut heap, pb, b);
    // SAFETY: inside the arena.
    unsafe { heap.extend(256) };
    // The free block at 64 ends 16 bytes below the end (the live tail):
    // the new memory is a separate block.
    assert_eq!(heap.check().unwrap(), FreeList { blocks: 2, bytes: 176 + 256, largest: 256 });
    free(&mut heap, pa, a);
    free(&mut heap, pc, c);
    assert_eq!(heap.check().unwrap(), FreeList { blocks: 1, bytes: 512, largest: 512 });
}

#[test]
fn a_free_after_a_smaller_layout_free_is_refused_as_accounting() {
    let arena = Arena::new(4096);
    let mut heap = arena.heap(0, 4096);
    let p = heap.alloc(layout(128, 16)).unwrap();
    // The first half freed with a too-small layout: accepted (overlaps
    // nothing free), leaving 64 bytes counted as used with nothing live.
    free(&mut heap, p, layout(64, 16));
    let (stats, list) = (heap.stats(), heap.check());
    // The second half overlaps nothing free either, but nothing is live.
    // SAFETY: a wrong free, which the heap must refuse without changes.
    let second = unsafe { p.add(64) };
    // SAFETY: as above.
    assert_eq!(unsafe { heap.dealloc(second, layout(64, 16)) }, Err(FreeError::Accounting));
    assert_eq!((heap.stats(), heap.check()), (stats, list));
    assert_eq!(list, Err(CheckError::Leaked { used: 64 }));
}

#[test]
fn check_reports_more_used_than_the_heap_has() {
    let arena = Arena::new(4096);
    let mut heap = arena.heap(0, 4096);
    heap.used = 8192; // corrupt counters (the tests can reach the fields)
    assert_eq!(heap.check(), Err(CheckError::UsedExceedsSize { used: 8192, size: 4096 }));
    assert_eq!(heap.check_bounded(0), Err(CheckError::UsedExceedsSize { used: 8192, size: 4096 }));
    assert_eq!(heap.stats().free(), 0);
}

#[test]
fn exhaustion_returns_none_and_changes_nothing() {
    let arena = Arena::new(4096);
    let mut empty = Heap::empty();
    assert_eq!(empty.alloc(layout(16, 16)), None);
    assert_eq!(empty.check().unwrap(), FreeList::default());
    let mut heap = arena.heap(0, 1024);
    let a = heap.alloc(layout(512, 16)).unwrap();
    let (stats, list) = (heap.stats(), heap.check().unwrap());
    assert_eq!(heap.alloc(layout(513, 16)), None);
    assert_eq!(heap.alloc(layout(16, 2048)), None, "no 2 KiB-aligned address left");
    assert_eq!(heap.alloc(layout(isize::MAX as usize - 15, 16)), None);
    assert_eq!((heap.stats(), heap.check().unwrap()), (stats, list));
    free(&mut heap, a, layout(512, 16));
}

#[test]
fn extend_merges_with_a_free_tail_or_adds_a_block() {
    let arena = Arena::new(1 << 16);
    let mut heap = arena.heap(0, 0);
    assert_eq!(heap.alloc(layout(16, 16)), None);
    // SAFETY: inside the arena.
    unsafe { heap.extend(1024) };
    assert_eq!(heap.check().unwrap(), FreeList { blocks: 1, bytes: 1024, largest: 1024 });
    // Free tail: merged.
    // SAFETY: inside the arena.
    unsafe { heap.extend(1024) };
    assert_eq!(heap.check().unwrap(), FreeList { blocks: 1, bytes: 2048, largest: 2048 });
    // Allocated tail: a new block.
    let all = heap.alloc(layout(2048, 16)).unwrap();
    // SAFETY: inside the arena.
    unsafe { heap.extend(512) };
    assert_eq!(heap.check().unwrap(), FreeList { blocks: 1, bytes: 512, largest: 512 });
    assert_eq!(heap.end(), Some(arena.ptr.addr().get() + 2560));
    free(&mut heap, all, layout(2048, 16));
    assert_eq!(heap.check().unwrap(), FreeList { blocks: 1, bytes: 2560, largest: 2560 });
}

#[test]
fn growth_for_is_always_enough() {
    let mut rng = Rng(0x2545_f491_4f6c_dd1d);
    for _ in 0..500 {
        let arena = Arena::new(1 << 20);
        let mut heap = arena.heap(16 * rng.below(8) as usize, 4096);
        // Fill the heap with a random mix, so the tail is allocated or free.
        while let Some(_) = heap.alloc(layout(16 + rng.below(512) as usize, 16)) {}
        let l = layout(1 + rng.below(8192) as usize, 1 << rng.below(15));
        let grow = growth_for(l).unwrap();
        if arena.offset(heap.base().unwrap()) + heap.stats().size + grow > 1 << 20 {
            continue;
        }
        // SAFETY: inside the arena (checked above).
        unsafe { heap.extend(grow) };
        let p = heap.alloc(l).unwrap_or_else(|| panic!("{l:?} did not fit after growing {grow}"));
        assert!(p.addr().get().is_multiple_of(l.align()));
        heap.check().unwrap();
    }
}

#[test]
fn check_finds_corruption_without_reading_outside_the_heap() {
    let arena = Arena::new(8192);
    let mut heap = arena.heap(0, 4096);
    let a = heap.alloc(layout(64, 16)).unwrap();
    let _b = heap.alloc(layout(64, 16)).unwrap();
    free(&mut heap, a, layout(64, 16));
    // The free block at `a` (offset 0) now precedes the tail block at 128.
    let node = a.as_ptr().cast::<usize>();
    let corrupt = |size: usize, next: usize| {
        // SAFETY: the first 16 bytes of the free block at `a` are its node.
        unsafe { node.write(size) };
        // SAFETY: as above.
        unsafe { node.add(1).write(next) };
        heap.check()
    };
    assert_eq!(corrupt(64, 1 << 40), Err(CheckError::BadLink { offset: 1 << 40 }));
    assert_eq!(corrupt(64, 8), Err(CheckError::BadLink { offset: 8 }));
    assert_eq!(corrupt(8, 128), Err(CheckError::BadSize { offset: 0, size: 8 }));
    assert_eq!(corrupt(8192, 128), Err(CheckError::BadSize { offset: 0, size: 8192 }));
    assert_eq!(corrupt(256, 128), Err(CheckError::Unordered { offset: 128 }));
    assert_eq!(corrupt(128, 128), Err(CheckError::Adjacent { offset: 128 }));
    assert!(matches!(corrupt(32, 128), Err(CheckError::Accounting { .. })));
    // A loop back to itself is caught as out of order, so walks terminate.
    assert_eq!(corrupt(64, 0), Err(CheckError::Unordered { offset: 0 }));
    assert_eq!(corrupt(64, 128).map(|l| l.blocks), Ok(2));
    // The tail block at 128 running past the end, by less than the heap's
    // length (so the bound must be relative to the block's offset).
    let tail = node.wrapping_add(128 / size_of::<usize>());
    // SAFETY: the first word of the free tail block (offset 128) is its size.
    unsafe { tail.write(4096 - 128 + 16) };
    assert_eq!(heap.check(), Err(CheckError::BadSize { offset: 128, size: 4096 - 128 + 16 }));
    // SAFETY: as above; restore it.
    unsafe { tail.write(4096 - 128) };
    assert_eq!(heap.check_bounded(1), Err(CheckError::TooManyBlocks));
}

/// xorshift64: deterministic randomness without a dependency.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }
}

/// Random allocations and frees (and growth) against a shadow map of which
/// allocation owns each granule: blocks never overlap, stay inside the heap,
/// are aligned, keep their contents, and the counters and the free list's
/// invariants hold throughout. Everything freed coalesces back to one block.
#[test]
fn randomized_stress_with_invariant_checks() {
    const ARENA: usize = 4 << 20;
    for seed in [1u64, 0xdead_beef, 0x1234_5678_9abc_def1] {
        let mut rng = Rng(seed);
        let arena = Arena::new(ARENA);
        let skew = 16 * rng.below(4) as usize;
        let mut heap = arena.heap(skew, 256 << 10);
        let mut owner = vec![0u32; ARENA / GRANULE];
        let mut live: Vec<(NonNull<u8>, Layout, u32)> = Vec::new();
        let mut freed: Vec<(NonNull<u8>, Layout)> = Vec::new();
        let (mut next_tag, mut used, mut refused) = (1u32, 0usize, 0usize);
        for step in 0..40_000 {
            match rng.below(100) {
                0..55 => {
                    let size = match rng.below(10) {
                        0..7 => 1 + rng.below(256),
                        7..9 => 1 + rng.below(4096),
                        _ => 1 + rng.below(64 << 10),
                    } as usize;
                    let l = layout(size, 1 << rng.below(13));
                    let Some(p) = heap.alloc(l) else { continue };
                    let off = arena.offset(p);
                    assert!(p.addr().get().is_multiple_of(l.align()), "step {step}: misaligned");
                    let base = arena.offset(heap.base().unwrap());
                    assert!(off >= base && off + size <= base + heap.stats().size, "step {step}: outside");
                    let block = block_size(l).unwrap();
                    for g in &mut owner[off / GRANULE..(off + block) / GRANULE] {
                        assert_eq!(*g, 0, "step {step}: overlaps a live block");
                        *g = next_tag;
                    }
                    // SAFETY: the block is ours and `size` bytes long.
                    unsafe { p.as_ptr().write_bytes(next_tag as u8, size) };
                    live.push((p, l, next_tag));
                    next_tag += 1;
                    used += block;
                }
                55..97 if !live.is_empty() => {
                    let (p, l, tag) = live.swap_remove(rng.below(live.len() as u64) as usize);
                    // SAFETY: as above; nothing else wrote to the block.
                    let bytes = unsafe { core::slice::from_raw_parts(p.as_ptr(), l.size()) };
                    assert!(bytes.iter().all(|&b| b == tag as u8), "step {step}: block contents changed");
                    let off = arena.offset(p);
                    let block = block_size(l).unwrap();
                    owner[off / GRANULE..(off + block) / GRANULE].fill(0);
                    free(&mut heap, p, l);
                    used -= block;
                    freed.push((p, l));
                    if freed.len() > 64 {
                        freed.remove(0);
                    }
                }
                97..99 if !freed.is_empty() => {
                    // A double free of a block that is still entirely free
                    // must be refused without changes. (Once fully reused, it
                    // can't be detected without headers.)
                    let (p, l) = freed[rng.below(freed.len() as u64) as usize];
                    let off = arena.offset(p);
                    let block = block_size(l).unwrap();
                    if owner[off / GRANULE..(off + block) / GRANULE].iter().all(|&g| g == 0) {
                        let (stats, list) = (heap.stats(), heap.check().unwrap());
                        // SAFETY: a double free, which the heap must refuse.
                        let r = unsafe { heap.dealloc(p, l) };
                        assert_eq!(r, Err(FreeError::NotAllocated), "step {step}: double free accepted");
                        assert_eq!((heap.stats(), heap.check().unwrap()), (stats, list), "step {step}");
                        refused += 1;
                    }
                }
                99.. => {
                    let grow = 16 * (1 + rng.below(1024) as usize);
                    if skew + heap.stats().size + grow <= ARENA {
                        // SAFETY: inside the arena (checked above).
                        unsafe { heap.extend(grow) };
                    }
                }
                _ => {}
            }
            let stats = heap.stats();
            assert_eq!((stats.used, stats.live), (used, live.len()), "step {step}");
            if step % 64 == 0 {
                let list = heap.check().unwrap_or_else(|e| panic!("step {step}: {e}"));
                assert_eq!(list.bytes, stats.free());
            }
        }
        assert!(refused > 100, "only {refused} double frees tried");
        for (p, l, _) in live.drain(..) {
            free(&mut heap, p, l);
        }
        let size = heap.stats().size;
        assert_eq!(heap.check().unwrap(), FreeList { blocks: 1, bytes: size, largest: size }, "seed {seed:#x}");
    }
}
