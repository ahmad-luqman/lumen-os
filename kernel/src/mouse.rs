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
//!   held across a wait. It polls the event queue every [`POLL_NS`]
//!   (sleeping in between, so it costs no CPU; MSI-X, #96, will replace the
//!   sleep with a wait its interrupt ends), feeds the events to a
//!   `lumen_input::Tablet`, and after a batch publishes the newest state
//!   and the batch's counts under one lock ([`status`]) and moves the
//!   pointer (`term::move_pointer`). A press and release inside one poll
//!   still shows: a state counts each button's presses so far. A batch of
//!   32 events or more (the event ring holds 32) counts as an overrun:
//!   events may have been lost (warned about once).
//! - **Failure.** A device that sets `DEVICE_NEEDS_RESET` or `FAILED` ends
//!   the thread: an error naming the DMA runs left outstanding, `mouse`
//!   and `lspci` show it failed, and the pointer leaves the screen.
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

use alloc::string::{String, ToString};
use core::fmt;

use log::{debug, error, info, warn};
use lumen_input::{AbsAxis, Event, PointerState, Tablet, evdev};
use lumen_pci::PciAddr;
use lumen_pci::names::virtio_type;
use virtio_drivers::device::input::VirtIOInput;
use virtio_drivers::transport::{DeviceStatus, Transport};

use crate::drivers::Driver;
use crate::pci::{DeviceInfo, DeviceResources};
use crate::sync::SpinLock;
use crate::virtio::{self, ProbeError, StatusHandle, VirtioHal, VirtioTransport};

/// The driver table's row.
pub const DRIVER: Driver = Driver { name: "virtio-tablet", matches, probe };

/// How often the `mouse` thread looks for events: 200 times a second, so a
/// move shows within a frame or so of a 60 Hz screen.
pub const POLL_NS: u64 = 5_000_000;

/// The event buffers the crate keeps posted (`VirtIOInput`'s queue size):
/// a poll that reads this many may have found the ring full.
const RING: usize = 32;

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

/// What the `mouse` thread has read.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Counts {
    /// Events read, and whole frames among them.
    pub events: u64,
    pub frames: u64,
    /// Events a tablet has no use for.
    pub ignored: u64,
    /// `SYN_DROPPED`s (the device's own buffer overflowed: a frame lost).
    pub dropped: u64,
    /// Polls that read a whole ring's worth (events may have been lost).
    pub overruns: u64,
}

/// Why the tablet stopped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Failure {
    /// The device set `DEVICE_NEEDS_RESET`.
    NeedsReset,
    /// The device set `FAILED` (or reads all ones: gone).
    Failed,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Failure::NeedsReset => "DEVICE_NEEDS_RESET",
            Failure::Failed => "FAILED",
        })
    }
}

/// The bound tablet and what its thread published, all at once.
#[derive(Clone, Debug)]
pub struct Bound {
    pub device: Device,
    /// The newest whole frame's state, if one came.
    pub state: Option<PointerState>,
    pub counts: Counts,
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
/// ranges, start the crate's driver and check it is ready, then publish it
/// and hand it to the `mouse` thread (the side effects after the last
/// fallible check, except the spawn itself, undone if it fails).
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
    let mut t = virtio::transport(res)?;
    let mut abs = [0u8; 1];
    let n = query(&mut t, cfg::EV_BITS, evdev::EV_ABS as u8, &mut abs)?;
    let xy = (1 << evdev::ABS_X) | (1 << evdev::ABS_Y);
    if n == 0 || abs[0] & xy != xy {
        info!(target: "mouse", "{addr}: no absolute X and Y axes, so not a tablet");
        return Err(ProbeError::NotMine);
    }
    let (x, y) = (axis(&mut t, evdev::ABS_X)?, axis(&mut t, evdev::ABS_Y)?);
    let mut name = [0u8; cfg::DATA_LEN];
    let len = query(&mut t, cfg::ID_NAME, 0, &mut name)?;
    // QEMU counts a terminating NUL; anything else unprintable shows as `?`.
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
    status.check_ready()?;
    let tablet = Tablet::new(x, y, screen.0, screen.1);
    info!(
        target: "mouse",
        "{addr}: \"{name}\", axes {} to {} by {} to {}, scaled to {}x{}",
        x.min(), x.max(), y.min(), y.max(), screen.0, screen.1
    );
    let device = Device { addr, name, screen };
    *BOUND.lock() = Some(Bound { device, state: None, counts: Counts::default(), failed: None });
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
    loop {
        let (mut events, mut frames, mut newest) = (0u64, 0u64, None);
        while let Some(e) = input.pop_pending_event() {
            events += 1;
            let ignored = tablet.ignored();
            let event = Event::new(e.event_type, e.code, e.value as i32);
            if let Some(s) = tablet.feed(event) {
                frames += 1;
                newest = Some(s);
            }
            if !logged_ignored && tablet.ignored() > ignored {
                logged_ignored = true;
                debug!(target: "mouse", "{addr}: ignoring events a tablet has no use for, first type {:#x} code {:#x}",
                    event.kind, event.code);
            }
        }
        let overrun = events >= RING as u64;
        if overrun && !warned_overrun {
            warned_overrun = true;
            warn!(target: "mouse", "{addr}: {events} events in one poll: the event ring was full, some may be lost \
                (said once; `mouse` counts them)");
        }
        if events > 0
            && let Some(b) = BOUND.lock().as_mut()
        {
            let c = &mut b.counts;
            c.events += events;
            c.frames += frames;
            c.ignored = tablet.ignored();
            c.dropped = tablet.dropped();
            c.overruns += u64::from(overrun);
            if newest.is_some() {
                b.state = newest;
            }
        }
        if let Some(s) = newest {
            crate::term::move_pointer(s.x, s.y);
        }
        let st = status.get();
        if st.intersects(DeviceStatus::DEVICE_NEEDS_RESET | DeviceStatus::FAILED) {
            let why = if st.contains(DeviceStatus::FAILED) { Failure::Failed } else { Failure::NeedsReset };
            let raw = status.raw();
            // (See the module doc for what dropping it does and leaks.)
            drop(input);
            let left = virtio::outstanding_runs();
            error!(
                target: "mouse",
                "{addr}: the tablet's status reads {raw:#04x} ({why}): the mouse stops; {left} DMA runs \
                 outstanding now (its posted event buffers, up to {RING}, never come back)"
            );
            if let Some(b) = BOUND.lock().as_mut() {
                b.failed = Some(why);
            }
            crate::pci::failed_after_bind(addr, alloc::format!("the device set {why} after binding"));
            crate::term::hide_pointer();
            return;
        }
        crate::time::sleep(POLL_NS);
    }
}

/// `mouse`'s text: its header line (the device and the counts, or that
/// there is no tablet), for `bound` (one snapshot, as its row uses).
pub fn header(bound: Option<&Bound>) -> String {
    let Some(b) = bound else { return "mouse: no tablet (QEMU: -device virtio-tablet-pci)".to_string() };
    let (d, c) = (&b.device, &b.counts);
    let failed = b.failed.map_or(String::new(), |why| alloc::format!(", failed: {why}"));
    alloc::format!(
        "mouse: \"{}\" on {}, screen {}x{}, {} events in {} frames ({} ignored, {} dropped, {} overruns){failed}",
        d.name,
        d.addr,
        d.screen.0,
        d.screen.1,
        c.events,
        c.frames,
        c.ignored,
        c.dropped,
        c.overruns
    )
}
