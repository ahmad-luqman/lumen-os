//! An absolute pointer's evdev events into pointer states.
//!
//! virtio-input (virtio 1.2 §5.8) passes Linux's evdev events through: a
//! frame of `EV_ABS` (position), `EV_KEY` (buttons) and `EV_REL` (wheel)
//! events, ended by `EV_SYN`/`SYN_REPORT`. A [`Tablet`] applies them to a
//! pending state and publishes it at the report, so a move to (x, y) is
//! never seen half done (x moved, y not yet). `SYN_DROPPED` (the device's
//! buffer overflowed) discards its whole frame, up to the next
//! `SYN_REPORT`, as evdev says to: the state stays what the last whole
//! frame made it.
//!
//! A consumer that looks now and then (the kernel's tablet thread, every
//! few milliseconds) sees only the newest state, so a state also carries
//! what happened in between: each button's presses so far, the wheels'
//! clicks so far, and the frame's sequence number.

use core::fmt;

/// The evdev types and codes a tablet uses (Linux's
/// `include/uapi/linux/input-event-codes.h`).
pub mod evdev {
    pub const EV_SYN: u16 = 0x00;
    pub const EV_KEY: u16 = 0x01;
    pub const EV_REL: u16 = 0x02;
    pub const EV_ABS: u16 = 0x03;

    pub const SYN_REPORT: u16 = 0;
    pub const SYN_DROPPED: u16 = 3;

    pub const ABS_X: u16 = 0x00;
    pub const ABS_Y: u16 = 0x01;

    pub const REL_HWHEEL: u16 = 0x06;
    pub const REL_WHEEL: u16 = 0x08;

    pub const BTN_LEFT: u16 = 0x110;
    pub const BTN_RIGHT: u16 = 0x111;
    pub const BTN_MIDDLE: u16 = 0x112;
    pub const BTN_SIDE: u16 = 0x113;
    pub const BTN_EXTRA: u16 = 0x114;
}

use evdev::*;

/// One evdev event, as virtio-input's event queue delivers it
/// (`virtio_input_event`: little-endian `type`, `code`, `value`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Event {
    pub kind: u16,
    pub code: u16,
    /// Signed, as evdev's `value` is (a wheel's -1).
    pub value: i32,
}

impl Event {
    pub const fn new(kind: u16, code: u16, value: i32) -> Event {
        Event { kind, code, value }
    }
}

/// An absolute axis's range, from the device's `VIRTIO_INPUT_CFG_ABS_INFO`
/// (QEMU's tablet: 0 to 32767 on both).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AbsAxis {
    min: i32,
    max: i32,
}

/// Why an axis range can't be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AxisError {
    pub min: i32,
    pub max: i32,
}

impl fmt::Display for AxisError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "axis range {} to {}: the maximum must be above the minimum", self.min, self.max)
    }
}

impl AbsAxis {
    /// The axis from `min` to `max` (both reachable); refused unless
    /// `max > min`.
    pub fn new(min: i32, max: i32) -> Result<AbsAxis, AxisError> {
        if max > min { Ok(AbsAxis { min, max }) } else { Err(AxisError { min, max }) }
    }

    pub fn min(&self) -> i32 {
        self.min
    }

    pub fn max(&self) -> i32 {
        self.max
    }

    /// `value` (clamped to the axis) as a pixel on a screen `pixels` wide:
    /// the minimum is pixel 0, the maximum `pixels - 1`, rounded to the
    /// nearest (so a pixel a host turned into an axis value by rounding,
    /// `round(x * range / (pixels - 1))`, comes back exactly while the
    /// range is at least `pixels - 1`). 0 on a screen 0 or 1 pixel wide.
    pub fn scale(&self, value: i32, pixels: usize) -> usize {
        if pixels <= 1 {
            return 0;
        }
        let v = (i64::from(value.clamp(self.min, self.max)) - i64::from(self.min)) as u128;
        let range = (i64::from(self.max) - i64::from(self.min)) as u128;
        let last = (pixels - 1) as u128;
        // At most `last`: v <= range.
        ((v * last + range / 2) / range) as usize
    }
}

/// One pointer button: the one place the buttons' order is spelled (their
/// bits in [`Buttons`], their slots in [`PointerState::presses`], their
/// names and evdev codes all come from here).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Button {
    Left,
    Right,
    Middle,
    Side,
    Extra,
}

impl Button {
    pub const ALL: [Button; 5] = [Button::Left, Button::Right, Button::Middle, Button::Side, Button::Extra];

    /// Its slot in [`PointerState::presses`] (left first), and the place
    /// of its bit in [`Buttons`].
    pub const fn index(self) -> usize {
        match self {
            Button::Left => 0,
            Button::Right => 1,
            Button::Middle => 2,
            Button::Side => 3,
            Button::Extra => 4,
        }
    }

    /// The set holding this button alone.
    pub const fn bit(self) -> Buttons {
        Buttons(1 << self.index())
    }

