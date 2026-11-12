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
//! a stack overflow ([`Registry::find`] says an address is in a stack's guard
//! page). Writers publish an entry's bounds before its kind and retract the
//! kind before the memory goes away, so a reader never sees a half-written
//! entry or one whose memory is gone (see [`Registry::get`]).

use core::sync::atomic::{AtomicU8, AtomicU64, Ordering, fence};

use crate::FRAME_SIZE;

/// The unmapped page below every stack.
pub const GUARD_SIZE: u64 = FRAME_SIZE;

/// How many stacks can be registered at once.
pub const MAX_STACKS: usize = 32;

/// What a stack is for (names it in crash reports). Stored in the registry as
/// one byte, so a reader gets it in one atomic load.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum StackKind {
    /// The boot CPU's kernel stack (`kmain` onward).
    Main = 1,
    /// IST stack for double faults.
    DoubleFault = 2,
    /// IST stack for NMIs.
    Nmi = 3,
    /// IST stack for machine checks.
    MachineCheck = 4,
    /// Allocated by a test.
    Test = 5,
}

impl StackKind {
    const ALL: [StackKind; 5] =
        [StackKind::Main, StackKind::DoubleFault, StackKind::Nmi, StackKind::MachineCheck, StackKind::Test];

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
}

/// Hands out stack addresses in the stack window, upward, a guard page below
/// each. Addresses are never reused: the window is far bigger than the
/// kernel will ever need (1 TiB for stacks of tens of KiB).
#[derive(Debug)]
pub struct StackWindow {
    next: u64,
    limit: u64,
}

impl StackWindow {
    /// A window `[base, limit)`, both page-aligned.
    pub const fn new(base: u64, limit: u64) -> StackWindow {
        assert!(base.is_multiple_of(FRAME_SIZE) && limit.is_multiple_of(FRAME_SIZE) && base <= limit);
        StackWindow { next: base, limit }
    }

    /// Addresses for the next stack of `size` bytes (a nonzero multiple of
    /// the page size), with its guard page below it; `None` once the window
    /// is full.
    pub fn take(&mut self, size: u64) -> Option<StackRange> {
        assert!(size != 0 && size.is_multiple_of(FRAME_SIZE), "stack size {size:#x} is not whole pages");
        let bottom = self.next.checked_add(GUARD_SIZE)?;
        let top = bottom.checked_add(size).filter(|&top| top <= self.limit)?;
        self.next = top;
        Some(StackRange { bottom, top })
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
    /// In the guard page below a registered stack: an overflow.
    Guard(Registered),
}

/// Proof of a registration, given back to [`Registry::unregister`]. Not
/// `Clone`: an entry is retracted once.
#[derive(Debug, PartialEq, Eq)]
pub struct Handle(usize);

/// Slot states other than a [`StackKind`].
const EMPTY: u8 = 0;
/// Claimed by a writer that is still filling in the bounds.
const FILLING: u8 = 0xff;

struct Slot {
    /// [`EMPTY`], [`FILLING`] or a [`StackKind`].
    state: AtomicU8,
    bottom: AtomicU64,
    top: AtomicU64,
}

impl Slot {
    const fn new() -> Slot {
        Slot { state: AtomicU8::new(EMPTY), bottom: AtomicU64::new(0), top: AtomicU64::new(0) }
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

    /// Publish a stack (whose memory must already be mapped); `None` if every
    /// slot is taken.
    pub fn register(&self, kind: StackKind, range: StackRange) -> Option<Handle> {
        let (i, slot) =
            self.slots.iter().enumerate().find(|(_, s)| {
                s.state.compare_exchange(EMPTY, FILLING, Ordering::Acquire, Ordering::Relaxed).is_ok()
            })?;
        slot.bottom.store(range.bottom, Ordering::Relaxed);
        slot.top.store(range.top, Ordering::Relaxed);
        // Release: a reader that sees the kind sees the bounds.
        slot.state.store(kind as u8, Ordering::Release);
        Some(Handle(i))
    }

    /// Retract a stack before its memory goes away; returns what it was.
    pub fn unregister(&self, handle: Handle) -> Registered {
        let r = self.get(handle.0).expect("a handle names a registered stack");
        self.slots[handle.0].state.store(EMPTY, Ordering::Release);
        r
    }

    /// Slot `i`, if it holds a registered stack. The kind is read before and
    /// after the bounds; if it changed in between (the entry was retracted
    /// or replaced), the slot reads as empty, so a reader never mixes two
    /// entries' bounds.
    fn get(&self, i: usize) -> Option<Registered> {
        let slot = &self.slots[i];
        let state = slot.state.load(Ordering::Acquire);
        let kind = StackKind::from_raw(state)?;
        let range = StackRange { bottom: slot.bottom.load(Ordering::Relaxed), top: slot.top.load(Ordering::Relaxed) };
        // Keep the bounds loads before the second state load.
        fence(Ordering::Acquire);
        (slot.state.load(Ordering::Relaxed) == state).then_some(Registered { kind, range })
    }

    /// Every registered stack.
    pub fn iter(&self) -> impl Iterator<Item = Registered> + '_ {
        (0..MAX_STACKS).filter_map(|i| self.get(i))
    }

