//! MSI-X for a claimed function (docs/ARCHITECTURE.md, section 7,
//! "Interrupts for devices: MSI-X"): one vector per device, to the boot
//! CPU, fixed delivery.
//!
//! [`DeviceResources::msix`] follows `Route`'s pattern: once per function
//! ([`MsiError::AlreadyAllocated`], recorded in its registry slot), it
//! claims an IDT vector with its handler (`idt::claim_msi_vector`), programs
//! table entry 0 **masked**, sets MSI-X Enable (Function Mask clear) in
//! Message Control and INTx-disable in the command register, and hands out
//! an [`MsiVector`], through which alone the entry is unmasked. The MSI-X
//! capability is the one enumeration found (`caps.find(id::MSIX)`), never a
//! caller's offset, and its table is reached through the claimed BAR's
//! mapping in the registry (no new mapping). Message Control has no
//! `write16`: the dword at the capability is read, modified and written
//! back by [`write_control`], private here (its first two bytes, the id and
//! next pointer, are read-only, unlike the status register beside the
//! command register). Vectors are never freed (M3): a device whose probe
//! fails after `msix` keeps its vector, its handler registered, and its
//! bus mastering off (so it can't send the message, a memory write).

use alloc::vec::Vec;
use core::fmt;

use log::debug;
use lumen_mm::VirtAddr;
use lumen_pci::msix::{self as m, Capability, Message, MsixProblem};
use lumen_pci::{ConfigSpace, PciAddr, cap, command};

use super::{DeviceResources, set_command, with_ecam, with_slot};

/// The table entry each device uses: one vector per device.
const ENTRY: u16 = 0;

/// What the registry keeps for a function whose MSI-X entry is programmed.
#[derive(Clone, Copy, Debug)]
pub(super) struct Programmed {
    /// Where its entry is mapped.
    entry: VirtAddr,
    vector: MsiVector,
}

