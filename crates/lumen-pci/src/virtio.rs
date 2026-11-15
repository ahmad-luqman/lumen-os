//! virtio's PCI capabilities (virtio 1.2 §4.1.4), and the check the kernel
//! makes before handing a device to `virtio-drivers`' `PciTransport::new`
//! (docs/ARCHITECTURE.md, section 7).
//!
//! `PciTransport::new` (virtio-drivers 0.13) walks the vendor-specific
//! capabilities and keeps the first of each type 1-4 whose length field is
//! at least 16 (20 for the notify one, which has its multiplier after the
//! common fields), then maps each kept structure through the `Hal` at its
//! BAR's address plus its offset. [`used`] picks exactly those, and
//! [`check`] refuses a device whose kept structures aren't inside a BAR the
//! kernel mapped, or whose capability fields the crate couldn't even read:
//! it computes their offsets in a `u8` and the structure's end in a `u32`,
//! and both would overflow (a panic) for a capability near the end of the
//! 256-byte header or an offset plus length past 4 GiB.
//!
//! The crate's capability walk stops at a next pointer below 0x40 or not
//! dword-aligned, where Lumen's (`CapList`) masks the reserved low bits
//! and goes on, and Lumen probes only a complete list: so the crate sees a
//! prefix of what [`used`] sees, and any structure it keeps is the one
//! [`used`] kept (or it misses the type and reports it missing itself,
//! before mapping anything for it).

use core::fmt;

use crate::{Cap, ConfigSpace, PciAddr};

/// `cfg_type` of the common configuration structure.
pub const COMMON_CFG: u8 = 1;
/// `cfg_type` of the notification structure.
pub const NOTIFY_CFG: u8 = 2;
/// `cfg_type` of the ISR status structure.
pub const ISR_CFG: u8 = 3;
/// `cfg_type` of the device-specific configuration structure.
pub const DEVICE_CFG: u8 = 4;

/// Where the common configuration's `device_status` byte is (virtio 1.2
/// §4.1.4.3: after the four feature dwords, `msix_config` and
/// `num_queues`).
pub const DEVICE_STATUS_OFFSET: u64 = 20;

/// One vendor-specific capability read as virtio's `virtio_pci_cap`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VirtioCap {
    /// Where the capability is in configuration space.
    pub at: u8,
    /// Its length field (`cap_len`): the bytes the capability has.
    pub cap_len: u8,
    pub cfg_type: u8,
    /// Which BAR holds the structure.
    pub bar: u8,
    /// Where in the BAR, and how long.
    pub offset: u32,
    pub length: u32,
}

impl VirtioCap {
    /// Read the vendor-specific capability `cap` of `addr`: its header
    /// dword and the BAR, offset and length dwords after it (only as many
    /// as fit below 256: a capability too near the end is reported by
    /// [`check`], not read past the header).
    pub fn read(cs: &mut impl ConfigSpace, addr: PciAddr, cap: Cap) -> VirtioCap {
        let at = cap.offset;
        let mut dword = |n: u16| {
            let offset = u16::from(at) + 4 * n;
            if offset < 0x100 { cs.read(addr, offset) } else { 0 }
        };
        let header = dword(0);
        VirtioCap {
            at,
            cap_len: (header >> 16) as u8,
            cfg_type: (header >> 24) as u8,
            bar: dword(1) as u8,
            offset: dword(2),
            length: dword(3),
        }
    }
}

/// The structures `PciTransport::new` uses: the first capability of each
/// type 1-4 that it accepts.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Used {
    pub common: Option<VirtioCap>,
    pub notify: Option<VirtioCap>,
    pub isr: Option<VirtioCap>,
    pub device: Option<VirtioCap>,
}

impl Used {
    /// The kept structures, in the order the crate maps them.
    pub fn iter(&self) -> impl Iterator<Item = VirtioCap> {
        [self.common, self.notify, self.isr, self.device].into_iter().flatten()
    }
}

/// Pick what `PciTransport::new` would from the vendor-specific
/// capabilities `caps`, in list order: a length field below 16 is skipped,
/// a notify capability needs 20, and only the first of each type counts.
pub fn used(caps: impl IntoIterator<Item = VirtioCap>) -> Used {
    let mut u = Used::default();
    for c in caps {
        if c.cap_len < 16 {
            continue;
        }
        let slot = match c.cfg_type {
            COMMON_CFG => &mut u.common,
            NOTIFY_CFG if c.cap_len >= 20 => &mut u.notify,
            ISR_CFG => &mut u.isr,
            DEVICE_CFG => &mut u.device,
            _ => continue,
        };
        slot.get_or_insert(c);
    }
    u
}

/// Why [`check`] refused a device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CapProblem {
    /// A capability whose fields end past the 256-byte header (the crate
    /// would overflow computing their offsets).
    PastHeader { cap: VirtioCap },
    /// A kept structure in a BAR that doesn't exist (above 5) or isn't a
    /// memory BAR the kernel mapped.
    BarNotMapped { cap: VirtioCap },
    /// A kept structure whose offset plus length passes its BAR's end (or
    /// 4 GiB).
    PastBar { cap: VirtioCap, bar_size: u64 },
}

impl fmt::Display for CapProblem {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = |c: &VirtioCap| match c.cfg_type {
            COMMON_CFG => "common configuration",
            NOTIFY_CFG => "notification",
            ISR_CFG => "ISR status",
            DEVICE_CFG => "device configuration",
            _ => "virtio",
        };
        match self {
            CapProblem::PastHeader { cap } => {
                write!(f, "the {} capability at {:#x} runs past the 256-byte header", name(cap), cap.at)
            }
            CapProblem::BarNotMapped { cap } => {
                write!(f, "the {} structure is in BAR {}, which isn't a mapped memory BAR", name(cap), cap.bar)
            }
            CapProblem::PastBar { cap, bar_size } => write!(
                f,
                "the {} structure ({:#x} bytes at {:#x}) runs past BAR {} ({bar_size:#x} bytes)",
                name(cap),
                cap.length,
                cap.offset,
                cap.bar
            ),
        }
    }
}

/// Whether `PciTransport::new` can be given this device: every capability
/// in `caps` it reads (length field at least 16) has its fields inside the
/// header, and every structure [`used`] keeps lies inside a mapped memory
/// BAR (`bar_size(i)`: BAR `i`'s size if the kernel mapped it).
pub fn check(caps: &[VirtioCap], bar_size: impl Fn(u8) -> Option<u64>) -> Result<Used, CapProblem> {
    for &cap in caps.iter().filter(|c| c.cap_len >= 16) {
        // The crate reads the dwords at +4, +8 and +12 of every one it
        // looks at, and at +16 of the notify one it keeps, adding in a u8.
        if u16::from(cap.at) + 12 > 0xff {
            return Err(CapProblem::PastHeader { cap });
        }
    }
    let u = used(caps.iter().copied());
    if let Some(cap) = u.notify
        && u16::from(cap.at) + 16 > 0xff
    {
        return Err(CapProblem::PastHeader { cap });
    }
    for cap in u.iter() {
        let size = if cap.bar < 6 { bar_size(cap.bar) } else { None };
        let Some(bar_size) = size else { return Err(CapProblem::BarNotMapped { cap }) };
        let fits = cap.offset.checked_add(cap.length).is_some_and(|end| u64::from(end) <= bar_size);
        if !fits {
            return Err(CapProblem::PastBar { cap, bar_size });
        }
    }
    Ok(u)
}
