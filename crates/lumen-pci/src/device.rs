//! A function's header, the [`DeviceInfo`] the kernel keeps for it, and
//! the brute-force scan that finds them.

use core::fmt;
use core::ops::RangeInclusive;

use crate::bar::size_bars;
use crate::{Bar, CapList, ConfigSpace, PciAddr, Problem, names, reg};

/// The header's layout (the header type field's low 7 bits).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum HeaderType {
    /// Type 0: a device (6 BARs).
    Endpoint,
    /// Type 1: a PCI-to-PCI bridge (2 BARs).
    PciBridge,
    /// Type 2: a CardBus bridge (no BARs or capabilities decoded).
    CardBus,
    /// 3-127, which PCI doesn't define.
    Unknown(u8),
}

/// The fields every header type shares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Header {
    pub vendor: u16,
    pub device: u16,
    pub command: u16,
    pub status: u16,
    pub revision: u8,
    pub prog_if: u8,
    pub subclass: u8,
    pub class: u8,
    pub header_type: HeaderType,
    /// The header type field's bit 7: functions 1-7 may exist (only
    /// meaningful on function 0).
    pub multi_function: bool,
}

impl Header {
    /// Class 6, subclass 0.
    pub fn is_host_bridge(&self) -> bool {
        (self.class, self.subclass) == (0x06, 0x00)
    }
}

/// The header of the function at `addr`, or `None` if nothing answers
/// there: the vendor id reads as 0xffff (as an absent function's all-ones
/// dword does) or 0.
pub(crate) fn read_header(cs: &mut impl ConfigSpace, addr: PciAddr) -> Option<Header> {
    let id = cs.read(addr, reg::ID);
    let (vendor, device) = (id as u16, (id >> 16) as u16);
    if vendor == 0xffff || vendor == 0 {
        return None;
    }
    let cmd = cs.read(addr, reg::COMMAND);
    let class = cs.read(addr, reg::CLASS);
    let kind = cs.read8(addr, reg::HEADER_TYPE + 2);
    Some(Header {
        vendor,
        device,
        command: cmd as u16,
        status: (cmd >> 16) as u16,
        revision: class as u8,
        prog_if: (class >> 8) as u8,
        subclass: (class >> 16) as u8,
        class: (class >> 24) as u8,
        header_type: match kind & 0x7f {
            0 => HeaderType::Endpoint,
            1 => HeaderType::PciBridge,
            2 => HeaderType::CardBus,
            other => HeaderType::Unknown(other),
        },
        multi_function: kind & 0x80 != 0,
    })
}

/// Why a BAR slot holds no [`Bar`] ([`DeviceInfo::bar_slot`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NoBar {
    /// There are only slots 0-5.
    OutOfRange,
    /// Not implemented, or past the header type's BARs.
    NotImplemented,
    /// The upper half of the 64-bit BAR in the slot before.
    UpperHalf,
}

/// A PCI-to-PCI bridge's bus numbers: the bus it is on, the bus behind it,
/// and the highest bus below it. Buses `secondary..=subordinate` are
/// reached through it (a secondary bus of 0: not configured).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BusNumbers {
    pub primary: u8,
    pub secondary: u8,
    pub subordinate: u8,
}

/// What the kernel knows about one function, read once at boot
/// (docs/ARCHITECTURE.md, section 7).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DeviceInfo {
    pub addr: PciAddr,
    pub vendor: u16,
    pub device: u16,
    pub class: u8,
    pub subclass: u8,
    pub prog_if: u8,
    pub revision: u8,
    /// Subsystem vendor and id (an endpoint's; `(0, 0)` for other header
    /// types). virtio's transitional devices give their device type here.
    pub subsystem: (u16, u16),
    /// The implemented BARs, sized; `None` for one that isn't implemented,
    /// is the upper half of a 64-bit BAR, or is past the header's BARs (an
    /// endpoint has 6, a bridge 2, others none): [`DeviceInfo::bar_slot`]
    /// says which.
    pub bars: [Option<Bar>; 6],
    /// MSI, MSI-X, vendor-specific (virtio's) and the others: complete,
    /// cut off, or not read (an unknown header type).
    pub caps: CapList,
    /// The INTx pin (1-4 for INTA-INTD, 0 for none): recorded only, INTx
    /// is never routed (no `_PRT`).
    pub irq_pin: u8,
    /// A PCI-to-PCI bridge's bus numbers.
    pub buses: Option<BusNumbers>,
}

impl DeviceInfo {
    /// Read the function at `addr`, sizing its BARs (which briefly turns
    /// its decoding off: see [`size_bars`]); `None` if there is none.
    pub(crate) fn read(cs: &mut impl ConfigSpace, addr: PciAddr) -> Option<DeviceInfo> {
        read_header(cs, addr).map(|h| DeviceInfo::from_header(cs, addr, &h))
    }

