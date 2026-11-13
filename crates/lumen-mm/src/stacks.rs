//! Kernel stacks: where they go in the stack window ([`StackWindow`]), and a
//! registry of them ([`Registry`]) that the crash reporter reads without
//! locks.
//!
//! Each stack gets an unmapped guard page directly below it. Stacks are laid
//! out upward, guard then stack, so the page directly above a stack is the
//! next stack's guard (or never handed out): running off either end faults
//! instead of silently corrupting a neighbour.
//!
//! The registry is what the backtrace walker trusts ([`Registry::contains`]:
//! a frame pointer is only followed into a registered stack) and what names
//! guard-page hits ([`Registry::find`] says an address is in a stack's guard
//! page). Each slot is a small seqlock: a writer fills it in only while its
//! sequence count is odd, so a reader never sees a half-written entry or a
//! mix of two; an entry is retracted before its memory goes away.

use core::ops::Range;
use core::sync::atomic::{AtomicU8, AtomicU64, Ordering, fence};

use crate::FRAME_SIZE;
use crate::window::Window;

/// The unmapped page below every stack.
pub const GUARD_SIZE: u64 = FRAME_SIZE;

/// How many stacks can be registered at once: four per CPU (main, #DF, NMI,
/// #MC) for up to eight CPUs, and the rest for kernel threads (one each) and
/// tests.
pub const MAX_STACKS: usize = 128;

/// What a stack is for (names it in crash reports). Stored in the registry as
/// one byte, so a reader gets it in one atomic load.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StackKind {
    /// A CPU's main kernel stack (the boot CPU's from `kmain_on_kernel_stack`,
    /// an AP's from `smp::ap_main`).
    Main = 1,
    /// IST stack for double faults.
    DoubleFault = 2,
    /// IST stack for NMIs.
    Nmi = 3,
    /// IST stack for machine checks.
    MachineCheck = 4,
    /// Allocated by a test.
    Test = 5,
    /// A kernel thread's stack (`sched::spawn`).
    Thread = 6,
}

impl StackKind {
    const ALL: [StackKind; 6] = [
        StackKind::Main,
        StackKind::DoubleFault,
        StackKind::Nmi,
        StackKind::MachineCheck,
        StackKind::Test,
        StackKind::Thread,
    ];

    fn from_raw(raw: u8) -> Option<StackKind> {
        Self::ALL.into_iter().find(|&k| k as u8 == raw)
    }

    pub fn name(self) -> &'static str {
        match self {
            StackKind::Main => "kernel main",
            StackKind::DoubleFault => "#DF IST",
            StackKind::Nmi => "NMI IST",
            StackKind::MachineCheck => "#MC IST",
            StackKind::Test => "test",
            StackKind::Thread => "thread",
        }
    }
}

/// A stack's addresses: `[bottom, top)`, page-aligned; its guard page is
/// `[bottom - GUARD_SIZE, bottom)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct StackRange {
    pub bottom: u64,
    pub top: u64,
}

impl StackRange {
    /// The start of the guard page below the stack.
    pub fn guard(self) -> u64 {
        self.bottom - GUARD_SIZE
    }

    pub fn size(self) -> u64 {
        self.top - self.bottom
    }

    /// The stack's addresses as a range.
    pub fn span(self) -> Range<u64> {
        self.bottom..self.top
    }

    /// Page-aligned, nonempty, with a guard page below that isn't the null
    /// page (readers compute `guard() - 1`).
    fn is_valid(self) -> bool {
        self.bottom.is_multiple_of(FRAME_SIZE)
            && self.top.is_multiple_of(FRAME_SIZE)
            && GUARD_SIZE < self.bottom
            && self.bottom < self.top
    }
}

/// Hands out stack addresses in the stack window, upward, a guard page below
/// each and the page above the last one left free. It holds the window's
/// [`Window`] token and splits off a sub-window for each stack, so each
/// stack's holder owns its pages. Guard pages get no token: a guard page's
/// range is split off and dropped when its stack is taken (the page above the
/// newest stack stays in this window until the next stack takes it as its
/// guard), so no one can ever map one. Addresses are never
/// reused; the caller sizes the window so that doesn't run out (the kernel's
/// is about 1 TiB).
#[derive(Debug)]
pub struct StackWindow {
    window: Window,
}

