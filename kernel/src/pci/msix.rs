//! MSI-X for a claimed function (docs/ARCHITECTURE.md, section 7,
//! "Interrupts for devices: MSI-X"): one vector per device, to the boot
//! CPU, fixed delivery.
//!
//! [`DeviceResources::msix`] follows `Route`'s pattern: once per function
//! ([`MsiError::AlreadyAllocated`], recorded in its registry slot as soon
//! as an IDT vector is claimed, before any write to the device), it claims
//! an IDT vector with its handler (`idt::claim_msi_vector`), programs table
//! entry 0 **masked**, sets MSI-X Enable (Function Mask clear) in Message
//! Control and INTx-disable in the command register, and hands out an
//! [`MsiVector`], through which alone the entry is unmasked. The MSI-X
//! capability is the one enumeration found (`caps.find(id::MSIX)`), never a
//! caller's offset, and its table is reached through the claimed BAR's
//! mapping in the registry (no new mapping). Message Control has no
//! `write16`: the dword at the capability is read, modified and written
//! back by [`write_control`], private here (its first two bytes, the id and
//! next pointer, are read-only, unlike the status register beside the
//! command register). Vectors are never freed (M3): a device whose probe
//! fails after `msix` keeps its vector, its handler registered, and its
//! bus mastering off (so it can't send the message, a memory write); one
//! that declines after `msix` is recorded as failed instead (`pci::probed`),
//! since MSI-X stays enabled on it.

use alloc::vec::Vec;
use core::fmt;

use log::debug;
use lumen_mm::VirtAddr;
use lumen_pci::msix::{self as m, Capability, Message, MsixProblem};
use lumen_pci::{ConfigSpace, PciAddr, cap, command};

use super::{DeviceResources, set_command, with_ecam, with_slot};

/// The table entry each device uses: one vector per device.
const ENTRY: u16 = 0;

/// A function's MSI-X state in its registry slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum MsixState {
    /// `msix` hasn't been called (or refused before claiming a vector).
    None,
    /// An IDT vector is claimed for it, but programming the device didn't
    /// finish (MSI-X Enable didn't stick; the error names the vector):
    /// never again.
    Claimed,
    /// Its entry is programmed and MSI-X enabled.
    Programmed(MsixRecord),
}

/// A device's MSI-X vector as listed (`irqs`): which function, which table
/// entry, which IDT vector. Read-only: only the owner's [`MsiVector`] masks
/// or unmasks the entry.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MsixRecord {
    pub pci: PciAddr,
    pub table_entry: u16,
    pub idt_vector: u8,
}

/// A device's MSI-X vector: its table entry (and where it is mapped) and
/// the IDT vector the entry sends. Private fields; not `Copy` or `Clone`:
/// its owner (the transport) unmasks and masks the entry, and listings get
/// a read-only [`MsixRecord`].
#[derive(Debug, PartialEq, Eq)]
pub struct MsiVector {
    pci: PciAddr,
    entry: u16,
    vector: u8,
    /// Where entry `entry` of the table is mapped (inside a BAR mapped for
    /// the kernel's life).
    at: VirtAddr,
}

/// Why [`DeviceResources::msix`] refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MsiError {
    /// The function has no MSI-X capability.
    NoCapability,
    /// Its MSI-X capability can't be used.
    Capability(MsixProblem),
    /// The table isn't inside a mapped memory BAR of the function: its BAR
    /// indicator, offset and length.
    Table { bir: u8, offset: u32, len: u64 },
    /// The function has its vector already (once per function).
    AlreadyAllocated,
    /// Every MSI vector is claimed.
    NoVector,
    /// The boot CPU, which every vector goes to, isn't online.
    BootCpuOffline,
    /// The boot CPU's local APIC id, which a message address can't hold
    /// (above 255: x2APIC).
    ApicIdTooLarge(u32),
    /// MSI-X Enable didn't stick: what Message Control reads after `wrote`
    /// (`vector` stays claimed: vectors are never freed).
    NotEnabled { vector: u8, wrote: u16, read: u16 },
    /// The entry still reads masked after it was unmasked.
    StillMasked { vector: u8 },
}

