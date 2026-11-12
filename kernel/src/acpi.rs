//! ACPI discovery: the CPUs, interrupt controllers and HPET the firmware
//! describes, read once at boot.
//!
//! Limine gives the RSDP's physical address; `lumen_acpi` walks RSDP → XSDT
//! (or RSDT) → MADT and HPET and checks every table. Tables are read
//! through [`Firmware`]: ACPI memory isn't in the HHDM, so a table there is
//! mapped uncached with `map_mmio` (a few KiB, read once; mappings are
//! permanent). A table in RAM the HHDM maps is read there instead, and if
//! that is usable memory, its frames are reserved first so the allocator
//! never hands them out (hard rule 2). What the kernel needs is copied out;
//! nothing keeps pointing into firmware memory.
//!
//! No ACPI, or broken tables, is not fatal: boot goes on without the
//! information, with a warning.

use alloc::vec::Vec;
use core::fmt;

use log::{debug, info, warn};
use lumen_acpi::{Acpi, Cpu, Error, Hpet, InterruptOverride, IoApic, Madt, PhysMem, Signature};
use lumen_mm::{PhysAddr, PhysRange};

use crate::boot;
use crate::mm::{self, frame, paging};
use crate::sync::SpinLock;

/// The reservation name for ACPI tables found in usable memory.
const ACPI_TABLE: &str = "acpi table";

/// What the kernel knows from ACPI.
#[derive(Debug)]
pub struct Info {
    /// Every CPU the MADT lists, enabled or not, in MADT order.
    pub cpus: Vec<Cpu>,
    /// The local APIC's physical address.
    pub local_apic: u64,
    pub io_apics: Vec<IoApic>,
    pub overrides: Vec<InterruptOverride>,
    /// The legacy 8259 PICs are present (to be masked).
    pub has_8259_pics: bool,
    pub hpet: Option<Hpet>,
}

impl Info {
    /// The CPUs ready to use.
    pub fn enabled_cpus(&self) -> impl Iterator<Item = &Cpu> {
        self.cpus.iter().filter(|c| c.enabled)
    }
}

static INFO: SpinLock<Option<&'static Info>> = SpinLock::new(None);

/// What ACPI described, if [`init`] found it.
#[cfg_attr(not(feature = "ktest"), allow(dead_code))] // first users: the APIC timer (#7), SMP (#9)
pub fn info() -> Option<&'static Info> {
    *INFO.lock()
}

/// Why the memory behind a table couldn't be read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadError {
    NotPhysical(u64),
    Mmio(paging::MmioError),
    /// The table is in usable memory and the frame allocator refused to
    /// reserve it (most likely it already handed the frame out, and the
    /// table may be overwritten).
    Reserve(frame::ReserveError),
}

impl fmt::Display for ReadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ReadError::NotPhysical(a) => write!(f, "{a:#x} is not a physical address"),
            ReadError::Mmio(e) => write!(f, "mapping it: {e}"),
            ReadError::Reserve(e) => write!(f, "it is in usable memory and can't be reserved: {e}"),
        }
    }
}

/// Physical memory as the ACPI parser reads it. Remembers why the last read
/// failed (the parser only learns that it did).
#[derive(Debug, Default)]
pub struct Firmware {
    pub last_error: Option<ReadError>,
}

impl Firmware {
    /// Map `[addr, addr + len)` for reading, for good.
    pub fn map(addr: u64, len: usize) -> Result<&'static [u8], ReadError> {
        let phys = PhysAddr::new(addr).ok_or(ReadError::NotPhysical(addr))?;
        let virt = match paging::map_mmio(phys, len as u64) {
            Ok(virt) => virt,
            Err(paging::MmioError::Ram(_)) if paging::hhdm_covers(phys, len as u64) => {
                frame::reserve(ACPI_TABLE, PhysRange::from_len(addr, len as u64)).map_err(ReadError::Reserve)?;
                mm::phys_to_virt(phys)
            }
            Err(e) => return Err(ReadError::Mmio(e)),
        };
        // SAFETY: `virt` maps `len` readable bytes for the kernel's whole
        // life: `map_mmio` mappings are permanent, and so is the HHDM, whose
        // frames here were just reserved (usable memory) or are never
        // allocatable (any other type). Nothing in the kernel writes
        // firmware tables, so the bytes don't change under the reference.
        Ok(unsafe { core::slice::from_raw_parts(virt.as_ptr::<u8>(), len) })
    }
}