impl StackWindow {
    pub fn new(window: Window) -> StackWindow {
        StackWindow { window }
    }

    /// Addresses for the next stack of `size` bytes (a nonzero multiple of
    /// the page size), with its guard page below it, and the sub-window that
    /// owns the stack (not the guard page); `None` once the window is full.
    pub fn take(&mut self, size: u64) -> Option<(StackRange, Window)> {
        assert!(size != 0 && size.is_multiple_of(FRAME_SIZE), "stack size {size:#x} is not whole pages");
        let len = GUARD_SIZE.checked_add(size)?;
        // The page above the stack must be free too (the next guard).
        if len.checked_add(GUARD_SIZE)? > self.window.len() {
            return None;
        }
        // The guard page's range is given up for good: no token, no mapping.
        drop(self.window.split_off(GUARD_SIZE)?);
        let own = self.window.split_off(size)?;
        let r = own.range();
        Some((StackRange { bottom: r.start, top: r.end }, own))
    }
}

/// A registered stack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Registered {
    pub kind: StackKind,
    pub range: StackRange,
}

/// Where an address falls ([`Registry::find`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    /// Inside a registered stack.
    Stack(Registered),
    /// In the guard page below a registered stack: usually that stack
    /// overflowing, but any access here lands in it (a stray pointer, or the
    /// stack below running off its top).
    Guard(Registered),
}

/// Proof of a registration, given back to [`Registry::unregister`] of the
/// same registry. Not `Clone`: an entry is retracted once. Dropping it keeps
/// the slot taken for good.
#[must_use = "dropping the handle leaves the stack registered for good"]
#[derive(Debug, PartialEq, Eq)]
pub struct Handle(usize);

/// Slot states other than a [`StackKind`].
const EMPTY: u8 = 0;
/// Claimed by a writer that is still filling in the bounds.
const FILLING: u8 = 0xff;

struct Slot {
    /// Odd while a writer is filling the slot in; bumped twice by every
    /// register (the only time the bounds change), so a reader can tell the
    /// slot was refilled under it even if it holds the same kind again.
    seq: AtomicU64,
    /// [`EMPTY`], [`FILLING`] (claimed by a writer) or a [`StackKind`].
    state: AtomicU8,
    bottom: AtomicU64,
    top: AtomicU64,
}

impl Slot {
    const fn new() -> Slot {
        Slot { seq: AtomicU64::new(0), state: AtomicU8::new(EMPTY), bottom: AtomicU64::new(0), top: AtomicU64::new(0) }
    }

    /// Fill in the slot (the caller has claimed it): readers ignore it from
    /// the first bump until the second.
    fn write(&self, change: impl FnOnce()) {
        self.seq.fetch_add(1, Ordering::Relaxed);
        // The odd count is visible before any change.
        fence(Ordering::Release);
        change();
        // Release: a reader that sees the even count sees the changes.
        self.seq.fetch_add(1, Ordering::Release);
    }
}

/// The registered kernel stacks; lock-free for readers and writers.
pub struct Registry {
    slots: [Slot; MAX_STACKS],
}

impl Default for Registry {
    fn default() -> Self {
        Self::new()
    }
}

impl Registry {
    pub const fn new() -> Registry {
        Registry { slots: [const { Slot::new() }; MAX_STACKS] }
    }

    /// Publish a stack; `None` if every slot is taken.
    ///
    /// # Safety
    /// `range` (the stack, not its guard page) is mapped until it is
    /// unregistered: readers such as the crash reporter dereference what the
    /// registry lists. (The kernel's readers are crash reports, which stop
    /// the other CPUs before reading, so a stack retracted and then unmapped
    /// by a stopped CPU is never half-read; and tests and kmon, on the CPU
    /// that owns the stacks they look at. A reader running alongside writers
    /// would need a grace period after `unregister` before the unmapping.)
    ///
    /// # Panics
    /// If `range` is not page-aligned, nonempty and with its guard page above
    /// the null page (a caller bug; a malformed entry would break every
    /// reader).
    pub unsafe fn register(&self, kind: StackKind, range: StackRange) -> Option<Handle> {
        assert!(range.is_valid(), "malformed stack range {range:?}");
        debug_assert_eq!(StackKind::from_raw(kind as u8), Some(kind), "StackKind::ALL is missing {kind:?}");
        // Claiming (EMPTY -> FILLING) makes this writer the slot's only one.
        let (i, slot) =
            self.slots.iter().enumerate().find(|(_, s)| {
                s.state.compare_exchange(EMPTY, FILLING, Ordering::Acquire, Ordering::Relaxed).is_ok()
            })?;
        slot.write(|| {
            slot.bottom.store(range.bottom, Ordering::Relaxed);
            slot.top.store(range.top, Ordering::Relaxed);
            slot.state.store(kind as u8, Ordering::Relaxed);
        });
        Some(Handle(i))
    }