    /// Its name, as `mouse` prints it.
    pub const fn name(self) -> &'static str {
        match self {
            Button::Left => "left",
            Button::Right => "right",
            Button::Middle => "middle",
            Button::Side => "side",
            Button::Extra => "extra",
        }
    }

    /// The button an `EV_KEY` code names, if it is a pointer's.
    pub const fn from_code(code: u16) -> Option<Button> {
        match code {
            BTN_LEFT => Some(Button::Left),
            BTN_RIGHT => Some(Button::Right),
            BTN_MIDDLE => Some(Button::Middle),
            BTN_SIDE => Some(Button::Side),
            BTN_EXTRA => Some(Button::Extra),
            _ => None,
        }
    }
}

impl From<Button> for Buttons {
    fn from(b: Button) -> Buttons {
        b.bit()
    }
}

/// The buttons held down: the tablet's state and the desktop's window
/// manager both use this set. It holds only [`Button`]s' bits (every way
/// to make one starts from a [`Button`]), so [`Buttons::each`] lists all of
/// it: lumen-wm's `Wm` keeps what is held through `each`.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Buttons(u8);

impl Buttons {
    pub const NONE: Buttons = Buttons(0);
    pub const LEFT: Buttons = Button::Left.bit();
    pub const RIGHT: Buttons = Button::Right.bit();
    pub const MIDDLE: Buttons = Button::Middle.bit();
    pub const SIDE: Buttons = Button::Side.bit();
    pub const EXTRA: Buttons = Button::Extra.bit();

    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    pub const fn contains(self, other: Buttons) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether `b` is held.
    pub fn has(self, b: Button) -> bool {
        self.contains(b.into())
    }

    pub const fn with(self, other: Buttons) -> Buttons {
        Buttons(self.0 | other.0)
    }

    pub const fn without(self, other: Buttons) -> Buttons {
        Buttons(self.0 & !other.0)
    }

    /// Each button in the set, left first.
    pub fn each(self) -> impl Iterator<Item = Button> {
        Button::ALL.into_iter().filter(move |b| self.has(*b))
    }

    fn set(&mut self, b: Buttons, down: bool) {
        *self = if down { self.with(b) } else { self.without(b) };
    }
}

/// The buttons' names, space-separated (`left right`), or `none`.
impl fmt::Display for Buttons {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.is_empty() {
            return f.write_str("none");
        }
        for (i, b) in self.each().enumerate() {
            if i > 0 {
                f.write_str(" ")?;
            }
            f.write_str(b.name())?;
        }
        Ok(())
    }
}

impl fmt::Debug for Buttons {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Buttons({self})")
    }
}

/// Where the pointer is and what is held, after a whole frame.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PointerState {
    /// The pixel it points at (from the top-left corner).
    pub x: usize,
    pub y: usize,
    pub buttons: Buttons,
    /// Wheel clicks so far (away from the user positive), and the
    /// horizontal wheel's (right positive).
    pub wheel: i64,
    pub hwheel: i64,
    /// Each button's presses so far, indexed by [`Button::index`] (left,
    /// right, middle, side, extra): a press and release between two looks
    /// shows here although [`PointerState::buttons`] has it up again.
    pub presses: [u64; 5],
    /// Whole frames so far, this one included (1 for the first).
    pub seq: u64,
}

impl PointerState {
    /// Every button's presses so far.
    pub fn total_presses(&self) -> u64 {
        self.presses.iter().sum()
    }

    /// Button `b`'s presses so far (all of `b`'s, if it names several).
    pub fn presses_of(&self, b: Buttons) -> u64 {
        b.each().map(|button| self.presses[button.index()]).sum()
    }
}

/// Whether an `EV_ABS` bitmap (virtio-input's `EV_BITS` for `EV_ABS`, bit
/// n for axis n) has both `ABS_X` and `ABS_Y`: what makes a tablet (an
/// empty one: the device has no absolute axes).
pub fn has_abs_xy(bits: &[u8]) -> bool {
    let xy = (1 << ABS_X) | (1 << ABS_Y);
    bits.first().is_some_and(|b| b & xy == xy)
}

/// The event types a device sends in its frames: `EV_KEY` (keys and
/// buttons), `EV_REL`, `EV_ABS`, `EV_MSC` and `EV_SW`. The rest of its
/// `EV_BITS` (LEDs, sound, autorepeat, force feedback) describe output to
/// the device.
pub const FRAME_TYPES: core::ops::RangeInclusive<u16> = 0x01..=0x05;

