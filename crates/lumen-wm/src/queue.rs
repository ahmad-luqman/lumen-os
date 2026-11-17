//! A window's event queue (#153 keeps one per window, behind its lock).
//!
//! **Entries and flags.** The queue holds [`Entry`]s: key events (presses
//! and releases), pointer events ([`PointerEntry`]) and automation
//! requests. `Lost`, `Close`, a focus loss and a focus gain are not entries:
//! they are flags beside the queue, set with [`EventQueue::set_lost`],
//! [`EventQueue::request_close`] and [`EventQueue::set_focus`], so they
//! need no slot and can never be dropped ([`Entry`] has no variant for one,
//! so one can't be pushed).
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
//!   key's or button's press. Any other release is ignored and counted: a
//!   key's as `key_releases_unpaired` (expected: keys go to the window
//!   focused when they arrive, so Shift held across a click, or the Enter
//!   of `desktop` itself, is released in a window that never saw it go
//!   down); a button's as `stray_forgotten` if its press was forgotten (by
//!   `FocusLost` or `Lost`) or dropped, else as `stray_unpressed` (never
//!   expected: the window manager only delivers a button's release after
//!   its press). A release of a press taken that doesn't fit sets `Lost`
//!   (counted in `overflowed`).
//!
//! The first drop since the queue last emptied is [`Pushed::Dropped`] with
//! `first_in_burst` set, for #153's one `WARN desktop: window <id> is
//! behind: dropping events` per burst. After the window is destroyed the
//! queue is dead ([`EventQueue::kill`]): pushes and flags are counted
//! (`dead_pushes`), never queued.

use alloc::collections::VecDeque;
use alloc::vec::Vec;

use crate::route::PointerEntry;
use lumen_input::Button;

/// Entries the queue holds.
pub const CAPACITY: usize = 64;
/// Presses, moves and requests are queued only while fewer entries than
/// this are.
pub const RATIONED: usize = 48;

/// A key event as the queue needs to see it: implemented for the desktop's
/// key event, `lumen_input::KeyEvent` (#151), below.
pub trait KeyLike {
    /// Names a key, to pair a release with its press.
    type Code: Copy + PartialEq;
    fn code(&self) -> Self::Code;
    /// A press (a typematic repeat is a press too), not a release.
    fn is_press(&self) -> bool;
}

/// The desktop's key event (#151): named by its physical key, a press
/// (typematic repeats included) or a release.
impl KeyLike for lumen_input::KeyEvent {
    type Code = lumen_input::KeyCode;
    fn code(&self) -> lumen_input::KeyCode {
        self.code
    }
    fn is_press(&self) -> bool {
        matches!(self.action, lumen_input::KeyAction::Press { .. })
    }
}

/// One entry of a window's queue.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Entry<K, R> {
    Key(K),
    Pointer(PointerEntry),
    /// An automation request (#156).
    Request(R),
}

/// What a press or release is of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Held<C> {
    Code(C),
    Button(Button),
}

/// What kind of entry, for the queue's rules.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Class<C> {
    Press(Held<C>),
    Release(Held<C>),
    Move,
    Request,
}

