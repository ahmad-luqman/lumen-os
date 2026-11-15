//! A function's header, the [`DeviceInfo`] the kernel keeps for it, and
//! the brute-force scan that finds them.

use core::ops::RangeInclusive;

use crate::{Bar, CapList, ConfigSpace, PciAddr, reg, size_bars};

/// The header's layout (the header type field's low 7 bits).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HeaderType {
    /// Type 0: a device (6 BARs).
    Endpoint,
    /// Type 1: a PCI-to-PCI bridge (2 BARs).
    PciBridge,
    /// Type 2: a CardBus bridge (no BARs or capabilities decoded).
    CardBus,
    Other(u8),
}

/// The fields every header type shares.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Header {
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

/// The header of the function at `addr`, or `None` if there is none: the
/// id dword reads as all ones (nothing answered) or a vendor of 0 or
/// 0xffff.
pub fn read_header(cs: &mut impl ConfigSpace, addr: PciAddr) -> Option<Header> {
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
            other => HeaderType::Other(other),
        },
        multi_function: kind & 0x80 != 0,
    })
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
    /// The BARs the firmware assigned, sized ([`size_bars`]).
    pub bars: [Option<Bar>; 6],
    /// MSI, MSI-X, vendor-specific (virtio's) and the others, complete or
    /// cut off.
    pub caps: CapList,
    /// The INTx pin (1-4 for INTA-INTD, 0 for none): shown only, INTx is
    /// never routed (no `_PRT`).
    pub irq_pin: u8,
}

impl DeviceInfo {
    /// Read the function at `addr`, sizing its BARs (which briefly turns
    /// its decoding off: see [`size_bars`]); `None` if there is none.
    pub fn read(cs: &mut impl ConfigSpace, addr: PciAddr) -> Option<DeviceInfo> {
        read_header(cs, addr).map(|h| DeviceInfo::from_header(cs, addr, &h))
    }

    fn from_header(cs: &mut impl ConfigSpace, addr: PciAddr, h: &Header) -> DeviceInfo {
        let subsystem = match h.header_type {
            HeaderType::Endpoint => {
                let s = cs.read(addr, reg::SUBSYSTEM);
                (s as u16, (s >> 16) as u16)
            }
            _ => (0, 0),
        };
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
        }
    }
}

/// Scan `buses` of `segment` by brute force: devices 0-31 on each bus,
/// functions 1-7 only where function 0 exists and says it is
/// multi-function. Bridges' secondary buses lie inside the range ECAM
/// covers, so they are reached without following bridges. Calls `found`
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