impl fmt::Display for MsiError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            MsiError::NoCapability => f.write_str("no MSI-X capability"),
            MsiError::Capability(p) => write!(f, "{p}"),
            MsiError::Table { bir, offset, len } => {
                write!(f, "the MSI-X table (BAR {bir} +{offset:#x}, {len} bytes) isn't inside a mapped memory BAR")
            }
            MsiError::AlreadyAllocated => f.write_str("its MSI-X vector is allocated already"),
            MsiError::NoVector => f.write_str("every MSI vector is taken"),
            MsiError::BootCpuOffline => f.write_str("the boot CPU isn't online"),
            MsiError::ApicIdTooLarge(id) => write!(f, "the boot CPU's local APIC id {id} doesn't fit a message"),
            MsiError::NotEnabled { vector, wrote, read } => write!(
                f,
                "MSI-X Enable didn't stick: Message Control reads {read:#06x} after {wrote:#06x} (vector {vector:#x} \
                 stays claimed)"
            ),
            MsiError::StillMasked { vector } => {
                write!(f, "its MSI-X entry (vector {vector:#x}) still reads masked after it was unmasked")
            }
        }
    }
}

impl DeviceResources {
    /// Give this function its MSI-X vector, run by `handler` (see the
    /// module doc): the entry is programmed masked, MSI-X enabled, INTx
    /// disabled. Once per function. Unmask it with [`MsiVector::unmask`]
    /// once the device is set up to use it.
    pub fn msix(&mut self, handler: fn()) -> Result<MsiVector, MsiError> {
        let info = self.info;
        let addr = info.addr;
        if with_slot(addr, |s| s.msix != MsixState::None).expect("claimed, so enumerated") {
            return Err(MsiError::AlreadyAllocated);
        }
        let found = info.caps.find(cap::id::MSIX).ok_or(MsiError::NoCapability)?;
        let msix = Capability::read(&mut self.config(), addr, found).map_err(MsiError::Capability)?;
        let table_err = MsiError::Table { bir: msix.table.bir, offset: msix.table.offset, len: msix.table_len() };
        let bar = self.bar(usize::from(msix.table.bir)).ok_or(table_err)?;
        let table =
            bar.virt_of(bar.phys().as_u64() + u64::from(msix.table.offset), msix.table_len()).ok_or(table_err)?;
        let entry_at = msix.entry_offset(ENTRY).expect("a table has at least one entry");
        let at = VirtAddr::new(table.as_u64() + entry_at).expect("inside a mapped BAR");
        let apic = crate::smp::online_lapic_id(0).ok_or(MsiError::BootCpuOffline)?;
        let apic_id = u8::try_from(apic).map_err(|_| MsiError::ApicIdTooLarge(apic))?;
        let vector = crate::arch::idt::claim_msi_vector(handler).map_err(|_| MsiError::NoVector)?;
        // Recorded before the device is touched: whatever happens next, the
        // function has had its one try.
        with_slot(addr, |s| s.msix = MsixState::Claimed);
        for (offset, value) in Message::fixed(apic_id, vector).masked_writes() {
            write_entry(at, offset, value);
        }
        let wrote = m::enabled(msix.control);
        let read = write_control(addr, msix.offset, wrote);
        if !m::delivers(read) {
            // The vector stays claimed (vectors are never freed); its entry
            // stays masked.
            return Err(MsiError::NotEnabled { vector, wrote, read });
        }
        let now = with_ecam(|e| e.read16(addr, lumen_pci::reg::COMMAND));
        set_command(addr, now | command::INTX_DISABLE, "INTx off once MSI-X is on");
        let record = MsixRecord { pci: addr, table_entry: ENTRY, idt_vector: vector };
        with_slot(addr, |s| s.msix = MsixState::Programmed(record));
        debug!(target: "pci", "{addr}: MSI-X entry {ENTRY} sends vector {vector:#x} to local APIC {apic_id}, masked");
        Ok(MsiVector { pci: addr, entry: ENTRY, vector, at })
    }
}

