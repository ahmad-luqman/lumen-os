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
//! [`check`] also wants each kept structure at least as long as what the
//! crate maps there (the common configuration's 56 bytes, a notify
//! register, the ISR byte, a device configuration dword): a shorter one
//! would surface as the crate's misleading "offset greater than BAR
//! length". [`notify_fits`] checks a queue's notify register inside the
//! notify structure (the crate indexes it with `unwrap`).
//!
//! The crate's capability walk stops at a next pointer below 0x40 or not
//! dword-aligned, where Lumen's (`CapList`) masks the reserved low bits
//! and goes on, and Lumen probes only a complete list: so the crate sees a
//! prefix of what [`used`] sees (apart from a list starting below 0x40,
//! which Lumen cuts off and never probes), and any structure it keeps is
//! the one [`used`] kept (or it misses the type and reports it missing
//! itself, before mapping anything for it).

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

/// The shortest length field `PciTransport::new` accepts: the generic
/// `virtio_pci_cap` (shorter ones are skipped).
pub const MIN_CAP_LEN: u8 = 16;
/// The shortest notify capability it accepts: `virtio_pci_notify_cap`, with
/// `notify_off_multiplier` after the generic fields.
pub const MIN_NOTIFY_CAP_LEN: u8 = 20;

/// Where a notify capability's `notify_off_multiplier` is, after the
/// generic fields.
pub const NOTIFY_MULTIPLIER_AT: u8 = 16;

/// Offsets in the common configuration structure, `virtio_pci_common_cfg`
/// (virtio 1.2 §4.1.4.3): what the kernel reads or writes beside the crate
/// (the device status handle; the MSI-X vectors).
pub mod common {
    /// `msix_config` (1.2's `config_msix_vector`), 16 bits.
    pub const MSIX_CONFIG: u64 = 16;
    /// `num_queues`, 16 bits.
    pub const NUM_QUEUES: u64 = 18;
    /// `device_status`, 8 bits.
    pub const DEVICE_STATUS: u64 = 20;
    /// `queue_select`, 16 bits: which queue the `queue_*` fields show.
    pub const QUEUE_SELECT: u64 = 22;
    /// `queue_msix_vector`, 16 bits.
    pub const QUEUE_MSIX_VECTOR: u64 = 26;
    /// `queue_notify_off`, 16 bits.
    pub const QUEUE_NOTIFY_OFF: u64 = 30;
    /// What `msix_config` and `queue_msix_vector` hold for no vector, and
    /// read back when the device can't map the one written (virtio 1.2
    /// §4.1.5.1.2).
    pub const NO_VECTOR: u16 = 0xffff;
    /// What `virtio-drivers`' `CommonCfg` maps: through `queue_device`'s 8
    /// bytes at 48 (virtio 1.2's full structure is 60 bytes, with
    /// `queue_notify_data` and `queue_reset`, which the crate doesn't use).
    pub const LEN: u32 = 56;
}

/// The shortest structure of each type the crate maps: the common
/// configuration, a notify register (16 bits), the ISR byte, a device
/// configuration dword (it maps that as `u32`s).
pub const fn min_length(cfg_type: u8) -> u32 {
    match cfg_type {
        COMMON_CFG => common::LEN,
        NOTIFY_CFG => 2,
        ISR_CFG => 1,
        DEVICE_CFG => 4,
        _ => 0,
    }
}

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
        if c.cap_len < MIN_CAP_LEN {
            continue;
        }
        let slot = match c.cfg_type {
            COMMON_CFG => &mut u.common,
            NOTIFY_CFG if c.cap_len >= MIN_NOTIFY_CAP_LEN => &mut u.notify,
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
    /// A kept structure shorter than what the crate maps there.
    TooShort { cap: VirtioCap, need: u32 },
    /// Queue `queue`'s notify register (`queue_notify_off` times the
    /// multiplier) isn't inside the notify structure.
    NotifyPastEnd { cap: VirtioCap, queue: u16, notify_off: u16, multiplier: u32 },
    /// A kept structure overlaps the function's MSI-X table or pending bit
    /// array (`table`: which), which the kernel programs itself.
    OverlapsMsix { cap: VirtioCap, table: bool },
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
            CapProblem::TooShort { cap, need } => {
                write!(f, "the {} structure is {:#x} bytes, shorter than {need:#x}", name(cap), cap.length)
            }
            CapProblem::NotifyPastEnd { cap, queue, notify_off, multiplier } => {
                let offset = u64::from(*notify_off) * u64::from(*multiplier);
                write!(
                    f,
                    "queue {queue}'s notify register at {offset:#x} (queue_notify_off {notify_off} times {multiplier}) \
                     is outside the notification structure ({:#x} bytes)",
                    cap.length
                )?;
                if *notify_off == u16::MAX && *multiplier == u32::MAX {
                    f.write_str(": both read all ones, the device isn't responding")?;
                }
                Ok(())
            }
            CapProblem::OverlapsMsix { cap, table } => write!(
                f,
                "the {} structure ({:#x} bytes at {:#x} in BAR {}) overlaps the MSI-X {}",
                name(cap),
                cap.length,
                cap.offset,
                cap.bar,
                if *table { "table" } else { "pending bit array" }
            ),
        }
    }
}

/// Whether `PciTransport::new` can be given this device: every capability
/// in `caps` it reads (length field at least 16) has its fields inside the
/// header, and every structure [`used`] keeps lies inside a mapped memory
/// BAR (`bar_size(i)`: BAR `i`'s size if the kernel mapped it) and clear of
/// the MSI-X table and pending bit array (`msix`: their (BAR, offset,
/// length), `msix::Capability::regions`; the kernel programs those itself).
pub fn check(
    caps: &[VirtioCap],
    bar_size: impl Fn(u8) -> Option<u64>,
    msix: &[(u8, u64, u64)],
) -> Result<Used, CapProblem> {
    for &cap in caps.iter().filter(|c| c.cap_len >= MIN_CAP_LEN) {
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
        let need = min_length(cap.cfg_type);
        if cap.length < need {
            return Err(CapProblem::TooShort { cap, need });
        }
        let (start, end) = (u64::from(cap.offset), u64::from(cap.offset) + u64::from(cap.length));
        for (i, &(bar, offset, len)) in msix.iter().enumerate() {
            if bar == cap.bar && start < offset + len && offset < end {
                return Err(CapProblem::OverlapsMsix { cap, table: i == 0 });
            }
        }
    }
    Ok(u)
}

/// Whether queue `queue`'s notify register lies inside the notify
/// structure `notify`: the crate writes the 16-bit register at
/// `queue_notify_off * multiplier` into it (virtio 1.2 §4.1.4.4), indexing
/// a slice with `unwrap`.
pub fn notify_fits(notify: VirtioCap, multiplier: u32, queue: u16, queue_notify_off: u16) -> Result<(), CapProblem> {
    let offset = u64::from(queue_notify_off) * u64::from(multiplier);
    // The crate's index is offset / 2 into `length / 2` registers.
    if offset / 2 < u64::from(notify.length) / 2 {
        Ok(())
    } else {
        Err(CapProblem::NotifyPastEnd { cap: notify, queue, notify_off: queue_notify_off, multiplier })
    }
}