    /// Retract a stack before its memory goes away; returns what it was. A
    /// reader that loaded the kind before this reads the entry as it was
    /// (whole: the bounds only change when the slot is refilled).
    pub fn unregister(&self, handle: Handle) -> Registered {
        let r = self.get(handle.0).expect("a handle names a registered stack");
        self.slots[handle.0].state.store(EMPTY, Ordering::Release);
        r
    }

    /// Slot `i`, if it holds a registered stack: read as a seqlock, so an
    /// entry that changed while it was read (even back to the same kind)
    /// reads as empty, never as a mix of two entries.
    fn get(&self, i: usize) -> Option<Registered> {
        let slot = &self.slots[i];
        let seq = slot.seq.load(Ordering::Acquire);
        if seq % 2 == 1 {
            return None;
        }
        let kind = StackKind::from_raw(slot.state.load(Ordering::Relaxed));
        let range = StackRange { bottom: slot.bottom.load(Ordering::Relaxed), top: slot.top.load(Ordering::Relaxed) };
        // Keep those loads before the second count load.
        fence(Ordering::Acquire);
        if slot.seq.load(Ordering::Relaxed) != seq {
            return None;
        }
        kind.map(|kind| Registered { kind, range })
    }

    /// Every registered stack.
    pub fn iter(&self) -> impl Iterator<Item = Registered> + '_ {
        (0..MAX_STACKS).filter_map(|i| self.get(i))
    }

    /// Which registered stack, or which stack's guard page, `addr` is in.
    pub fn find(&self, addr: u64) -> Option<Hit> {
        self.iter().find_map(|r| {
            if r.range.span().contains(&addr) {
                Some(Hit::Stack(r))
            } else if (r.range.guard()..r.range.bottom).contains(&addr) {
                Some(Hit::Guard(r))
            } else {
                None
            }
        })
    }

    /// Is `[addr, addr + len)` entirely inside one registered stack?
    pub fn contains(&self, addr: u64, len: u64) -> bool {
        let Some(end) = addr.checked_add(len) else { return false };
        self.iter().any(|r| r.range.bottom <= addr && end <= r.range.top)
    }
}

#[cfg(test)]
mod tests {
    extern crate std;

    use super::*;
    use crate::window::Claims;
    use std::sync::atomic::AtomicBool;
    use std::vec::Vec;

    const BASE: u64 = 0xffff_d000_0000_0000;
    const K: u64 = 1024;

    /// A window over `[start, end)` (host tests: nothing is mapped).
    fn window(start: u64, end: u64) -> Window {
        // SAFETY: host tests never map anything.
        unsafe { Claims::new([(start, end)]) }.claim(0).unwrap()
    }

    /// The next stack's range from `w`.
    fn take(w: &mut StackWindow, size: u64) -> Option<StackRange> {
        w.take(size).map(|(range, _)| range)
    }

    /// Register `range` (host tests: nothing is dereferenced).
    fn register(reg: &Registry, kind: StackKind, range: StackRange) -> Option<Handle> {
        // SAFETY: host tests never read through registered ranges.
        unsafe { reg.register(kind, range) }
    }

    #[test]
    fn each_stack_gets_its_own_sub_window() {
        let mut w = StackWindow::new(window(BASE, BASE + 1024 * K));
        let (a, own_a) = w.take(64 * K).unwrap();
        let (b, own_b) = w.take(16 * K).unwrap();
        // Each sub-window is the stack alone: the guard page has no token.
        assert_eq!(own_a.range(), a.span());
        assert_eq!(own_b.range(), b.span());
        assert!(!own_a.owns_page(a.guard()) && !own_b.owns_page(b.guard()));
        assert!(own_a.range().end <= own_b.range().start);
    }