/// A device's MSI-X vector: its table entry and the IDT vector the entry
/// sends. Private fields; `Copy` while vectors are never freed (it must
/// stop being `Copy` if freeing is added).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MsiVector {
    pci: PciAddr,
    entry: u16,
    vector: u8,
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
    /// The boot CPU's local APIC id, which a message address can't hold
    /// (above 255), or `None` if the boot CPU isn't online.
    Destination(Option<u32>),
    /// MSI-X Enable didn't stick: what Message Control reads after `wrote`.
    NotEnabled { wrote: u16, read: u16 },
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
            MsiError::Destination(Some(id)) => write!(f, "the boot CPU's local APIC id {id} doesn't fit a message"),
            MsiError::Destination(None) => f.write_str("the boot CPU isn't online"),
            MsiError::NotEnabled { wrote, read } => {
                write!(f, "MSI-X Enable didn't stick: Message Control reads {read:#06x} after {wrote:#06x}")
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
        if with_slot(addr, |s| s.msix.is_some()).expect("claimed, so enumerated") {
            return Err(MsiError::AlreadyAllocated);
        }
        let found = info.caps.find(cap::id::MSIX).ok_or(MsiError::NoCapability)?;
        let msix = Capability::read(&mut self.config(), addr, found).map_err(MsiError::Capability)?;
        let table_err = MsiError::Table { bir: msix.table.bir, offset: msix.table.offset, len: msix.table_len() };
        let bar = self.bar(usize::from(msix.table.bir)).ok_or(table_err)?;
        let table =
            bar.virt_of(bar.phys().as_u64() + u64::from(msix.table.offset), msix.table_len()).ok_or(table_err)?;
        let entry_at = msix.entry_offset(ENTRY).expect("a table has at least one entry");
        let entry = VirtAddr::new(table.as_u64() + entry_at).expect("inside a mapped BAR");
        let apic = crate::smp::online_lapic_id(0);
        let apic_id = apic.and_then(|id| u8::try_from(id).ok()).ok_or(MsiError::Destination(apic))?;
        let vector = crate::arch::idt::claim_msi_vector(handler).map_err(|_| MsiError::NoVector)?;
        // Masked first (reset leaves it masked; this doesn't rely on it),
        // then the message.
        let dwords = Message::fixed(apic_id, vector).entry(true);
        write_entry(entry, m::ENTRY_VECTOR_CONTROL, m::VECTOR_MASKED);
        write_entry(entry, m::ENTRY_ADDR_LO, dwords[0]);
        write_entry(entry, m::ENTRY_ADDR_HI, dwords[1]);
        write_entry(entry, m::ENTRY_DATA, dwords[2]);
        let wrote = m::enabled(msix.control);
        let read = write_control(addr, msix.offset, wrote);
        if read & (m::CONTROL_ENABLE | m::CONTROL_FUNCTION_MASK) != m::CONTROL_ENABLE {
            // The vector stays claimed (vectors are never freed); its entry
            // stays masked.
            return Err(MsiError::NotEnabled { wrote, read });
        }
        let now = with_ecam(|e| e.read16(addr, lumen_pci::reg::COMMAND));
        set_command(addr, now | command::INTX_DISABLE, "INTx off once MSI-X is on");
        let v = MsiVector { pci: addr, entry: ENTRY, vector };
        with_slot(addr, |s| s.msix = Some(Programmed { entry, vector: v }));
        debug!(target: "pci", "{addr}: MSI-X entry {ENTRY} sends vector {vector:#x} to local APIC {apic_id}, masked");
        Ok(v)
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

#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // read back by the ktests
fn read_entry(entry: VirtAddr, offset: u64) -> u32 {
    // SAFETY: as in `write_entry`.
    unsafe { (entry.as_u64() as *const u32).byte_add(offset as usize).read_volatile() }
}

impl MsiVector {
    /// The function it belongs to.
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
    /// held pending while masked is sent at once).
    pub fn unmask(&self) {
        self.set_vector_control(0);
    }

    /// Mask its entry again.
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // nothing masks one again in M3 but the ktests
    pub fn mask(&self) {
        self.set_vector_control(m::VECTOR_MASKED);
    }

    /// Whether its entry is masked (read from the table).
    #[cfg_attr(not(feature = "ktest"), allow(dead_code))] // the ktests
    pub fn is_masked(&self) -> bool {
        read_entry(self.entry_addr(), m::ENTRY_VECTOR_CONTROL) & m::VECTOR_MASKED != 0
    }

    fn set_vector_control(&self, value: u32) {
        write_entry(self.entry_addr(), m::ENTRY_VECTOR_CONTROL, value);
    }

    /// Where its entry is mapped: only `msix` makes an `MsiVector`, and it
    /// records the entry first.
    fn entry_addr(&self) -> VirtAddr {
        with_slot(self.pci, |s| s.msix.map(|p| p.entry)).flatten().expect("an MsiVector's function has its entry")
    }
}

/// Every MSI-X vector handed out so far, by IDT vector (for `irqs`).
pub fn msix_vectors() -> Vec<MsiVector> {
    let mut all: Vec<MsiVector> = super::REGISTRY.lock().iter().filter_map(|s| s.msix.map(|p| p.vector)).collect();
    all.sort_by_key(|v| v.vector);
    all
}

/// Ktests: Message Control and the command register of `addr`, as read now.
#[cfg(feature = "ktest")]
pub fn ktest_control(addr: PciAddr) -> Option<(u16, u16)> {
    let info = super::device(addr)?;
    let found = info.caps.find(cap::id::MSIX)?;
    Some(with_ecam(|e| (e.read16(addr, u16::from(found.offset) + 2), e.read16(addr, lumen_pci::reg::COMMAND))))
}

/// Ktests: the message `v`'s entry holds (address, data).
#[cfg(feature = "ktest")]
pub fn ktest_message(v: MsiVector) -> (u64, u32) {
    let e = v.entry_addr();
    let addr = u64::from(read_entry(e, m::ENTRY_ADDR_LO)) | u64::from(read_entry(e, m::ENTRY_ADDR_HI)) << 32;
    (addr, read_entry(e, m::ENTRY_DATA))
}

crate::ktest!(
    fn msix_is_programmed_masked_once_per_function() {
        // The ktest machine's virtio keyboard (no driver takes one in M3):
        // `msix` claims an IDT vector with the handler, programs entry 0
        // masked with a message to the boot CPU's local APIC, sets MSI-X
        // Enable (Function Mask clear) and INTx-disable; a second call is
        // refused, and so is one after the function was declined and
        // claimed again (once per function, kept in the registry). Only the
        // `MsiVector` unmasks it (and masks it again); `msix_vectors` lists
        // it.
        use lumen_pci::msix::{CONTROL_ENABLE, CONTROL_FUNCTION_MASK};

        use super::{Probed, claim, probed};
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
        kassert_eq!(ktest_message(v), (0xfee0_0000 | apic << 12, u32::from(v.idt_vector())));
        kassert_eq!(res.msix(nothing), Err(MsiError::AlreadyAllocated));
        kassert!(msix_vectors().contains(&v));
        v.unmask();
        kassert!(!v.is_masked());
        v.mask();
        kassert!(v.is_masked());
        drop(res);
        probed(addr, "ktest-msix", Probed::Declined);
        let mut res = claim(addr, "ktest-msix").map_err(|_| "second claim refused")?;
        kassert_eq!(res.msix(nothing), Err(MsiError::AlreadyAllocated));
        drop(res);
        probed(addr, "ktest-msix", Probed::Failed(alloc::string::String::from("ktest: done")));
        kassert_eq!(alloc::format!("{}", MsiError::AlreadyAllocated), "its MSI-X vector is allocated already");
        kassert_eq!(alloc::format!("{}", MsiError::NoCapability), "no MSI-X capability");
        Ok(())
    }
);
