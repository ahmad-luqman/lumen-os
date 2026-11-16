//! The mouse: virtio's tablet (an absolute pointer, #98) through the virtio
//! adapter, and the pointer it moves on the screen.
//!
//! - **Which device.** virtio's keyboard and tablet share a PCI id
//!   (1af4:1052, virtio input), but QEMU gives them different class codes:
//!   the keyboard 09/00 (input, keyboard), the tablet 09/80 (input, other).
//!   [`matches`] takes only the latter, so the keyboard is never offered to
//!   this driver. Other "other" input devices (a multitouch panel, a host
//!   device passed through) share 09/80: [`probe`] reads the device's
//!   `EV_BITS` for `EV_ABS` through the raw transport, before the crate's
//!   `VirtIOInput::new` posts its 32 event buffers, and declines one that
//!   lacks `ABS_X` and `ABS_Y`. Only the first tablet is used: another is
//!   declined before its transport is set up (the state and the pointer
//!   are single).
//! - **One owner.** The probe hands the `VirtIOInput` to the `mouse`
//!   thread, the only code that touches the device afterwards: no lock is
//!   held across a wait. It sleeps until the device's MSI-X interrupt
//!   (through `virtio::transport`) or a [`hold`] rings [`BELL`] (or
//!   [`IDLE_CHECK_NS`] passes, to look at the device's status), or, held,
//!   until [`release`], so it costs no CPU while
//!   nothing happens; then it drains the event queue, feeds the events to
//!   a `lumen_input::Tablet`, and after a batch publishes the newest state
//!   and the batch's counts under one lock ([`status`]) and moves the
//!   pointer (`term::move_pointer`). A press and release inside one batch
//!   still shows, while no frame was dropped at the device: a state counts
//!   each button's presses so far (that a click happened, not where). A
//!   device that can't take its vector fails the probe.
//! - **Overruns.** The ring holds 32 event buffers and QEMU drops a whole
//!   frame that doesn't fit, without a `SYN_DROPPED`. How big a frame can
//!   be comes from the device: the probe counts its `EV_BITS`
//!   (`lumen_input::frame_bound`: 12 on QEMU's tablet), and the boot line
//!   says it; a bound of the ring's size or more is warned
//!   about. A batch that read so many events that a frame of that size
//!   may not have fit (21 or more of 32 on QEMU's tablet) counts as an
//!   overrun (warned about once), and the state is marked stale until a
//!   clean batch brings a frame (the warning says when the batch was the
//!   first after `mouse hold`; a held overflow uses up the one warning, so
//!   a later real overrun is only counted). What the count guarantees, and
//!   when, is
//!   `lumen_input::may_have_lost_frames`'s doc (#124); with a bound of the
//!   ring's size or more every non-empty batch counts, so the state stays
//!   stale while events come. A batch of 16 events or more (half the ring)
//!   looks again at once instead of waiting. A batch is warned about
//!   before its counts are published, so a `mouse` in between shows the
//!   frames and overruns before it.
//! - **Failure.** A device that stops being ready (`FAILED`, which for the
//!   tablet means the status reads all ones or something else gave up on
//!   it, `DEVICE_NEEDS_RESET`, or `DRIVER_OK` gone after a reset) ends the
//!   thread: `mouse` shows it failed (nothing held), the pointer leaves the
//!   screen, an error gives the DMA runs outstanding in all (the tablet's
//!   up to 32 posted buffers among them), and `lspci` shows it failed
//!   (`pci::failed_after_bind`). Not tested: QEMU can't make the device
//!   fail.
//!   Dropping the driver: the crate's `Drop` unsets the queues (nothing,
//!   on PCI), then the transport resets the device (one that doesn't reset
//!   loses its bus mastering first), then the queues free their rings; the
//!   up to 32 event buffers still posted are never unshared, so their
//!   bounce runs leak (section 7's known limit).
//! - **Known limits of the crate:** `pop_pending_event` returns `None` on
//!   an error popping or re-posting a buffer (indistinguishable from "no
//!   event"; a buffer it fails to re-post is lost to the ring), and it
//!   doesn't check the length the device wrote (a short event would read
//!   the previous contents).
//! - **Holding.** `mouse hold` ([`hold`]) stops the `mouse` thread reading
//!   the ring until `mouse release` ([`release`]): one state ([`Reads`]:
//!   running, hold asked, held), changed by compare-and-swap. `hold` asks,
//!   rings [`BELL`] (with nothing behind it) in case the thread waits for
//!   the device, and returns once the thread has parked, so no event is
//!   read from then on; events wait in the ring, where QEMU drops each
//!   frame that no longer fits. Released, the thread reads everything in
//!   the ring in one batch. So an overrun can be made on purpose, whatever
//!   the host's speed (#142: the scenario that waited for a busy guest to
//!   fall behind didn't always). `hold` fails with no tablet, a failed one
//!   (also one that fails while it waits), a release from elsewhere first,
//!   or no park within [`HOLD_WAIT_NS`] (a bug, logged). While held the
//!   device's status isn't looked at either. The state is every
//!   console's: a hold from one console is released from any, and `mouse
//!   hold | ...` holds as a side effect. Both are logged at INFO, so a
//!   forgotten hold shows in `dmesg`.
//! - **The `InputDevice` class** (docs/ARCHITECTURE.md, section 7): the
//!   tablet's messages are `lumen_input::PointerState`s; M4's desktop
//!   consumes them.