    #[test]
    fn window_puts_a_guard_page_below_each_stack() {
        let mut w = StackWindow::new(window(BASE, BASE + 1024 * K));
        let a = take(&mut w, 64 * K).unwrap();
        let b = take(&mut w, 16 * K).unwrap();
        assert_eq!(a, StackRange { bottom: BASE + GUARD_SIZE, top: BASE + GUARD_SIZE + 64 * K });
        assert_eq!(a.guard(), BASE);
        assert_eq!(a.span(), a.bottom..a.top);
        // The page above `a` is `b`'s guard.
        assert_eq!(b.guard(), a.top);
        assert_eq!(b.size(), 16 * K);
    }

    #[test]
    fn window_stops_at_its_limit() {
        let mut w = StackWindow::new(window(BASE, BASE + 5 * FRAME_SIZE));
        assert_eq!(take(&mut w, 4 * FRAME_SIZE), None, "a guard page below and a free page above need room too");
        let a = take(&mut w, 3 * FRAME_SIZE).unwrap();
        // The last stack still has an unused page above it.
        assert_eq!(a.top + GUARD_SIZE, BASE + 5 * FRAME_SIZE);
        assert_eq!(take(&mut w, FRAME_SIZE), None);
        let top = u64::MAX - FRAME_SIZE + 1;
        let mut at_top = StackWindow::new(window(top - FRAME_SIZE, top));
        assert_eq!(take(&mut at_top, FRAME_SIZE), None, "no room for the guard pages");
    }

    #[test]
    #[should_panic(expected = "not whole pages")]
    fn window_refuses_partial_pages() {
        take(&mut StackWindow::new(window(BASE, BASE + 1024 * K)), 100);
    }

    #[test]
    fn malformed_ranges_are_refused() {
        let bad = [
            StackRange { bottom: 0, top: FRAME_SIZE },              // no room for a guard
            StackRange { bottom: GUARD_SIZE, top: 2 * FRAME_SIZE }, // guard is the null page
            StackRange { bottom: BASE + FRAME_SIZE, top: BASE + FRAME_SIZE + 8 }, // misaligned top
            StackRange { bottom: BASE + 8, top: BASE + FRAME_SIZE }, // misaligned
            StackRange { bottom: BASE + FRAME_SIZE, top: BASE + FRAME_SIZE }, // empty
            StackRange { bottom: BASE + 2 * FRAME_SIZE, top: BASE + FRAME_SIZE }, // backwards
        ];
        for range in bad {
            let refused = std::panic::catch_unwind(|| register(&Registry::new(), StackKind::Test, range));
            assert!(refused.is_err(), "{range:?} was accepted");
        }
    }

    #[test]
    fn find_and_contains_know_stacks_and_guards() {
        let reg = Registry::new();
        let mut w = StackWindow::new(window(BASE, BASE + 1024 * K));
        let main = take(&mut w, 64 * K).unwrap();
        let ist = take(&mut w, 16 * K).unwrap();
        let hm = register(&reg, StackKind::Main, main).unwrap();
        let _hi = register(&reg, StackKind::DoubleFault, ist).unwrap();
        let main_r = Registered { kind: StackKind::Main, range: main };
        let ist_r = Registered { kind: StackKind::DoubleFault, range: ist };
        assert_eq!(reg.find(main.bottom), Some(Hit::Stack(main_r)));
        assert_eq!(reg.find(main.top - 1), Some(Hit::Stack(main_r)));
        assert_eq!(reg.find(main.bottom - 8), Some(Hit::Guard(main_r)));
        assert_eq!(reg.find(main.guard()), Some(Hit::Guard(main_r)));
        assert_eq!(reg.find(main.guard() - 1), None);
        // Directly above `main`: `ist`'s guard.
        assert_eq!(reg.find(main.top), Some(Hit::Guard(ist_r)));
        assert!(reg.contains(main.bottom, 64 * K));
        assert!(reg.contains(main.top - 16, 16));
        assert!(!reg.contains(main.top - 8, 16), "straddles the top");
        assert!(!reg.contains(main.bottom - 8, 16), "straddles the guard");
        assert!(!reg.contains(u64::MAX - 4, 16), "wraps around");
        assert_eq!(reg.unregister(hm), main_r);
        assert_eq!(reg.find(main.bottom), None);
        assert_eq!(reg.find(main.bottom - 8), None);
        assert!(!reg.contains(main.bottom, 16));
        assert_eq!(reg.iter().collect::<Vec<_>>(), [ist_r]);
    }

