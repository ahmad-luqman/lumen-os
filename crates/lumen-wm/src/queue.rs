//! A window's event queue (#153 keeps one per window, behind its lock).
//!
//! **Sticky flags beside the queue.** `Lost`, `Close`, a focus loss and a
//! focus gain need no slot, so they are never dropped. Each is delivered
//! in its place: after the entries queued before it was set (a flag set
//! again moves to the end). Delivering it first, ahead of older entries,
//! would let an owner clear its held keys and buttons on `FocusLost` or
//! `Lost` and then see a press queued before it, whose release (a stray
//! now) never comes: a key stuck down. A loss is never folded away: a loss
//! then a gain (a pause, then resuming) delivers both, so the owner still
//! releases what it held; a gain then a loss delivers only the loss. When
//! several are due at once they come in the order `Lost`, focus lost,
//! focus gained, `Close`.
//!
//! **The queue:** [`CAPACITY`] entries.
//! - A move replaces a move at the tail (unless a flag is due after the
//!   tail).
//! - Presses, moves and requests are queued only while fewer than
//!   [`RATIONED`] entries are: past that a press or move is dropped
//!   (counted in `lost`) and a request refused ([`Pushed::Busy`]: `ui
//!   invoke` says "window <id> busy").
//! - A release is queued, in any free slot, only if the queue took that
//!   key's or button's press; any other release is ignored and counted
//!   (`stray_releases`). `FocusLost` and `Lost` forget the presses taken.
//! - Anything else that doesn't fit sets `Lost` (counted in `lost`).
//!
//! The first drop since the queue last emptied is [`Pushed::Dropped`] with
//! `first_in_burst` set, for #153's one `WARN desktop: window <id> is
//! behind: dropping events` per burst. After the window is destroyed the
//! queue is closed: pushes are counted (`dead_pushes`), never queued.

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
    /// Anything else.
    Other,
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
    /// Dropped (counted in `lost`); `first_in_burst` if it is the first
    /// since the queue last emptied.
    Dropped {
        first_in_burst: bool,
    },
    /// A request refused: too many entries queued.
    Busy,
    /// A release whose press the queue didn't take: ignored.
    Stray,
    /// The queue is closed (its window destroyed).
    Dead,
}

/// What the queue counted.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct QueueCounts {
    /// Events dropped (presses and moves past [`RATIONED`], anything else
    /// past [`CAPACITY`]).
    pub lost: u64,
    /// Requests refused as busy.
    pub busy: u64,
    pub stray_releases: u64,
    pub dead_pushes: u64,
    /// Moves merged into the move at the tail.
    pub merged: u64,
}

/// A window's event queue: see the module's doc.
#[derive(Clone, Debug)]
pub struct EventQueue<E: QueueEvent> {
    entries: VecDeque<E>,
    /// Keys and buttons whose press was queued and whose release wasn't.
    pressed: Vec<E::Key>,
    /// Each flag set, with how many entries come before it.
    lost: Option<usize>,
    focus_lost: Option<usize>,
    focus_gained: Option<usize>,
    close: Option<usize>,
    closed: bool,
    behind: bool,
    counts: QueueCounts,
}

impl<E: QueueEvent> Default for EventQueue<E> {
    fn default() -> Self {
        Self::new()
    }
}

