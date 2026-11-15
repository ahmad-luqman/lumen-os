//! A fake configuration space for the tests: functions with BARs that
//! behave like hardware's (only their address bits above the size are
//! writable), a command register, and a capability list laid out by hand.

extern crate std;

use std::collections::BTreeMap;
use std::vec::Vec;

use crate::{ConfigSpace, PciAddr, command, reg};

#[derive(Clone, Debug)]
pub struct Function {
    pub regs: [u32; 64],
    /// Each BAR's writable bits (0: not implemented). A 64-bit BAR's upper
    /// half has its own entry.
    pub bar_masks: [u32; 6],
    /// Whether all ones was written to a BAR while I/O or memory decoding
    /// was on.
    pub sized_while_decoding: bool,
    /// Every write to the command register, in order.
    pub commands: Vec<u16>,
}

impl Function {
    /// A type 0 function with this id, class and subclass (revision 1,
    /// prog-if 0), memory and I/O decoding and bus mastering on.
    pub fn endpoint(vendor: u16, device: u16, class: u8, subclass: u8) -> Function {
        let mut regs = [0; 64];
        regs[0] = u32::from(device) << 16 | u32::from(vendor);
        regs[1] = u32::from(command::IO_SPACE | command::MEMORY_SPACE | command::BUS_MASTER);
        regs[2] = u32::from(class) << 24 | u32::from(subclass) << 16 | 1;
        Function { regs, bar_masks: [0; 6], sized_while_decoding: false, commands: Vec::new() }
    }

    fn set_byte(&mut self, offset: usize, b: u8) {
        let shift = (offset % 4) * 8;
        self.regs[offset / 4] = self.regs[offset / 4] & !(0xff << shift) | u32::from(b) << shift;
    }

    pub fn header_type(mut self, kind: u8) -> Function {
        self.set_byte(0x0e, kind);
        self
    }

    /// Set the header type's multi-function bit.
    pub fn multi(mut self) -> Function {
        let kind = (self.regs[3] >> 16) as u8;
        self.set_byte(0x0e, kind | 0x80);
        self
    }

    pub fn prog_if(mut self, p: u8) -> Function {
        self.set_byte(0x09, p);
        self
    }

    pub fn subsystem(mut self, vendor: u16, id: u16) -> Function {
        self.regs[usize::from(reg::SUBSYSTEM) / 4] = u32::from(id) << 16 | u32::from(vendor);
        self
    }

    /// Set status register bits (besides the capabilities bit).
    pub fn status(mut self, bits: u16) -> Function {
        self.regs[1] |= u32::from(bits) << 16;
        self
    }

    /// A bridge's primary, secondary and subordinate bus numbers.
    pub fn bridge_buses(mut self, primary: u8, secondary: u8, subordinate: u8) -> Function {
        self.regs[6] = u32::from(subordinate) << 16 | u32::from(secondary) << 8 | u32::from(primary);
        self
    }

    pub fn irq_pin(mut self, pin: u8) -> Function {
        self.set_byte(0x3d, pin);
        self
    }

    /// A 32-bit memory BAR `i` at `phys`, decoding `size` bytes (a power
    /// of two, at least 16).
    pub fn mem32(mut self, i: usize, phys: u32, size: u32, prefetchable: bool) -> Function {
        self.regs[4 + i] = phys | if prefetchable { 8 } else { 0 };
        self.bar_masks[i] = !(size - 1) & !0xf;
        self
    }

    /// A 64-bit memory BAR in slots `i` and `i + 1`.
    pub fn mem64(mut self, i: usize, phys: u64, size: u64, prefetchable: bool) -> Function {
        self.regs[4 + i] = phys as u32 | 0b100 | if prefetchable { 8 } else { 0 };
        self.regs[5 + i] = (phys >> 32) as u32;
        let mask = !(size - 1);
        self.bar_masks[i] = mask as u32 & !0xf;
        self.bar_masks[i + 1] = (mask >> 32) as u32;
        self
    }

    /// An I/O BAR `i` at `port`, decoding `size` bytes with `bits` address
    /// bits (16 or 32: a 16-bit decoder reads its upper half back as 0).
    pub fn io(mut self, i: usize, port: u32, size: u32, bits: u32) -> Function {
        self.regs[4 + i] = port | 1;
        let mask = !(size - 1) & !3;
        self.bar_masks[i] = if bits == 16 { mask & 0xffff } else { mask };
        self
    }

