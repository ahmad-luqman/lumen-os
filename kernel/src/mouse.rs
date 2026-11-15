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
//!   ([`status`]) and moves the pointer (`term::move_pointer`). A device
//!   that sets `DEVICE_NEEDS_RESET` or `FAILED` ends the thread, with an
//!   error, and takes the pointer off the screen.
//! - **The `InputDevice` class** (docs/ARCHITECTURE.md, section 7): the
//!   tablet's messages are `lumen_input::PointerState`s, one per frame;
//!   M4's desktop consumes them.

use alloc::string::String;
use core::sync::atomic::{AtomicU64, Ordering};

use log::{error, info};
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

/// The tablet bound at boot, for `mouse`.
#[derive(Clone, Debug)]
pub struct Device {
    pub addr: PciAddr,
    /// Its `ID_NAME` (QEMU's: "QEMU Virtio Tablet").
    pub name: String,
    pub screen: (usize, usize),
}

/// What `mouse` shows.
#[derive(Clone, Debug)]
pub struct Status {
    pub device: Device,
    /// The newest whole frame's state, if one came.
    pub state: Option<PointerState>,
    /// Events read from the device, and frames among them.
    pub events: u64,
    pub frames: u64,
    /// Events a tablet has no use for.
    pub ignored: u64,
    /// Set when the device failed (the thread ended): why.
    pub failed: Option<&'static str>,
}

struct Shared {
    device: Option<Device>,
    state: Option<PointerState>,
    ignored: u64,
    failed: Option<&'static str>,
}

static SHARED: SpinLock<Shared> = SpinLock::new(Shared { device: None, state: None, ignored: 0, failed: None });
static EVENTS: AtomicU64 = AtomicU64::new(0);
static FRAMES: AtomicU64 = AtomicU64::new(0);

/// The tablet's state, if one is bound (`None` without one).
pub fn status() -> Option<Status> {
    let s = SHARED.lock();
    let device = s.device.clone()?;
    Some(Status {
        device,
        state: s.state,
        events: EVENTS.load(Ordering::Relaxed),
        frames: FRAMES.load(Ordering::Relaxed),
        ignored: s.ignored,
        failed: s.failed,
    })
}

/// virtio input, class input/other: QEMU's tablet (see the module doc).
fn matches(d: &DeviceInfo) -> bool {
    virtio_type(d.vendor, d.device, d.subsystem.1) == Some(VIRTIO_INPUT)
        && (d.class, d.subclass) == (CLASS_INPUT, SUBCLASS_OTHER)
}

/// Read `select`/`subsel` from the device's configuration into `out`
/// (at most its length); returns the size the device reports.
fn query(t: &mut VirtioTransport, select: u8, subsel: u8, out: &mut [u8]) -> Result<usize, ProbeError> {
    t.write_config_space(cfg::SELECT, select).map_err(ProbeError::Device)?;
    t.write_config_space(cfg::SUBSEL, subsel).map_err(ProbeError::Device)?;
    let size = usize::from(t.read_config_space::<u8>(cfg::SIZE).map_err(ProbeError::Device)?);
    for (i, b) in out.iter_mut().take(size.min(cfg::DATA_LEN)).enumerate() {
        *b = t.read_config_space(cfg::DATA + i).map_err(ProbeError::Device)?;
    }
    Ok(size)
}

/// Axis `axis`'s range (`ABS_INFO`: min and max, little-endian, first).
fn axis(t: &mut VirtioTransport, axis: u16) -> Result<AbsAxis, ProbeError> {
    let mut info = [0u8; 8];
    let size = query(t, cfg::ABS_INFO, axis as u8, &mut info)?;
    if size < info.len() {
        return Err(ProbeError::Unusable(alloc::format!("axis {axis}: ABS_INFO is {size} bytes")));
    }
    let min = i32::from_le_bytes([info[0], info[1], info[2], info[3]]);
    let max = i32::from_le_bytes([info[4], info[5], info[6], info[7]]);
    AbsAxis::new(min, max).map_err(|e| ProbeError::Unusable(alloc::format!("axis {axis}: {e}")))
}

/// Bring up a tablet: decline a device without both absolute axes (before
/// the crate posts any buffer), read the axes' ranges, start the crate's
/// driver, and hand it to the `mouse` thread.
fn probe(res: DeviceResources) -> Result<(), ProbeError> {
    let addr = res.info().addr;
    // One tablet: its state and the pointer are single. A second is
    // declined before anything is set up (`probe_all` probes one function
    // at a time, so nothing binds one meanwhile).
    if let Some(first) = SHARED.lock().device.as_ref().map(|d| d.addr) {
        info!(target: "mouse", "{addr}: a tablet is bound already (on {first}): only one is used");
        return Err(ProbeError::NotMine);
    }
    let mut t = virtio::transport(res)?;
    let mut abs = [0u8; 1];
    let size = query(&mut t, cfg::EV_BITS, evdev::EV_ABS as u8, &mut abs)?;
    let xy = (1 << evdev::ABS_X) | (1 << evdev::ABS_Y);
    if size == 0 || abs[0] & xy != xy {
        info!(target: "mouse", "{addr}: no absolute X and Y axes, so not a tablet");
        return Err(ProbeError::NotMine);
    }
    let (x, y) = (axis(&mut t, evdev::ABS_X)?, axis(&mut t, evdev::ABS_Y)?);
    let mut name = [0u8; cfg::DATA_LEN];
    let len = query(&mut t, cfg::ID_NAME, 0, &mut name)?.min(cfg::DATA_LEN);
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
    let tablet = Tablet::new(x, y, screen.0, screen.1);
    info!(
        target: "mouse",
        "{addr}: \"{name}\", axes {} to {} by {} to {}, scaled to {}x{}",
        x.min(), x.max(), y.min(), y.max(), screen.0, screen.1
    );
    SHARED.lock().device = Some(Device { addr, name, screen });
    match crate::sched::spawn("mouse", move || run(input, tablet, status)) {
        Ok(_) => Ok(()),
        Err(e) => {
            SHARED.lock().device = None;
            Err(ProbeError::Thread(e))
        }
    }
}

/// The `mouse` thread: the device's only user from here on.
fn run(mut input: Input, mut tablet: Tablet, status: StatusHandle) {
    loop {
        let mut newest = None;
        while let Some(e) = input.pop_pending_event() {
            EVENTS.fetch_add(1, Ordering::Relaxed);
            if let Some(s) = tablet.feed(Event::new(e.event_type, e.code, e.value)) {
                FRAMES.fetch_add(1, Ordering::Relaxed);
                newest = Some(s);
            }
        }
        if let Some(s) = newest {
            {
                let mut shared = SHARED.lock();
                shared.state = Some(s);
                shared.ignored = tablet.ignored();
            }
            crate::term::move_pointer(s.x, s.y);
        }
        let st = status.get();
        if st.intersects(DeviceStatus::DEVICE_NEEDS_RESET | DeviceStatus::FAILED) {
            let why = if st.contains(DeviceStatus::FAILED) { "FAILED" } else { "DEVICE_NEEDS_RESET" };
            error!(target: "mouse", "the tablet's status reads {:#04x} ({why}): the mouse stops", status.raw());
            SHARED.lock().failed = Some(why);
            crate::term::hide_pointer();
            // Dropping the driver resets the device before its queues go
            // (`VirtioTransport`'s drop; one that doesn't reset loses its
            // bus mastering first).
            return;
        }
        crate::time::sleep(POLL_NS);
    }
}