    fn from_header(cs: &mut impl ConfigSpace, addr: PciAddr, h: &Header) -> DeviceInfo {
        let (mut subsystem, mut buses) = ((0, 0), None);
        match h.header_type {
            HeaderType::Endpoint => {
                let s = cs.read(addr, reg::SUBSYSTEM);
                subsystem = (s as u16, (s >> 16) as u16);
            }
            HeaderType::PciBridge => {
                let b = cs.read(addr, reg::BRIDGE_BUSES);
                buses = Some(BusNumbers { primary: b as u8, secondary: (b >> 8) as u8, subordinate: (b >> 16) as u8 });
            }
            _ => {}
        }
        DeviceInfo {
            addr,
            vendor: h.vendor,
            device: h.device,
            class: h.class,
            subclass: h.subclass,
            prog_if: h.prog_if,
            revision: h.revision,
            subsystem,
            bars: size_bars(cs, addr, h),
            caps: CapList::read(cs, addr, h.status, h.header_type),
            irq_pin: cs.read8(addr, reg::INTERRUPT + 1),
            buses,
        }
    }

    /// BAR slot `index`: its [`Bar`], or why it has none.
    pub fn bar_slot(&self, index: usize) -> Result<Bar, NoBar> {
        let bar = *self.bars.get(index).ok_or(NoBar::OutOfRange)?;
        match bar {
            Some(bar) => Ok(bar),
            None if index > 0 && self.bars[index - 1].is_some_and(|b| b.slots() == 2) => Err(NoBar::UpperHalf),
            None => Err(NoBar::NotImplemented),
        }
    }

    /// The device's name, if Lumen knows it ([`names::device`]).
    pub fn name(&self) -> Option<&'static str> {
        names::device(self.vendor, self.device, self.subsystem.1)
    }

    /// The class code's name ([`names::class`]).
    pub fn class_name(&self) -> &'static str {
        names::class(self.class, self.subclass)
    }

    /// Whether a driver may be given this device: its capability list was
    /// read in full (#95's `probe_all` skips it otherwise).
    pub fn probeable(&self) -> bool {
        self.caps.is_complete()
    }

    /// What is wrong with this function, for warnings: a capability list
    /// cut off or not read, each unassigned or malformed BAR, and a bridge
    /// with no secondary bus, bus numbers that can't be right, or buses
    /// that `covered(segment, bus)` (asked about the bridge's own segment)
    /// says aren't all in the configuration space being scanned (the
    /// devices behind it would be invisible).
    pub fn problems(&self, covered: impl Fn(u16, u8) -> bool) -> impl Iterator<Item = Problem> {
        let caps = (!self.caps.is_complete()).then_some(Problem::Caps(self.caps.end()));
        let bars = self.bars.into_iter().enumerate().filter_map(|(index, bar)| match bar? {
            Bar::Unassigned { io, size, .. } => Some(Problem::UnassignedBar { index, io, size }),
            Bar::Malformed(why) => Some(Problem::MalformedBar { index, why }),
            _ => None,
        });
        let (segment, own) = (self.addr.segment(), self.addr.bus());
        let bridge = self.buses.and_then(|b| {
            let BusNumbers { primary, secondary, subordinate } = b;
            Some(if secondary == 0 {
                Problem::BridgeUnconfigured
            } else if primary != own || secondary <= own {
                Problem::BridgeMisnumbered { primary, secondary, subordinate }
            } else if secondary > subordinate {
                Problem::BridgeBusesBackwards { secondary, subordinate }
            } else if !(secondary..=subordinate).all(|bus| covered(segment, bus)) {
                Problem::BusesOutsideEcam { secondary, subordinate }
            } else {
                return None;
            })
        });
        caps.into_iter().chain(bars).chain(bridge)
    }
}

/// The boot log's line for a function: `0000:00:00.0 8086:29c0 host
/// bridge (class 060000) Q35 host bridge`.
impl fmt::Display for DeviceInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {:04x}:{:04x} {} (class {:02x}{:02x}{:02x})",
            self.addr,
            self.vendor,
            self.device,
            self.class_name(),
            self.class,
            self.subclass,
            self.prog_if
        )?;
        match self.name() {
            Some(name) => write!(f, " {name}"),
            None => Ok(()),
        }
    }
}

/// Scan `buses` of `segment` by brute force: devices 0-31 on each bus,
/// functions 1-7 only where function 0 exists and says it is
/// multi-function. Bridges' secondary buses normally lie inside the range
/// ECAM covers, so they are reached without following bridges (one that
/// doesn't is [`Problem::BusesOutsideEcam`]). Calls `found`
/// for each function, in address order.
pub fn enumerate(
    cs: &mut impl ConfigSpace,
    segment: u16,
    buses: RangeInclusive<u8>,
    mut found: impl FnMut(DeviceInfo),
) {
    for bus in buses {
        for device in 0..32 {
            let addr = |function| PciAddr::new(segment, bus, device, function).expect("device < 32, function < 8");
            let Some(h) = read_header(cs, addr(0)) else { continue };
            found(DeviceInfo::from_header(cs, addr(0), &h));
            if h.multi_function {
                (1..8).filter_map(|f| DeviceInfo::read(cs, addr(f))).for_each(&mut found);
            }
        }
    }
}
