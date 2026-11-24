//! Allocation counts are local to the test thread, excluding the harness.
use linux_raw_sys::general::*;
use lumen_tty::{LineDiscipline, SetWhen, Termios};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

struct Counting;
thread_local! {
    static ENABLED: Cell<bool> = const { Cell::new(false) };
    static COUNT: Cell<usize> = const { Cell::new(0) };
}
fn count() {
    if ENABLED.with(Cell::get) {
        COUNT.with(|n| n.set(n.get() + 1));
    }
}
// SAFETY: all operations delegate unchanged to the system allocator.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count();
        // SAFETY: the caller provides the layout required by GlobalAlloc.
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        // SAFETY: pointer and layout obey the caller's GlobalAlloc contract.
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count();
        // SAFETY: the caller provides a valid allocation and new size.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}
#[global_allocator]
static GLOBAL: Counting = Counting;

#[test]
fn input_editing_reads_and_mode_changes_never_allocate() {
    let mut t = Termios::linux_default();
    t.c_lflag |= IEXTEN | ECHOCTL | ECHOKE;
    t.c_cc[VWERASE as usize] = 23;
    t.c_cc[VLNEXT as usize] = 22;
    t.c_cc[VREPRINT as usize] = 18;
    let mut d = LineDiscipline::new(t);
    let mut buf = [0; 4096];
    COUNT.with(|n| n.set(0));
    ENABLED.with(|c| c.set(true));
    for _ in 0..20 {
        for &byte in b"a\t\xe2\x82\xac\x7f word\x17\x12\x16\x03\nabc\x04\x04" {
            d.receive(byte);
        }
        while d.readable() {
            d.read(&mut buf);
        }
        for _ in 0..5000 {
            d.receive(b'x');
        }
        d.receive(21);
        d.receive(3);
        for _ in 0..4096 {
            d.receive(4);
        }
        while d.readable() {
            d.read(&mut buf);
        }
        let mut raw = t;
        raw.c_lflag = 0;
        d.set_termios(raw, SetWhen::Now);
        for _ in 0..5000 {
            d.receive(b'x');
        }
        d.read(&mut buf[..11]);
        for _ in 0..11 {
            d.receive(b'y');
        } // force ring wrap
        d.set_termios(t, SetWhen::Drain);
        d.output_drained();
        d.read(&mut buf);
        d.receive(b'x');
        d.set_termios(raw, SetWhen::Flush);
        d.output_drained();
        d.set_termios(t, SetWhen::Now);
    }
    ENABLED.with(|c| c.set(false));
    assert_eq!(COUNT.with(Cell::get), 0);
    assert_eq!(d.input_room(), 4096);
}

#[test]
fn output_allocates_once_per_batch_without_growth() {
    let mut d = LineDiscipline::default();
    let input = [b'\n'; 4096];
    COUNT.with(|n| n.set(0));
    ENABLED.with(|c| c.set(true));
    let output = d.write(&input);
    ENABLED.with(|c| c.set(false));
    assert_eq!(COUNT.with(Cell::get), 1);
    assert_eq!(output.len(), 8192);
    assert!(output.as_chunks::<2>().0.iter().all(|pair| pair == b"\r\n"));
}
