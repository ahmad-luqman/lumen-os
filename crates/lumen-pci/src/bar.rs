//! Base address registers: what each one decodes, sized by writing all
//! ones and reading back which address bits stick.

use crate::device::{Header, HeaderType};
use crate::{ConfigSpace, PciAddr, command, reg};

/// One BAR the firmware assigned. The kernel keeps these addresses (OVMF
/// assigns them; Lumen never reassigns in M3).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Bar {
    /// Memory space. A 64-bit BAR also takes the next BAR's slot (which
    /// is then `None`).
    Memory { phys: u64, size: u64, prefetchable: bool, is_64: bool },
    /// I/O space.
    Io { port: u32, size: u32 },
}

impl Bar {
    /// Whether this is a memory BAR.
    pub fn is_memory(&self) -> bool {
        matches!(self, Bar::Memory { .. })
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
/// A slot is `None` when the BAR isn't implemented (no address bit sticks),
/// is a memory BAR at 0 or an I/O BAR at port 0 (unassigned), uses the
/// reserved memory type, is the upper half of a 64-bit BAR, is a 64-bit BAR
/// in the last slot, or is a 64-bit BAR whose range ends at 2^64. (The size
/// is the lowest address bit that sticks, so it never exceeds the address's
/// alignment and a range can't run past its address space otherwise.)
pub fn size_bars(cs: &mut impl ConfigSpace, addr: PciAddr, header: &Header) -> [Option<Bar>; 6] {
    let mut bars = [None; 6];
    let count = match header.header_type {
        HeaderType::Endpoint => 6,
        HeaderType::PciBridge => 2,
        _ => return bars,
    };
    let saved = cs.read16(addr, reg::COMMAND);
    let decode_off = !header.is_host_bridge();
    if decode_off {
        cs.write_command(addr, saved & !(command::IO_SPACE | command::MEMORY_SPACE));
    }
    let mut i = 0;
    while i < count {
        let (bar, slots) = size_one(cs, addr, i, count);
        bars[i] = bar;
        i += slots;
    }
    if decode_off {
        cs.write_command(addr, saved);
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

/// The lowest set bit of `mask`: the size of the range a BAR decodes (its
/// writable address bits are the high ones, down to the size's bit).
fn lowest_bit(mask: u64) -> u64 {
    mask.isolate_lowest_one()
}

/// Size BAR `i` of `count`; returns it and how many slots it takes.
fn size_one(cs: &mut impl ConfigSpace, addr: PciAddr, i: usize, count: usize) -> (Option<Bar>, usize) {
    let offset = reg::BAR0 + 4 * i as u16;
    let (value, readback) = probe(cs, addr, offset);
    if value & 1 == 1 {
        let port = value & !3;
        // Read-only address bits keep the port's value, so the size is at
        // most the port's alignment: the range ends within 32 bits.
        let size = lowest_bit(u64::from(readback & !3)) as u32;
        let bar = (port != 0 && size != 0).then_some(Bar::Io { port, size });
        return (bar, 1);
    }
    let prefetchable = value & 8 != 0;
    match (value >> 1) & 3 {
        // 32-bit, or below 1 MiB (a PCI 2.x leftover, decoded the same).
        0 | 1 => {
            let phys = u64::from(value & !0xf);
            let size = lowest_bit(u64::from(readback & !0xf));
            let bar = (phys != 0 && size != 0).then_some(Bar::Memory { phys, size, prefetchable, is_64: false });
            (bar, 1)
        }
        2 if i + 1 < count => {
            let (high, high_readback) = probe(cs, addr, offset + 4);
            let phys = u64::from(high) << 32 | u64::from(value & !0xf);
            let size = lowest_bit(u64::from(high_readback) << 32 | u64::from(readback & !0xf));
            // Only a range ending exactly at 2^64 doesn't fit.
            let fits = phys.checked_add(size).is_some();
            let bar = (phys != 0 && size != 0 && fits).then_some(Bar::Memory { phys, size, prefetchable, is_64: true });
            (bar, 2)
        }
        // A 64-bit BAR with no slot left for its upper half, or the
        // reserved type.
        _ => (None, 1),
    }
}