    /// Which registered stack, or which stack's guard page, `addr` is in.
    pub fn find(&self, addr: u64) -> Option<Hit> {
        self.iter().find_map(|r| {
            if (r.range.bottom..r.range.top).contains(&addr) {
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
    use std::sync::atomic::AtomicBool;
    use std::vec::Vec;

    const BASE: u64 = 0xffff_d000_0000_0000;
    const K: u64 = 1024;

    #[test]
    fn window_puts_a_guard_page_below_each_stack() {
        let mut w = StackWindow::new(BASE, BASE + 1024 * K);
        let a = w.take(64 * K).unwrap();
        let b = w.take(16 * K).unwrap();
        assert_eq!(a, StackRange { bottom: BASE + GUARD_SIZE, top: BASE + GUARD_SIZE + 64 * K });
        assert_eq!(a.guard(), BASE);
        // The page above `a` is `b`'s guard.
        assert_eq!(b.guard(), a.top);
        assert_eq!(b.size(), 16 * K);
    }

    #[test]
    fn window_stops_at_its_limit() {
        let mut w = StackWindow::new(BASE, BASE + 4 * FRAME_SIZE);
        assert_eq!(w.take(4 * FRAME_SIZE), None, "the guard page needs room too");
        let a = w.take(3 * FRAME_SIZE).unwrap();
        assert_eq!(a.top, BASE + 4 * FRAME_SIZE);
        assert_eq!(w.take(FRAME_SIZE), None);
        let mut top = StackWindow::new(u64::MAX - FRAME_SIZE + 1, u64::MAX - FRAME_SIZE + 1);
        assert_eq!(top.take(FRAME_SIZE), None, "no overflow at the top of the address space");
    }

    #[test]
    #[should_panic(expected = "not whole pages")]
    fn window_refuses_partial_pages() {
        StackWindow::new(BASE, BASE + 1024 * K).take(100);
    }

    #[test]
    fn find_and_contains_know_stacks_and_guards() {
        let reg = Registry::new();
        let mut w = StackWindow::new(BASE, BASE + 1024 * K);
        let main = w.take(64 * K).unwrap();
        let ist = w.take(16 * K).unwrap();
        let hm = reg.register(StackKind::Main, main).unwrap();
        let _hi = reg.register(StackKind::DoubleFault, ist).unwrap();
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
        let mut w = StackWindow::new(BASE, BASE + 4096 * K);
        let mut handles: Vec<_> =
            (0..MAX_STACKS).map(|_| reg.register(StackKind::Test, w.take(FRAME_SIZE).unwrap()).unwrap()).collect();
        let extra = w.take(FRAME_SIZE).unwrap();
        assert_eq!(reg.register(StackKind::Test, extra), None);
        reg.unregister(handles.remove(7));
        assert_eq!(reg.register(StackKind::Main, extra), Some(Handle(7)));
        assert_eq!(reg.find(extra.bottom), Some(Hit::Stack(Registered { kind: StackKind::Main, range: extra })));
    }

    #[test]
    fn every_kind_has_a_name_and_round_trips() {
        for k in StackKind::ALL {
            assert_eq!(StackKind::from_raw(k as u8), Some(k));
            assert!(!k.name().is_empty());
        }
        assert_eq!(StackKind::from_raw(EMPTY), None);
        assert_eq!(StackKind::from_raw(FILLING), None);
    }

    /// Readers racing a writer that keeps registering and retracting stacks
    /// only ever see whole entries: bounds that belong together, with the
    /// kind they were registered under.
    #[test]
    fn readers_never_see_a_torn_entry() {
        let reg = Registry::new();
        let stop = AtomicBool::new(false);
        // Even-numbered stacks are Main, odd ones Test; bottom and top encode
        // the same index, so a mix of two entries is detectable.
        let range =
            |n: u64| StackRange { bottom: BASE + n * 0x10_0000 + GUARD_SIZE, top: BASE + n * 0x10_0000 + 0x8000 };
        let kind = |n: u64| if n % 2 == 0 { StackKind::Main } else { StackKind::Test };
        std::thread::scope(|s| {
            s.spawn(|| {
                for n in 0..200_000u64 {
                    let h = reg.register(kind(n), range(n)).unwrap();
                    reg.unregister(h);
                }
                stop.store(true, Ordering::Relaxed);
            });
            for _ in 0..3 {
                s.spawn(|| {
                    let mut seen = 0u64;
                    while !stop.load(Ordering::Relaxed) {
                        for r in reg.iter() {
                            let n = (r.range.bottom - GUARD_SIZE - BASE) / 0x10_0000;
                            assert_eq!(r, Registered { kind: kind(n), range: range(n) }, "torn entry");
                            seen += 1;
                        }
                    }
                    std::println!("reader saw {seen} entries");
                });
            }
        });
    }
}