use alloc::string::String;
use core::fmt;
use core::sync::atomic::{AtomicU8, Ordering};

use log::{debug, error, info, warn};
use lumen_input::{AbsAxis, Event, PointerState, Tablet, evdev, frame_bound, has_abs_xy, may_have_lost_frames};
use lumen_pci::PciAddr;
use lumen_pci::names::virtio_type;
use virtio_drivers::device::input::VirtIOInput;
use virtio_drivers::transport::{DeviceStatus, Transport};

use crate::drivers::Driver;
use crate::pci::{DeviceInfo, DeviceResources};
use crate::sched::WaitQueue;
use crate::sync::SpinLock;
use crate::virtio::{self, Doorbell, ProbeError, StatusHandle, VirtioHal, VirtioTransport};

/// The driver table's row.
pub const DRIVER: Driver = Driver { name: "virtio-tablet", matches, probe };

/// How long the `mouse` thread waits for an interrupt before it looks at
/// the device's status anyway (a device that stops without saying so, its
/// status reading all ones, sends none).
const IDLE_CHECK_NS: u64 = 1_000_000_000;

/// Rung by the tablet's MSI-X interrupt, whatever it signals (a buffer
/// back on either queue, events among them, or a configuration change),
/// and by [`hold`] with nothing behind it (to wake the thread so it
/// parks).
static BELL: Doorbell = Doorbell::new();

fn on_interrupt() {
    BELL.ring();
}

/// Whether the `mouse` thread reads the ring: [`hold`] asks it to stop,
/// the thread parks, [`release`] lets it read again. One state, changed by
/// compare-and-swap, so `hold` can't see a park the thread is already
/// leaving. Shared by every console: a hold from one is released by any.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Reads {
    /// It reads (the normal state).
    Running = 0,
    /// [`hold`] asked it to stop; it reads nothing more once it sees this,
    /// and parks.
    HoldAsked = 1,
    /// Parked: it reads nothing until [`release`].
    Held = 2,
}

static READS: AtomicU8 = AtomicU8::new(Reads::Running as u8);
/// Where the `mouse` thread waits, parked, for [`release`].
static RELEASED: WaitQueue = WaitQueue::new();
/// Where [`hold`] waits for the thread to park.
static PARKED: WaitQueue = WaitQueue::new();

