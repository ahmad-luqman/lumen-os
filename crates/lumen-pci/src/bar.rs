//! Base address registers: what each one decodes, sized by writing all
//! ones and reading back which address bits stick.

use core::fmt;

use crate::device::{Header, HeaderType};
use crate::{ConfigSpace, PciAddr, command, reg};

/// One implemented BAR. The kernel keeps the firmware's addresses (OVMF
/// assigns them; Lumen never reassigns in M3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bar {
    /// Memory space. A 64-bit BAR also takes the next BAR's slot (which
    /// is then `None`): see [`Bar::slots`].
    Memory { phys: u64, size: u64, prefetchable: bool, is_64: bool },
    /// I/O space, within the 16-bit port space.
    Io { port: u32, size: u32 },
    /// Implemented, but the firmware left it at address 0 (unassigned): a
    /// device that needs it can't be used. `is_64` as for memory.
    Unassigned { io: bool, size: u64, is_64: bool },
    /// Implemented, but it can't be used as it is.
    Malformed(BarProblem),
}

/// Why a BAR is [`Bar::Malformed`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarProblem {
    /// Memory type 0b11 (bits 2-1), which PCI reserves.
    ReservedType,
    /// A 64-bit BAR in the last slot: no room for its upper half.
    NoUpperHalf,
    /// An I/O range that doesn't end within the 16-bit port space.
    IoPastPortSpace { port: u32, size: u32 },
    /// A 64-bit range that doesn't end below 2^64 (it still takes 2 slots).
    PastAddressSpace { phys: u64, size: u64 },
}

impl fmt::Display for BarProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            BarProblem::ReservedType => f.write_str("it uses the reserved memory type"),
            BarProblem::NoUpperHalf => f.write_str("it is 64-bit in the last slot (no upper half)"),
            BarProblem::IoPastPortSpace { port, size } => {
                write!(f, "its I/O range {port:#x} (+{size:#x}) ends past port 0xffff")
            }
            BarProblem::PastAddressSpace { phys, size } => {
                write!(f, "its range {phys:#x} (+{size:#x}) doesn't end below 2^64")
            }
        }
    }
}

impl Bar {
    /// Whether this is an assigned memory BAR (one a driver can map).
    pub fn is_memory(&self) -> bool {
        matches!(self, Bar::Memory { .. })
    }

    /// How many BAR slots it takes: 2 for a 64-bit memory BAR (assigned,
    /// unassigned, or with a range ending at 2^64), whose upper half is the
    /// next slot; 1 for the others (a 64-bit BAR in the last slot has no
    /// upper half).
    pub fn slots(&self) -> usize {
        match self {
            Bar::Memory { is_64: true, .. }
            | Bar::Unassigned { is_64: true, .. }
            | Bar::Malformed(BarProblem::PastAddressSpace { .. }) => 2,
            _ => 1,
        }
    }
}

/// Size the BARs of the function at `addr`, whose header is `header` (6
/// BARs for an endpoint, 2 for a PCI-to-PCI bridge, none otherwise).
///
/// I/O and memory decoding are turned off in the command register while a
/// BAR holds all ones (it would otherwise decode a bogus range), and the
/// command register is restored after; every BAR gets its value back. The
/// exception is a host bridge, sized with decoding left on, as Linux does
/// (`mmio_always_on`): turning a host bridge's decoding off can cut the
/// CPU off from memory on real hardware.
///
/// A slot is `None` when the BAR isn't implemented (no address bit sticks)
/// or is the upper half of a 64-bit BAR.
pub(crate) fn size_bars(cs: &mut impl ConfigSpace, addr: PciAddr, header: &Header) -> [Option<Bar>; 6] {
    let mut bars = [None; 6];
    let count = match header.header_type {
        HeaderType::Endpoint => 6,
        HeaderType::PciBridge => 2,
        _ => return bars,
    };
    let decode_off = !header.is_host_bridge();
    if decode_off {
        cs.write_command(addr, header.command & !(command::IO_SPACE | command::MEMORY_SPACE));
    }
    let mut i = 0;
    while i < count {
        let (bar, slots) = size_one(cs, addr, i, count);
        bars[i] = bar;
        i += slots;
    }
    if decode_off {
        cs.write_command(addr, header.command);
    }
    bars
}

/// Write all ones to the BAR at `offset`, read back which bits stick, and
/// put its value back. Returns `(value, readback)`.
fn probe(cs: &mut impl ConfigSpace, addr: PciAddr, offset: u16) -> (u32, u32) {
    let value = cs.read(addr, offset);
    cs.write(addr, offset, u32::MAX);
    let readback = cs.read(addr, offset);
    cs.write(addr, offset, value);
    (value, readback)
}

/// The size of the range a BAR decodes: the lowest address bit that sticks
/// (its writable bits are the high ones, down to the size's). Read-only
/// address bits keep the address's value, so the size is at most the
/// address's alignment.
fn lowest_bit(mask: u64) -> u64 {
    mask.isolate_lowest_one()
}

/// Size BAR `i` of `count`; returns it and the slots it takes:
/// [`Bar::slots`], or for a BAR that isn't implemented, 2 if its type
/// says 64-bit (its upper half isn't sized as a BAR of its own).
fn size_one(cs: &mut impl ConfigSpace, addr: PciAddr, i: usize, count: usize) -> (Option<Bar>, usize) {
    let offset = reg::BAR0 + 4 * i as u16;
    let (value, readback) = probe(cs, addr, offset);
    if value & 1 == 1 {
        let port = value & !3;
        let size = lowest_bit(u64::from(readback & !3)) as u32;
        let bar = match (port, size) {
            (_, 0) => None,
            (0, _) => Some(Bar::Unassigned { io: true, size: u64::from(size), is_64: false }),
            // The size is at most the port's alignment, so port + size
            // <= 2^32.
            _ if u64::from(port) + u64::from(size) > 0x1_0000 => {
                Some(Bar::Malformed(BarProblem::IoPastPortSpace { port, size }))
            }
            _ => Some(Bar::Io { port, size }),
        };
        return (bar, 1);
    }
    let prefetchable = value & 8 != 0;
    let (phys, size, is_64) = match (value >> 1) & 3 {
        // 32-bit, or 0b01 below 1 MiB (a PCI 2.x leftover, decoded the
        // same). The size is at most the address's alignment: the range
        // ends within 32 bits.
        0 | 1 => (u64::from(value & !0xf), lowest_bit(u64::from(readback & !0xf)), false),
        2 if i + 1 < count => {
            let (high, high_readback) = probe(cs, addr, offset + 4);
            let phys = u64::from(high) << 32 | u64::from(value & !0xf);
            (phys, lowest_bit(u64::from(high_readback) << 32 | u64::from(readback & !0xf)), true)
        }
        2 => return (Some(Bar::Malformed(BarProblem::NoUpperHalf)), 1),
        _ => return (Some(Bar::Malformed(BarProblem::ReservedType)), 1),
    };
    let bar = match (phys, size) {
        (_, 0) => None,
        (0, _) => Some(Bar::Unassigned { io: false, size, is_64 }),
        // Only a 64-bit range ending exactly at 2^64 can fail this.
        _ if phys.checked_add(size).is_none() => Some(Bar::Malformed(BarProblem::PastAddressSpace { phys, size })),
        _ => Some(Bar::Memory { phys, size, prefetchable, is_64 }),
    };
    (bar, bar.map_or(if is_64 { 2 } else { 1 }, |b| b.slots()))
}