/// Write Message Control of the MSI-X capability at `cap` (read, modify,
/// write the dword: see the module doc) and read it back.
fn write_control(addr: PciAddr, cap: u8, control: u16) -> u16 {
    let at = u16::from(cap);
    with_ecam(|e| {
        let dword = e.read(addr, at);
        e.write(addr, at, m::with_control(dword, control));
        e.read16(addr, at + 2)
    })
}

/// Write the dword at `offset` of the table entry at `entry`.
fn write_entry(entry: VirtAddr, offset: u64, value: u32) {
    // SAFETY: `entry` is a table entry inside a memory BAR the registry
    // mapped uncached for the kernel's life (checked against the BAR's
    // bounds by `virt_of`), and `offset` is one of its four dwords (aligned:
    // the table's offset is 8-byte aligned, entries 16 bytes). Device
    // registers: volatile.
    unsafe { (entry.as_u64() as *mut u32).byte_add(offset as usize).write_volatile(value) }
}

fn read_entry(entry: VirtAddr, offset: u64) -> u32 {
    // SAFETY: as in `write_entry`.
    unsafe { (entry.as_u64() as *const u32).byte_add(offset as usize).read_volatile() }
}

impl MsiVector {
    /// The function it belongs to.
    #[cfg(feature = "ktest")]
    pub fn pci(&self) -> PciAddr {
        self.pci
    }

    /// Its MSI-X table entry (the number virtio's `msix_config` and
    /// `queue_msix_vector` take).
    pub fn table_entry(&self) -> u16 {
        self.entry
    }

    /// The IDT vector its message sends.
    pub fn idt_vector(&self) -> u8 {
        self.vector
    }

    /// Unmask its entry: the device may send it from now on (a message it
    /// held pending while masked is sent at once). Read back: an entry that
    /// still reads masked would never deliver. No lock: the entry's mapping
    /// is the vector's own.
    pub fn unmask(&self) -> Result<(), MsiError> {
        write_entry(self.at, m::ENTRY_VECTOR_CONTROL, 0);
        if read_entry(self.at, m::ENTRY_VECTOR_CONTROL) & m::VECTOR_MASKED != 0 {
            return Err(MsiError::StillMasked { vector: self.vector });
        }
        Ok(())
    }

    /// Mask its entry again (nothing does in M3 but the ktests).
    #[cfg(feature = "ktest")]
    pub fn mask(&self) {
        write_entry(self.at, m::ENTRY_VECTOR_CONTROL, m::VECTOR_MASKED);
    }

    /// Whether its entry is masked (read from the table).
    #[cfg(feature = "ktest")]
    pub fn is_masked(&self) -> bool {
        read_entry(self.at, m::ENTRY_VECTOR_CONTROL) & m::VECTOR_MASKED != 0
    }

    /// The message its entry holds (address, data).
    #[cfg(feature = "ktest")]
    pub fn message(&self) -> (u64, u32) {
        let lo = u64::from(read_entry(self.at, m::ENTRY_ADDR_LO));
        let hi = u64::from(read_entry(self.at, m::ENTRY_ADDR_HI));
        (lo | hi << 32, read_entry(self.at, m::ENTRY_DATA))
    }
}

/// Every MSI-X vector programmed so far, by IDT vector (for `irqs`).
pub fn msix_vectors() -> Vec<MsixRecord> {
    let mut all: Vec<MsixRecord> = super::REGISTRY
        .lock()
        .iter()
        .filter_map(|s| match s.msix {
            MsixState::Programmed(r) => Some(r),
            _ => None,
        })
        .collect();
    all.sort_by_key(|r| r.idt_vector);
    all
}

