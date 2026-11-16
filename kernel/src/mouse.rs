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
//!   (through `virtio::transport`) rings [`BELL`] (or [`IDLE_CHECK_NS`]
//!   passes, to look at the device's status), so it costs no CPU while
//!   nothing happens; then it drains the event queue, feeds the events to
//!   a `lumen_input::Tablet`, and after a batch publishes the newest state
//!   and the batch's counts under one lock ([`status`]) and moves the
//!   pointer (`term::move_pointer`). A press and release inside one batch
//!   still shows, while no frame was dropped at the device: a state counts
//!   each button's presses so far (that a click happened, not where). A
//!   device that can't take its vector fails the probe.
//! - **Overruns.** The ring holds 32 event buffers and QEMU drops a whole
//!   frame (a move is 3 events) that doesn't fit, without a `SYN_DROPPED`.
//!   A batch of 30 events or more may have missed frames: it counts as an
//!   overrun (warned about once), the state is marked stale until a clean
//!   batch brings a frame, and the thread looks again at once instead of
//!   waiting. The count is a lower bound: frames the device dropped
//!   can't be counted exactly. But no dropped frame goes uncounted: when
//!   one was dropped at most 2 buffers were free, so at least 30 were
//!   filled (or popped and not yet re-posted), and the batch going on or
//!   the next reads all of them before it ends (`lumen_input`'s
//!   `no_dropped_frame_goes_uncounted` models the ring, #124). A batch is
//!   warned about before its counts are published, so a `mouse` in
//!   between shows the frames and overruns before it.
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
//! - **The `InputDevice` class** (docs/ARCHITECTURE.md, section 7): the
//!   tablet's messages are `lumen_input::PointerState`s; M4's desktop
//!   consumes them.

use alloc::string::String;
use core::fmt;

use log::{debug, error, info, warn};
use lumen_input::{AbsAxis, Event, PointerState, Tablet, evdev, has_abs_xy, may_have_lost_frames};
use lumen_pci::PciAddr;
use lumen_pci::names::virtio_type;
use virtio_drivers::device::input::VirtIOInput;
use virtio_drivers::transport::{DeviceStatus, Transport};

use crate::drivers::Driver;
use crate::pci::{DeviceInfo, DeviceResources};
use crate::sync::SpinLock;
use crate::virtio::{self, Doorbell, ProbeError, StatusHandle, VirtioHal, VirtioTransport};

/// The driver table's row.
pub const DRIVER: Driver = Driver { name: "virtio-tablet", matches, probe };

/// How long the `mouse` thread waits for an interrupt before it looks at
/// the device's status anyway (a device that stops without saying so, its
/// status reading all ones, sends none).
const IDLE_CHECK_NS: u64 = 1_000_000_000;

/// Rung by the tablet's MSI-X interrupt: whatever it signals (a buffer back
/// on either queue, events among them, or a configuration change).
static BELL: Doorbell = Doorbell::new();

fn on_interrupt() {
    BELL.ring();
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
    /// the ring (`lumen_input::may_have_lost_frames`): a lower bound on
    /// trouble, since QEMU drops such frames without saying so and they
    /// can't be counted exactly.
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
        "{addr}: \"{name}\", axes {} to {} by {} to {}, scaled to {}x{}",
        x.min(), x.max(), y.min(), y.max(), screen.0, screen.1
    );
    let device = Device { addr, name, screen };
    *BOUND.lock() = Some(Bound { device, state: None, counts: Counts::default(), stale: false, failed: None });
    match crate::sched::spawn("mouse", move || run(input, tablet, status, addr)) {
        Ok(_) => Ok(()),
        Err(e) => {
            *BOUND.lock() = None;
            Err(ProbeError::Thread(e))
        }
    }
}

/// The `mouse` thread: the device's only user from here on.
fn run(mut input: Input, mut tablet: Tablet, status: StatusHandle, addr: PciAddr) {
    let (mut warned_overrun, mut logged_ignored) = (false, false);
    // Events after a wait that ran to its deadline with no ring, the ring
    // count still unchanged: they came without their interrupt.
    let mut rule = virtio::MissedRule::default();
    let mut warned_cannot_block = false;
    loop {
        // Read before draining, so an interrupt after the drain ends the
        // wait below.
        let seen = BELL.rings();
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
        let overrun = may_have_lost_frames(events, RING);
        if overrun && !warned_overrun {
            warned_overrun = true;
            warn!(target: "mouse", "{addr}: {events} events in one batch: the event ring was nearly full, so frames \
                may have been lost (said once; `mouse` counts such batches)");
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
