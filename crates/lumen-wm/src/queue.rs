//! A window's event queue (#153 keeps one per window, behind its lock).
//!
//! **Entries and flags.** The queue holds key presses and releases, pointer
//! presses, moves and releases, and automation requests ([`Class`]).
//! `Lost`, `Close`, a focus loss and a focus gain are not entries: they are
//! flags beside the queue, set with [`EventQueue::set_lost`],
//! [`EventQueue::request_close`] and [`EventQueue::set_focus`], so they
//! need no slot and can never be dropped (an entry type for them can't be
//! pushed: [`Class`] has no class for one).
//!
//! **Flags in their place.** Each flag is delivered after the entries
//! queued before it was set (a flag set again moves to the end).
//! Delivering it first, ahead of older entries, would let an owner clear
//! its held keys and buttons on `FocusLost` or `Lost` and then see a press
//! queued before it, whose release (a stray now) never comes: a key stuck
//! down. A loss is never folded away: a loss then a gain (a pause, then
//! resuming) delivers both, so the owner still releases what it held; a
//! gain then a loss delivers only the loss. When several are due at once
//! they come in the order `Lost`, focus lost, focus gained, `Close`.
//!
//! **The queue:** [`CAPACITY`] entries.
//! - A move replaces a move at the tail (unless a flag is due after the
//!   tail).
//! - Presses, moves and requests are queued only while fewer than
//!   [`RATIONED`] entries are: past that a press or move is dropped
//!   (counted in `rationed`) and a request refused ([`Pushed::Busy`]: `ui
//!   invoke` says "window <id> busy").
//! - A release is queued, in any free slot, only if the queue took that
//!   key's or button's press. Any other release is ignored, counted as
//!   `stray_forgotten` if its press was forgotten (by `FocusLost` or
//!   `Lost`) or dropped, else as `stray_unpressed` (a release of nothing
//!   pressed: a bug upstream). A release of a press taken that doesn't fit
//!   sets `Lost` (counted in `overflowed`).
//!
//! The first drop since the queue last emptied is [`Pushed::Dropped`] with
//! `first_in_burst` set, for #153's one `WARN desktop: window <id> is
//! behind: dropping events` per burst. After the window is destroyed the
//! queue is dead ([`EventQueue::kill`]): pushes and flags are counted
//! (`dead_pushes`), never queued.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

/// Entries the queue holds.
pub const CAPACITY: usize = 64;
/// Presses, moves and requests are queued only while fewer entries than
/// this are.
pub const RATIONED: usize = 48;

/// What kind of entry an event is, for the queue's rules.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Class<K> {
    /// A key or button went down (`K` names which).
    Press(K),
    /// A key or button came up.
    Release(K),
    Move,
    /// An automation request (#156).
    Request,
}

/// An event a window's queue can hold.
pub trait QueueEvent {
    /// Names a key or button, to match a release with its press.
    type Key: Copy + PartialEq;
    fn class(&self) -> Class<Self::Key>;
}

/// What [`EventQueue::next_event`] returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Next<E> {
    /// Events were lost: treat every key and button as released.
    Lost,
    /// The window gained (`true`) or lost the focus; losing it also means
    /// every key and button is released.
    Focus(bool),
    /// The close box was clicked, or `ui invoke <root> close`.
    Close,
    Event(E),
}

/// What [`EventQueue::push`] did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Pushed {
    Queued,
    /// A move replaced the move at the tail.
    Merged,
    /// Dropped (counted in `rationed` or `overflowed`); `first_in_burst`
    /// if it is the first since the queue last emptied.
    Dropped {
        first_in_burst: bool,
    },
    /// A request refused: too many entries queued.
    Busy,
    /// A release whose press the queue didn't take: ignored.
    Stray,
    /// The queue is dead (its window destroyed).
    Dead,
}

/// What the queue counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueueCounts {
    /// Presses and moves dropped past [`RATIONED`].
    pub rationed: u64,
    /// Releases that didn't fit at [`CAPACITY`] (each set `Lost`).
    pub overflowed: u64,
    /// Requests refused as busy.
    pub busy: u64,
    /// Releases whose press was forgotten (`FocusLost`, `Lost`) or
    /// dropped: expected after a reset or a drop.
    pub stray_forgotten: u64,
    /// Releases of a key or button never pressed: never expected.
    pub stray_unpressed: u64,
    /// Pushes and flags after the queue died.
    pub dead_pushes: u64,
    /// Moves merged into the move at the tail.
    pub merged: u64,
}

