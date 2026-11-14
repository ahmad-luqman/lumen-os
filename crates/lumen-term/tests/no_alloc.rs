//! Writing to a terminal never allocates (the kernel logs while holding the
//! frame allocator's lock): a counting allocator sees no allocation after
//! `Terminal::new`, whatever is written.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

struct Counting;

static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);

// SAFETY: forwards to the system allocator, only counting calls.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as ours.
        unsafe { System.alloc(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: same contract as ours.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        // SAFETY: same contract as ours.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

#[test]
fn writing_never_allocates() {
    // Built before counting: the input, then the terminal.
    let mut input = Vec::new();
    for i in 0..500 {
        input.extend(
            format!(
                "\x1b[3{c}mline {i} \u{4e00}\u{301} \x1b[38;5;{i}m\x1b[48;2;1;2;3m\t\x1b[1;7mx\x1b[0m\
                 \x1b]0;title {i}\x07\x1bP$q\x1b\\\x1b[2;5r\x1b[3;1H\n\x1bM\x1b[r\x1b[L\x1b[M\x1b[@\x1b[P\
                 \x1b[X\x1b[K\x1b[1J\x1b[S\x1b[T\x1b7\x1b8\x1b[s\x1b[u\x1b[?25l\x1b[?25h\r\n",
                c = i % 8
            )
            .bytes(),
        );
        input.push(0xff); // invalid UTF-8
    }
    let mut t = lumen_term::Terminal::new(80, 25, 200);
    let before = ALLOCATIONS.load(Ordering::Relaxed);
    for chunk in input.chunks(7) {
        t.write(chunk);
    }
    t.scroll_view(50);
    t.write(b"more\r\n");
    t.reset_view();
    t.write(b"\x1b[2J\x1b[3J\x1bc");
    let after = ALLOCATIONS.load(Ordering::Relaxed);
    assert_eq!(after - before, 0, "writing allocated {} times", after - before);
    assert_eq!(t.scrollback_len(), 0);
}