/// How long [`hold`] waits for the `mouse` thread to park.
const HOLD_WAIT_NS: u64 = 1_000_000_000;

/// Whether the `mouse` thread reads ([`Reads`]).
pub fn reads() -> Reads {
    match READS.load(Ordering::SeqCst) {
        0 => Reads::Running,
        1 => Reads::HoldAsked,
        _ => Reads::Held,
    }
}

/// From `from` to `to`, if it is still `from`; what it was.
fn reads_swap(from: Reads, to: Reads) -> Result<(), Reads> {
    READS.compare_exchange(from as u8, to as u8, Ordering::SeqCst, Ordering::SeqCst).map(drop).map_err(|_| reads())
}

/// Why [`hold`] couldn't hold the `mouse` thread.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HoldError {
    /// No tablet is bound.
    NoTablet,
    /// The tablet failed (before the hold, or while it waited): its thread
    /// has ended.
    Failed,
    /// Released (from another console) before the thread parked.
    Released,
    /// The thread didn't park within [`HOLD_WAIT_NS`] (a bug; logged).
    NotParked,
}

impl fmt::Display for HoldError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            HoldError::NoTablet => f.write_str("no tablet"),
            HoldError::Failed => f.write_str("the tablet failed"),
            HoldError::Released => f.write_str("released before the mouse thread stopped reading"),
            HoldError::NotParked => {
                write!(f, "the mouse thread didn't stop reading within {} ms", HOLD_WAIT_NS / 1_000_000)
            }
        }
    }
}

/// The bound tablet's address, if it hasn't failed.
fn working_tablet() -> Result<PciAddr, HoldError> {
    match BOUND.lock().as_ref() {
        None => Err(HoldError::NoTablet),
        Some(b) if b.failed.is_some() => Err(HoldError::Failed),
        Some(b) => Ok(b.device.addr),
    }
}

/// Stop the `mouse` thread reading the ring (see the module doc); returns
/// once it is parked, so nothing is read from then on (`Ok` at once if it
/// is held already). Call it from a thread that can block (it waits up to
/// [`HOLD_WAIT_NS`]); the hold is every console's.
pub fn hold() -> Result<(), HoldError> {
    let addr = working_tablet()?;
    if !ask_hold() {
        return Ok(());
    }
    match wait_parked(crate::time::now_ns().saturating_add(HOLD_WAIT_NS)) {
        Reads::Held => {
            info!(target: "mouse", "{addr}: held: the mouse thread reads nothing until `mouse release`");
            Ok(())
        }
        Reads::Running => Err(HoldError::Released),
        Reads::HoldAsked => {
            // Not parked in time: take the request back (quietly: it never
            // held), then find why.
            set_running();
            if working_tablet() == Err(HoldError::Failed) {
                return Err(HoldError::Failed);
            }
            error!(target: "mouse", "{addr}: {} (a bug): not held", HoldError::NotParked);
            Err(HoldError::NotParked)
        }
    }
}

/// Ask the `mouse` thread to park (`Running` to `HoldAsked`) and wake it
/// if it waits for the device (a ring of [`BELL`] with nothing behind
/// it); whether it wasn't held already.
fn ask_hold() -> bool {
    match reads_swap(Reads::Running, Reads::HoldAsked) {
        Ok(()) | Err(Reads::HoldAsked) => {
            BELL.ring();
            true
        }
        Err(_) => false,
    }
}

/// Wait until the `mouse` thread has parked or the request is gone, at
/// most until `deadline`; the state then (`HoldAsked` if it ran out).
fn wait_parked(deadline: u64) -> Reads {
    PARKED.wait_until_or_deadline(deadline, || reads() != Reads::HoldAsked);
    reads()
}

/// Let the `mouse` thread read again ([`hold`]): it reads everything in the
/// ring in one batch. Releasing a tablet that isn't held only says so.
pub fn release() {
    if set_running()
        && let Some(b) = BOUND.lock().as_ref()
    {
        info!(target: "mouse", "{}: released: the mouse thread reads again", b.device.addr);
    }
}