impl QueueCounts {
    /// Every event dropped (the `windows` command's `lost` column).
    pub fn lost(&self) -> u64 {
        self.rationed + self.overflowed
    }
}

/// A window's event queue: see the module's doc.
#[derive(Clone, Debug)]
pub struct EventQueue<E: QueueEvent> {
    entries: VecDeque<E>,
    /// Keys and buttons whose press was queued and whose release wasn't.
    pressed: Vec<E::Key>,
    /// Keys and buttons whose press was forgotten or dropped and whose
    /// release hasn't come.
    forgotten: Vec<E::Key>,
    /// Each flag set, with how many entries come before it.
    lost: Option<usize>,
    focus_lost: Option<usize>,
    focus_gained: Option<usize>,
    close: Option<usize>,
    dead: bool,
    behind: bool,
    counts: QueueCounts,
}

impl<E: QueueEvent> Default for EventQueue<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E: QueueEvent> EventQueue<E> {
    /// An empty queue. Its [`CAPACITY`] entries are allocated here, once,
    /// with room for 8 keys or buttons held at once (more grow it).
    pub fn new() -> Self {
        EventQueue {
            entries: VecDeque::with_capacity(CAPACITY),
            pressed: Vec::with_capacity(8),
            forgotten: Vec::with_capacity(8),
            lost: None,
            focus_lost: None,
            focus_gained: None,
            close: None,
            dead: false,
            behind: false,
            counts: QueueCounts::default(),
        }
    }

    /// Entries queued (pending flags aren't counted).
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// No entry queued; a flag may still be pending (owners block on
    /// [`EventQueue::is_idle`]).
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Nothing to deliver: no entry and no flag.
    pub fn is_idle(&self) -> bool {
        self.entries.is_empty() && self.flags().iter().all(Option::is_none)
    }

    pub fn counts(&self) -> QueueCounts {
        self.counts
    }

    pub fn is_dead(&self) -> bool {
        self.dead
    }

    /// Queues `e` by the rules in the module's doc.
    pub fn push(&mut self, e: E) -> Pushed {
        if self.dead {
            self.counts.dead_pushes += 1;
            return Pushed::Dead;
        }
        let len = self.entries.len();
        match e.class() {
            Class::Move => {
                let tail_is_move = self.entries.back().is_some_and(|b| b.class() == Class::Move);
                if tail_is_move && !self.flag_after_tail() {
                    *self.entries.back_mut().expect("a tail") = e;
                    self.counts.merged += 1;
                    Pushed::Merged
                } else if len < RATIONED {
                    self.entries.push_back(e);
                    Pushed::Queued
                } else {
                    self.counts.rationed += 1;
                    self.dropped()
                }
            }
            Class::Press(k) => {
                if len < RATIONED {
                    self.entries.push_back(e);
                    add(&mut self.pressed, k);
                    Pushed::Queued
                } else {
                    // Its release will be a stray, an expected one.
                    add(&mut self.forgotten, k);
                    self.counts.rationed += 1;
                    self.dropped()
                }
            }
            Class::Request => {
                if len < RATIONED {
                    self.entries.push_back(e);
                    Pushed::Queued
                } else {
                    self.counts.busy += 1;
                    Pushed::Busy
                }
            }
            Class::Release(k) => {
                if !remove(&mut self.pressed, k) {
                    if remove(&mut self.forgotten, k) {
                        self.counts.stray_forgotten += 1;
                    } else {
                        self.counts.stray_unpressed += 1;
                    }
                    return Pushed::Stray;
                }
                if len < CAPACITY {
                    self.entries.push_back(e);
                    Pushed::Queued
                } else {
                    self.set_lost();
                    self.counts.overflowed += 1;
                    self.dropped()
                }
            }
        }
    }

    /// Events for the window were lost elsewhere (a gap in a ring): the
    /// owner is told after what is queued now.
    pub fn set_lost(&mut self) {
        if self.dead {
            self.counts.dead_pushes += 1;
            return;
        }
        self.lost = Some(self.entries.len());
        self.forget_presses();
    }

