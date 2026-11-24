//! Thread-local allocation and free counts exclude the test harness and
//! other test threads. Only construction may touch the heap; subsequent
//! operations use a fixed-capacity output sink. Every workload is bounded.

use linux_raw_sys::general::*;
use lumen_tty::{LineDiscipline, Output, Read, Termios, TimerState};
use std::alloc::{GlobalAlloc, Layout, System};
use std::cell::Cell;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Counts {
    allocations: usize,
    frees: usize,
    reallocations: usize,
}

thread_local! {
    static COUNTS: Cell<Option<Counts>> = const { Cell::new(None) };
}

struct Counting;

fn count(update: impl FnOnce(&mut Counts)) {
    // No allocation, including TLS initialization. Ignore accesses during
    // thread teardown, when the harness may still release its own storage.
    let _ = COUNTS.try_with(|cell| {
        if let Some(mut counts) = cell.get() {
            update(&mut counts);
            cell.set(Some(counts));
        }
    });
}

// SAFETY: all operations forward their arguments unchanged to System;
// the counters only observe calls on the current thread.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        count(|n| n.allocations = n.allocations.saturating_add(1));
        // SAFETY: the caller supplies the layout required by GlobalAlloc.
        unsafe { System.alloc(layout) }
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        count(|n| n.allocations = n.allocations.saturating_add(1));
        // SAFETY: the caller supplies the layout required by GlobalAlloc.
        unsafe { System.alloc_zeroed(layout) }
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        count(|n| n.frees = n.frees.saturating_add(1));
        // SAFETY: the pointer and layout satisfy the caller's deallocation contract.
        unsafe { System.dealloc(ptr, layout) }
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        count(|n| n.reallocations = n.reallocations.saturating_add(1));
        // SAFETY: the caller supplies a valid allocation, layout and new size.
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static GLOBAL: Counting = Counting;

struct StopCounting;
impl Drop for StopCounting {
    fn drop(&mut self) {
        COUNTS.with(|cell| cell.set(None));
    }
}

fn measured<T>(f: impl FnOnce() -> T) -> (T, Counts) {
    COUNTS.with(|cell| {
        assert!(cell.get().is_none(), "measurements must not nest");
        cell.set(Some(Counts::default()));
    });
    let _stop = StopCounting; // Also disables counting if the workload panics.
    let result = f();
    let counts = COUNTS.with(|cell| cell.replace(None).unwrap());
    (result, counts)
}

fn observe_readiness(d: &LineDiscipline) {
    for timer in [TimerState::Idle, TimerState::Running, TimerState::Expired] {
        for dest_len in [0, 1, 4096] {
            let _ = std::hint::black_box(d.read_decision(dest_len, timer));
        }
    }
    std::hint::black_box(d.poll_readable());
    for byte in [b'x', b'\n', b'\r', 0, 3, 4, 127] {
        std::hint::black_box(d.accepts(byte));
    }
}

#[test]
fn constructor_allocates_and_drop_frees_positive_control() {
    let (d, counts) = measured(|| std::hint::black_box(LineDiscipline::try_new(Termios::linux_default()).unwrap()));
    assert!(counts.allocations > 0, "the allocation counter must observe try_new");
    assert_eq!(counts.frees, 0);
    assert_eq!(counts.reallocations, 0);
    assert_eq!(d.input_room(), 4095);
    let (_, dropped) = measured(|| drop(d));
    assert_eq!(dropped.allocations, 0);
    assert_eq!(dropped.reallocations, 0);
    assert_eq!(dropped.frees, counts.allocations, "the free counter must observe destruction");
}

