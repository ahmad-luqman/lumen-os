//! PCI configuration space for the Lumen kernel: function addresses, the
//! header, BAR sizing, the capability list (walked with a guard) and a
//! brute-force scan that turns what it finds into [`DeviceInfo`]s.
//!
//! Everything goes through [`ConfigSpace`], 32-bit reads and writes at a
//! function and a dword-aligned offset: the kernel implements it over ECAM
//! (the MCFG's memory-mapped configuration space), the tests over a fake.
//! Nothing here allocates or maps anything; BAR addresses are the
//! firmware's and are never reassigned.
//!
//! The kernel uses this rather than `virtio-drivers`' `PciRoot` because that
//! one's `enumerate_bus` ignores the multi-function bit, knows no segment,
//! and its capability walk has no loop guard (docs/ARCHITECTURE.md,
//! section 7).

#![no_std]

mod bar;
pub mod cap;
mod device;
pub mod names;

use core::fmt;

pub use bar::{Bar, size_bars};
pub use cap::{Cap, CapEnd, CapList, MAX_CAPS};
pub use device::{DeviceInfo, Header, HeaderType, enumerate, read_header};

/// Offsets in a function's configuration space header.
pub mod reg {
    /// Vendor id (bits 0-15), device id (16-31).
    pub const ID: u16 = 0x00;
    /// Command (bits 0-15), status (16-31).
    pub const COMMAND: u16 = 0x04;
    /// Revision (bits 0-7), programming interface, subclass, class (24-31).
    pub const CLASS: u16 = 0x08;
    /// Cache line size, latency timer, header type (bits 16-23), BIST.
    pub const HEADER_TYPE: u16 = 0x0c;
    /// The first BAR; each is 4 bytes.
    pub const BAR0: u16 = 0x10;
    /// Subsystem vendor (bits 0-15) and id (16-31), header type 0 only.
    pub const SUBSYSTEM: u16 = 0x2c;
    /// The first capability's offset (bits 0-7), header types 0 and 1.
    pub const CAP_PTR: u16 = 0x34;
    /// Interrupt line (bits 0-7) and pin (8-15).
    pub const INTERRUPT: u16 = 0x3c;
}

/// Command register bits.
pub mod command {
    pub const IO_SPACE: u16 = 1 << 0;
    pub const MEMORY_SPACE: u16 = 1 << 1;
    pub const BUS_MASTER: u16 = 1 << 2;
    pub const INTX_DISABLE: u16 = 1 << 10;
}

/// The status register's "capabilities list" bit: the function has one.
pub const STATUS_CAP_LIST: u16 = 1 << 4;

/// One function's address: segment, bus, device (0-31), function (0-7).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PciAddr {
    segment: u16,
    bus: u8,
    device: u8,
    function: u8,
}

impl PciAddr {
    /// `None` if `device` is past 31 or `function` past 7.
    pub const fn new(segment: u16, bus: u8, device: u8, function: u8) -> Option<PciAddr> {
        if device < 32 && function < 8 { Some(PciAddr { segment, bus, device, function }) } else { None }
    }

    pub const fn segment(&self) -> u16 {
        self.segment
    }

    pub const fn bus(&self) -> u8 {
        self.bus
    }

    pub const fn device(&self) -> u8 {
        self.device
    }

    pub const fn function(&self) -> u8 {
        self.function
    }

    /// The offset of this function's 4 KiB in its bus's ECAM megabyte:
    /// `device << 15 | function << 12`.
    pub const fn ecam_offset(&self) -> u32 {
        (self.device as u32) << 15 | (self.function as u32) << 12
    }
}

/// `ssss:bb:dd.f` in hex, as `lspci -D` prints it (`0000:00:1f.2`).
impl fmt::Display for PciAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04x}:{:02x}:{:02x}.{:x}", self.segment, self.bus, self.device, self.function)
    }
}

/// Configuration space as the decoder sees it.
///
/// `offset` is dword-aligned and below 4096 (the extended space ECAM gives;
/// everything here stays below 256). A function that doesn't exist reads
/// as all ones, as PCI's master abort does.
pub trait ConfigSpace {
    fn read(&mut self, addr: PciAddr, offset: u16) -> u32;
    fn write(&mut self, addr: PciAddr, offset: u16, value: u32);

    /// The 16 bits at `offset` (2-byte aligned), from the dword holding them.
    fn read16(&mut self, addr: PciAddr, offset: u16) -> u16 {
        (self.read(addr, offset & !3) >> ((offset & 2) * 8)) as u16
    }

    /// The byte at `offset`, from the dword holding it.
    fn read8(&mut self, addr: PciAddr, offset: u16) -> u8 {
        (self.read(addr, offset & !3) >> ((offset & 3) * 8)) as u8
    }

    /// Write the command register without touching the status register in
    /// the same dword: status bits are write-one-to-clear, so they are
    /// written as 0.
    fn write_command(&mut self, addr: PciAddr, command: u16) {
        self.write(addr, reg::COMMAND, u32::from(command));
    }
}

impl<C: ConfigSpace + ?Sized> ConfigSpace for &mut C {
    fn read(&mut self, addr: PciAddr, offset: u16) -> u32 {
        (**self).read(addr, offset)
    }

    fn write(&mut self, addr: PciAddr, offset: u16, value: u32) {
        (**self).write(addr, offset, value)
    }
}

#[cfg(test)]
mod fake;
#[cfg(test)]
mod tests;