/// Back to `Running`, waking the parked thread and a hold waiting for it
/// to park; whether it wasn't running.
fn set_running() -> bool {
    let was = READS.swap(Reads::Running as u8, Ordering::SeqCst);
    RELEASED.wake_all();
    PARKED.wake_all();
    was != Reads::Running as u8
}

/// The `mouse` thread's side of a hold, at the top of its loop: if a hold
/// was asked, park (`HoldAsked` to `Held`) until released; whether it
/// parked (then it looks again before reading: a release and a new hold
/// while it slept leave it asked to park once more).
fn park_if_asked() -> bool {
    match reads() {
        Reads::Running => false,
        Reads::HoldAsked | Reads::Held => {
            // A release just now leaves it `Running`: read on.
            if reads_swap(Reads::HoldAsked, Reads::Held).is_ok() || reads() == Reads::Held {
                PARKED.wake_all();
                RELEASED.wait_until(|| reads() != Reads::Held);
            }
            true
        }
    }
}

/// The event buffers the crate keeps posted (`VirtIOInput`'s queue size).
const RING: u64 = 32;

/// virtio input's device type (virtio 1.2 §5.8).
const VIRTIO_INPUT: u16 = 18;
/// PCI class "input device", subclass "other" (QEMU's tablet; its keyboard
/// is subclass 0, a relative mouse 2).
const CLASS_INPUT: u8 = 0x09;
const SUBCLASS_OTHER: u8 = 0x80;

/// virtio input's configuration (virtio 1.2 §5.8.4): `select` and `subsel`
/// choose what `size` and `data` hold.
mod cfg {
    pub const SELECT: usize = 0;
    pub const SUBSEL: usize = 1;
    pub const SIZE: usize = 2;
    pub const DATA: usize = 8;
    pub const DATA_LEN: usize = 128;
    pub const ID_NAME: u8 = 0x01;
    pub const EV_BITS: u8 = 0x11;
    pub const ABS_INFO: u8 = 0x12;
}

type Input = VirtIOInput<VirtioHal, VirtioTransport>;

/// The tablet bound at boot.
#[derive(Clone, Debug)]
pub struct Device {
    pub addr: PciAddr,
    /// Its `ID_NAME` (QEMU's: "QEMU Virtio Tablet").
    pub name: String,
    /// The screen its positions are scaled to (the framebuffer's size, or
    /// the axes' own ranges without a terminal).
    pub screen: (usize, usize),
}

/// What the `mouse` thread has read (whole frames: the state's `seq`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Events read.
    pub events: u64,
    /// Events a tablet has no use for.
    pub ignored: u64,
    /// `SYN_DROPPED`s (the device said it lost a frame).
    pub dropped: u64,
    /// Batches that read so many events that a frame may not have fit in
    /// the ring: what that guarantees is `lumen_input::may_have_lost_frames`'s
    /// doc. QEMU drops such frames without saying so, so they can't be
    /// counted exactly.
    pub overruns: u64,
}

/// Why the tablet stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The device set `DEVICE_NEEDS_RESET`.
    NeedsReset,
    /// The status shows `FAILED`. A driver sets that bit, so for the
    /// tablet it means the status reads all ones (the device is gone) or
    /// something else gave up on the device.
    Failed,
    /// `DRIVER_OK` went clear (the status reads 0 after a reset): the
    /// device stopped running.
    Stopped,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Failure::NeedsReset => "DEVICE_NEEDS_RESET",
            Failure::Failed => "FAILED",
            Failure::Stopped => "DRIVER_OK clear",
        })
    }
}