    /// Raw BAR register `i` and its writable bits.
    pub fn raw_bar(mut self, i: usize, value: u32, mask: u32) -> Function {
        self.regs[4 + i] = value;
        self.bar_masks[i] = mask;
        self
    }

    /// A capability list: the header's pointer (`first`), and each
    /// capability as `(offset, id, next)`. Sets the status register's
    /// capabilities bit.
    pub fn caps(mut self, first: u8, caps: &[(u8, u8, u8)]) -> Function {
        self.regs[1] |= u32::from(crate::STATUS_CAP_LIST) << 16;
        self.set_byte(usize::from(reg::CAP_PTR), first);
        for &(offset, id, next) in caps {
            self.set_byte(usize::from(offset), id);
            self.set_byte(usize::from(offset) + 1, next);
        }
        self
    }

    /// The body of a vendor-specific capability `caps` laid out at `at`, as
    /// virtio's `virtio_pci_cap` (virtio 1.2 §4.1.4): its length field,
    /// `cfg_type`, BAR, offset and length (each dword only if it fits in
    /// the 256 bytes the fake has).
    pub fn virtio_cap(mut self, at: u8, cap_len: u8, cfg_type: u8, bar: u8, offset: u32, length: u32) -> Function {
        let at = usize::from(at);
        self.set_byte(at + 2, cap_len);
        self.set_byte(at + 3, cfg_type);
        for (n, value) in [(1, u32::from(bar)), (2, offset), (3, length)] {
            if let Some(r) = self.regs.get_mut(at / 4 + n) {
                *r = value;
            }
        }
        self
    }

    pub fn command(&self) -> u16 {
        self.regs[1] as u16
    }

    pub fn status_bits(&self) -> u16 {
        (self.regs[1] >> 16) as u16
    }

    pub fn bar(&self, i: usize) -> u32 {
        self.regs[4 + i]
    }
}

#[derive(Default, Debug)]
pub struct Fake {
    pub functions: BTreeMap<PciAddr, Function>,
    pub reads: usize,
}

impl Fake {
    pub fn with(mut self, addr: PciAddr, f: Function) -> Fake {
        self.functions.insert(addr, f);
        self
    }

    pub fn get(&self, addr: PciAddr) -> &Function {
        &self.functions[&addr]
    }
}

/// `segment:bus:device.function`, for the tests.
pub fn at(segment: u16, bus: u8, device: u8, function: u8) -> PciAddr {
    PciAddr::new(segment, bus, device, function).unwrap()
}

impl ConfigSpace for Fake {
    fn read(&mut self, addr: PciAddr, offset: u16) -> u32 {
        assert!(offset.is_multiple_of(4) && offset < 4096, "bad offset {offset:#x}");
        self.reads += 1;
        match self.functions.get(&addr) {
            Some(f) => f.regs.get(usize::from(offset) / 4).copied().unwrap_or(0),
            None => u32::MAX,
        }
    }

    fn write(&mut self, addr: PciAddr, offset: u16, value: u32) {
        assert!(offset.is_multiple_of(4) && offset < 4096, "bad offset {offset:#x}");
        let Some(f) = self.functions.get_mut(&addr) else { return };
        let i = usize::from(offset) / 4;
        match offset {
            reg::ID | reg::CLASS => {}
            reg::COMMAND => {
                // Status bits are write-one-to-clear: a 1 written clears
                // that bit, a 0 keeps it.
                let status = f.regs[1] & 0xffff_0000 & !(value & 0xffff_0000);
                f.regs[1] = status | value & 0xffff;
                f.commands.push(value as u16);
            }
            0x10..=0x27 => {
                let bar = i - 4;
                let decoding = f.command() & (command::IO_SPACE | command::MEMORY_SPACE) != 0;
                if value == u32::MAX && decoding {
                    f.sized_while_decoding = true;
                }
                let mask = f.bar_masks[bar];
                f.regs[i] = f.regs[i] & !mask | value & mask;
            }
            _ if i < 64 => f.regs[i] = value,
            _ => {}
        }
    }
}