impl<K: KeyLike, R> Entry<K, R> {
    fn class(&self) -> Class<K::Code> {
        match self {
            Entry::Key(k) if k.is_press() => Class::Press(Held::Code(k.code())),
            Entry::Key(k) => Class::Release(Held::Code(k.code())),
            Entry::Pointer(PointerEntry::Down { button, .. }) => Class::Press(Held::Button(*button)),
            Entry::Pointer(PointerEntry::Up { button, .. }) => Class::Release(Held::Button(*button)),
            Entry::Pointer(PointerEntry::Move { .. }) => Class::Move,
            Entry::Request(_) => Class::Request,
        }
    }
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
    /// Button releases whose press was forgotten (`FocusLost`, `Lost`) or
    /// dropped: expected after a reset or a drop.
    pub stray_forgotten: u64,
    /// Button releases of a button never pressed: never expected.
    pub stray_unpressed: u64,
    /// Key releases whose press the queue didn't take: expected (keys go
    /// to the window focused when they arrive).
    pub key_releases_unpaired: u64,
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

/// Field by field: a closed window's counts added to the totals (`desktop
/// status`, #153).
impl core::ops::AddAssign<&QueueCounts> for QueueCounts {
    fn add_assign(&mut self, other: &QueueCounts) {
        let QueueCounts {
            rationed,
            overflowed,
            busy,
            stray_forgotten,
            stray_unpressed,
            key_releases_unpaired,
            dead_pushes,
            merged,
        } = *other;
        self.rationed += rationed;
        self.overflowed += overflowed;
        self.busy += busy;
        self.stray_forgotten += stray_forgotten;
        self.stray_unpressed += stray_unpressed;
        self.key_releases_unpaired += key_releases_unpaired;
        self.dead_pushes += dead_pushes;
        self.merged += merged;
    }
}

/// A window's event queue: see the module's doc.
#[derive(Clone, Debug)]
pub struct EventQueue<K: KeyLike, R> {
    entries: VecDeque<Entry<K, R>>,
    /// Keys and buttons whose press was queued and whose release wasn't.
    pressed: Vec<Held<K::Code>>,
    /// Buttons whose press was forgotten or dropped and whose release
    /// hasn't come (none of them in `pressed`).
    forgotten: Vec<Button>,
    /// Each flag set, with how many entries come before it.
    lost: Option<usize>,
    focus_lost: Option<usize>,
    focus_gained: Option<usize>,
    close: Option<usize>,
    dead: bool,
    behind: bool,
    counts: QueueCounts,
}

impl<K: KeyLike, R> Default for EventQueue<K, R> {
    fn default() -> Self {
        Self::new()
    }
}

impl<K: KeyLike, R> EventQueue<K, R> {
    /// An empty queue. Its [`CAPACITY`] entries are allocated here, once,
    /// with room for 8 keys or buttons held at once (more grow it).
    pub fn new() -> Self {
        EventQueue {
            entries: VecDeque::with_capacity(CAPACITY),
            pressed: Vec::with_capacity(8),
            forgotten: Vec::with_capacity(5),
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
    pub fn push(&mut self, e: Entry<K, R>) -> Pushed {
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
            Class::Press(h) => {
                if len < RATIONED {
                    self.entries.push_back(e);
                    add(&mut self.pressed, h);
                    if let Held::Button(b) = h {
                        remove(&mut self.forgotten, b);
                    }
                    Pushed::Queued
                } else {
                    // Its release will be an expected stray, unless the
                    // owner holds it already (a key's typematic repeat).
                    if let Held::Button(b) = h
                        && !self.pressed.contains(&h)
                    {
                        add(&mut self.forgotten, b);
                    }
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
            Class::Release(h) => {
                if !remove(&mut self.pressed, h) {
                    match h {
                        Held::Code(_) => self.counts.key_releases_unpaired += 1,
                        Held::Button(b) if remove(&mut self.forgotten, b) => self.counts.stray_forgotten += 1,
                        Held::Button(_) => self.counts.stray_unpressed += 1,
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
    pub fn next_event(&mut self) -> Option<Next<Entry<K, R>>> {
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

    /// The owner treats every key and button as released: a button's
    /// release will be an expected stray (a key's is counted apart anyway).
    fn forget_presses(&mut self) {
        for h in self.pressed.drain(..) {
            if let Held::Button(b) = h {
                add(&mut self.forgotten, b);
            }
        }
    }

    fn dropped(&mut self) -> Pushed {
        let first_in_burst = !self.behind;
        self.behind = true;
        Pushed::Dropped { first_in_burst }
    }
}

fn add<T: PartialEq>(set: &mut Vec<T>, x: T) {
    if !set.contains(&x) {
        set.push(x);
    }
}

/// Removes `x`; whether it was there.
fn remove<T: PartialEq>(set: &mut Vec<T>, x: T) -> bool {
    match set.iter().position(|y| *y == x) {
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
    use lumen_input::Buttons;

    /// A key event: its code, and whether it is a press.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    struct Key(u8, bool);

    impl KeyLike for Key {
        type Code = u8;
        fn code(&self) -> u8 {
            self.0
        }
        fn is_press(&self) -> bool {
            self.1
        }
    }

    type Q = EventQueue<Key, u32>;
    type E = Entry<Key, u32>;

    fn down(k: u8) -> E {
        Entry::Key(Key(k, true))
    }
    fn up(k: u8) -> E {
        Entry::Key(Key(k, false))
    }
    fn mv(x: i32) -> E {
        Entry::Pointer(PointerEntry::Move { x, y: 0, buttons: Buttons::NONE })
    }
    fn ask(i: u32) -> E {
        Entry::Request(i)
    }
    fn bdown(b: Button) -> E {
        Entry::Pointer(PointerEntry::Down { x: 0, y: 0, button: b, buttons: b.into() })
    }
    fn bup(b: Button) -> E {
        Entry::Pointer(PointerEntry::Up { x: 0, y: 0, button: b, buttons: Buttons::NONE })
    }

    fn drain(q: &mut Q) -> Vec<Next<E>> {
        core::iter::from_fn(|| q.next_event()).collect()
    }

    /// 48 presses (keys 0 to 47), then releases of 1 to 16: full at 64.
    fn full() -> Q {
        let mut q = Q::new();
        for k in 0..RATIONED as u8 {
            assert_eq!(q.push(down(k)), Pushed::Queued);
        }
        for k in 1..=(CAPACITY - RATIONED) as u8 {
            assert_eq!(q.push(up(k)), Pushed::Queued);
        }
        assert_eq!(q.len(), CAPACITY);
        q
    }

    /// Filled with requests to just below the threshold.
    fn rationed() -> Q {
        let mut q = Q::new();
        for i in 0..RATIONED as u32 {
            assert_eq!(q.push(ask(i)), Pushed::Queued);
        }
        q
    }

    #[test]
    fn the_sizes_are_48_of_64() {
        assert_eq!((RATIONED, CAPACITY), (48, 64));
    }

    #[test]
    fn moves_merge_at_the_tail_and_nowhere_else() {
        let mut q = Q::new();
        assert_eq!(q.push(mv(1)), Pushed::Queued);
        assert_eq!(q.push(mv(2)), Pushed::Merged);
        assert_eq!(q.push(down(0)), Pushed::Queued);
        assert_eq!(q.push(mv(3)), Pushed::Queued);
        assert_eq!(q.push(mv(4)), Pushed::Merged);
        assert_eq!(drain(&mut q), [Next::Event(mv(2)), Next::Event(down(0)), Next::Event(mv(4))]);
        assert_eq!(q.counts().merged, 2);
    }

    #[test]
    fn presses_moves_and_requests_fit_until_48_are_queued() {
        let mut q = Q::new();
        for i in 0..RATIONED as u32 - 1 {
            q.push(ask(i));
        }
        // At 47 a move, then (at 48) a press, a move and a request don't.
        assert_eq!(q.push(mv(1)), Pushed::Queued);
        assert_eq!(q.push(down(1)), Pushed::Dropped { first_in_burst: true });
        assert_eq!(q.push(ask(1)), Pushed::Busy);
        // A move at the tail still merges at 48.
        assert_eq!(q.push(mv(2)), Pushed::Merged);
        let mut q = Q::new();
        for i in 0..RATIONED as u32 - 1 {
            q.push(ask(i));
        }
        assert_eq!(q.push(down(1)), Pushed::Queued);
        assert_eq!(q.push(mv(1)), Pushed::Dropped { first_in_burst: true });
    }

    #[test]
    fn a_full_queue_drops_presses_but_keeps_releases() {
        let mut q = Q::new();
        for k in 0..8 {
            assert_eq!(q.push(down(k)), Pushed::Queued);
        }
        assert_eq!(q.push(bdown(Button::Left)), Pushed::Queued);
        for i in 9..RATIONED as u32 {
            assert_eq!(q.push(ask(i)), Pushed::Queued);
        }
        assert_eq!(q.len(), RATIONED);
        // Past the threshold: a press and a move are dropped, the first
        // drop of the burst said so.
        assert_eq!(q.push(bdown(Button::Right)), Pushed::Dropped { first_in_burst: true });
        assert_eq!(q.push(mv(1)), Pushed::Dropped { first_in_burst: false });
        // Every release of a press taken still fits.
        for k in 0..8 {
            assert_eq!(q.push(up(k)), Pushed::Queued);
        }
        assert_eq!(q.push(bup(Button::Left)), Pushed::Queued);
        // The dropped button's release is an expected stray; a release of
        // a button never pressed is the other kind; a key's is counted
        // apart.
        assert_eq!(q.push(bup(Button::Right)), Pushed::Stray);
        assert_eq!(q.push(bup(Button::Middle)), Pushed::Stray);
        assert_eq!(q.push(up(200)), Pushed::Stray);
        let c = q.counts();
        assert_eq!(
            (c.rationed, c.overflowed, c.lost(), c.stray_forgotten, c.stray_unpressed, c.key_releases_unpaired),
            (2, 0, 2, 1, 1, 1)
        );
        let got = drain(&mut q);
        assert_eq!(got.len(), RATIONED + 9);
        assert_eq!(got.last(), Some(&Next::Event(bup(Button::Left))));
        // Emptied: the next drop is a new burst.
        for i in 0..RATIONED as u32 {
            q.push(ask(i));
        }
        assert_eq!(q.push(down(1)), Pushed::Dropped { first_in_burst: true });
    }

    #[test]
    fn a_release_that_doesnt_fit_sets_lost() {
        let mut q = full();
        // Key 0's press was taken; its release finds no slot.
        assert_eq!(q.push(up(0)), Pushed::Dropped { first_in_burst: true });
        let c = q.counts();
        assert_eq!((c.overflowed, c.rationed, c.lost(), c.key_releases_unpaired), (1, 0, 1, 0));
        // Lost forgot the other presses: their releases are unpaired now.
        assert_eq!(q.push(up(30)), Pushed::Stray);
        assert_eq!(q.counts().key_releases_unpaired, 1);
        let got = drain(&mut q);
        assert_eq!(got.len(), CAPACITY + 1);
        assert_eq!(got.last(), Some(&Next::Lost));
    }

    #[test]
    fn forgotten_buttons_dont_go_stale() {
        // A press dropped while the button is already pressed (a repeat)
        // isn't forgotten: its later release pairs with the press taken,
        // and another release is a release of nothing pressed.
        let mut q = Q::new();
        q.push(bdown(Button::Left));
        for i in 1..RATIONED as u32 {
            q.push(ask(i));
        }
        assert_eq!(q.push(bdown(Button::Left)), Pushed::Dropped { first_in_burst: true });
        assert_eq!(q.push(bup(Button::Left)), Pushed::Queued);
        assert_eq!(q.push(bup(Button::Left)), Pushed::Stray);
        assert_eq!((q.counts().stray_forgotten, q.counts().stray_unpressed), (0, 1));
        // A press dropped, then the same button's press taken: the taken
        // one clears the forgotten mark.
        let mut q = rationed();
        assert_eq!(q.push(bdown(Button::Right)), Pushed::Dropped { first_in_burst: true });
        drain(&mut q);
        assert_eq!(q.push(bdown(Button::Right)), Pushed::Queued);
        assert_eq!(q.push(bup(Button::Right)), Pushed::Queued);
        assert_eq!(q.push(bup(Button::Right)), Pushed::Stray);
        assert_eq!((q.counts().stray_forgotten, q.counts().stray_unpressed), (0, 1));
    }

    #[test]
    fn a_request_past_the_threshold_is_refused_as_busy() {
        let mut q = rationed();
        assert_eq!(q.push(ask(99)), Pushed::Busy);
        assert_eq!((q.counts().busy, q.counts().lost()), (1, 0));
        q.next_event();
        assert_eq!(q.push(ask(100)), Pushed::Queued);
    }

    #[test]
    fn lost_close_and_focus_are_never_dropped() {
        let mut q = full();
        q.set_focus(false);
        q.request_close();
        q.set_lost();
        assert_eq!(q.push(up(0)), Pushed::Stray, "lost forgot key 0's press");
        let got = drain(&mut q);
        assert_eq!(&got[CAPACITY..], [Next::Lost, Next::Focus(false), Next::Close]);
        assert!(q.is_idle());
    }

    #[test]
    fn flags_due_together_come_lost_loss_gain_close() {
        let mut q = Q::new();
        q.push(mv(1));
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
        let mut q = Q::new();
        q.push(bdown(Button::Left));
        q.set_focus(false);
        q.push(bdown(Button::Right));
        // Left's release is a stray (focus lost forgot it); right's isn't.
        assert_eq!(q.push(bup(Button::Left)), Pushed::Stray);
        assert_eq!(q.push(bup(Button::Right)), Pushed::Queued);
        assert_eq!(
            drain(&mut q),
            [
                Next::Event(bdown(Button::Left)),
                Next::Focus(false),
                Next::Event(bdown(Button::Right)),
                Next::Event(bup(Button::Right))
            ]
        );
        assert_eq!((q.counts().stray_forgotten, q.counts().stray_unpressed), (1, 0));
    }

    #[test]
    fn a_focus_loss_is_never_folded_into_a_gain() {
        let mut q = Q::new();
        q.push(down(1));
        q.set_focus(false); // a pause
        q.set_focus(true); // resumed before the owner read anything
        assert_eq!(q.push(up(1)), Pushed::Stray);
        assert_eq!(drain(&mut q), [Next::Event(down(1)), Next::Focus(false), Next::Focus(true)]);
        // A gain then a loss: only the loss.
        q.set_focus(true);
        q.set_focus(false);
        assert_eq!(drain(&mut q), [Next::Focus(false)]);
    }

    #[test]
    fn a_flag_set_again_moves_to_the_end_and_a_loss_drops_a_pending_gain() {
        let mut q = Q::new();
        q.set_lost();
        q.set_focus(true);
        q.push(mv(1));
        q.set_lost();
        q.set_focus(false);
        // A move after a due flag isn't merged into the move before it.
        assert_eq!(q.push(mv(2)), Pushed::Queued);
        assert_eq!(drain(&mut q), [Next::Event(mv(1)), Next::Lost, Next::Focus(false), Next::Event(mv(2))]);
    }

    #[test]
    fn a_dead_queue_counts_pushes_and_flags_and_takes_none() {
        let mut q = Q::new();
        q.push(down(1));
        q.kill();
        assert!(q.is_dead());
        assert_eq!(q.push(up(1)), Pushed::Dead);
        assert_eq!(q.push(ask(1)), Pushed::Dead);
        q.set_lost();
        q.set_focus(false);
        q.request_close();
        assert_eq!(q.counts().dead_pushes, 5);
        assert_eq!(drain(&mut q), [Next::Event(down(1))]);
    }

    #[test]
    fn the_desktop_s_key_event_is_a_key_like() {
        // `lumen_input::KeyEvent` as the queue sees it: its key names it, a
        // press is a press; a press and its release go through a queue of
        // them in order, and a release whose press was never queued is a
        // stray, counted.
        use lumen_input::{KeyAction, KeyCode, KeyEvent, Mods};
        let ev = |code, action| KeyEvent { code, action, mods: Mods::NONE, after_drop: false };
        let a_down = ev(KeyCode::A, KeyAction::Press { text: Some('a') });
        let a_up = ev(KeyCode::A, KeyAction::Release);
        assert_eq!((a_down.code(), a_down.is_press(), a_up.is_press()), (KeyCode::A, true, false));
        let mut q = EventQueue::<KeyEvent, ()>::new();
        assert_eq!(q.push(Entry::Key(a_down)), Pushed::Queued);
        assert_eq!(q.push(Entry::Key(a_up)), Pushed::Queued);
        assert_eq!(q.push(Entry::Key(ev(KeyCode::B, KeyAction::Release))), Pushed::Stray);
        let got: Vec<_> = core::iter::from_fn(|| q.next_event()).collect();
        assert_eq!(got, [Next::Event(Entry::Key(a_down)), Next::Event(Entry::Key(a_up))]);
        assert_eq!(q.counts().key_releases_unpaired, 1);
    }

    #[test]
    fn counts_add_field_by_field() {
        let a = QueueCounts {
            rationed: 1,
            overflowed: 2,
            busy: 3,
            stray_forgotten: 4,
            stray_unpressed: 5,
            key_releases_unpaired: 6,
            dead_pushes: 7,
            merged: 8,
        };
        let mut sum = a;
        sum += &a;
        assert_eq!(
            sum,
            QueueCounts {
                rationed: 2,
                overflowed: 4,
                busy: 6,
                stray_forgotten: 8,
                stray_unpressed: 10,
                key_releases_unpaired: 12,
                dead_pushes: 14,
                merged: 16,
            }
        );
        assert_eq!(sum.lost(), 6);
    }
}