impl<E: QueueEvent> EventQueue<E> {
    /// An empty queue. It allocates its [`CAPACITY`] entries here, once.
    pub fn new() -> Self {
        EventQueue {
            entries: VecDeque::with_capacity(CAPACITY),
            pressed: Vec::new(),
            lost: None,
            focus_lost: None,
            focus_gained: None,
            close: None,
            closed: false,
            behind: false,
            counts: QueueCounts::default(),
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

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

    pub fn is_closed(&self) -> bool {
        self.closed
    }

    /// Queues `e` by the rules in the module's doc.
    pub fn push(&mut self, e: E) -> Pushed {
        if self.closed {
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
                    self.drop_one()
                }
            }
            Class::Press(k) => {
                if len < RATIONED {
                    self.entries.push_back(e);
                    if !self.pressed.contains(&k) {
                        self.pressed.push(k);
                    }
                    Pushed::Queued
                } else {
                    self.drop_one()
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
                let Some(at) = self.pressed.iter().position(|p| *p == k) else {
                    self.counts.stray_releases += 1;
                    return Pushed::Stray;
                };
                self.pressed.swap_remove(at);
                if len < CAPACITY {
                    self.entries.push_back(e);
                    Pushed::Queued
                } else {
                    self.set_lost();
                    self.drop_one()
                }
            }
            Class::Other => {
                if len < CAPACITY {
                    self.entries.push_back(e);
                    Pushed::Queued
                } else {
                    self.set_lost();
                    self.drop_one()
                }
            }
        }
    }

    /// Events for the window were lost elsewhere (a gap in a ring): the
    /// owner is told after what is queued now.
    pub fn set_lost(&mut self) {
        self.lost = Some(self.entries.len());
        self.pressed.clear();
    }

    /// The window gained or lost the focus, delivered after what is queued
    /// now. A loss drops a gain not yet delivered; a gain keeps a loss.
    pub fn set_focus(&mut self, gained: bool) {
        let at = Some(self.entries.len());
        if gained {
            self.focus_gained = at;
        } else {
            self.focus_lost = at;
            self.focus_gained = None;
            self.pressed.clear();
        }
    }

    /// The window was asked to close.
    pub fn request_close(&mut self) {
        self.close = Some(self.entries.len());
    }

    /// The window was destroyed: nothing is queued from now on.
    pub fn close_queue(&mut self) {
        self.closed = true;
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

    fn drop_one(&mut self) -> Pushed {
        self.counts.lost += 1;
        let first_in_burst = !self.behind;
        self.behind = true;
        Pushed::Dropped { first_in_burst }
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
        Focus,
    }

    impl QueueEvent for Ev {
        type Key = u8;
        fn class(&self) -> Class<u8> {
            match *self {
                Ev::Down(k) => Class::Press(k),
                Ev::Up(k) => Class::Release(k),
                Ev::Move(_) => Class::Move,
                Ev::Ask(_) => Class::Request,
                Ev::Focus => Class::Other,
            }
        }
    }

    fn drain(q: &mut EventQueue<Ev>) -> Vec<Next<Ev>> {
        core::iter::from_fn(|| q.next_event()).collect()
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
    fn a_full_queue_drops_presses_but_keeps_releases() {
        let mut q = EventQueue::new();
        for k in 0..8 {
            assert_eq!(q.push(Ev::Down(k)), Pushed::Queued);
        }
        for i in 8..RATIONED as i32 {
            assert_eq!(q.push(Ev::Ask(i as u32)), Pushed::Queued);
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
        // The dropped press's release is a stray.
        assert_eq!(q.push(Ev::Up(9)), Pushed::Stray);
        let c = q.counts();
        assert_eq!((c.lost, c.stray_releases), (2, 1));
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
    fn a_request_past_the_threshold_is_refused_as_busy() {
        let mut q = EventQueue::new();
        for i in 0..RATIONED as u32 {
            assert_eq!(q.push(Ev::Ask(i)), Pushed::Queued);
        }
        assert_eq!(q.push(Ev::Ask(99)), Pushed::Busy);
        assert_eq!(q.counts().busy, 1);
        assert_eq!(q.counts().lost, 0);
        q.next_event();
        assert_eq!(q.push(Ev::Ask(100)), Pushed::Queued);
    }

    #[test]
    fn what_doesnt_fit_at_all_sets_lost() {
        let mut q = EventQueue::new();
        for k in 0..RATIONED as u8 {
            q.push(Ev::Down(k));
        }
        for _ in RATIONED..CAPACITY {
            assert_eq!(q.push(Ev::Focus), Pushed::Queued);
        }
        assert_eq!(q.len(), CAPACITY);
        // Full: another plain event, or a release of a press taken, sets
        // Lost; Lost forgets the presses, so the later releases are strays.
        assert_eq!(q.push(Ev::Focus), Pushed::Dropped { first_in_burst: true });
        assert_eq!(q.push(Ev::Up(0)), Pushed::Stray);
        let got = drain(&mut q);
        assert_eq!(got.len(), CAPACITY + 1);
        assert_eq!(got.last(), Some(&Next::Lost));
    }

    #[test]
    fn lost_close_and_focus_are_never_dropped() {
        let mut q = EventQueue::new();
        for k in 0..RATIONED as u8 {
            q.push(Ev::Down(k));
        }
        for _ in RATIONED..CAPACITY {
            q.push(Ev::Focus);
        }
        q.set_focus(false);
        q.request_close();
        q.set_lost();
        assert_eq!(q.push(Ev::Focus), Pushed::Dropped { first_in_burst: true });
        let got = drain(&mut q);
        assert_eq!(&got[CAPACITY..], [Next::Lost, Next::Focus(false), Next::Close]);
        assert!(q.is_idle());
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
    fn a_closed_queue_counts_pushes_and_takes_none() {
        let mut q = EventQueue::new();
        q.push(Ev::Down(1));
        q.close_queue();
        assert_eq!(q.push(Ev::Up(1)), Pushed::Dead);
        assert_eq!(q.push(Ev::Ask(1)), Pushed::Dead);
        assert_eq!(q.counts().dead_pushes, 2);
        assert_eq!(drain(&mut q), [Next::Event(Ev::Down(1))]);
    }
}