impl PhysMem<'static> for Firmware {
    fn read(&mut self, addr: u64, len: usize) -> Option<&'static [u8]> {
        Firmware::map(addr, len).inspect_err(|&e| self.last_error = Some(e)).ok()
    }
}

/// An ACPI failure, with the reason a read failed if that was the cause.
#[derive(Debug)]
struct Failure {
    error: Error,
    cause: Option<ReadError>,
}

impl fmt::Display for Failure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.error)?;
        match (self.error, self.cause) {
            (Error::Unreadable { .. }, Some(cause)) => write!(f, " ({cause})"),
            _ => Ok(()),
        }
    }
}

/// Read the MADT and HPET and log a summary. Needs the heap and `map_mmio`
/// (after `mm::init`); call once.
pub fn init() {
    assert!(INFO.lock().is_none(), "acpi::init called twice");
    let Some(rsdp) = boot::RSDP.get_response() else {
        warn!(target: "acpi", "no RSDP from Limine: running without ACPI");
        return;
    };
    let mut fw = Firmware::default();
    let found = discover(&mut fw, rsdp.address() as u64);
    let info = match found {
        Ok(info) => info,
        Err(error) => {
            let failure = Failure { error, cause: fw.last_error };
            warn!(target: "acpi", "running without ACPI: {failure}");
            return;
        }
    };
    let cpus = info.enabled_cpus().count();
    let hpet = info.hpet.map(|h| h.address);
    match hpet {
        Some(addr) => info!(
            target: "acpi",
            "{cpus} CPU{}, LAPIC at {:#x}, HPET at {addr:#x}",
            if cpus == 1 { "" } else { "s" },
            info.local_apic
        ),
        None => {
            info!(target: "acpi", "{cpus} CPU{}, LAPIC at {:#x}, no HPET", if cpus == 1 { "" } else { "s" }, info.local_apic)
        }
    }
    *INFO.lock() = Some(alloc::boxed::Box::leak(alloc::boxed::Box::new(info)));
}

/// Walk the tables from the RSDP at `rsdp`. A missing or broken HPET is
/// logged and left out; anything wrong before or in the MADT fails.
fn discover(fw: &mut Firmware, rsdp: u64) -> Result<Info, Error> {
    let mut acpi = Acpi::new(fw, rsdp)?;
    log_tables(&mut acpi);
    let madt = Madt::parse(acpi.find(Signature::MADT)?)?;
    let hpet = match acpi.find(Signature::HPET).and_then(Hpet::parse) {
        Ok(h) => Some(h),
        Err(e) => {
            let cause = acpi.into_mem().last_error.filter(|_| matches!(e, Error::Unreadable { .. }));
            warn!(target: "acpi", "no usable HPET: {}", Failure { error: e, cause });
            None
        }
    };
    let info = Info {
        cpus: madt.cpus().collect(),
        local_apic: madt.local_apic_address(),
        io_apics: madt.io_apics().collect(),
        overrides: madt.interrupt_overrides().collect(),
        has_8259_pics: madt.has_8259_pics(),
        hpet,
    };
    for c in &info.cpus {
        let state = if c.enabled {
            "enabled"
        } else if c.online_capable {
            "online-capable"
        } else {
            "disabled"
        };
        debug!(target: "acpi", "CPU uid {} APIC id {} {state}", c.uid, c.apic_id);
    }
    let boot_cpu = crate::arch::apic_id();
    if !info.enabled_cpus().any(|c| c.apic_id == boot_cpu) {
        warn!(target: "acpi", "the boot CPU (APIC id {boot_cpu}) is not an enabled CPU in the MADT");
    }
    for io in &info.io_apics {
        info!(target: "acpi", "I/O APIC {} at {:#x}, GSI base {}", io.id, io.address, io.gsi_base);
    }
    debug!(target: "acpi", "{} interrupt overrides, 8259 PICs {}", info.overrides.len(), info.has_8259_pics);
    Ok(info)
}