/// The most events one frame can hold on a device whose `EV_BITS`
/// bitmaps (virtio-input's configuration: for an event type, bit n for
/// code n) are `bits`, as (type, bitmap) pairs: one event per code of a
/// [`FRAME_TYPES`] type it can send, plus the `SYN_REPORT` ending the
/// frame. Not capped: compare it with the ring (see
/// [`may_have_lost_frames`]).
///
/// That holds while a frame carries each code at most once. QEMU's
/// virtio input device passes on every event its input layer queues
/// between two syncs (`virtio_input_handle_event`, one virtio event each;
/// hw/input/virtio-input-hid.c). QMP's `input-send-event` takes any list
/// of events before its one sync, so a list repeating a code can make a
/// bigger frame. QEMU's tablet advertises 8 buttons (`keymap_button`:
/// left, right, middle, gear up and down, side, extra, touch), `REL_WHEEL`,
/// `ABS_X` and `ABS_Y` (8.2 to 10.x with `wheel-axis` on, the default,
/// `virtio_tablet_config_v2`; 11.0 onward, which dropped the property,
/// `virtio_tablet_config`): frames of up to 12 events.
pub fn frame_bound<'a>(bits: impl IntoIterator<Item = (u16, &'a [u8])>) -> u64 {
    let codes = bits.into_iter().filter(|(ty, _)| FRAME_TYPES.contains(ty)).flat_map(|(_, b)| b);
    1 + codes.map(|b| u64::from(b.count_ones())).sum::<u64>()
}

/// Whether a batch that read `events` events from a ring of `ring` event
/// buffers may have missed frames of up to `frame` events
/// ([`frame_bound`]): once fewer than `frame` buffers were free, a whole
/// frame may not have fit, and QEMU drops such a frame without a
/// `SYN_DROPPED` (`virtio_input_send`, hw/input/virtio-input.c, tracing
/// `virtio_input_queue_full`).
///
/// This is the one statement of what counting such batches (the kernel's
/// overruns) guarantees. It can count a batch that lost nothing, and it is
/// never 0 after a dropped frame: the first batch to end after the drop
/// counts. That holds given that every `ring` buffer is posted (a buffer
/// lost to the ring makes it smaller) and no frame is bigger than `frame`
/// (a bigger one can be dropped with fewer buffers in use): when a frame
/// was dropped, at least `ring - frame + 1` buffers were filled or being
/// re-posted, and the first batch to end after the drop reads them all
/// (`tests::no_dropped_frame_goes_uncounted`; `the_preconditions_are_needed`
/// shows each precondition is needed).
///
/// An empty batch never counts. With a `frame` of `ring` or more every
/// non-empty batch counts, so overruns can't be told apart, and a frame
/// too big for the ring is dropped without leaving events to count.
pub fn may_have_lost_frames(events: u64, ring: u64, frame: u64) -> bool {
    events > 0 && events + frame > ring
}

/// An absolute pointer (virtio's tablet): its axes and the screen it
/// points on.
#[derive(Clone, Debug)]
pub struct Tablet {
    x: AbsAxis,
    y: AbsAxis,
    width: usize,
    height: usize,
    pending: PointerState,
    current: Option<PointerState>,
    dropping: bool,
    ignored: u64,
    first_ignored: Option<(u16, u16)>,
    dropped: u64,
}

impl Tablet {
    /// A tablet whose axes `x` and `y` span a `width` x `height` screen.
    /// It starts at (0, 0) with nothing held, unreported.
    pub fn new(x: AbsAxis, y: AbsAxis, width: usize, height: usize) -> Tablet {
        Tablet {
            x,
            y,
            width,
            height,
            pending: PointerState::default(),
            current: None,
            dropping: false,
            ignored: 0,
            first_ignored: None,
            dropped: 0,
        }
    }

    /// Apply one event; at a `SYN_REPORT`, the state the frame made (also
    /// when it changed nothing), except the report ending a frame a
    /// `SYN_DROPPED` discarded. Events a tablet doesn't use are counted
    /// ([`Tablet::ignored`]).
    pub fn feed(&mut self, e: Event) -> Option<PointerState> {
        if self.dropping {
            if (e.kind, e.code) == (EV_SYN, SYN_REPORT) {
                self.dropping = false;
                self.pending = self.current.unwrap_or_default();
            }
            return None;
        }
        match (e.kind, e.code) {
            (EV_SYN, SYN_REPORT) => {
                self.pending.seq += 1;
                self.current = Some(self.pending);
                return self.current;
            }
            (EV_SYN, SYN_DROPPED) => {
                self.dropping = true;
                self.dropped += 1;
            }
            (EV_ABS, ABS_X) => self.pending.x = self.x.scale(e.value, self.width),
            (EV_ABS, ABS_Y) => self.pending.y = self.y.scale(e.value, self.height),
            (EV_REL, REL_WHEEL) => self.pending.wheel += i64::from(e.value),
            (EV_REL, REL_HWHEEL) => self.pending.hwheel += i64::from(e.value),
            (EV_KEY, code) => match Button::from_code(code) {
                Some(button) => {
                    let b = button.bit();
                    // 0 released, 1 pressed, 2 held (autorepeat). A press
                    // counts even if the button seems held (its release was
                    // in a dropped frame); an autorepeat only if it doesn't.
                    let down = e.value != 0;
                    if e.value == 1 || (down && !self.pending.buttons.contains(b)) {
                        self.pending.presses[button.index()] += 1;
                    }
                    self.pending.buttons.set(b, down);
                }
                None => self.ignore(e),
            },
            _ => self.ignore(e),
        }
        None
    }

    fn ignore(&mut self, e: Event) {
        self.ignored += 1;
        self.first_ignored.get_or_insert((e.kind, e.code));
    }