/// The bound tablet and what its thread published, all at once.
#[derive(Clone, Debug)]
pub struct Bound {
    pub device: Device,
    /// The newest whole frame's state, if one came (its buttons cleared
    /// once the tablet failed: nothing is held any more).
    pub state: Option<PointerState>,
    pub counts: Counts,
    /// The newest batch that brought frames may have missed some (an
    /// overrun): the state may be older than the device's.
    pub stale: bool,
    /// Set when the device failed (its thread ended).
    pub failed: Option<Failure>,
}

static BOUND: SpinLock<Option<Bound>> = SpinLock::new(None);

/// The tablet, if one is bound: one consistent snapshot.
pub fn status() -> Option<Bound> {
    BOUND.lock().clone()
}

/// virtio input, class input/other: QEMU's tablet (see the module doc).
fn matches(d: &DeviceInfo) -> bool {
    virtio_type(d.vendor, d.device, d.subsystem.1) == Some(VIRTIO_INPUT)
        && (d.class, d.subclass) == (CLASS_INPUT, SUBCLASS_OTHER)
}

/// Read `select`/`subsel` from the device's configuration into `out`;
/// returns how many bytes it copied (the device's size, at most `out`'s
/// length; 0 for nothing there).
fn query(t: &mut VirtioTransport, select: u8, subsel: u8, out: &mut [u8]) -> Result<usize, ProbeError> {
    t.write_config_space(cfg::SELECT, select).map_err(ProbeError::Device)?;
    t.write_config_space(cfg::SUBSEL, subsel).map_err(ProbeError::Device)?;
    let size = usize::from(t.read_config_space::<u8>(cfg::SIZE).map_err(ProbeError::Device)?);
    let n = size.min(cfg::DATA_LEN).min(out.len());
    for (i, b) in out[..n].iter_mut().enumerate() {
        *b = t.read_config_space(cfg::DATA + i).map_err(ProbeError::Device)?;
    }
    Ok(n)
}

/// Axis `axis`'s range (`ABS_INFO`: min and max, little-endian, first).
fn axis(t: &mut VirtioTransport, axis: u16) -> Result<AbsAxis, ProbeError> {
    let mut info = [0u8; 8];
    let n = query(t, cfg::ABS_INFO, axis as u8, &mut info)?;
    if n < info.len() {
        return Err(ProbeError::Unusable(alloc::format!("axis {axis}: ABS_INFO gives {n} bytes")));
    }
    let min = i32::from_le_bytes([info[0], info[1], info[2], info[3]]);
    let max = i32::from_le_bytes([info[4], info[5], info[6], info[7]]);
    AbsAxis::new(min, max).map_err(|e| ProbeError::Unusable(alloc::format!("axis {axis}: {e}")))
}

/// The most events one of the device's frames holds
/// (`lumen_input::frame_bound` over its `EV_BITS`): the ring's size or
/// more makes every non-empty batch an overrun (`may_have_lost_frames`).
fn frame_events(t: &mut VirtioTransport) -> Result<u64, ProbeError> {
    let mut maps = alloc::vec::Vec::new();
    for ty in lumen_input::FRAME_TYPES {
        let mut bits = [0u8; cfg::DATA_LEN];
        let n = query(t, cfg::EV_BITS, ty as u8, &mut bits)?;
        maps.push((ty, bits[..n].to_vec()));
    }
    Ok(frame_bound(maps.iter().map(|(ty, bits)| (*ty, bits.as_slice()))))
}