    /// The window gained or lost the focus, delivered after what is queued
    /// now. A loss drops a gain not yet delivered; a gain keeps a loss.
    pub fn set_focus(&mut self, gained: bool) {
        if self.dead {
            self.counts.dead_pushes += 1;
            return;
        }
        let at = Some(self.entries.len());
        if gained {
            self.focus_gained = at;
        } else {
            self.focus_lost = at;
            self.focus_gained = None;
            self.forget_presses();
        }
    }

    /// The window was asked to close.
    pub fn request_close(&mut self) {
        if self.dead {
            self.counts.dead_pushes += 1;
            return;
        }
        self.close = Some(self.entries.len());
    }

    /// The window was destroyed: nothing is queued from now on.
    pub fn kill(&mut self) {
        self.dead = true;
    }

    /// The next thing to deliver: a flag that is due, else the oldest
    /// entry.
    pub fn next_event(&mut self) -> Option<Next<E>> {
        if self.lost == Some(0) {
            self.lost = None;
            return Some(Next::Lost);
        }
        if self.focus_lost == Some(0) {
            self.focus_lost = None;
            return Some(Next::Focus(false));
        }
        if self.focus_gained == Some(0) {
            self.focus_gained = None;
            return Some(Next::Focus(true));
        }
        if self.close == Some(0) {
            self.close = None;
            return Some(Next::Close);
        }
        let e = self.entries.pop_front()?;
        for at in [&mut self.lost, &mut self.focus_lost, &mut self.focus_gained, &mut self.close].into_iter().flatten()
        {
            *at = at.saturating_sub(1);
        }
        if self.entries.is_empty() {
            self.behind = false;
        }
        Some(Next::Event(e))
    }

    fn flags(&self) -> [Option<usize>; 4] {
        [self.lost, self.focus_lost, self.focus_gained, self.close]
    }

    /// A flag is due after the last entry, so nothing may merge into it.
    fn flag_after_tail(&self) -> bool {
        self.flags().contains(&Some(self.entries.len()))
    }

    /// The owner treats every key and button as released: their releases
    /// will be expected strays.
    fn forget_presses(&mut self) {
        for k in self.pressed.drain(..) {
            add(&mut self.forgotten, k);
        }
    }

    fn dropped(&mut self) -> Pushed {
        let first_in_burst = !self.behind;
        self.behind = true;
        Pushed::Dropped { first_in_burst }
    }
}

fn add<K: PartialEq>(set: &mut Vec<K>, k: K) {
    if !set.contains(&k) {
        set.push(k);
    }
}

