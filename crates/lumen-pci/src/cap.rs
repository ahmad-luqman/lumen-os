//! The capability list, walked with a guard: a list that loops or points
//! into the header is cut off, and the device is marked so the kernel never
//! probes it.

use crate::device::HeaderType;
use crate::{ConfigSpace, PciAddr, STATUS_CAP_LIST, reg};

/// The longest list there can be: one capability per dword from 0x40 to
/// 0xfc. A 49th entry means the list loops.
pub const MAX_CAPS: usize = 48;

/// Capability ids.
pub mod id {
    pub const POWER_MANAGEMENT: u8 = 0x01;
    pub const MSI: u8 = 0x05;
    /// Vendor-specific: virtio's structures are described by these.
    pub const VENDOR: u8 = 0x09;
    pub const PCI_EXPRESS: u8 = 0x10;
    pub const MSIX: u8 = 0x11;
}

/// One capability: its id and where it is in configuration space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Cap {
    pub id: u8,
    pub offset: u8,
}

/// How a capability list ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapEnd {
    /// At a next pointer of 0, as it should.
    Complete,
    /// Cut off at a pointer into the header (below 0x40): `from` is the
    /// offset of the capability that pointed there (0 for the header's
    /// capabilities pointer).
    IntoHeader { from: u8, to: u8 },
    /// Cut off after [`MAX_CAPS`] entries: the list loops.
    TooLong,
}

/// A function's capabilities, in list order, and how the list ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CapList {
    caps: [Cap; MAX_CAPS],
    len: u8,
    end: CapEnd,
}

impl CapList {
    /// No capabilities, and nothing wrong.
    pub const EMPTY: CapList = CapList { caps: [Cap { id: 0, offset: 0 }; MAX_CAPS], len: 0, end: CapEnd::Complete };

    /// Walk the capability list of the function at `addr` (status register
    /// `status`, header type `header_type`). A function without the
    /// status register's capabilities bit, or a CardBus bridge, has none.
    /// Pointers have their low two bits masked off (reserved, as the PCI
    /// specification tells software to do), so every offset is
    /// dword-aligned; one below 0x40, or a 49th entry, cuts the list off.
    pub fn read(cs: &mut impl ConfigSpace, addr: PciAddr, status: u16, header_type: HeaderType) -> CapList {
        let mut list = CapList::EMPTY;
        let has_list = matches!(header_type, HeaderType::Endpoint | HeaderType::PciBridge);
        if status & STATUS_CAP_LIST == 0 || !has_list {
            return list;
        }
        let (mut from, mut next) = (0, cs.read8(addr, reg::CAP_PTR) & !3);
        while next != 0 {
            if next < 0x40 {
                list.end = CapEnd::IntoHeader { from, to: next };
                return list;
            }
            if usize::from(list.len) == MAX_CAPS {
                list.end = CapEnd::TooLong;
                return list;
            }
            let dword = cs.read(addr, u16::from(next));
            list.caps[usize::from(list.len)] = Cap { id: dword as u8, offset: next };
            list.len += 1;
            (from, next) = (next, (dword >> 8) as u8 & !3);
        }
        list
    }

    /// The capabilities found (before any cut-off), in list order.
    pub fn iter(&self) -> impl Iterator<Item = Cap> + '_ {
        self.caps[..usize::from(self.len)].iter().copied()
    }

    pub fn len(&self) -> usize {
        usize::from(self.len)
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    pub fn end(&self) -> CapEnd {
        self.end
    }

    /// Whether the list ended cleanly. A device whose list didn't is
    /// listed but never probed.
    pub fn is_complete(&self) -> bool {
        self.end == CapEnd::Complete
    }

    /// The first capability with id `id`.
    pub fn find(&self, id: u8) -> Option<Cap> {
        self.iter().find(|c| c.id == id)
    }
}