/// Bring up a tablet: decline a second one, or a device without both
/// absolute axes (before the crate posts any buffer), read the axes'
/// ranges, start the crate's driver and check it is ready; then publish it
/// and hand it to the `mouse` thread. Those side effects come after the
/// last fallible check; the spawn, which can fail too, comes last, and a
/// failed one takes the publishing back.
fn probe(res: DeviceResources) -> Result<(), ProbeError> {
    let addr = res.info().addr;
    // One tablet: its state and the pointer are single. A second is
    // declined before anything is set up (`probe_all` probes one function
    // at a time, so nothing binds one meanwhile).
    let first = BOUND.lock().as_ref().map(|b| b.device.addr);
    if let Some(first) = first {
        info!(target: "mouse", "{addr}: a tablet is bound already (on {first}): only one is used");
        return Err(ProbeError::NotMine);
    }
    let mut t = virtio::transport(res, on_interrupt)?;
    let mut abs = [0u8; 1];
    let n = query(&mut t, cfg::EV_BITS, evdev::EV_ABS as u8, &mut abs)?;
    if !has_abs_xy(&abs[..n]) {
        info!(target: "mouse", "{addr}: no absolute X and Y axes, so not a tablet");
        return Err(ProbeError::NotMine);
    }
    let (x, y) = (axis(&mut t, evdev::ABS_X)?, axis(&mut t, evdev::ABS_Y)?);
    let frame = frame_events(&mut t)?;
    let mut name = [0u8; cfg::DATA_LEN];
    let len = query(&mut t, cfg::ID_NAME, 0, &mut name)?;
    // A terminating NUL, if counted, is dropped; anything else unprintable
    // shows as `?`.
    let name: String = String::from_utf8_lossy(&name[..len])
        .trim_end_matches('\0')
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .collect();
    // Without a screen the pointer moves over the axes' own units.
    let units = |a: AbsAxis| (i64::from(a.max()) - i64::from(a.min()) + 1) as usize;
    let screen = crate::term::screen_size().unwrap_or((units(x), units(y)));
    let status = t.status();
    let input = Input::new(t).map_err(ProbeError::Device)?;
    virtio::check_device(&status)?;
    let tablet = Tablet::new(x, y, screen.0, screen.1);
    info!(
        target: "mouse",
        "{addr}: \"{name}\", axes {} to {} by {} to {}, scaled to {}x{}, frames of up to {frame} events",
        x.min(), x.max(), y.min(), y.max(), screen.0, screen.1
    );
    if frame >= RING {
        warn!(target: "mouse", "{addr}: frames of up to {frame} events don't fit the {RING}-buffer ring: every non-empty \
            batch counts as an overrun, so overruns can't be told apart");
    }
    let device = Device { addr, name, screen };
    *BOUND.lock() = Some(Bound { device, state: None, counts: Counts::default(), stale: false, failed: None });
    match crate::sched::spawn("mouse", move || run(input, tablet, frame, status, addr)) {
        Ok(_) => Ok(()),
        Err(e) => {
            *BOUND.lock() = None;
            Err(ProbeError::Thread(e))
        }
    }
}

