//! The capability list, walked with a guard: a list that loops or points
//! into the header is cut off and marked incomplete (`pci::claim` refuses
//! such a device, recorded as not probed).

use core::fmt;

use crate::device::HeaderType;
use crate::{ConfigSpace, PciAddr, STATUS_CAP_LIST, reg};

/// The longest list there can be: one capability per dword from 0x40 to
/// 0xfc. A 49th entry means the list loops.
pub const MAX_CAPS: usize = 48;

/// Capability ids.
pub mod id {
    pub const MSI: u8 = 0x05;
    /// Vendor-specific: virtio's structures are described by these.
    pub const VENDOR: u8 = 0x09;
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
    /// Not read: the header type (3-127) is one PCI doesn't define, so
    /// where its capabilities pointer is isn't known.
    UnknownHeader(u8),
}

impl fmt::Display for CapEnd {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            CapEnd::Complete => f.write_str("complete"),
            CapEnd::IntoHeader { from: 0, to } => {
                write!(f, "the capabilities pointer points into the header ({to:#x})")
            }
            CapEnd::IntoHeader { from, to } => {
                write!(f, "the capability at {from:#x} points into the header ({to:#x})")
            }
            CapEnd::TooLong => write!(f, "more than {MAX_CAPS} capabilities (the list loops)"),
            CapEnd::UnknownHeader(kind) => write!(f, "header type {kind} is unknown"),
        }
    }
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
    /// status register's capabilities bit has none, and so, as far as
    /// Lumen knows, does a CardBus bridge (its pointer, at 0x14, isn't
    /// decoded); an unknown header type's list isn't read and ends
    /// [`CapEnd::UnknownHeader`]. Pointers have their low two bits masked
    /// off (reserved, as the PCI specification tells software to do), so
    /// every offset is dword-aligned; one below 0x40, or a 49th entry,
    /// cuts the list off.
    pub(crate) fn read(cs: &mut impl ConfigSpace, addr: PciAddr, status: u16, header_type: HeaderType) -> CapList {
        let mut list = CapList::EMPTY;
        match header_type {
            HeaderType::Endpoint | HeaderType::PciBridge => {}
            HeaderType::CardBus => return list,
            HeaderType::Unknown(kind) => {
                list.end = CapEnd::UnknownHeader(kind);
                return list;
            }
        }
        if status & STATUS_CAP_LIST == 0 {
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

    /// The capabilities found (before any cut-off), in list order. A
    /// driver must check [`is_complete`](Self::is_complete) first: a
    /// cut-off list may be missing the one it looks for.
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
    /// listed, and `pci::claim` refuses it.
    pub fn is_complete(&self) -> bool {
        self.end == CapEnd::Complete
    }

    /// The first capability with id `id` (check
    /// [`is_complete`](Self::is_complete) first, as for [`iter`](Self::iter)).
    pub fn find(&self, id: u8) -> Option<Cap> {
        self.iter().find(|c| c.id == id)
    }

    /// The MSI or MSI-X capability whose bytes hold the dword at `offset`
    /// of `addr`, if one does (an MSI capability's length read from its
    /// message control). The kernel refuses a driver's write there: those
    /// are programmed through a path private to `pci` (#96).
    pub fn interrupt_cap_at(&self, cs: &mut impl ConfigSpace, addr: PciAddr, offset: u16) -> Option<Cap> {
        self.iter().find(|c| {
            let len = match c.id {
                id::MSIX => MSIX_LEN,
                id::MSI => msi_len((cs.read(addr, u16::from(c.offset)) >> 16) as u16),
                _ => return false,
            };
            let start = u16::from(c.offset);
            (start..start + u16::from(len)).contains(&offset)
        })
    }
}

/// An MSI-X capability's bytes (PCI 3.0 §6.8.2): header and message
/// control, table offset, PBA offset.
pub const MSIX_LEN: u8 = 12;

/// An MSI capability's bytes for its message control (PCI 3.0 §6.8.1): 12
/// with a 32-bit address, 16 with a 64-bit one (bit 7), 8 more with
/// per-vector masking (bit 8: mask and pending bits).
pub const fn msi_len(control: u16) -> u8 {
    let address = if control & (1 << 7) != 0 { 16 } else { 12 };
    address + if control & (1 << 8) != 0 { 8 } else { 0 }
}