    /// The first event it had no use for: its type and code.
    pub fn first_ignored(&self) -> Option<(u16, u16)> {
        self.first_ignored
    }

    /// Events it had no use for.
    pub fn ignored(&self) -> u64 {
        self.ignored
    }

    /// `SYN_DROPPED`s seen (each discarded its whole frame).
    pub fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// Marks the tablet's frames that may follow frames lost at the device
/// (`crate::PointerEvent::after_overrun`): every frame of a batch that counted as
/// an overrun ([`may_have_lost_frames`]) except its first, and the first
/// frame of the next batch that brings one (virtio re-posts each buffer as
/// it is popped, so a frame can be dropped while the batch is still being
/// read, i.e. anywhere after its first frame, or just after its last).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AfterOverrun {
    /// The next frame follows a possible loss: the last batch that brought
    /// frames counted as an overrun, or one since (bringing none) did.
    carry: bool,
}

impl AfterOverrun {
    pub const fn new() -> Self {
        Self { carry: false }
    }

    /// Whether each of a batch's `frames` follows a possible loss, in
    /// order, given whether the batch counted as an `overrun`; then
    /// remembers the batch for the next one.
    pub fn batch(&mut self, frames: usize, overrun: bool) -> impl Iterator<Item = bool> + use<> {
        let carry = self.carry;
        // A batch with no frame passes a loss on to the next frame.
        self.carry = overrun || (carry && frames == 0);
        (0..frames).map(move |i| if i == 0 { carry } else { overrun })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const QEMU: AbsAxis = AbsAxis { min: 0, max: 32767 };

    fn abs(code: u16, v: i32) -> Event {
        Event::new(EV_ABS, code, v)
    }
    fn key(code: u16, down: bool) -> Event {
        Event::new(EV_KEY, code, i32::from(down))
    }
    fn rel(code: u16, v: i32) -> Event {
        Event::new(EV_REL, code, v)
    }
    const SYN: Event = Event::new(EV_SYN, SYN_REPORT, 0);
    const DROPPED: Event = Event::new(EV_SYN, SYN_DROPPED, 0);

    fn feed_all(t: &mut Tablet, events: &[Event]) -> Vec<PointerState> {
        events.iter().filter_map(|&e| t.feed(e)).collect()
    }

    #[test]
    fn axes_need_a_range() {
        assert_eq!(AbsAxis::new(0, 0), Err(AxisError { min: 0, max: 0 }));
        assert_eq!(AbsAxis::new(5, -5), Err(AxisError { min: 5, max: -5 }));
        assert!(AbsAxis::new(-1, 0).is_ok());
        assert_eq!(
            AxisError { min: 3, max: 3 }.to_string(),
            "axis range 3 to 3: the maximum must be above the minimum"
        );
    }

    #[test]
    fn scaling_maps_the_ends_to_the_edges_and_clamps() {
        assert_eq!(QEMU.scale(0, 1280), 0);
        assert_eq!(QEMU.scale(32767, 1280), 1279);
        assert_eq!(QEMU.scale(16384, 1281), 640);
        assert_eq!(QEMU.scale(-7, 1280), 0);
        assert_eq!(QEMU.scale(i32::MAX, 1280), 1279);
        assert_eq!(QEMU.scale(100, 1), 0);
        assert_eq!(QEMU.scale(100, 0), 0);
        // A negative minimum and the widest range there is.
        let wide = AbsAxis::new(i32::MIN, i32::MAX).unwrap();
        assert_eq!(wide.scale(i32::MIN, 800), 0);
        assert_eq!(wide.scale(i32::MAX, 800), 799);
        assert_eq!(wide.scale(0, 801), 400);
        let centred = AbsAxis::new(-100, 100).unwrap();
        assert_eq!(centred.scale(0, 201), 100);
        assert_eq!(centred.scale(-100, 201), 0);
        // An axis with fewer steps than pixels.
        let coarse = AbsAxis::new(0, 3).unwrap();
        assert_eq!((0..=3).map(|v| coarse.scale(v, 10)).collect::<Vec<_>>(), [0, 3, 6, 9]);
    }

    #[test]
    fn every_pixel_the_harness_aims_at_comes_back() {
        // The harness turns a pixel into QEMU's 0-32767 by rounding
        // (`lumen_harness.qmp.abs_of`, which this mirrors: change them
        // together); the kernel must land on the same pixel, on every
        // screen up to 32768 pixels (where the axis has a step per pixel).
        for pixels in [2usize, 3, 640, 800, 1024, 1280, 1920, 2560, 4096, 32768] {
            let last = pixels as u64 - 1;
            for x in 0..pixels as u64 {
                let v = (x * 32767 + last / 2) / last;
                assert_eq!(QEMU.scale(v as i32, pixels), x as usize, "pixel {x} of {pixels}");
            }
        }
    }

    #[test]
    fn a_frame_is_published_at_its_report_only() {
        let mut t = Tablet::new(QEMU, QEMU, 1280, 800);
        assert_eq!(t.feed(abs(ABS_X, 32767)), None);
        assert_eq!(t.feed(abs(ABS_Y, 32767)), None, "not before the report");
        let s = t.feed(SYN).unwrap();
        assert_eq!((s.x, s.y, s.seq), (1279, 799, 1));
        // A frame moving x only keeps y; an empty frame reports the same
        // (each its own sequence number).
        assert_eq!(feed_all(&mut t, &[abs(ABS_X, 0), SYN]), [PointerState { x: 0, seq: 2, ..s }]);
        assert_eq!(feed_all(&mut t, &[SYN]), [PointerState { x: 0, seq: 3, ..s }]);
    }

    #[test]
    fn each_button_is_its_own_bit_in_the_named_order() {
        // `Button` is the one place the order is spelled: each must be the
        // `Buttons` constant of its name, the name `mouse` prints, and its
        // own `presses` slot when pressed alone (side and extra apart), and
        // the set operations must agree on overlapping sets.
        let named = [Buttons::LEFT, Buttons::RIGHT, Buttons::MIDDLE, Buttons::SIDE, Buttons::EXTRA];
        let codes = [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE, BTN_SIDE, BTN_EXTRA];
        let names = ["left", "right", "middle", "side", "extra"];
        for (i, b) in Button::ALL.into_iter().enumerate() {
            let set = named[i];
            assert_eq!((Buttons::from(b), b.index(), b.name()), (set, i, names[i]), "{b:?}");
            assert_eq!(Button::from_code(codes[i]), Some(b));
            assert!(set.has(b) && !set.is_empty(), "{b:?}");
            assert_eq!(set.each().collect::<Vec<_>>(), [b]);
            assert_eq!(Buttons::NONE.with(set).without(set), Buttons::NONE);
            let mut t = Tablet::new(QEMU, QEMU, 100, 100);
            let s = feed_all(&mut t, &[key(codes[i], true), SYN])[0];
            let mut alone = [0; 5];
            alone[i] = 1;
            assert_eq!((s.buttons, s.presses, s.buttons.to_string()), (set, alone, names[i].into()), "{b:?}");
        }
        assert_eq!(Button::from_code(BTN_EXTRA + 1), None);
        assert!(Buttons::NONE.is_empty() && Buttons::NONE.each().next().is_none());
        let lr = Buttons::LEFT.with(Buttons::RIGHT);
        assert_eq!(lr.each().collect::<Vec<_>>(), [Button::Left, Button::Right]);
        assert_eq!(lr.with(Buttons::LEFT), lr, "with a button already held");
        assert_eq!(lr.without(Buttons::LEFT), Buttons::RIGHT);
        assert_eq!(Buttons::LEFT.without(Buttons::RIGHT), Buttons::LEFT, "without a button not held");
        assert!(lr.contains(Buttons::RIGHT) && !lr.contains(Buttons::MIDDLE));
    }

    #[test]
    fn buttons_follow_press_and_release() {
        let mut t = Tablet::new(QEMU, QEMU, 100, 100);
        let states = feed_all(
            &mut t,
            &[
                key(BTN_LEFT, true),
                SYN,
                key(BTN_RIGHT, true),
                Event::new(EV_KEY, BTN_MIDDLE, 2),
                SYN,
                key(BTN_LEFT, false),
                SYN,
                key(BTN_RIGHT, false),
                key(BTN_MIDDLE, false),
                key(BTN_SIDE, true),
                key(BTN_EXTRA, true),
                SYN,
            ],
        );
        let b: Vec<String> = states.iter().map(|s| s.buttons.to_string()).collect();
        assert_eq!(b, ["left", "left right middle", "right middle", "side extra"]);
        assert_eq!(Buttons::NONE.to_string(), "none");
        assert_eq!(format!("{:?}", Buttons::LEFT), "Buttons(left)");
        // Presses count once each (an autorepeat 2 after a press isn't
        // another).
        assert_eq!(states[3].presses, [1, 1, 1, 1, 1]);
        assert_eq!(states[3].total_presses(), 5);
        assert_eq!(states[3].presses_of(Buttons::RIGHT), 1);
        assert_eq!(states[3].presses_of(Buttons::NONE), 0);
        assert_eq!(t.ignored(), 0);
    }

    #[test]
    fn a_click_between_two_looks_still_shows() {
        // Pressed and released in frames read in one batch: the newest
        // state has the button up, but its presses went up by one.
        let mut t = Tablet::new(QEMU, QEMU, 100, 100);
        let batch = feed_all(&mut t, &[key(BTN_LEFT, true), SYN, key(BTN_LEFT, false), SYN]);
        let newest = batch.last().copied().unwrap();
        assert_eq!(newest.buttons, Buttons::NONE);
        assert_eq!((newest.presses[0], newest.seq), (1, 2));
        // Even pressed and released inside one frame.
        let s = feed_all(&mut t, &[key(BTN_RIGHT, true), key(BTN_RIGHT, false), SYN])[0];
        assert_eq!((s.buttons, s.presses_of(Buttons::RIGHT), s.total_presses()), (Buttons::NONE, 1, 2));
        // Held with autorepeat, released: one press.
        let held = [key(BTN_LEFT, true), SYN, Event::new(EV_KEY, BTN_LEFT, 2), SYN, key(BTN_LEFT, false), SYN];
        assert_eq!(feed_all(&mut t, &held).last().unwrap().presses[0], 2);
    }

    #[test]
    fn wheels_count_clicks_both_ways() {
        let mut t = Tablet::new(QEMU, QEMU, 100, 100);
        let s = feed_all(
            &mut t,
            &[
                rel(REL_WHEEL, 1),
                rel(REL_WHEEL, 1),
                rel(REL_HWHEEL, 2),
                SYN,
                rel(REL_WHEEL, -3),
                rel(REL_HWHEEL, -3),
                SYN,
            ],
        );
        assert_eq!((s[0].wheel, s[0].hwheel), (2, 2));
        assert_eq!((s[1].wheel, s[1].hwheel), (-1, -1));
    }

    #[test]
    fn a_dropped_frame_is_discarded_up_to_its_report() {
        let mut t = Tablet::new(QEMU, QEMU, 1280, 800);
        let first = feed_all(&mut t, &[abs(ABS_X, 32767), key(BTN_LEFT, true), SYN])[0];
        // Half a frame, then the device's buffer overflowed: everything up
        // to the next report is discarded, and the state is the last whole
        // frame's (not the half frame's x).
        let after = feed_all(
            &mut t,
            &[abs(ABS_X, 0), DROPPED, abs(ABS_Y, 32767), key(BTN_LEFT, false), SYN, abs(ABS_Y, 16384), SYN],
        );
        assert_eq!(after, [PointerState { y: 400, seq: 2, ..first }]);
        assert_eq!(t.dropped(), 1);
        // Before any whole frame: back to the start.
        let mut t = Tablet::new(QEMU, QEMU, 1280, 800);
        assert_eq!(
            feed_all(&mut t, &[abs(ABS_X, 32767), DROPPED, SYN, SYN]),
            [PointerState { seq: 1, ..PointerState::default() }]
        );
    }

    #[test]
    fn other_events_are_counted_and_change_nothing() {
        let mut t = Tablet::new(QEMU, QEMU, 1280, 800);
        let s = feed_all(
            &mut t,
            &[
                abs(0x02, 5),                // ABS_Z
                Event::new(EV_KEY, 0x1e, 1), // KEY_A
                rel(0x00, 3),                // REL_X
                Event::new(0x04, 0x04, 7),   // EV_MSC
                Event::new(EV_SYN, 0x02, 0), // SYN_MT_REPORT
                SYN,
            ],
        );
        assert_eq!(s, [PointerState { seq: 1, ..PointerState::default() }]);
        assert_eq!(t.ignored(), 5);
        assert_eq!(t.first_ignored(), Some((EV_ABS, 0x02)));
        assert_eq!(Tablet::new(QEMU, QEMU, 1, 1).first_ignored(), None);
    }

    #[test]
    fn a_press_after_a_lost_release_is_counted() {
        // The release was in a dropped frame, so the button still seems
        // held when it is pressed again: that press counts all the same.
        let mut t = Tablet::new(QEMU, QEMU, 100, 100);
        let events = [
            key(BTN_LEFT, true),
            SYN,
            DROPPED,
            key(BTN_LEFT, false),
            SYN,
            key(BTN_LEFT, true),
            SYN,
            key(BTN_LEFT, false),
            SYN,
        ];
        let last = *feed_all(&mut t, &events).last().unwrap();
        assert_eq!((last.presses_of(Buttons::LEFT), last.buttons), (2, Buttons::NONE));
    }

    #[test]
    fn a_tablet_has_absolute_x_and_y() {
        assert!(has_abs_xy(&[0b11]));
        assert!(has_abs_xy(&[0xff, 0xff]));
        assert!(!has_abs_xy(&[0b01]), "X only");
        assert!(!has_abs_xy(&[0b10]), "Y only");
        assert!(!has_abs_xy(&[0b1100]), "other axes only");
        assert!(!has_abs_xy(&[]), "no absolute axes");
    }

    /// QEMU's tablet's `EV_BITS` (hw/input/virtio-input-hid.c): `EV_KEY`
    /// from `keymap_button` (left, right, middle, side, extra
    /// 0x110..=0x114, touch 0x14a, gear down and up 0x150 and 0x151),
    /// `EV_REL` `REL_WHEEL` and `EV_ABS` `ABS_X` and `ABS_Y`: 8.2 to 10.x
    /// with `wheel-axis` on (the default; `virtio_tablet_config_v2`), and
    /// 11.0 onward, which dropped the property (`virtio_tablet_config`).
    fn qemu_tablet_bits() -> [Vec<u8>; 3] {
        let mut key = vec![0u8; 0x151 / 8 + 1];
        for code in [0x110, 0x111, 0x112, 0x113, 0x114, 0x14a, 0x150, 0x151] {
            key[code / 8] |= 1 << (code % 8);
        }
        [key, vec![0, 1 << (REL_WHEEL - 8)], vec![(1 << ABS_X) | (1 << ABS_Y)]]
    }

    /// QEMU's tablet's frames: up to 12 events.
    const QEMU_FRAME: u64 = 12;

    #[test]
    fn a_frame_holds_each_code_once_and_a_sync() {
        let bits = qemu_tablet_bits();
        let typed = [(EV_KEY, &bits[0][..]), (EV_REL, &bits[1][..]), (EV_ABS, &bits[2][..])];
        assert_eq!(frame_bound(typed), QEMU_FRAME);
        assert_eq!(frame_bound([]), 1, "nothing but the sync");
        assert_eq!(frame_bound([(EV_KEY, &[0xffu8, 0x01][..]), (EV_REL, &[][..])]), 10);
    }

    #[test]
    fn only_the_types_a_device_sends_count() {
        // EV_MSC (4) and EV_SW (5) are sent in frames; EV_LED (0x11),
        // EV_SND (0x12), EV_REP (0x14) and EV_FF (0x15) are output to the
        // device; EV_SYN (0) is the sync, counted once.
        let one: &[u8] = &[0b1];
        assert_eq!(frame_bound([(0x04, one), (0x05, one)]), 3);
        assert_eq!(frame_bound([(0x00, one), (0x11, one), (0x12, one), (0x14, one), (0x15, one)]), 1);
    }

    #[test]
    fn a_frame_too_big_for_the_ring_counts_every_batch() {
        // A bound of the ring's size or more: any batch that read anything
        // counts (overruns can't be told apart); an empty one never does.
        for frame in [32, 33, 1000] {
            assert!(!may_have_lost_frames(0, 32, frame));
            assert!(may_have_lost_frames(1, 32, frame));
        }
        assert!(!may_have_lost_frames(1, 32, 31), "31 fits with 1 used");
        assert!(may_have_lost_frames(2, 32, 31));
    }

    #[test]
    fn a_poll_near_a_full_ring_may_have_lost_frames() {
        // 32 buffers, frames of up to 12 events: with 20 read, 12 were
        // free at worst (any frame fits); with 21, only 11 (a 12-event
        // frame may have been dropped). With moves only (3) it would be 30.
        assert!(!may_have_lost_frames(0, 32, QEMU_FRAME));
        assert!(!may_have_lost_frames(20, 32, QEMU_FRAME));
        assert!(may_have_lost_frames(21, 32, QEMU_FRAME));
        assert!(may_have_lost_frames(100, 32, QEMU_FRAME));
        assert!(!may_have_lost_frames(29, 32, 3));
        assert!(may_have_lost_frames(30, 32, 3));
    }

    /// QEMU's tablet and the kernel's `mouse` thread sharing a ring, one
    /// step at a time in a random order. The device sends a frame only if
    /// it has a buffer for each of its events (QEMU's `virtio_input_send`
    /// pops a buffer per event at the frame's sync, and drops the whole
    /// frame if it runs out, tracing `virtio_input_queue_full`). The
    /// thread pops a used buffer and re-posts it in two steps (the crate's
    /// `pop_pending_event`: the device may act in between), and a batch
    /// ends when nothing is left to pop; it counts as an overrun if
    /// `may_have_lost_frames(events, counted_ring, frame)`.
    #[derive(Default)]
    struct Ring {
        /// The ring size and frame bound the thread counts with.
        counted_ring: u64,
        frame: u64,
        /// Buffers the device may fill.
        free: u64,
        /// Events filled and not yet popped.
        used: u64,
        /// A buffer popped and not yet re-posted.
        held: bool,
        /// Events this batch has popped.
        batch: u64,
        /// Frames dropped since the last batch that may have lost frames.
        lost: u64,
        sent: u64,
        dropped: u64,
        /// A batch that ended with frames dropped and wasn't counted.
        uncounted: Option<(u64, u64)>,
    }

    impl Ring {
        fn new(posted: u64, counted_ring: u64, frame: u64) -> Ring {
            // All `posted` buffers (32, less any lost to the ring) start free.
            Ring { counted_ring, frame, free: posted, ..Ring::default() }
        }

        fn device_sends(&mut self, events: u64) {
            if self.free >= events {
                self.free -= events;
                self.used += events;
                self.sent += 1;
            } else {
                self.lost += 1;
                self.dropped += 1;
            }
        }

        /// The thread's next step: re-post, pop, or end the batch.
        fn thread_steps(&mut self) {
            if self.held {
                self.held = false;
                self.free += 1;
            } else if self.used > 0 {
                self.used -= 1;
                self.held = true;
                self.batch += 1;
            } else {
                if may_have_lost_frames(self.batch, self.counted_ring, self.frame) {
                    self.lost = 0;
                } else if self.lost > 0 && self.uncounted.is_none() {
                    self.uncounted = Some((self.lost, self.batch));
                }
                self.batch = 0;
            }
        }
    }

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }

    /// What [`model`] found.
    struct Outcome {
        /// The first batch that ended with frames dropped and wasn't
        /// counted: (frames dropped, events in the batch).
        uncounted: Option<(u64, u64)>,
        /// Frames sent and dropped in all (up to that batch's run).
        sent: u64,
        dropped: u64,
    }

    /// 200 runs of bursts and pauses with frames of the sizes `sizes`
    /// (drawn with equal odds), on `posted` buffers, the thread counting
    /// with a ring of 32 and frames of `frame`.
    fn model(sizes: &[u64], posted: u64, frame: u64) -> Outcome {
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        let (mut sent, mut dropped) = (0, 0);
        for _ in 0..200 {
            let mut r = Ring::new(posted, 32, frame);
            // How often the device acts against the thread during a burst:
            // from evenly to many times as often.
            let device_weight = 1 + rng.below(16);
            for _ in 0..100 {
                // A burst (short or long), then a pause in which only the
                // thread runs: half the time long enough to read
                // everything, so a batch ends right after a burst that
                // filled the ring, perhaps to the last frame that fitted.
                let longest = if rng.below(2) == 0 { 8 } else { 100 };
                let burst = rng.below(longest);
                for _ in 0..burst {
                    if rng.below(device_weight + 1) == 0 {
                        r.thread_steps();
                    } else {
                        let size = sizes[rng.below(sizes.len() as u64) as usize];
                        r.device_sends(size);
                    }
                }
                let full = rng.below(2) == 0;
                let pause = if full { 3 * posted } else { rng.below(3 * posted) };
                for _ in 0..pause {
                    r.thread_steps();
                }
            }
            // Drain: the thread's last batches end.
            for _ in 0..3 * posted {
                r.thread_steps();
            }
            (sent, dropped) = (sent + r.sent, dropped + r.dropped);
            if r.uncounted.is_some() {
                return Outcome { uncounted: r.uncounted, sent, dropped };
            }
            assert_eq!(r.lost, 0, "frames dropped after the last batch");
        }
        Outcome { uncounted: None, sent, dropped }
    }