/// The `mouse` thread: the device's only user from here on.
fn run(mut input: Input, mut tablet: Tablet, frame: u64, status: StatusHandle, addr: PciAddr) {
    let (mut warned_overrun, mut logged_ignored) = (false, false);
    // The next batch is the first after a park (`mouse hold`).
    let mut after_hold = false;
    // Events after a wait that ran to its deadline with no ring, the ring
    // count still unchanged: they came without their interrupt.
    let mut rule = virtio::MissedRule::default();
    let mut warned_cannot_block = false;
    loop {
        // The doorbell's count, read before the hold check and the drain:
        // a doorbell ring after either (an interrupt, or `hold`'s) then
        // ends the wait below, so a `hold` that comes after the check
        // can't wait out the idle look.
        let seen = BELL.rings();
        // Held: park, reading nothing, until released, then look again
        // before reading.
        if park_if_asked() {
            after_hold = true;
            continue;
        }
        let (mut events, mut newest) = (0u64, None);
        while let Some(e) = input.pop_pending_event() {
            events += 1;
            if let Some(s) = tablet.feed(Event::new(e.event_type, e.code, e.value as i32)) {
                newest = Some(s);
            }
        }
        if events > 0 && rule.answered(seen) {
            BELL.note_missed(Some(addr));
        }
        if !logged_ignored && let Some((kind, code)) = tablet.first_ignored() {
            logged_ignored = true;
            debug!(target: "mouse", "{addr}: ignoring events a tablet has no use for, first type {kind:#x} code {code:#x}");
        }
        let overrun = may_have_lost_frames(events, RING, frame);
        if overrun && !warned_overrun {
            warned_overrun = true;
            let held = if after_hold { " after `mouse hold`" } else { "" };
            warn!(target: "mouse", "{addr}: {events} events in one batch{held}: the event ring was nearly full, so \
                frames may have been lost (said once; `mouse` counts such batches)");
        }
        if events > 0 {
            after_hold = false;
        }
        if events > 0
            && let Some(b) = BOUND.lock().as_mut()
        {
            let c = &mut b.counts;
            c.events += events;
            c.ignored = tablet.ignored();
            c.dropped = tablet.dropped();
            c.overruns += u64::from(overrun);
            if overrun {
                b.stale = true;
            } else if newest.is_some() {
                b.stale = false;
            }
            if newest.is_some() {
                b.state = newest;
            }
        }
        if let Some(s) = newest {
            crate::term::move_pointer(s.x, s.y);
        }
        if status.check_ready().is_err() {
            return fail(input, status, addr);
        }
        // A busy batch looks again at once: the ring may be filling.
        if events < RING / 2 {
            match BELL.wait_after(seen, crate::time::now_ns().saturating_add(IDLE_CHECK_NS)) {
                // The `mouse` thread holds no lock and has interrupts on:
                // this would be a bug, not a hot loop.
                virtio::Wake::CannotBlock => {
                    if !warned_cannot_block {
                        warned_cannot_block = true;
                        warn!(target: "mouse", "{addr}: the mouse thread can't block: looking every 5 ms");
                    }
                    crate::time::sleep(5_000_000);
                }
                wake => rule.blocked(seen, wake),
            }
        }
    }
}

/// The device stopped being ready (`FAILED`, `DEVICE_NEEDS_RESET`, or
/// `DRIVER_OK` gone): publish it (nothing held any more), drop the driver,
/// say so, and mark the function failed.
fn fail(input: Input, status: StatusHandle, addr: PciAddr) {
    let (raw, st) = (status.raw(), status.get());
    let why = if st.contains(DeviceStatus::FAILED) {
        Failure::Failed
    } else if st.contains(DeviceStatus::DEVICE_NEEDS_RESET) {
        Failure::NeedsReset
    } else {
        Failure::Stopped
    };
    if let Some(b) = BOUND.lock().as_mut() {
        b.failed = Some(why);
        if let Some(s) = b.state.as_mut() {
            s.buttons = lumen_input::Buttons::NONE;
        }
    }
    crate::term::hide_pointer();
    // (See the module doc for what dropping it does and leaks.)
    drop(input);
    let left = virtio::outstanding_runs();
    error!(
        target: "mouse",
        "{addr}: the tablet's status reads {raw:#04x} ({why}): the mouse stops; {left} DMA runs outstanding in \
         all (the tablet's up to {RING} posted buffers among them)"
    );
    if !crate::pci::failed_after_bind(addr, DRIVER.name, alloc::format!("its status showed {why} after binding")) {
        warn!(target: "mouse", "{addr}: couldn't record the tablet as failed: the function isn't bound to it");
    }
}

/// A made-up bound tablet for the hold ktests (the ktest boot binds none;
/// no thread reads it): the hold state and `BOUND` are put back on drop.
#[cfg(feature = "ktest")]
struct MadeUp;

#[cfg(feature = "ktest")]
impl MadeUp {
    fn bind(failed: Option<Failure>) -> Result<MadeUp, &'static str> {
        let addr = PciAddr::new(0, 0xff, 0x1e, 7).ok_or("addr")?;
        let device = Device { addr, name: String::from("made-up tablet"), screen: (1280, 800) };
        let mut bound = BOUND.lock();
        if bound.is_some() {
            return Err("a tablet is bound already: these ktests need none");
        }
        *bound = Some(Bound { device, state: None, counts: Counts::default(), stale: false, failed });
        Ok(MadeUp)
    }
}