    #[test]
    fn a_full_registry_refuses_and_slots_are_reused() {
        let reg = Registry::new();
        let mut w = StackWindow::new(window(BASE, BASE + 4096 * K));
        let mut handles: Vec<_> = (0..MAX_STACKS)
            .map(|_| register(&reg, StackKind::Test, take(&mut w, FRAME_SIZE).unwrap()).unwrap())
            .collect();
        let extra = take(&mut w, FRAME_SIZE).unwrap();
        assert_eq!(register(&reg, StackKind::Test, extra), None);
        reg.unregister(handles.remove(7));
        assert_eq!(register(&reg, StackKind::Main, extra), Some(Handle(7)));
        assert_eq!(reg.find(extra.bottom), Some(Hit::Stack(Registered { kind: StackKind::Main, range: extra })));
    }

    #[test]
    fn every_kind_has_a_name_and_round_trips() {
        // Adding a variant must not compile until this match (and so, by
        // the comment, `ALL`) is updated: a kind missing from `ALL` would
        // register but be invisible to `find`.
        for k in StackKind::ALL {
            match k {
                StackKind::Main
                | StackKind::DoubleFault
                | StackKind::Nmi
                | StackKind::MachineCheck
                | StackKind::Test
                | StackKind::Thread => {} // keep in step with `StackKind::ALL`
            }
            assert_eq!(StackKind::from_raw(k as u8), Some(k));
            assert!(!k.name().is_empty());
            assert!(k as u8 != EMPTY && k as u8 != FILLING);
        }
        assert_eq!(StackKind::from_raw(EMPTY), None);
        assert_eq!(StackKind::from_raw(FILLING), None);
    }

    /// Readers racing a writer that keeps registering and retracting stacks
    /// in one slot only ever see whole entries: bounds that belong together,
    /// with the kind they were registered under. (`make test-host` also runs
    /// the stacks tests optimized, where compiler-reordering mistakes in the
    /// seqlock are likelier to surface.)
    ///
    /// A read that overlaps a write reads as empty, so whether the readers see
    /// an entry at all depends on scheduling: the writer keeps going until one
    /// has (within a cap, so a reader that can never see one fails, not hangs).
    #[test]
    fn readers_never_see_a_torn_entry() {
        const ROUNDS: u64 = 200_000;
        const MAX_ROUNDS: u64 = 100 * ROUNDS;
        let reg = Registry::new();
        let stop = AtomicBool::new(false);
        let sighted = AtomicBool::new(false);
        // Even-numbered stacks are Main, odd ones Test; bottom and top encode
        // the same index, so a mix of two entries is detectable.
        let range =
            |n: u64| StackRange { bottom: BASE + n * 0x10_0000 + GUARD_SIZE, top: BASE + n * 0x10_0000 + 0x8000 };
        let kind = |n: u64| if n % 2 == 0 { StackKind::Main } else { StackKind::Test };
        // Stops the readers even if the writer panics (else they spin forever).
        struct StopOnDrop<'a>(&'a AtomicBool);
        impl Drop for StopOnDrop<'_> {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Relaxed);
            }
        }
        std::thread::scope(|s| {
            s.spawn(|| {
                let _stop = StopOnDrop(&stop);
                let mut round = 0u64;
                while round < ROUNDS || (!sighted.load(Ordering::Relaxed) && round < MAX_ROUNDS) {
                    let n = round % ROUNDS;
                    let h = register(&reg, kind(n), range(n)).unwrap();
                    assert_eq!(h, Handle(0));
                    reg.unregister(h);
                    round += 1;
                }
            });
            let readers: Vec<_> = (0..3)
                .map(|_| {
                    s.spawn(|| {
                        let mut seen = 0u64;
                        while !stop.load(Ordering::Relaxed) {
                            if let Some(r) = reg.get(0) {
                                let n = (r.range.bottom - GUARD_SIZE - BASE) / 0x10_0000;
                                assert_eq!(r, Registered { kind: kind(n), range: range(n) }, "torn entry");
                                sighted.store(true, Ordering::Relaxed);
                                seen += 1;
                            }
                        }
                        seen
                    })
                })
                .collect();
            let seen: u64 = readers.into_iter().map(|r| r.join().unwrap()).sum();
            assert!(seen > 0, "the readers never saw an entry");
        });
    }
}
