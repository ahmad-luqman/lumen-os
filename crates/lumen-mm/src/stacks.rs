//! Kernel stacks: where they go in the stack window ([`StackWindow`]), and a
//! registry of them ([`Registry`]) that the crash reporter reads without
//! locks.
//!
//! Each stack gets an unmapped guard page directly below it. Stacks are laid
//! out upward, guard then stack, so the page directly above a stack is the
//! next stack's guard (or not handed out): running off either end faults
//! instead of silently corrupting a neighbour. Freed stacks' addresses are
//! taken again (#300), once every CPU has flushed them from its TLB.
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

/// How many stacks can be registered at once: five per CPU (main, #DF, NMI,
/// #MC, #DB) for up to eight CPUs, and the rest for kernel threads (one each)
/// and tests.
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
    /// IST stack for debug exceptions (`#DB`, vector 1).
    Debug = 7,
}

impl StackKind {
    const ALL: [StackKind; 7] = [
        StackKind::Main,
        StackKind::DoubleFault,
        StackKind::Nmi,
        StackKind::MachineCheck,
        StackKind::Test,
        StackKind::Thread,
        StackKind::Debug,
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
            StackKind::Debug => "#DB IST",
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

/// How many free ranges a [`StackWindow`] keeps. Adjacent free ranges are
/// joined, so taken slots separate any two of them: there are at most one
/// more than the slots taken, which the registry caps at [`MAX_STACKS`] (plus
/// the few being set up or given back at a time). Twice that leaves room.
pub const FREE_RANGES: usize = 2 * MAX_STACKS;

/// A stack's addresses from [`StackWindow::take`]: its guard page and the
/// stack above it, each owned by a [`Window`]. Only the stack's window is
/// handed out ([`StackSlot::window`]): the guard page's stays in here, so no
/// one can ever map it, and goes back with the slot
/// ([`StackWindow::give_back`]). Dropping a slot gives its addresses up for
/// good (its guard page still never mapped).
#[derive(Debug)]
#[must_use = "dropping a stack slot gives its addresses up for good"]
pub struct StackSlot {
    guard: Window,
    stack: Window,
}

impl StackSlot {
    /// The stack's addresses (its guard page is just below).
    pub fn range(&self) -> StackRange {
        let r = self.stack.range();
        StackRange { bottom: r.start, top: r.end }
    }

    /// The window of the stack's own pages (not the guard page): what maps
    /// and unmaps them.
    pub fn window(&mut self) -> &mut Window {
        &mut self.stack
    }

    /// [`window`](Self::window), read-only.
    pub fn stack_window(&self) -> &Window {
        &self.stack
    }
}

/// A free range of the stack window, and the retirement generation of the
/// newest stack given back into it ([`StackWindow::give_back`]).
#[derive(Debug)]
struct Free {
    window: Window,
    retired: u64,
}

/// Hands out stack addresses in the stack window, a guard page below each,
/// and takes them back for reuse.
///
/// **Layout.** Slots tile the window from its bottom up: each is a guard
/// page, then the stack. So the page directly above a stack is the next
/// slot's guard page, or the first page of a free range or of the untouched
/// rest of the window, which becomes a guard page when it is taken: running
/// off either end of a stack faults instead of silently corrupting a
/// neighbour. A guard page is never mapped: its window stays inside the
/// [`StackSlot`].
///
/// **Reuse (#300).** A stack given back ([`give_back`](Self::give_back))
/// joins the free ranges, merged with its free neighbours, and a freed slot
/// is taken again before the untouched rest of the window is: steady-state
/// spawning and exiting reuses the same addresses, whose page tables are
/// already there, instead of mapping a new table every 2 MiB for good. A
/// freed stack's pages may still be cached in other CPUs' TLBs (its unmap
/// flushes only the CPU that ran it), so a free range carries the
/// generation it was retired at, and [`take`](Self::take) hands it out only
/// once every CPU has flushed through that generation (`flushed`): the
/// kernel's `mm::tlb` counts the generations and makes the CPUs flush.
/// Taking is first fit: the lowest free range that fits and has been
/// flushed through, carved from its bottom (an exact fit's page above is a
/// taken slot's guard page, or the untouched rest's first page), else the
/// untouched rest, keeping the page above the newest stack free.
#[derive(Debug)]
pub struct StackWindow {
    /// The untouched rest of the window (`None` until [`claim`](Self::claim)).
    rest: Option<Window>,
    free: [Option<Free>; FREE_RANGES],
}

impl Default for StackWindow {
    fn default() -> Self {
        Self::empty()
    }
}

impl StackWindow {
    /// A stack window with no addresses yet ([`claim`](Self::claim) gives
    /// it some): constant, so the kernel's can be a static.
    pub const fn empty() -> StackWindow {
        StackWindow { rest: None, free: [const { None }; FREE_RANGES] }
    }

    /// A stack window over `window`.
    pub fn new(window: Window) -> StackWindow {
        let mut w = StackWindow::empty();
        w.claim(window);
        w
    }

    /// Hand stacks out of `window` from now on.
    ///
    /// # Panics
    /// If this already has a window (a caller bug).
    pub fn claim(&mut self, window: Window) {
        assert!(self.rest.is_none(), "the stack window is claimed twice");
        self.rest = Some(window);
    }

    /// Has [`claim`](Self::claim) given this its window?
    pub fn is_claimed(&self) -> bool {
        self.rest.is_some()
    }

    /// The lowest free range a stack of `size` bytes fits in, among those
    /// every CPU has flushed through if `flushed` is given.
    fn lowest_fit(&self, size: u64, flushed: Option<u64>) -> Option<usize> {
        let need = GUARD_SIZE.checked_add(size)?;
        self.free
            .iter()
            .enumerate()
            .filter_map(|(i, f)| f.as_ref().map(|f| (i, f)))
            .filter(|(_, f)| f.window.len() >= need && flushed.is_none_or(|g| f.retired <= g))
            .min_by_key(|(_, f)| f.window.range().start)
            .map(|(i, _)| i)
    }

    /// The generation every CPU must have flushed through before the lowest
    /// free range a stack of `size` bytes fits in may be handed out, if they
    /// haven't yet (`flushed`); `None` if no free range fits or the lowest
    /// one is ready.
    pub fn wait_for(&self, size: u64, flushed: u64) -> Option<u64> {
        let i = self.lowest_fit(size, None)?;
        let retired = self.free[i].as_ref().expect("lowest_fit names a free range").retired;
        (retired > flushed).then_some(retired)
    }

    /// Addresses for a stack of `size` bytes (a nonzero multiple of the page
    /// size), with its guard page below it: from the lowest free range that
    /// fits and that every CPU has flushed through (`flushed`), else from
    /// the untouched rest of the window; `None` once neither has room.
    pub fn take(&mut self, size: u64, flushed: u64) -> Option<StackSlot> {
        assert!(size != 0 && size.is_multiple_of(FRAME_SIZE), "stack size {size:#x} is not whole pages");
        let len = GUARD_SIZE.checked_add(size)?;
        if let Some(i) = self.lowest_fit(size, Some(flushed)) {
            let Free { mut window, retired } = self.free[i].take().expect("lowest_fit names a free range");
            let slot = carve(&mut window, size);
            if !window.is_empty() {
                // The rest of the range, where it came from.
                self.free[i] = Some(Free { window, retired });
            }
            return slot;
        }
        let rest = self.rest.as_mut()?;
        // The page above the stack must be free too (the next guard).
        if len.checked_add(GUARD_SIZE)? > rest.len() {
            return None;
        }
        carve(rest, size)
    }

    /// Take back `slot`, whose pages are all unmapped, retired at
    /// generation `retired` (a CPU that has flushed through it holds none of
    /// its pages in its TLB): it joins its free neighbours. False if it
    /// couldn't be kept (more than [`FREE_RANGES`] free ranges, which can't
    /// happen while at most [`MAX_STACKS`] slots are taken): its addresses
    /// are then given up for good, as dropping it would.
    #[must_use = "a slot that couldn't be kept is lost for good"]
    pub fn give_back(&mut self, slot: StackSlot, retired: u64) -> bool {
        let StackSlot { guard, stack } = slot;
        let mut window = guard.join(stack).unwrap_or_else(|_| unreachable!("a guard page is just below its stack"));
        let mut retired = retired;
        // Join the free range just below, then the one just above.
        let start = window.range().start;
        if let Some(below) = self.free.iter_mut().find(|f| f.as_ref().is_some_and(|f| f.window.range().end == start)) {
            let below = below.take().expect("found");
            retired = retired.max(below.retired);
            window = below.window.join(window).unwrap_or_else(|_| unreachable!("they meet"));
        }
        let end = window.range().end;
        if let Some(above) = self.free.iter_mut().find(|f| f.as_ref().is_some_and(|f| f.window.range().start == end)) {
            let above = above.take().expect("found");
            retired = retired.max(above.retired);
            window = window.join(above.window).unwrap_or_else(|_| unreachable!("they meet"));
        }
        match self.free.iter_mut().find(|f| f.is_none()) {
            Some(empty) => {
                *empty = Some(Free { window, retired });
                true
            }
            None => false,
        }
    }

    /// How many free ranges there are.
    pub fn free_ranges(&self) -> usize {
        self.free.iter().flatten().count()
    }
}

/// A slot for a stack of `size` bytes from the bottom of `window`, if it has
/// room for the guard page and the stack.
fn carve(window: &mut Window, size: u64) -> Option<StackSlot> {
    if window.len() < GUARD_SIZE.checked_add(size)? {
        return None;
    }
    let guard = window.split_off(GUARD_SIZE)?;
    let stack = window.split_off(size)?;
    Some(StackSlot { guard, stack })
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
    /// by a stopped CPU is never half-read; and tests and the shell, on the CPU
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

    /// The next stack's range from `w` (from the untouched rest, or a free
    /// range every CPU has flushed through; nothing is given back here).
    fn take(w: &mut StackWindow, size: u64) -> Option<StackRange> {
        w.take(size, u64::MAX).map(|slot| slot.range())
    }

    /// Register `range` (host tests: nothing is dereferenced).
    fn register(reg: &Registry, kind: StackKind, range: StackRange) -> Option<Handle> {
        // SAFETY: host tests never read through registered ranges.
        unsafe { reg.register(kind, range) }
    }

    #[test]
    fn each_stack_gets_its_own_sub_window() {
        let mut w = StackWindow::new(window(BASE, BASE + 1024 * K));
        let a = w.take(64 * K, 0).unwrap();
        let b = w.take(16 * K, 0).unwrap();
        // Each stack's window is the stack alone, not its guard page.
        assert_eq!(a.stack_window().range(), a.range().span());
        assert_eq!(b.stack_window().range(), b.range().span());
        assert!(!a.stack_window().owns_page(a.range().guard()) && !b.stack_window().owns_page(b.range().guard()));
        assert!(a.range().top <= b.range().guard());
    }

    /// A freed slot is taken again, once every CPU has flushed through the
    /// generation it was retired at, at the same addresses and with the same
    /// guard page below it (and the next slot's guard page above it); until
    /// then the untouched rest of the window is used.
    #[test]
    fn a_freed_slot_is_reused_with_its_guard_page_once_flushed() {
        let mut w = StackWindow::new(window(BASE, BASE + 1024 * K));
        let a = w.take(64 * K, 0).unwrap();
        let b = w.take(64 * K, 0).unwrap();
        let (ra, rb) = (a.range(), b.range());
        assert_eq!(w.wait_for(64 * K, 0), None, "nothing to wait for: nothing is free");
        assert!(w.give_back(a, 5));
        assert_eq!(w.free_ranges(), 1);
        // Not yet flushed through generation 5: wait for it, or go on above.
        assert_eq!(w.wait_for(64 * K, 4), Some(5));
        assert_eq!(w.wait_for(64 * K, 5), None);
        let c = w.take(64 * K, 4).unwrap();
        assert_eq!(c.range().guard(), rb.top, "past the pending slot");
        // Flushed: the freed slot comes back whole.
        let mut again = w.take(64 * K, 5).unwrap();
        assert_eq!(again.range(), ra);
        assert_eq!(again.window().range(), ra.span());
        assert!(!again.stack_window().owns_page(ra.guard()), "the guard page stays unowned");
        assert_eq!(rb.guard(), ra.top, "the page above is the next slot's guard");
        assert_eq!(w.free_ranges(), 0);
        drop((b, c));
    }

    /// Free neighbours are joined (the generation the newest one's), so a
    /// bigger stack fits where smaller ones were, and a smaller one is
    /// carved from the bottom of a bigger free range, the rest kept.
    #[test]
    fn free_neighbours_join_and_big_ranges_are_carved() {
        let mut w = StackWindow::new(window(BASE, BASE + 1024 * K));
        let slots: Vec<_> = (0..3).map(|_| w.take(16 * K, 0).unwrap()).collect();
        let first = slots[0].range();
        let [a, b, c] = <[StackSlot; 3]>::try_from(slots).unwrap();
        assert!(w.give_back(a, 1));
        assert!(w.give_back(c, 3));
        assert_eq!(w.free_ranges(), 2);
        assert!(w.give_back(b, 2), "joins both neighbours");
        assert_eq!(w.free_ranges(), 1);
        // Three 16 KiB slots and their guards: room for a 56 KiB stack.
        assert_eq!(w.wait_for(56 * K, 2), Some(3), "the newest generation counts");
        let big = w.take(56 * K, 3).unwrap();
        assert_eq!(big.range().guard(), first.guard());
        assert_eq!(w.free_ranges(), 0);
        assert!(w.give_back(big, 4));
        let small = w.take(4 * K, 4).unwrap();
        assert_eq!(small.range(), StackRange { bottom: first.bottom, top: first.bottom + 4 * K });
        assert_eq!(w.free_ranges(), 1, "the rest of the range stays free");
        let next = w.take(4 * K, 4).unwrap();
        assert_eq!(next.range().guard(), small.range().top);
        drop((small, next));
    }

    /// Free ranges past `FREE_RANGES` can't be kept: `give_back` says so.
    #[test]
    fn free_ranges_past_the_limit_are_refused() {
        let mut w = StackWindow::new(window(BASE, BASE + 2 * (FREE_RANGES as u64 + 2) * 2 * FRAME_SIZE));
        let slots: Vec<_> = (0..2 * (FREE_RANGES + 1)).map(|_| w.take(FRAME_SIZE, 0).unwrap()).collect();
        // Every other one: no two free ranges meet.
        let mut kept = Vec::new();
        for (i, s) in slots.into_iter().enumerate() {
            if i % 2 == 0 { kept.push(w.give_back(s, 1)) } else { drop(s) }
        }
        assert_eq!(kept.iter().filter(|&&k| k).count(), FREE_RANGES);
        assert_eq!(kept.last(), Some(&false));
    }

    /// Page-table frames in a map, counted (a fake for the table counts).
    #[derive(Default)]
    struct Tables {
        tables: std::collections::BTreeMap<u64, [u64; crate::paging::ENTRIES]>,
    }

    impl crate::paging::TableMemory for Tables {
        fn read(&self, table: crate::PhysFrame, index: usize) -> Option<u64> {
            Some(self.tables[&table.as_u64()][index])
        }
    }

    impl crate::paging::TableMemoryMut for Tables {
        fn write(&mut self, table: crate::PhysFrame, index: usize, value: u64) {
            self.tables.get_mut(&table.as_u64()).unwrap()[index] = value;
        }

        fn alloc_table(&mut self) -> Option<crate::PhysFrame> {
            let f = crate::PhysFrame::from_u64(0x10_0000 + self.tables.len() as u64 * FRAME_SIZE).unwrap();
            self.tables.insert(f.as_u64(), [0; crate::paging::ENTRIES]);
            Some(f)
        }
    }

    /// #300: threads spawned and exiting, their stacks mapped and unmapped
    /// through the real page-table code, map a bounded number of page tables
    /// however many come and go (without reuse, 4000 64 KiB stacks walk
    /// 266 MiB of the window and map an L1 table every 2 MiB of it, for
    /// good). Up to 8 live at a time, freed out of order, each retired at
    /// its own generation and every CPU flushed through it before the next
    /// spawn (as `mm::tlb::sync` makes sure).
    #[test]
    fn spawning_and_exiting_maps_a_bounded_number_of_tables() {
        use crate::paging::{self, Attrs, PageSize};
        let mut w = StackWindow::new(window(BASE, BASE + (1 << 40)));
        let mut mem = Tables::default();
        let root = paging::TableMemoryMut::alloc_table(&mut mem).unwrap();
        let frame = crate::PhysFrame::from_u64(0x8000_0000).unwrap();
        let v = |a: u64| crate::VirtAddr::new(a).unwrap();
        let (mut live, mut generation, mut seed) = (Vec::new(), 0u64, 0x2545_f491_4f6c_dd1du64);
        let mut tables_after_warmup = 0;
        for spawn in 0..4000 {
            let slot = w.take(64 * K, generation).expect("the window has room");
            for page in slot.range().span().step_by(FRAME_SIZE as usize) {
                paging::map(&mut mem, root, v(page), frame, PageSize::Size4K, Attrs::DATA).unwrap();
            }
            live.push(slot);
            if live.len() == 8 || spawn == 3999 {
                // Some exit, oldest or not.
                seed ^= seed << 13;
                seed ^= seed >> 7;
                seed ^= seed << 17;
                let exiting = live.swap_remove(seed as usize % live.len());
                for page in exiting.range().span().step_by(FRAME_SIZE as usize) {
                    paging::unmap(&mut mem, root, v(page)).unwrap();
                }
                generation += 1;
                assert!(w.give_back(exiting, generation));
            }
            if spawn == 100 {
                tables_after_warmup = mem.tables.len();
            }
        }
        // The root, an L3 and an L2 table, and the L1 tables 8 live stacks
        // can straddle: their slots stay within the lowest 9 x 68 KiB.
        assert!(mem.tables.len() <= 5, "{} tables", mem.tables.len());
        assert_eq!(mem.tables.len(), tables_after_warmup, "a table mapped after the first 100 spawns");
        assert!(w.free_ranges() <= live.len() + 1);
    }

    /// A randomized model: stacks of mixed sizes taken and given back, every
    /// CPU flushing at random times. Live stacks never overlap each other or
    /// each other's guard pages; the page above each live stack is in no
    /// live stack; a free range is never handed out before every CPU has
    /// flushed through its newest stack's generation; and the free ranges
    /// stay at most one more than the slots taken.
    #[test]
    fn mixed_sizes_keep_guards_and_wait_for_flushes() {
        let mut w = StackWindow::new(window(BASE, BASE + (1 << 30)));
        // Live slots, and every page's retirement generation once freed.
        let mut live: Vec<StackSlot> = Vec::new();
        let mut retired_at: std::collections::BTreeMap<u64, u64> = std::collections::BTreeMap::new();
        let (mut generation, mut flushed, mut seed) = (0u64, 0u64, 0x9e37_79b9_7f4a_7c15u64);
        let mut next = || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut reused = 0;
        for _ in 0..20_000 {
            let r = next();
            if r % 7 == 0 {
                flushed = generation;
            }
            if live.len() < 64 && (live.is_empty() || r % 3 != 0) {
                let size = [1, 2, 4, 16, 17, 64][(next() % 6) as usize] * FRAME_SIZE;
                let slot = w.take(size, flushed).expect("room");
                let range = slot.range();
                assert_eq!(range.size(), size);
                for page in (range.guard()..range.top).step_by(FRAME_SIZE as usize) {
                    if let Some(g) = retired_at.remove(&page) {
                        assert!(g <= flushed, "page {page:#x} retired at {g}, handed out with {flushed} flushed");
                        reused += 1;
                    }
                }
                for other in &live {
                    let o = other.range();
                    assert!(range.top <= o.guard() || o.top <= range.guard(), "{range:?} overlaps {o:?}");
                    assert!(!o.span().contains(&range.top), "the page above {range:?} is in {o:?}");
                    assert!(!range.span().contains(&o.top), "the page above {o:?} is in {range:?}");
                }
                live.push(slot);
            } else {
                let slot = live.swap_remove((next() as usize) % live.len());
                generation += 1;
                let range = slot.range();
                for page in (range.guard()..range.top).step_by(FRAME_SIZE as usize) {
                    retired_at.insert(page, generation);
                }
                assert!(w.give_back(slot, generation));
            }
            assert!(w.free_ranges() <= live.len() + 1, "{} free ranges, {} live", w.free_ranges(), live.len());
        }
        assert!(reused > 10_000, "only {reused} pages reused");
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
                | StackKind::Thread
                | StackKind::Debug => {} // keep in step with `StackKind::ALL`
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
        let kind = |n: u64| if n.is_multiple_of(2) { StackKind::Main } else { StackKind::Test };
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