struct FixedOutput {
    bytes: [u8; 8192],
    len: usize,
}
impl Output for FixedOutput {
    fn room(&self) -> usize {
        self.bytes.len() - self.len
    }
    fn push(&mut self, bytes: &[u8]) {
        self.bytes[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
    }
    fn flush(&mut self) {
        self.len = 0;
    }
}

#[test]
fn input_editing_reads_readiness_and_mode_changes_never_allocate_or_free() {
    let mut t = Termios::linux_default();
    t.c_lflag |= IEXTEN | ECHOCTL | ECHOKE;
    t.c_cc[VWERASE as usize] = 23;
    t.c_cc[VLNEXT as usize] = 22;
    t.c_cc[VREPRINT as usize] = 18;
    let mut d = LineDiscipline::try_new(t).unwrap();
    let mut out = FixedOutput { bytes: [0; 8192], len: 0 };
    let mut buf = [0; 4096];
    let (_, counts) = measured(|| {
        for _ in 0..20 {
            observe_readiness(&d);
            for &byte in b"a\t\xe2\x82\xac\x7f word\x17\x12\x16\x03\nabc\x04\x04" {
                std::hint::black_box(d.accepts(byte));
                let _ = d.receive(byte, &mut out);
                observe_readiness(&d);
            }
            for _ in 0..4096 {
                if !d.poll_readable() {
                    break;
                }
                let _ = d.read(&mut buf);
            }
            assert!(!d.poll_readable());
            out.flush();
            for _ in 0..5000 {
                let _ = d.receive(b'x', &mut out);
            }
            let _ = d.receive(21, &mut out);
            let _ = d.receive(3, &mut out);
            for _ in 0..4096 {
                let _ = d.receive(4, &mut out);
            }
            for _ in 0..4096 {
                if !d.poll_readable() {
                    break;
                }
                assert_eq!(d.read(&mut buf).0, Read::Eof);
            }
            assert!(!d.poll_readable());
            let mut raw = t;
            raw.c_lflag = 0;
            raw.c_cc[VMIN as usize] = 3;
            raw.c_cc[VTIME as usize] = 2;
            let _ = d.set_termios(raw, false);
            observe_readiness(&d);
            for _ in 0..5000 {
                let _ = d.receive(b'x', &mut out);
            }
            observe_readiness(&d);
            let _ = d.read(&mut buf[..11]);
            for _ in 0..11 {
                let _ = d.receive(b'y', &mut out);
            }
            let _ = d.set_termios(t, false);
            let _ = d.read(&mut buf);
            let _ = d.receive(b'x', &mut out);
            let _ = d.set_termios(t, true);
            observe_readiness(&d);
            let _ = d.receive_break(&mut out);
            let _ = d.set_termios(t, true);
            // Exercise non-flushing signal delivery as well as the flushing path.
            let mut noflush = t;
            noflush.c_lflag |= NOFLSH;
            let _ = d.set_termios(noflush, false);
            let _ = d.receive(3, &mut out);
            let _ = d.set_termios(t, false);
        }
    });
    assert_eq!(counts, Counts::default(), "operations after try_new must not touch the heap");
    assert_eq!(d.input_room(), 4095);
}

#[test]
fn write_into_fixed_sink_never_allocates_or_frees() {
    let mut d = LineDiscipline::try_new(Termios::linux_default()).unwrap();
    let mut out = FixedOutput { bytes: [0; 8192], len: 0 };
    let (consumed, counts) = measured(|| {
        let consumed = d.write(&[b'\n'; 4096], &mut out).consumed;
        assert_eq!(d.write(b"x", &mut out).consumed, 0);
        consumed
    });
    assert_eq!(counts, Counts::default(), "write must not touch the heap");
    assert_eq!(consumed, 4096);
    assert_eq!(out.len, 8192);
    assert!(out.bytes.as_chunks::<2>().0.iter().all(|pair| pair == b"\r\n"));
    let (_, counts) = measured(|| {
        out.flush();
        assert_eq!(d.write(b"hello\t\r\n\x08", &mut out).consumed, 9);
        let mut raw = Termios::linux_default();
        raw.c_oflag = 0;
        let _ = d.set_termios(raw, false);
        assert_eq!(d.write(b"\r\n", &mut out).consumed, 2);
    });
    assert_eq!(counts, Counts::default());
}
