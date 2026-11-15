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

/// The buttons held down.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct Buttons(u8);

impl Buttons {
    pub const NONE: Buttons = Buttons(0);
    pub const LEFT: Buttons = Buttons(1 << 0);
    pub const RIGHT: Buttons = Buttons(1 << 1);
    pub const MIDDLE: Buttons = Buttons(1 << 2);
    pub const SIDE: Buttons = Buttons(1 << 3);
    pub const EXTRA: Buttons = Buttons(1 << 4);

    const NAMES: [(Buttons, &'static str); 5] = [
        (Buttons::LEFT, "left"),
        (Buttons::RIGHT, "right"),
        (Buttons::MIDDLE, "middle"),
        (Buttons::SIDE, "side"),
        (Buttons::EXTRA, "extra"),
    ];

    /// The button an `EV_KEY` code names, if it is a pointer's: its index
    /// in [`PointerState::presses`] and its bit.
    fn from_code(code: u16) -> Option<(usize, Buttons)> {
        let i = match code {
            BTN_LEFT => 0,
            BTN_RIGHT => 1,
            BTN_MIDDLE => 2,
            BTN_SIDE => 3,
            BTN_EXTRA => 4,
            _ => return None,
        };
        Some((i, Buttons::NAMES[i].0))
    }

    pub const fn contains(self, other: Buttons) -> bool {
        self.0 & other.0 == other.0
    }

    fn set(&mut self, b: Buttons, down: bool) {
        if down {
            self.0 |= b.0;
        } else {
            self.0 &= !b.0;
        }
    }
}

/// The buttons' names, space-separated (`left right`), or `none`.
impl fmt::Display for Buttons {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.0 == 0 {
            return f.write_str("none");
        }
        let mut first = true;
        for (b, name) in Buttons::NAMES {
            if self.contains(b) {
                if !first {
                    f.write_str(" ")?;
                }
                f.write_str(name)?;
                first = false;
            }
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
    /// Each button's presses so far (left, right, middle, side, extra): a
    /// press and release between two looks shows here although
    /// [`PointerState::buttons`] has it up again.
    pub presses: [u64; 5],
    /// Whole frames so far, this one included (1 for the first).
    pub seq: u64,
}

impl PointerState {
    /// Every button's presses so far.
    pub fn clicks(&self) -> u64 {
        self.presses.iter().sum()
    }
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
            (EV_KEY, code) => match Buttons::from_code(code) {
                Some((i, b)) => {
                    // 0 released, 1 pressed, 2 held (autorepeat).
                    let down = e.value != 0;
                    if down && !self.pending.buttons.contains(b) {
                        self.pending.presses[i] += 1;
                    }
                    self.pending.buttons.set(b, down);
                }
                None => self.ignored += 1,
            },
            _ => self.ignored += 1,
        }
        None
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
        assert_eq!(states[3].clicks(), 5);
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
        assert_eq!((s.buttons, s.presses[1], s.clicks()), (Buttons::NONE, 1, 2));
        // Held with autorepeat, released: one press.
        let held = [key(BTN_LEFT, true), SYN, Event::new(EV_KEY, BTN_LEFT, 2), SYN, key(BTN_LEFT, false), SYN];
        assert_eq!(feed_all(&mut t, &held).last().unwrap().presses[0], 2);
    }

    #[test]
    fn wheels_count_clicks_both_ways() {
        let mut t = Tablet::new(QEMU, QEMU, 100, 100);
        let s = feed_all(
            &mut t,
            &[rel(REL_WHEEL, 1), rel(REL_WHEEL, 1), SYN, rel(REL_WHEEL, -3), rel(REL_HWHEEL, -1), SYN],
        );
        assert_eq!((s[0].wheel, s[0].hwheel), (2, 0));
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
    }
}