    #[test]
    fn no_dropped_frame_goes_uncounted() {
        // Every frame the device drops is counted by the first batch to
        // end after the drop (see `may_have_lost_frames`). Frames of every
        // size QEMU's tablet can send with each code once, from 2 to all
        // 12 events: a press or release is 2, a move 3, one QMP event with
        // a button and both axes 4, two buttons and the axes 5.
        let sizes: Vec<u64> = (2..=QEMU_FRAME).collect();
        let all = model(&sizes, 32, QEMU_FRAME);
        assert_eq!(all.uncounted, None, "(frames dropped, events in the batch)");
        // The runs dropped frames and delivered them (the model isn't
        // vacuous).
        assert!(all.sent > 10_000 && all.dropped > 10_000, "sent {}, dropped {}", all.sent, all.dropped);
        // The burst scenario's frames: moves (3), and 4 buttons with a move (7).
        assert_eq!(model(&[3, 7], 32, QEMU_FRAME).uncounted, None);
    }

    #[test]
    fn the_preconditions_are_needed() {
        // A frame one bigger than the bound can be dropped with fewer
        // buffers in use than the bound counts on: it goes uncounted.
        let mut sizes: Vec<u64> = (2..=QEMU_FRAME).collect();
        sizes.push(QEMU_FRAME + 1);
        assert!(model(&sizes, 32, QEMU_FRAME).uncounted.is_some());
        // So can a 4-event frame when only moves were counted on (3), as
        // before #124's review.
        assert!(model(&[3, 4], 32, 3).uncounted.is_some());
        // And a 7-event frame (the burst scenario's) when 3 was counted on.
        assert!(model(&[7], 32, 3).uncounted.is_some());
        // A buffer lost to the ring (a re-post that failed) leaves 31
        // posted while the thread counts on 32.
        assert!(model(&[2, 3, QEMU_FRAME], 31, QEMU_FRAME).uncounted.is_some());
    }