/// Ktests: Message Control and the command register of `addr`, as read now.
#[cfg(feature = "ktest")]
pub fn ktest_control(addr: PciAddr) -> Option<(u16, u16)> {
    let info = super::device(addr)?;
    let found = info.caps.find(cap::id::MSIX)?;
    Some(with_ecam(|e| (e.read16(addr, u16::from(found.offset) + 2), e.read16(addr, lumen_pci::reg::COMMAND))))
}

crate::ktest!(
    fn msix_is_programmed_masked_once_per_function() {
        // The ktest machine's virtio keyboard (no driver takes one in M3):
        // `msix` claims an IDT vector with the handler, programs entry 0
        // masked with a message to the boot CPU's local APIC, sets MSI-X
        // Enable (Function Mask clear) and INTx-disable; a second call is
        // refused. Declined after that, the function is recorded as failed
        // (MSI-X stays on). Its `MsiVector` unmasks (read back) and masks the
        // entry; `msix_vectors` lists it as a read-only record.
        use lumen_pci::msix::{CONTROL_ENABLE, CONTROL_FUNCTION_MASK};

        use super::{Binding, Probed, claim, probed};
        use crate::arch::idt;
        use crate::{kassert, kassert_eq};
        const KEYBOARD: u16 = 18;
        fn nothing() {}
        let d = crate::virtio::ktest_device(KEYBOARD)?;
        let addr = d.addr;
        let mut res = claim(addr, "ktest-msix").map_err(|_| "claim refused")?;
        let (control, _) = ktest_control(addr).ok_or("the keyboard has no MSI-X capability")?;
        kassert!(control & CONTROL_ENABLE == 0);
        let v = res.msix(nothing).map_err(|e| {
            crate::ktest::diag(format_args!("{e}"));
            "msix refused"
        })?;
        kassert_eq!((v.pci(), v.table_entry()), (addr, 0));
        kassert!(idt::msi_claimed(v.idt_vector()));
        let (control, command) = ktest_control(addr).ok_or("no MSI-X capability")?;
        kassert_eq!(control & (CONTROL_ENABLE | CONTROL_FUNCTION_MASK), CONTROL_ENABLE);
        kassert!(command & command::INTX_DISABLE != 0);
        kassert!(v.is_masked());
        let apic = u64::from(crate::smp::online_lapic_id(0).ok_or("the boot CPU is online")?);
        kassert_eq!(v.message(), (0xfee0_0000 | apic << 12, u32::from(v.idt_vector())));
        kassert_eq!(res.msix(nothing), Err(MsiError::AlreadyAllocated));
        let record = MsixRecord { pci: addr, table_entry: 0, idt_vector: v.idt_vector() };
        kassert!(msix_vectors().contains(&record));
        kassert_eq!(v.unmask(), Ok(()));
        kassert!(!v.is_masked());
        v.mask();
        kassert!(v.is_masked());
        drop(res);
        let binding = probed(addr, "ktest-msix", Probed::Declined);
        let why = alloc::string::String::from("declined it after its MSI-X vector was set up (it has had its one)");
        kassert_eq!(binding, Binding::Failed("ktest-msix", why));
        kassert_eq!(alloc::format!("{}", MsiError::AlreadyAllocated), "its MSI-X vector is allocated already");
        kassert_eq!(alloc::format!("{}", MsiError::NoCapability), "no MSI-X capability");
        kassert_eq!(alloc::format!("{}", MsiError::BootCpuOffline), "the boot CPU isn't online");
        kassert_eq!(
            alloc::format!("{}", MsiError::NotEnabled { vector: 0x92, wrote: 0x8001, read: 0x0001 }),
            "MSI-X Enable didn't stick: Message Control reads 0x0001 after 0x8001 (vector 0x92 stays claimed)"
        );
        kassert_eq!(
            alloc::format!("{}", MsiError::ApicIdTooLarge(300)),
            "the boot CPU's local APIC id 300 doesn't fit a message"
        );
        Ok(())
    }
);