#[cfg(feature = "ktest")]
impl Drop for MadeUp {
    fn drop(&mut self) {
        READS.store(Reads::Running as u8, Ordering::SeqCst);
        *BOUND.lock() = None;
    }
}

crate::ktest!(
    fn a_hold_needs_a_working_tablet() {
        // No tablet, or a failed one: `hold` refuses at once and asks
        // nothing of a thread.
        use crate::kassert_eq;
        kassert_eq!(hold(), Err(HoldError::NoTablet));
        let _bound = MadeUp::bind(Some(Failure::Failed))?;
        kassert_eq!(hold(), Err(HoldError::Failed));
        kassert_eq!(reads(), Reads::Running);
        Ok(())
    }
);

crate::ktest!(
    fn a_hold_that_no_thread_answers_is_taken_back() {
        // A tablet with no thread to park (made up): `hold` waits
        // HOLD_WAIT_NS, takes the request back and says why: `NotParked`
        // (logged as a bug), or `Failed` if the tablet failed meanwhile.
        use crate::kassert_eq;
        let _bound = MadeUp::bind(None)?;
        kassert_eq!(hold(), Err(HoldError::NotParked));
        kassert_eq!(reads(), Reads::Running);
        let failer = crate::sched::spawn("hold-ktest-fail", || {
            crate::time::sleep(100_000_000);
            if let Some(b) = BOUND.lock().as_mut() {
                b.failed = Some(Failure::Failed);
            }
        })
        .map_err(|_| "spawn")?;
        let got = hold();
        failer.join();
        kassert_eq!(got, Err(HoldError::Failed));
        kassert_eq!(reads(), Reads::Running);
        Ok(())
    }
);

crate::ktest!(
    fn a_release_and_a_hold_back_to_back_read_nothing_between() {
        // A stand-in for the `mouse` thread (its loop's hold check, a
        // "read" counted, a wait for the doorbell) on a made-up tablet.
        // Held, released and held again with nothing able to run in
        // between: it parks again without reading. A hold while held
        // returns at once. Released for good, it reads on.
        use crate::{kassert, kassert_eq};
        use core::sync::atomic::{AtomicBool, AtomicU64};
        static STOP: AtomicBool = AtomicBool::new(false);
        static READ: AtomicU64 = AtomicU64::new(0);
        STOP.store(false, Ordering::SeqCst);
        READ.store(0, Ordering::SeqCst);
        let _bound = MadeUp::bind(None)?;
        let reader = crate::sched::spawn("hold-ktest-reader", || {
            loop {
                let seen = BELL.rings();
                if park_if_asked() {
                    continue;
                }
                if STOP.load(Ordering::SeqCst) {
                    break;
                }
                READ.fetch_add(1, Ordering::SeqCst);
                // Rung or not, it reads again (a look every 10 ms at most).
                let _ = BELL.wait_after(seen, crate::time::now_ns().saturating_add(10_000_000));
            }
        })
        .map_err(|_| "spawn")?;
        kassert_eq!(hold(), Ok(()));
        kassert_eq!(reads(), Reads::Held);
        let before = READ.load(Ordering::SeqCst);
        {
            let _off = crate::arch::irq::push_off();
            release();
            kassert!(ask_hold());
        }
        kassert_eq!(wait_parked(crate::time::now_ns().saturating_add(HOLD_WAIT_NS)), Reads::Held);
        kassert_eq!(hold(), Ok(()));
        kassert_eq!(READ.load(Ordering::SeqCst), before);
        STOP.store(true, Ordering::SeqCst);
        release();
        reader.join();
        kassert_eq!(READ.load(Ordering::SeqCst), before);
        Ok(())
    }
);
