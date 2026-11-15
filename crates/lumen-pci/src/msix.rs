//! MSI-X (PCI 3.0 §6.8.2): the capability's registers, where its table is,
//! and what a table entry holds. The kernel programs them (`pci::msix`);
//! this only decodes and encodes.
//!
//! The capability is 12 bytes: the header and Message Control (bits 16-31
//! of its first dword: the table size minus one in bits 0-10, Function
//! Mask in 14, MSI-X Enable in 15), then the table's BAR indicator (BIR,
//! bits 0-2) and offset (the rest, 8-byte aligned), then the same for the
//! pending bit array (PBA). Each table entry is 16 bytes: the message
//! address (low and high dwords), the message data, and Vector Control
//! (bit 0: masked, set at reset).

use core::fmt;

use crate::{Cap, ConfigSpace, PciAddr};

/// Message Control: MSI-X Enable.
pub const CONTROL_ENABLE: u16 = 1 << 15;
/// Message Control: Function Mask (every entry masked, whatever its own
/// mask bit says).
pub const CONTROL_FUNCTION_MASK: u16 = 1 << 14;
/// Message Control: the table size minus one.
pub const CONTROL_TABLE_SIZE: u16 = 0x7ff;

/// One table entry's bytes.
pub const ENTRY_LEN: u64 = 16;
/// An entry's dwords, by offset: message address (low, high), message
/// data, Vector Control.
pub const ENTRY_ADDR_LO: u64 = 0;
pub const ENTRY_ADDR_HI: u64 = 4;
pub const ENTRY_DATA: u64 = 8;
pub const ENTRY_VECTOR_CONTROL: u64 = 12;
/// Vector Control: the entry is masked (no message is sent; a pending one
/// is sent once unmasked).
pub const VECTOR_MASKED: u32 = 1;

/// Where the local APICs' interrupt messages go (Intel SDM vol. 3 §11.11.1):
/// 0xfee in bits 20-31, the destination APIC id in bits 12-19.
pub const MESSAGE_ADDRESS_BASE: u64 = 0xfee0_0000;

/// A structure in one of the function's BARs: the BAR's index (BIR) and
/// the offset in it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Location {
    pub bir: u8,
    pub offset: u32,
}

impl Location {
    fn from_dword(dword: u32) -> Location {
        Location { bir: (dword & 7) as u8, offset: dword & !7 }
    }
}

/// An MSI-X capability as read from configuration space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capability {
    /// Where the capability is in configuration space.
    pub offset: u8,
    /// Message Control as read.
    pub control: u16,
    pub table: Location,
    pub pba: Location,
}

/// Why an MSI-X capability can't be used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MsixProblem {
    /// The capability given isn't MSI-X: its id.
    NotMsix(u8),
    /// The table's BAR indicator isn't 0-5 (6 and 7 are reserved).
    TableBir(u8),
    /// The pending bit array's BAR indicator isn't 0-5.
    PbaBir(u8),
}

impl fmt::Display for MsixProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            MsixProblem::NotMsix(id) => write!(f, "capability {id:#04x} isn't MSI-X"),
            MsixProblem::TableBir(b) => write!(f, "the MSI-X table's BAR indicator is {b} (reserved)"),
            MsixProblem::PbaBir(b) => write!(f, "the MSI-X pending bit array's BAR indicator is {b} (reserved)"),
        }
    }
}

impl Capability {
    /// Read the MSI-X capability at `cap`'s offset (one
    /// `CapList::find(id::MSIX)` gave) of `addr`; the id is checked in
    /// configuration space, not taken from `cap`.
    pub fn read(cs: &mut impl ConfigSpace, addr: PciAddr, cap: Cap) -> Result<Capability, MsixProblem> {
        let at = u16::from(cap.offset);
        let header = cs.read(addr, at);
        let id = header as u8;
        if id != crate::cap::id::MSIX {
            return Err(MsixProblem::NotMsix(id));
        }
        let control = (header >> 16) as u16;
        let table = Location::from_dword(cs.read(addr, at + 4));
        let pba = Location::from_dword(cs.read(addr, at + 8));
        if table.bir > 5 {
            return Err(MsixProblem::TableBir(table.bir));
        }
        if pba.bir > 5 {
            return Err(MsixProblem::PbaBir(pba.bir));
        }
        Ok(Capability { offset: cap.offset, control, table, pba })
    }

    /// How many entries the table has (1 to 2048).
    pub fn table_size(&self) -> u16 {
        (self.control & CONTROL_TABLE_SIZE) + 1
    }

    /// The table's length in bytes.
    pub fn table_len(&self) -> u64 {
        u64::from(self.table_size()) * ENTRY_LEN
    }

    /// The pending bit array's length in bytes: a bit per entry, in
    /// 64-bit words.
    pub fn pba_len(&self) -> u64 {
        u64::from(self.table_size()).div_ceil(64) * 8
    }

    /// The table's and the pending bit array's places: (BAR, offset,
    /// length) each.
    pub fn regions(&self) -> [(u8, u64, u64); 2] {
        [
            (self.table.bir, u64::from(self.table.offset), self.table_len()),
            (self.pba.bir, u64::from(self.pba.offset), self.pba_len()),
        ]
    }

    /// Entry `entry`'s offset from the table's start, if the table has it.
    pub fn entry_offset(&self, entry: u16) -> Option<u64> {
        (entry < self.table_size()).then(|| u64::from(entry) * ENTRY_LEN)
    }
}

/// Message Control with MSI-X enabled and Function Mask clear (each entry's
/// own mask bit decides), the rest as it was.
pub const fn enabled(control: u16) -> u16 {
    (control | CONTROL_ENABLE) & !CONTROL_FUNCTION_MASK
}

/// Whether Message Control as read lets entries deliver: MSI-X Enable set
/// and Function Mask clear.
pub const fn delivers(control: u16) -> bool {
    control & (CONTROL_ENABLE | CONTROL_FUNCTION_MASK) == CONTROL_ENABLE
}

/// The capability's first dword with Message Control replaced by `control`
/// (the header's id and next pointer, read-only, as read).
pub const fn with_control(dword: u32, control: u16) -> u32 {
    dword & 0xffff | (control as u32) << 16
}

/// An interrupt message: what the device writes, and where.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Message {
    pub address: u64,
    pub data: u32,
}

impl Message {
    /// Fixed delivery of `vector` to the local APIC with id `apic_id`:
    /// physical destination mode, no redirection hint (address bits 2 and 3
    /// clear), edge triggered (data bits 14 and 15 clear), delivery mode 0
    /// (fixed, data bits 8-10).
    pub const fn fixed(apic_id: u8, vector: u8) -> Message {
        Message { address: MESSAGE_ADDRESS_BASE | (apic_id as u64) << 12, data: vector as u32 }
    }

    /// The entry's four dwords (address low, high, data, Vector Control),
    /// masked or not.
    pub const fn entry(&self, masked: bool) -> [u32; 4] {
        [self.address as u32, (self.address >> 32) as u32, self.data, if masked { VECTOR_MASKED } else { 0 }]
    }

    /// The writes that program an entry with this message, left masked, in
    /// order (offset in the entry, value): Vector Control masked first, so
    /// the entry is never live with half a message, then the address and
    /// the data.
    pub const fn masked_writes(&self) -> [(u64, u32); 4] {
        let [lo, hi, data, _] = self.entry(true);
        [(ENTRY_VECTOR_CONTROL, VECTOR_MASKED), (ENTRY_ADDR_LO, lo), (ENTRY_ADDR_HI, hi), (ENTRY_DATA, data)]
    }
}