/// Removes `k`; whether it was there.
fn remove<K: PartialEq>(set: &mut Vec<K>, k: K) -> bool {
    match set.iter().position(|x| *x == k) {
        Some(at) => {
            set.swap_remove(at);
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in for #153's `WindowEvent`.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Ev {
        Down(u8),
        Up(u8),
        Move(i32),
        Ask(u32),
    }

    impl QueueEvent for Ev {
        type Key = u8;
        fn class(&self) -> Class<u8> {
            match *self {
                Ev::Down(k) => Class::Press(k),
                Ev::Up(k) => Class::Release(k),
                Ev::Move(_) => Class::Move,
                Ev::Ask(_) => Class::Request,
            }
        }
    }

    fn drain(q: &mut EventQueue<Ev>) -> Vec<Next<Ev>> {
        core::iter::from_fn(|| q.next_event()).collect()
    }

    /// 48 presses (keys 0 to 47), then releases of 1 to 16: full at 64.
    fn full() -> EventQueue<Ev> {
        let mut q = EventQueue::new();
        for k in 0..RATIONED as u8 {
            assert_eq!(q.push(Ev::Down(k)), Pushed::Queued);
        }
        for k in 1..=(CAPACITY - RATIONED) as u8 {
            assert_eq!(q.push(Ev::Up(k)), Pushed::Queued);
        }
        assert_eq!(q.len(), CAPACITY);
        q
    }

    #[test]
    fn the_sizes_are_48_of_64() {
        assert_eq!((RATIONED, CAPACITY), (48, 64));
    }

    #[test]
    fn moves_merge_at_the_tail_and_nowhere_else() {
        let mut q = EventQueue::new();
        assert_eq!(q.push(Ev::Move(1)), Pushed::Queued);
        assert_eq!(q.push(Ev::Move(2)), Pushed::Merged);
        assert_eq!(q.push(Ev::Down(0)), Pushed::Queued);
        assert_eq!(q.push(Ev::Move(3)), Pushed::Queued);
        assert_eq!(q.push(Ev::Move(4)), Pushed::Merged);
        assert_eq!(drain(&mut q), [Next::Event(Ev::Move(2)), Next::Event(Ev::Down(0)), Next::Event(Ev::Move(4))]);
        assert_eq!(q.counts().merged, 2);
    }

    #[test]
    fn presses_moves_and_requests_fit_until_48_are_queued() {
        let mut q = EventQueue::new();
        for i in 0..RATIONED as u32 - 1 {
            q.push(Ev::Ask(i));
        }
        // At 47 a move, then (at 48) a press, a move and a request don't.
        assert_eq!(q.push(Ev::Move(1)), Pushed::Queued);
        assert_eq!(q.push(Ev::Down(1)), Pushed::Dropped { first_in_burst: true });
        assert_eq!(q.push(Ev::Ask(1)), Pushed::Busy);
        // A move at the tail still merges at 48.
        assert_eq!(q.push(Ev::Move(2)), Pushed::Merged);
        let mut q = EventQueue::new();
        for i in 0..RATIONED as u32 - 1 {
            q.push(Ev::Ask(i));
        }
        assert_eq!(q.push(Ev::Down(1)), Pushed::Queued);
        assert_eq!(q.push(Ev::Move(1)), Pushed::Dropped { first_in_burst: true });
    }

    #[test]
    fn a_full_queue_drops_presses_but_keeps_releases() {
        let mut q = EventQueue::new();
        for k in 0..8 {
            assert_eq!(q.push(Ev::Down(k)), Pushed::Queued);
        }
        for i in 8..RATIONED as u32 {
            assert_eq!(q.push(Ev::Ask(i)), Pushed::Queued);
        }
        assert_eq!(q.len(), RATIONED);
        // Past the threshold: a press and a move are dropped, the first
        // drop of the burst said so.
        assert_eq!(q.push(Ev::Down(9)), Pushed::Dropped { first_in_burst: true });
        assert_eq!(q.push(Ev::Move(1)), Pushed::Dropped { first_in_burst: false });
        // Every release of a press taken still fits.
        for k in 0..8 {
            assert_eq!(q.push(Ev::Up(k)), Pushed::Queued);
        }
        // The dropped press's release is an expected stray; a release of
        // a key never pressed is the other kind.
        assert_eq!(q.push(Ev::Up(9)), Pushed::Stray);
        assert_eq!(q.push(Ev::Up(200)), Pushed::Stray);
        let c = q.counts();
        assert_eq!((c.rationed, c.overflowed, c.lost(), c.stray_forgotten, c.stray_unpressed), (2, 0, 2, 1, 1));
        let got = drain(&mut q);
        assert_eq!(got.len(), RATIONED + 8);
        assert!(got.ends_with(&(0..8).map(|k| Next::Event(Ev::Up(k))).collect::<Vec<_>>()));
        // Emptied: the next drop is a new burst.
        for i in 0..RATIONED as u32 {
            q.push(Ev::Ask(i));
        }
        assert_eq!(q.push(Ev::Down(1)), Pushed::Dropped { first_in_burst: true });
    }

    #[test]
    fn a_release_that_doesnt_fit_sets_lost() {
        let mut q = full();
        // Key 0's press was taken; its release finds no slot.
        assert_eq!(q.push(Ev::Up(0)), Pushed::Dropped { first_in_burst: true });
        let c = q.counts();
        assert_eq!((c.overflowed, c.rationed, c.stray_unpressed), (1, 0, 0));
        // Lost forgot the other presses: their releases are expected strays.
        assert_eq!(q.push(Ev::Up(30)), Pushed::Stray);
        assert_eq!(q.counts().stray_forgotten, 1);
        let got = drain(&mut q);
        assert_eq!(got.len(), CAPACITY + 1);
        assert_eq!(got.last(), Some(&Next::Lost));
    }

    #[test]
    fn a_request_past_the_threshold_is_refused_as_busy() {
        let mut q = EventQueue::new();
        for i in 0..RATIONED as u32 {
            assert_eq!(q.push(Ev::Ask(i)), Pushed::Queued);
        }
        assert_eq!(q.push(Ev::Ask(99)), Pushed::Busy);
        assert_eq!((q.counts().busy, q.counts().lost()), (1, 0));
        q.next_event();
        assert_eq!(q.push(Ev::Ask(100)), Pushed::Queued);
    }

    #[test]
    fn lost_close_and_focus_are_never_dropped() {
        let mut q = full();
        q.set_focus(false);
        q.request_close();
        q.set_lost();
        assert_eq!(q.push(Ev::Up(0)), Pushed::Stray, "lost forgot key 0's press");
        let got = drain(&mut q);
        assert_eq!(&got[CAPACITY..], [Next::Lost, Next::Focus(false), Next::Close]);
        assert!(q.is_idle());
    }

    #[test]
    fn flags_due_together_come_lost_loss_gain_close() {
        let mut q = EventQueue::new();
        q.push(Ev::Move(1));
        q.request_close();
        q.set_focus(true);
        assert!(!q.is_empty() && !q.is_idle());
        q.next_event();
        assert!(q.is_empty() && !q.is_idle(), "flags pending: empty but not idle");
        assert_eq!(drain(&mut q), [Next::Focus(true), Next::Close]);
        q.set_focus(false);
        q.set_focus(true);
        q.request_close();
        q.set_lost();
        assert_eq!(drain(&mut q), [Next::Lost, Next::Focus(false), Next::Focus(true), Next::Close]);
    }

    #[test]
    fn a_flag_comes_after_what_was_queued_before_it() {
        let mut q = EventQueue::new();
        q.push(Ev::Down(1));
        q.set_focus(false);
        q.push(Ev::Down(2));
        // The release of 1 is a stray (focus lost forgot it); 2's isn't.
        assert_eq!(q.push(Ev::Up(1)), Pushed::Stray);
        assert_eq!(q.push(Ev::Up(2)), Pushed::Queued);
        assert_eq!(
            drain(&mut q),
            [Next::Event(Ev::Down(1)), Next::Focus(false), Next::Event(Ev::Down(2)), Next::Event(Ev::Up(2))]
        );
        assert_eq!((q.counts().stray_forgotten, q.counts().stray_unpressed), (1, 0));
    }

    #[test]
    fn a_focus_loss_is_never_folded_into_a_gain() {
        let mut q = EventQueue::new();
        q.push(Ev::Down(1));
        q.set_focus(false); // a pause
        q.set_focus(true); // resumed before the owner read anything
        assert_eq!(q.push(Ev::Up(1)), Pushed::Stray);
        assert_eq!(drain(&mut q), [Next::Event(Ev::Down(1)), Next::Focus(false), Next::Focus(true)]);
        // A gain then a loss: only the loss.
        q.set_focus(true);
        q.set_focus(false);
        assert_eq!(drain(&mut q), [Next::Focus(false)]);
    }

    #[test]
    fn a_flag_set_again_moves_to_the_end_and_a_loss_drops_a_pending_gain() {
        let mut q = EventQueue::new();
        q.set_lost();
        q.set_focus(true);
        q.push(Ev::Move(1));
        q.set_lost();
        q.set_focus(false);
        // A move after a due flag isn't merged into the move before it.
        assert_eq!(q.push(Ev::Move(2)), Pushed::Queued);
        assert_eq!(drain(&mut q), [Next::Event(Ev::Move(1)), Next::Lost, Next::Focus(false), Next::Event(Ev::Move(2))]);
    }

    #[test]
    fn a_dead_queue_counts_pushes_and_flags_and_takes_none() {
        let mut q = EventQueue::new();
        q.push(Ev::Down(1));
        q.kill();
        assert!(q.is_dead());
        assert_eq!(q.push(Ev::Up(1)), Pushed::Dead);
        assert_eq!(q.push(Ev::Ask(1)), Pushed::Dead);
        q.set_lost();
        q.set_focus(false);
        q.request_close();
        assert_eq!(q.counts().dead_pushes, 5);
        assert_eq!(drain(&mut q), [Next::Event(Ev::Down(1))]);
    }
}