/// Log the root table's signature and what it lists, at debug level.
fn log_tables(acpi: &mut Acpi<'static, &mut Firmware>) {
    use core::fmt::Write;
    let (root, revision) = (acpi.root().signature(), acpi.rsdp().revision);
    let mut line = alloc::string::String::new();
    for sig in acpi.signatures() {
        // Writing to a String can't fail.
        let _ = match sig {
            Ok(s) => write!(line, " {s}"),
            Err(e) => write!(line, " ({e})"),
        };
    }
    debug!(target: "acpi", "revision {revision}, {root} lists{line}");
}

crate::ktest!(
    fn madt_lists_boot_cpu() {
        use crate::{kassert, kassert_eq};
        let info = info().ok_or("ACPI was not found at boot")?;
        let id = crate::arch::apic_id();
        let boot_cpu = info.cpus.iter().find(|c| c.apic_id == id);
        if boot_cpu.is_none() {
            crate::ktest::diag(format_args!("boot CPU APIC id {id}, MADT lists {:?}", info.cpus));
        }
        kassert_eq!(boot_cpu.map(|c| c.enabled), Some(true));
        kassert!(info.local_apic != 0);
        Ok(())
    }
);

crate::ktest!(
    fn tables_outside_the_hhdm_are_mapped_uncached() {
        use lumen_mm::paging::Cache;

        use crate::{kassert, kassert_eq};
        let rsdp = boot::RSDP.get_response().ok_or("no RSDP")?.address() as u64;
        let phys = PhysAddr::new(rsdp).ok_or("RSDP not physical")?;
        kassert!(!paging::hhdm_covers(phys, 20));
        let bytes = Firmware::map(rsdp, 20).map_err(|_| "mapping the RSDP failed")?;
        kassert_eq!(&bytes[..8], b"RSD PTR ");
        let virt = lumen_mm::VirtAddr::new(bytes.as_ptr() as u64).ok_or("not canonical")?;
        let m = paging::walk(virt).mapping().ok_or("not mapped")?;
        kassert_eq!(m.phys_of(virt), Some(phys));
        kassert_eq!(m.cache(), Some(Cache::Uncached));
        Ok(())
    }
);

crate::ktest!(
    fn tables_in_usable_memory_are_reserved_or_refused() {
        // Stand-ins for a table in usable memory: a free frame (reserved,
        // then read through the HHDM) and an allocated one (refused).
        use crate::{kassert, kassert_eq};
        let held = frame::alloc_owned().ok_or("out of frames")?;
        let f = held.frame();
        let held_addr = f.as_u64();
        kassert_eq!(Firmware::map(held_addr, 36), Err(ReadError::Reserve(frame::ReserveError::InUse(held_addr))));
        kassert!(frame::is_allocated(f)); // a refused read changes nothing
        drop(held);
        // The frame is free again: now it can be reserved.
        let free_before = frame::free_frames();
        let bytes = Firmware::map(held_addr, 36).map_err(|_| "reading a free frame failed")?;
        kassert_eq!(bytes.as_ptr() as u64, mm::phys_to_virt(PhysAddr::new(held_addr).ok_or("phys")?).as_u64());
        kassert_eq!(frame::free_frames(), free_before - 1);
        kassert!(!frame::is_allocated(f));
        // Reading it again takes nothing more.
        Firmware::map(held_addr, 36).map_err(|_| "second read failed")?;
        kassert_eq!(frame::free_frames(), free_before - 1);
        Ok(())
    }
);