    #[test]
    fn after_overrun_marks_an_overrun_batch_after_its_first_and_the_next_batch_s_first() {
        let mut marks = AfterOverrun::new();
        let batch = |m: &mut AfterOverrun, frames, overrun| m.batch(frames, overrun).collect::<Vec<_>>();
        assert_eq!(batch(&mut marks, 2, false), [false, false]);
        assert_eq!(batch(&mut marks, 3, true), [false, true, true]);
        assert_eq!(batch(&mut marks, 2, false), [true, false]);
        assert_eq!(batch(&mut marks, 1, false), [false]);
        // An overrun batch with no frame passes the mark on, across empty
        // batches, to the next frame.
        assert_eq!(batch(&mut marks, 0, true), Vec::<bool>::new());
        assert_eq!(batch(&mut marks, 0, false), Vec::<bool>::new());
        assert_eq!(batch(&mut marks, 2, false), [true, false]);
        // Back-to-back overruns: the second's first frame follows the
        // first's loss.
        assert_eq!(batch(&mut marks, 2, true), [false, true]);
        assert_eq!(batch(&mut marks, 2, true), [true, true]);
        assert_eq!(batch(&mut marks, 1, false), [true]);
        assert_eq!(batch(&mut marks, 1, false), [false]);
    }
}
